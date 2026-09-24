//! oam's own HTTP connection pool for the fetch transport (#216).
//!
//! It replaces hyper-util's legacy `Client`, which -- on a cache-miss -- raced
//! a fresh connect against a pooled checkout and, when the pooled one won, drove
//! the started connect to completion and parked it idle having carried no
//! request. That spare held a `node:http` server's `close()` for the pool's 90 s
//! idle timeout, where undici (node's fetch) never opens a connection except to
//! send on it.
//!
//! This pool is **reuse-XOR-connect**: a request either takes an idle
//! connection or opens exactly one it will send on -- there is no spare. It also
//! gives `agent.destroy()` real teeth (divergence 38): [`Pool::destroy`] drops
//! every pooled sender, which ends its connection.
//!
//! Built on the low-level `hyper::client::conn::{http1,http2}` dispatchers over
//! [`OamConnector`] (reused verbatim), the same way `bridge.rs` and
//! `h2_session.rs` drive a single connection. The mechanics that matter:
//!
//! - **Liveness is the send handback, never `is_ready()`.** A FIN'd but
//!   not-yet-dropped connection still reports `is_ready() == true`
//!   (`http_client_stale_pool.rs`), so a checked-out connection is proven live
//!   only by `try_send_request`: if it hands the request back unsent
//!   (`TrySendError::take_message()`), the pool re-dials (hyper-util's
//!   `retry_canceled_requests`), gated on the connection having been reused. A
//!   request already on the wire (`is_incomplete_message`) is NOT retried here;
//!   it is surfaced for `send.rs`'s single idempotent stale-resend, so the two
//!   layers never double-send.
//! - **Parking is driven by the sender becoming ready again**, not by the
//!   response body: after an `Ok` send, the h1 sender is parked once its
//!   `poll_ready` re-arms (the dispatcher has drained the response). A dropped
//!   or half-read body errors that and closes the connection instead; a
//!   `content-length: 0` body re-arms at once and is pooled, keeping same-origin
//!   redirect reuse.
//! - **`ConnStats` is cloned at checkout** to snapshot the response-byte count,
//!   so `SendError::response_started` has its baseline and `send.rs`'s resend
//!   stays gated.
//! - **`ConnInfo` is re-attached** to every response (`res.extensions_mut()`),
//!   where hyper-util copied it, so `req.socket` facts survive.
//! - h2 multiplexes one connection per origin (its `SendRequest` is cloned per
//!   stream) and is evicted on a connection-level error so a retry re-dials.

use std::collections::{HashMap, VecDeque};
use std::future::poll_fn;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http::uri::{Authority, Parts, Scheme};
use http::{Method, Request, Response, Uri};
use hyper::body::Incoming;
use hyper::client::conn::{http1, http2};
use hyper_util::rt::TokioExecutor;

use super::connector::{ConnInfo, ConnStats, OamConnector};
use super::{BoxError, ReqBody};

/// The pool is keyed on scheme + authority exactly as hyper-util was, so a
/// pooled connection is only ever reused for the origin it was opened to and
/// `host` and `host:443` stay distinct.
type PoolKey = (Scheme, Authority);
type H1Sender = http1::SendRequest<ReqBody>;
type H2Sender = http2::SendRequest<ReqBody>;

/// hyper-util's `pool_idle_timeout`, the reqwest default oam inherited.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// The reaper never ticks faster than this (hyper-util's `MIN_CHECK`).
const MIN_REAP_TICK: Duration = Duration::from_millis(90);
/// At most one retry after a reused connection hands a request back unsent; a
/// fresh connection is never retried, so the loop settles in two.
const MAX_ATTEMPTS: usize = 3;

/// A runtime's owned connection pool. Cloning shares the connections.
#[derive(Clone)]
pub(crate) struct Pool {
    connector: OamConnector,
    inner: Arc<PoolInner>,
}

struct PoolInner {
    /// Parked HTTP/1 senders, newest at the back.
    idle: Mutex<HashMap<PoolKey, VecDeque<IdleH1>>>,
    /// One multiplexed HTTP/2 sender per origin.
    h2: Mutex<HashMap<PoolKey, H2Entry>>,
    /// `Some(90s)` for a pooling client; `None` for the supplied route, which
    /// never parks and never reuses (each supplied connection is used once).
    idle_timeout: Option<Duration>,
    /// Bumped by [`Pool::destroy`]; a connection from an older generation is
    /// never re-parked.
    generation: AtomicU64,
    /// The idle reaper is spawned once, on the first park.
    reaper_started: AtomicBool,
}

struct IdleH1 {
    sender: H1Sender,
    /// The pool's own stats copy (shared counters, no checkout baseline).
    stats: ConnStats,
    info: ConnInfo,
    proxied: bool,
    parked_at: Instant,
}

struct H2Entry {
    sender: H2Sender,
    stats: ConnStats,
    info: ConnInfo,
    proxied: bool,
}

impl Pool {
    /// A pooling client (`Some(idle_timeout)`), or a non-pooling one (`None`)
    /// for the supplied route.
    pub(crate) fn new(connector: OamConnector, idle_timeout: Option<Duration>) -> Pool {
        Pool {
            connector,
            inner: Arc::new(PoolInner {
                idle: Mutex::new(HashMap::new()),
                h2: Mutex::new(HashMap::new()),
                idle_timeout,
                generation: AtomicU64::new(0),
                reaper_started: AtomicBool::new(false),
            }),
        }
    }

    /// A pooling client with hyper-util's 90 s idle timeout.
    pub(crate) fn pooled(connector: OamConnector) -> Pool {
        Pool::new(connector, Some(DEFAULT_IDLE_TIMEOUT))
    }

    /// Drop every pooled connection: `agent.destroy()`. Any in-flight request
    /// keeps its own sender and finishes, but on an older generation, so its
    /// connection is not re-parked.
    pub(crate) fn destroy(&self) {
        self.inner.generation.fetch_add(1, Ordering::Relaxed);
        self.inner
            .idle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.inner
            .h2
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Send one request on this pool. `close_requested` is set when the request
    /// carried `Connection: close` (h1), so its connection is not re-parked.
    pub(crate) async fn request(
        &self,
        mut req: Request<ReqBody>,
        close_requested: bool,
    ) -> Result<Response<Incoming>, PoolFail> {
        let Some(key) = pool_key(req.uri()) else {
            return Err(PoolFail {
                error: PoolError::connect(Box::<dyn std::error::Error + Send + Sync>::from(
                    "request url has no scheme or authority",
                )),
                reused: false,
                response_started: false,
            });
        };
        let is_connect = req.method() == Method::CONNECT;
        // hyper-util saves the request's absolute URI and restores it before
        // each attempt's origin/absolute rewrite, so a retry does not rewrite an
        // already-rewritten URI.
        let original_uri = req.uri().clone();
        let mut allow_reuse = true;

        for _ in 0..MAX_ATTEMPTS {
            let conn = match self.checkout(&key, allow_reuse).await {
                Ok(conn) => conn,
                Err(error) => {
                    return Err(PoolFail {
                        error: PoolError::connect(error),
                        reused: false,
                        response_started: false,
                    });
                }
            };
            // `attempt_stats` carries this attempt's `read` baseline, snapshotted
            // at checkout before any byte of its response could arrive.
            let Conn {
                proto,
                stats: attempt_stats,
                info,
                proxied,
                is_h2,
                key: conn_key,
            } = conn;
            *req.uri_mut() = original_uri.clone();
            set_host_header(&mut req, is_h2);
            rewrite_request_uri(req.uri_mut(), is_h2, proxied, is_connect);

            match send_on(proto, req).await {
                SendResult::Ok(mut response, proto) => {
                    // Count this use so the connection's next checkout reports it
                    // as reused (the result is only needed on a failure).
                    attempt_stats.count_one();
                    response.extensions_mut().insert(info.clone());
                    self.on_success(proto, conn_key, info, proxied, &response, close_requested);
                    return Ok(response);
                }
                SendResult::Unsent(returned, error, proto) => {
                    let reused = attempt_stats.count_one();
                    let response_started = attempt_stats.response_started();
                    self.on_failure(proto, is_h2, &conn_key);
                    if reused && allow_reuse {
                        // A pooled connection handed the request back unsent:
                        // re-dial and send it on a fresh one (retry_canceled).
                        req = returned;
                        allow_reuse = false;
                        continue;
                    }
                    return Err(PoolFail {
                        error: PoolError::send(error),
                        reused,
                        response_started,
                    });
                }
                SendResult::Sent(error, proto) => {
                    let reused = attempt_stats.count_one();
                    let response_started = attempt_stats.response_started();
                    self.on_failure(proto, is_h2, &conn_key);
                    return Err(PoolFail {
                        error: PoolError::send(error),
                        reused,
                        response_started,
                    });
                }
            }
        }

        // Unreachable: a fresh connection is never retried, so at most two
        // attempts run. Surface a canceled error rather than panic.
        Err(PoolFail {
            error: PoolError::connect(Box::<dyn std::error::Error + Send + Sync>::from(
                "connection retries exhausted",
            )),
            reused: false,
            response_started: false,
        })
    }

    /// Reuse an idle connection for `key`, or open exactly one. Never both.
    async fn checkout(&self, key: &PoolKey, allow_reuse: bool) -> Result<Conn, BoxError> {
        if allow_reuse && self.inner.idle_timeout.is_some() {
            if let Some(conn) = self.reuse_h2(key) {
                return Ok(conn);
            }
            if let Some(conn) = self.reuse_h1(key) {
                return Ok(conn);
            }
        }
        self.connect(key).await
    }

    fn reuse_h2(&self, key: &PoolKey) -> Option<Conn> {
        let mut map = self.inner.h2.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.get(key)?;
        // A dead h2 connection is dropped, not handed out; the send handback is
        // still the authoritative guard for one whose GOAWAY is in flight.
        if entry.sender.is_closed() {
            map.remove(key);
            return None;
        }
        Some(Conn {
            proto: Proto::H2(entry.sender.clone()),
            stats: entry.stats.clone(),
            info: entry.info.clone(),
            proxied: entry.proxied,
            is_h2: true,
            key: key.clone(),
        })
    }

    fn reuse_h1(&self, key: &PoolKey) -> Option<Conn> {
        let mut map = self.inner.idle.lock().unwrap_or_else(|e| e.into_inner());
        let deque = map.get_mut(key)?;
        while let Some(entry) = deque.pop_front() {
            if let Some(timeout) = self.inner.idle_timeout
                && Instant::now().duration_since(entry.parked_at) > timeout
            {
                continue; // expired -> drop, close
            }
            if entry.sender.is_closed() {
                continue; // a processed FIN; drop (lazy skip)
            }
            let stats = entry.stats.clone();
            let conn = Conn {
                proto: Proto::H1(entry.sender, entry.stats),
                stats,
                info: entry.info,
                proxied: entry.proxied,
                is_h2: false,
                key: key.clone(),
            };
            if deque.is_empty() {
                map.remove(key);
            }
            return Some(conn);
        }
        map.remove(key);
        None
    }

    async fn connect(&self, key: &PoolKey) -> Result<Conn, BoxError> {
        let uri = domain_as_uri(key);
        let conn = self.connector.clone().connect(uri).await?;
        let is_h2 = conn.negotiated_h2();
        let proxied = conn.is_proxied();
        let pool_stats = conn.pool_stats();
        let info = conn.conn_info();
        let stats = pool_stats.clone();
        if is_h2 {
            let (sender, connection) = http2::handshake(TokioExecutor::new(), conn)
                .await
                .map_err(|e| Box::new(e) as BoxError)?;
            tokio::spawn(async move {
                let _ = connection.await;
            });
            self.inner
                .h2
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    key.clone(),
                    H2Entry {
                        sender: sender.clone(),
                        stats: pool_stats,
                        info: info.clone(),
                        proxied,
                    },
                );
            Ok(Conn {
                proto: Proto::H2(sender),
                stats,
                info,
                proxied,
                is_h2: true,
                key: key.clone(),
            })
        } else {
            let (sender, connection) = http1::handshake(conn)
                .await
                .map_err(|e| Box::new(e) as BoxError)?;
            tokio::spawn(async move {
                let _ = connection.with_upgrades().await;
            });
            Ok(Conn {
                proto: Proto::H1(sender, pool_stats),
                stats,
                info,
                proxied,
                is_h2: false,
                key: key.clone(),
            })
        }
    }

    /// After an `Ok` send: park an h1 connection once its sender re-arms (the
    /// response drained) unless the request or response asked to close it; an
    /// h2 connection stays in its entry for the next stream.
    fn on_success(
        &self,
        sender: Proto,
        key: PoolKey,
        info: ConnInfo,
        proxied: bool,
        response: &Response<Incoming>,
        close_requested: bool,
    ) {
        let Proto::H1(mut sender, stats) = sender else {
            return; // h2: the entry is already in the map
        };
        let should_park = !close_requested && !says_close(response.headers());
        let inner = Arc::downgrade(&self.inner);
        let gen_id = self.inner.generation.load(Ordering::Relaxed);
        tokio::spawn(async move {
            // Wait until the dispatcher wants the next request -- it has drained
            // the response body -- or errors (a dropped/half-read body, or a
            // dead connection), in which case the sender drops and closes it.
            if !sender.is_ready() && poll_fn(|cx| sender.poll_ready(cx)).await.is_err() {
                return;
            }
            if !should_park {
                return; // Connection: close -> the body drained, now close
            }
            if let Some(inner) = inner.upgrade()
                && inner.generation.load(Ordering::Relaxed) == gen_id
            {
                inner
                    .idle
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entry(key)
                    .or_default()
                    .push_back(IdleH1 {
                        sender,
                        stats,
                        info,
                        proxied,
                        parked_at: Instant::now(),
                    });
                inner.ensure_reaper();
            }
        });
    }

    /// After a failed send: an h1 connection's sender drops (closing it); an h2
    /// connection is evicted if the failure closed it, so a retry re-dials.
    fn on_failure(&self, sender: Proto, is_h2: bool, key: &PoolKey) {
        if is_h2
            && let Proto::H2(sender) = &sender
            && sender.is_closed()
        {
            let mut map = self.inner.h2.lock().unwrap_or_else(|e| e.into_inner());
            if map.get(key).is_some_and(|e| e.sender.is_closed()) {
                map.remove(key);
            }
        }
        // h1: dropping `sender` here closes the connection.
    }
}

impl PoolInner {
    /// Spawn the idle reaper the first time a connection is parked: one task per
    /// pool that drops expired or dead idle entries and self-cancels when the
    /// pool is gone. Lazy skip-on-checkout handles the rest.
    fn ensure_reaper(self: &Arc<Self>) {
        let Some(timeout) = self.idle_timeout else {
            return;
        };
        if self.reaper_started.swap(true, Ordering::Relaxed) {
            return;
        }
        let weak = Arc::downgrade(self);
        let tick = timeout.max(MIN_REAP_TICK);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tick).await;
                match weak.upgrade() {
                    Some(inner) => inner.clear_expired(),
                    None => break,
                }
            }
        });
    }

    fn clear_expired(&self) {
        let Some(timeout) = self.idle_timeout else {
            return;
        };
        let now = Instant::now();
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        idle.retain(|_, deque| {
            deque.retain(|entry| {
                now.duration_since(entry.parked_at) <= timeout && !entry.sender.is_closed()
            });
            !deque.is_empty()
        });
        drop(idle);
        let mut h2 = self.h2.lock().unwrap_or_else(|e| e.into_inner());
        h2.retain(|_, entry| !entry.sender.is_closed());
    }
}

/// A checked-out connection, ready for one request.
struct Conn {
    proto: Proto,
    /// This attempt's stats copy (its `read` baseline snapshotted at checkout).
    stats: ConnStats,
    info: ConnInfo,
    proxied: bool,
    is_h2: bool,
    key: PoolKey,
}

enum Proto {
    /// An owned h1 sender and the pool's stats copy to re-park it with.
    H1(H1Sender, ConnStats),
    /// A clone of the origin's multiplexed h2 sender.
    H2(H2Sender),
}

/// What one `try_send_request` produced, with the sender handed back so the
/// caller can park it (`Ok`) or drop / evict it (failure).
enum SendResult {
    Ok(Response<Incoming>, Proto),
    /// The request was handed back unsent (`take_message`): safe to re-dial.
    Unsent(Request<ReqBody>, hyper::Error, Proto),
    /// The error came after bytes were on the wire: for `send.rs` to classify.
    Sent(hyper::Error, Proto),
}

/// Send one request on a checked-out connection, handing the sender back so the
/// caller can park (`Ok`), drop, or evict it. `try_send_request` (never
/// `send_request`) is the only call that returns the request unsent.
async fn send_on(proto: Proto, req: Request<ReqBody>) -> SendResult {
    match proto {
        Proto::H1(mut sender, stats) => match sender.try_send_request(req).await {
            Ok(response) => SendResult::Ok(response, Proto::H1(sender, stats)),
            Err(mut err) => match err.take_message() {
                Some(returned) => {
                    SendResult::Unsent(returned, err.into_error(), Proto::H1(sender, stats))
                }
                None => SendResult::Sent(err.into_error(), Proto::H1(sender, stats)),
            },
        },
        Proto::H2(mut sender) => match sender.try_send_request(req).await {
            Ok(response) => SendResult::Ok(response, Proto::H2(sender)),
            Err(mut err) => match err.take_message() {
                Some(returned) => SendResult::Unsent(returned, err.into_error(), Proto::H2(sender)),
                None => SendResult::Sent(err.into_error(), Proto::H2(sender)),
            },
        },
    }
}

/// A request that produced no response, plus the two signals `send.rs`'s
/// stale-resend is gated on.
pub(crate) struct PoolFail {
    pub(crate) error: PoolError,
    pub(crate) reused: bool,
    pub(crate) response_started: bool,
}

/// The transport's send error, in place of `hyper_util::client::legacy::Error`.
/// Its `source()` reproduces the whole chain the send path classifies: a
/// connect failure boxes the connector's error (a `ConnectError`, a
/// `HandshakeFailed`/`io::Error` for TLS, `NoProtocolsAvailable`, a
/// `TlsSetupError`); a dispatch failure carries the `hyper::Error` (an
/// `is_incomplete_message`, or an `h2::Error` reachable through it).
#[derive(Debug)]
pub struct PoolError {
    kind: PoolErrorKind,
}

#[derive(Debug)]
enum PoolErrorKind {
    Connect(BoxError),
    Send(hyper::Error),
}

impl PoolError {
    fn connect(error: BoxError) -> PoolError {
        PoolError {
            kind: PoolErrorKind::Connect(error),
        }
    }

    fn send(error: hyper::Error) -> PoolError {
        PoolError {
            kind: PoolErrorKind::Send(error),
        }
    }
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            PoolErrorKind::Connect(_) => f.write_str("error connecting for url"),
            PoolErrorKind::Send(_) => f.write_str("error sending request for url"),
        }
    }
}

impl std::error::Error for PoolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            PoolErrorKind::Connect(error) => Some(&**error),
            PoolErrorKind::Send(error) => Some(error),
        }
    }
}

/// The pool key for a request URI: `(scheme, authority)`, as written.
fn pool_key(uri: &Uri) -> Option<PoolKey> {
    Some((uri.scheme()?.clone(), uri.authority()?.clone()))
}

/// `scheme://authority/` -- the dst the connector dials for `key` (hyper-util's
/// `domain_as_uri`).
fn domain_as_uri((scheme, authority): &PoolKey) -> Uri {
    Uri::builder()
        .scheme(scheme.clone())
        .authority(authority.clone())
        .path_and_query("/")
        .build()
        .expect("scheme and authority make a valid uri")
}

/// Does the header map carry `Connection: close`?
fn says_close(headers: &http::HeaderMap) -> bool {
    headers
        .get_all(http::header::CONNECTION)
        .iter()
        .any(|value| {
            value.to_str().is_ok_and(|value| {
                value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("close"))
            })
        })
}

/// Rewrite the request URI for the connection it goes out on, exactly as
/// hyper-util does. An h2 request keeps its absolute URI, from which the h2
/// layer builds `:authority`; hyper-util rewrites only h1. Over h1: CONNECT
/// keeps only the authority; an http request through a proxy keeps the absolute
/// form; everything else is stripped to origin form.
fn rewrite_request_uri(uri: &mut Uri, is_h2: bool, proxied: bool, is_connect: bool) {
    if is_h2 {
        return;
    }
    if is_connect {
        authority_form(uri);
    } else if proxied {
        // absolute_form: the URI already carries scheme + authority.
    } else {
        origin_form(uri);
    }
}

/// Add the `Host` header from the request's authority if it has none, as
/// hyper-util's `set_host` did before it stripped the authority to origin form
/// (h1 only; an h2 request carries `:authority` and no `Host`). `prepare.rs`
/// relies on this -- a redirect hop is built without a `Host`.
fn set_host_header(req: &mut Request<ReqBody>, is_h2: bool) {
    if is_h2 || req.headers().contains_key(http::header::HOST) {
        return;
    }
    let uri = req.uri().clone();
    let Some(hostname) = uri.host() else {
        return;
    };
    let value = match non_default_port(&uri) {
        Some(port) => format!("{hostname}:{port}"),
        None => hostname.to_string(),
    };
    if let Ok(header) = http::HeaderValue::from_str(&value) {
        req.headers_mut().insert(http::header::HOST, header);
    }
}

/// The URI's port unless it is the scheme's default (hyper-util's
/// `get_non_default_port`), so a `Host` header omits `:80` / `:443`.
fn non_default_port(uri: &Uri) -> Option<u16> {
    let port = uri.port_u16()?;
    let secure = matches!(uri.scheme_str(), Some("https" | "wss"));
    match (port, secure) {
        (443, true) | (80, false) => None,
        _ => Some(port),
    }
}

fn origin_form(uri: &mut Uri) {
    let path = match uri.path_and_query() {
        Some(path) if path.as_str() != "/" => {
            let mut parts = Parts::default();
            parts.path_and_query = Some(path.clone());
            Uri::from_parts(parts).expect("a path is a valid uri")
        }
        _ => Uri::default(),
    };
    *uri = path;
}

fn authority_form(uri: &mut Uri) {
    if let Some(authority) = uri.authority() {
        let mut parts = Parts::default();
        parts.authority = Some(authority.clone());
        *uri = Uri::from_parts(parts).expect("an authority is a valid uri");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A marker error the send path's classifiers look for, to prove
    /// `PoolError::source()` reproduces the chain `find_in_chain` walks. The
    /// connector's real errors (`ConnectError`, `HandshakeFailed`,
    /// `NoProtocolsAvailable`, `TlsSetupError`) reach the send path the same
    /// way, exercised end to end by `tests/http_client_transport.rs`.
    #[derive(Debug)]
    struct Marker;

    impl std::fmt::Display for Marker {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("marker")
        }
    }

    impl std::error::Error for Marker {}

    fn reaches<T: std::error::Error + 'static>(error: &(dyn std::error::Error + 'static)) -> bool {
        let mut current = Some(error);
        while let Some(e) = current {
            if e.downcast_ref::<T>().is_some() {
                return true;
            }
            current = e.source();
        }
        false
    }

    #[test]
    fn a_connect_pool_error_exposes_its_inner_error_to_the_classifiers() {
        let error = PoolError::connect(Box::new(Marker));
        assert!(
            reaches::<Marker>(&error),
            "PoolError::source() must reach the connector's error, or the send \
             path misclassifies a connect / TLS failure as a generic send error",
        );
    }
}
