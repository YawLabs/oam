//! The connector oam's pool dials through: node's connect algorithm
//! (`net_connect`), then TLS, with the environment proxy and the addresses
//! a `connect.lookup` hook answered for the connection decided here.
//!
//! What reqwest's connector did, and this one keeps (reqwest 0.13.4
//! connect.rs, hyper-util 0.1.20 connect/http.rs):
//!
//! - TCP_NODELAY on, keepalive after 15 s idle probing every 15 s (3 probes
//!   where the platform lets the count be set), and on Linux a 30 s
//!   TCP_USER_TIMEOUT -- all best-effort, none of them fails a connect;
//! - the TLS server name checked before anything dials, so a host rustls
//!   cannot name fails without a connection attempt;
//! - an https destination behind an http(s) proxy goes through a CONNECT
//!   tunnel carrying the proxy credentials and oam's user-agent, and the
//!   request's ALPN offer ([`Alpn`]) still goes to the origin inside it; an
//!   http destination is sent to the proxy in absolute form;
//! - a socks proxy is refused (reqwest had no socks support compiled in).
//!
//! What changed: every dial, proxy dials included, is node's algorithm, so a
//! refused or unresolvable host fails with node's error -- the resolved
//! address, one error per attempted address, the resolver's code -- which the
//! send path finds by walking the error's `source()` chain. The
//! `ConnectError` is boxed exactly once on the way out; wrapping it in an
//! `io::Error` would hide it from that walk.

use std::collections::HashMap;
use std::future::Future;
use std::mem::MaybeUninit;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin as StdPin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use http::Uri;
use http::header::{HeaderMap, HeaderValue, USER_AGENT};
use hyper_util::client::legacy::connect::proxy::Tunnel;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::client::proxy::matcher::{Intercept, Matcher};
use hyper_util::rt::TokioIo;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tower_service::Service;

use super::BoxError;
use super::prepare::host_for_connect;
use super::tls_config::{self, Alpn, TlsConfigs, TlsRange};
use crate::net_connect::{self, AttemptLog, ConnectOptions, Pin};
use std::sync::atomic::AtomicU8;

/// The byte stream under a connection: TCP, TLS over TCP, or TLS over a
/// tunnel.
pub(crate) trait AsyncIo: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> AsyncIo for T {}

/// What one connection has carried, shared by every copy of its `Connected`:
/// hyper-util clones that into each request that checks the connection out,
/// extras included, and `capture_connection` hands the copy back to the
/// sender.
///
/// - **Requests.** [`super::transport::HttpTransport::send`] counts a request
///   in once it is done with the connection, so a request that finds the
///   count above zero went out on a connection an earlier request had
///   already used -- a POOLED connection, the only kind a server can have
///   closed while it sat idle. hyper-util knows this too (`is_reused`) but
///   keeps it to itself unless the request never left.
/// - **Response bytes.** Every byte of HTTP read off the connection (above
///   TLS) is counted, and the copy a request gets records the count at the
///   moment it checked the connection out (see the `Clone` impl). An HTTP/1
///   connection carries one exchange at a time, so whatever was read after
///   that moment is part of the response to this request: the send path can
///   tell a connection that died before answering from one that died
///   halfway through a response head, which hyper reports with the same
///   `IncompleteMessage`.
#[derive(Debug)]
pub(crate) struct ConnStats {
    counters: Arc<ConnCounters>,
    /// The response-byte count when this copy was made from the pool's own
    /// copy; `None` on the pool's own copy.
    read_at_checkout: Option<u64>,
}

#[derive(Debug, Default)]
pub(crate) struct ConnCounters {
    uses: AtomicU64,
    read: AtomicU64,
    /// Every byte of HTTP written onto the connection (above TLS), for the
    /// `bytesWritten` of the socket facts a failure reports
    /// ([`ConnInfo::socket_facts`]).
    written: AtomicU64,
}

impl ConnStats {
    fn new() -> ConnStats {
        ConnStats {
            counters: Arc::new(ConnCounters::default()),
            read_at_checkout: None,
        }
    }

    /// The copy the pool keeps (the one `Connection::connected` hands
    /// hyper-util): no checkout count, so every copy made from it records
    /// one.
    fn clone_for_pool(&self) -> ConnStats {
        ConnStats {
            counters: self.counters.clone(),
            read_at_checkout: None,
        }
    }

    /// Counts one more request on this connection: true when an earlier
    /// request had already used it.
    pub(crate) fn count_one(&self) -> bool {
        self.counters.uses.fetch_add(1, Ordering::Relaxed) > 0
    }

    /// Some part of a response arrived on this connection after the request
    /// holding this copy checked it out. Also true when the copy carries no
    /// checkout count, so an unknown answer never licenses a resend.
    pub(crate) fn response_started(&self) -> bool {
        self.read_at_checkout
            .is_none_or(|at| self.counters.read.load(Ordering::Relaxed) > at)
    }
}

/// A copy made from the pool's own copy records the response-byte count at
/// that moment; a copy of a copy keeps the count it was made with.
///
/// This is when hyper-util makes the copies (0.1.20, client.rs
/// `try_send_request`): right after checkout it clones the pool entry's
/// `Connected` into the request's `capture_connection` slot, and only then
/// hands the request to the connection. So the count in the captured copy is
/// the count before a byte of this request's response could have been read,
/// and reading the extras back out of that copy (`get_extras`, a clone of a
/// copy) keeps it. An h2 connection's pool entries are clones too, so their
/// counts are stale -- harmless, as the resend these counts gate is for
/// HTTP/1's `IncompleteMessage` alone.
impl Clone for ConnStats {
    fn clone(&self) -> ConnStats {
        ConnStats {
            counters: self.counters.clone(),
            read_at_checkout: Some(
                self.read_at_checkout
                    .unwrap_or_else(|| self.counters.read.load(Ordering::Relaxed)),
            ),
        }
    }
}

/// Where one connection goes and what it negotiated: the local and peer
/// address of the TCP stream the connector dialled and, for an https origin,
/// the TLS session. `http.request` reports them on `req.socket` /
/// `res.socket` as node reports the socket it dialled (the peer's IP, never
/// the host as written). hyper-util copies this extra into the extensions of
/// every response the connection carries -- pooled and h2 ones included --
/// and `send::respond` serialises it into the payload. Through an
/// environment proxy the peer is the proxy.
#[derive(Debug, Clone, Default)]
pub(crate) struct ConnInfo {
    pub(crate) local: Option<SocketAddr>,
    pub(crate) peer: Option<SocketAddr>,
    pub(crate) tls: Option<TlsInfo>,
    /// The [`ConnCloser`] id JS closes this connection by
    /// ([`close_connection`]): an HTTP/1 connection the transport dialled.
    /// None on an h2 connection, which many requests share at once.
    pub(crate) connection: Option<u64>,
    /// How many times the connection has been checked out, shared with its
    /// [`ConnCloser`] (None where `connection` is).
    leases: Option<Arc<AtomicU64>>,
    /// Which checkout the response holding this copy came on
    /// ([`ConnInfo::take_lease`]): a close JS asks for through that
    /// response's socket reaches the connection only while no later request
    /// has taken it.
    pub(crate) lease: Option<u64>,
    /// What the connection has read and written so far (its [`ConnStats`]'
    /// counters), set once it is handed to the pool.
    counters: Option<Arc<ConnCounters>>,
}

impl ConnInfo {
    fn of(tcp: &EagerTcp) -> ConnInfo {
        ConnInfo {
            local: tcp.stream.local_addr().ok(),
            peer: tcp.stream.peer_addr().ok(),
            tls: None,
            connection: Some(tcp.closer.id),
            leases: Some(tcp.closer.leases.clone()),
            lease: None,
            counters: None,
        }
    }

    /// One more checkout of the connection: this copy -- the one the
    /// request's response carries -- records which. One relaxed add per
    /// request; nothing on an h2 connection.
    pub(crate) fn take_lease(&mut self) {
        if let Some(leases) = &self.leases {
            self.lease = Some(leases.fetch_add(1, Ordering::Relaxed) + 1);
        }
    }

    /// The connection as undici describes the socket of a `SocketError`
    /// (`util.getSocketInfo`): its two ends and the bytes it has carried.
    /// Read when a request fails, never on the way to a response.
    pub(crate) fn socket_facts(&self) -> crate::SocketFacts {
        let counted = |pick: fn(&ConnCounters) -> &AtomicU64| {
            self.counters
                .as_ref()
                .map(|counters| pick(counters).load(Ordering::Relaxed))
        };
        crate::SocketFacts {
            local_address: self.local.as_ref().map(crate::http_server::node_ip_string),
            local_port: self.local.map(|local| local.port()),
            remote_address: self.peer.as_ref().map(crate::http_server::node_ip_string),
            remote_port: self.peer.map(|peer| peer.port()),
            remote_family: self
                .peer
                .map(|peer| if peer.is_ipv6() { "IPv6" } else { "IPv4" }.to_string()),
            bytes_written: counted(|counters| &counters.written),
            bytes_read: counted(|counters| &counters.read),
        }
    }

    /// The same endpoints, with the origin's TLS session.
    fn with_tls(&self, conn: &rustls::ClientConnection) -> ConnInfo {
        ConnInfo {
            tls: Some(TlsInfo::of(conn)),
            ..self.clone()
        }
    }
}

/// An origin TLS session's facts, in the spelling `tls.connect` reports them
/// (`crate::tls`): what `res.socket.getProtocol()`, `getCipher()`,
/// `alpnProtocol` and `getPeerCertificate()` answer.
#[derive(Debug, Clone)]
pub(crate) struct TlsInfo {
    pub(crate) protocol: String,
    pub(crate) cipher: String,
    pub(crate) cipher_standard_name: String,
    pub(crate) alpn: Option<String>,
    /// The peer's chain as base64 DER, leaf first.
    pub(crate) peer_certificates: Option<Vec<String>>,
}

impl TlsInfo {
    fn of(conn: &rustls::ClientConnection) -> TlsInfo {
        let (cipher, cipher_standard_name) = conn
            .negotiated_cipher_suite()
            .map(|c| crate::tls::cipher_names(c.suite()))
            .unwrap_or_default();
        TlsInfo {
            protocol: conn
                .protocol_version()
                .map(crate::tls::protocol_name)
                .unwrap_or_default(),
            cipher,
            cipher_standard_name,
            alpn: conn
                .alpn_protocol()
                .map(|p| String::from_utf8_lossy(p).into_owned()),
            peer_certificates: crate::tls::peer_certificates_b64(conn.peer_certificates()),
        }
    }
}

/// A connection handed to hyper-util. `Connected` is not publicly `Clone`, so
/// the facts it carries are kept here and a fresh one is built on every
/// call.
pub(crate) struct OamConn {
    io: TokioIo<Counted>,
    /// ALPN selected h2.
    h2: bool,
    /// An http request through a proxy: hyper writes the absolute form.
    proxied: bool,
    /// The requests and response bytes this connection has carried.
    stats: ConnStats,
    /// Where it goes and what it negotiated.
    info: ConnInfo,
}

impl OamConn {
    fn new(io: Box<dyn AsyncIo>, h2: bool, proxied: bool, mut info: ConnInfo) -> OamConn {
        let stats = ConnStats::new();
        // An h2 connection carries every request to its origin at once: one
        // request's socket closing it would end all the others.
        if h2 {
            info.connection = None;
            info.leases = None;
        }
        info.counters = Some(stats.counters.clone());
        OamConn {
            io: TokioIo::new(Counted {
                io,
                counters: stats.counters.clone(),
            }),
            h2,
            proxied,
            stats,
            info,
        }
    }

    /// ALPN negotiated HTTP/2 (the owned pool picks the h1 or h2 dispatcher
    /// from this, where hyper-util read the `negotiated_h2()` marker).
    pub(crate) fn negotiated_h2(&self) -> bool {
        self.h2
    }

    /// The connection reaches its origin through an http proxy, so an http
    /// request on it is written in absolute form (`absolute_form`).
    pub(crate) fn is_proxied(&self) -> bool {
        self.proxied
    }

    /// The pool's own [`ConnStats`] copy (shared counters, no checkout
    /// baseline): the pool clones THIS at each checkout to snapshot the
    /// response-byte count, exactly as hyper-util's captured copy did.
    pub(crate) fn pool_stats(&self) -> ConnStats {
        self.stats.clone_for_pool()
    }

    /// The connection's socket/TLS facts, re-attached to every response the
    /// pool returns (`response.extensions_mut().insert(..)`).
    pub(crate) fn conn_info(&self) -> ConnInfo {
        self.info.clone()
    }
}

/// The byte stream under a connection, counting what is read from it and
/// written onto it into the connection's [`ConnStats`]. It sits above TLS,
/// so the count is HTTP bytes only: a TLS close_notify or session ticket is
/// not a response.
struct Counted {
    io: Box<dyn AsyncIo>,
    counters: Arc<ConnCounters>,
}

impl Counted {
    fn count_written(&self, polled: &Poll<std::io::Result<usize>>) {
        if let Poll::Ready(Ok(written)) = polled {
            self.counters
                .written
                .fetch_add(*written as u64, Ordering::Relaxed);
        }
    }
}

impl AsyncRead for Counted {
    fn poll_read(
        self: StdPin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let polled = StdPin::new(&mut this.io).poll_read(cx, buf);
        let read = buf.filled().len().saturating_sub(before);
        if read > 0 {
            this.counters.read.fetch_add(read as u64, Ordering::Relaxed);
        }
        polled
    }
}

impl AsyncWrite for Counted {
    fn poll_write(
        self: StdPin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let polled = StdPin::new(&mut this.io).poll_write(cx, buf);
        this.count_written(&polled);
        polled
    }

    fn poll_flush(self: StdPin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        StdPin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(self: StdPin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        StdPin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_write_vectored(
        self: StdPin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let polled = StdPin::new(&mut this.io).poll_write_vectored(cx, bufs);
        this.count_written(&polled);
        polled
    }
}

impl Connection for OamConn {
    fn connected(&self) -> Connected {
        let connected = Connected::new()
            .proxy(self.proxied)
            .extra(self.stats.clone_for_pool())
            .extra(self.info.clone());
        if self.h2 {
            connected.negotiated_h2()
        } else {
            connected
        }
    }
}

impl hyper::rt::Read for OamConn {
    fn poll_read(
        self: StdPin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        StdPin::new(&mut self.get_mut().io).poll_read(cx, buf)
    }
}

impl hyper::rt::Write for OamConn {
    fn poll_write(
        self: StdPin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        StdPin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }

    fn poll_flush(
        self: StdPin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        StdPin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(
        self: StdPin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        StdPin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_write_vectored(
        self: StdPin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        StdPin::new(&mut self.get_mut().io).poll_write_vectored(cx, bufs)
    }
}

/// The platform TLS configs could not be built. The send path reports it as
/// `tls configuration error: ...` instead of the generic send failure.
#[derive(Debug)]
pub(crate) struct TlsSetupError(pub(crate) String);

impl std::fmt::Display for TlsSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "tls configuration error: {}", self.0)
    }
}

impl std::error::Error for TlsSetupError {}

/// The request's TLS version range -- node's live defaults, resolved by JS
/// -- has nothing rustls can offer: node's `ERR_SSL_NO_PROTOCOLS_AVAILABLE`,
/// with the message tls.connect reports, which the send path maps
/// (`SendError::to_outcome`).
#[derive(Debug)]
pub(crate) struct NoProtocolsAvailable;

impl std::fmt::Display for NoProtocolsAvailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no protocols available for the requested TLS version range")
    }
}

impl std::error::Error for NoProtocolsAvailable {}

/// The TLS handshake with the server failed: rustls's error, marked as the
/// handshake's. The request's outcome tells a fatal alert answering the
/// handshake -- node: `write EPROTO`, the request head being a write queued
/// behind it -- from one sent after it (node: the alert's own code), and
/// the transport closing before the handshake was done from a reset after
/// it (#196).
#[derive(Debug)]
pub(crate) struct HandshakeFailed(pub(crate) std::io::Error);

impl std::fmt::Display for HandshakeFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "handshake failed: {}", self.0)
    }
}

impl std::error::Error for HandshakeFailed {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// Where a transport's TLS configs come from.
#[derive(Clone)]
pub enum TlsSource {
    /// The platform verifier plus NODE_EXTRA_CA_CERTS (with, on macOS, a
    /// second verdict on a chain the bundle anchors), built on first use (see
    /// [`tls_config::platform`]).
    Platform,
    /// Fixed configs (tests: a private root instead of the system store).
    Fixed(TlsConfigs),
    /// Every https connect fails with this configuration error.
    Unavailable(String),
}

/// What every connector of one transport shares.
pub(crate) struct Shared {
    pub(crate) tls: TlsSource,
    /// The environment proxy rules. `Matcher` is not `Clone`; it lives here,
    /// behind the transport's `Arc`.
    pub(crate) proxy: Option<Matcher>,
    pub(crate) user_agent: HeaderValue,
    /// The TLS version range of the request being sent (`TlsRange::code`):
    /// node's live `tls.DEFAULT_MIN_VERSION` / `DEFAULT_MAX_VERSION`, which
    /// every send stores before its request goes out and the connector reads
    /// at the handshake. Two concurrent requests
    /// can only disagree while a default is being reassigned, a value that is
    /// process-wide in node too; a pooled connection made under an earlier
    /// value is reused, as node's undici reuses its own.
    pub(crate) tls_range: AtomicU8,
}

impl Shared {
    pub(crate) fn tls_range(&self) -> TlsRange {
        TlsRange::from_code(self.tls_range.load(Ordering::Relaxed))
    }

    pub(crate) fn set_tls_range(&self, range: TlsRange) {
        self.tls_range.store(range.code(), Ordering::Relaxed);
    }

    /// The config offering `alpn` -- the request's offer for an origin
    /// handshake, [`Alpn::None`] for the handshake with an https proxy -- in
    /// the range the request being sent asked for (`set_tls_range`).
    async fn tls(&self, alpn: Alpn) -> Result<Arc<ClientConfig>, BoxError> {
        let configs = match &self.tls {
            TlsSource::Platform => {
                let range = self.tls_range();
                if range == TlsRange::None {
                    return Err(Box::new(NoProtocolsAvailable));
                }
                tls_config::platform(range)
                    .await
                    .map_err(|e| Box::new(TlsSetupError(e)) as BoxError)?
            }
            // Tests' prebuilt configs: their versions are their own, the
            // request's range is not applied.
            TlsSource::Fixed(configs) => configs.clone(),
            TlsSource::Unavailable(message) => {
                return Err(Box::new(TlsSetupError(message.clone())));
            }
        };
        Ok(configs.offering(alpn).clone())
    }
}

/// One `connect.lookup` answer, for the one connection it was asked for: the
/// addresses the hook returned for `key`'s authority, in the hook's order.
/// undici calls the hook once per connection it opens, so an answer opens
/// exactly one connection and is gone: the next connection to the same
/// authority asks the hook again.
#[derive(Debug, Clone)]
pub(crate) struct HookPin {
    /// The [`authority_key`] the hook was asked about. A pin opens a
    /// connection to that authority and no other.
    pub(crate) key: String,
    pub(crate) addrs: Vec<net_connect::PinAddr>,
    /// Whether the hook has failed the connection since it answered
    /// (`None` for an answer no later callback can reach).
    pub(crate) gate: Option<Arc<LookupGate>>,
}

/// The connection one hook answer opens, as the hook's later callbacks see
/// it (#169). node's `lookupAndConnectMultiple` (lib/net.js) acts on every
/// callback the hook makes while the socket is still connecting: an error
/// -- or an answer node's address rules refuse, or a throw -- fails the
/// connect, as the first answer would have. Once the socket has connected
/// it ignores them. The gate is that window: open from the answer until the
/// dial's TCP connect completes (or fails on its own), when it settles; a
/// [`LookupGate::fail`] while it is open abandons the dial, which fails with
/// [`LookupFailed`].
///
/// Gates are registered by the lookup's continuation token, so JS can fail
/// one by token (`fetchLookupFail`); the registry holds them weakly, and a
/// gate leaves it when its pin is gone.
#[derive(Debug)]
pub(crate) struct LookupGate {
    state: std::sync::atomic::AtomicU8,
    failed: tokio::sync::Notify,
    token: u64,
    registry: std::sync::Weak<Mutex<LookupGates>>,
}

/// Open lookup gates by continuation token.
pub(crate) type LookupGates = HashMap<u64, std::sync::Weak<LookupGate>>;

const GATE_OPEN: u8 = 0;
const GATE_SETTLED: u8 = 1;
const GATE_FAILED: u8 = 2;

impl LookupGate {
    /// A gate for the answer to lookup `token`, registered in `registry`.
    pub(crate) fn open(registry: &Arc<Mutex<LookupGates>>, token: u64) -> Arc<LookupGate> {
        let gate = Arc::new(LookupGate {
            state: std::sync::atomic::AtomicU8::new(GATE_OPEN),
            failed: tokio::sync::Notify::new(),
            token,
            registry: Arc::downgrade(registry),
        });
        registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(token, Arc::downgrade(&gate));
        gate
    }

    /// The hook failed the connection: true if it was still connecting (the
    /// dial is abandoned), false once it had connected or failed already.
    pub(crate) fn fail(&self) -> bool {
        let failed = self
            .state
            .compare_exchange(GATE_OPEN, GATE_FAILED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if failed {
            self.failed.notify_waiters();
        }
        failed
    }

    /// Run the TCP connect `connect` inside the window: abandoned when the
    /// hook fails the connection first, and the window closed when it ends.
    async fn guard<T>(
        &self,
        connect: impl Future<Output = Result<T, BoxError>>,
    ) -> Result<T, BoxError> {
        let abandoned = async {
            let notified = self.failed.notified();
            let mut notified = std::pin::pin!(notified);
            notified.as_mut().enable();
            if self.state.load(Ordering::Acquire) != GATE_FAILED {
                notified.await;
            }
        };
        let result = tokio::select! {
            biased;
            () = abandoned => return Err(Box::new(LookupFailed)),
            result = connect => result,
        };
        // Connected, or refused on its own: from here a callback is too
        // late. A failure that won the race to the state still wins.
        match self.state.compare_exchange(
            GATE_OPEN,
            GATE_SETTLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => result,
            Err(_) => Err(Box::new(LookupFailed)),
        }
    }
}

impl Drop for LookupGate {
    fn drop(&mut self) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let mut gates = registry.lock().unwrap_or_else(|e| e.into_inner());
        // Only its own entry (a dead weak): a token is never reused, but a
        // live entry under it is not this gate's to remove.
        if gates
            .get(&self.token)
            .is_some_and(|weak| weak.strong_count() == 0)
        {
            gates.remove(&self.token);
        }
    }
}

/// The hook failed the connection its answer was opening ([`LookupGate`]):
/// the fetch fails with the hook's error, which JS holds.
#[derive(Debug)]
pub(crate) struct LookupFailed;

impl std::fmt::Display for LookupFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the connect.lookup hook failed the connection")
    }
}

impl std::error::Error for LookupFailed {}

/// What one dial takes from the request it is made for, rather than from
/// the connector: a pool is shared by requests that each carry their own.
#[derive(Debug, Clone)]
pub(crate) struct DialParams {
    /// node's happy-eyeballs attempt timeout as the request's caller read
    /// it (`net.getDefaultAutoSelectFamilyAttemptTimeout()` when the fetch
    /// started), latched by the dial as node latches it per socket.
    pub(crate) attempt_timeout: Duration,
    /// On a lookup-hooked connector, the hook's answer for this dial's host.
    pub(crate) pin: Option<HookPin>,
}

/// A lookup-hooked connector was asked to open a connection to a host NAME
/// with no hook answer for it: the request has to wait for the hook (the
/// fetch loop parks, and resumes with the answer as a [`HookPin`]). Carries
/// the authority's [`authority_key`] and the host the hook is asked about.
#[derive(Debug)]
pub(crate) struct NeedsLookup {
    pub(crate) key: String,
    pub(crate) host: String,
}

impl std::fmt::Display for NeedsLookup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "a new connection to {} needs a connect.lookup answer",
            self.key
        )
    }
}

impl std::error::Error for NeedsLookup {}

/// The key one hook answer is filed under: `host:port`, host lowercased and
/// unbracketed, port defaulted by scheme so `http://h/` and `http://h:80/`
/// are the same authority. It must agree between the [`NeedsLookup`] a
/// dial asks with and the [`HookPin`] it is then given.
pub(crate) fn authority_key(uri: &Uri) -> Option<String> {
    let host = host_for_connect(uri)?;
    let port = uri
        .port_u16()
        .unwrap_or(if uri.scheme_str() == Some("https") {
            443
        } else {
            80
        });
    Some(format!("{}:{port}", host.to_ascii_lowercase()))
}

/// A connection JS supplied for one fetch whose undici dispatcher carries a
/// `connect` FUNCTION: the near end of a [`crate::byte_pipe`] JS pumps to and
/// from the socket that function handed back. That socket is already the
/// whole transport -- connected, and for an https origin already TLS -- so
/// the connector adds nothing to it.
pub(crate) struct SuppliedConn {
    pub(crate) io: tokio::io::DuplexStream,
    /// The socket negotiated h2 by ALPN (undici then speaks h2 over it).
    pub(crate) h2: bool,
}

/// One connector-hooked fetch's supplied connections: [`authority_key`] ->
/// the connections JS supplied for it, oldest first. Each is used once.
pub(crate) type SuppliedConns = Arc<Mutex<HashMap<String, Vec<SuppliedConn>>>>;

/// Which client a connector serves.
#[derive(Clone)]
pub(crate) enum Via {
    /// The shared pool: the environment proxy applies and DNS is getaddrinfo.
    Pooled,
    /// A lookup-hooked dispatcher's pool. A host name is dialled only at
    /// the addresses the hook answered for that dial ([`DialParams::pin`])
    /// -- never through getaddrinfo, never through the environment proxy
    /// (an undici Agent never reads HTTP_PROXY, and a proxy would resolve
    /// the name itself, defeating the hook). An IP literal is dialled as
    /// written: node never calls lookup for one.
    Hooked,
    /// One connector-hooked fetch's own client: every connection is one JS
    /// supplied for the authority, from the socket the dispatcher's
    /// `connect` function returned. Nothing is dialled here -- no DNS, no
    /// environment proxy, no TLS (undici hands the request to whatever
    /// socket its connector returns, and so does this).
    Supplied { conns: SuppliedConns },
}

#[derive(Clone)]
pub(crate) struct OamConnector {
    pub(crate) shared: Arc<Shared>,
    pub(crate) via: Via,
}

type ConnFuture = StdPin<Box<dyn Future<Output = Result<OamConn, BoxError>> + Send>>;

/// undici's connect timeout expired (lib/core/connect.js `onConnectTimeout`,
/// 6.24.1): the request fails with its `ConnectTimeoutError`, code
/// `UND_ERR_CONNECT_TIMEOUT`, whose message this is. undici names the
/// addresses net.connect had attempted when the connect was a multi-address
/// one (`attempted addresses: ::1:80, 127.0.0.1:80,`), and otherwise the
/// host and port it asked for (`attempted address: example.test:443,`) --
/// measured on node v22.22.2 for an IP literal, a name with two addresses,
/// and the default ports.
#[derive(Debug)]
pub(crate) struct ConnectTimedOut(pub(crate) String);

impl ConnectTimedOut {
    fn new(
        host: &str,
        port: u16,
        attempted: Option<&[std::net::SocketAddr]>,
        timeout: Duration,
    ) -> ConnectTimedOut {
        let tried = match attempted {
            // node's `${address}:${port}`: an IPv6 address unbracketed.
            Some(list) => {
                let list: Vec<String> = list
                    .iter()
                    .map(|addr| format!("{}:{}", addr.ip(), addr.port()))
                    .collect();
                format!("attempted addresses: {},", list.join(", "))
            }
            None => format!("attempted address: {host}:{port},"),
        };
        ConnectTimedOut(format!(
            "Connect Timeout Error ({tried} timeout: {}ms)",
            timeout.as_millis()
        ))
    }
}

impl std::fmt::Display for ConnectTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConnectTimedOut {}

/// A hooked fetch reached the connector for a host its hook never resolved.
/// The fetch loop parks for the hook before every send, so this is a
/// backstop: it fails the request rather than fall back to system DNS.
#[derive(Debug)]
struct UnresolvedHost(String);

impl std::fmt::Display for UnresolvedHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no connect.lookup result for host {}", self.0)
    }
}

impl std::error::Error for UnresolvedHost {}

/// A connector-hooked fetch reached the connector for an authority JS
/// supplied no connection for. The fetch loop parks for one before every
/// send, so this is a backstop: it fails the request rather than dial the
/// origin itself, which would skip the dispatcher's `connect` function.
#[derive(Debug)]
struct UnsuppliedConnection(String);

impl std::fmt::Display for UnsuppliedConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no connection from the dispatcher's connect for {}",
            self.0
        )
    }
}

impl std::error::Error for UnsuppliedConnection {}

impl OamConnector {
    /// Dial one connection for `dst`: node's connect algorithm, TLS offering
    /// `alpn` to an https origin, the environment proxy / CONNECT tunnel, or
    /// a hooked / supplied connection (whose socket JS made, ALPN and all).
    /// The owned pool calls this directly instead of through the `Service`
    /// impl hyper-util used. `params` are the requesting request's own: its
    /// attempt timeout, and on a hooked connector its hook's answer.
    pub(crate) async fn connect(
        self,
        dst: Uri,
        alpn: Alpn,
        params: DialParams,
    ) -> Result<OamConn, BoxError> {
        self.connect_logged(dst, alpn, params, &AttemptLog::default())
            .await
    }

    /// On a lookup-hooked connector, the hook answer a new connection to
    /// `dst` needs: `None` for an IP literal (node never looks one up), on
    /// every other connector, and for a URI with no host (which the dial
    /// itself refuses).
    pub(crate) fn needs_lookup(&self, dst: &Uri) -> Option<NeedsLookup> {
        if !matches!(self.via, Via::Hooked) {
            return None;
        }
        let host = host_for_connect(dst)?;
        if host.parse::<IpAddr>().is_ok() {
            return None;
        }
        let key = authority_key(dst)?;
        Some(NeedsLookup { key, host })
    }

    /// [`OamConnector::connect`] under undici's connect timeout: the whole
    /// connect -- the lookup, every address attempt and, for https, the TLS
    /// handshake (undici clears its timer on `secureConnect`) -- has
    /// `timeout` to finish, or fails as [`ConnectTimedOut`]. `None` is no
    /// timeout (`http.request`, which has none in node, or a dispatcher that
    /// set 0). Dropping the connect future closes whatever it had open.
    pub(crate) async fn connect_within(
        self,
        dst: Uri,
        alpn: Alpn,
        timeout: Option<Duration>,
        params: DialParams,
    ) -> Result<OamConn, BoxError> {
        let Some(timeout) = timeout else {
            return self.connect(dst, alpn, params).await;
        };
        // The host undici's message names: the one it handed net.connect --
        // the origin's, or the proxy's when the request goes through one.
        let via_proxy = match &self.via {
            Via::Pooled => self.shared.proxy.as_ref().and_then(|m| m.intercept(&dst)),
            _ => None,
        };
        let dialled = via_proxy.as_ref().map_or(&dst, |intercept| intercept.uri());
        let host = host_for_connect(dialled).unwrap_or_default();
        let port = dialled
            .port_u16()
            .unwrap_or(if dialled.scheme_str() == Some("https") {
                443
            } else {
                80
            });
        let log = AttemptLog::default();
        match tokio::time::timeout(timeout, self.connect_logged(dst, alpn, params, &log)).await {
            Ok(connected) => connected,
            Err(_elapsed) => Err(Box::new(ConnectTimedOut::new(
                &host,
                port,
                log.attempted().as_deref(),
                timeout,
            ))),
        }
    }

    async fn connect_logged(
        self,
        dst: Uri,
        alpn: Alpn,
        params: DialParams,
        log: &AttemptLog,
    ) -> Result<OamConn, BoxError> {
        let https = dst.scheme_str() == Some("https");
        let host = host_for_connect(&dst).ok_or("request url has no host")?;
        let port = dst.port_u16().unwrap_or(if https { 443 } else { 80 });
        let mut gate = None;
        let opts = match &self.via {
            Via::Supplied { conns } => return supplied(conns, &dst),
            Via::Pooled => {
                if let Some(intercept) = self.shared.proxy.as_ref().and_then(|m| m.intercept(&dst))
                {
                    return self
                        .through_proxy(dst, &host, intercept, alpn, params.attempt_timeout)
                        .await;
                }
                ConnectOptions {
                    attempt_timeout: params.attempt_timeout,
                    pin: None,
                    local: None,
                }
            }
            Via::Hooked => {
                let pin = if host.parse::<IpAddr>().is_ok() {
                    None
                } else {
                    // Only an answer for THIS authority: the hook was asked
                    // about a host and port, so a hop to the same name on
                    // another port (or to another name) is never dialled on
                    // addresses approved for the first.
                    let key = authority_key(&dst);
                    let Some(answer) = params.pin.filter(|pin| Some(&pin.key) == key.as_ref())
                    else {
                        return Err(Box::new(UnresolvedHost(host)));
                    };
                    gate = answer.gate;
                    Some(Pin {
                        host: host.to_ascii_lowercase(),
                        addrs: answer.addrs,
                    })
                };
                ConnectOptions {
                    attempt_timeout: params.attempt_timeout,
                    pin,
                    local: None,
                }
            }
        };
        let name = if https {
            Some(server_name(&host)?)
        } else {
            None
        };
        let tcp = match gate {
            Some(gate) => gate.guard(dial(&host, port, &opts, log)).await?,
            None => dial(&host, port, &opts, log).await?,
        };
        let info = ConnInfo::of(&tcp);
        let Some(name) = name else {
            return Ok(OamConn::new(Box::new(tcp), false, false, info));
        };
        let config = self.shared.tls(alpn).await?;
        let (tls, h2) = tls_handshake(config, name, tcp).await?;
        let info = info.with_tls(tls.get_ref().1);
        Ok(OamConn::new(Box::new(tls), h2, false, info))
    }

    async fn through_proxy(
        self,
        dst: Uri,
        host: &str,
        intercept: Intercept,
        alpn: Alpn,
        attempt_timeout: Duration,
    ) -> Result<OamConn, BoxError> {
        if !matches!(intercept.uri().scheme_str(), Some("http" | "https")) {
            return Err(format!(
                "unsupported proxy scheme {}",
                intercept.uri().scheme_str().unwrap_or("")
            )
            .into());
        }
        let mut transport = ProxyTransport {
            shared: self.shared.clone(),
            attempt_timeout,
        };
        if dst.scheme_str() != Some("https") {
            let mut conn = transport.call(intercept.uri().clone()).await?;
            conn.proxied = true;
            return Ok(conn);
        }
        let name = server_name(host)?;
        let mut tunnel = Tunnel::new(intercept.uri().clone(), transport);
        if let Some(auth) = intercept.basic_auth() {
            tunnel = tunnel.with_auth(auth.clone());
        }
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, self.shared.user_agent.clone());
        tunnel = tunnel.with_headers(headers);
        let tunneled = tunnel.call(dst).await?;
        // The TCP endpoints are the proxy's; the TLS session is the origin's.
        let endpoints = tunneled.info.clone();
        let config = self.shared.tls(alpn).await?;
        let (tls, h2) = tls_handshake(config, name, TokioIo::new(tunneled)).await?;
        let info = endpoints.with_tls(tls.get_ref().1);
        Ok(OamConn::new(Box::new(tls), h2, false, info))
    }
}

/// The next connection JS supplied for `dst`'s authority, used as it is.
fn supplied(conns: &SuppliedConns, dst: &Uri) -> Result<OamConn, BoxError> {
    let key = authority_key(dst).ok_or("request url has no host")?;
    let conn = conns
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&key)
        .and_then(|queue| (!queue.is_empty()).then(|| queue.remove(0)));
    let Some(conn) = conn else {
        return Err(Box::new(UnsuppliedConnection(key)));
    };
    Ok(OamConn::new(
        Box::new(conn.io),
        conn.h2,
        false,
        ConnInfo::default(),
    ))
}

/// Dials a proxy: the CONNECT tunnel's inner connector and the connection an
/// http destination is sent through. It never applies a hook's addresses
/// (those name the origin) and never consults the proxy rules (a proxy URI
/// the rules also intercept would recurse).
#[derive(Clone)]
pub(crate) struct ProxyTransport {
    pub(crate) shared: Arc<Shared>,
    pub(crate) attempt_timeout: Duration,
}

impl Service<Uri> for ProxyTransport {
    type Response = OamConn;
    type Error = BoxError;
    type Future = ConnFuture;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, proxy: Uri) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            let https = proxy.scheme_str() == Some("https");
            let host = host_for_connect(&proxy).ok_or("proxy url has no host")?;
            let port = proxy.port_u16().unwrap_or(if https { 443 } else { 80 });
            let name = if https {
                Some(server_name(&host)?)
            } else {
                None
            };
            let opts = ConnectOptions {
                attempt_timeout: this.attempt_timeout,
                pin: None,
                local: None,
            };
            let tcp = dial(&host, port, &opts, &AttemptLog::default()).await?;
            // The proxy's endpoints. Its own TLS session is not an origin's
            // and is not reported.
            let info = ConnInfo::of(&tcp);
            let Some(name) = name else {
                return Ok(OamConn::new(Box::new(tcp), false, false, info));
            };
            // No ALPN towards the proxy: what goes through it is an
            // http/1.1 CONNECT or an absolute-form request, which h2 cannot
            // carry. (reqwest offered h2 here for an http destination.)
            let config = this.shared.tls(Alpn::None).await?;
            let (tls, _) = tls_handshake(config, name, tcp).await?;
            Ok(OamConn::new(Box::new(tls), false, false, info))
        })
    }
}

/// `net_connect::connect`, then the socket options. `log` learns what is
/// attempted while the connect runs (see [`OamConnector::connect_within`]).
async fn dial(
    host: &str,
    port: u16,
    opts: &ConnectOptions,
    log: &AttemptLog,
) -> Result<EagerTcp, BoxError> {
    let connected = net_connect::connect_logged(host, port, opts, log)
        .await
        .map_err(|e| Box::new(e) as BoxError)?;
    tune(&connected.stream);
    Ok(EagerTcp {
        stream: connected.stream,
        closer: ConnCloser::new(),
    })
}

/// How JS closes one connection the transport dialled: node's `destroy()`
/// or `resetAndDestroy()` on the `req.socket` of an `http.request` it
/// carries. The connection's task waits on [`ConnCloser::requested`] beside
/// hyper's dispatcher and drops the connection when it fires, whether a
/// response is streaming over it or it sits idle in the pool; a reset arms
/// SO_LINGER 0 first, through the stream's own drop ([`EagerTcp`]).
///
/// Found by id in a process-wide table of weak entries, which the closer
/// leaves when the connection's last holder (its stream, its task) drops it:
/// one entry per connection, written at the dial and at the close, nothing
/// per request.
///
/// A close names the checkout it was asked through (`lease`): a socket JS
/// kept from a finished request must not close the connection once the pool
/// has handed it to another request -- node's `req.socket` is that request's
/// own, and a `fetch` never shares undici's pool with it.
pub(crate) struct ConnCloser {
    id: u64,
    requested: AtomicBool,
    reset: AtomicBool,
    wake: tokio::sync::Notify,
    /// The connection's checkouts so far ([`ConnInfo::take_lease`]).
    leases: Arc<AtomicU64>,
}

type Closers = Mutex<HashMap<u64, std::sync::Weak<ConnCloser>>>;

fn closers() -> &'static Closers {
    static CLOSERS: std::sync::OnceLock<Closers> = std::sync::OnceLock::new();
    CLOSERS.get_or_init(Default::default)
}

impl ConnCloser {
    fn new() -> Arc<ConnCloser> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let closer = Arc::new(ConnCloser {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            requested: AtomicBool::new(false),
            reset: AtomicBool::new(false),
            wake: tokio::sync::Notify::new(),
            leases: Arc::new(AtomicU64::new(0)),
        });
        closers()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(closer.id, Arc::downgrade(&closer));
        closer
    }

    /// The closer of a live connection, by the id its responses carry.
    pub(crate) fn find(id: u64) -> Option<Arc<ConnCloser>> {
        closers()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .and_then(std::sync::Weak::upgrade)
    }

    /// Resolves once JS has asked for the connection to close.
    pub(crate) async fn requested(&self) {
        // One waiter (the connection's task); `notify_one` keeps the permit
        // for a request made before it first waits.
        while !self.requested.load(Ordering::Acquire) {
            self.wake.notified().await;
        }
    }

    fn close(&self, reset: bool) {
        if reset {
            self.reset.store(true, Ordering::Release);
        }
        self.requested.store(true, Ordering::Release);
        self.wake.notify_one();
    }
}

impl Drop for ConnCloser {
    fn drop(&mut self) {
        closers()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

/// `__oam.fetchConnClose(id, reset, lease)`: close the transport connection
/// with this id -- with a reset when `reset` -- if it is still open and,
/// when `lease` names the checkout of the response asking, no request has
/// taken it since: mid-response, or idle in the pool after it.
pub fn close_connection(id: u64, reset: bool, lease: Option<u64>) {
    if let Some(closer) = ConnCloser::find(id) {
        if lease.is_some_and(|lease| closer.leases.load(Ordering::Relaxed) != lease) {
            return;
        }
        closer.close(reset);
    }
}

/// The most one direct read takes. The part of the caller's buffer it reads
/// into is zero-filled first (a safe read needs initialised memory), so this
/// bounds that cost; the rest waits for the next read.
const DIRECT_READ_MAX: usize = 16 * 1024;

/// A fetch connection's TCP stream. A read tokio reports as pending is
/// checked against the kernel before it is reported: an EOF or bytes the
/// kernel already holds are returned now.
///
/// Why: hyper reads an idle HTTP/1 connection before it writes the next
/// request onto it, and a connection that reads EOF there is closed and
/// hands the request back UNSENT (hyper-util then takes another connection,
/// or dials one). But tokio answers a read from its readiness cache, and the
/// cache only learns of the server's FIN when a worker next polls the
/// reactor. With both I/O workers busy -- requests going out back to back,
/// a redirect loop that never leaves Rust -- the read comes back `Pending`
/// without a syscall and hyper writes the request onto a connection the
/// kernel already knows is closed. For a GET that cost a resend; a POST, which
/// is never resent, failed with `fetch failed` where node's succeeded: node's
/// event loop reads the FIN before it writes the next request.
///
/// The check is a one-byte `MSG_PEEK`, so the common answer (nothing there)
/// costs one non-blocking syscall and copies nothing. Reading around tokio is
/// sound: tokio has registered the waker before it returned `Pending`, and a
/// readiness event for bytes read here costs tokio one `WouldBlock`. A FIN
/// still in flight when the request is written is not covered; node loses
/// that race too, and for an idempotent request the send path's single
/// resend covers it.
///
/// It also carries the connection's [`ConnCloser`], and a connection JS
/// reset is dropped with SO_LINGER 0, so the close is the reset.
pub(crate) struct EagerTcp {
    stream: tokio::net::TcpStream,
    closer: Arc<ConnCloser>,
}

impl Drop for EagerTcp {
    fn drop(&mut self) {
        if self.closer.reset.load(Ordering::Acquire) {
            crate::tcp::arm_reset(&self.stream);
        }
    }
}

impl AsyncRead for EagerTcp {
    fn poll_read(
        self: StdPin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let stream = &mut self.get_mut().stream;
        if let ready @ Poll::Ready(_) = StdPin::new(&mut *stream).poll_read(cx, buf) {
            return ready;
        }
        // No room: nothing to report, and a zero-byte read would read as EOF.
        if buf.remaining() == 0 {
            return Poll::Pending;
        }
        let socket = socket2::SockRef::from(&*stream);
        let mut probe = [MaybeUninit::<u8>::uninit()];
        match socket.peek(&mut probe) {
            // EOF: the server closed, and the reactor has not reported it.
            Ok(0) => Poll::Ready(Ok(())),
            Ok(_) => {
                let dst = buf.initialize_unfilled_to(buf.remaining().min(DIRECT_READ_MAX));
                match std::io::Read::read(&mut &*socket, dst) {
                    Ok(n) => {
                        buf.advance(n);
                        Poll::Ready(Ok(()))
                    }
                    Err(e) => pending_or_error(e, cx),
                }
            }
            Err(e) => pending_or_error(e, cx),
        }
    }
}

/// A direct read's error: `WouldBlock` is tokio's `Pending` standing, an
/// interrupted call is retried on the next poll (no readiness event is owed
/// for it), anything else is the read's error.
fn pending_or_error(e: std::io::Error, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
    match e.kind() {
        std::io::ErrorKind::WouldBlock => Poll::Pending,
        std::io::ErrorKind::Interrupted => {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
        _ => Poll::Ready(Err(e)),
    }
}

impl AsyncWrite for EagerTcp {
    fn poll_write(
        self: StdPin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        StdPin::new(&mut self.get_mut().stream).poll_write(cx, buf)
    }

    fn poll_flush(self: StdPin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        StdPin::new(&mut self.get_mut().stream).poll_flush(cx)
    }

    fn poll_shutdown(self: StdPin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        StdPin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_write_vectored(
        self: StdPin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        StdPin::new(&mut self.get_mut().stream).poll_write_vectored(cx, bufs)
    }
}

/// reqwest's socket defaults (async_impl/client.rs:303-307, applied by
/// hyper-util connect/http.rs). Best-effort: hyper-util logs a failure and
/// keeps the connection, and so does this.
pub(crate) fn tune(stream: &tokio::net::TcpStream) {
    // net_connect leaves Nagle on (node's net.connect does); http.Agent and
    // undici turn it off, as reqwest did.
    let _ = stream.set_nodelay(true);
    let keepalive = socket2::TcpKeepalive::new().with_time(KEEPALIVE_IDLE);
    #[cfg(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "windows",
        target_os = "android",
        target_os = "freebsd",
        target_os = "ios",
    ))]
    let keepalive = keepalive.with_interval(KEEPALIVE_INTERVAL);
    // hyper-util sets the probe count everywhere socket2 0.5 could, which
    // leaves Windows out.
    #[cfg(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "android",
        target_os = "freebsd",
        target_os = "ios",
    ))]
    let keepalive = keepalive.with_retries(KEEPALIVE_RETRIES);
    let socket = socket2::SockRef::from(stream);
    let _ = socket.set_tcp_keepalive(&keepalive);
    #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
    let _ = socket.set_tcp_user_timeout(Some(TCP_USER_TIMEOUT));
}

const KEEPALIVE_IDLE: Duration = Duration::from_secs(15);
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "windows",
    target_os = "android",
    target_os = "freebsd",
    target_os = "ios",
))]
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "android",
    target_os = "freebsd",
    target_os = "ios",
))]
const KEEPALIVE_RETRIES: u32 = 3;
#[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
const TCP_USER_TIMEOUT: Duration = Duration::from_secs(30);

/// The TLS name for `host` (brackets already stripped). A failure is a plain
/// error: the send path reports it as the generic send failure, as
/// hyper-rustls's refusal was.
fn server_name(host: &str) -> Result<ServerName<'static>, BoxError> {
    ServerName::try_from(host.to_string()).map_err(|e| Box::new(e) as BoxError)
}

/// Handshake over `io`; the bool is whether ALPN selected h2.
async fn tls_handshake<IO>(
    config: Arc<ClientConfig>,
    name: ServerName<'static>,
    io: IO,
) -> Result<(tokio_rustls::client::TlsStream<IO>, bool), BoxError>
where
    IO: AsyncRead + AsyncWrite + Unpin,
{
    let tls = tokio_rustls::TlsConnector::from(config)
        .connect(name, io)
        .await
        .map_err(|e| Box::new(HandshakeFailed(e)) as BoxError)?;
    let h2 = tls.get_ref().1.alpn_protocol() == Some(b"h2".as_slice());
    Ok((tls, h2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared(proxy: Option<Matcher>) -> Arc<Shared> {
        Arc::new(Shared {
            tls: TlsSource::Unavailable("test".to_string()),
            proxy,
            user_agent: HeaderValue::from_static("oam/test"),
            tls_range: AtomicU8::new(TlsRange::Both.code()),
        })
    }

    #[tokio::test]
    async fn tune_sets_nodelay_and_keepalive() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        tune(&stream);
        let socket = socket2::SockRef::from(&stream);
        assert!(socket.tcp_nodelay().unwrap());
        assert!(socket.keepalive().unwrap());
    }

    /// A dialled connection and a peer that reads it to the end on a thread
    /// of its own, reporting how that read ended.
    async fn dialled_with_peer() -> (EagerTcp, std::sync::mpsc::Receiver<std::io::Result<usize>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            let mut all = Vec::new();
            let _ = tx.send(std::io::Read::read_to_end(&mut peer, &mut all).map(|_| all.len()));
        });
        let opts = ConnectOptions {
            attempt_timeout: Duration::from_millis(250),
            pin: None,
            local: None,
        };
        (
            dial("127.0.0.1", port, &opts, &AttemptLog::default())
                .await
                .unwrap(),
            rx,
        )
    }

    async fn peer_read_ended(
        rx: std::sync::mpsc::Receiver<std::io::Result<usize>>,
    ) -> std::io::Result<usize> {
        tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(30)))
            .await
            .unwrap()
            .expect("the peer's read never ended")
    }

    /// `req.socket.resetAndDestroy()` on the fetch path: closing the
    /// connection by the id its responses carry wakes the connection's task,
    /// the stream's drop is then a reset, and the id leaves the table with
    /// the connection -- a late close is a no-op.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_connection_closed_by_its_id_resets_and_leaves_the_table() {
        let (tcp, peer) = dialled_with_peer().await;
        let id = ConnInfo::of(&tcp)
            .connection
            .expect("an h1 connection is named");
        let task = ConnCloser::find(id).expect("a live connection is in the table");
        let waiting = tokio::spawn(async move { task.requested().await });
        close_connection(id, true, None);
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("the connection's task was woken")
            .unwrap();
        drop(tcp);
        let ended = peer_read_ended(peer).await;
        assert_eq!(
            ended.as_ref().map_err(std::io::Error::kind).err(),
            Some(std::io::ErrorKind::ConnectionReset),
            "{ended:?}"
        );
        assert!(
            ConnCloser::find(id).is_none(),
            "the closed connection left the table"
        );
        close_connection(id, true, None);
    }

    /// `destroy()` rather than `resetAndDestroy()`: the connection still
    /// closes, with the FIN of an orderly end.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_connection_closed_without_a_reset_ends_with_a_fin() {
        let (tcp, peer) = dialled_with_peer().await;
        let id = ConnInfo::of(&tcp).connection.unwrap();
        close_connection(id, false, None);
        tokio::time::timeout(Duration::from_secs(5), tcp.closer.requested())
            .await
            .expect("the close was asked for");
        drop(tcp);
        assert_eq!(peer_read_ended(peer).await.unwrap(), 0);
    }

    /// A close asked through a finished request's socket reaches the
    /// connection while that request's checkout is its latest -- mid-response
    /// or idle in the pool after it -- and not once the pool has handed it to
    /// another request: a socket kept from the first must not fail the
    /// second, as node's does not (stale.mjs: an `https.get`'s kept
    /// `req.socket.destroy()` failed a later `fetch()` on the reused
    /// connection).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_close_through_an_earlier_checkout_leaves_a_reused_connection_open() {
        let (tcp, _peer) = dialled_with_peer().await;
        let mut first = ConnInfo::of(&tcp);
        first.take_lease();
        let mut second = first.clone();
        second.take_lease();
        assert_eq!((first.lease, second.lease), (Some(1), Some(2)));
        let id = first.connection.unwrap();
        close_connection(id, false, first.lease);
        assert!(
            !tcp.closer.requested.load(Ordering::Acquire),
            "the first request's socket closed the second's connection"
        );
        close_connection(id, false, second.lease);
        tokio::time::timeout(Duration::from_secs(5), tcp.closer.requested())
            .await
            .expect("the current request's socket closes it");
    }

    /// An h2 connection carries many requests at once: it is not named, so
    /// no one request's socket can close it under the others.
    #[tokio::test]
    async fn an_h2_connection_is_not_named() {
        let (tcp, _peer) = dialled_with_peer().await;
        let info = ConnInfo::of(&tcp);
        assert!(info.connection.is_some());
        let conn = OamConn::new(Box::new(tcp), true, false, info);
        assert_eq!(conn.conn_info().connection, None);
        let mut taken = conn.conn_info();
        taken.take_lease();
        assert_eq!(taken.lease, None);
    }

    /// The proxy dial takes the URI it is given even when the proxy rules
    /// would intercept that URI too.
    #[tokio::test]
    async fn proxy_transport_never_consults_the_rules() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept = tokio::spawn(async move { listener.accept().await.map(|_| ()) });
        let rules = Matcher::builder().all("http://127.0.0.1:1").build();
        let mut transport = ProxyTransport {
            shared: shared(Some(rules)),
            attempt_timeout: Duration::from_millis(250),
        };
        let uri: Uri = format!("http://127.0.0.1:{port}/").parse().unwrap();
        let conn = transport.call(uri).await.unwrap();
        assert!(!conn.proxied && !conn.h2);
        accept.await.unwrap().unwrap();
    }

    /// A hooked connector never resolves a host its hook did not answer for:
    /// not with no answer, and not with an answer for another authority --
    /// the same name on another port included.
    #[tokio::test]
    async fn hooked_connector_fails_closed_on_an_unresolved_host() {
        let connector = OamConnector {
            shared: shared(None),
            via: Via::Hooked,
        };
        let uri: Uri = "http://localhost:1/".parse().unwrap();
        let needed = connector
            .needs_lookup(&uri)
            .expect("a name needs an answer");
        assert_eq!(
            (needed.key.as_str(), needed.host.as_str()),
            ("localhost:1", "localhost")
        );
        let elsewhere = |key: &str| DialParams {
            attempt_timeout: Duration::from_millis(250),
            pin: Some(HookPin {
                key: key.to_string(),
                addrs: vec!["127.0.0.1".parse().unwrap()],
                gate: None,
            }),
        };
        for dial in [
            DialParams {
                attempt_timeout: Duration::from_millis(250),
                pin: None,
            },
            elsewhere("localhost:2"),
            elsewhere("other.test:1"),
        ] {
            let err = match connector
                .clone()
                .connect(uri.clone(), Alpn::Http1, dial)
                .await
            {
                Ok(_) => panic!("connected without a lookup result for this authority"),
                Err(e) => e,
            };
            assert!(err.downcast_ref::<UnresolvedHost>().is_some(), "{err}");
        }
        assert!(
            connector
                .needs_lookup(&"http://127.0.0.1:1/".parse().unwrap())
                .is_none(),
            "an IP literal is dialled as written"
        );
    }

    /// #169: a hook that fails the connection while its answer's connect
    /// is still being made abandons that connect -- whether it fails first
    /// or in the middle -- and fails it with `LookupFailed`; once the
    /// connect is made (or failed on its own), a failure is too late and
    /// changes nothing. The registry finds a gate by its token while its
    /// pin lives, and forgets it after.
    #[tokio::test]
    async fn a_lookup_gate_fails_only_a_connect_still_being_made() {
        let registry = Arc::new(Mutex::new(LookupGates::new()));
        let find = |token: u64| {
            registry
                .lock()
                .unwrap()
                .get(&token)
                .and_then(std::sync::Weak::upgrade)
        };

        // Failed before the connect starts: it never runs.
        let gate = LookupGate::open(&registry, 1);
        assert!(find(1).is_some_and(|found| found.fail()));
        let err = gate
            .guard(async { Ok::<_, BoxError>(()) })
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<LookupFailed>().is_some(), "{err}");
        assert!(!gate.fail(), "failed once");

        // Failed while the connect is pending: abandoned.
        let gate = LookupGate::open(&registry, 2);
        let failing = gate.clone();
        let connect = tokio::spawn(async move {
            failing
                .guard(std::future::pending::<Result<(), BoxError>>())
                .await
        });
        tokio::task::yield_now().await;
        assert!(gate.fail());
        let err = tokio::time::timeout(Duration::from_secs(5), connect)
            .await
            .expect("the pending connect was abandoned")
            .unwrap()
            .unwrap_err();
        assert!(err.downcast_ref::<LookupFailed>().is_some(), "{err}");

        // Connected: a failure after is ignored, and the connection stands.
        let gate = LookupGate::open(&registry, 3);
        gate.guard(async { Ok::<_, BoxError>(()) }).await.unwrap();
        assert!(!gate.fail());

        // A connect that failed on its own keeps its own error.
        let gate = LookupGate::open(&registry, 4);
        let err = gate
            .guard(async { Err::<(), BoxError>("refused".into()) })
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "refused");
        assert!(!gate.fail());

        // The registry holds no gate whose pin is gone.
        drop(gate);
        assert!(find(4).is_none());
        assert!(!registry.lock().unwrap().contains_key(&4));
    }
}
