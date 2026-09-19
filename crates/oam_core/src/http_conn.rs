//! node's server timeouts, for the HTTP/1 connections oam's http server
//! accepts.
//!
//! What node v22.22.2 does (lib/_http_server.js, src/node_http_parser.cc;
//! measured):
//!
//! - **headersTimeout** (default 60 s) and **requestTimeout** (default 300 s):
//!   a connection's clock starts when it is accepted, and again at the first
//!   byte of each later request. While a request is being received, a check
//!   that runs every `connectionsCheckingInterval` (default 30 s) answers it
//!   `408 Request Timeout` with `Connection: close` -- unless a response head
//!   already went out -- and closes the connection, once its headers are not
//!   in after headersTimeout, or it is not all in after requestTimeout. A
//!   request whose body is in is not checked again, however long its handler
//!   takes. So a silent connection is closed between 60 and 90 s.
//! - **keepAliveTimeout** (default 5 s) plus **keepAliveTimeoutBuffer**
//!   (default 1 s): once a response is done and none is in flight, the
//!   socket's inactivity timeout becomes their sum; the next request's headers
//!   put `server.timeout` back.
//! - the socket timeout -- **server.timeout** (default 0, off) for a new
//!   connection, `req.setTimeout` / `res.setTimeout` / `socket.setTimeout`,
//!   and the keep-alive timeout above: after that long with nothing read or
//!   written, 'timeout' is emitted on the request (while it is incomplete),
//!   the response and the server, and the connection is destroyed when none
//!   of them has a listener. It fires once until there is activity again.
//!
//! [`ConnWatch`] carries one connection's side of that, [`WatchedIo`] feeds
//! it the connection's reads and writes, and [`ServerTimeouts`] holds the
//! server's current settings, which JS updates as the server's properties
//! change. A server driven by node's http module has its check run from JS
//! (node reads `headersTimeout` / `requestTimeout` at each check) and its
//! socket timeouts go to JS as events; `oam.serve` has neither, so the check
//! runs here and an expired socket timeout closes the connection.

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::Notify;
// tokio's clock: the same as std's outside tests, and paused with the runtime
// in them.
use tokio::time::Instant;

/// node's answer to a request that did not arrive in time (`socketOnError`
/// writes exactly this, then destroys the socket).
pub const REQUEST_TIMEOUT_RESPONSE: &[u8] =
    b"HTTP/1.1 408 Request Timeout\r\nConnection: close\r\n\r\n";

/// How long writing that answer may take before the connection is closed
/// anyway: a client that stopped reading gets no more of the server's time.
const FAREWELL_BUDGET: Duration = Duration::from_secs(2);

/// A server's timeout settings, in milliseconds (0 is off).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeoutSettings {
    /// `headersTimeout`.
    pub headers_ms: u64,
    /// `requestTimeout`.
    pub request_ms: u64,
    /// `keepAliveTimeout + keepAliveTimeoutBuffer`, 0 when keepAliveTimeout
    /// is 0.
    pub keep_alive_ms: u64,
    /// `server.timeout`: a new connection's socket timeout.
    pub socket_ms: u64,
    /// `connectionsCheckingInterval`, for a check that runs here.
    pub check_interval_ms: u64,
    /// JS runs the headers / request check and handles socket timeouts
    /// (a node:http server); otherwise both happen here.
    pub js_driven: bool,
}

impl Default for TimeoutSettings {
    /// node's defaults.
    fn default() -> Self {
        TimeoutSettings {
            headers_ms: 60_000,
            request_ms: 300_000,
            keep_alive_ms: 5_000 + 1_000,
            socket_ms: 0,
            check_interval_ms: 30_000,
            js_driven: false,
        }
    }
}

/// A server's current settings, shared by its connections: node's timeouts,
/// and `maxConnections` with the count it is held against.
pub struct ServerTimeouts {
    headers: AtomicU64,
    request: AtomicU64,
    keep_alive: AtomicU64,
    socket: AtomicU64,
    /// node's `server.maxConnections` as the number JS compares with
    /// (`connections >= maxConnections` refuses a new one), stored as f64
    /// bits: infinity when it is not set, NaN (never refuses) for a value
    /// that is not a number.
    max_connections: AtomicU64,
    /// Connections being served (node's `server._connections`).
    connections: AtomicUsize,
    /// The server has an 'upgrade' listener: node hands a request that asks
    /// for an upgrade to it only then, and serves it as an ordinary request
    /// otherwise.
    upgrade_listener: AtomicBool,
    fixed: TimeoutSettings,
}

/// A connection counted against its server's `maxConnections`, uncounted
/// when dropped.
pub struct ConnSlot {
    timeouts: Arc<ServerTimeouts>,
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.timeouts.connections.fetch_sub(1, Ordering::AcqRel);
    }
}

impl ServerTimeouts {
    pub fn new(settings: TimeoutSettings) -> Arc<Self> {
        Arc::new(ServerTimeouts {
            headers: AtomicU64::new(settings.headers_ms),
            request: AtomicU64::new(settings.request_ms),
            keep_alive: AtomicU64::new(settings.keep_alive_ms),
            socket: AtomicU64::new(settings.socket_ms),
            max_connections: AtomicU64::new(f64::INFINITY.to_bits()),
            connections: AtomicUsize::new(0),
            upgrade_listener: AtomicBool::new(false),
            fixed: settings,
        })
    }

    /// Whether the server has an 'upgrade' listener, as JS last said.
    pub fn set_upgrade_listener(&self, listening: bool) {
        self.upgrade_listener.store(listening, Ordering::Relaxed);
    }

    pub fn upgrade_listener(&self) -> bool {
        self.upgrade_listener.load(Ordering::Relaxed)
    }

    /// `server.maxConnections`, as `Number(value)` (infinity when unset).
    pub fn set_max_connections(&self, limit: f64) {
        self.max_connections
            .store(limit.to_bits(), Ordering::Relaxed);
    }

    /// Count a new connection, or `None` when node would refuse it
    /// (`connections >= maxConnections`). There is no other limit: like
    /// node's, the server takes connections while the OS gives them.
    pub fn admit(self: &Arc<Self>) -> Option<ConnSlot> {
        let limit = f64::from_bits(self.max_connections.load(Ordering::Relaxed));
        let live = self.connections.fetch_add(1, Ordering::AcqRel);
        // `>=` is false for NaN, as in JS.
        if live as f64 >= limit {
            self.connections.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(ConnSlot {
            timeouts: Arc::clone(self),
        })
    }

    /// The server's properties as JS read them last.
    pub fn update(&self, headers_ms: u64, request_ms: u64, keep_alive_ms: u64, socket_ms: u64) {
        self.headers.store(headers_ms, Ordering::Relaxed);
        self.request.store(request_ms, Ordering::Relaxed);
        self.keep_alive.store(keep_alive_ms, Ordering::Relaxed);
        self.socket.store(socket_ms, Ordering::Relaxed);
    }

    pub fn headers_and_request_ms(&self) -> (u64, u64) {
        (
            self.headers.load(Ordering::Relaxed),
            self.request.load(Ordering::Relaxed),
        )
    }

    fn keep_alive_ms(&self) -> u64 {
        self.keep_alive.load(Ordering::Relaxed)
    }

    fn socket_ms(&self) -> u64 {
        self.socket.load(Ordering::Relaxed)
    }

    /// How often a check run here looks (node's interval is at least 1 ms).
    pub fn check_interval(&self) -> Duration {
        Duration::from_millis(self.fixed.check_interval_ms.max(1))
    }

    pub fn js_driven(&self) -> bool {
        self.fixed.js_driven
    }
}

/// Why a connection is being closed from outside hyper, weakest first: a
/// stronger reason given later takes over.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CloseReason {
    /// `socket.end()`: finish what is being written, then close (hyper's
    /// graceful shutdown).
    End,
    /// `socket.destroy()`, or a socket timeout nobody handled: close it.
    Destroy,
    /// headersTimeout / requestTimeout: answer 408 when no response head went
    /// out, then close.
    RequestTimeout,
}

/// A socket timeout that fired (node's socket 'timeout').
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fired {
    /// The request whose response was in flight, if any.
    pub request_id: Option<u64>,
    /// That request was all in -- node's `req.complete`, which decides
    /// whether 'timeout' reaches the request.
    pub request_complete: bool,
}

/// Where a connection is in node's terms.
struct Phase {
    /// A request is being received: node's "active" connections, the ones
    /// the headers / request check looks at. From the accept or a request's
    /// first byte until its body is all in.
    active: bool,
    /// A byte of the request being received has arrived (node's
    /// `on_message_begin` ran for it). Until then the clock runs from the
    /// accept, or from the end of the previous request.
    begun: bool,
    /// When the request being received began (the clock both checks use):
    /// the accept, then its first byte.
    message_start: Instant,
    /// Its headers are in.
    headers_complete: bool,
    /// Counts the requests received on the connection: the one being
    /// received is this one. A request's end is only taken for it when it
    /// carries the same number, so the end of a body read late cannot mark
    /// the request after it as all in.
    generation: u64,
    /// Requests dispatched whose response is not done.
    in_flight: u32,
    /// The latest of them (node's `parser.incoming` / `socket._httpMessage`).
    current_request: Option<u64>,
    /// Its generation, and whether it is all in.
    current_generation: u64,
    current_complete: bool,
    /// A response head went out for it (node: `_headerSent`), so a timeout
    /// closes the connection without a 408.
    response_started: bool,
    /// The socket timeout is the keep-alive timeout (node's
    /// `keepAliveTimeoutSet`); the next request's headers put `server.timeout`
    /// back.
    keep_alive_set: bool,
}

/// One connection's timeouts.
pub struct ConnWatch {
    pub id: u64,
    pub server_id: u64,
    timeouts: Arc<ServerTimeouts>,
    epoch: Instant,
    phase: Mutex<Phase>,
    /// Milliseconds after `epoch` of the last byte read or written.
    last_activity_ms: AtomicU64,
    /// The socket's inactivity timeout, 0 when off.
    socket_timeout_ms: AtomicU64,
    /// The socket timeout fired and there has been no activity since.
    fired: AtomicBool,
    timer_changed: Notify,
    close: Mutex<Option<CloseReason>>,
    close_notify: Notify,
    /// hyper holds bytes it has not written out yet: it wrote, or tried to,
    /// and has not flushed since. A connection handed to JS mid-stream (an
    /// upgrade) waits for them, or they would be lost with hyper's buffer.
    unflushed: AtomicBool,
    flushed: Notify,
}

impl ConnWatch {
    /// A connection accepted now: its first request's clock starts, and its
    /// socket timeout is the server's `timeout`.
    pub fn new(id: u64, server_id: u64, timeouts: Arc<ServerTimeouts>) -> Arc<Self> {
        let now = Instant::now();
        let socket_ms = timeouts.socket_ms();
        Arc::new(ConnWatch {
            id,
            server_id,
            timeouts,
            epoch: now,
            phase: Mutex::new(Phase {
                active: true,
                begun: false,
                message_start: now,
                headers_complete: false,
                generation: 0,
                in_flight: 0,
                current_request: None,
                current_generation: 0,
                current_complete: false,
                response_started: false,
                keep_alive_set: false,
            }),
            last_activity_ms: AtomicU64::new(0),
            socket_timeout_ms: AtomicU64::new(socket_ms),
            fired: AtomicBool::new(false),
            timer_changed: Notify::new(),
            close: Mutex::new(None),
            close_notify: Notify::new(),
            unflushed: AtomicBool::new(false),
            flushed: Notify::new(),
        })
    }

    fn phase(&self) -> std::sync::MutexGuard<'_, Phase> {
        self.phase.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn touch(&self) {
        self.last_activity_ms
            .store(self.now_ms(), Ordering::Relaxed);
        if self.fired.swap(false, Ordering::AcqRel) {
            self.timer_changed.notify_one();
        }
    }

    /// Bytes arrived. The first byte of a request starts its clock again
    /// (node's `on_message_begin` resets the start the accept set), and the
    /// first byte after a request was all in begins the next one.
    pub fn note_read(&self, n: usize) {
        if n == 0 {
            return;
        }
        self.touch();
        let mut phase = self.phase();
        if !phase.begun {
            phase.begun = true;
            phase.active = true;
            phase.generation += 1;
            phase.message_start = Instant::now();
            phase.headers_complete = false;
        }
    }

    /// Bytes went out.
    pub fn note_write(&self, n: usize) {
        if n > 0 {
            self.touch();
        }
    }

    /// A write left bytes to flush (it wrote some, or could not write).
    fn note_unflushed(&self) {
        self.unflushed.store(true, Ordering::Release);
    }

    /// Everything written so far is out.
    fn note_flushed(&self) {
        if self.unflushed.swap(false, Ordering::AcqRel) {
            self.flushed.notify_waiters();
        }
    }

    /// Whether written bytes still wait for a flush.
    pub fn unflushed(&self) -> bool {
        self.unflushed.load(Ordering::Acquire)
    }

    /// Resolves at the next flush that empties hyper's buffer.
    pub fn next_flush(&self) -> tokio::sync::futures::Notified<'_> {
        self.flushed.notified()
    }

    /// Resolves once nothing waits for a flush.
    pub async fn flushed(&self) {
        loop {
            let flushed = self.flushed.notified();
            if !self.unflushed() {
                return;
            }
            flushed.await;
        }
    }

    /// A request's headers are in and it is being dispatched as
    /// `request_id`. Returns its generation, for [`ConnWatch::message_complete`].
    ///
    /// A head whose first byte was not seen as the start of a request -- it
    /// came in the same read as the previous body's end, or was read while
    /// that body was still being taken -- starts its request now.
    pub fn headers_complete(&self, request_id: u64) -> u64 {
        let mut phase = self.phase();
        if !phase.begun || phase.headers_complete {
            phase.begun = true;
            phase.active = true;
            phase.generation += 1;
            phase.message_start = Instant::now();
        }
        phase.headers_complete = true;
        phase.in_flight += 1;
        phase.current_request = Some(request_id);
        phase.current_generation = phase.generation;
        phase.current_complete = false;
        phase.response_started = false;
        let generation = phase.generation;
        // node's resetSocketTimeout: `socket.setTimeout(server.timeout || 0)`.
        if phase.keep_alive_set {
            phase.keep_alive_set = false;
            drop(phase);
            self.set_socket_timeout(self.timeouts.socket_ms());
        }
        generation
    }

    /// Request `generation` is all in (or its body is no longer read): the
    /// headers / request check is done with it, unless a later request is
    /// already being received.
    pub fn message_complete(&self, generation: u64) {
        let mut phase = self.phase();
        if phase.current_generation == generation {
            phase.current_complete = true;
        }
        if phase.generation == generation {
            phase.active = false;
            phase.begun = false;
        }
    }

    /// The response head for the current request went out.
    pub fn response_started(&self) {
        self.phase().response_started = true;
    }

    /// A response is done. With none left in flight, the socket timeout
    /// becomes the keep-alive timeout (node's resOnFinish).
    pub fn response_finished(&self) {
        let mut phase = self.phase();
        phase.in_flight = phase.in_flight.saturating_sub(1);
        if phase.in_flight > 0 {
            return;
        }
        phase.current_request = None;
        phase.response_started = false;
        let keep_alive = self.timeouts.keep_alive_ms();
        if keep_alive > 0 {
            phase.keep_alive_set = true;
            drop(phase);
            self.set_socket_timeout(keep_alive);
        }
    }

    /// `socket.setTimeout(ms)`: the inactivity timeout, counted from now.
    pub fn set_socket_timeout(&self, ms: u64) {
        self.socket_timeout_ms.store(ms, Ordering::Relaxed);
        self.last_activity_ms
            .store(self.now_ms(), Ordering::Relaxed);
        self.fired.store(false, Ordering::Release);
        self.timer_changed.notify_one();
    }

    /// The request whose response is in flight, if any.
    pub fn current_request(&self) -> Option<u64> {
        self.phase().current_request
    }

    /// JS handles this connection's timeouts and closed exchanges.
    pub fn js_driven(&self) -> bool {
        self.timeouts.js_driven()
    }

    /// Whether a 408 may still be written: no response head went out.
    pub fn may_answer(&self) -> bool {
        !self.phase().response_started
    }

    /// Close the connection. A stronger reason than one already given
    /// takes over (a destroy after an end, a request timeout after either).
    pub fn close(&self, reason: CloseReason) {
        {
            let mut close = self.close.lock().unwrap_or_else(|e| e.into_inner());
            if close.is_none_or(|current| reason > current) {
                *close = Some(reason);
            }
        }
        self.close_notify.notify_one();
    }

    /// Resolves once [`ConnWatch::close`] was called with `at_least` or a
    /// stronger reason, with the reason.
    pub async fn closed(&self, at_least: CloseReason) -> CloseReason {
        loop {
            let notified = self.close_notify.notified();
            if let Some(reason) = *self.close.lock().unwrap_or_else(|e| e.into_inner())
                && reason >= at_least
            {
                return reason;
            }
            notified.await;
        }
    }

    /// node's headers / request check for this connection at `now`: true when
    /// it is due, and then it is not checked again for this request. Like
    /// node's `ConnectionsList::Expired`, a headers timeout longer than the
    /// request timeout swaps with it.
    pub fn expire(&self, headers_ms: u64, request_ms: u64, now: Instant) -> bool {
        let (headers_ms, request_ms) = if request_ms > 0 && headers_ms > request_ms {
            (request_ms, headers_ms)
        } else {
            (headers_ms, request_ms)
        };
        let mut phase = self.phase();
        if !phase.active {
            return false;
        }
        let elapsed = u64::try_from(
            now.saturating_duration_since(phase.message_start)
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        let due = (!phase.headers_complete && headers_ms > 0 && elapsed >= headers_ms)
            || (request_ms > 0 && elapsed >= request_ms);
        if due {
            phase.active = false;
        }
        due
    }

    /// Resolves when the socket timeout expires, with the request in flight
    /// then (if any). It does not fire again until there is activity or a
    /// new timeout is set. Cancel-safe: nothing changes until it resolves.
    pub async fn next_timeout(&self) -> Fired {
        loop {
            let changed = self.timer_changed.notified();
            let timeout = self.socket_timeout_ms.load(Ordering::Relaxed);
            if timeout == 0 || self.fired.load(Ordering::Acquire) {
                changed.await;
                continue;
            }
            let deadline = self
                .last_activity_ms
                .load(Ordering::Relaxed)
                .saturating_add(timeout);
            let now = self.now_ms();
            if now >= deadline {
                self.fired.store(true, Ordering::Release);
                let phase = self.phase();
                return Fired {
                    request_id: phase.current_request,
                    request_complete: phase.current_complete,
                };
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(deadline - now)) => {}
                _ = changed => {}
            }
        }
    }
}

/// Close a connection whose hyper side is gone: for a request timeout,
/// node's 408 first when `may_answer` (no response head went out; read
/// before hyper's side was dropped, which ends the response).
pub async fn finish_close<S: AsyncWrite + Unpin>(
    mut stream: S,
    reason: CloseReason,
    may_answer: bool,
) {
    if reason == CloseReason::RequestTimeout && may_answer {
        let _ = tokio::time::timeout(FAREWELL_BUDGET, async {
            stream.write_all(REQUEST_TIMEOUT_RESPONSE).await?;
            stream.shutdown().await
        })
        .await;
    }
}

/// The connection's stream, reporting its reads and writes to the watch.
pub struct WatchedIo<S> {
    inner: S,
    watch: Arc<ConnWatch>,
}

impl<S> WatchedIo<S> {
    pub fn new(inner: S, watch: Arc<ConnWatch>) -> Self {
        WatchedIo { inner, watch }
    }

    pub fn into_inner(self) -> S {
        self.inner
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for WatchedIo<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &polled {
            self.watch.note_read(buf.filled().len() - before);
        }
        polled
    }
}

impl<S> WatchedIo<S> {
    fn note_write_poll(&self, polled: &Poll<io::Result<usize>>) {
        match polled {
            Poll::Ready(Ok(n)) => {
                self.watch.note_write(*n);
                if *n > 0 {
                    self.watch.note_unflushed();
                }
            }
            Poll::Pending => self.watch.note_unflushed(),
            Poll::Ready(Err(_)) => {}
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for WatchedIo<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let polled = Pin::new(&mut self.inner).poll_write(cx, data);
        self.note_write_poll(&polled);
        polled
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let polled = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        self.note_write_poll(&polled);
        polled
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let polled = Pin::new(&mut self.inner).poll_flush(cx);
        // hyper flushes the stream only once its own buffer is written out.
        if let Poll::Ready(Ok(())) = &polled {
            self.watch.note_flushed();
        }
        polled
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch(settings: TimeoutSettings) -> Arc<ConnWatch> {
        ConnWatch::new(1, 1, ServerTimeouts::new(settings))
    }

    /// A connection that sends nothing is due for headersTimeout from its
    /// accept; once its headers are in only requestTimeout applies, and a
    /// request whose body is in is never due.
    #[test]
    fn the_headers_and_request_checks_follow_the_message() {
        // The first byte starts the clock again.
        let w = watch(TimeoutSettings::default());
        let accepted = w.phase().message_start;
        std::thread::sleep(Duration::from_millis(20));
        w.note_read(1);
        let first_byte = w.phase().message_start;
        assert!(first_byte > accepted);
        w.note_read(1);
        assert_eq!(w.phase().message_start, first_byte, "only the first byte");

        let w = watch(TimeoutSettings::default());
        let start = w.phase().message_start;
        assert!(!w.expire(1000, 2000, start + Duration::from_millis(999)));
        assert!(w.expire(1000, 2000, start + Duration::from_millis(1000)));
        // Checked once: not due again for the same request.
        assert!(!w.expire(1000, 2000, start + Duration::from_millis(5000)));

        let w = watch(TimeoutSettings::default());
        w.note_read(10);
        let start = w.phase().message_start;
        let _ = w.headers_complete(7);
        assert_eq!(w.phase().message_start, start, "the headers keep the clock");
        assert!(!w.expire(1000, 2000, start + Duration::from_millis(1500)));
        assert!(w.expire(1000, 2000, start + Duration::from_millis(2000)));

        let w = watch(TimeoutSettings::default());
        let start = w.phase().message_start;
        let generation = w.headers_complete(7);
        w.message_complete(generation);
        assert!(!w.expire(1000, 2000, start + Duration::from_secs(60)));
        // The next request's first byte starts a new clock.
        w.note_read(1);
        let second = w.phase().message_start;
        assert!(second >= start);
        assert!(!w.expire(1000, 0, second + Duration::from_millis(999)));
        assert!(w.expire(1000, 0, second + Duration::from_millis(1000)));
        // 0 turns a check off.
        let w = watch(TimeoutSettings::default());
        let start = w.phase().message_start;
        assert!(!w.expire(0, 0, start + Duration::from_secs(3600)));
        // A headers timeout over the request timeout swaps with it.
        let w = watch(TimeoutSettings::default());
        let start = w.phase().message_start;
        assert!(w.expire(2000, 1000, start + Duration::from_millis(1000)));
    }

    /// The end of a body read late belongs to its own request: a request
    /// whose head came before it stays under the check.
    #[test]
    fn a_late_body_end_does_not_complete_the_next_request() {
        let w = watch(TimeoutSettings::default());
        w.note_read(100);
        let first = w.headers_complete(1);
        // The next head was parsed before the first body's pump finished.
        let second = w.headers_complete(2);
        assert_ne!(first, second);
        let second_start = w.phase().message_start;
        w.message_complete(first);
        assert!(
            w.phase().active,
            "the second request is still being received"
        );
        assert!(w.expire(0, 1000, second_start + Duration::from_millis(1000)));
        // Its own end completes it.
        let w = watch(TimeoutSettings::default());
        let first = w.headers_complete(1);
        w.message_complete(first);
        assert!(!w.phase().active);
        assert!(w.phase().current_complete);
    }

    /// The socket timeout becomes keepAliveTimeout + buffer once no response
    /// is in flight, and server.timeout again at the next request's headers.
    #[test]
    fn the_keep_alive_timeout_replaces_the_socket_timeout_between_requests() {
        let timeouts = ServerTimeouts::new(TimeoutSettings {
            socket_ms: 700,
            keep_alive_ms: 1500,
            ..TimeoutSettings::default()
        });
        let w = ConnWatch::new(1, 1, timeouts.clone());
        assert_eq!(w.socket_timeout_ms.load(Ordering::Relaxed), 700);
        let _ = w.headers_complete(1);
        let _ = w.headers_complete(2);
        assert_eq!(w.current_request(), Some(2));
        w.response_finished();
        assert_eq!(
            w.socket_timeout_ms.load(Ordering::Relaxed),
            700,
            "one still in flight"
        );
        w.response_finished();
        assert_eq!(w.current_request(), None);
        assert_eq!(w.socket_timeout_ms.load(Ordering::Relaxed), 1500);
        timeouts.update(60_000, 300_000, 1500, 900);
        let _ = w.headers_complete(3);
        assert_eq!(w.socket_timeout_ms.load(Ordering::Relaxed), 900);
        // keepAliveTimeout 0 leaves the socket timeout alone.
        timeouts.update(60_000, 300_000, 0, 900);
        w.set_socket_timeout(250);
        w.response_finished();
        assert_eq!(w.socket_timeout_ms.load(Ordering::Relaxed), 250);
    }

    /// A 408 is owed only while no response head for the current request
    /// went out.
    #[test]
    fn a_408_is_owed_until_a_response_starts() {
        let w = watch(TimeoutSettings::default());
        assert!(w.may_answer());
        let _ = w.headers_complete(1);
        w.response_started();
        assert!(!w.may_answer());
        w.response_finished();
        assert!(w.may_answer());
    }

    #[tokio::test]
    async fn the_socket_timeout_fires_once_until_there_is_activity() {
        let w = watch(TimeoutSettings {
            socket_ms: 150,
            ..TimeoutSettings::default()
        });
        let started = Instant::now();
        assert_eq!(w.next_timeout().await.request_id, None);
        assert!(started.elapsed() >= Duration::from_millis(150));
        // Fired: no second event without activity.
        let again = tokio::time::timeout(Duration::from_millis(400), w.next_timeout()).await;
        assert!(again.is_err());
        // Activity re-arms it.
        w.note_write(10);
        let _ = w.headers_complete(4);
        let rearmed = Instant::now();
        let fired = w.next_timeout().await;
        assert_eq!(fired.request_id, Some(4));
        assert!(!fired.request_complete);
        assert!(rearmed.elapsed() >= Duration::from_millis(100));
        // 0 turns it off.
        w.set_socket_timeout(0);
        let off = tokio::time::timeout(Duration::from_millis(400), w.next_timeout()).await;
        assert!(off.is_err());
    }

    /// No limit until maxConnections is set; then `connections >= limit`
    /// refuses, as node compares it, and a slot counts until it drops.
    #[test]
    fn max_connections_is_the_only_limit() {
        let timeouts = ServerTimeouts::new(TimeoutSettings::default());
        let unlimited: Vec<ConnSlot> = (0..1000).filter_map(|_| timeouts.admit()).collect();
        assert_eq!(unlimited.len(), 1000);
        drop(unlimited);
        timeouts.set_max_connections(2.0);
        let a = timeouts.admit().expect("first");
        let b = timeouts.admit().expect("second");
        assert!(timeouts.admit().is_none(), "2 >= 2 refuses");
        drop(a);
        let c = timeouts.admit().expect("a slot came free");
        assert!(timeouts.admit().is_none());
        // A fractional limit: 2 >= 1.5 refuses, 1 >= 1.5 does not.
        timeouts.set_max_connections(1.5);
        assert!(timeouts.admit().is_none());
        drop(b);
        let d = timeouts.admit().expect("1 of 1.5");
        // NaN (a value that is not a number) never refuses; 0 always does.
        timeouts.set_max_connections(f64::NAN);
        assert!(timeouts.admit().is_some());
        timeouts.set_max_connections(0.0);
        drop((c, d));
        assert!(timeouts.admit().is_none());
        timeouts.set_max_connections(f64::INFINITY);
        assert!(timeouts.admit().is_some());
    }

    #[tokio::test]
    async fn close_resolves_with_the_strongest_reason() {
        let w = watch(TimeoutSettings::default());
        w.close(CloseReason::End);
        assert_eq!(w.closed(CloseReason::End).await, CloseReason::End);
        let later =
            tokio::time::timeout(Duration::from_millis(100), w.closed(CloseReason::Destroy)).await;
        assert!(later.is_err(), "an end is not a destroy");
        w.close(CloseReason::RequestTimeout);
        w.close(CloseReason::Destroy);
        assert_eq!(
            w.closed(CloseReason::Destroy).await,
            CloseReason::RequestTimeout
        );
    }
}
