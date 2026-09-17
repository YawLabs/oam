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
use std::net::IpAddr;
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
use tokio::io::{AsyncRead, AsyncWrite};
use tower_service::Service;

use super::BoxError;
use super::prepare::host_for_connect;
use super::tls_config::{self, TlsConfigs};
use crate::net_connect::{self, ConnectOptions, Pin};

/// The byte stream under a connection: TCP, TLS over TCP, or TLS over a
/// tunnel.
pub(crate) trait AsyncIo: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> AsyncIo for T {}

/// A connection handed to hyper-util. `Connected` is not publicly `Clone`, so
/// the two facts it carries are kept as flags and a fresh one is built on
/// every call.
pub(crate) struct OamConn {
    io: TokioIo<Box<dyn AsyncIo>>,
    /// ALPN selected h2.
    h2: bool,
    /// An http request through a proxy: hyper writes the absolute form.
    proxied: bool,
}

impl OamConn {
    fn new(io: Box<dyn AsyncIo>, h2: bool, proxied: bool) -> OamConn {
        OamConn {
            io: TokioIo::new(io),
            h2,
            proxied,
        }
    }
}

impl Connection for OamConn {
    fn connected(&self) -> Connected {
        let connected = Connected::new().proxy(self.proxied);
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
        let Some(name) = name else {
            return Ok(OamConn::new(Box::new(tcp), false, false));
        };
        let config = self.shared.tls(false).await?;
        let (tls, h2) = tls_handshake(config, name, tcp).await?;
        Ok(OamConn::new(Box::new(tls), h2, false))
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
        let config = self.shared.tls(false).await?;
        let (tls, h2) = tls_handshake(config, name, TokioIo::new(tunneled)).await?;
        Ok(OamConn::new(Box::new(tls), h2, false))
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
            let Some(name) = name else {
                return Ok(OamConn::new(Box::new(tcp), false, false));
            };
            // No ALPN towards the proxy: what goes through it is an
            // http/1.1 CONNECT or an absolute-form request, which h2 cannot
            // carry. (reqwest offered h2 here for an http destination.)
            let config = this.shared.tls(true).await?;
            let (tls, _) = tls_handshake(config, name, tcp).await?;
            Ok(OamConn::new(Box::new(tls), false, false))
        })
    }
}

/// `net_connect::connect`, then the socket options.
async fn dial(
    host: &str,
    port: u16,
    opts: &ConnectOptions,
) -> Result<tokio::net::TcpStream, BoxError> {
    let connected = net_connect::connect(host, port, opts)
        .await
        .map_err(|e| Box::new(e) as BoxError)?;
    tune(&connected.stream);
    Ok(connected.stream)
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
