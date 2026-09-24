//! The transport a fetch sends through: hyper-util's legacy client over
//! oam's connector (`connector.rs`), one pooled client per runtime plus a one-off
//! client for a fetch whose dispatcher carries a `connect.lookup` hook.
//!
//! The pooled client is built the way reqwest 0.13.4 built oam's
//! (async_impl/client.rs:940-1000): hyper-util's defaults except the tokio
//! timers, a 90 s idle timeout and no cap on idle connections per host. The
//! request body is one boxed type for every shape, and its size hint is the
//! wire's: a buffered body goes out with `content-length`, a streamed one
//! chunked.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::Uri;
use http::header::HeaderValue;
use http_body_util::{BodyExt, Empty, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::capture_connection;
use hyper_util::client::proxy::matcher::Matcher;
use hyper_util::rt::{TokioExecutor, TokioTimer};

use super::connector::{
    ConnStats, HostAddrs, OamConnector, Shared, SuppliedConn, SuppliedConns, TlsSetupError, Via,
    authority_key,
};
use super::prepare::host_for_connect;
use super::{BoxError, ReqBody};
use crate::OpOutcome;
use crate::net_connect::{ConnectError, DEFAULT_ATTEMPT_TIMEOUT};

pub use super::connector::TlsSource;
use super::connector::{HandshakeFailed, NoProtocolsAvailable};
use super::tls_config::TlsRange;
use std::sync::atomic::AtomicU8;

/// The proxy rules a transport applies to its pooled requests.
pub enum ProxySource {
    /// HTTP_PROXY / HTTPS_PROXY / ALL_PROXY / NO_PROXY (uppercase first, then
    /// lowercase; nothing at all when REQUEST_METHOD is set), read once, now.
    /// oam's `process.env` writes never reach the OS environment, so reading
    /// it once per runtime sees what reqwest's per-client read saw.
    Env,
    /// These rules (tests). Boxed: a `Matcher` is 320 bytes.
    Fixed(Box<Matcher>),
    /// No proxy.
    None,
}

/// How to build a transport.
pub struct TransportOptions {
    pub tls: TlsSource,
    pub proxy: ProxySource,
    /// The `user-agent` default requests get and a CONNECT carries.
    pub user_agent: HeaderValue,
}

/// A runtime's fetch transport. Cloning shares the pool.
#[derive(Clone)]
pub struct HttpTransport {
    client: Client<OamConnector, ReqBody>,
    shared: Arc<Shared>,
}

impl Default for HttpTransport {
    fn default() -> Self {
        HttpTransport::new()
    }
}

impl HttpTransport {
    /// Platform TLS, the environment proxy, `user-agent: oam/<version>`.
    /// Infallible, and needs no runtime: building the pool spawns nothing.
    pub fn new() -> HttpTransport {
        HttpTransport::with_options(TransportOptions {
            tls: TlsSource::Platform,
            proxy: ProxySource::Env,
            user_agent: HeaderValue::from_static(concat!("oam/", env!("CARGO_PKG_VERSION"))),
        })
    }

    pub fn with_options(options: TransportOptions) -> HttpTransport {
        let proxy = match options.proxy {
            // client-proxy-system is off, so this is the environment alone.
            ProxySource::Env => Some(Matcher::from_system()),
            ProxySource::Fixed(matcher) => Some(*matcher),
            ProxySource::None => None,
        };
        let shared = Arc::new(Shared {
            tls: options.tls,
            proxy,
            user_agent: options.user_agent,
            attempt_timeout_ms: AtomicU64::new(
                u64::try_from(DEFAULT_ATTEMPT_TIMEOUT.as_millis()).unwrap_or(250),
            ),
            tls_range: AtomicU8::new(TlsRange::Both.code()),
        });
        let client = build_client(OamConnector {
            shared: shared.clone(),
            via: Via::Pooled,
        });
        HttpTransport { client, shared }
    }

    pub fn user_agent(&self) -> &HeaderValue {
        &self.shared.user_agent
    }

    /// The route one fetch takes. With `lookup_hook`, the fetch gets its own
    /// client, whose connector dials a host name only at the addresses
    /// recorded with [`Route::set_addrs`] and never through the environment
    /// proxy; its pool is its own and dies with the route. Otherwise the
    /// fetch shares the process pool.
    pub fn route(
        &self,
        lookup_hook: bool,
        attempt_timeout: Duration,
        tls_range: TlsRange,
    ) -> Route {
        let hooked = lookup_hook.then(|| {
            let addrs: HostAddrs = Arc::new(Mutex::new(HashMap::new()));
            let client = build_client(OamConnector {
                shared: self.shared.clone(),
                via: Via::Hooked {
                    addrs: addrs.clone(),
                    attempt_timeout,
                },
            });
            Hooked { addrs, client }
        });
        Route {
            attempt_timeout,
            tls_range,
            hooked,
            supplied: None,
        }
    }

    /// The route of one fetch whose undici dispatcher carries a `connect`
    /// FUNCTION: its own client, whose every connection is one JS supplied
    /// (recorded with [`Route::supply`]) from the socket that function
    /// returned. Nothing on it is pooled -- each hop parks for a connection
    /// of its own and uses it once -- so a supplied connection can never be
    /// reused for a request the dispatcher's function was not asked about.
    pub fn supplied_route(&self, attempt_timeout: Duration, tls_range: TlsRange) -> Route {
        let conns: SuppliedConns = Arc::new(Mutex::new(HashMap::new()));
        let client = Client::builder(TokioExecutor::new())
            .timer(TokioTimer::new())
            .pool_timer(TokioTimer::new())
            .pool_max_idle_per_host(0)
            .build(OamConnector {
                shared: self.shared.clone(),
                via: Via::Supplied {
                    conns: conns.clone(),
                },
            });
        Route {
            attempt_timeout,
            tls_range,
            hooked: None,
            supplied: Some(Supplied { conns, client }),
        }
    }

    /// Send one request (one hop) on `route`.
    pub async fn send(
        &self,
        route: &Route,
        mut request: http::Request<ReqBody>,
    ) -> Result<http::Response<Incoming>, SendError> {
        // Every route's connector handshakes through the shared state, so
        // the request's version range goes there whichever client sends it.
        self.shared.set_tls_range(route.tls_range);
        let client = match (&route.hooked, &route.supplied) {
            (Some(hooked), _) => &hooked.client,
            (None, Some(supplied)) => &supplied.client,
            (None, None) => {
                self.shared.set_attempt_timeout(route.attempt_timeout);
                &self.client
            }
        };
        // A request that told the server the connection closes after it --
        // node's `Connection: close`, which http.request sends for a socket it
        // does not keep -- must be the last one on that connection (RFC 9112
        // s9.6), whether or not the response says `close` back. hyper drops a
        // connection from the pool only when the RESPONSE says so, so it went
        // back and the next request could go out on a connection the server
        // was already closing: a POST failed with ECONNRESET, a GET was sent
        // twice. It is poisoned instead, as node destroys the socket. HTTP/1
        // only: an h2 connection carries every stream on it, and no
        // Connection header at all.
        let close_requested = request
            .headers()
            .get_all(http::header::CONNECTION)
            .iter()
            .any(|value| {
                value.to_str().is_ok_and(|value| {
                    value
                        .split(',')
                        .any(|token| token.trim().eq_ignore_ascii_case("close"))
                })
            });
        let capture = capture_connection(&mut request);
        let result = client.request(request).await;
        if close_requested
            && let Some(connected) = capture.connection_metadata().as_ref()
            && !connected.is_negotiated_h2()
        {
            connected.poison();
        }
        // Count this request in on the connection it went out on, whatever
        // the outcome, and learn whether an earlier request had used it and
        // whether any of a response arrived. No connection at all (a connect
        // that failed) is not a reused one, and no response started on it.
        let (reused, response_started) = match capture.connection_metadata().as_ref() {
            Some(connected) => {
                let mut extras = http::Extensions::new();
                connected.get_extras(&mut extras);
                match extras.get::<ConnStats>() {
                    Some(stats) => (stats.count_one(), stats.response_started()),
                    None => (false, true),
                }
            }
            None => (false, false),
        };
        result.map_err(|error| SendError {
            error,
            reused,
            response_started,
        })
    }

    /// The `proxy-authorization` value this hop needs: only an http request
    /// on a pooled route that the proxy rules send to an http(s) proxy with
    /// credentials. An https request carries the credentials in its CONNECT
    /// instead, and a hooked or supplied route never uses a proxy.
    pub fn proxy_authorization(&self, route: &Route, uri: &Uri) -> Option<HeaderValue> {
        if route.hooked.is_some() || route.supplied.is_some() || uri.scheme_str() != Some("http") {
            return None;
        }
        let intercept = self.shared.proxy.as_ref()?.intercept(uri)?;
        if !matches!(intercept.uri().scheme_str(), Some("http" | "https")) {
            return None;
        }
        intercept.basic_auth().cloned()
    }

    /// Whether the proxy rules would send a request for `url` on a pooled
    /// route through a proxy (any scheme, a refused socks one included). A
    /// URL that is not a URI answers true: the caller asks in order to stay
    /// off the proxy, so it then keeps off this transport altogether.
    pub fn env_proxied(&self, url: &str) -> bool {
        let Ok(uri) = url.parse::<Uri>() else {
            return true;
        };
        self.shared
            .proxy
            .as_ref()
            .is_some_and(|rules| rules.intercept(&uri).is_some())
    }
}

/// One client builder for both kinds of client.
///
/// Both pool. A hooked client's pool is scoped to its own fetch -- the
/// [`Route`] that owns it is dropped in `send::respond`, and its idle
/// connections die with it -- and hyper-util keys the pool on the request's
/// scheme and authority, so a pooled connection can only ever be reused for
/// the origin it was opened to. Without it a single hooked fetch through four
/// same-host redirects opened FIVE connections where node opens two
/// (measured), paying a TCP -- and over https a full TLS -- handshake per hop
/// for an authority whose hook-approved address set had not changed.
fn build_client(connector: OamConnector) -> Client<OamConnector, ReqBody> {
    Client::builder(TokioExecutor::new())
        .timer(TokioTimer::new())
        .pool_timer(TokioTimer::new())
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(usize::MAX)
        .build(connector)
}

/// How one fetch reaches the network.
pub struct Route {
    attempt_timeout: Duration,
    /// The TLS version range its https handshakes run in: node's live
    /// defaults as JS resolved them for this request.
    tls_range: TlsRange,
    hooked: Option<Hooked>,
    supplied: Option<Supplied>,
}

struct Hooked {
    addrs: HostAddrs,
    client: Client<OamConnector, ReqBody>,
}

struct Supplied {
    conns: SuppliedConns,
    client: Client<OamConnector, ReqBody>,
}

impl Route {
    /// The fetch has a `connect.lookup` hook.
    pub fn is_hooked(&self) -> bool {
        self.hooked.is_some()
    }

    /// On a hooked route, the authority the hook must resolve before `uri`
    /// can be dialled, as `(key, host)`: `None` for an IP literal (node never
    /// looks one up), for an authority this fetch already resolved (node
    /// reuses the connection it opened, and within one fetch its addresses
    /// stand), and on a pooled route.
    ///
    /// The key is the whole authority, not the host. A guard's policy can
    /// turn on the PORT -- allow 443 on an internal name, refuse 22 or 6379 --
    /// and while the map was keyed on the host alone a 302 to the same name
    /// on another port was followed without asking it again. node asks per
    /// connection, so it asks for the new authority too.
    pub fn lookup_needed(&self, uri: &Uri) -> Option<(String, String)> {
        let hooked = self.hooked.as_ref()?;
        let host = host_for_connect(uri)?;
        if host.parse::<IpAddr>().is_ok() {
            return None;
        }
        let key = authority_key(uri)?;
        let known = hooked
            .addrs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&key);
        (!known).then_some((key, host))
    }

    /// On a supplied route, the authority key `uri` needs a connection for:
    /// `None` when one JS supplied is waiting for it, and on every other
    /// route. Every hop asks, IP literals included -- undici calls its
    /// connector for every connection, whatever the host.
    pub fn connection_needed(&self, uri: &Uri) -> Option<String> {
        let supplied = self.supplied.as_ref()?;
        let key = authority_key(uri)?;
        let waiting = supplied
            .conns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .is_some_and(|queue| !queue.is_empty());
        (!waiting).then_some(key)
    }

    /// Record a connection JS supplied under the `key`
    /// [`Route::connection_needed`] returned (no-op on another route, which
    /// drops it).
    pub(crate) fn supply(&self, key: &str, conn: SuppliedConn) {
        if let Some(supplied) = &self.supplied {
            supplied
                .conns
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(key.to_string())
                .or_default()
                .push(conn);
        }
    }

    /// Record the hook's addresses under the `key` [`Route::lookup_needed`]
    /// returned (no-op on a pooled route).
    pub fn set_addrs(&self, key: &str, addrs: Vec<IpAddr>) {
        if let Some(hooked) = &self.hooked {
            hooked
                .addrs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.to_string(), addrs);
        }
    }
}

/// A request that produced no response.
#[derive(Debug)]
pub struct SendError {
    error: hyper_util::client::legacy::Error,
    /// It went out on a connection an earlier request had already used.
    reused: bool,
    /// Some part of a response arrived on that connection after this request
    /// took it (see `ConnStats`).
    response_started: bool,
}

impl SendError {
    /// The connect failure behind this error, if a connect failed: through
    /// hyper-util's error and, for a proxied https request, through the
    /// tunnel's -- a proxy that refused or did not resolve is named in it.
    pub fn connect_error(&self) -> Option<&ConnectError> {
        find_in_chain::<ConnectError>(&self.error)
    }

    /// The op outcome for this failure of the hop to `url`:
    /// - a connect failure is node's error: `NodeFailed` for a resolver
    ///   failure or a single refused address, `NodeAggregateFailed` for a
    ///   multi-address connect;
    /// - TLS that could not be configured is `tls configuration error: ...`;
    /// - everything else -- a TLS handshake or verification failure, a proxy
    ///   that refused the CONNECT, an unsupported proxy scheme, a reset
    ///   before the response head, an aborted streamed body -- keeps
    ///   reqwest's uncoded `error sending request for url (...)`, which
    ///   node_compat's http client maps to `socket hang up`.
    pub fn to_outcome(&self, url: &url::Url) -> OpOutcome {
        if let Some(connect) = self.connect_error() {
            return connect.to_outcome();
        }
        // A certificate refused in Node's terms (tls_config's
        // NodeNamedRefusals): its code and message, as tls.connect reports
        // them.
        if let Some(refusal) = node_cert_refusal(&self.error)
            && let Some(code) = refusal.code
        {
            return OpOutcome::node_failed(code, refusal.message.clone());
        }
        // A version range with nothing to offer (node's live defaults leave
        // none): the code tls.connect reports for the same range.
        if let Some(none) = find_in_chain::<NoProtocolsAvailable>(&self.error) {
            return OpOutcome::node_failed("ERR_SSL_NO_PROTOCOLS_AVAILABLE", none.to_string());
        }
        // A fatal alert the server sent (#196). The two callers of this
        // transport shape it differently, as node's two do, so the outcome
        // carries the alert and the JS side -- which knows whether it is a
        // `fetch` or an option-less `https.get` -- finishes it:
        // - `fetch` reports the alert as the request's `cause`, code and
        //   all, whether it answered the handshake or came after it;
        // - `https.get` fails a handshake-time alert with `write EPROTO`
        //   (its request head was a write queued behind the handshake, which
        //   OpenSSL then refused; measured for every alert, #146) and keeps
        //   a post-handshake alert's own code.
        // The `handshake failed:` message prefix marks a handshake-time
        // alert for the JS; a description node cannot name is `ECONNRESET`
        // (its disconnect, which `fetch` keeps) with the same prefix and the
        // `received fatal alert` text a transport EOF lacks, so `https.get`
        // can still tell it from a close and make it `write EPROTO`.
        if let Some(alert) = received_alert(&self.error) {
            let handshake = find_in_chain::<HandshakeFailed>(&self.error).is_some();
            let detail = format!("received fatal alert: {alert:?}");
            let message = if handshake {
                format!("handshake failed: {detail}")
            } else {
                detail
            };
            return match crate::tls::alert_code(alert) {
                Some(code) => OpOutcome::node_failed(code, message),
                None if handshake => OpOutcome::node_failed("ECONNRESET", message),
                None => OpOutcome::node_failed("EPROTO", message),
            };
        }
        // The transport closed before the handshake was done: node's
        // disconnect, with its own message (the JS side adds the options
        // the request dialled with).
        if let Some(handshake) = find_in_chain::<HandshakeFailed>(&self.error)
            && handshake.0.kind() == std::io::ErrorKind::UnexpectedEof
        {
            return crate::tls::disconnected_before_secure();
        }
        if let Some(tls) = find_in_chain::<TlsSetupError>(&self.error) {
            return OpOutcome::Failed(tls.to_string());
        }
        OpOutcome::Failed(format!("error sending request for url ({url})"))
    }

    /// hyper's `IncompleteMessage`, "connection closed before message
    /// completed": the connection died before a WHOLE response head arrived.
    /// hyper reports it the same way whether nothing arrived or half a head
    /// did (h1 io.rs `parse`: EOF is `new_incomplete` either way), so pair
    /// it with [`SendError::response_started`]. hyper-util retries a request
    /// its dispatcher handed back UNSENT (`retry_canceled_requests`, on by
    /// default) but not this one, where the request had already gone onto
    /// the connection.
    ///
    /// Why oam hits it where node does not: a server that advertises
    /// keep-alive and then FINs (the idle-timeout shape) leaves a pooled
    /// connection that the next request can be written onto before the FIN
    /// arrives, because oam's redirect loop stays in Rust with no event-loop
    /// tick between the 3xx and the hop. A FIN the kernel already holds is
    /// caught before the write (`connector::EagerTcp`); one still in flight
    /// is not, in oam or in node.
    pub fn is_incomplete_message(&self) -> bool {
        find_in_chain::<hyper::Error>(&self.error).is_some_and(|e| e.is_incomplete_message())
    }

    /// Some part of a response -- even a few bytes of a status line --
    /// arrived for this request before its connection failed. The server had
    /// then started answering, so the request is not one it ignored; RFC 9110
    /// s9.2.2's example of a retry worth guessing at is a connection that
    /// "closed before any part of a response is received". Also true when
    /// oam cannot tell.
    pub fn response_started(&self) -> bool {
        self.response_started
    }

    /// The request went out on a connection an earlier request had already
    /// used -- a pooled one. Only such a connection can have been closed by
    /// the server while it sat idle; a FRESH connection that dies before the
    /// response is the server's answer, and sending the request again would
    /// deliver it twice where node delivers it once (measured: a server that
    /// closes every connection unanswered sees a GET once from node).
    pub fn on_reused_connection(&self) -> bool {
        self.reused
    }

    /// reqwest's retry classification (retry.rs:303-313): the server refused
    /// the stream before processing it (REFUSED_STREAM) or is shutting the
    /// connection down gracefully (GOAWAY with NO_ERROR). Either way the
    /// request was not processed and may be sent again (RFC 9113 s8.7).
    pub fn is_h2_retryable(&self) -> bool {
        find_in_chain::<h2::Error>(&self.error).is_some_and(|e| {
            e.is_remote()
                && ((e.is_go_away() && e.reason() == Some(h2::Reason::NO_ERROR))
                    || (e.is_reset() && e.reason() == Some(h2::Reason::REFUSED_STREAM)))
        })
    }
}

impl std::fmt::Display for SendError {
    /// The whole source chain, for tests and debugging.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut current: Option<&(dyn std::error::Error + 'static)> = Some(&self.error);
        let mut first = true;
        while let Some(error) = current {
            if !first {
                f.write_str(" -> ")?;
            }
            first = false;
            write!(f, "{error}")?;
            current = error.source();
        }
        Ok(())
    }
}

impl std::error::Error for SendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// The Node-named certificate refusal in `error`'s chain, if the handshake
/// failed with one. tokio-rustls hands the rustls error over as an
/// `io::Error`'s payload, which `source()` does not reach -- hence the
/// `get_ref`.
fn node_cert_refusal<'a>(
    error: &'a (dyn std::error::Error + 'static),
) -> Option<&'a crate::tls::VerifyFailure> {
    let io = find_in_chain::<std::io::Error>(error)?;
    let rustls::Error::InvalidCertificate(rustls::CertificateError::Other(other)) =
        io.get_ref()?.downcast_ref::<rustls::Error>()?
    else {
        return None;
    };
    other
        .0
        .downcast_ref::<crate::tls::NodeCertRefusal>()
        .map(|refusal| &refusal.0)
}

/// The fatal alert a handshake or a read in this error's chain received, if
/// one did. tokio-rustls wraps the `rustls::Error` in an `io::Error` whose
/// `source()` is the rustls error's own (none), not the rustls error, so
/// the chain walk cannot see it: each io error on the way is opened with
/// `get_ref` instead (`tls::received_alert`), as tls.connect's is.
fn received_alert(error: &(dyn std::error::Error + 'static)) -> Option<rustls::AlertDescription> {
    let mut current = Some(error);
    while let Some(e) = current {
        if let Some(rustls::Error::AlertReceived(alert)) = e.downcast_ref::<rustls::Error>() {
            return Some(*alert);
        }
        if let Some(io) = e.downcast_ref::<std::io::Error>()
            && let Some(alert) = crate::tls::received_alert(io)
        {
            return Some(alert);
        }
        current = e.source();
    }
    None
}

fn find_in_chain<'a, T: std::error::Error + 'static>(
    error: &'a (dyn std::error::Error + 'static),
) -> Option<&'a T> {
    let mut current = Some(error);
    while let Some(e) = current {
        if let Some(found) = e.downcast_ref::<T>() {
            return Some(found);
        }
        current = e.source();
    }
    None
}

/// A request with no body.
pub fn empty_body() -> ReqBody {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

/// A buffered request body (sent with `content-length`).
pub fn full_body(bytes: Bytes) -> ReqBody {
    Full::new(bytes).map_err(|never| match never {}).boxed()
}

/// A request body streamed from JS through an outbound body channel (sent
/// chunked). An `Err` item aborts the request (`fetchBodyChannelCancel`).
pub fn channel_body(rx: tokio::sync::mpsc::Receiver<Result<Vec<u8>, String>>) -> ReqBody {
    let chunks = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| {
            (
                item.map(|chunk| Frame::data(Bytes::from(chunk)))
                    .map_err(BoxError::from),
                rx,
            )
        })
    });
    StreamBody::new(chunks).boxed()
}
