//! TCP client and server ops (node:net), and the pipe ones that share them.
//!
//! Streams are split into independent read and write halves via
//! `TcpStream::into_split()` -- or, for a Windows named pipe or a Unix domain
//! socket (`net.connect({ path })`, `server.listen(path)`), the halves
//! [`crate::pipe`] makes -- and every op below takes either kind, so a pipe
//! socket reads, writes, ends and closes as a TCP one does. Each half uses
//! the remove-await-reinsert
//! pattern with its own map, so reads and writes proceed concurrently
//! without blocking each other. The closed set prevents handle
//! resurrection when a close races an in-flight read/write -- and holds
//! nothing else: a marker lives exactly as long as the await it guards.

use crate::{NodeSysError, OpOutcome, node_errno, node_error_code};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

/// One stream's read half: a TCP connection's, or a pipe's.
pub(crate) enum ReadHalf {
    Tcp(OwnedReadHalf),
    #[cfg(unix)]
    Unix(tokio::net::unix::OwnedReadHalf),
    #[cfg(windows)]
    Pipe(crate::pipe::PipeRead),
}

impl ReadHalf {
    async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            ReadHalf::Tcp(reader) => reader.read(buf).await,
            #[cfg(unix)]
            ReadHalf::Unix(reader) => reader.read(buf).await,
            #[cfg(windows)]
            ReadHalf::Pipe(reader) => reader.read(buf).await,
        }
    }

    /// The TCP socket underneath, for the socket option a reset sets.
    fn tcp(&self) -> Option<&tokio::net::TcpStream> {
        match self {
            ReadHalf::Tcp(reader) => Some(reader.as_ref()),
            _ => None,
        }
    }
}

/// One stream's write half: a TCP connection's, or a pipe's.
pub(crate) enum WriteHalf {
    Tcp(OwnedWriteHalf),
    #[cfg(unix)]
    Unix(tokio::net::unix::OwnedWriteHalf),
    #[cfg(windows)]
    Pipe(crate::pipe::PipeWrite),
}

impl WriteHalf {
    fn try_write(&self, data: &[u8]) -> std::io::Result<usize> {
        match self {
            WriteHalf::Tcp(writer) => writer.try_write(data),
            #[cfg(unix)]
            WriteHalf::Unix(writer) => writer.try_write(data),
            #[cfg(windows)]
            WriteHalf::Pipe(writer) => writer.try_write(data),
        }
    }

    async fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        match self {
            WriteHalf::Tcp(writer) => writer.write_all(data).await,
            #[cfg(unix)]
            WriteHalf::Unix(writer) => writer.write_all(data).await,
            #[cfg(windows)]
            WriteHalf::Pipe(writer) => writer.write_all(data).await,
        }
    }

    /// Whether dropping the half is its whole shutdown, done in the call: a
    /// socket's is (tokio shuts the write side down when an owned write half
    /// goes, and the FIN leaves at once). A Windows named pipe's shutdown
    /// first waits for the peer to read what was written.
    fn shuts_down_on_drop(&self) -> bool {
        #[cfg(windows)]
        {
            !matches!(self, WriteHalf::Pipe(_))
        }
        #[cfg(not(windows))]
        {
            true
        }
    }

    /// The shutdown of the write side (see [`tcp_shutdown`]). A failure is
    /// not reported: node's afterShutdown reports none.
    async fn shutdown(&mut self) {
        match self {
            WriteHalf::Tcp(writer) => {
                let _ = writer.shutdown().await;
            }
            #[cfg(unix)]
            WriteHalf::Unix(writer) => {
                let _ = writer.shutdown().await;
            }
            #[cfg(windows)]
            WriteHalf::Pipe(writer) => writer.shutdown().await,
        }
    }

    /// The TCP socket underneath, for the socket option a reset sets.
    fn tcp(&self) -> Option<&tokio::net::TcpStream> {
        match self {
            WriteHalf::Tcp(writer) => Some(writer.as_ref()),
            _ => None,
        }
    }

    /// Drop the half WITHOUT the shutdown a socket's owned write half sends
    /// when it goes (see [`discard_writer_reset`]).
    fn forget(self) {
        match self {
            WriteHalf::Tcp(writer) => writer.forget(),
            #[cfg(unix)]
            WriteHalf::Unix(writer) => writer.forget(),
            #[cfg(windows)]
            WriteHalf::Pipe(writer) => drop(writer),
        }
    }
}

/// A Windows named pipe's halves as the registry keeps them.
#[cfg(windows)]
fn pipe_halves(
    (reader, writer): (crate::pipe::PipeRead, crate::pipe::PipeWrite),
) -> (ReadHalf, WriteHalf) {
    (ReadHalf::Pipe(reader), WriteHalf::Pipe(writer))
}

/// A Unix domain socket's halves as the registry keeps them.
#[cfg(unix)]
fn pipe_halves(
    (reader, writer): (
        tokio::net::unix::OwnedReadHalf,
        tokio::net::unix::OwnedWriteHalf,
    ),
) -> (ReadHalf, WriteHalf) {
    (ReadHalf::Unix(reader), WriteHalf::Unix(writer))
}

/// A listening server: a TCP listener, or a pipe's ([`crate::pipe`]).
enum Listener {
    Tcp(tokio::net::TcpListener),
    Pipe(crate::pipe::PipeListener),
}

#[derive(Default)]
pub struct TcpState {
    readers: HashMap<u64, ReadHalf>,
    writers: HashMap<u64, WriteHalf>,
    listeners: HashMap<u64, Listener>,
    /// Stream handles closed while a half was checked out for an await
    /// (the last half back removes the marker), plus server ids closed
    /// mid-accept (`reinsert_listener` removes those). A stream id used to
    /// stay here for the life of the process once closed (#139).
    closed: HashSet<u64>,
    /// The closed stream handles among `closed` that were closed by
    /// [`tcp_reset`]: a half that comes back from its await drops without
    /// a FIN and leaves the socket to close with a reset. Cleared with the
    /// closed marker.
    reset: HashSet<u64>,
    /// Stream handles whose write side has been shut down (the FIN is
    /// out), until they are closed: [`tcp_reset`] refuses them until the
    /// shutdown has finished, as libuv's `uv_tcp_close_reset` refuses a
    /// stream that is shutting down.
    shut_down: HashSet<u64>,
    /// Halves currently out of the maps for an await, per stream handle.
    in_flight: HashMap<u64, u32>,
    /// A parked accept's wake-up, per server id; per stream handle, the
    /// wake-up of the reads and writes parked on it, which [`tcp_close`]
    /// and [`tcp_reset`] send (created by the first read or queued write).
    cancel: HashMap<u64, std::sync::Arc<tokio::sync::Notify>>,
    /// The back of each stream handle's write queue: the latest `tcp_write`
    /// or `tcp_shutdown` issued for it and still pending (see [`WriteTurn`]).
    /// The last turn out removes the entry.
    write_tail: HashMap<u64, (u64, tokio::sync::oneshot::Receiver<()>)>,
    write_seq: u64,
}

impl TcpState {
    pub fn register_stream(&mut self, handle: u64, reader: OwnedReadHalf, writer: OwnedWriteHalf) {
        self.readers.insert(handle, ReadHalf::Tcp(reader));
        self.writers.insert(handle, WriteHalf::Tcp(writer));
    }

    /// Both halves of a TCP stream, for a TLS upgrade. Both present means
    /// nothing is in flight, so there is no await to guard and no marker to
    /// leave. A pipe stream is left where it is: no TLS server runs over a
    /// pipe.
    pub fn take_halves(&mut self, handle: u64) -> Option<(OwnedReadHalf, OwnedWriteHalf)> {
        match (self.readers.remove(&handle), self.writers.remove(&handle)) {
            (Some(ReadHalf::Tcp(reader)), Some(WriteHalf::Tcp(writer))) => {
                // The stream leaves the registry: so does its parked ops'
                // wake-up.
                self.cancel.remove(&handle);
                Some((reader, writer))
            }
            (reader, writer) => {
                if let Some(reader) = reader {
                    self.readers.insert(handle, reader);
                }
                if let Some(writer) = writer {
                    self.writers.insert(handle, writer);
                }
                None
            }
        }
    }

    /// The wake-up of the ops parked on stream `handle` (see [`tcp_close`],
    /// [`tcp_reset`]). Taken with the half the op checks out, under the same
    /// lock, so a close or reset that comes after the checkout always
    /// reaches the op.
    fn wake_for(&mut self, handle: u64) -> std::sync::Arc<tokio::sync::Notify> {
        self.cancel
            .entry(handle)
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Notify::new()))
            .clone()
    }

    fn take_reader(&mut self, handle: u64) -> Option<ReadHalf> {
        let reader = self.readers.remove(&handle)?;
        *self.in_flight.entry(handle).or_insert(0) += 1;
        Some(reader)
    }

    fn take_writer(&mut self, handle: u64) -> Option<WriteHalf> {
        let writer = self.writers.remove(&handle)?;
        *self.in_flight.entry(handle).or_insert(0) += 1;
        Some(writer)
    }

    /// One checked-out half is back (reinserted or dropped). The last one
    /// back after a `tcp_close` clears the closed marker.
    fn release(&mut self, handle: u64) {
        if let Some(n) = self.in_flight.get_mut(&handle) {
            *n -= 1;
            if *n == 0 {
                self.in_flight.remove(&handle);
                self.closed.remove(&handle);
                self.reset.remove(&handle);
            }
        }
    }

    /// (closed markers -- reset and shut-down ones included --, cancel
    /// notifies, handles with a half in flight, readers, writers): all 0
    /// once every stream handle is closed.
    #[cfg(test)]
    pub(crate) fn bookkeeping(&self) -> (usize, usize, usize, usize, usize) {
        (
            self.closed.len() + self.reset.len() + self.shut_down.len(),
            self.cancel.len(),
            self.in_flight.len(),
            self.readers.len(),
            self.writers.len(),
        )
    }

    /// Handles with a write or a shutdown still queued: 0 once every one
    /// issued has finished.
    #[cfg(test)]
    pub(crate) fn queued_writes(&self) -> usize {
        self.write_tail.len()
    }
}

pub type TcpRegistry = std::sync::Arc<std::sync::Mutex<TcpState>>;

/// A half checked out of the registry for an await. Dropping it releases
/// the handle's in-flight count on every exit path (reinserted, or dropped
/// after an error), so the closed marker cannot outlive the await.
struct InFlight {
    registry: TcpRegistry,
    handle: u64,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .release(self.handle);
    }
}

/// One op's place in its stream handle's write queue.
///
/// A write and the shutdown behind it are two ops, and each takes the write
/// half out of the registry for as long as it runs. With no order between
/// them, a `tcp_shutdown` issued while a write was in flight found no writer
/// and returned without sending a FIN -- the half went back afterwards, and
/// the FIN left only when the socket was closed -- so `socket.end()` had to
/// wait for the write's promise before it could even ask: the FIN went out
/// one op round trip late, where libuv queues the shutdown behind the write
/// and sends it in the same loop turn (#156).
///
/// The place is taken when the op is ISSUED -- synchronously, inside
/// `tcp_write` / `tcp_shutdown`, before the future they return is first
/// polled -- so the order is the order JS made the calls in, whatever order
/// the runtime polls the futures in. Each turn waits for the one before it
/// and releases the next when it is dropped: finished, failed, or abandoned
/// (a turn abandoned while still waiting releases the next early; that only
/// happens when the runtime is dropping its tasks).
///
/// An op issued while nothing is queued does not take a turn at all when the
/// socket can finish it on the spot (see [`tcp_write`], [`tcp_shutdown`]).
struct WriteTurn {
    registry: TcpRegistry,
    handle: u64,
    seq: u64,
    before: Option<tokio::sync::oneshot::Receiver<()>>,
    /// Never sent on: dropped with the turn, which is what wakes the next.
    _done: tokio::sync::oneshot::Sender<()>,
}

impl WriteTurn {
    /// The next place in `handle`'s queue. `state` is `registry`, locked by
    /// the caller: whatever it checked before queueing still holds.
    fn take(state: &mut TcpState, registry: &TcpRegistry, handle: u64) -> WriteTurn {
        let (done, released) = tokio::sync::oneshot::channel();
        state.write_seq += 1;
        let seq = state.write_seq;
        let before = state
            .write_tail
            .insert(handle, (seq, released))
            .map(|(_, before)| before);
        WriteTurn {
            registry: registry.clone(),
            handle,
            seq,
            before,
            _done: done,
        }
    }

    /// Until every write and shutdown issued before this one is done.
    async fn wait(&mut self) {
        if let Some(before) = self.before.take() {
            // Err is the turn before being dropped: exactly the signal.
            let _ = before.await;
        }
    }
}

impl Drop for WriteTurn {
    fn drop(&mut self) {
        let mut guard = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        if guard
            .write_tail
            .get(&self.handle)
            .is_some_and(|(seq, _)| *seq == self.seq)
        {
            guard.write_tail.remove(&self.handle);
        }
    }
}

/// How a write or a shutdown left the call that issued it.
enum Issued {
    /// Finished on the spot.
    Done(OpOutcome),
    /// Queued behind the handle's earlier writes, `written` bytes of it
    /// already taken by the socket.
    Queued { turn: WriteTurn, written: usize },
}

fn reinsert_reader(registry: &TcpRegistry, handle: u64, reader: ReadHalf) -> bool {
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    if guard.closed.contains(&handle) {
        if guard.reset.contains(&handle) {
            discard_reader_reset(reader);
        }
        false
    } else {
        guard.readers.insert(handle, reader);
        true
    }
}

fn reinsert_writer(registry: &TcpRegistry, handle: u64, writer: WriteHalf) -> bool {
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    if guard.closed.contains(&handle) {
        if guard.reset.contains(&handle) {
            discard_writer_reset(writer);
        }
        false
    } else {
        guard.writers.insert(handle, writer);
        true
    }
}

/// SO_LINGER {on, 0}: the socket's close sends a reset (RST) rather than
/// the FIN of an orderly release, and drops whatever is still unsent --
/// libuv's `uv_tcp_close_reset`. Through either half: the option belongs to
/// the one socket both share. EINVAL is ignored as libuv ignores it (POSIX
/// allows it for a socket already shut down; macOS and illumos return it).
fn set_linger_zero(stream: &tokio::net::TcpStream) -> std::io::Result<()> {
    match socket2::SockRef::from(stream).set_linger(Some(std::time::Duration::ZERO)) {
        Err(e) if node_error_code(&e) == "EINVAL" => Ok(()),
        other => other,
    }
}

/// Arm a stream that is about to be dropped so that its close is a reset:
/// node's resetAndDestroy() on a socket oam's http server or http client
/// holds rather than this registry. Best-effort, as a failed close is: the
/// stream closes either way.
pub(crate) fn arm_reset(stream: &tokio::net::TcpStream) {
    let _ = set_linger_zero(stream);
}

/// A read half of a reset stream, out of the registry for good. The socket
/// closes (with the reset) when the last half goes, so each half re-arms
/// the linger on its way out: the reset may have found neither in the maps.
fn discard_reader_reset(reader: ReadHalf) {
    if let Some(stream) = reader.tcp() {
        let _ = set_linger_zero(stream);
    }
    drop(reader);
}

/// The write half of a reset stream: dropped WITHOUT the shutdown an owned
/// write half sends when it goes, which would put a FIN ahead of the reset.
fn discard_writer_reset(writer: WriteHalf) {
    if let Some(stream) = writer.tcp() {
        let _ = set_linger_zero(stream);
    }
    writer.forget();
}

/// Reinsert a listener ONLY if it was not closed mid-flight.
fn reinsert_listener(registry: &TcpRegistry, server_id: u64, listener: Listener) -> bool {
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    if guard.closed.remove(&server_id) {
        drop(listener);
        false
    } else {
        guard.listeners.insert(server_id, listener);
        true
    }
}

/// Format a SocketAddr as a Node-style structured address object. Shared
/// with the TLS ops, whose sockets carry the same `localAddr`/`remoteAddr`.
pub(crate) fn addr_to_json(addr: std::net::SocketAddr) -> serde_json::Value {
    serde_json::json!({
        "address": addr.ip().to_string(),
        "port": addr.port(),
        "family": if addr.is_ipv6() { "IPv6" } else { "IPv4" },
    })
}

/// Where a server's `listen()` asked to listen: the host it named (`None`:
/// none), the port, and node's `ipv6Only` option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenAt {
    pub host: Option<String>,
    pub port: u16,
    pub ipv6_only: bool,
}

/// The backlog every listener is created with: node's default (lib/net.js
/// `backlog || 511`).
const LISTEN_BACKLOG: i32 = 511;

/// Bind and listen as node's `server.listen()` does (lib/net.js
/// `setupListenHandle` / `createServerHandle`, libuv underneath), for every
/// server kind -- net, tls, http, https and http2:
///
/// - No host: `::`, dual-stack, so IPv4 clients reach it too (as
///   `::ffff:a.b.c.d`). Where that socket cannot be had -- no IPv6 on the
///   host -- `0.0.0.0` instead. An address in use is not a reason to fall
///   back: libuv reports it at `listen`, after the choice was made, so it
///   fails on `::`.
/// - An IPv6 address, `::` included, is dual-stack too unless `ipv6Only`.
/// - A name is looked up (getaddrinfo, the resolver's order) and the first
///   address is bound, as node's `lookupAndListen` does.
///
/// Returns the listener and the address it is bound at. A failure is node's
/// `listen` error (`listen EADDRINUSE: address already in use :::8080`), or
/// the lookup's for a name.
pub async fn bind_listener(
    at: &ListenAt,
) -> Result<(tokio::net::TcpListener, std::net::SocketAddr), Box<NodeSysError>> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    let ip = match at.host.as_deref() {
        None => {
            let any6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), at.port);
            match bind_one(any6, at.ipv6_only) {
                Ok(bound) => return Ok(bound),
                Err(ListenFailure::Listen(e)) => {
                    return Err(Box::new(listen_error(&e, "::", at.port)));
                }
                Err(ListenFailure::Bind(e)) if e.kind() == std::io::ErrorKind::AddrInUse => {
                    return Err(Box::new(listen_error(&e, "::", at.port)));
                }
                Err(ListenFailure::Bind(_)) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            }
        }
        Some(host) => match host.parse::<IpAddr>() {
            Ok(ip) => ip,
            Err(_) => match crate::net_connect::resolve(host, None, 0).await {
                Ok(addrs) => addrs[0],
                Err(crate::net_connect::ConnectError::Resolve(e)) => return Err(e),
                Err(other) => {
                    return Err(Box::new(listen_error(
                        &std::io::Error::other(other),
                        host,
                        at.port,
                    )));
                }
            },
        },
    };
    match bind_one(SocketAddr::new(ip, at.port), at.ipv6_only) {
        Ok(bound) => Ok(bound),
        Err(ListenFailure::Bind(e) | ListenFailure::Listen(e)) => {
            Err(Box::new(listen_error(&e, &ip.to_string(), at.port)))
        }
    }
}

/// Which step of [`bind_one`] failed: up to and including the bind, or the
/// listen after it.
enum ListenFailure {
    Bind(std::io::Error),
    Listen(std::io::Error),
}

/// One listening socket at `addr`: SO_REUSEADDR off Windows (as libuv and
/// std set it), IPV6_V6ONLY as `ipv6_only` says for an IPv6 one.
fn bind_one(
    addr: std::net::SocketAddr,
    ipv6_only: bool,
) -> Result<(tokio::net::TcpListener, std::net::SocketAddr), ListenFailure> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))
        .map_err(ListenFailure::Bind)?;
    if addr.is_ipv6() {
        socket.set_only_v6(ipv6_only).map_err(ListenFailure::Bind)?;
    }
    #[cfg(not(windows))]
    socket
        .set_reuse_address(true)
        .map_err(ListenFailure::Bind)?;
    socket.bind(&addr.into()).map_err(ListenFailure::Bind)?;
    socket
        .listen(LISTEN_BACKLOG)
        .map_err(ListenFailure::Listen)?;
    socket
        .set_nonblocking(true)
        .map_err(ListenFailure::Listen)?;
    let listener =
        tokio::net::TcpListener::from_std(socket.into()).map_err(ListenFailure::Listen)?;
    let local = listener.local_addr().map_err(ListenFailure::Listen)?;
    Ok((listener, local))
}

/// node's `UVExceptionWithHostPort(err, 'listen', address, port)`: `listen
/// CODE: <uv message> address:port`, the `:port` (and a `port` key) only for
/// a non-zero port.
fn listen_error(error: &std::io::Error, address: &str, port: u16) -> NodeSysError {
    let code = node_error_code(error);
    let text = crate::uv_strerror(code)
        .map(str::to_string)
        .unwrap_or_else(|| error.to_string());
    let mut message = format!("listen {code}: {text} {address}");
    if port > 0 {
        message.push_str(&format!(":{port}"));
    }
    NodeSysError {
        code: code.to_string(),
        message,
        errno: node_errno(code, error),
        syscall: Some("listen".to_string()),
        hostname: None,
        address: Some(address.to_string()),
        port: (port > 0).then_some(port),
    }
}

/// Map an IO error to a NodeFailed outcome with the appropriate syscall.
///
/// The message is node's for a stream or server error, `new
/// ErrnoException(err, syscall)`: `read ECONNRESET`, `write EPIPE`, `accept
/// EMFILE` -- what the peer of a `resetAndDestroy()` reads (measured on
/// v22.22.2). It used to be the fs shape, `ECONNRESET: connection reset by
/// peer, read '66'`, the handle's internal id standing in for a path.
fn tcp_fail(error: std::io::Error, syscall: &str) -> OpOutcome {
    errno_failure(&error, syscall)
}

/// [`tcp_fail`]'s shape for an error read off some other socket: the HTTP
/// client's connection, whose reset node reports as the same `read
/// ECONNRESET`.
pub(crate) fn errno_failure(error: &std::io::Error, syscall: &str) -> OpOutcome {
    let code = node_error_code(error);
    // syscall + errno, but no `path`: a host:port is not a filesystem path,
    // and node does not put one on a net error.
    OpOutcome::node_failed_at(
        code,
        format!("{syscall} {code}"),
        syscall,
        None,
        node_errno(code, error),
    )
}

/// net.connect / net.createConnection: establish a TCP client connection.
/// Returns Json {handle, localAddr, remoteAddr}.
///
/// The connect itself is node's algorithm (crate::net_connect, shared with
/// tls_connect): `attempt_timeout` is `net.getDefaultAutoSelectFamilyAttemptTimeout()`
/// as JS read it for this call, and a failure rejects with node's own shape --
/// `connect ECONNREFUSED 127.0.0.1:8080` with errno, code, syscall, address and
/// port, a `getaddrinfo ENOTFOUND host` DNS error, or the NodeAggregateError of
/// a name whose every address failed.
pub async fn tcp_connect(
    registry: TcpRegistry,
    ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
    host: String,
    port: u16,
    attempt_timeout: std::time::Duration,
) -> OpOutcome {
    tcp_connect_pinned(registry, ids, host, port, attempt_timeout, None, None).await
}

/// [`tcp_connect`] with the addresses JS resolved for `host` standing in for
/// getaddrinfo: net.connect's `lookup` option, a replaced `dns.lookup`, or a
/// redeemed `netResolve` ticket (see `net_connect::ResolvedAnswers`). The
/// engine has already checked every one of them against the net grant.
/// `local` is net.connect's `localAddress` / `localPort`.
pub async fn tcp_connect_pinned(
    registry: TcpRegistry,
    ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
    host: String,
    port: u16,
    attempt_timeout: std::time::Duration,
    pin: Option<crate::net_connect::Pin>,
    local: Option<crate::net_connect::LocalBind>,
) -> OpOutcome {
    let opts = crate::net_connect::ConnectOptions {
        attempt_timeout,
        pin,
        local,
        ..crate::net_connect::ConnectOptions::default()
    };
    let stream = match crate::net_connect::connect(&host, port, &opts).await {
        Ok(connected) => connected.stream,
        Err(e) => return e.to_outcome(),
    };

    let local_addr = stream.local_addr().ok();
    let remote_addr = stream.peer_addr().ok();
    let handle = ids.fetch_add(1, Ordering::Relaxed);
    let (reader, writer) = stream.into_split();
    registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .register_stream(handle, reader, writer);

    let mut payload = serde_json::json!({ "handle": handle });
    if let Some(la) = local_addr {
        payload["localAddr"] = addr_to_json(la);
    }
    if let Some(ra) = remote_addr {
        payload["remoteAddr"] = addr_to_json(ra);
    }
    OpOutcome::Json(payload.to_string())
}

/// net.connect({ path }): dial the Windows named pipe or Unix domain socket
/// `target` ([`crate::pipe::connect`]) and register the stream with the TCP
/// ones, so every stream op takes it. `target` is `path` as the permission
/// gate resolved it (the same string unless a relative path had to be
/// resolved against the cwd); `path`, as the script gave it, is what an
/// error names. Returns Json {handle}: a pipe has no addresses. A failure
/// rejects with node's `connect ENOENT <path>` shape.
pub async fn pipe_connect(
    registry: TcpRegistry,
    ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
    path: String,
    target: String,
) -> OpOutcome {
    if let Err(e) = crate::pipe::refuse_nul("connect", &target, &path) {
        return OpOutcome::sys(*e);
    }
    let (reader, writer) = match crate::pipe::connect(&target, &path).await {
        Ok(halves) => pipe_halves(halves),
        Err(e) => return OpOutcome::sys(*e),
    };
    let handle = ids.fetch_add(1, Ordering::Relaxed);
    {
        let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
        guard.readers.insert(handle, reader);
        guard.writers.insert(handle, writer);
    }
    OpOutcome::Json(serde_json::json!({ "handle": handle }).to_string())
}

/// Read up to `len` bytes from a TCP stream. Remove-await-reinsert on the
/// read half only -- writes proceed independently.
pub async fn tcp_read(registry: TcpRegistry, handle: u64, len: usize) -> OpOutcome {
    let wake;
    let (mut reader, woken) = {
        let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
        let Some(reader) = guard.take_reader(handle) else {
            return OpOutcome::Failed(format!("tcp: read handle {handle} is gone"));
        };
        wake = guard.wake_for(handle);
        // Created under the lock: a reset from here on reaches it.
        (reader, wake.notified())
    };
    let _in_flight = InFlight {
        registry: registry.clone(),
        handle,
    };

    let mut buf = vec![0u8; len.clamp(1, 8 * 1024 * 1024)];
    // The read first: when it is ready on the first poll, the wake-up is
    // never polled (nor registered with the Notify) at all.
    let read = tokio::select! {
        biased;
        read = reader.read(&mut buf) => read,
        () = woken => {
            // tcp_close / tcp_reset: this half is the one keeping the socket
            // open. The handle is marked closed, so it is dropped here, with
            // the reset when it was one; the read ends like an EOF.
            reinsert_reader(&registry, handle, reader);
            return OpOutcome::Done;
        }
    };
    match read {
        Ok(0) => {
            reinsert_reader(&registry, handle, reader);
            OpOutcome::Done
        }
        Ok(n) => {
            reinsert_reader(&registry, handle, reader);
            buf.truncate(n);
            OpOutcome::Bytes(buf)
        }
        Err(e) => tcp_fail(e, "read"),
    }
}

/// Write bytes to a TCP stream. Remove-await-reinsert on the write half
/// only -- reads proceed independently.
///
/// The write starts HERE, in the call, before the returned future is polled:
///
/// - With nothing queued for the handle, the socket is offered the bytes at
///   once (libuv's `uv_try_write`). What a writable socket takes is on the
///   wire when this returns, so a write the kernel had room for is complete
///   whatever happens to the handle next -- as in node, where `write()` then
///   `destroy()` still delivers the write.
/// - What is left (or all of it, behind earlier writes) takes the next
///   place in the handle's write queue ([`WriteTurn`]): writes and the
///   shutdown of one handle run in the order they were issued.
pub fn tcp_write(
    registry: TcpRegistry,
    handle: u64,
    data: Vec<u8>,
) -> impl std::future::Future<Output = OpOutcome> + Send + 'static {
    started_future(tcp_write_start(registry, handle, data))
}

/// [`tcp_write`], telling the caller whether the write finished in the
/// call: [`Started::Done`] when the socket took every byte at once (or the
/// write failed on the spot), [`Started::Pending`] with the rest of the
/// write otherwise. The engine settles a write that finished in the call
/// without a trip through the event loop, as libuv reports a `uv_try_write`
/// that took everything: node runs that write's callback before the 'close'
/// of a `destroy()` on the next line, and so must oam (#156).
pub fn tcp_write_start(
    registry: TcpRegistry,
    handle: u64,
    data: Vec<u8>,
) -> Started<impl std::future::Future<Output = OpOutcome> + Send + 'static> {
    let issued = {
        let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
        let mut written = 0;
        let mut finished = None;
        if !guard.write_tail.contains_key(&handle) {
            // Nothing queued means nothing in flight either: the half is in
            // the map unless the handle is closed or already shut down.
            match guard.writers.get(&handle) {
                None => {
                    finished = Some(OpOutcome::Failed(format!(
                        "tcp: write handle {handle} is gone"
                    )));
                }
                Some(writer) => match writer.try_write(&data) {
                    Ok(n) if n == data.len() => finished = Some(OpOutcome::Done),
                    Ok(n) => written = n,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => finished = Some(tcp_fail(e, "write")),
                },
            }
        }
        match finished {
            Some(outcome) => Issued::Done(outcome),
            None => Issued::Queued {
                turn: WriteTurn::take(&mut guard, &registry, handle),
                written,
            },
        }
    };
    let (mut turn, written) = match issued {
        Issued::Done(outcome) => return Started::Done(outcome),
        Issued::Queued { turn, written } => (turn, written),
    };
    Started::Pending(async move {
        turn.wait().await;
        let wake;
        let (mut writer, woken) = {
            let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
            let Some(writer) = guard.take_writer(handle) else {
                return OpOutcome::Failed(format!("tcp: write handle {handle} is gone"));
            };
            wake = guard.wake_for(handle);
            (writer, wake.notified())
        };
        // Declared after `turn`, so dropped before it: the half is back in
        // the registry (or gone) by the time the next turn is released.
        let _in_flight = InFlight {
            registry: registry.clone(),
            handle,
        };

        // A write the peer is not draining parks here for as long as the
        // peer likes; a close or a reset must not wait for it (see
        // tcp_close). What it has not handed the socket yet is dropped, as
        // libuv cancels a write still pending when its handle closes.
        let wrote = tokio::select! {
            biased;
            wrote = writer.write_all(&data[written..]) => wrote,
            () = woken => {
                reinsert_writer(&registry, handle, writer);
                return OpOutcome::Failed(format!("tcp: write handle {handle} is gone"));
            }
        };
        match wrote {
            Ok(()) => {
                reinsert_writer(&registry, handle, writer);
                OpOutcome::Done
            }
            Err(e) => tcp_fail(e, "write"),
        }
    })
}

/// Half-close the write side (sends FIN). Removes the write half and
/// drops it -- no further writes are possible.
///
/// Like [`tcp_write`], it starts in the call. With nothing queued for the
/// handle the FIN leaves at once; otherwise the shutdown takes the next
/// place in the handle's write queue ([`WriteTurn`]), as libuv queues a
/// shutdown behind its writes, and the FIN leaves as soon as the last byte
/// before it has been written. Either way a caller asks for the FIN in the
/// same turn as its last write, without waiting for that write to finish.
pub fn tcp_shutdown(
    registry: TcpRegistry,
    handle: u64,
) -> impl std::future::Future<Output = OpOutcome> + Send + 'static {
    started_future(tcp_shutdown_start(registry, handle))
}

/// [`tcp_shutdown`], telling the caller whether it finished in the call
/// (see [`tcp_write_start`]): with nothing queued the FIN leaves at once.
pub fn tcp_shutdown_start(
    registry: TcpRegistry,
    handle: u64,
) -> Started<impl std::future::Future<Output = OpOutcome> + Send + 'static> {
    let issued = {
        let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
        // A named pipe's shutdown cannot finish in the call (it waits for
        // the peer to read): it is queued like one behind a write.
        let in_call = guard
            .writers
            .get(&handle)
            .is_none_or(WriteHalf::shuts_down_on_drop);
        if guard.write_tail.contains_key(&handle) || !in_call {
            Issued::Queued {
                turn: WriteTurn::take(&mut guard, &registry, handle),
                written: 0,
            }
        } else {
            // Dropping the half IS the shutdown: tokio shuts the write side
            // of the socket down when an owned write half goes. Nothing to
            // do for a handle that is closed or already shut down.
            if let Some(writer) = guard.writers.remove(&handle) {
                drop(writer);
                guard.shut_down.insert(handle);
            }
            Issued::Done(OpOutcome::Done)
        }
    };
    let mut turn = match issued {
        Issued::Done(outcome) => return Started::Done(outcome),
        Issued::Queued { turn, .. } => turn,
    };
    Started::Pending(async move {
        turn.wait().await;
        let wake;
        let (writer, woken) = {
            let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
            let Some(writer) = guard.take_writer(handle) else {
                return OpOutcome::Done;
            };
            // The FIN goes out now: from here on a reset is refused.
            guard.shut_down.insert(handle);
            wake = guard.wake_for(handle);
            (writer, wake.notified())
        };
        let mut writer = writer;
        let _in_flight = InFlight {
            registry: registry.clone(),
            handle,
        };
        // A named pipe's shutdown waits for the peer to read; a close must
        // not wait for that (see tcp_close), and drops the half at once.
        tokio::select! {
            biased;
            () = writer.shutdown() => {}
            () = woken => {}
        }
        drop(writer);
        OpOutcome::Done
    })
}

/// How [`tcp_write_start`] or [`tcp_shutdown_start`] left the call.
pub enum Started<F> {
    /// Finished in the call, with this outcome.
    Done(OpOutcome),
    /// Still to finish: the future completes the op.
    Pending(F),
}

/// The single future [`tcp_write`] / [`tcp_shutdown`] return, whichever way
/// the op left the call.
async fn started_future<F>(started: Started<F>) -> OpOutcome
where
    F: std::future::Future<Output = OpOutcome>,
{
    match started {
        Started::Done(outcome) => outcome,
        Started::Pending(rest) => rest.await,
    }
}

/// Close a TCP stream, at once, as libuv's `uv_close` does: remove both
/// halves and, if one is out for an await, mark the handle closed so that
/// await does not resurrect it -- the mark is cleared when the await
/// returns (#139) -- and wake it, so it drops its half now.
///
/// The socket closes when its last half goes. A flowing socket's read is
/// always parked, and a write the peer is not draining parks too: left to
/// finish, they kept the descriptor open until the peer answered the FIN
/// (or drained), so a destroy() of a socket whose peer never reads -- a
/// paused one -- held it, and the process, open for good. The parked read
/// ends like an EOF, the parked write fails as a write to a closed handle
/// (the writes queued behind it find the handle gone).
pub fn tcp_close(registry: &TcpRegistry, handle: u64) {
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    guard.readers.remove(&handle);
    guard.writers.remove(&handle);
    guard.shut_down.remove(&handle);
    // The ops parked on the handle hold their own reference to it.
    let wake = guard.cancel.remove(&handle);
    if guard.in_flight.get(&handle).is_some_and(|n| *n > 0) {
        guard.closed.insert(handle);
    }
    drop(guard);
    if let Some(wake) = wake {
        wake.notify_waiters();
    }
}

/// Why [`tcp_reset`] could not reset a stream: node's errno code for it.
/// The stream is closed either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResetRefused(pub &'static str);

/// node's `socket.resetAndDestroy()` (libuv's `uv_tcp_close_reset`): close
/// the stream so that its peer sees a reset (`read ECONNRESET`) instead of
/// an end, discarding whatever is still unsent.
///
/// SO_LINGER {on, 0}, then the close. The close happens when the last half
/// of the stream goes, and a half out for an await (the parked read of a
/// flowing socket, a write the peer is not draining) is woken to go at
/// once, re-arming the linger on its way out; no half drops with the FIN
/// an owned write half otherwise sends. The writes and the shutdown queued
/// behind find the handle gone.
///
/// Refused with EINVAL, as libuv refuses it, while the stream's shutdown
/// is under way: its FIN is out (a shutdown still queued behind writes is
/// not -- the reset cancels it) and `shutdown_finished` is false, i.e. JS
/// has not yet seen the shutdown finish (node's 'finish', which libuv's
/// shutdown callback emits; once it has run libuv resets as usual). The
/// stream is then closed as [`tcp_close`] closes it (libuv leaves it open;
/// see docs/node-divergences.md). Any other failure to set the linger is
/// returned the same way.
pub fn tcp_reset(
    registry: &TcpRegistry,
    handle: u64,
    shutdown_finished: bool,
) -> Result<(), ResetRefused> {
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    if !shutdown_finished && guard.shut_down.contains(&handle) {
        drop(guard);
        tcp_close(registry, handle);
        return Err(ResetRefused("EINVAL"));
    }
    let state = &mut *guard;
    state.shut_down.remove(&handle);
    let reader = state.readers.remove(&handle);
    let writer = state.writers.remove(&handle);
    // A pipe has no reset (node refuses one in JS, ERR_INVALID_HANDLE_TYPE):
    // were one asked for, it would just close.
    let here = match (&reader, &writer) {
        (Some(reader), _) => reader.tcp(),
        (None, Some(writer)) => writer.tcp(),
        (None, None) => None,
    };
    if let Some(Err(e)) = here.map(set_linger_zero) {
        // libuv returns the error and closes nothing; the stream is closed
        // here all the same, in the ordinary way, so nothing leaks.
        let code = node_error_code(&e);
        if let Some(reader) = reader {
            state.readers.insert(handle, reader);
        }
        if let Some(writer) = writer {
            state.writers.insert(handle, writer);
        }
        drop(guard);
        tcp_close(registry, handle);
        return Err(ResetRefused(code));
    }
    if state.in_flight.get(&handle).is_some_and(|n| *n > 0) {
        state.closed.insert(handle);
        state.reset.insert(handle);
    }
    if let Some(wake) = state.cancel.remove(&handle) {
        wake.notify_waiters();
    }
    drop(guard);
    if let Some(writer) = writer {
        writer.forget();
    }
    drop(reader);
    Ok(())
}

/// net.createServer + server.listen: bind a TCP listener ([`bind_listener`]).
/// Returns Json {serverId, port, hostname, family}.
pub async fn tcp_listen(
    registry: TcpRegistry,
    ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
    at: ListenAt,
) -> OpOutcome {
    let (listener, local_addr) = match bind_listener(&at).await {
        Ok(bound) => bound,
        Err(e) => return OpOutcome::sys(*e),
    };

    let server_id = ids.fetch_add(1, Ordering::Relaxed);
    registry
        .lock()
        .expect("tcp registry lock")
        .listeners
        .insert(server_id, Listener::Tcp(listener));

    OpOutcome::Json(
        serde_json::json!({
            "serverId": server_id,
            "port": local_addr.port(),
            "hostname": local_addr.ip().to_string(),
            "family": if local_addr.is_ipv6() { "IPv6" } else { "IPv4" },
        })
        .to_string(),
    )
}

/// net.createServer + server.listen(path): listen on the Windows named pipe
/// or Unix domain socket `path` ([`crate::pipe::bind`]). Returns Json
/// {serverId}; [`tcp_accept`] and [`tcp_server_close`] take the server as
/// they take a TCP one. `target` and `path` as for [`pipe_connect`]. A
/// failure rejects with node's `listen EADDRINUSE: address already in use
/// <path>` shape.
pub async fn pipe_listen(
    registry: TcpRegistry,
    ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
    path: String,
    target: String,
) -> OpOutcome {
    if let Err(e) = crate::pipe::refuse_nul("listen", &target, &path) {
        return OpOutcome::sys(*e);
    }
    let listener = match crate::pipe::bind(&target, &path) {
        Ok(listener) => listener,
        Err(e) => return OpOutcome::sys(*e),
    };
    let server_id = ids.fetch_add(1, Ordering::Relaxed);
    registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .listeners
        .insert(server_id, Listener::Pipe(listener));
    OpOutcome::Json(serde_json::json!({ "serverId": server_id }).to_string())
}

/// A connection a listener accepted.
enum Accepted {
    Tcp(tokio::net::TcpStream, std::net::SocketAddr),
    Pipe(ReadHalf, WriteHalf),
}

async fn accept_one(listener: &mut Listener) -> std::io::Result<Accepted> {
    match listener {
        Listener::Tcp(listener) => {
            let (stream, peer) = listener.accept().await?;
            Ok(Accepted::Tcp(stream, peer))
        }
        Listener::Pipe(listener) => {
            let (reader, writer) = pipe_halves(listener.accept().await?);
            Ok(Accepted::Pipe(reader, writer))
        }
    }
}

/// Accept one connection from a listener, TCP or pipe. Remove-await-reinsert
/// on the LISTENER. Splits and stores the accepted stream's read/write
/// halves. Returns Json {handle, remoteAddr, localAddr?} -- for a pipe just
/// {handle} -- or Done if the listener was closed. The accepted socket's own
/// address too, so `socket.address()`, `localAddress` and `localPort` on a
/// server-side net.Socket answer as node's do.
pub async fn tcp_accept(
    registry: TcpRegistry,
    server_id: u64,
    stream_ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> OpOutcome {
    let (mut listener, notify) = {
        let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
        // Closed, or another accept already holds the listener. Checked
        // BEFORE the cancel Notify is created: an accept that lands after
        // `tcp_server_close` removed the server's Notify would otherwise
        // insert a fresh one that nothing ever removes (#139).
        let Some(listener) = guard.listeners.remove(&server_id) else {
            return OpOutcome::Done;
        };
        let notify = guard
            .cancel
            .entry(server_id)
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Notify::new()))
            .clone();
        (listener, notify)
    };

    tokio::select! {
        result = accept_one(&mut listener) => {
            reinsert_listener(&registry, server_id, listener);
            let accepted = match result {
                Ok(accepted) => accepted,
                Err(e) => return tcp_fail(e, "accept"),
            };
            let handle = stream_ids.fetch_add(1, Ordering::Relaxed);
            match accepted {
                Accepted::Tcp(stream, peer_addr) => {
                    let local_addr = stream.local_addr().ok();
                    let (reader, writer) = stream.into_split();
                    registry
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .register_stream(handle, reader, writer);

                    let mut payload = serde_json::json!({
                        "handle": handle,
                        "remoteAddr": addr_to_json(peer_addr),
                    });
                    if let Some(la) = local_addr {
                        payload["localAddr"] = addr_to_json(la);
                    }
                    OpOutcome::Json(payload.to_string())
                }
                Accepted::Pipe(reader, writer) => {
                    {
                        let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
                        guard.readers.insert(handle, reader);
                        guard.writers.insert(handle, writer);
                    }
                    OpOutcome::Json(serde_json::json!({ "handle": handle }).to_string())
                }
            }
        }
        _ = notify.notified() => {
            // The close that woke this accept set the marker for the two
            // accept-completed branches, which reinsert the listener; this
            // one drops it itself, so the marker has nothing left to guard.
            drop(listener);
            registry
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .closed
                .remove(&server_id);
            OpOutcome::Done
        }
    }
}

/// Close a TCP server. Remove the listener; if an accept is parked on it, mark
/// it closed so the accept does not resurrect it, and wake the accept. The
/// marker lives only while an accept is in flight: `reinsert_listener`
/// removes it when that accept observes it, and a server closed with no
/// accept parked leaves nothing behind (#139).
pub fn tcp_server_close(registry: &TcpRegistry, server_id: u64) {
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    let had_listener = guard.listeners.remove(&server_id).is_some();
    if let Some(notify) = guard.cancel.remove(&server_id) {
        // An accept holds the listener while it is parked, so "no listener
        // in the map but a cancel Notify registered" is exactly "an accept is
        // in flight": the marker is what stops it re-inserting the listener.
        if !had_listener {
            guard.closed.insert(server_id);
        }
        notify.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    /// Accepts N connections; each echoes its first read, then waits for the
    /// client's FIN (so a second client read can park) before closing.
    async fn echo_server(listener: tokio::net::TcpListener, connections: usize) {
        for _ in 0..connections {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 64];
                if let Ok(n) = stream.read(&mut buf).await {
                    let _ = stream.write_all(&buf[..n]).await;
                }
                let _ = stream.read(&mut buf).await;
            });
        }
    }

    /// #139's tcp.rs twin: every closed stream leaves the registry empty,
    /// whether the close came after EOF, while a read was parked, or before
    /// a late read.
    #[tokio::test]
    async fn closed_streams_leave_no_bookkeeping_behind() {
        let registry: TcpRegistry = std::sync::Arc::new(std::sync::Mutex::new(TcpState::default()));
        let ids = std::sync::Arc::new(AtomicU64::new(1));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        const N: usize = 9;
        tokio::spawn(echo_server(listener, N));

        for i in 0..N {
            let OpOutcome::Json(payload) = tcp_connect(
                registry.clone(),
                ids.clone(),
                "127.0.0.1".into(),
                port,
                crate::net_connect::DEFAULT_ATTEMPT_TIMEOUT,
            )
            .await
            else {
                panic!("connect failed");
            };
            let info: serde_json::Value = serde_json::from_str(&payload).unwrap();
            let handle = info["handle"].as_u64().unwrap();

            assert!(matches!(
                tcp_write(registry.clone(), handle, b"ping".to_vec()).await,
                OpOutcome::Done
            ));
            assert!(matches!(
                tcp_read(registry.clone(), handle, 64).await,
                OpOutcome::Bytes(b) if b == b"ping"
            ));
            match i % 3 {
                0 => {
                    // Our FIN, the server's close, EOF, then close.
                    assert!(matches!(
                        tcp_shutdown(registry.clone(), handle).await,
                        OpOutcome::Done
                    ));
                    assert!(matches!(
                        tcp_read(registry.clone(), handle, 64).await,
                        OpOutcome::Done
                    ));
                    tcp_close(&registry, handle);
                }
                1 => {
                    // Close while a read is parked: the marker's one
                    // legitimate use. Dropping the write half sends FIN, the
                    // server closes, the parked read sees EOF and must NOT
                    // reinsert its half.
                    let parked = tokio::spawn(tcp_read(registry.clone(), handle, 64));
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    tcp_close(&registry, handle);
                    assert!(matches!(parked.await.unwrap(), OpOutcome::Done));
                }
                _ => {
                    // Close first; a read that lands afterwards fails.
                    tcp_close(&registry, handle);
                    assert!(matches!(
                        tcp_read(registry.clone(), handle, 64).await,
                        OpOutcome::Failed(_)
                    ));
                }
            }
        }

        let bookkeeping = registry.lock().unwrap().bookkeeping();
        assert_eq!(
            bookkeeping,
            (0, 0, 0, 0, 0),
            "(closed, cancel, in_flight, readers, writers)"
        );
    }

    /// The server-side twin of the test above (#139): a listener closed with
    /// no accept parked, closed while one is parked, or closed after accepts
    /// completed leaves nothing behind, and an accept issued after the close
    /// creates no cancel Notify. Also pins the accepted socket's own address
    /// in the accept payload.
    #[tokio::test]
    async fn closed_servers_leave_no_bookkeeping_behind() {
        let registry: TcpRegistry = std::sync::Arc::new(std::sync::Mutex::new(TcpState::default()));
        let ids = std::sync::Arc::new(AtomicU64::new(1));

        for shape in 0..3 {
            let OpOutcome::Json(payload) = tcp_listen(
                registry.clone(),
                ids.clone(),
                ListenAt {
                    host: Some("127.0.0.1".into()),
                    port: 0,
                    ipv6_only: false,
                },
            )
            .await
            else {
                panic!("listen failed");
            };
            let info: serde_json::Value = serde_json::from_str(&payload).unwrap();
            let server_id = info["serverId"].as_u64().unwrap();
            let port = u16::try_from(info["port"].as_u64().unwrap()).unwrap();
            match shape {
                0 => {
                    // Closed with nothing parked: no marker may be left.
                    tcp_server_close(&registry, server_id);
                }
                1 => {
                    // Closed while an accept is parked: the marker's one use.
                    let parked = tokio::spawn(tcp_accept(registry.clone(), server_id, ids.clone()));
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    tcp_server_close(&registry, server_id);
                    assert!(matches!(parked.await.unwrap(), OpOutcome::Done));
                }
                _ => {
                    // An accept completes (its Notify stays registered), the
                    // stream is closed, then the server.
                    let client = tokio::net::TcpStream::connect(("127.0.0.1", port))
                        .await
                        .unwrap();
                    let OpOutcome::Json(accepted) =
                        tcp_accept(registry.clone(), server_id, ids.clone()).await
                    else {
                        panic!("accept failed");
                    };
                    let accepted: serde_json::Value = serde_json::from_str(&accepted).unwrap();
                    assert_eq!(
                        accepted["localAddr"]["port"].as_u64(),
                        Some(u64::from(port)),
                        "the accept payload carries the accepted socket's own address"
                    );
                    tcp_close(&registry, accepted["handle"].as_u64().unwrap());
                    drop(client);
                    tcp_server_close(&registry, server_id);
                }
            }
            // An accept after the close must neither resurrect the server
            // nor register a cancel Notify for it.
            assert!(matches!(
                tcp_accept(registry.clone(), server_id, ids.clone()).await,
                OpOutcome::Done
            ));
        }

        let bookkeeping = registry.lock().unwrap().bookkeeping();
        assert_eq!(
            bookkeeping,
            (0, 0, 0, 0, 0),
            "(closed, cancel, in_flight, readers, writers)"
        );
    }

    /// #139, the duplex case the single-half tests miss: a handle closed
    /// while BOTH a read and a write are in flight (in_flight == 2) keeps
    /// its closed marker until the SECOND half returns, not the first. This
    /// is a net.Socket writing a request body while reading the response,
    /// destroyed mid-flight: the read is woken by the close and returns
    /// first, the write drains later. If the marker cleared on the first
    /// half back, the second would see no marker, reinsert its half after
    /// close, and resurrect the handle -- the surviving half (and the TCP
    /// connection, and the event loop) leaking at exit, the exact #139 hang.
    /// Driven through the real take/close/reinsert/release primitives the
    /// ops use (each op holds a half out of the maps behind an `InFlight`
    /// guard); the awaits between are irrelevant to the bookkeeping.
    #[tokio::test]
    async fn a_close_with_both_halves_in_flight_clears_only_on_the_last() {
        let registry: TcpRegistry = std::sync::Arc::new(std::sync::Mutex::new(TcpState::default()));
        let ids = std::sync::Arc::new(AtomicU64::new(1));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(echo_server(listener, 1));

        let OpOutcome::Json(payload) = tcp_connect(
            registry.clone(),
            ids.clone(),
            "127.0.0.1".into(),
            port,
            crate::net_connect::DEFAULT_ATTEMPT_TIMEOUT,
        )
        .await
        else {
            panic!("connect failed");
        };
        let info: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let handle = info["handle"].as_u64().unwrap();

        // A read op and a write op each check out their half (in_flight ==
        // 2), each behind an `InFlight` guard, exactly as tcp_read/tcp_write.
        let reader = registry.lock().unwrap().take_reader(handle).unwrap();
        let g_read = InFlight {
            registry: registry.clone(),
            handle,
        };
        let writer = registry.lock().unwrap().take_writer(handle).unwrap();
        let g_write = InFlight {
            registry: registry.clone(),
            handle,
        };
        assert_eq!(
            registry.lock().unwrap().in_flight.get(&handle).copied(),
            Some(2),
            "both halves in flight for this handle"
        );

        // Close: the marker is set because a half is still out.
        tcp_close(&registry, handle);
        assert_eq!(
            registry.lock().unwrap().bookkeeping().0,
            1,
            "marker set while a half is in flight"
        );

        // The read op returns first (its select woken by the close): the
        // reinsert drops the half because the handle is closed, and the
        // guard's drop releases -> in_flight 2->1, marker MUST stay.
        assert!(
            !reinsert_reader(&registry, handle, reader),
            "read half dropped, not reinserted"
        );
        drop(g_read);
        let bk = registry.lock().unwrap().bookkeeping();
        assert_eq!(bk.0, 1, "marker must survive the first half back");
        assert_eq!(bk.2, 1, "one half still in flight");

        // The write op returns: the last half back clears the marker.
        assert!(
            !reinsert_writer(&registry, handle, writer),
            "write half dropped, not reinserted"
        );
        drop(g_write);
        assert_eq!(
            registry.lock().unwrap().bookkeeping(),
            (0, 0, 0, 0, 0),
            "(closed, cancel, in_flight, readers, writers)"
        );
    }

    /// A blocking peer on its own thread: accepts one connection, waits
    /// `before_reading`, reads to EOF and reports how many bytes came and
    /// whether the stream then ended cleanly. It never writes and closes only
    /// after the EOF, so an EOF it sees is the FIN of a shutdown, not of a
    /// close.
    fn reading_peer(
        before_reading: std::time::Duration,
    ) -> (u16, std::sync::mpsc::Receiver<std::io::Result<usize>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            std::thread::sleep(before_reading);
            let mut all = Vec::new();
            let result = std::io::Read::read_to_end(&mut stream, &mut all).map(|_| all.len());
            let _ = tx.send(result);
        });
        (port, rx)
    }

    async fn connected(registry: &TcpRegistry, ids: &std::sync::Arc<AtomicU64>, port: u16) -> u64 {
        let OpOutcome::Json(payload) = tcp_connect(
            registry.clone(),
            ids.clone(),
            "127.0.0.1".into(),
            port,
            crate::net_connect::DEFAULT_ATTEMPT_TIMEOUT,
        )
        .await
        else {
            panic!("connect failed");
        };
        let info: serde_json::Value = serde_json::from_str(&payload).unwrap();
        info["handle"].as_u64().unwrap()
    }

    /// Writes to `handle` until one does not fit the socket and is queued:
    /// from here on a write is certainly in flight (the peer is not reading
    /// yet). How much a socket takes before that is the platform's business
    /// -- Windows accepts a whole buffer of any size once, Linux fills its
    /// send buffer -- so the writes are issued until the queue shows one.
    /// Returns the writes' futures, not yet polled, and the bytes issued.
    fn write_until_queued(
        registry: &TcpRegistry,
        handle: u64,
    ) -> (
        Vec<impl std::future::Future<Output = OpOutcome> + Send + 'static>,
        usize,
    ) {
        const CHUNK: usize = 4 * 1024 * 1024;
        let mut writes = Vec::new();
        while registry.lock().unwrap().queued_writes() == 0 {
            assert!(
                writes.len() < 256,
                "the socket took 1 GiB without queueing a write"
            );
            writes.push(tcp_write(registry.clone(), handle, vec![7u8; CHUNK]));
        }
        let issued = writes.len() * CHUNK;
        (writes, issued)
    }

    /// #156: a shutdown issued while a write is still in flight sends the
    /// data and then the FIN. Before the write queue it found the write half
    /// checked out, returned at once and sent nothing: the peer saw no EOF
    /// until the handle was closed.
    ///
    /// The shutdown is spawned FIRST, to pin that the order is the order of
    /// the calls, not of the polls.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_shutdown_issued_behind_a_write_in_flight_sends_the_data_then_the_fin() {
        let registry: TcpRegistry = std::sync::Arc::new(std::sync::Mutex::new(TcpState::default()));
        let ids = std::sync::Arc::new(AtomicU64::new(1));
        let (port, peer) = reading_peer(std::time::Duration::from_millis(200));
        let handle = connected(&registry, &ids, port).await;

        let (writes, issued) = write_until_queued(&registry, handle);
        let shutdown = tokio::spawn(tcp_shutdown(registry.clone(), handle));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        for write in writes {
            assert!(matches!(
                tokio::spawn(write).await.unwrap(),
                OpOutcome::Done
            ));
        }
        assert!(matches!(shutdown.await.unwrap(), OpOutcome::Done));
        // The handle is still open: the EOF below is the shutdown's FIN.
        let received = tokio::task::spawn_blocking(move || {
            peer.recv_timeout(std::time::Duration::from_secs(30))
        })
        .await
        .unwrap()
        .expect("the peer saw no EOF: the FIN was never sent")
        .expect("the peer's read failed");
        assert_eq!(
            received, issued,
            "every byte written arrives before the FIN"
        );

        // A write issued after the shutdown has no half to write to.
        assert!(matches!(
            tcp_write(registry.clone(), handle, b"late".to_vec()).await,
            OpOutcome::Failed(_)
        ));
        assert_eq!(registry.lock().unwrap().queued_writes(), 0);
        tcp_close(&registry, handle);
        assert_eq!(
            registry.lock().unwrap().bookkeeping(),
            (0, 0, 0, 0, 0),
            "(closed, cancel, in_flight, readers, writers)"
        );
    }

    /// #156, the common shape (`socket.write(response); socket.end()`): a
    /// write the socket has room for and the shutdown behind it are both
    /// done when the calls that issue them return -- the futures are not
    /// polled until the peer has read the data and seen the FIN. That is
    /// what lets JS ask for the FIN in the same turn as the write.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_write_that_fits_and_its_shutdown_are_on_the_wire_when_issued() {
        let registry: TcpRegistry = std::sync::Arc::new(std::sync::Mutex::new(TcpState::default()));
        let ids = std::sync::Arc::new(AtomicU64::new(1));
        let (port, peer) = reading_peer(std::time::Duration::ZERO);
        let handle = connected(&registry, &ids, port).await;

        let write = tcp_write(registry.clone(), handle, b"response".to_vec());
        let shutdown = tcp_shutdown(registry.clone(), handle);
        assert_eq!(
            registry.lock().unwrap().queued_writes(),
            0,
            "a connected socket takes a small write at once: nothing is queued"
        );
        let received = tokio::task::spawn_blocking(move || {
            peer.recv_timeout(std::time::Duration::from_secs(30))
        })
        .await
        .unwrap()
        .expect("the peer saw no EOF before the futures were polled")
        .expect("the peer's read failed");
        assert_eq!(received, b"response".len());

        assert!(matches!(write.await, OpOutcome::Done));
        assert!(matches!(shutdown.await, OpOutcome::Done));
        tcp_close(&registry, handle);
        assert_eq!(
            registry.lock().unwrap().bookkeeping(),
            (0, 0, 0, 0, 0),
            "(closed, cancel, in_flight, readers, writers)"
        );
    }

    /// A handle closed while writes and a shutdown are queued leaves nothing
    /// behind: the write in flight ends (the close marker stops its half
    /// going back), the ops behind it find no half, and the queue's tail
    /// entry goes with the last turn.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_close_with_writes_queued_leaves_no_bookkeeping_behind() {
        let registry: TcpRegistry = std::sync::Arc::new(std::sync::Mutex::new(TcpState::default()));
        let ids = std::sync::Arc::new(AtomicU64::new(1));
        // A peer that reads only once the handle has been closed.
        let (port, peer) = reading_peer(std::time::Duration::from_millis(300));
        let handle = connected(&registry, &ids, port).await;

        let (writes, _) = write_until_queued(&registry, handle);
        let writes: Vec<_> = writes.into_iter().map(tokio::spawn).collect();
        let behind = tokio::spawn(tcp_write(registry.clone(), handle, b"behind".to_vec()));
        let shutdown = tokio::spawn(tcp_shutdown(registry.clone(), handle));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tcp_close(&registry, handle);

        // The queued write holds its half; once the peer drains (or resets)
        // it, it ends one way or the other. The ops behind it find the
        // handle gone.
        for write in writes {
            let _ = write.await.unwrap();
        }
        assert!(matches!(behind.await.unwrap(), OpOutcome::Failed(_)));
        assert!(matches!(shutdown.await.unwrap(), OpOutcome::Done));
        let _ = tokio::task::spawn_blocking(move || {
            peer.recv_timeout(std::time::Duration::from_secs(30))
        })
        .await;
        assert_eq!(registry.lock().unwrap().queued_writes(), 0);
        assert_eq!(
            registry.lock().unwrap().bookkeeping(),
            (0, 0, 0, 0, 0),
            "(closed, cancel, in_flight, readers, writers)"
        );
    }

    /// destroy() of a flowing socket (its read parked) whose peer is not
    /// draining what it was sent: the close ends every op at once, as
    /// libuv's uv_close does, long before the peer reads -- the descriptor
    /// goes with the last half -- and the peer then sees an orderly end, not
    /// a reset. Before, the parked read and the stuck write kept the socket
    /// open until the peer answered: forever, for a peer that never reads
    /// (a paused node socket), which kept the process alive with it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_close_wakes_the_parked_read_and_the_stuck_write() {
        let registry: TcpRegistry = std::sync::Arc::new(std::sync::Mutex::new(TcpState::default()));
        let ids = std::sync::Arc::new(AtomicU64::new(1));
        let (port, peer) = reading_peer(std::time::Duration::from_secs(3));
        let handle = connected(&registry, &ids, port).await;

        let parked = tokio::spawn(tcp_read(registry.clone(), handle, 64));
        let (writes, _) = write_until_queued(&registry, handle);
        let writes: Vec<_> = writes.into_iter().map(tokio::spawn).collect();
        let behind = tokio::spawn(tcp_write(registry.clone(), handle, b"behind".to_vec()));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            registry.lock().unwrap().in_flight.get(&handle).copied(),
            Some(2),
            "the read and a write are both out for an await"
        );

        tcp_close(&registry, handle);
        let prompt = std::time::Duration::from_secs(1);
        let read = tokio::time::timeout(prompt, parked)
            .await
            .expect("the parked read was not woken by the close")
            .unwrap();
        assert!(matches!(read, OpOutcome::Done));
        let mut outcomes = Vec::new();
        for write in writes {
            outcomes.push(
                tokio::time::timeout(prompt, write)
                    .await
                    .expect("the stuck write was not woken by the close")
                    .unwrap(),
            );
        }
        assert!(
            matches!(outcomes.last(), Some(OpOutcome::Failed(_))),
            "the stuck write fails as a write to a closed handle"
        );
        assert!(matches!(
            tokio::time::timeout(prompt, behind).await.unwrap().unwrap(),
            OpOutcome::Failed(_)
        ));
        assert_eq!(
            registry.lock().unwrap().bookkeeping(),
            (0, 0, 0, 0, 0),
            "(closed, cancel, in_flight, readers, writers)"
        );
        let seen = tokio::task::spawn_blocking(move || {
            peer.recv_timeout(std::time::Duration::from_secs(30))
        })
        .await
        .unwrap()
        .expect("the peer's read never ended");
        assert!(seen.is_ok(), "the peer saw a reset, not an end: {seen:?}");
        assert_eq!(registry.lock().unwrap().queued_writes(), 0);
    }

    /// What `reading_peer` saw: the connection reset (node's `read
    /// ECONNRESET`), not an end.
    async fn peer_saw_reset(peer: std::sync::mpsc::Receiver<std::io::Result<usize>>) {
        let seen = tokio::task::spawn_blocking(move || {
            peer.recv_timeout(std::time::Duration::from_secs(30))
        })
        .await
        .unwrap()
        .expect("the peer's read never ended");
        match seen {
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::ConnectionReset, "{e}"),
            Ok(n) => panic!("the peer saw an orderly end after {n} bytes, not a reset"),
        }
    }

    /// socket.resetAndDestroy() on an idle connected socket: the peer's read
    /// fails with ECONNRESET -- no FIN first -- and nothing is left behind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reset_closes_the_stream_with_an_rst() {
        let registry: TcpRegistry = std::sync::Arc::new(std::sync::Mutex::new(TcpState::default()));
        let ids = std::sync::Arc::new(AtomicU64::new(1));
        let (port, peer) = reading_peer(std::time::Duration::ZERO);
        let handle = connected(&registry, &ids, port).await;

        assert_eq!(tcp_reset(&registry, handle, false), Ok(()));
        peer_saw_reset(peer).await;
        assert!(matches!(
            tcp_write(registry.clone(), handle, b"late".to_vec()).await,
            OpOutcome::Failed(_)
        ));
        assert_eq!(
            registry.lock().unwrap().bookkeeping(),
            (0, 0, 0, 0, 0),
            "(closed, cancel, in_flight, readers, writers)"
        );
    }

    /// The shape resetAndDestroy() is for -- a flowing socket (its read
    /// parked) whose peer is not draining what it was sent (a write stuck,
    /// more queued behind it, and the FIN of an end()): every op ends at once,
    /// long before the peer reads, nothing behind the stuck write is sent,
    /// and the peer then sees the reset. Without the wake-up the reset waited
    /// for the peer: the parked read and the stuck write held the socket
    /// open, and the write half's drop sent a FIN.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reset_wakes_the_parked_read_and_the_stuck_write() {
        let registry: TcpRegistry = std::sync::Arc::new(std::sync::Mutex::new(TcpState::default()));
        let ids = std::sync::Arc::new(AtomicU64::new(1));
        let (port, peer) = reading_peer(std::time::Duration::from_secs(3));
        let handle = connected(&registry, &ids, port).await;

        let parked = tokio::spawn(tcp_read(registry.clone(), handle, 64));
        let (writes, _) = write_until_queued(&registry, handle);
        let writes: Vec<_> = writes.into_iter().map(tokio::spawn).collect();
        let behind = tokio::spawn(tcp_write(registry.clone(), handle, b"behind".to_vec()));
        let shutdown = tokio::spawn(tcp_shutdown(registry.clone(), handle));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            registry.lock().unwrap().in_flight.get(&handle).copied(),
            Some(2),
            "the read and a write are both out for an await"
        );

        assert_eq!(tcp_reset(&registry, handle, false), Ok(()));
        let prompt = std::time::Duration::from_secs(1);
        let read = tokio::time::timeout(prompt, parked)
            .await
            .expect("the parked read was not woken by the reset")
            .unwrap();
        assert!(matches!(read, OpOutcome::Done));
        for write in writes {
            // The last is the stuck one; those before it were taken whole.
            let _ = tokio::time::timeout(prompt, write)
                .await
                .expect("the stuck write was not woken by the reset")
                .unwrap();
        }
        assert!(matches!(
            tokio::time::timeout(prompt, behind).await.unwrap().unwrap(),
            OpOutcome::Failed(_)
        ));
        assert!(matches!(
            tokio::time::timeout(prompt, shutdown)
                .await
                .unwrap()
                .unwrap(),
            OpOutcome::Done
        ));
        peer_saw_reset(peer).await;
        assert_eq!(registry.lock().unwrap().queued_writes(), 0);
        assert_eq!(
            registry.lock().unwrap().bookkeeping(),
            (0, 0, 0, 0, 0),
            "(closed, cancel, in_flight, readers, writers)"
        );
    }

    /// libuv refuses `uv_tcp_close_reset` on a stream whose shutdown is out
    /// (EINVAL), and so does tcp_reset -- after closing the stream, which
    /// libuv leaves open. The peer sees the end the FIN began.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reset_after_the_fin_is_refused_and_closes_the_stream() {
        let registry: TcpRegistry = std::sync::Arc::new(std::sync::Mutex::new(TcpState::default()));
        let ids = std::sync::Arc::new(AtomicU64::new(1));
        let (port, peer) = reading_peer(std::time::Duration::ZERO);
        let handle = connected(&registry, &ids, port).await;

        assert!(matches!(
            tcp_write(registry.clone(), handle, b"bye".to_vec()).await,
            OpOutcome::Done
        ));
        assert!(matches!(
            tcp_shutdown(registry.clone(), handle).await,
            OpOutcome::Done
        ));
        assert_eq!(
            tcp_reset(&registry, handle, false),
            Err(ResetRefused("EINVAL"))
        );
        let received = tokio::task::spawn_blocking(move || {
            peer.recv_timeout(std::time::Duration::from_secs(30))
        })
        .await
        .unwrap()
        .expect("the peer's read never ended")
        .expect("the peer saw a reset after the FIN");
        assert_eq!(received, b"bye".len());
        assert_eq!(
            registry.lock().unwrap().bookkeeping(),
            (0, 0, 0, 0, 0),
            "(closed, cancel, in_flight, readers, writers)"
        );
    }

    /// Once JS has seen the shutdown finish (node's 'finish': libuv's
    /// shutdown callback has run) the reset goes ahead again, read half and
    /// parked read included.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reset_once_the_shutdown_finished_goes_ahead() {
        let registry: TcpRegistry = std::sync::Arc::new(std::sync::Mutex::new(TcpState::default()));
        let ids = std::sync::Arc::new(AtomicU64::new(1));
        let (port, _peer) = reading_peer(std::time::Duration::from_secs(3));
        let handle = connected(&registry, &ids, port).await;

        assert!(matches!(
            tcp_shutdown(registry.clone(), handle).await,
            OpOutcome::Done
        ));
        let parked = tokio::spawn(tcp_read(registry.clone(), handle, 64));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(tcp_reset(&registry, handle, true), Ok(()));
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(1), parked)
                .await
                .expect("the parked read was not woken by the reset")
                .unwrap(),
            OpOutcome::Done
        ));
        assert_eq!(
            registry.lock().unwrap().bookkeeping(),
            (0, 0, 0, 0, 0),
            "(closed, cancel, in_flight, readers, writers)"
        );
    }

    /// #172: a listen() without a host is node's dual-stack `::`, an explicit
    /// `::` too unless `ipv6Only`, and an address in use fails on `::` with
    /// node's listen error rather than falling back to IPv4.
    #[tokio::test]
    async fn a_listener_without_a_host_is_dual_stack_as_node_s() {
        let at = |host: Option<&str>, port: u16, ipv6_only: bool| ListenAt {
            host: host.map(str::to_string),
            port,
            ipv6_only,
        };
        // Through oam's connector: a refused loopback connect is prompt on
        // Windows there.
        let options = crate::net_connect::ConnectOptions::default();
        let reaches = async |port: u16, ip: &str| {
            crate::net_connect::connect(ip, port, &options)
                .await
                .is_ok()
        };
        for (host, ipv6_only, v4, v6) in [
            (None, false, true, true),
            (Some("::"), false, true, true),
            (None, true, false, true),
            (Some("::"), true, false, true),
            (Some("0.0.0.0"), false, true, false),
            (Some("127.0.0.1"), false, true, false),
        ] {
            let (listener, local) = bind_listener(&at(host, 0, ipv6_only)).await.unwrap();
            let expected = match host {
                None => "::",
                Some(h) => h,
            };
            assert_eq!(local.ip().to_string(), expected, "{host:?}");
            let port = local.port();
            let (r4, r6) = (reaches(port, "127.0.0.1").await, reaches(port, "::1").await);
            assert_eq!((r4, r6), (v4, v6), "{host:?} ipv6Only={ipv6_only}");
            drop(listener);
        }

        // Port in use: node's `listen EADDRINUSE: address already in use
        // :::PORT`, on `::` -- libuv reports it at listen, after the
        // address was chosen.
        let (held, local) = bind_listener(&at(None, 0, false)).await.unwrap();
        let Err(error) = bind_listener(&at(None, local.port(), false)).await else {
            panic!("a second listener on the same port must fail");
        };
        assert_eq!(error.code, "EADDRINUSE");
        assert_eq!(
            error.message,
            format!(
                "listen EADDRINUSE: address already in use :::{}",
                local.port()
            )
        );
        assert_eq!(error.syscall.as_deref(), Some("listen"));
        assert_eq!(error.address.as_deref(), Some("::"));
        assert_eq!(error.port, Some(local.port()));
        drop(held);
    }

    /// A pipe path for one test: a Windows named pipe, a Unix domain socket
    /// in the temp dir elsewhere.
    fn test_pipe_path(tag: &str) -> String {
        let leaf = format!("oam-core-{tag}-{}", std::process::id());
        if cfg!(windows) {
            format!(r"\\.\pipe\{leaf}")
        } else {
            std::env::temp_dir()
                .join(format!("{leaf}.sock"))
                .to_string_lossy()
                .into_owned()
        }
    }

    fn json(outcome: OpOutcome) -> serde_json::Value {
        match outcome {
            OpOutcome::Json(payload) => serde_json::from_str(&payload).unwrap(),
            _ => panic!("expected a Json outcome"),
        }
    }

    /// #219: a pipe server and client go through the very registry ops a
    /// TCP stream does -- listen, accept, connect, read, write, close -- the
    /// accept and connect payloads carry no addresses, a closed server's path
    /// dials ENOENT again (on Unix the socket file is unlinked), and nothing
    /// is left behind.
    #[tokio::test]
    async fn a_pipe_round_trips_through_the_registry_ops() {
        let registry: TcpRegistry = std::sync::Arc::new(std::sync::Mutex::new(TcpState::default()));
        let ids = std::sync::Arc::new(AtomicU64::new(1));
        let path = test_pipe_path("round-trip");

        let server =
            json(pipe_listen(registry.clone(), ids.clone(), path.clone(), path.clone()).await);
        let server_id = server["serverId"].as_u64().unwrap();
        assert_eq!(server.as_object().unwrap().len(), 1, "{server}");
        // Taken: node's EADDRINUSE, `listen` shaped.
        match pipe_listen(registry.clone(), ids.clone(), path.clone(), path.clone()).await {
            OpOutcome::NodeFailed {
                code,
                message,
                syscall,
                address,
                port,
                ..
            } => {
                assert_eq!(code, "EADDRINUSE");
                assert_eq!(
                    message,
                    format!("listen EADDRINUSE: address already in use {path}")
                );
                assert_eq!(syscall.as_deref(), Some("listen"));
                assert_eq!(address.as_deref(), Some(path.as_str()));
                assert_eq!(port, None);
            }
            _ => panic!("a second listen on the path must fail"),
        }

        let accepting = tokio::spawn(tcp_accept(registry.clone(), server_id, ids.clone()));
        let client =
            json(pipe_connect(registry.clone(), ids.clone(), path.clone(), path.clone()).await);
        let accepted = json(accepting.await.unwrap());
        assert_eq!(client.as_object().unwrap().len(), 1, "{client}");
        assert_eq!(accepted.as_object().unwrap().len(), 1, "{accepted}");
        let (c, s) = (
            client["handle"].as_u64().unwrap(),
            accepted["handle"].as_u64().unwrap(),
        );

        assert!(matches!(
            tcp_write(registry.clone(), c, b"ping".to_vec()).await,
            OpOutcome::Done
        ));
        assert!(matches!(
            tcp_read(registry.clone(), s, 64).await,
            OpOutcome::Bytes(b) if b == b"ping"
        ));
        assert!(matches!(
            tcp_write(registry.clone(), s, b"pong".to_vec()).await,
            OpOutcome::Done
        ));
        assert!(matches!(
            tcp_read(registry.clone(), c, 64).await,
            OpOutcome::Bytes(b) if b == b"pong"
        ));
        // The client goes: the server reads the end of the stream.
        tcp_close(&registry, c);
        assert!(matches!(
            tcp_read(registry.clone(), s, 64).await,
            OpOutcome::Done
        ));
        tcp_close(&registry, s);
        tcp_server_close(&registry, server_id);

        match pipe_connect(registry.clone(), ids.clone(), path.clone(), path.clone()).await {
            OpOutcome::NodeFailed {
                code,
                message,
                syscall,
                address,
                ..
            } => {
                assert_eq!(code, "ENOENT");
                assert_eq!(message, format!("connect ENOENT {path}"));
                assert_eq!(syscall.as_deref(), Some("connect"));
                assert_eq!(address.as_deref(), Some(path.as_str()));
            }
            _ => panic!("a closed server's path must not connect"),
        }
        let state = registry.lock().unwrap();
        assert_eq!(state.bookkeeping(), (0, 0, 0, 0, 0));
        assert!(state.listeners.is_empty());
    }

    /// #219: a named pipe has no half-close, so its shutdown is libuv's:
    /// it waits until the peer has read everything written (FlushFileBuffers)
    /// -- here a write far larger than the pipe's buffer, which the client
    /// does not read for a while -- and then ends the stream both ways: the
    /// side that shut down reads EOF, and once it closes, so does the peer.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_named_pipe_shutdown_waits_for_the_peer_then_ends_both_ways() {
        let registry: TcpRegistry = std::sync::Arc::new(std::sync::Mutex::new(TcpState::default()));
        let ids = std::sync::Arc::new(AtomicU64::new(1));
        let path = test_pipe_path("shutdown");
        let server_id = json(
            pipe_listen(registry.clone(), ids.clone(), path.clone(), path.clone()).await,
        )["serverId"]
            .as_u64()
            .unwrap();
        let accepting = tokio::spawn(tcp_accept(registry.clone(), server_id, ids.clone()));
        let c = json(pipe_connect(registry.clone(), ids.clone(), path.clone(), path.clone()).await)
            ["handle"]
            .as_u64()
            .unwrap();
        let s = json(accepting.await.unwrap())["handle"].as_u64().unwrap();

        const LEN: usize = 1024 * 1024;
        let writing = tokio::spawn(tcp_write(registry.clone(), s, vec![7u8; LEN]));
        let shutting = tokio::spawn(tcp_shutdown(registry.clone(), s));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !shutting.is_finished(),
            "the shutdown waits for the peer to read"
        );

        let mut read = 0;
        while read < LEN {
            match tcp_read(registry.clone(), c, 65536).await {
                OpOutcome::Bytes(b) => read += b.len(),
                _ => panic!("the client reads every byte before any end"),
            }
        }
        assert!(matches!(writing.await.unwrap(), OpOutcome::Done));
        let shut = tokio::time::timeout(std::time::Duration::from_secs(10), shutting).await;
        assert!(matches!(shut, Ok(Ok(OpOutcome::Done))));
        // Ended both ways: the side that shut down reads EOF at once.
        assert!(matches!(
            tcp_read(registry.clone(), s, 64).await,
            OpOutcome::Done
        ));
        tcp_close(&registry, s);
        assert!(matches!(
            tcp_read(registry.clone(), c, 64).await,
            OpOutcome::Done
        ));
        tcp_close(&registry, c);
        tcp_server_close(&registry, server_id);
        assert_eq!(registry.lock().unwrap().bookkeeping(), (0, 0, 0, 0, 0));
    }

    /// #219: a read parked on a named pipe when its own side shuts down ends
    /// like an EOF, and a close during a shutdown that is still waiting for
    /// the peer does not wait for it.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_named_pipe_shutdown_ends_a_parked_read_and_a_close_does_not_wait_for_it() {
        let registry: TcpRegistry = std::sync::Arc::new(std::sync::Mutex::new(TcpState::default()));
        let ids = std::sync::Arc::new(AtomicU64::new(1));
        let path = test_pipe_path("parked");
        let server_id = json(
            pipe_listen(registry.clone(), ids.clone(), path.clone(), path.clone()).await,
        )["serverId"]
            .as_u64()
            .unwrap();
        let accepting = tokio::spawn(tcp_accept(registry.clone(), server_id, ids.clone()));
        let c = json(pipe_connect(registry.clone(), ids.clone(), path.clone(), path.clone()).await)
            ["handle"]
            .as_u64()
            .unwrap();
        let s = json(accepting.await.unwrap())["handle"].as_u64().unwrap();

        // Nothing written: the shutdown has nothing to wait for, and the
        // server's parked read ends.
        let parked = tokio::spawn(tcp_read(registry.clone(), s, 64));
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(matches!(
            tcp_shutdown(registry.clone(), s).await,
            OpOutcome::Done
        ));
        let ended = tokio::time::timeout(std::time::Duration::from_secs(5), parked).await;
        assert!(matches!(ended, Ok(Ok(OpOutcome::Done))));
        tcp_close(&registry, s);

        // The client writes what the server never reads, shuts down, and is
        // closed while that shutdown still waits: the close is at once.
        let accepting = tokio::spawn(tcp_accept(registry.clone(), server_id, ids.clone()));
        let c2 = json(
            pipe_connect(registry.clone(), ids.clone(), path.clone(), path.clone()).await,
        )["handle"]
            .as_u64()
            .unwrap();
        let s2 = json(accepting.await.unwrap())["handle"].as_u64().unwrap();
        // More than the pipe's buffer and the read the server's end keeps
        // in flight on its own can hold.
        let writing = tokio::spawn(tcp_write(registry.clone(), c2, vec![1u8; 1024 * 1024]));
        let shutting = tokio::spawn(tcp_shutdown(registry.clone(), c2));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!shutting.is_finished(), "nobody has read the 1 MiB yet");
        tcp_close(&registry, c2);
        let closed = tokio::time::timeout(std::time::Duration::from_secs(5), shutting).await;
        assert!(matches!(closed, Ok(Ok(OpOutcome::Done))));
        let written = tokio::time::timeout(std::time::Duration::from_secs(5), writing).await;
        assert!(written.is_ok(), "the write settles too");

        tcp_close(&registry, c);
        tcp_close(&registry, s2);
        tcp_server_close(&registry, server_id);
        assert_eq!(registry.lock().unwrap().bookkeeping(), (0, 0, 0, 0, 0));
    }

    /// A named-pipe server (`tag`) and one connected pair, as (server id,
    /// client handle, server handle).
    #[cfg(windows)]
    async fn named_pipe_pair(
        registry: &TcpRegistry,
        ids: &std::sync::Arc<AtomicU64>,
        tag: &str,
    ) -> (u64, u64, u64) {
        let path = test_pipe_path(tag);
        let server_id = json(
            pipe_listen(registry.clone(), ids.clone(), path.clone(), path.clone()).await,
        )["serverId"]
            .as_u64()
            .unwrap();
        let accepting = tokio::spawn(tcp_accept(registry.clone(), server_id, ids.clone()));
        let c =
            json(pipe_connect(registry.clone(), ids.clone(), path.clone(), path).await)["handle"]
                .as_u64()
                .unwrap();
        let s = json(accepting.await.unwrap())["handle"].as_u64().unwrap();
        (server_id, c, s)
    }

    /// Regression guard: a listening named pipe kept ONE instance waiting,
    /// so a second client found the pipe busy until the server's next
    /// accept -- a burst of clients was admitted one per accept. libuv keeps
    /// four waiting; so does oam now, before any accept runs.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_listening_named_pipe_keeps_four_instances_waiting() {
        use tokio::net::windows::named_pipe::ClientOptions;
        let registry: TcpRegistry = std::sync::Arc::new(std::sync::Mutex::new(TcpState::default()));
        let ids = std::sync::Arc::new(AtomicU64::new(1));
        let path = test_pipe_path("four-waiting");
        let server_id = json(
            pipe_listen(registry.clone(), ids.clone(), path.clone(), path.clone()).await,
        )["serverId"]
            .as_u64()
            .unwrap();
        let clients: Vec<_> = (0..4)
            .map(|n| {
                ClientOptions::new()
                    .open(&path)
                    .unwrap_or_else(|e| panic!("client {n} finds a waiting instance: {e}"))
            })
            .collect();
        let busy = ClientOptions::new().open(&path).unwrap_err();
        assert_eq!(
            busy.raw_os_error(),
            Some(windows_sys::Win32::Foundation::ERROR_PIPE_BUSY as i32),
            "four waiting, all four taken"
        );
        // An accept hands one out and puts a fresh instance in its place.
        let accepted = json(tcp_accept(registry.clone(), server_id, ids.clone()).await);
        let fifth = ClientOptions::new().open(&path);
        assert!(fifth.is_ok(), "the accepted instance is replaced");
        drop((clients, fifth));
        tcp_close(&registry, accepted["handle"].as_u64().unwrap());
        tcp_server_close(&registry, server_id);
    }

    /// Regression guard: a dial that found every instance taken slept in a
    /// back-off up to 50 ms (each sleep ~15 ms or more on Windows) instead of
    /// waiting for an instance with WaitNamedPipeW, and with one waiting
    /// instance 200 clients at once took 3-6 s to connect, where node takes
    /// tens of ms. The bound is generous; the fixed code takes ~50 ms here.
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_burst_of_named_pipe_clients_all_connect_promptly() {
        const CLIENTS: usize = 200;
        let registry: TcpRegistry = std::sync::Arc::new(std::sync::Mutex::new(TcpState::default()));
        let ids = std::sync::Arc::new(AtomicU64::new(1));
        let path = test_pipe_path("burst");
        let server_id = json(
            pipe_listen(registry.clone(), ids.clone(), path.clone(), path.clone()).await,
        )["serverId"]
            .as_u64()
            .unwrap();
        let accepting = tokio::spawn({
            let (registry, ids) = (registry.clone(), ids.clone());
            async move {
                let mut handles = Vec::new();
                for _ in 0..CLIENTS {
                    let accepted = json(tcp_accept(registry.clone(), server_id, ids.clone()).await);
                    handles.push(accepted["handle"].as_u64().unwrap());
                }
                handles
            }
        });
        let started = std::time::Instant::now();
        let dials: Vec<_> = (0..CLIENTS)
            .map(|_| {
                tokio::spawn(pipe_connect(
                    registry.clone(),
                    ids.clone(),
                    path.clone(),
                    path.clone(),
                ))
            })
            .collect();
        let mut clients = Vec::new();
        for dial in dials {
            clients.push(json(dial.await.unwrap())["handle"].as_u64().unwrap());
        }
        let took = started.elapsed();
        let servers = accepting.await.unwrap();
        assert!(
            took < std::time::Duration::from_secs(2),
            "{CLIENTS} clients took {took:?} to connect"
        );
        for handle in clients.into_iter().chain(servers) {
            tcp_close(&registry, handle);
        }
        tcp_server_close(&registry, server_id);
        assert_eq!(registry.lock().unwrap().bookkeeping(), (0, 0, 0, 0, 0));
    }

    /// Regression guard: tokio's named pipe takes a write whole into a
    /// buffer of its own and reports it written while its WriteFile still
    /// waits for room, so a 1 MiB write to a peer that reads nothing was
    /// done at once -- its callback ran where node's never does, and
    /// `destroy()` could not keep any of it from the peer. Handed over in
    /// pipe-buffer chunks, it is done only once the peer has made room for
    /// all but its last chunk; a write that fits is still done in the call.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_named_pipe_write_is_done_only_once_the_pipe_has_taken_it() {
        let registry: TcpRegistry = std::sync::Arc::new(std::sync::Mutex::new(TcpState::default()));
        let ids = std::sync::Arc::new(AtomicU64::new(1));
        let (server_id, c, s) = named_pipe_pair(&registry, &ids, "write-done").await;

        assert!(
            matches!(
                tcp_write_start(registry.clone(), c, vec![1u8; 100]),
                Started::Done(OpOutcome::Done)
            ),
            "a write the pipe has room for is done in the call"
        );
        const LEN: usize = 1024 * 1024;
        let writing = match tcp_write_start(registry.clone(), c, vec![2u8; LEN]) {
            Started::Pending(rest) => tokio::spawn(rest),
            Started::Done(_) => panic!("1 MiB the peer has not read is not done in the call"),
        };
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !writing.is_finished(),
            "nor is it done while the peer reads nothing"
        );
        let mut read = 0;
        while read < LEN + 100 {
            match tcp_read(registry.clone(), s, 65536).await {
                OpOutcome::Bytes(b) => read += b.len(),
                _ => panic!("every byte arrives"),
            }
        }
        let wrote = tokio::time::timeout(std::time::Duration::from_secs(10), writing).await;
        assert!(matches!(wrote, Ok(Ok(OpOutcome::Done))));
        tcp_close(&registry, c);
        tcp_close(&registry, s);
        tcp_server_close(&registry, server_id);
        assert_eq!(registry.lock().unwrap().bookkeeping(), (0, 0, 0, 0, 0));
    }

    /// Regression guard: a named pipe's shutdown flushes on a thread of its
    /// own through a duplicated handle, and a close only abandoned the wait
    /// for it, so with a peer that never reads, the thread and the handle --
    /// which holds the pipe open -- outlived the close until the peer read
    /// or went: one thread per such socket. The close cancels the flush now,
    /// so the pipe closes and the peer's next write fails.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_close_cancels_the_shutdown_flush_a_peer_never_reads() {
        let registry: TcpRegistry = std::sync::Arc::new(std::sync::Mutex::new(TcpState::default()));
        let ids = std::sync::Arc::new(AtomicU64::new(1));
        let (server_id, c, s) = named_pipe_pair(&registry, &ids, "flush-cancel").await;

        assert!(matches!(
            tcp_write_start(registry.clone(), c, vec![1u8; 32 * 1024]),
            Started::Done(OpOutcome::Done)
        ));
        let shutting = tokio::spawn(tcp_shutdown(registry.clone(), c));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!shutting.is_finished(), "the flush waits for the peer");
        tcp_close(&registry, c);
        let closed = tokio::time::timeout(std::time::Duration::from_secs(5), shutting).await;
        assert!(matches!(closed, Ok(Ok(OpOutcome::Done))));

        // The client's pipe is closed for good: the server's write fails.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while let OpOutcome::Done = tcp_write(registry.clone(), s, b"x".to_vec()).await {
            assert!(
                std::time::Instant::now() < deadline,
                "the client end stays open after its close"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        tcp_close(&registry, s);
        tcp_server_close(&registry, server_id);
        assert_eq!(registry.lock().unwrap().bookkeeping(), (0, 0, 0, 0, 0));
    }
}
