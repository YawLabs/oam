//! The connector hyper-util's legacy client dials through: node's connect
//! algorithm (`net_connect`), then TLS, with the environment proxy and a
//! lookup-hooked fetch's own addresses decided here.
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
//!   tunnel carrying the proxy credentials and oam's user-agent, and h2 is
//!   still negotiated with the origin inside it; an http destination is sent
//!   to the proxy in absolute form;
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
use std::sync::atomic::{AtomicU64, Ordering};
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
use super::tls_config::{self, TlsConfigs};
use crate::net_connect::{self, ConnectOptions, Pin};

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
struct ConnCounters {
    uses: AtomicU64,
    read: AtomicU64,
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
}

impl ConnInfo {
    fn of(tcp: &EagerTcp) -> ConnInfo {
        ConnInfo {
            local: tcp.0.local_addr().ok(),
            peer: tcp.0.peer_addr().ok(),
            tls: None,
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
    fn new(io: Box<dyn AsyncIo>, h2: bool, proxied: bool, info: ConnInfo) -> OamConn {
        let stats = ConnStats::new();
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
}

/// The byte stream under a connection, counting what is read from it into
/// the connection's [`ConnStats`]. It sits above TLS, so the count is HTTP
/// bytes only: a TLS close_notify or session ticket is not a response.
struct Counted {
    io: Box<dyn AsyncIo>,
    counters: Arc<ConnCounters>,
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
        StdPin::new(&mut self.get_mut().io).poll_write(cx, buf)
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
        StdPin::new(&mut self.get_mut().io).poll_write_vectored(cx, bufs)
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

/// Where a transport's TLS configs come from.
#[derive(Clone)]
pub enum TlsSource {
    /// The platform verifier plus NODE_EXTRA_CA_CERTS, built on first use
    /// (see [`tls_config::platform`]).
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
    /// The pooled connector's attempt timeout in milliseconds. hyper-util
    /// hands a connector only the `Uri`, so the fetch op stores the value JS
    /// sent before every pooled send and the connector reads it here. Two
    /// concurrent requests can only disagree while
    /// `net.setDefaultAutoSelectFamilyAttemptTimeout()` is changing a value
    /// that is process-wide in node too.
    pub(crate) attempt_timeout_ms: AtomicU64,
}

impl Shared {
    pub(crate) fn attempt_timeout(&self) -> Duration {
        Duration::from_millis(self.attempt_timeout_ms.load(Ordering::Relaxed))
    }

    pub(crate) fn set_attempt_timeout(&self, timeout: Duration) {
        let ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
        self.attempt_timeout_ms.store(ms, Ordering::Relaxed);
    }

    /// The config for an origin handshake, or with `for_proxy` for the
    /// handshake with an https proxy.
    async fn tls(&self, for_proxy: bool) -> Result<Arc<ClientConfig>, BoxError> {
        let configs = match &self.tls {
            TlsSource::Platform => tls_config::platform()
                .await
                .map_err(|e| Box::new(TlsSetupError(e)) as BoxError)?,
            TlsSource::Fixed(configs) => configs.clone(),
            TlsSource::Unavailable(message) => {
                return Err(Box::new(TlsSetupError(message.clone())));
            }
        };
        Ok(if for_proxy {
            configs.proxy
        } else {
            configs.dst
        })
    }
}

/// A lookup-hooked fetch's resolved authorities: [`authority_key`] -> the
/// addresses its `connect.lookup` hook returned, in the hook's order.
pub(crate) type HostAddrs = Arc<Mutex<HashMap<String, Vec<IpAddr>>>>;

/// The key one hook answer is filed under: `host:port`, host lowercased and
/// unbracketed, port defaulted by scheme so `http://h/` and `http://h:80/`
/// are the same authority. It must agree between `Route::lookup_needed`,
/// which files the answer, and [`OamConnector::connect`], which reads it.
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

/// Which client a connector serves.
#[derive(Clone)]
pub(crate) enum Via {
    /// The shared pool: the environment proxy applies, DNS is getaddrinfo,
    /// and the attempt timeout comes from [`Shared`].
    Pooled,
    /// One lookup-hooked fetch's own client. A host name is dialled only at
    /// the addresses its hook returned -- never through getaddrinfo, never
    /// through the environment proxy (an undici Agent never reads
    /// HTTP_PROXY, and a proxy would resolve the name itself, defeating the
    /// hook). An IP literal is dialled as written: node never calls lookup
    /// for one.
    Hooked {
        addrs: HostAddrs,
        attempt_timeout: Duration,
    },
}

#[derive(Clone)]
pub(crate) struct OamConnector {
    pub(crate) shared: Arc<Shared>,
    pub(crate) via: Via,
}

type ConnFuture = StdPin<Box<dyn Future<Output = Result<OamConn, BoxError>> + Send>>;

impl Service<Uri> for OamConnector {
    type Response = OamConn;
    type Error = BoxError;
    type Future = ConnFuture;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        Box::pin(self.clone().connect(dst))
    }
}

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

impl OamConnector {
    async fn connect(self, dst: Uri) -> Result<OamConn, BoxError> {
        let https = dst.scheme_str() == Some("https");
        let host = host_for_connect(&dst).ok_or("request url has no host")?;
        let port = dst.port_u16().unwrap_or(if https { 443 } else { 80 });
        let opts = match &self.via {
            Via::Pooled => {
                if let Some(intercept) = self.shared.proxy.as_ref().and_then(|m| m.intercept(&dst))
                {
                    return self.through_proxy(dst, &host, intercept).await;
                }
                ConnectOptions {
                    attempt_timeout: self.shared.attempt_timeout(),
                    pin: None,
                }
            }
            Via::Hooked {
                addrs,
                attempt_timeout,
            } => {
                let pin = if host.parse::<IpAddr>().is_ok() {
                    None
                } else {
                    // Keyed on the authority the hook was asked about, so a
                    // hop to the same name on another port cannot be dialled
                    // on the first port's approved addresses.
                    let resolved = authority_key(&dst).and_then(|key| {
                        addrs
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .get(&key)
                            .cloned()
                    });
                    let Some(resolved) = resolved else {
                        return Err(Box::new(UnresolvedHost(host)));
                    };
                    Some(Pin {
                        host: host.to_ascii_lowercase(),
                        addrs: resolved,
                    })
                };
                ConnectOptions {
                    attempt_timeout: *attempt_timeout,
                    pin,
                }
            }
        };
        let name = if https {
            Some(server_name(&host)?)
        } else {
            None
        };
        let tcp = dial(&host, port, &opts).await?;
        let info = ConnInfo::of(&tcp);
        let Some(name) = name else {
            return Ok(OamConn::new(Box::new(tcp), false, false, info));
        };
        let config = self.shared.tls(false).await?;
        let (tls, h2) = tls_handshake(config, name, tcp).await?;
        let info = info.with_tls(tls.get_ref().1);
        Ok(OamConn::new(Box::new(tls), h2, false, info))
    }

    async fn through_proxy(
        self,
        dst: Uri,
        host: &str,
        intercept: Intercept,
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
            attempt_timeout: self.shared.attempt_timeout(),
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
        let config = self.shared.tls(false).await?;
        let (tls, h2) = tls_handshake(config, name, TokioIo::new(tunneled)).await?;
        let info = endpoints.with_tls(tls.get_ref().1);
        Ok(OamConn::new(Box::new(tls), h2, false, info))
    }
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
            };
            let tcp = dial(&host, port, &opts).await?;
            // The proxy's endpoints. Its own TLS session is not an origin's
            // and is not reported.
            let info = ConnInfo::of(&tcp);
            let Some(name) = name else {
                return Ok(OamConn::new(Box::new(tcp), false, false, info));
            };
            // No ALPN towards the proxy: what goes through it is an
            // http/1.1 CONNECT or an absolute-form request, which h2 cannot
            // carry. (reqwest offered h2 here for an http destination.)
            let config = this.shared.tls(true).await?;
            let (tls, _) = tls_handshake(config, name, tcp).await?;
            Ok(OamConn::new(Box::new(tls), false, false, info))
        })
    }
}

/// `net_connect::connect`, then the socket options.
async fn dial(host: &str, port: u16, opts: &ConnectOptions) -> Result<EagerTcp, BoxError> {
    let connected = net_connect::connect(host, port, opts)
        .await
        .map_err(|e| Box::new(e) as BoxError)?;
    tune(&connected.stream);
    Ok(EagerTcp(connected.stream))
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
pub(crate) struct EagerTcp(tokio::net::TcpStream);

impl AsyncRead for EagerTcp {
    fn poll_read(
        self: StdPin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let stream = &mut self.get_mut().0;
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
        StdPin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: StdPin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        StdPin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: StdPin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        StdPin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }

    fn poll_write_vectored(
        self: StdPin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        StdPin::new(&mut self.get_mut().0).poll_write_vectored(cx, bufs)
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
        .await?;
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
            attempt_timeout_ms: AtomicU64::new(250),
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

    /// A hooked connector never resolves a host its hook did not answer for.
    #[tokio::test]
    async fn hooked_connector_fails_closed_on_an_unresolved_host() {
        let mut connector = OamConnector {
            shared: shared(None),
            via: Via::Hooked {
                addrs: Arc::new(Mutex::new(HashMap::new())),
                attempt_timeout: Duration::from_millis(250),
            },
        };
        let uri: Uri = "http://localhost:1/".parse().unwrap();
        let err = match connector.call(uri).await {
            Ok(_) => panic!("connected without a lookup result"),
            Err(e) => e,
        };
        assert!(err.downcast_ref::<UnresolvedHost>().is_some(), "{err}");
    }
}
