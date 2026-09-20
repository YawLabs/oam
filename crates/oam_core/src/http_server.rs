//! Inbound HTTP/1.1 server on hyper (the same hyper the fetch transport's
//! client, `http_client`, is built on).
//!
//! Flow: `http_serve` binds a tokio listener and spawns an accept loop;
//! each hyper request collects its body (wave-1: BUFFERED, capped — most
//! API payloads are small; streaming request bodies land later), parks a
//! oneshot responder, and pushes metadata onto the server's queue. JS
//! long-polls `http_accept` — the pending accept op is what keeps the
//! event loop alive while listening, exactly Node's semantics. Responses
//! come back either as one full buffer or as a CHANNEL BODY the JS side
//! pushes chunks into (the SSE/token-streaming path).
//!
//! Hardening (after the M2 safety fleet): a global RETAINED-BODY BUDGET
//! bounds buffered upload memory, the streaming push has a STALL TIMEOUT so
//! a half-open client can't wedge the pump, and close() does a GRACEFUL
//! shutdown (in-flight requests finish, keep-alive is disabled) instead of
//! resetting live connections. Every HTTP/1 connection is held to node's
//! server timeouts (`http_conn`): headersTimeout / requestTimeout,
//! keepAliveTimeout and the socket timeout. Like node's, a server has no
//! connection limit of its own (a fixed one let a few hundred held
//! connections keep everyone else out); `server.maxConnections` sets one.

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Frame;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use crate::http_conn::{CloseReason, ConnWatch, Fired, ServerTimeouts, TimeoutSettings, WatchedIo};
use crate::http_head::{HeadError, HeadPolicy, ParsedHead};

// http2.createSecureServer's connections, served over node:tls.
mod secure;
pub use secure::{SecureSession, http2_serve_tls};

/// Per-request body cap (wave-1 buffered bodies).
const MAX_REQUEST_BODY: usize = 100 * 1024 * 1024;
/// After the per-request cap is hit we drain (discard) up to this many
/// additional bytes before returning the 413 response.  The drain serves
/// a single purpose: on Windows, dropping a TcpStream that still has
/// unread data in the kernel recv-buffer causes an immediate TCP RST.
/// That RST races with the 413 response bytes in the send-buffer, so the
/// client reads "connection reset" instead of "413".  Draining clears the
/// recv-buffer, the connection closes with a clean FIN, and the client
/// reads the 413 first.  The drain is capped so an adversarial client
/// cannot pin the connection indefinitely; anything beyond DRAIN_BUDGET
/// bytes above the cap is still rejected with RST (and the test body is
/// sized to stay under the drain ceiling -- see e2e.rs).
const DRAIN_BUDGET: usize = 16 * 1024 * 1024; // 16 MB post-cap drain
/// Aggregate cap on RETAINED request-body bytes across all in-flight
/// requests of the run — bounds the body-buffer memory a flood of uploads
/// can pin.
///
/// This is the SECOND line of defence, not the first. Streaming bodies are
/// bounded per-request by the chunk channel's capacity, so N concurrent
/// unread uploads pin N * capacity chunks regardless of how large the
/// bodies are. The budget is what bounds the AGGREGATE once N itself grows
/// large. Both are needed: backpressure alone scales with connection count,
/// and a connection count alone says nothing about bytes.
const DEFAULT_GLOBAL_BODY_BUDGET: usize = 512 * 1024 * 1024;

/// Parse `OAM_MAX_BODY_BYTES` into the aggregate body budget.
///
/// Same shape as `OAM_MAX_HEAP_MB`: unset, empty, non-numeric or `0` means
/// "use the default". Set it to match a container memory limit, or low to
/// exercise the load-shedding path.
fn global_body_budget() -> usize {
    static CACHED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("OAM_MAX_BODY_BYTES")
            .ok()
            .and_then(|raw| raw.trim().parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT_GLOBAL_BODY_BUDGET)
    })
}
/// How long an accept loop waits after the OS refused an accept (out of file
/// descriptors, say) before it tries again, instead of spinning on the
/// error.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(10);
/// How long a connection being handed to an 'upgrade' / 'connect' listener
/// may take to write out what hyper still holds for it (a previous response
/// on the connection, to a client that is not reading) before it is closed
/// instead.
const TAKEOVER_FLUSH_BUDGET: Duration = Duration::from_secs(10);
/// A single streaming-response chunk that cannot be delivered within this
/// window means the consumer is gone or wedged (a half-open socket the OS
/// hasn't reset yet): end the stream so the JS pump never parks forever.
const STREAM_PUSH_TIMEOUT: Duration = Duration::from_secs(60);

pub struct IncomingRequest {
    pub id: u64,
    pub method: String,
    /// Path + query, as received.
    pub uri: String,
    pub headers: Vec<(String, String)>,
    pub is_upgrade: bool,
    pub socket_handle: Option<u64>,
    /// Both ends of the connection the request arrived on.
    pub conn: ConnAddrs,
    /// The connection's id for `httpConnSetTimeout` / `httpConnDestroy`
    /// (an HTTP/1 connection held to node's timeouts).
    pub conn_id: Option<u64>,
    /// For an upgrade or CONNECT: the bytes that came after the head (node's
    /// `head` argument), already read off the socket.
    pub head: Vec<u8>,
    /// The request has no body: an HTTP/2 request whose HEADERS frame ended
    /// the stream (node's `endAfterHeaders`, END_STREAM in 'stream' flags).
    pub end_stream: bool,
    /// On an https server: what the connection's TLS handshake settled
    /// (`HandshakeInfo::to_json`), which `req.socket` reports --
    /// `authorized`, `authorizationError`, `getPeerCertificate()`,
    /// `alpnProtocol`, `getProtocol()`, ... Shared by every request on the
    /// connection.
    pub tls: Option<Arc<serde_json::Value>>,
}

/// What a server's accept queue carries to JS.
pub enum ServerEvent {
    Request(IncomingRequest),
    /// An https server's connection whose TLS handshake failed (node's
    /// 'tlsClientError', which the https server passes on as 'clientError'):
    /// node's code for it, when it has one, and the message.
    TlsClientError {
        conn: ConnAddrs,
        code: Option<String>,
        message: String,
    },
    /// A connection's socket timeout expired (node's socket 'timeout'): JS
    /// emits 'timeout' and destroys the connection when nobody listens.
    Timeout {
        conn_id: u64,
        fired: Fired,
        conn: ConnAddrs,
    },
    /// An https connection past its handshake, before anything on it is
    /// parsed as HTTP (node's 'secureConnection'). The connection waits for
    /// `httpConnResume` before it is served, so the listeners run where node
    /// runs them -- one that destroys the socket stops the request reaching
    /// the handler.
    SecureConnection {
        conn_id: u64,
        conn: ConnAddrs,
        /// What the handshake settled (`HandshakeInfo::to_json`): the same
        /// record the connection's requests carry.
        tls: Arc<serde_json::Value>,
    },
    /// An https connection is over: JS lets go of the socket it handed to
    /// 'secureConnection' and closes it (node's socket 'close').
    ConnectionClosed {
        conn_id: u64,
    },
    /// A connection refused under `server.maxConnections` (node's 'drop').
    Drop {
        conn: ConnAddrs,
    },
    /// An exchange ended without its response (the connection was closed
    /// under it: a request timeout, a destroyed socket, a lost client): JS
    /// aborts the request and closes the response (node's abortIncoming;
    /// 'close' without 'finish').
    Closed {
        request_id: u64,
    },
}

/// Both ends of an accepted connection, as the OS reports them.
///
/// node's `req.socket` carries these (`remoteAddress` / `remotePort` /
/// `remoteFamily`, `localAddress` / `localPort` / `localFamily`), and they
/// are what an application's access decisions key on: loopback-only admin
/// routes, `trust proxy` settings (proxy-addr reads `remoteAddress`), per-IP
/// allow lists and rate limits. They are taken from the accepted socket
/// itself, never from anything the client sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnAddrs {
    pub remote: std::net::SocketAddr,
    /// `None` only if the OS could not report the accepted socket's own
    /// address (node leaves the fields undefined then too).
    pub local: Option<std::net::SocketAddr>,
}

impl ConnAddrs {
    /// The accept meta fields, node's names and shapes.
    fn write_meta(&self, meta: &mut serde_json::Value) {
        meta["remoteAddress"] = serde_json::json!(node_ip_string(&self.remote));
        meta["remotePort"] = serde_json::json!(self.remote.port());
        meta["remoteFamily"] = serde_json::json!(node_family(&self.remote));
        if let Some(local) = &self.local {
            meta["localAddress"] = serde_json::json!(node_ip_string(local));
            meta["localPort"] = serde_json::json!(local.port());
            meta["localFamily"] = serde_json::json!(node_family(local));
        }
    }
}

/// `IPv4` / `IPv6` from the socket's address family, as node reports it. An
/// IPv4 client of a dual-stack `::` listener arrives as a v4-mapped IPv6
/// address, and node reports that as `IPv6` -- so the family comes from the
/// socket address, never from the address's IPv4-ness.
fn node_family(addr: &std::net::SocketAddr) -> &'static str {
    match addr {
        std::net::SocketAddr::V4(_) => "IPv4",
        std::net::SocketAddr::V6(_) => "IPv6",
    }
}

/// An address as node's `remoteAddress` / `localAddress` spell it
/// (src/tcp_wrap.cc AddressToJS): `inet_ntop` text, a v4-mapped IPv6 address
/// left mapped (`::ffff:127.0.0.1`; Rust's `Ipv6Addr` Display writes that
/// form too), and a link-local IPv6 address with a nonzero scope id
/// suffixed `%<interface>` -- the index on Windows, the interface name on
/// POSIX (`uv_if_indextoiid`).
pub fn node_ip_string(addr: &std::net::SocketAddr) -> String {
    match addr {
        std::net::SocketAddr::V4(v4) => v4.ip().to_string(),
        std::net::SocketAddr::V6(v6) => {
            let ip = v6.ip();
            let scope = v6.scope_id();
            if scope != 0 && ip.is_unicast_link_local() {
                format!("{ip}%{}", interface_id(scope))
            } else {
                ip.to_string()
            }
        }
    }
}

/// `uv_if_indextoiid`: the scope id itself on Windows, the interface name on
/// POSIX. Linux names come from sysfs; where the name cannot be found (or
/// on a POSIX system without sysfs, macOS among them) the index is written,
/// which still names the interface unambiguously.
fn interface_id(scope: u32) -> String {
    #[cfg(target_os = "linux")]
    if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
        for entry in entries.flatten() {
            let index = std::fs::read_to_string(entry.path().join("ifindex"));
            if index.ok().and_then(|s| s.trim().parse::<u32>().ok()) == Some(scope) {
                return entry.file_name().to_string_lossy().into_owned();
            }
        }
    }
    scope.to_string()
}

/// An inbound request body as the JS side will consume it.
///
/// Slice 1 of docs/design/streaming-bodies.md: every request is still
/// `Full`, so behavior is unchanged. The variant exists so the streaming
/// path has a seam to land on -- `Stream` will carry a chunk receiver and
/// the handler will be dispatched on headers rather than on last byte.
/// A queued request-body chunk that carries its own reservation against
/// GLOBAL_BODY_BUDGET and refunds it when dropped.
///
/// A drop guard rather than manual refunds: a chunk leaves the queue several
/// ways -- read by JS, discarded by `_dump`, dropped when the consumer
/// cancels, or dropped with the whole channel when the connection dies -- and
/// a refund missed on any one path leaks budget until the process restarts,
/// which fails CLOSED (every later upload rejected as busy).
pub struct BudgetedChunk {
    data: Vec<u8>,
    /// Held separately: `into_data` moves `data` out, and Drop still has to
    /// refund the original size.
    len: usize,
    state: std::sync::Arc<HttpState>,
}

impl BudgetedChunk {
    fn new(data: Vec<u8>, state: std::sync::Arc<HttpState>) -> Self {
        let len = data.len();
        Self { data, len, state }
    }

    /// Take the bytes. The reservation is still refunded when the husk drops.
    pub fn into_data(mut self) -> Vec<u8> {
        std::mem::take(&mut self.data)
    }
}

impl Drop for BudgetedChunk {
    fn drop(&mut self) {
        self.state.body_bytes.fetch_sub(self.len, Ordering::AcqRel);
    }
}

pub enum RequestBody {
    /// Collected up front, subject to MAX_REQUEST_BODY + GLOBAL_BODY_BUDGET.
    Full(Vec<u8>),
    /// Chunks as they arrive. The handler is dispatched on headers, so 413
    /// is no longer available -- exceeding MAX_REQUEST_BODY delivers Err on
    /// this channel instead, and the connection is torn down.
    ///
    /// Taken out for the duration of each read await and reinserted after,
    /// the same remove-await-reinsert the accept queue uses; the JS side is
    /// the single consumer.
    Stream(mpsc::Receiver<Result<BudgetedChunk, String>>),
    /// The receiver is checked out by an in-flight read. The entry STAYS in
    /// the registry so a miss can be told apart from "no such body" -- a
    /// bare removal made a checked-out stream look absent, and the buffered
    /// fallback then reported EOF while frames were still queued.
    StreamPending,
}

/// Outcome of checking out a streamed body receiver.
pub enum BodyCheckout {
    Ready(mpsc::Receiver<Result<BudgetedChunk, String>>),
    /// Another read holds it; the caller must NOT treat this as EOF.
    InFlight,
    /// No streamed body for this id (buffered, or already finished).
    Absent,
}

pub enum ResponseBody {
    Full(Vec<u8>),
    /// Chunk channel plus a drop-signal: the oneshot sender rides inside
    /// ChannelBody, so dropping the body (finished OR connection lost)
    /// resolves the paired stream_watch receiver.
    Stream(mpsc::Receiver<Vec<u8>>, oneshot::Sender<()>),
    /// JS destroyed the request without responding (req.destroy()): the
    /// connection is torn down instead of synthesizing a response, so the
    /// client observes a connection error (Node's socket-destroy semantics).
    Abort,
}

pub struct ResponseSpec {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: ResponseBody,
}

struct ServerEntry {
    /// Taken out for the duration of each accept await (remove-await-
    /// reinsert; the JS accept loop is the single consumer).
    queue: Option<mpsc::Receiver<ServerEvent>>,
    /// watch (not oneshot): every CONNECTION task selects on it too.
    /// Keep-alive sockets (a client pool can idle one for 90s) would
    /// otherwise hold queue_tx clones long after close, leaving the
    /// pending accept op pinning the event loop open.
    shutdown: Option<tokio::sync::watch::Sender<bool>>,
    /// node's timeout settings for the server's connections (none for the
    /// http2 server).
    timeouts: Option<Arc<ServerTimeouts>>,
    /// An https server's TLS side, which JS replaces for the connections
    /// accepted from then on (`https_set_tls`).
    tls: Option<Arc<HttpsTls>>,
}

/// An https server's TLS side: its secure context (`tls::server`, built at
/// `https.createServer()`) and what each connection is accepted with --
/// `requestCert`, `rejectUnauthorized`, the ALPN list, `handshakeTimeout` --
/// read for every connection, as node's tlsConnectionListener reads them off
/// the server. `server.setSecureContext()` and a changed option swap it;
/// connections already accepted keep what they were accepted with.
pub struct HttpsTls(
    Mutex<(
        Arc<crate::tls::server::ServerContext>,
        crate::tls::server::AcceptOptions,
    )>,
);

impl HttpsTls {
    pub fn new(
        context: Arc<crate::tls::server::ServerContext>,
        options: crate::tls::server::AcceptOptions,
    ) -> Self {
        HttpsTls(Mutex::new((context, options)))
    }

    /// What the next connection is accepted with.
    fn current(
        &self,
    ) -> (
        Arc<crate::tls::server::ServerContext>,
        crate::tls::server::AcceptOptions,
    ) {
        let guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        (Arc::clone(&guard.0), guard.1.clone())
    }

    fn replace(
        &self,
        context: Arc<crate::tls::server::ServerContext>,
        options: crate::tls::server::AcceptOptions,
    ) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = (context, options);
    }
}

#[derive(Default)]
pub struct HttpState {
    next: AtomicU64,
    servers: Mutex<HashMap<u64, ServerEntry>>,
    /// connection id -> the timeouts of an HTTP/1 connection being served.
    conns: Mutex<HashMap<u64, Arc<ConnWatch>>>,
    /// request id -> the hyper-side responder waiting for JS.
    pending: Mutex<HashMap<u64, oneshot::Sender<ResponseSpec>>>,
    /// request id -> request body (fetched once by JS).
    bodies: Mutex<HashMap<u64, RequestBody>>,
    /// request id -> the trailer fields of its chunked body (node's
    /// `req.trailers`), from when the body's end is read until JS takes them
    /// at that end. Kept only while the body's entry is: whatever removes the
    /// body removes these (lock order: `bodies`, then this).
    trailers: Mutex<HashMap<u64, Vec<(String, String)>>>,
    /// response-stream id -> chunk sender (JS pushes, hyper drains).
    streams: Mutex<HashMap<u64, mpsc::Sender<Vec<u8>>>>,
    /// response-stream id -> resolves when hyper drops the response body
    /// (normal completion OR connection loss). httpStreamClosed takes the
    /// receiver; JS tells the two cases apart via its own finished flag.
    stream_watch: Mutex<HashMap<u64, oneshot::Receiver<()>>>,
    /// Retained request-body bytes across all in-flight requests; reserved
    /// when a body is buffered, refunded when the request finishes.
    body_bytes: AtomicUsize,
}

impl HttpState {
    fn next_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Start tracking a connection's timeouts; it is forgotten when the
    /// returned guard drops (the connection task ends, or the socket went to
    /// JS as an upgrade).
    fn register_conn(self: &Arc<Self>, watch: Arc<ConnWatch>) -> ConnRegistration {
        let id = watch.id;
        self.conns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, watch);
        ConnRegistration {
            state: Arc::clone(self),
            id,
        }
    }

    fn conn(&self, conn_id: u64) -> Option<Arc<ConnWatch>> {
        self.conns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&conn_id)
            .cloned()
    }

    /// node's checkConnections for `server_id`: every connection whose
    /// request did not arrive within `headers_ms` / `requestTimeout` is
    /// answered 408 and closed. Returns how many.
    pub fn expire_connections(&self, server_id: u64, headers_ms: u64, request_ms: u64) -> usize {
        if headers_ms == 0 && request_ms == 0 {
            return 0;
        }
        let now = tokio::time::Instant::now();
        let due: Vec<Arc<ConnWatch>> = self
            .conns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|w| w.server_id == server_id && w.expire(headers_ms, request_ms, now))
            .cloned()
            .collect();
        for watch in &due {
            watch.close(CloseReason::RequestTimeout);
        }
        due.len()
    }

    /// The server's timeout properties, as JS read them.
    pub fn update_timeouts(
        &self,
        server_id: u64,
        headers_ms: u64,
        request_ms: u64,
        keep_alive_ms: u64,
        socket_ms: u64,
    ) {
        if let Some(timeouts) = self
            .servers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&server_id)
            .and_then(|entry| entry.timeouts.clone())
        {
            timeouts.update(headers_ms, request_ms, keep_alive_ms, socket_ms);
        }
    }

    /// `server.maxConnections`, as `Number(value)` (infinity when unset).
    pub fn set_max_connections(&self, server_id: u64, limit: f64) {
        if let Some(timeouts) = self
            .servers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&server_id)
            .and_then(|entry| entry.timeouts.clone())
        {
            timeouts.set_max_connections(limit);
        }
    }

    /// Whether the server has an 'upgrade' listener (node hands an upgrade
    /// request to it only then).
    pub fn set_upgrade_listener(&self, server_id: u64, listening: bool) {
        if let Some(timeouts) = self
            .servers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&server_id)
            .and_then(|entry| entry.timeouts.clone())
        {
            timeouts.set_upgrade_listener(listening);
        }
    }

    /// An https server's new secure context and accept options
    /// (`server.setSecureContext()`, a changed `requestCert`, ...), for the
    /// connections it accepts from now on.
    pub fn set_https_tls(
        &self,
        server_id: u64,
        context: Arc<crate::tls::server::ServerContext>,
        options: crate::tls::server::AcceptOptions,
    ) {
        if let Some(tls) = self
            .servers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&server_id)
            .and_then(|entry| entry.tls.clone())
        {
            tls.replace(context, options);
        }
    }

    /// `socket.setTimeout(ms)` on a server connection.
    pub fn set_conn_timeout(&self, conn_id: u64, ms: u64) {
        if let Some(watch) = self.conn(conn_id) {
            watch.set_socket_timeout(ms);
        }
    }

    /// JS has run the connection's `'secureConnection'` listeners: it may be
    /// served now (unless one of them destroyed it, which `close_reason`
    /// reports to the waiting connection task).
    pub fn resume_conn(&self, conn_id: u64) {
        if let Some(watch) = self.conn(conn_id) {
            watch.resume();
        }
    }

    /// `socket.destroy()` on a server connection, or `socket.end()`
    /// (`graceful`: what is being written is finished first).
    pub fn destroy_conn(&self, conn_id: u64, graceful: bool) {
        if let Some(watch) = self.conn(conn_id) {
            watch.close(if graceful {
                CloseReason::End
            } else {
                CloseReason::Destroy
            });
        }
    }

    /// Sync (isolate-thread) helpers consumed by the engine natives.
    /// Take the fully-collected body. Returns None for a request whose body
    /// is being streamed (no such request yet -- see slice 2), so callers
    /// keep working unchanged when that variant lands.
    pub fn take_request_body(&self, id: u64) -> Option<Vec<u8>> {
        let taken = self
            .bodies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)?;
        match taken {
            RequestBody::Full(bytes) => Some(bytes),
            // Streamed: put it back, the caller wants the chunk ops.
            other => {
                self.bodies
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id, other);
                None
            }
        }
    }

    /// Take the chunk receiver for a streamed body (remove-await-reinsert).
    pub fn take_body_stream(&self, id: u64) -> BodyCheckout {
        let mut guard = self.bodies.lock().unwrap_or_else(|e| e.into_inner());
        match guard.remove(&id) {
            Some(RequestBody::Stream(rx)) => {
                // Leave a marker so the entry still EXISTS while we await --
                // a bare removal made a checked-out stream look absent, and
                // the buffered fallback then reported EOF mid-body.
                guard.insert(id, RequestBody::StreamPending);
                BodyCheckout::Ready(rx)
            }
            Some(RequestBody::StreamPending) => {
                guard.insert(id, RequestBody::StreamPending);
                BodyCheckout::InFlight
            }
            Some(other) => {
                guard.insert(id, other);
                BodyCheckout::Absent
            }
            None => BodyCheckout::Absent,
        }
    }

    /// Reinsert a receiver taken by `take_body_stream`. Dropped silently if
    /// the request finished while the read was in flight.
    pub fn put_body_stream(&self, id: u64, rx: mpsc::Receiver<Result<BudgetedChunk, String>>) {
        let mut guard = self.bodies.lock().unwrap_or_else(|e| e.into_inner());
        // Reinsert ONLY onto the StreamPending marker left by
        // take_body_stream. A missing entry means the body was cancelled or
        // the request torn down while this read was in flight; resurrecting
        // it here would leak the stream past the request's lifetime, and
        // dropping rx instead is what stops the pump.
        if let Some(slot @ RequestBody::StreamPending) = guard.get_mut(&id) {
            *slot = RequestBody::Stream(rx);
        }
    }

    /// Drop a streamed body outright (JS cancelled / destroyed the request).
    pub fn cancel_body_stream(&self, id: u64) {
        let mut bodies = self.bodies.lock().unwrap_or_else(|e| e.into_inner());
        bodies.remove(&id);
        self.trailers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }

    /// The body was read to its end: forget it, and take its trailer fields
    /// (none when it had no trailer section).
    pub fn finish_request_body(&self, id: u64) -> Option<Vec<(String, String)>> {
        let mut bodies = self.bodies.lock().unwrap_or_else(|e| e.into_inner());
        bodies.remove(&id);
        self.trailers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
    }

    /// Keep a body's trailer fields for JS, while the body is still wanted.
    fn store_trailers(&self, id: u64, trailers: &hyper::HeaderMap) {
        let bodies = self.bodies.lock().unwrap_or_else(|e| e.into_inner());
        if bodies.contains_key(&id) {
            self.trailers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, header_pairs(trailers));
        }
    }

    pub fn respond_full(
        &self,
        id: u64,
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> bool {
        let Some(responder) = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
        else {
            return false;
        };
        responder
            .send(ResponseSpec {
                status,
                headers,
                body: ResponseBody::Full(body),
            })
            .is_ok()
    }

    /// Start a streaming response; returns the stream handle JS pushes to.
    pub fn respond_stream(
        &self,
        id: u64,
        status: u16,
        headers: Vec<(String, String)>,
    ) -> Option<u64> {
        let responder = self
            .pending
            .lock()
            .expect("http pending lock")
            .remove(&id)?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>(16);
        let (closed_tx, closed_rx) = oneshot::channel::<()>();
        let stream_id = self.next_id();
        self.streams
            .lock()
            .expect("http streams lock")
            .insert(stream_id, tx);
        self.stream_watch
            .lock()
            .expect("http stream_watch lock")
            .insert(stream_id, closed_rx);
        let ok = responder
            .send(ResponseSpec {
                status,
                headers,
                body: ResponseBody::Stream(rx, closed_tx),
            })
            .is_ok();
        if ok {
            Some(stream_id)
        } else {
            self.end_stream(stream_id);
            None
        }
    }

    /// req.destroy() before a response was sent: hand hyper an Abort spec so
    /// the connection is dropped without a response. Returns false when the
    /// exchange already responded (or never existed) -- a harmless no-op.
    pub fn abort_request(&self, id: u64) -> bool {
        let Some(responder) = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
        else {
            return false;
        };
        responder
            .send(ResponseSpec {
                status: 0,
                headers: Vec::new(),
                body: ResponseBody::Abort,
            })
            .is_ok()
    }

    pub fn stream_sender(&self, stream_id: u64) -> Option<mpsc::Sender<Vec<u8>>> {
        self.streams
            .lock()
            .expect("http streams lock")
            .get(&stream_id)
            .cloned()
    }

    /// Dropping the sender ends the hyper body (clean EOF / final chunk).
    pub fn end_stream(&self, stream_id: u64) {
        self.streams
            .lock()
            .expect("http streams lock")
            .remove(&stream_id);
        // Reap the watch entry if no httpStreamClosed op ever took it (the
        // oam.serve path and pre-watcher callers never do).
        self.stream_watch
            .lock()
            .expect("http stream_watch lock")
            .remove(&stream_id);
    }

    pub fn close_server(&self, server_id: u64) {
        if let Some(mut entry) = self
            .servers
            .lock()
            .expect("http servers lock")
            .remove(&server_id)
            && let Some(shutdown) = entry.shutdown.take()
        {
            let _ = shutdown.send(true);
        }
    }
}

/// hyper Body over the JS-pushed chunk channel. `_closed_tx` is never sent
/// on: its DROP (body finished or connection torn down) is the signal the
/// paired stream_watch receiver resolves on.
struct ChannelBody {
    rx: mpsc::Receiver<Vec<u8>>,
    _closed_tx: oneshot::Sender<()>,
}

impl hyper::body::Body for ChannelBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.rx.poll_recv(cx) {
            std::task::Poll::Ready(Some(chunk)) => {
                std::task::Poll::Ready(Some(Ok(Frame::data(Bytes::from(chunk)))))
            }
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

type BoxedBody = http_body_util::combinators::BoxBody<Bytes, std::convert::Infallible>;

fn spec_to_response(spec: ResponseSpec) -> hyper::Response<BoxedBody> {
    let mut builder = hyper::Response::builder().status(spec.status);
    for (name, value) in &spec.headers {
        builder = builder.header(name, value);
    }
    let body: BoxedBody = match spec.body {
        ResponseBody::Full(bytes) => http_body_util::Full::new(Bytes::from(bytes)).boxed(),
        ResponseBody::Stream(rx, closed_tx) => ChannelBody {
            rx,
            _closed_tx: closed_tx,
        }
        .boxed(),
        // Intercepted in handle_request (returns Err(RequestAborted) before
        // building a response); never reaches the spec-to-response path.
        ResponseBody::Abort => http_body_util::Empty::new().boxed(),
    };
    builder.body(body).unwrap_or_else(|_| {
        hyper::Response::builder()
            .status(500)
            .body(http_body_util::Full::new(Bytes::from_static(b"oam: bad response spec")).boxed())
            .expect("static 500 builds")
    })
}

// ---- Upgrade and CONNECT requests ----
// Every request head is parsed by hyper. One that node would hand to an
// 'upgrade' or 'connect' listener takes its connection out of hyper instead
// of being answered: the connection loop gets the socket and what hyper had
// read past the head, and both go to JS.

/// A request that takes its connection out of hyper (node's upgrade and
/// CONNECT): its id and its head as received.
pub struct Takeover {
    id: u64,
    head: ParsedHead,
}

/// Where a connection's service hands a [`Takeover`] to its connection loop.
#[derive(Clone)]
struct UpgradeRoute {
    tx: Arc<Mutex<Option<oneshot::Sender<Takeover>>>>,
    timeouts: Arc<ServerTimeouts>,
}

impl UpgradeRoute {
    fn new(timeouts: Arc<ServerTimeouts>) -> (Self, oneshot::Receiver<Takeover>) {
        let (tx, rx) = oneshot::channel();
        (
            UpgradeRoute {
                tx: Arc::new(Mutex::new(Some(tx))),
                timeouts,
            },
            rx,
        )
    }

    /// Hand the connection over; false if it already was, or its loop is
    /// gone.
    fn take(&self, takeover: Takeover) -> bool {
        let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner()).take();
        tx.is_some_and(|tx| tx.send(takeover).is_ok())
    }
}

/// How a server treats a request that asks to leave HTTP: an upgrade (an
/// `Upgrade` header and `upgrade` in `Connection`), or CONNECT.
#[derive(Clone)]
enum Upgrades {
    /// `oam.serve`: every request goes to the handler.
    Serve,
    /// https, and the HTTP/1 side of http2.createServer, whose sockets cannot
    /// be handed to JS: an upgrade is served as an ordinary request, and a
    /// CONNECT is closed, as node closes one no 'connect' listener takes.
    CloseConnect,
    /// A node:http server: a CONNECT, and an upgrade while the server has an
    /// 'upgrade' listener, take the connection out of hyper, as node hands
    /// the socket to 'connect' / 'upgrade' (any request on the connection
    /// can). An upgrade with no listener is an ordinary request, as in node.
    Route(UpgradeRoute),
}

/// hyper's answer to a refused head: the status, `connection: close` (so
/// hyper closes the connection instead of reading on from wherever the
/// refused request's body was supposed to end), and no body.
fn refused_head_response(error: HeadError) -> hyper::Response<BoxedBody> {
    hyper::Response::builder()
        .status(error.status())
        .header(hyper::header::CONNECTION, "close")
        .body(http_body_util::Empty::new().boxed())
        .expect("static refusal builds")
}

/// An HTTP/1 connection builder for a server with `policy`.
fn http1_builder(policy: HeadPolicy) -> hyper::server::conn::http1::Builder {
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder.max_buf_size(policy.read_buffer_limit());
    builder
}

/// Removes a connection from `HttpState::conns` when dropped.
struct ConnRegistration {
    state: Arc<HttpState>,
    id: u64,
}

impl Drop for ConnRegistration {
    fn drop(&mut self) {
        self.state
            .conns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

/// Marks a request as all in when dropped: its body was read to the end,
/// failed, or will not be read (node's message complete, after which
/// headersTimeout / requestTimeout no longer apply to it). It carries the
/// request's generation, so a body finished late cannot complete the
/// request after it.
struct MessageDone(Option<(Arc<ConnWatch>, u64)>);

impl Drop for MessageDone {
    fn drop(&mut self) {
        if let Some((watch, generation)) = &self.0 {
            watch.message_complete(*generation);
        }
    }
}

/// A response body that tells the connection's watch when the response is
/// done (written to the end, or dropped with the connection): the moment
/// node starts an idle keep-alive connection's timeout.
struct ResponseDone {
    inner: BoxedBody,
    watch: Arc<ConnWatch>,
}

impl hyper::body::Body for ResponseDone {
    type Data = Bytes;
    type Error = <BoxedBody as hyper::body::Body>::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        std::pin::Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for ResponseDone {
    fn drop(&mut self) {
        self.watch.response_finished();
    }
}

/// The response for a request on a watched connection: its head is about
/// to go out, and its body reports when it is done.
fn watched_response(
    response: hyper::Response<BoxedBody>,
    watch: Option<&Arc<ConnWatch>>,
) -> hyper::Response<BoxedBody> {
    let Some(watch) = watch else {
        return response;
    };
    watch.response_started();
    let (parts, body) = response.into_parts();
    hyper::Response::from_parts(
        parts,
        ResponseDone {
            inner: body,
            watch: Arc::clone(watch),
        }
        .boxed(),
    )
}

/// A connection's socket timeout expired: node emits 'timeout' on the
/// request, the response and the server and destroys the socket when none
/// of them listens. A node:http server decides in JS; for any other server,
/// or when JS cannot be told (its queue is full or gone), the connection is
/// closed here.
fn socket_timed_out(
    queue: &mpsc::Sender<ServerEvent>,
    watch: &ConnWatch,
    js_driven: bool,
    fired: Fired,
    conn: ConnAddrs,
) {
    let told = js_driven
        && queue
            .try_send(ServerEvent::Timeout {
                conn_id: watch.id,
                fired,
                conn,
            })
            .is_ok();
    if !told {
        watch.close(CloseReason::Destroy);
    }
}

/// Count an accepted connection against the server's `maxConnections`;
/// when node would refuse it, drop it and tell JS (node's 'drop').
fn admit(
    timeouts: &Arc<ServerTimeouts>,
    queue: &mpsc::Sender<ServerEvent>,
    conn: ConnAddrs,
) -> Option<crate::http_conn::ConnSlot> {
    let slot = timeouts.admit();
    if slot.is_none() && timeouts.js_driven() {
        let _ = queue.try_send(ServerEvent::Drop { conn });
    }
    slot
}

/// node's checkConnections for a server whose connections JS does not
/// check (`oam.serve`): every `connectionsCheckingInterval`, with the
/// server's headers / request timeouts.
async fn check_connections(
    state: Arc<HttpState>,
    server_id: u64,
    timeouts: Arc<ServerTimeouts>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let interval = timeouts.check_interval();
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let (headers_ms, request_ms) = timeouts.headers_and_request_ms();
                state.expire_connections(server_id, headers_ms, request_ms);
            }
            _ = shutdown.changed() => break,
        }
    }
}

/// Serve one HTTP/1 connection with hyper, held to node's timeouts: runs
/// until the connection ends, closing it early when `watch` is closed
/// (a request timeout answers 408 first), gracefully on server shutdown or
/// `socket.end()`.
///
/// When a request takes the connection (`takeover`), what hyper was writing
/// is written out first, and the stream comes back with the bytes hyper had
/// read past that request's head.
#[allow(clippy::too_many_arguments)]
async fn serve_http1<S, Svc>(
    stream: S,
    watch: Arc<ConnWatch>,
    policy: HeadPolicy,
    service: Svc,
    queue: mpsc::Sender<ServerEvent>,
    js_driven: bool,
    addrs: ConnAddrs,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    takeover: Option<oneshot::Receiver<Takeover>>,
) -> Option<(S, Bytes, Takeover)>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    Svc: hyper::service::Service<
            hyper::Request<hyper::body::Incoming>,
            Response = hyper::Response<BoxedBody>,
            Error = RequestAborted,
        > + Unpin
        + Send
        + 'static,
    Svc::Future: Send + 'static,
{
    let io = hyper_util::rt::TokioIo::new(WatchedIo::new(stream, Arc::clone(&watch)));
    let mut conn = http1_builder(policy).serve_connection(io, service);
    // A connection no request can take has a receiver that never fires.
    let (mut taken, mut takeable) = match takeover {
        Some(rx) => (rx, true),
        None => (oneshot::channel().1, false),
    };
    enum Ended {
        Done,
        Closed(CloseReason),
        Taken(Takeover),
    }
    // GRACEFUL shutdown on close(): disable keep-alive and let the
    // IN-FLIGHT request finish (Node's server.close() semantics), instead
    // of resetting it. An idle keep-alive connection just closes -- so the
    // queue_tx clone still drops promptly and the accept op isn't pinned.
    let mut shutting_down = false;
    let ended = loop {
        // Biased toward hyper: a response JS has already handed over (a
        // `res.end()` just before `socket.destroy()`) is written before a
        // close is acted on, as node writes it before closing.
        tokio::select! {
            biased;
            _ = &mut conn => break Ended::Done,
            takeover = &mut taken, if takeable => match takeover {
                Ok(takeover) => break Ended::Taken(takeover),
                Err(_) => takeable = false,
            },
            _ = shutdown.changed(), if !shutting_down => {
                shutting_down = true;
                std::pin::Pin::new(&mut conn).graceful_shutdown();
            }
            reason = watch.closed(if shutting_down {
                CloseReason::Destroy
            } else {
                CloseReason::End
            }) => {
                if reason == CloseReason::End {
                    // socket.end(): finish what is being written, then close.
                    shutting_down = true;
                    std::pin::Pin::new(&mut conn).graceful_shutdown();
                } else {
                    break Ended::Closed(reason);
                }
            }
            fired = watch.next_timeout() => {
                socket_timed_out(&queue, &watch, js_driven, fired, addrs);
            }
            // hyper does not come back by itself to a request it read while
            // its write buffer was still draining (it polls that request's
            // handler only once the buffer is empty, and nothing wakes it
            // then): a flush that empties the buffer polls it again.
            _ = watch.next_flush() => {}
        }
    };
    match ended {
        Ended::Done => None,
        Ended::Closed(reason) => {
            // hyper's side ends here: an in-flight handler future is dropped
            // (its RequestGuard tells JS the exchange ended), and the stream
            // comes back for node's farewell. Read before hyper's side goes:
            // dropping it ends the response.
            let may_answer = watch.may_answer();
            let stream = conn.into_parts().io.into_inner().into_inner();
            crate::http_conn::finish_close(stream, reason, may_answer).await;
            None
        }
        Ended::Taken(takeover) => {
            // hyper may still hold part of an earlier response on this
            // connection, for a client that has not read it yet: its buffer
            // goes with hyper's side, so it is written out first (node's
            // socket keeps writing it after the handover).
            let budget = tokio::time::sleep(TAKEOVER_FLUSH_BUDGET);
            tokio::pin!(budget);
            while watch.unflushed() {
                tokio::select! {
                    biased;
                    _ = &mut conn => return None,
                    _ = watch.flushed() => {}
                    _ = &mut budget => return None,
                    _ = shutdown.changed() => return None,
                }
            }
            let parts = conn.into_parts();
            Some((parts.io.into_inner().into_inner(), parts.read_buf, takeover))
        }
    }
}

/// Bind + spawn the accept loop. Resolves Json {serverId, port}.
#[allow(clippy::too_many_arguments)]
pub async fn http_serve(
    state: Arc<HttpState>,
    tcp: super::tcp::TcpRegistry,
    tcp_ids: Arc<std::sync::atomic::AtomicU64>,
    host: String,
    port: u16,
    // Dispatch the JS handler on headers and stream the body (slice 2 of
    // docs/design/streaming-bodies.md). Off = today's buffered behavior.
    stream_request_body: bool,
    // maxHeaderSize / insecureHTTPParser for this server.
    policy: HeadPolicy,
    // node's server timeouts (headersTimeout, keepAliveTimeout, ...).
    timeouts: TimeoutSettings,
) -> super::OpOutcome {
    let listener = match tokio::net::TcpListener::bind((host.as_str(), port)).await {
        Ok(listener) => listener,
        Err(e) => return super::OpOutcome::Failed(format!("listen {host}:{port}: {e}")),
    };
    let local_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    let server_id = state.next_id();
    let (queue_tx, queue_rx) = mpsc::channel::<ServerEvent>(64);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let server_timeouts = ServerTimeouts::new(timeouts);
    state
        .servers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            server_id,
            ServerEntry {
                queue: Some(queue_rx),
                shutdown: Some(shutdown_tx),
                timeouts: Some(Arc::clone(&server_timeouts)),
                tls: None,
            },
        );
    if !server_timeouts.js_driven() {
        tokio::spawn(check_connections(
            state.clone(),
            server_id,
            Arc::clone(&server_timeouts),
            shutdown_rx.clone(),
        ));
    }

    let accept_state = state.clone();
    let accept_tcp = tcp;
    let accept_tcp_ids = tcp_ids;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                accepted = listener.accept() => {
                    let Ok((stream, peer)) = accepted else {
                        tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                        continue;
                    };
                    let conn_addrs = ConnAddrs {
                        remote: peer,
                        local: stream.local_addr().ok(),
                    };
                    let Some(slot) = admit(&server_timeouts, &queue_tx, conn_addrs) else {
                        drop(stream);
                        continue;
                    };
                    // Everything after the accept runs on the connection's
                    // own task: nothing a client sends, or does not send,
                    // holds up this loop.
                    let conn_state = accept_state.clone();
                    let conn_queue = queue_tx.clone();
                    let conn_tcp = accept_tcp.clone();
                    let conn_tcp_ids = accept_tcp_ids.clone();
                    let conn_stream_bodies = stream_request_body;
                    let conn_timeouts = Arc::clone(&server_timeouts);
                    let conn_shutdown = shutdown_rx.clone();
                    tokio::spawn(async move {
                        let slot = slot;
                        // node's timeouts hold from the accept: a connection
                        // that never sends a byte is answered 408 once
                        // headersTimeout passes, like any other.
                        let watch =
                            ConnWatch::new(conn_state.next_id(), server_id, conn_timeouts.clone());
                        let registration = conn_state.register_conn(Arc::clone(&watch));
                        let js_driven = conn_timeouts.js_driven();
                        // A node:http server routes upgrades and CONNECT;
                        // oam.serve hands every request to its handler.
                        let (upgrades, taken) = if js_driven {
                            let (route, taken) = UpgradeRoute::new(Arc::clone(&conn_timeouts));
                            (Upgrades::Route(route), Some(taken))
                        } else {
                            (Upgrades::Serve, None)
                        };
                        let service_state = conn_state.clone();
                        let service_queue = conn_queue.clone();
                        let service_watch = Arc::clone(&watch);
                        let service = hyper::service::service_fn(move |req| {
                            handle_request(
                                service_state.clone(),
                                service_queue.clone(),
                                req,
                                conn_stream_bodies, // per-server opt-in
                                conn_addrs,
                                None,
                                policy,
                                Some(Arc::clone(&service_watch)),
                                upgrades.clone(),
                            )
                        });
                        let taken = serve_http1(
                            stream,
                            watch,
                            policy,
                            service,
                            conn_queue.clone(),
                            js_driven,
                            conn_addrs,
                            conn_shutdown,
                            taken,
                        )
                        .await;
                        // The socket leaves node's http timeouts (node drops
                        // its 'timeout' handling for an upgraded socket) and
                        // maxConnections (node counts it until it closes).
                        drop(registration);
                        drop(slot);
                        let Some((stream, head, takeover)) = taken else {
                            return;
                        };
                        let handle = conn_tcp_ids.fetch_add(1, Ordering::Relaxed);
                        let (reader, writer) = stream.into_split();
                        conn_tcp
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .register_stream(handle, reader, writer);
                        let _ = conn_queue
                            .send(ServerEvent::Request(IncomingRequest {
                                id: takeover.id,
                                method: takeover.head.method,
                                uri: takeover.head.target,
                                headers: takeover.head.headers,
                                is_upgrade: true,
                                socket_handle: Some(handle),
                                conn: conn_addrs,
                                conn_id: None,
                                head: head.to_vec(),
                                end_stream: false,
                                tls: None,
                            }))
                            .await;
                    });
                }
            }
        }
        // Last queue_tx drops with the loop + finished connections: the
        // pending accept op resolves Done and the JS loop exits.
    });

    super::OpOutcome::Json(
        serde_json::json!({ "serverId": server_id, "port": local_port }).to_string(),
    )
}

/// RAII cleanup tied to the REQUEST lifetime, not the body read. When
/// handle_request returns — handler responded, client disconnected (future
/// cancelled at any await), or queue-send failed — both the buffered body
/// and the pending responder are removed. Without this, a handler that
/// never reads the body (a GET route, a 401, a webhook branching on
/// headers) leaked the body forever: a remote, attacker-controlled DoS.
struct RequestGuard {
    state: Arc<HttpState>,
    id: u64,
    /// Body bytes this request reserved against the global budget; refunded
    /// on drop along with the map cleanup.
    reserved: usize,
    /// Set once the request has been handed to the JS accept queue. Until
    /// then no JS reap path can exist, so Drop must remove the body entry
    /// itself (queue send failed, or the future was cancelled mid-send).
    dispatched: bool,
    /// Where to report an exchange that ends before JS answered it (the
    /// connection was closed under it), for a server whose JS keeps the
    /// request / response pair.
    closed_to: Option<mpsc::Sender<ServerEvent>>,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        let unanswered = self
            .state
            .pending
            .lock()
            .expect("http pending lock")
            .remove(&self.id)
            .is_some();
        // After the request event in the queue; a send that waits for room
        // rather than one that can be dropped.
        if unanswered
            && self.dispatched
            && let Some(queue) = self.closed_to.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let request_id = self.id;
            runtime.spawn(async move {
                let _ = queue.send(ServerEvent::Closed { request_id }).await;
            });
        }
        let mut bodies = self.state.bodies.lock().expect("http bodies lock");
        match bodies.get(&self.id) {
            // Streamed bodies on a DISPATCHED request outlive this guard:
            // handle_request returns the moment JS sends response headers --
            // or is cancelled by a client disconnect -- while the handler
            // may still be mid-read, and an aborted upload must surface the
            // pump's queued error to the consumer rather than vanish into a
            // clean EOF. The JS side reaps instead: a terminal read
            // (EOF/error) in the read op, _dump for a never-consumed body,
            // and _destroy's cancel. A request JS never learned about has
            // no JS reaper, so it IS removed here.
            Some(RequestBody::Stream(_) | RequestBody::StreamPending) if self.dispatched => {}
            Some(_) => {
                bodies.remove(&self.id);
                self.state
                    .trailers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&self.id);
            }
            None => {
                self.state
                    .trailers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&self.id);
            }
        }
        drop(bodies);
        if self.reserved > 0 {
            self.state
                .body_bytes
                .fetch_sub(self.reserved, Ordering::AcqRel);
        }
    }
}

fn status_body(status: u16, text: &'static [u8]) -> hyper::Response<BoxedBody> {
    hyper::Response::builder()
        .status(status)
        .body(http_body_util::Full::new(Bytes::from_static(text)).boxed())
        .expect("static status body builds")
}

/// The status node answers a request body its parser refuses with: `413`
/// for chunk extensions over the limit, `431` for oversized trailers, `400`
/// for any other malformed body (a bad chunk size -- whitespace after it
/// included --, a missing CRLF, a bare LF in the trailers). `None` when the
/// body did not fail on its bytes -- the client went away mid-body -- which
/// node answers with nothing.
///
/// hyper reports a body it could not decode as an `io::Error` of kind
/// `InvalidInput` / `InvalidData` behind the `hyper::Error`, and a body cut
/// short as `UnexpectedEof`; the texts are hyper's (decode.rs).
fn refused_body_status(error: &hyper::Error) -> Option<u16> {
    let io = std::error::Error::source(error)?.downcast_ref::<std::io::Error>()?;
    match io.kind() {
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData => {
            let text = io.to_string();
            Some(match text.as_str() {
                "chunk extensions over limit" => 413,
                "chunk trailers bytes over limit" | "chunk trailers count overflow" => 431,
                _ => 400,
            })
        }
        _ => None,
    }
}

/// Answer request `id` with `status` and `Connection: close` in place of
/// the handler's response, as node does when its parser refuses a body
/// before the response head went out. A handler that already responded
/// took the responder out of `pending`, and nothing is sent.
fn refuse_unanswered(state: &HttpState, id: u64, status: u16) {
    let responder = state
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
    if let Some(responder) = responder {
        let _ = responder.send(ResponseSpec {
            status,
            headers: vec![("connection".to_string(), "close".to_string())],
            body: ResponseBody::Full(Vec::new()),
        });
    }
}

/// Feed request chunks to the JS side as they arrive, enforcing
/// MAX_REQUEST_BODY cumulatively. Exceeding it sends Err (the JS request
/// stream errors) rather than a 413, because the handler was dispatched on
/// headers and may already have responded. Ends by dropping the sender, which
/// the reader sees as EOF.
///
/// A body the parser refuses is answered with node's status first (when the
/// handler has not responded yet), then the error reaches the handler.
async fn pump_request_body(
    mut body: hyper::body::Incoming,
    chunk_tx: mpsc::Sender<Result<BudgetedChunk, String>>,
    state: std::sync::Arc<HttpState>,
    id: u64,
    // Dropped when the pump ends: the request is all in, as far as node's
    // headers / request timeouts go.
    _message_done: MessageDone,
) {
    use http_body_util::BodyExt;
    let mut total: usize = 0;
    while let Some(frame) = body.frame().await {
        let frame = match frame {
            Ok(f) => f,
            Err(e) => {
                if let Some(status) = refused_body_status(&e) {
                    refuse_unanswered(&state, id, status);
                }
                let _ = chunk_tx.send(Err(format!("request body: {e}"))).await;
                return;
            }
        };
        let data = match frame.into_data() {
            Ok(data) => data,
            // The trailer section, the body's last frame: kept for JS to
            // take at the end (node's req.trailers).
            Err(frame) => {
                if let Ok(trailers) = frame.into_trailers() {
                    state.store_trailers(id, &trailers);
                }
                continue;
            }
        };
        total += data.len();
        if total > MAX_REQUEST_BODY {
            let _ = chunk_tx
                .send(Err("request body too large".to_string()))
                .await;
            return;
        }
        // Charge the GLOBAL budget for the bytes about to be queued. This is
        // the per-chunk accounting the buffered path's comment promised and
        // that did not exist: streaming is always on, so the buffered
        // reservation was never reached and GLOBAL_BODY_BUDGET was charged
        // NOWHERE. Each request was capped at MAX_REQUEST_BODY, but N
        // concurrent uploads were bounded only by N.
        let len = data.len();
        let prev = state.body_bytes.fetch_add(len, Ordering::AcqRel);
        if prev + len > global_body_budget() {
            state.body_bytes.fetch_sub(len, Ordering::AcqRel);
            // The handler was dispatched on headers, so 503 is no longer
            // available; the consumer sees the error instead.
            let _ = chunk_tx.send(Err("server is busy".to_string())).await;
            return;
        }
        // send() awaits when the channel is full: that IS the backpressure.
        // Err means the consumer is gone, so stop reading the socket -- the
        // chunk drops here and refunds itself.
        if chunk_tx
            .send(Ok(BudgetedChunk::new(
                data.to_vec(),
                std::sync::Arc::clone(&state),
            )))
            .await
            .is_err()
        {
            return;
        }
    }
}

/// Why [`collect_body`] produced no body.
enum CollectError {
    /// Over `MAX_REQUEST_BODY` (the drain has run).
    TooLarge,
    /// The parser refused the body: answer with this status.
    Refused(u16),
    /// The body was cut short (the client went away): no answer.
    Gone,
}

/// Collect up to `MAX_REQUEST_BODY` bytes from `body`, then drain up to
/// `DRAIN_BUDGET` more (discarding them) before returning.
///
/// The drain step is what makes 413 reliable on Windows.  Without it,
/// closing a TcpStream with unread kernel recv-buffer data sends a TCP RST
/// instead of FIN.  The RST races with the 413 bytes in the send-buffer;
/// the client may read "connection reset" instead of "413".  Draining the
/// recv-buffer lets the OS close the connection gracefully (FIN) so the
/// 413 response lands first.
///
/// A body that fails part way is never returned as if it were whole: a
/// malformed one is `Refused` (node's status for it), a truncated one
/// `Gone`.
async fn collect_body(
    mut body: hyper::body::Incoming,
) -> Result<(bytes::Bytes, Option<hyper::HeaderMap>), CollectError> {
    use bytes::BufMut;
    let mut buf = bytes::BytesMut::new();
    let mut over_cap = false;
    let mut drained: usize = 0;
    let mut trailers = None;

    loop {
        let frame = match body.frame().await {
            Some(Ok(f)) => f,
            None => break,
            Some(Err(_)) if over_cap => break,
            Some(Err(e)) => {
                return Err(match refused_body_status(&e) {
                    Some(status) => CollectError::Refused(status),
                    None => CollectError::Gone,
                });
            }
        };
        let chunk = match frame.into_data() {
            Ok(chunk) => chunk,
            // The trailer section (node's req.trailers); other frame types
            // are ignored.
            Err(frame) => {
                trailers = frame.into_trailers().ok();
                continue;
            }
        };
        if over_cap {
            // Drain phase: discard bytes up to DRAIN_BUDGET to let the
            // kernel recv-buffer clear so close() can use FIN, not RST.
            drained += chunk.len();
            if drained >= DRAIN_BUDGET {
                break;
            }
        } else {
            buf.put(chunk);
            if buf.len() > MAX_REQUEST_BODY {
                over_cap = true;
                buf.clear(); // release buffered memory immediately
            }
        }
    }

    if over_cap {
        Err(CollectError::TooLarge)
    } else {
        Ok((buf.freeze(), trailers))
    }
}

/// A header map's fields as JS reads them: lowercased names, each value on
/// its own.
fn header_pairs(map: &hyper::HeaderMap) -> Vec<(String, String)> {
    map.iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

/// Service-level error returned only for ResponseBody::Abort: hyper drops
/// the connection without writing a response, so the client sees a reset --
/// Node's req.destroy() semantics. Every other path still answers with a
/// synthesized status.
#[derive(Debug)]
struct RequestAborted;

impl std::fmt::Display for RequestAborted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request aborted by handler")
    }
}

impl std::error::Error for RequestAborted {}

/// One request, from hyper to JS and back. On a connection held to node's
/// timeouts (`watch`), its headers are in now, its body is all in when the
/// body is read to the end (or no longer read), and the response it gets
/// reports its start and its end.
#[allow(clippy::too_many_arguments)]
async fn handle_request(
    state: Arc<HttpState>,
    queue: mpsc::Sender<ServerEvent>,
    req: hyper::Request<hyper::body::Incoming>,
    stream_request_body: bool,
    conn: ConnAddrs,
    // An https connection's handshake (IncomingRequest::tls).
    tls: Option<Arc<serde_json::Value>>,
    policy: HeadPolicy,
    watch: Option<Arc<ConnWatch>>,
    upgrades: Upgrades,
) -> Result<hyper::Response<BoxedBody>, RequestAborted> {
    let id = state.next_id();
    let message_done = MessageDone(
        watch
            .as_ref()
            .map(|w| (Arc::clone(w), w.headers_complete(id))),
    );
    let conn_id = watch.as_ref().map(|w| w.id);
    let notify_closed = watch.as_ref().is_some_and(|w| w.js_driven());
    let response = dispatch_request(
        state,
        queue,
        req,
        stream_request_body,
        conn,
        tls,
        policy,
        id,
        message_done,
        conn_id,
        notify_closed,
        upgrades,
    )
    .await?;
    Ok(watched_response(response, watch.as_ref()))
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_request(
    state: Arc<HttpState>,
    queue: mpsc::Sender<ServerEvent>,
    req: hyper::Request<hyper::body::Incoming>,
    stream_request_body: bool,
    conn: ConnAddrs,
    tls: Option<Arc<serde_json::Value>>,
    policy: HeadPolicy,
    id: u64,
    message_done: MessageDone,
    conn_id: Option<u64>,
    // Tell JS when the exchange ends without its response (a node:http
    // server keeps the pair until then).
    notify_closed: bool,
    upgrades: Upgrades,
) -> Result<hyper::Response<BoxedBody>, RequestAborted> {
    // node's rules for the head, on the bytes hyper parsed (HTTP/1 only;
    // an HTTP/2 request has no such head). A refused request never reaches
    // JS and its body is never read: the connection closes after the
    // refusal, so nothing after the head is ever parsed as a request.
    if let Some(raw) = req.extensions().get::<hyper::ext::RawRequestHead>() {
        if let Err(error) = crate::http_head::check_request_head(raw.as_bytes(), policy) {
            return Ok(refused_head_response(error));
        }
        let connect = req.method() == hyper::Method::CONNECT;
        match &upgrades {
            Upgrades::Serve => {}
            Upgrades::CloseConnect => {
                if connect {
                    // No 'connect' listener can take it here: closed, as
                    // node closes a CONNECT nobody tunnels.
                    return Err(RequestAborted);
                }
            }
            Upgrades::Route(route) => {
                // node: every CONNECT, and an upgrade while there is an
                // 'upgrade' listener. An upgrade that declares a body is
                // left to hyper, which would read that body as the request's
                // (node hands those bytes to the listener as `head`); hyper
                // reads none for a CONNECT.
                let upgrade = !connect
                    && route.timeouts.upgrade_listener()
                    && hyper::body::Body::is_end_stream(req.body())
                    && crate::http_head::is_upgrade(
                        req.headers()
                            .iter()
                            .map(|(name, value)| (name.as_str(), value.as_bytes())),
                    );
                if (connect || upgrade)
                    && let Ok(head) = crate::http_head::parse_request_head(raw.as_bytes(), policy)
                    && route.take(Takeover { id, head })
                {
                    // The connection loop takes the socket; this exchange
                    // is never answered, and its future is dropped with
                    // hyper's side of the connection.
                    drop(message_done);
                    return std::future::pending().await;
                }
            }
        }
    }
    // node's req.url is the request target exactly as the client wrote it
    // (http/1.x only: an h2 request has no request line). hyper's URI type
    // rewrites some -- an absolute form's scheme is lowercased and a missing
    // path gains `/`, a fragment is dropped -- so it is read off the head.
    // Only a head hyper accepted gets here, so the target is ASCII.
    let raw_target = req
        .extensions()
        .get::<hyper::ext::RawRequestHead>()
        .and_then(|raw| crate::http_head::request_target(raw.as_bytes()))
        .map(|target| target.iter().map(|&b| char::from(b)).collect::<String>());
    let (parts, body) = req.into_parts();
    let end_stream =
        parts.version == hyper::Version::HTTP_2 && hyper::body::Body::is_end_stream(&body);
    // Collect the body, enforcing MAX_REQUEST_BODY.  When the cap is hit we
    // drain up to DRAIN_BUDGET additional bytes before returning 413.
    //
    // Why drain?  On Windows, dropping a TcpStream that still has unread
    // data in the kernel recv-buffer triggers an immediate TCP RST.  The RST
    // races with the 413 response bytes sitting in the send-buffer, so the
    // client reads "connection reset" instead of "413 Request Entity Too
    // Large".  Draining empties the recv-buffer so the connection can close
    // with a clean FIN and the client reliably reads the status line first.
    // Streamed: dispatch on headers and pump chunks behind the request.
    // 413 is unavailable from here on (the handler may already be
    // responding), so the cap becomes an Err delivered on the chunk channel.
    // A body that DECLARES itself over the cap is rejected before the
    // handler is dispatched, so 413 (and the Windows drain behind it) still
    // works exactly as it did. Only an undeclared body -- chunked, or a
    // lying Content-Length -- can exceed mid-stream, and that is the case
    // the pump turns into a stream error because the handler may already
    // have responded.
    let declared_oversize = parts
        .headers
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|len| len > MAX_REQUEST_BODY);
    let ((collected, trailers), body_stream) = if stream_request_body && !declared_oversize {
        ((bytes::Bytes::new(), None), Some((body, message_done)))
    } else {
        let collected = match collect_body(body).await {
            Ok(collected) => collected,
            Err(CollectError::TooLarge) => return Ok(status_body(413, b"request body too large")),
            Err(CollectError::Refused(status)) => {
                return Ok(hyper::Response::builder()
                    .status(status)
                    .header(hyper::header::CONNECTION, "close")
                    .body(http_body_util::Empty::new().boxed())
                    .expect("static refusal builds"));
            }
            Err(CollectError::Gone) => return Err(RequestAborted),
        };
        drop(message_done);
        (collected, None)
    };
    // Reserve the retained bytes; refund (RequestGuard) on completion. A
    // streamed body reserves nothing here -- it is charged per outstanding
    // chunk instead, so a prompt consumer is not billed for the whole upload.
    let body_len = collected.len();
    let prev = state.body_bytes.fetch_add(body_len, Ordering::AcqRel);
    if prev + body_len > global_body_budget() {
        state.body_bytes.fetch_sub(body_len, Ordering::AcqRel);
        return Ok(status_body(503, b"server is busy"));
    }
    let mut headers = header_pairs(&parts.headers);
    // An HTTP/2 request's :authority and :scheme are pseudo-headers hyper
    // folds into the URI; node's server hands them to the handler with the
    // rest of its headers.
    if parts.version == hyper::Version::HTTP_2 {
        let mut pseudo = Vec::new();
        if let Some(authority) = parts.uri.authority() {
            pseudo.push((":authority".to_string(), authority.as_str().to_string()));
        }
        if let Some(scheme) = parts.uri.scheme_str() {
            pseudo.push((":scheme".to_string(), scheme.to_string()));
        }
        headers.splice(0..0, pseudo);
    }
    // Over http/1.x that includes the ABSOLUTE form a client of a forward
    // proxy sends (`GET http://host/p HTTP/1.1`), which a proxy written on
    // this server reads to know where to send the request. Over h2 the URI
    // is assembled from the pseudo-headers and node's compat req.url is
    // `:path` alone.
    let uri = raw_target.unwrap_or_else(|| {
        parts
            .uri
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_else(|| parts.uri.path().to_string())
    });

    let (tx, rx) = oneshot::channel::<ResponseSpec>();
    state
        .pending
        .lock()
        .expect("http pending lock")
        .insert(id, tx);
    if let Some((body, message_done)) = body_stream {
        // Bounded: an unconsumed body applies backpressure to hyper rather
        // than growing without limit. This is the memory ceiling that
        // replaces the buffered path's byte reservation.
        let (chunk_tx, chunk_rx) = mpsc::channel::<Result<BudgetedChunk, String>>(8);
        state
            .bodies
            .lock()
            .expect("http bodies lock")
            .insert(id, RequestBody::Stream(chunk_rx));
        tokio::spawn(pump_request_body(
            body,
            chunk_tx,
            std::sync::Arc::clone(&state),
            id,
            message_done,
        ));
    } else {
        state
            .bodies
            .lock()
            .expect("http bodies lock")
            .insert(id, RequestBody::Full(collected.to_vec()));
        if let Some(trailers) = &trailers {
            state.store_trailers(id, trailers);
        }
    }
    // From here on, every exit cleans up — including a cancelled future
    // if the client disconnects while the handler runs — and refunds the
    // reserved body bytes.
    let mut guard = RequestGuard {
        state: state.clone(),
        id,
        reserved: body_len,
        dispatched: false,
        closed_to: notify_closed.then(|| queue.clone()),
    };

    let sent = queue
        .send(ServerEvent::Request(IncomingRequest {
            id,
            method: parts.method.as_str().to_string(),
            uri,
            headers,
            is_upgrade: false,
            socket_handle: None,
            conn,
            conn_id,
            head: Vec::new(),
            end_stream,
            tls,
        }))
        .await;
    if sent.is_err() {
        return Ok(hyper::Response::builder()
            .status(503)
            .body(http_body_util::Full::new(Bytes::from_static(b"server is closing")).boxed())
            .expect("static 503 builds"));
    }
    guard.dispatched = true;

    match rx.await {
        Ok(ResponseSpec {
            body: ResponseBody::Abort,
            ..
        }) => Err(RequestAborted),
        Ok(spec) => Ok(spec_to_response(spec)),
        Err(_) => Ok(hyper::Response::builder()
            .status(500)
            .body(
                http_body_util::Full::new(Bytes::from_static(b"handler dropped the request"))
                    .boxed(),
            )
            .expect("static 500 builds")),
    }
}

/// Bind an https server: http_serve's request/response lifecycle (shared
/// HttpState, same accept/respond ops), each accepted connection first taken
/// through node:tls's server handshake (`tls::server::accept_stream`) with
/// the server's secure context and options. So `requestCert`,
/// `rejectUnauthorized` and `ca`, the ALPN list and `handshakeTimeout` hold
/// exactly as they do for tls.createServer: a client the server refuses
/// never reaches the HTTP layer. A handshake that fails is reported to JS
/// (node's 'tlsClientError'); a connection that completes it is served as
/// HTTP/1.1, its requests carrying what the handshake settled.
#[allow(clippy::too_many_arguments)]
pub async fn https_serve(
    state: Arc<HttpState>,
    host: String,
    port: u16,
    // The secure context and accept options, replaceable from JS.
    tls: Arc<HttpsTls>,
    // maxHeaderSize / insecureHTTPParser for this server.
    policy: HeadPolicy,
    // node's server timeouts.
    timeouts: TimeoutSettings,
) -> super::OpOutcome {
    let listener = match tokio::net::TcpListener::bind((host.as_str(), port)).await {
        Ok(listener) => listener,
        Err(e) => return super::OpOutcome::Failed(format!("listen {host}:{port}: {e}")),
    };
    let local_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    let server_id = state.next_id();
    let (queue_tx, queue_rx) = mpsc::channel::<ServerEvent>(64);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let server_timeouts = ServerTimeouts::new(timeouts);
    state
        .servers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            server_id,
            ServerEntry {
                queue: Some(queue_rx),
                shutdown: Some(shutdown_tx),
                timeouts: Some(Arc::clone(&server_timeouts)),
                tls: Some(Arc::clone(&tls)),
            },
        );
    if !server_timeouts.js_driven() {
        tokio::spawn(check_connections(
            state.clone(),
            server_id,
            Arc::clone(&server_timeouts),
            shutdown_rx.clone(),
        ));
    }

    let accept_state = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                accepted = listener.accept() => {
                    let Ok((stream, peer)) = accepted else {
                        tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                        continue;
                    };
                    // Taken from the TCP socket before the TLS handshake
                    // wraps it: the peer is the TCP peer, as in node.
                    let conn_addrs = ConnAddrs {
                        remote: peer,
                        local: stream.local_addr().ok(),
                    };
                    let Some(slot) = admit(&server_timeouts, &queue_tx, conn_addrs) else {
                        drop(stream);
                        continue;
                    };
                    // What this connection is accepted with: the server's
                    // context and options as they are now.
                    let (context, options) = tls.current();
                    let conn_state = accept_state.clone();
                    let conn_queue = queue_tx.clone();
                    let conn_timeouts = Arc::clone(&server_timeouts);
                    let mut conn_shutdown = shutdown_rx.clone();
                    // Everything after the accept runs on the connection's
                    // own task: a client that never finishes its handshake
                    // holds up no one else.
                    tokio::spawn(async move {
                        let slot = slot;
                        let handshake = tokio::select! {
                            accepted = crate::tls::server::accept_stream(
                                stream, &context, &options,
                            ) => accepted,
                            _ = conn_shutdown.changed() => return,
                        };
                        let refusal = match handshake {
                            Ok((tls_stream, info)) => {
                                // node's onServerSocketSecure: a certificate
                                // that did not verify, under
                                // rejectUnauthorized, and the socket is
                                // destroyed before 'secureConnection' (a
                                // resumed session's; a chain sent in the
                                // handshake was refused there).
                                if options.request_cert
                                    && options.reject_unauthorized
                                    && !info.authorized
                                {
                                    drop(tls_stream);
                                    Some((
                                        Some("ECONNRESET".to_string()),
                                        "socket hang up".to_string(),
                                    ))
                                } else {
                                    serve_https_connection(
                                        tls_stream,
                                        info,
                                        conn_state,
                                        conn_queue.clone(),
                                        conn_timeouts,
                                        server_id,
                                        conn_addrs,
                                        policy,
                                        conn_shutdown,
                                    )
                                    .await;
                                    None
                                }
                            }
                            Err(failed) => Some(failure_parts(failed)),
                        };
                        // The connection is gone: it no longer counts
                        // against maxConnections, and JS hears why (node's
                        // 'tlsClientError').
                        drop(slot);
                        if let Some((code, message)) = refusal {
                            let _ = conn_queue
                                .send(ServerEvent::TlsClientError {
                                    conn: conn_addrs,
                                    code,
                                    message,
                                })
                                .await;
                        }
                    });
                }
            }
        }
    });

    super::OpOutcome::Json(
        serde_json::json!({ "serverId": server_id, "port": local_port }).to_string(),
    )
}

/// One https connection past its handshake, served as HTTP/1.1 until it
/// ends. node's http timeouts start here (its http side sees
/// 'secureConnection').
#[allow(clippy::too_many_arguments)]
async fn serve_https_connection(
    tls_stream: tokio_rustls::server::TlsStream<crate::tls::server::ServerIo>,
    info: crate::tls::server::HandshakeInfo,
    state: Arc<HttpState>,
    queue: mpsc::Sender<ServerEvent>,
    timeouts: Arc<ServerTimeouts>,
    server_id: u64,
    conn_addrs: ConnAddrs,
    policy: HeadPolicy,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let tls_meta = Some(Arc::new(info.to_json()));
    let watch = ConnWatch::new(state.next_id(), server_id, Arc::clone(&timeouts));
    let _registration = state.register_conn(Arc::clone(&watch));
    // node's tlsConnectionListener: the server's 'secureConnection'
    // listeners see the connection before its HTTP parser does, and the
    // application decides there whether to serve it at all -- the documented
    // mutual-TLS pattern reads `authorized` / `authorizationError` /
    // `getPeerCertificate()` and destroys the clients it refuses. Nothing on
    // this connection is parsed as HTTP until JS has run them and answered.
    let announced = queue
        .send(ServerEvent::SecureConnection {
            conn_id: watch.id,
            conn: conn_addrs,
            tls: Arc::clone(tls_meta.as_ref().expect("https connection has a handshake")),
        })
        .await
        .is_ok();
    if announced {
        tokio::select! {
            _ = watch.resume_wait() => {}
            // The server closed under the handshake: nobody is left to run
            // the listeners, so the connection is not served.
            _ = shutdown.changed() => return,
        }
    }
    let done = queue.clone();
    let conn_id = watch.id;
    // A listener refused this client: the connection closes without a
    // request ever reaching the handler, and without an answer on the wire.
    if watch.close_reason().is_some() {
        drop(tls_stream);
        if announced {
            let _ = done.send(ServerEvent::ConnectionClosed { conn_id }).await;
        }
        return;
    }
    let js_driven = timeouts.js_driven();
    let service_queue = queue.clone();
    let service_watch = Arc::clone(&watch);
    let service = hyper::service::service_fn(move |req| {
        handle_request(
            Arc::clone(&state),
            service_queue.clone(),
            req,
            false, // TLS: buffered until a later slice
            conn_addrs,
            tls_meta.clone(),
            policy,
            Some(Arc::clone(&service_watch)),
            Upgrades::CloseConnect,
        )
    });
    serve_http1(
        tls_stream, watch, policy, service, queue, js_driven, conn_addrs, shutdown, None,
    )
    .await;
    if announced {
        let _ = done.send(ServerEvent::ConnectionClosed { conn_id }).await;
    }
}

/// A failed handshake's error as 'tlsClientError' carries it: node's code,
/// when there is one, and the message.
fn failure_parts(failed: super::OpOutcome) -> (Option<String>, String) {
    match failed {
        super::OpOutcome::NodeFailed { code, message, .. } => (Some(code), message),
        super::OpOutcome::Failed(message) => (None, message),
        _ => (None, "TLS handshake failed".to_string()),
    }
}

/// Long-poll the next request. Json metadata, or Done when the server
/// closed (queue drained + senders dropped).
pub async fn http_accept(state: Arc<HttpState>, server_id: u64) -> super::OpOutcome {
    let (queue, mut shutdown_rx) = {
        let mut guard = state.servers.lock().expect("http servers lock");
        match guard.get_mut(&server_id) {
            Some(entry) => (
                entry.queue.take(),
                entry.shutdown.as_ref().map(|tx| tx.subscribe()),
            ),
            None => (None, None),
        }
    };
    let Some(mut queue) = queue else {
        // Server was already closed (close_server removed the entry) or
        // another accept is in flight.  Either way, signal Done so the JS
        // accept loop exits cleanly instead of surfacing an unhandled
        // rejection.
        return super::OpOutcome::Done;
    };
    // Wait for the next request OR a server.close() shutdown. Idle keep-alive
    // connection tasks hold queue_tx clones that can outlive graceful_shutdown,
    // so queue.recv() alone may park forever after close(); the shutdown watch
    // lets accept return Done promptly, matching Node's closeIdleConnections.
    let next = match shutdown_rx {
        Some(ref mut sd) => {
            tokio::select! {
                n = queue.recv() => n,
                _ = sd.changed() => None,
            }
        }
        None => queue.recv().await,
    };
    if let Some(entry) = state
        .servers
        .lock()
        .expect("http servers lock")
        .get_mut(&server_id)
    {
        entry.queue = Some(queue);
    }
    match next {
        Some(ServerEvent::Request(request)) => {
            let mut meta = serde_json::json!({
                "requestId": request.id,
                "method": request.method,
                "uri": request.uri,
                "headers": request.headers,
            });
            request.conn.write_meta(&mut meta);
            if request.end_stream {
                meta["endStream"] = serde_json::json!(true);
            }
            if let Some(tls) = &request.tls {
                meta["tls"] = serde_json::Value::clone(tls);
            }
            if let Some(conn_id) = request.conn_id {
                meta["connectionId"] = serde_json::json!(conn_id);
            }
            if request.is_upgrade {
                meta["isUpgrade"] = serde_json::json!(true);
                meta["socketHandle"] = serde_json::json!(request.socket_handle);
                // What came after the head, one char per byte (latin1).
                let head: String = request.head.iter().map(|&b| char::from(b)).collect();
                meta["head"] = serde_json::json!(head);
            }
            super::OpOutcome::Json(meta.to_string())
        }
        Some(ServerEvent::Timeout {
            conn_id,
            fired,
            conn,
        }) => {
            let mut meta = serde_json::json!({
                "event": "timeout",
                "connectionId": conn_id,
            });
            if let Some(request_id) = fired.request_id {
                meta["requestId"] = serde_json::json!(request_id);
                meta["requestComplete"] = serde_json::json!(fired.request_complete);
            }
            conn.write_meta(&mut meta);
            super::OpOutcome::Json(meta.to_string())
        }
        Some(ServerEvent::SecureConnection { conn_id, conn, tls }) => {
            let mut meta = serde_json::json!({
                "event": "secureConnection",
                "connectionId": conn_id,
                "tls": serde_json::Value::clone(&tls),
            });
            conn.write_meta(&mut meta);
            super::OpOutcome::Json(meta.to_string())
        }
        Some(ServerEvent::ConnectionClosed { conn_id }) => super::OpOutcome::Json(
            serde_json::json!({ "event": "connectionClosed", "connectionId": conn_id }).to_string(),
        ),
        Some(ServerEvent::Drop { conn }) => {
            let mut meta = serde_json::json!({ "event": "drop" });
            conn.write_meta(&mut meta);
            super::OpOutcome::Json(meta.to_string())
        }
        Some(ServerEvent::TlsClientError {
            conn,
            code,
            message,
        }) => {
            let mut meta = serde_json::json!({
                "event": "tlsClientError",
                "code": code,
                "message": message,
            });
            conn.write_meta(&mut meta);
            super::OpOutcome::Json(meta.to_string())
        }
        Some(ServerEvent::Closed { request_id }) => super::OpOutcome::Json(
            serde_json::json!({ "event": "closed", "requestId": request_id }).to_string(),
        ),
        None => super::OpOutcome::Done,
    }
}

/// HTTP/2 connection preface: `PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n` (24 bytes).
/// If the first bytes on the wire match this, the connection is h2c
/// (prior-knowledge HTTP/2). Otherwise, treat it as HTTP/1.1 — exactly
/// what Node's `http2.createServer()` does: accept both protocols.
const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// Bind + spawn an HTTP/2 cleartext (h2c) accept loop. Same request/response
/// lifecycle as http_serve — shared HttpState, same accept/respond ops.
///
/// Each connection auto-detects the protocol: if the client sends the HTTP/2
/// connection preface it runs through hyper's http2 builder, otherwise it
/// falls back to the http1 builder. This matches Node's `http2.createServer()`
/// semantics: h2c with prior knowledge AND HTTP/1.1 clients both work.
pub async fn http2_serve(
    state: Arc<HttpState>,
    host: String,
    port: u16,
    // Applied to the HTTP/1 connections this server also accepts.
    policy: HeadPolicy,
) -> super::OpOutcome {
    let listener = match tokio::net::TcpListener::bind((host.as_str(), port)).await {
        Ok(listener) => listener,
        Err(e) => return super::OpOutcome::Failed(format!("listen {host}:{port}: {e}")),
    };
    let local_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    let server_id = state.next_id();
    let (queue_tx, queue_rx) = mpsc::channel::<ServerEvent>(64);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    // node's Http2Server has none of the http server's timeouts, but this
    // server also takes HTTP/1 and has to wait for a client's first bytes
    // to tell which: until a connection turns out to be HTTP/2, and for
    // good when it is HTTP/1, it is held to the http server's defaults,
    // checked here.
    let server_timeouts = ServerTimeouts::new(TimeoutSettings::default());
    state
        .servers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            server_id,
            ServerEntry {
                queue: Some(queue_rx),
                shutdown: Some(shutdown_tx),
                timeouts: Some(Arc::clone(&server_timeouts)),
                tls: None,
            },
        );
    tokio::spawn(check_connections(
        state.clone(),
        server_id,
        Arc::clone(&server_timeouts),
        shutdown_rx.clone(),
    ));

    let accept_state = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                accepted = listener.accept() => {
                    let Ok((stream, peer)) = accepted else {
                        tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                        continue;
                    };
                    let conn_addrs = ConnAddrs {
                        remote: peer,
                        local: stream.local_addr().ok(),
                    };
                    let Some(slot) = admit(&server_timeouts, &queue_tx, conn_addrs) else {
                        drop(stream);
                        continue;
                    };
                    let conn_state = accept_state.clone();
                    let conn_queue = queue_tx.clone();
                    let conn_timeouts = Arc::clone(&server_timeouts);
                    let mut conn_shutdown = shutdown_rx.clone();
                    tokio::spawn(async move {
                        let _slot = slot;
                        let watch =
                            ConnWatch::new(conn_state.next_id(), server_id, conn_timeouts.clone());
                        let registration = conn_state.register_conn(Arc::clone(&watch));
                        // Peek the first bytes to detect HTTP/2 prior-knowledge.
                        // The preface is 24 bytes; TCP segmentation may deliver
                        // fewer on the first peek. Retry a few times with a
                        // short wait before falling back to HTTP/1.1. A client
                        // that sends nothing is closed at headersTimeout.
                        let mut peek_buf = [0u8; 24];
                        let mut is_h2 = false;
                        for _ in 0..3u8 {
                            let peeked = tokio::select! {
                                peeked = stream.peek(&mut peek_buf) => peeked,
                                _ = conn_shutdown.changed() => return,
                                // Nothing was said: close without an answer
                                // (the client may speak HTTP/2).
                                _ = watch.closed(CloseReason::End) => return,
                            };
                            match peeked {
                                Ok(n) if n >= H2_PREFACE.len() => {
                                    is_h2 = peek_buf[..H2_PREFACE.len()] == *H2_PREFACE;
                                    break;
                                }
                                Ok(n) if n > 0 && n < H2_PREFACE.len() => {
                                    // Partial read: if the first few bytes match
                                    // the preface prefix, wait for more data.
                                    if peek_buf[..n] == H2_PREFACE[..n] {
                                        tokio::time::sleep(Duration::from_millis(5)).await;
                                        continue;
                                    }
                                    // Definitely not h2.
                                    break;
                                }
                                _ => break,
                            }
                        }

                        if is_h2 {
                            // HTTP/2: no timeouts, as in node.
                            drop(registration);
                            let io = hyper_util::rt::TokioIo::new(stream);
                            let service = hyper::service::service_fn(move |req| {
                                handle_request(
                                    conn_state.clone(),
                                    conn_queue.clone(),
                                    req,
                                    false, // http2: buffered until a later slice
                                    conn_addrs,
                                    None,
                                    policy,
                                    None,
                                    Upgrades::Serve,
                                )
                            });
                            let conn = hyper::server::conn::http2::Builder::new(
                                hyper_util::rt::TokioExecutor::new(),
                            )
                            .serve_connection(io, service);
                            let mut conn = std::pin::pin!(conn);
                            let mut shutting_down = false;
                            loop {
                                tokio::select! {
                                    result = conn.as_mut() => {
                                        let _ = result;
                                        break;
                                    }
                                    _ = conn_shutdown.changed(), if !shutting_down => {
                                        shutting_down = true;
                                        conn.as_mut().graceful_shutdown();
                                    }
                                }
                            }
                        } else {
                            let _registration = registration;
                            let service_queue = conn_queue.clone();
                            let service_watch = Arc::clone(&watch);
                            let service = hyper::service::service_fn(move |req| {
                                handle_request(
                                    conn_state.clone(),
                                    service_queue.clone(),
                                    req,
                                    false, // http2: buffered until a later slice
                                    conn_addrs,
                                    None,
                                    policy,
                                    Some(Arc::clone(&service_watch)),
                                    Upgrades::CloseConnect,
                                )
                            });
                            serve_http1(
                                stream,
                                watch,
                                policy,
                                service,
                                conn_queue,
                                false,
                                conn_addrs,
                                conn_shutdown,
                                None,
                            )
                            .await;
                        }
                    });
                }
            }
        }
    });

    super::OpOutcome::Json(
        serde_json::json!({ "serverId": server_id, "port": local_port }).to_string(),
    )
}

/// Backpressured chunk push for streaming responses. A bounded timeout is
/// the half-open backstop: when a client stops reading (or vanishes
/// without a reset the OS has noticed yet), hyper stops draining the body,
/// the channel fills, and `send` would park forever — wedging the JS pump.
/// The timeout ends the stream so the pump always makes progress.
/// Resolves when hyper drops the response body for `stream_id` -- normal
/// completion after end_stream, OR the client tearing the connection down
/// mid-stream. JS distinguishes the cases by whether it already finished
/// the response; the unfinished case surfaces Node's 'close'-without-
/// 'finish' shape on the ServerResponse.
pub async fn http_stream_closed(state: Arc<HttpState>, stream_id: u64) -> super::OpOutcome {
    let watch = state
        .stream_watch
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&stream_id);
    let Some(rx) = watch else {
        // Stream already ended (or never existed): closed by definition.
        return super::OpOutcome::Done;
    };
    // Err(RecvError) IS the signal: the sender rides inside ChannelBody and
    // is dropped, never sent on.
    let _ = rx.await;
    super::OpOutcome::Done
}

pub async fn http_body_push(
    state: Arc<HttpState>,
    stream_id: u64,
    bytes: Vec<u8>,
) -> super::OpOutcome {
    let Some(sender) = state.stream_sender(stream_id) else {
        return super::OpOutcome::Failed(format!("http stream {stream_id} is gone"));
    };
    match tokio::time::timeout(STREAM_PUSH_TIMEOUT, sender.send(bytes)).await {
        Ok(Ok(())) => super::OpOutcome::Done,
        Ok(Err(_)) => {
            // Receiver dropped (hyper ended the response / connection gone).
            state.end_stream(stream_id);
            super::OpOutcome::Failed("client disconnected".to_string())
        }
        Err(_elapsed) => {
            // Stalled: consumer not reading. Drop the stream; the lingering
            // socket is reaped by the OS / graceful close.
            state.end_stream(stream_id);
            super::OpOutcome::Failed("stream stalled: client is not reading".to_string())
        }
    }
}

#[cfg(test)]
mod address_tests {
    use super::*;

    fn meta_for(remote: &str, local: Option<&str>) -> serde_json::Value {
        let conn = ConnAddrs {
            remote: remote.parse().unwrap(),
            local: local.map(|l| l.parse().unwrap()),
        };
        let mut meta = serde_json::json!({});
        conn.write_meta(&mut meta);
        meta
    }

    #[test]
    fn ipv4_peers_are_dotted_quads() {
        let meta = meta_for("192.168.1.55:60884", Some("192.168.1.55:60881"));
        assert_eq!(
            meta,
            serde_json::json!({
                "remoteAddress": "192.168.1.55", "remotePort": 60884, "remoteFamily": "IPv4",
                "localAddress": "192.168.1.55", "localPort": 60881, "localFamily": "IPv4",
            })
        );
    }

    /// node keeps an IPv4 client of a dual-stack listener v4-mapped, with
    /// family IPv6 (measured: `::ffff:127.0.0.1` / `IPv6`). Unmapping it
    /// would change what an allow list compares against.
    #[test]
    fn a_v4_mapped_peer_stays_mapped_and_ipv6() {
        let meta = meta_for("[::ffff:127.0.0.1]:60888", Some("[::ffff:127.0.0.1]:60887"));
        assert_eq!(meta["remoteAddress"], "::ffff:127.0.0.1");
        assert_eq!(meta["remoteFamily"], "IPv6");
        assert_eq!(meta["localAddress"], "::ffff:127.0.0.1");
        assert_eq!(meta["localFamily"], "IPv6");
    }

    #[test]
    fn ipv6_peers_are_compressed() {
        let meta = meta_for("[2600:6c51:403f:4249:0:0:0:1045]:1", None);
        assert_eq!(meta["remoteAddress"], "2600:6c51:403f:4249::1045");
        assert_eq!(meta["remoteFamily"], "IPv6");
        assert!(
            meta.get("localAddress").is_none(),
            "no local end, no fields"
        );
    }

    /// A link-local peer carries its interface (node: `fe80::...%18` on
    /// Windows). Only link-local addresses do; a global one with a scope id
    /// does not.
    #[test]
    fn a_link_local_peer_carries_its_scope() {
        let ll = std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
            "fe80::b222:28be:4382:9993".parse().unwrap(),
            5000,
            0,
            18,
        ));
        let text = node_ip_string(&ll);
        assert!(text.starts_with("fe80::b222:28be:4382:9993%"), "{text}");
        #[cfg(windows)]
        assert_eq!(text, "fe80::b222:28be:4382:9993%18");
        let unscoped = std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
            "fe80::1".parse().unwrap(),
            5000,
            0,
            0,
        ));
        assert_eq!(node_ip_string(&unscoped), "fe80::1");
        let global = std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
            "2001:db8::1".parse().unwrap(),
            5000,
            0,
            7,
        ));
        assert_eq!(node_ip_string(&global), "2001:db8::1");
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    /// The global body budget is the only thing bounding queued request-body
    /// bytes ACROSS requests -- per-request backpressure caps each channel at
    /// 8 chunks, but nothing capped the aggregate. These assert the accounting
    /// the streaming path now performs, because the failure modes are silent
    /// in both directions: under-charging leaves the flood protection off (how
    /// it shipped), and a missed refund fails CLOSED, rejecting every later
    /// upload as "busy" until the process restarts.
    fn state() -> std::sync::Arc<HttpState> {
        std::sync::Arc::new(HttpState::default())
    }

    #[test]
    fn chunk_charges_on_create_and_refunds_on_drop() {
        let state = state();
        assert_eq!(state.body_bytes.load(Ordering::Acquire), 0);
        // The pump charges before constructing; the chunk owns the refund.
        state.body_bytes.fetch_add(1024, Ordering::AcqRel);
        let chunk = BudgetedChunk::new(vec![7u8; 1024], std::sync::Arc::clone(&state));
        assert_eq!(state.body_bytes.load(Ordering::Acquire), 1024);
        drop(chunk);
        assert_eq!(
            state.body_bytes.load(Ordering::Acquire),
            0,
            "dropping a queued chunk must refund its reservation"
        );
    }

    #[test]
    fn into_data_yields_the_bytes_and_still_refunds() {
        let state = state();
        state.body_bytes.fetch_add(4, Ordering::AcqRel);
        let chunk = BudgetedChunk::new(vec![1, 2, 3, 4], std::sync::Arc::clone(&state));
        let data = chunk.into_data();
        assert_eq!(data, vec![1, 2, 3, 4], "the reader must get the real bytes");
        assert_eq!(
            state.body_bytes.load(Ordering::Acquire),
            0,
            "into_data moves the bytes out; the husk still refunds"
        );
    }

    #[test]
    fn budget_rejects_only_past_the_ceiling() {
        let state = state();
        // Just under: admitted.
        let under = global_body_budget() - 1;
        let prev = state.body_bytes.fetch_add(under, Ordering::AcqRel);
        assert!(
            prev + under <= global_body_budget(),
            "under budget is admitted"
        );
        // The next byte crosses it, which is what the pump refuses.
        let prev = state.body_bytes.fetch_add(2, Ordering::AcqRel);
        assert!(
            prev + 2 > global_body_budget(),
            "crossing GLOBAL_BODY_BUDGET must be detectable by the pump"
        );
        state.body_bytes.fetch_sub(2, Ordering::AcqRel);
        state.body_bytes.fetch_sub(under, Ordering::AcqRel);
        assert_eq!(state.body_bytes.load(Ordering::Acquire), 0);
    }
}

#[cfg(test)]
mod takeover_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A connection taken by a request (an upgrade) that arrives while hyper
    /// is still writing an earlier response: the stream comes back once that
    /// response is out, with the bytes read past the taking request's head.
    /// hyper reads the next head before its write buffer is empty when the
    /// previous request's body ends after its response, as here, but polls
    /// that request's handler only once the buffer has drained -- and nothing
    /// woke it then, so the handover (like any request pipelined there) hung
    /// until the client sent something more.
    #[tokio::test]
    async fn a_taken_connection_first_writes_out_what_hyper_held() {
        const BIG: usize = 1024 * 1024;
        // A small pipe: the response backs up into hyper at once.
        let (mut client, server) = tokio::io::duplex(16 * 1024);
        let timeouts = ServerTimeouts::new(TimeoutSettings::default());
        let watch = ConnWatch::new(1, 1, Arc::clone(&timeouts));
        let (route, taken) = UpgradeRoute::new(timeouts);
        let service =
            hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let route = route.clone();
                async move {
                    if req.uri().path() == "/big" {
                        // Answered at once; its body is read after, as the
                        // server's body pump reads it.
                        tokio::spawn(req.into_body().collect());
                        return Ok::<_, RequestAborted>(hyper::Response::new(
                            http_body_util::Full::new(Bytes::from(vec![b'a'; BIG])).boxed(),
                        ));
                    }
                    let head = crate::http_head::parse_request_head(
                        req.extensions()
                            .get::<hyper::ext::RawRequestHead>()
                            .expect("an HTTP/1 head")
                            .as_bytes(),
                        HeadPolicy::process_default(),
                    )
                    .expect("a good head");
                    assert!(route.take(Takeover { id: 7, head }));
                    std::future::pending().await
                }
            });
        let (queue, _events) = mpsc::channel(8);
        let (_shutdown_tx, shutdown) = tokio::sync::watch::channel(false);
        let addrs = ConnAddrs {
            remote: "127.0.0.1:1".parse().unwrap(),
            local: None,
        };
        let served = tokio::spawn(serve_http1(
            server,
            Arc::clone(&watch),
            HeadPolicy::process_default(),
            service,
            queue,
            false,
            addrs,
            shutdown,
            Some(taken),
        ));
        client
            .write_all(b"POST /big HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(watch.unflushed(), "hyper holds most of the response");
        client
            .write_all(b"helloGET /ws HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: x\r\n\r\nafter")
            .await
            .unwrap();
        // Read the response while the handover waits for it to go out.
        let mut response = vec![0u8; 0];
        let reader = async {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = client.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                response.extend_from_slice(&buf[..n]);
                if response.len() >= BIG && response.ends_with(b"UPGRADED") {
                    break;
                }
            }
        };
        let handover = async {
            let (mut stream, head, takeover) =
                served.await.unwrap().expect("the connection is taken");
            assert_eq!(takeover.id, 7);
            assert_eq!(takeover.head.target, "/ws");
            assert_eq!(&head[..], b"after");
            stream.write_all(b"UPGRADED").await.unwrap();
            stream.shutdown().await.unwrap();
        };
        tokio::time::timeout(Duration::from_secs(20), async {
            tokio::join!(reader, handover)
        })
        .await
        .expect("no hang");
        let head_end = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert_eq!(response.len(), head_end + BIG + b"UPGRADED".len());
        assert!(
            response[head_end..head_end + BIG]
                .iter()
                .all(|&b| b == b'a')
        );
        assert!(response.ends_with(b"UPGRADED"));
    }
}
