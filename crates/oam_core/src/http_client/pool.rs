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

use super::connector::{
    ConnCloser, ConnInfo, ConnStats, DialParams, HookPin, NeedsLookup, OamConnector,
};
use super::redirect::{self, Rules};
use super::sent::Dispatched;
use super::tls_config::Alpn;
use super::transport::HeadAwaitsBody;
use super::{BoxError, ReqBody};

/// The pool is keyed on scheme + authority exactly as hyper-util was, so a
/// pooled connection is only ever reused for the origin it was opened to and
/// `host` and `host:443` stay distinct -- and, for https, on the ALPN offer
/// it was opened with: a request that offers `http/1.1` alone never rides
/// an h2 connection an `allowH2` request opened to the same origin, and
/// fetch and `https.request` keep to their own connections, as undici's pool
/// and an https.Agent do in node. (An http origin offers nothing; its key
/// carries [`Alpn::None`] whoever asks.)
type PoolKey = (Scheme, Authority, Alpn);
type H1Sender = http1::SendRequest<ReqBody>;
type H2Sender = http2::SendRequest<ReqBody>;

/// One reactor turn before a just-parked h1 connection is handed out (#155).
/// A server that answers and FINs in one callback (a 3xx then `end()`, a
/// keep-alive timeout) makes a connection parked moments ago the one whose
/// FIN can still be in flight: a request written at once races it, and
/// either lands in a close the server never reads (a double delivery after
/// oam's one resend) or fails a request oam may not send twice (RFC 9110
/// s9.2.2). The checkout sleeps until the entry is [`FRESH_GATE`] old, which
/// parks the send task on the reactor: the connection's own dispatcher polls
/// meanwhile -- hyper reads an idle connection before every write, and
/// `EagerTcp`'s peek asks the kernel -- so a FIN that lands during the wait
/// closes the entry, [`Pool::reuse_h1`] skips it, and the request dials
/// fresh instead of racing. Node takes a turn of its own here (its event
/// loop reads the FIN before it writes); this is oam's. A timing heuristic,
/// swept against node (issue #155's tables) from 0 to 5 ms on Windows
/// arm64: 0 reproduces the gap (0-51 doubles and 9-31 failures per 1,000),
/// 1 ms onward measured at node's 0/0 across all three open rows, so 1 ms
/// is the smallest clean window. tokio's timer rounds the wait up to its ms
/// tick, which matches the constant. No env knob: the sweep is done, and a
/// gate a deployment can silently disable is a gate that rots.
///
/// h1 only: an [`H2Entry`] carries no park time (no `parked_at`), so an h2
/// reuse is unchanged -- gating one would add a field, not a flag. The cost
/// falls only on a connection parked within the window: 1,000 sequential
/// fetches measured 0.5 s -> 2.7 s with the gate (+2.2 ms per just-parked
/// reuse), still under node's 4.3 s for the same run.
const FRESH_GATE: Duration = Duration::from_millis(1);

/// How long a checkout must wait before taking the entry parked at
/// `parked_at`: `None` when it is already [`FRESH_GATE`] old, else the
/// remaining time. Pure -- the reactor belongs to [`Pool::reuse_h1`].
fn fresh_delay(parked_at: Instant, now: Instant, window: Duration) -> Option<Duration> {
    let age = now.saturating_duration_since(parked_at);
    // `then`, not `then_some`: the subtraction must not evaluate when the
    // entry is already window-old (Duration subtraction panics on overflow).
    (age < window).then(|| window - age)
}

/// hyper-util's `pool_idle_timeout`, the reqwest default oam inherited.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// The reaper never wakes sooner than this after its last pass (hyper-util's
/// `MIN_CHECK`): connections parked within it of each other close together.
const MIN_REAP_TICK: Duration = Duration::from_millis(90);
/// At most one retry after a reused connection hands a request back unsent; a
/// fresh connection is never retried, so the loop settles in two.
const MAX_ATTEMPTS: usize = 3;

/// How one request may get a connection it has to open: the request's own
/// connect parameters, which every request sharing a pool carries for
/// itself.
pub(crate) struct Dial {
    /// undici's connect timeout (`OamConnector::connect_within`); `None` is
    /// none.
    pub(crate) connect_timeout: Option<Duration>,
    /// What a connection this request opens to an https origin offers, and
    /// which pooled connections it may take.
    pub(crate) alpn: Alpn,
    /// node's happy-eyeballs attempt timeout, for this request's dials.
    pub(crate) attempt_timeout: Duration,
    /// On a lookup-hooked pool, the hook's answer the request was resumed
    /// with: it OPENS a connection (the hook was asked for one, and undici
    /// asks it once per connection), so the request takes no idle one, and
    /// it is spent on that one dial.
    pub(crate) pin: Option<HookPin>,
}

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
    /// `dial` is how a connection this request has to open is made; it is
    /// the request's own, so a fetch and an `http.request` sharing the pool
    /// each connect under theirs. `follows` names the rules a 3xx answer to
    /// it is followed by, if it is: a redirect whose next hop keeps a method
    /// that is not idempotent never sends that hop on this connection
    /// ([`retires_for_the_hop`]).
    ///
    /// On a lookup-hooked pool, a request that has to open a connection to
    /// a host name without a hook answer for it fails with [`NeedsLookup`]
    /// and the request handed back unsent ([`PoolFail::returned`]), for the
    /// caller to send again with the answer.
    pub(crate) async fn request(
        &self,
        mut req: Request<ReqBody>,
        close_requested: bool,
        mut dial: Dial,
        follows: Option<Rules>,
    ) -> Result<Response<Incoming>, PoolFail> {
        let Some(key) = pool_key(req.uri(), dial.alpn) else {
            return Err(PoolFail {
                error: PoolError::connect(Box::<dyn std::error::Error + Send + Sync>::from(
                    "request url has no scheme or authority",
                )),
                reused: false,
                response_started: false,
                conn: None,
                returned: None,
            });
        };
        let is_connect = req.method() == Method::CONNECT;
        // Only a request that could not be sent again needs its method after
        // the send (a standard method's clone allocates nothing).
        let unresendable = follows
            .filter(|_| !super::send::is_idempotent(req.method()))
            .map(|rules| (rules, req.method().clone()));
        // hyper-util saves the request's absolute URI and restores it before
        // each attempt's origin/absolute rewrite, so a retry does not rewrite an
        // already-rewritten URI.
        let original_uri = req.uri().clone();
        // A hook answer opens a connection of its own: see `Dial::pin`.
        let mut allow_reuse = dial.pin.is_none();

        for _ in 0..MAX_ATTEMPTS {
            let conn = match self.checkout(&key, allow_reuse, &mut dial).await {
                Ok(conn) => conn,
                Err(Checkout::NeedsLookup(needed)) => {
                    // Unsent, and as the caller built it: a retry below
                    // handed it back with its URI already rewritten.
                    *req.uri_mut() = original_uri;
                    return Err(PoolFail {
                        error: PoolError::connect(Box::new(needed)),
                        reused: false,
                        response_started: false,
                        conn: None,
                        returned: Some(req),
                    });
                }
                Err(Checkout::Failed(error)) => {
                    return Err(PoolFail {
                        error: PoolError::connect(error),
                        reused: false,
                        response_started: false,
                        conn: None,
                        returned: None,
                    });
                }
            };
            // `attempt_stats` carries this attempt's `read` baseline, snapshotted
            // at checkout before any byte of its response could arrive.
            let Conn {
                proto,
                stats: attempt_stats,
                mut info,
                proxied,
                is_h2,
                key: conn_key,
            } = conn;
            // The request has a connection, and hyper writes it as soon as
            // it is handed over: node's 'finish' for http.request, and where
            // undici's headersTimeout starts (`sent`).
            if let Some(dispatched) = req.extensions().get::<Dispatched>() {
                dispatched.fire_on(info.connection);
            }
            info.take_lease();
            // A streamed body whose head waits for it: the connection is
            // there, and nothing goes out before the first chunk. A body
            // that fails first fails the request unsent; the connection,
            // which carried nothing, goes with it.
            if let Some(gate) = req.extensions_mut().remove::<HeadAwaitsBody>()
                && let Err(error) = gate.prime(&mut req).await
            {
                drop(proto);
                return Err(PoolFail {
                    error: PoolError::body(error),
                    reused: false,
                    response_started: false,
                    conn: Some(info),
                    returned: None,
                });
            }
            *req.uri_mut() = original_uri.clone();
            set_host_header(&mut req, is_h2);
            rewrite_request_uri(req.uri_mut(), is_h2, proxied, is_connect);

            match send_on(proto, req).await {
                SendResult::Ok(mut response, proto) => {
                    // Count this use so the connection's next checkout reports it
                    // as reused (the result is only needed on a failure).
                    attempt_stats.count_one();
                    response.extensions_mut().insert(info.clone());
                    let retire = unresendable.as_ref().is_some_and(|(rules, method)| {
                        retires_for_the_hop(&response, method, *rules)
                    });
                    self.on_success(
                        proto,
                        conn_key,
                        info,
                        proxied,
                        &mut response,
                        close_requested || retire,
                    );
                    return Ok(response);
                }
                SendResult::Unsent(returned, error, proto) => {
                    let reused = attempt_stats.count_one();
                    let response_started = attempt_stats.response_started();
                    self.on_failure(proto, is_h2, &conn_key);
                    if reused && allow_reuse {
                        // A pooled connection handed the request back unsent:
                        // re-dial and send it on a fresh one (retry_canceled).
                        // No connection has it while that one dials, so a
                        // headers timeout stops until its checkout (`sent`).
                        if let Some(dispatched) = returned.extensions().get::<Dispatched>() {
                            dispatched.unsent();
                        }
                        req = returned;
                        allow_reuse = false;
                        continue;
                    }
                    return Err(PoolFail {
                        error: PoolError::send(error),
                        reused,
                        response_started,
                        conn: Some(info),
                        returned: None,
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
                        conn: Some(info),
                        returned: None,
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
            conn: None,
            returned: None,
        })
    }

    /// Reuse an idle connection for `key`, or open exactly one. Never both.
    /// A hooked pool opens one to a host name only with the hook's answer
    /// for it, which the dial spends.
    async fn checkout(
        &self,
        key: &PoolKey,
        allow_reuse: bool,
        dial: &mut Dial,
    ) -> Result<Conn, Checkout> {
        if allow_reuse && self.inner.idle_timeout.is_some() {
            if let Some(conn) = self.reuse_h2(key) {
                return Ok(conn);
            }
            if let Some(conn) = self.reuse_h1(key).await {
                return Ok(conn);
            }
        }
        let uri = domain_as_uri(key);
        let pin = match self.connector.needs_lookup(&uri) {
            None => None,
            Some(needed) => match dial.pin.take() {
                Some(pin) if pin.key == needed.key => Some(pin),
                _ => return Err(Checkout::NeedsLookup(needed)),
            },
        };
        let params = DialParams {
            attempt_timeout: dial.attempt_timeout,
            pin,
        };
        self.connect(key, uri, dial.connect_timeout, params)
            .await
            .map_err(Checkout::Failed)
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

    /// Whether a request for `uri` offering `alpn` would find a connection
    /// to reuse right now: a live h2 connection to the origin, or a live,
    /// unexpired idle h1 one. A look, not a reservation -- another request
    /// can take it first, and the checkout then decides afresh.
    pub(crate) fn has_idle(&self, uri: &Uri, alpn: Alpn) -> bool {
        let Some(key) = pool_key(uri, alpn) else {
            return false;
        };
        if self.inner.idle_timeout.is_none() {
            return false;
        }
        let h2_live = self
            .inner
            .h2
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .is_some_and(|entry| !entry.sender.is_closed());
        if h2_live {
            return true;
        }
        let now = Instant::now();
        self.inner
            .idle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .is_some_and(|deque| {
                deque.iter().any(|entry| {
                    !entry.sender.is_closed()
                        && self.inner.idle_timeout.is_none_or(|timeout| {
                            idle_left(timeout, now, entry.parked_at).is_some()
                        })
                })
            })
    }

    /// Take the front idle h1 connection for `key`, or nothing: expired and
    /// already-closed entries are dropped (their sender closes the
    /// connection), a live one is handed out.
    ///
    /// #155: the entry is POPPED FIRST, so the one aged by the wait below is
    /// the one used -- a concurrent checkout cannot steal it mid-wait and
    /// leave this request racing a younger entry it never aged. The wait
    /// parks the send task until the entry is [`FRESH_GATE`] old: the
    /// connection's own dispatcher polls meanwhile (hyper reads the idle
    /// connection before every write, `EagerTcp`'s peek asks the kernel), so
    /// a server's FIN that was in flight lands, its EOF read closes the
    /// sender, and the re-check here drops it -- the request dials fresh
    /// instead of writing into the close. A sleep, not a `yield_now` spin:
    /// a spin keeps every worker draining runnable queues so none parks on
    /// the epoll driver, which is what delivers the FIN's readiness -- the
    /// very turn this gate exists to create. An entry already window-old
    /// (the steady-state reuse) waits nothing.
    async fn reuse_h1(&self, key: &PoolKey) -> Option<Conn> {
        loop {
            // Pop under the lock; nothing else can take this entry after.
            let entry = {
                let mut map = self.inner.idle.lock().unwrap_or_else(|e| e.into_inner());
                let deque = map.get_mut(key)?;
                let entry = deque.pop_front();
                if deque.is_empty() {
                    map.remove(key);
                }
                entry
            }?;
            if let Some(timeout) = self.inner.idle_timeout
                && idle_left(timeout, Instant::now(), entry.parked_at).is_none()
            {
                continue; // expired -> drop, close
            }
            if entry.sender.is_closed() {
                continue; // a processed FIN; drop (lazy skip)
            }
            if let Some(delay) = fresh_delay(entry.parked_at, Instant::now(), FRESH_GATE) {
                tokio::time::sleep(delay).await;
                // Hands the run queue to a dispatcher woken at the deadline.
                tokio::task::yield_now().await;
                if entry.sender.is_closed() {
                    continue; // the FIN landed during the wait -> drop
                }
            }
            let stats = entry.stats.clone();
            return Some(Conn {
                proto: Proto::H1(entry.sender, entry.stats),
                stats,
                info: entry.info,
                proxied: entry.proxied,
                is_h2: false,
                key: key.clone(),
            });
        }
    }

    async fn connect(
        &self,
        key: &PoolKey,
        uri: Uri,
        connect_timeout: Option<Duration>,
        params: DialParams,
    ) -> Result<Conn, BoxError> {
        let conn = self
            .connector
            .clone()
            .connect_within(uri, key.2, connect_timeout, params)
            .await?;
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
            // JS may close the connection through a response it carried
            // (`req.socket.destroy()` / `resetAndDestroy()`): the task then
            // drops it, mid-response or idle in the pool alike.
            let closer = info.connection.and_then(ConnCloser::find);
            tokio::spawn(async move {
                match closer {
                    Some(closer) => {
                        // The close first: a body JS drops right after asking
                        // (the destroyed socket's response) must not get
                        // hyper to shut the connection down with a FIN ahead
                        // of the reset.
                        tokio::select! {
                            biased;
                            () = closer.requested() => {}
                            _ = connection.with_upgrades() => {}
                        }
                    }
                    None => {
                        let _ = connection.with_upgrades().await;
                    }
                }
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
    /// h2 connection stays in its entry for the next stream. An h1 response
    /// carries a [`Released`] that settles once the connection is parked or
    /// closed.
    fn on_success(
        &self,
        sender: Proto,
        key: PoolKey,
        info: ConnInfo,
        proxied: bool,
        response: &mut Response<Incoming>,
        close_requested: bool,
    ) {
        let Proto::H1(mut sender, stats) = sender else {
            return; // h2: the entry is already in the map
        };
        let should_park = !close_requested && !says_close(response.headers());
        let inner = Arc::downgrade(&self.inner);
        let gen_id = self.inner.generation.load(Ordering::Relaxed);
        let (released, settles) = tokio::sync::watch::channel(());
        response.extensions_mut().insert(Released(settles));
        tokio::spawn(async move {
            // Dropped on every way out of this task -- after the park, or
            // with the connection -- which is what `Released` waits for.
            let _released = released;
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
    ///
    /// It wakes when the oldest idle connection expires, not on a fixed tick:
    /// a tick of the timeout itself (hyper-util's) closed a connection parked
    /// just after a tick only at the tick after next, up to twice the timeout
    /// after it went idle.
    fn ensure_reaper(self: &Arc<Self>) {
        let Some(timeout) = self.idle_timeout else {
            return;
        };
        if self.reaper_started.swap(true, Ordering::Relaxed) {
            return;
        }
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut wait = timeout;
            loop {
                tokio::time::sleep(wait.max(MIN_REAP_TICK)).await;
                match weak.upgrade() {
                    Some(inner) => wait = inner.clear_expired(timeout),
                    None => break,
                }
            }
        });
    }

    /// Drop the idle connections that have been idle for `timeout` and the
    /// dead ones, and return how long until the next one left expires
    /// (`timeout` when none is idle: one parked from now on expires no
    /// sooner).
    fn clear_expired(&self, timeout: Duration) -> Duration {
        let now = Instant::now();
        let mut next = timeout;
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        idle.retain(|_, deque| {
            deque.retain(|entry| match idle_left(timeout, now, entry.parked_at) {
                Some(left) if !entry.sender.is_closed() => {
                    next = next.min(left);
                    true
                }
                _ => false,
            });
            !deque.is_empty()
        });
        drop(idle);
        let mut h2 = self.h2.lock().unwrap_or_else(|e| e.into_inner());
        h2.retain(|_, entry| !entry.sender.is_closed());
        next
    }
}

/// How long a connection parked at `parked_at` may stay idle after `now`;
/// `None` once it has been idle for `timeout`.
fn idle_left(timeout: Duration, now: Instant, parked_at: Instant) -> Option<Duration> {
    timeout
        .checked_sub(now.saturating_duration_since(parked_at))
        .filter(|left| !left.is_zero())
}

/// Settles once the h1 connection a response came on is done with it: parked
/// in the pool for the next request, or closed. The fetch loop waits for it
/// between a 3xx and the hop it follows with, so that hop finds the
/// connection idle whenever it can be reused, every time: parking runs on a
/// task of its own, and without the wait the hop raced it (and, on a hooked
/// route, called the hook for a connection it then opened only some of the
/// time). It settles promptly -- the response is dropped first, so the
/// connection either re-arms at once (an empty body) or closes.
#[derive(Clone)]
pub(crate) struct Released(tokio::sync::watch::Receiver<()>);

impl Released {
    pub(crate) async fn settled(mut self) {
        // The task holds the sender and never sends: `changed` returns once
        // it is dropped.
        let _ = self.0.changed().await;
    }
}

/// Why a checkout produced no connection.
enum Checkout {
    /// The dial failed.
    Failed(BoxError),
    /// A hooked pool has no idle connection for the authority and no hook
    /// answer to open one with.
    NeedsLookup(NeedsLookup),
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
    /// The connection the request went out on; `None` when none was had.
    pub(crate) conn: Option<ConnInfo>,
    /// The request, unsent, when the failure is a [`NeedsLookup`]: the
    /// caller sends it again once the hook has answered (a streamed body
    /// cannot be built twice).
    pub(crate) returned: Option<Request<ReqBody>>,
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
    /// The body failed before the head went out ([`HeadAwaitsBody`]).
    Body(BoxError),
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

    fn body(error: BoxError) -> PoolError {
        PoolError {
            kind: PoolErrorKind::Body(error),
        }
    }
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            PoolErrorKind::Connect(_) => f.write_str("error connecting for url"),
            PoolErrorKind::Send(_) | PoolErrorKind::Body(_) => {
                f.write_str("error sending request for url")
            }
        }
    }
}

impl std::error::Error for PoolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            PoolErrorKind::Connect(error) => Some(&**error),
            PoolErrorKind::Send(error) => Some(error),
            PoolErrorKind::Body(error) => Some(&**error),
        }
    }
}

/// The pool key for a request URI: `(scheme, authority)`, as written, and the
/// ALPN offer for an https one.
fn pool_key(uri: &Uri, alpn: Alpn) -> Option<PoolKey> {
    let scheme = uri.scheme()?;
    let alpn = if *scheme == Scheme::HTTPS {
        alpn
    } else {
        Alpn::None
    };
    Some((scheme.clone(), uri.authority()?.clone(), alpn))
}

/// `scheme://authority/` -- the dst the connector dials for `key` (hyper-util's
/// `domain_as_uri`).
fn domain_as_uri((scheme, authority, _): &PoolKey) -> Uri {
    Uri::builder()
        .scheme(scheme.clone())
        .authority(authority.clone())
        .path_and_query("/")
        .build()
        .expect("scheme and authority make a valid uri")
}

/// `response` is a redirect the fetch loop follows with a hop whose method
/// -- `method`, kept under `rules` -- is not idempotent (#155): a POST that a
/// 307 or 308 sends on, say. Such a hop never goes out on the connection the
/// 3xx came on, which is then not pooled. A server that answers with a 3xx
/// often closes the connection right behind it, and the hop, sent at once,
/// could be written into that close before its FIN arrived: a request oam
/// may not send twice (RFC 9110 s9.2.2), so the fetch failed where node's,
/// whose event loop reads the FIN before it writes, succeeds. The hop dials
/// a connection of its own instead -- a cost on this path alone; a hop that
/// may be resent (a GET, or the GET a 302 turns a POST into) still takes
/// the pooled connection, and the one resend covers it.
fn retires_for_the_hop(response: &Response<Incoming>, method: &Method, rules: Rules) -> bool {
    let status = response.status().as_u16();
    redirect::is_redirect_status(status)
        && response.headers().contains_key(http::header::LOCATION)
        && !redirect::rewrites_to_get(status, method, rules)
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

    /// The reaper closes an idle connection when its timeout is up, wherever
    /// its park fell between the reaper's passes. With a fixed tick of the
    /// timeout, a connection parked just after a tick lived to the tick
    /// after next: here the second connection, parked 0.6 timeouts after
    /// the first (whose park started the reaper), stayed idle 1.4 timeouts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_reaper_closes_an_idle_connection_at_its_timeout() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        const TIMEOUT: Duration = Duration::from_millis(1000);
        // An origin whose connections answer one request, keep alive, and
        // report how long after the answer the client closed them.
        async fn origin(closed: tokio::sync::mpsc::UnboundedSender<Duration>) -> u16 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let closed = closed.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 4096];
                        let mut head = Vec::new();
                        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                            let n = stream.read(&mut buf).await.unwrap();
                            assert!(n > 0, "closed before the request");
                            head.extend_from_slice(&buf[..n]);
                        }
                        stream
                            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                            .await
                            .unwrap();
                        let answered = Instant::now();
                        while stream.read(&mut buf).await.unwrap_or(0) > 0 {}
                        let _ = closed.send(answered.elapsed());
                    });
                }
            });
            port
        }
        let (closed_tx, mut closed_rx) = tokio::sync::mpsc::unbounded_channel();
        // Two origins, so the second request opens a connection of its own.
        let first = origin(closed_tx.clone()).await;
        let second = origin(closed_tx).await;
        let pool = Pool::new(
            OamConnector {
                shared: Arc::new(super::super::connector::Shared {
                    tls: super::super::TlsSource::Unavailable("not used".into()),
                    proxy: None,
                    user_agent: http::HeaderValue::from_static("oam-test"),
                    tls_range: std::sync::atomic::AtomicU8::new(
                        super::super::TlsRange::Both.code(),
                    ),
                }),
                via: super::super::connector::Via::Pooled,
            },
            Some(TIMEOUT),
        );
        let get = |port: u16| {
            let pool = pool.clone();
            async move {
                let mut req = Request::new(super::super::transport::empty_body());
                *req.uri_mut() = format!("http://127.0.0.1:{port}/").parse().unwrap();
                let dial = Dial {
                    connect_timeout: None,
                    alpn: Alpn::default(),
                    attempt_timeout: Duration::from_millis(250),
                    pin: None,
                };
                let response = pool.request(req, false, dial, None).await.ok().unwrap();
                let released = response.extensions().get::<Released>().cloned().unwrap();
                http_body_util::BodyExt::collect(response.into_body())
                    .await
                    .unwrap();
                released.settled().await;
            }
        };
        get(first).await;
        tokio::time::sleep(TIMEOUT * 6 / 10).await;
        get(second).await;
        for _ in 0..2 {
            let idle = tokio::time::timeout(TIMEOUT * 3, closed_rx.recv())
                .await
                .expect("an idle connection was never closed")
                .unwrap();
            assert!(
                idle >= TIMEOUT - Duration::from_millis(50)
                    && idle < TIMEOUT + Duration::from_millis(350),
                "an idle connection closed {idle:?} after its answer; the timeout is {TIMEOUT:?}"
            );
        }
    }

    /// The idle rule the reaper and a checkout share: a connection may stay
    /// idle for the timeout and no longer, and what is left of it is what
    /// the reaper sleeps for -- so a connection closes at the timeout, not
    /// up to a whole tick after it.
    #[test]
    fn an_idle_connection_expires_at_the_timeout() {
        let timeout = Duration::from_secs(90);
        let parked = Instant::now();
        assert_eq!(idle_left(timeout, parked, parked), Some(timeout));
        assert_eq!(
            idle_left(timeout, parked + Duration::from_secs(89), parked),
            Some(Duration::from_secs(1))
        );
        assert_eq!(idle_left(timeout, parked + timeout, parked), None);
        assert_eq!(
            idle_left(timeout, parked + Duration::from_secs(175), parked),
            None
        );
        // A clock read before the park (another thread's `now`) is not idle
        // time.
        assert_eq!(
            idle_left(timeout, parked, parked + Duration::from_secs(1)),
            Some(timeout)
        );
    }

    /// #155: the checkout waits out [`FRESH_GATE`] before handing a
    /// just-parked entry to a request. The expected delay is
    /// `max(0, gate - age)`; the gate off (`Duration::ZERO`) must wait
    /// nothing, and gate on must wait a young entry (calibrated against the
    /// test's own jitter so it holds on a slow box).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_just_parked_entry_delays_the_checkout_by_the_fresh_gate() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        // A live origin that answers EVERY request on a connection with a 200
        // and keeps it open (keep-alive): both requests ride one connection,
        // and its entry parks after each body drains.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    loop {
                        // Read one request head (a GET carries no body).
                        let mut head = Vec::new();
                        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                            let Ok(n) = stream.read(&mut buf).await else {
                                return;
                            };
                            if n == 0 {
                                return;
                            }
                            head.extend_from_slice(&buf[..n]);
                            // Only this test's client asks here, but an
                            // unbounded head buffer is a shape to refuse
                            // rather than trust.
                            if head.len() > 64 * 1024 {
                                return;
                            }
                        }
                        // Answer it; the loop keeps the connection alive for
                        // the next one until the client closes.
                        if stream
                            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                });
            }
        });
        let pool = Pool::new(
            OamConnector {
                shared: Arc::new(super::super::connector::Shared {
                    tls: super::super::TlsSource::Unavailable("not used".into()),
                    proxy: None,
                    user_agent: http::HeaderValue::from_static("oam-test"),
                    tls_range: std::sync::atomic::AtomicU8::new(
                        super::super::TlsRange::Both.code(),
                    ),
                }),
                via: super::super::connector::Via::Pooled,
            },
            Some(Duration::from_secs(90)),
        );
        let dial = || Dial {
            connect_timeout: None,
            alpn: Alpn::default(),
            attempt_timeout: Duration::from_millis(250),
            pin: None,
        };
        let uri: Uri = format!("http://127.0.0.1:{port}/").parse().unwrap();
        let get = || {
            let mut req = Request::new(super::super::transport::empty_body());
            *req.uri_mut() = uri.clone();
            req
        };
        // First request: opens the connection; its response parks it. Every
        // await is bounded: a regression here must fail, not hang the suite.
        let response = tokio::time::timeout(
            Duration::from_secs(10),
            pool.request(get(), false, dial(), None),
        )
        .await
        .expect("the first request timed out")
        .unwrap_or_else(|e| panic!("first request failed: {}", e.error));
        let released = response.extensions().get::<Released>().cloned().unwrap();
        http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap();
        // The entry is just parked. Measure the checkout DIRECTLY, not
        // through a whole request (whose debug round trip exceeds the 1ms
        // window): `reuse_h1` is what sleeps. `phase` -- the body drain to
        // that pop -- bounds the entry's age when the age is read inside it.
        let phase_started = Instant::now();
        released.settled().await;
        let phase = phase_started.elapsed();
        let key = pool_key(&uri, Alpn::default()).unwrap();
        let young_started = Instant::now();
        let young = pool.reuse_h1(&key).await;
        let young_wait = young_started.elapsed();
        // Park a second connection for the baseline pop: request 2 dials
        // fresh (the young pop already took the parked entry).
        let response = tokio::time::timeout(
            Duration::from_secs(10),
            pool.request(get(), false, dial(), None),
        )
        .await
        .expect("the second request timed out")
        .unwrap_or_else(|e| panic!("second request failed: {}", e.error));
        let released = response.extensions().get::<Released>().cloned().unwrap();
        http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap();
        released.settled().await;
        // Baseline: the same pop path once the entry is past the gate.
        tokio::time::sleep(FRESH_GATE * 2).await;
        let base_started = Instant::now();
        let old = pool.reuse_h1(&key).await;
        let baseline = base_started.elapsed();
        assert!(
            young.is_some() && old.is_some(),
            "both parked entries must be available to the checkout"
        );
        // The gate stays at a swept window (>=500us: the sweep's floor was
        // 1ms, and 0 reproduced the #155 gap). A constant dropped to zero
        // fails here, not on an expectation derived from the same constant
        // it would be testing.
        assert!(
            FRESH_GATE >= Duration::from_micros(500),
            "FRESH_GATE must stay at a swept window of at least 500us (the #155 sweep's \
             floor is 1ms; 0 measured at the gap), got {FRESH_GATE:?}"
        );
        // And the checkout must actually sleep it out: `phase` bounds the
        // entry's age, `baseline` is the no-wait cost of the same pop path,
        // so `required` is what remains of the window. A checkout that stops
        // sleeping takes about `baseline` and fails the assert below. A box
        // too slow to prove anything (gate - phase - baseline <= 0) degrades
        // the floor to zero instead of panicking: the constant guard above
        // still pins the gate, and a load stall during the settle -- which
        // this suite already flaked on elsewhere -- must not become a false
        // failure.
        let required = FRESH_GATE
            .checked_sub(phase)
            .and_then(|left| left.checked_sub(baseline))
            .unwrap_or(Duration::ZERO);
        assert!(
            young_wait >= required,
            "the checkout waited only {young_wait:?}, short of {required:?} \
             (FRESH_GATE {FRESH_GATE:?}, phase {phase:?}, baseline {baseline:?}): \
             the freshness wait did not run"
        );
    }

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

    #[test]
    fn a_just_parked_connection_waits_out_the_fresh_gate_and_an_old_one_does_not() {
        let window = Duration::from_micros(200);
        let parked_at = Instant::now();
        // Just parked: the wait is (nearly) the whole window.
        let delay = fresh_delay(parked_at, parked_at, window).expect("young entry waits");
        assert_eq!(delay, Duration::from_micros(200));
        // Half a window old: only the remainder.
        let delay = fresh_delay(parked_at, parked_at + Duration::from_micros(120), window)
            .expect("entry inside the window waits");
        assert_eq!(delay, Duration::from_micros(80));
        // Window-old (the steady-state reuse): no wait at all.
        assert_eq!(
            fresh_delay(parked_at, parked_at + Duration::from_micros(200), window),
            None
        );
        // A much older entry never waits -- and the subtraction must not
        // evaluate here at all (it would panic on underflow).
        assert_eq!(
            fresh_delay(parked_at, parked_at + Duration::from_secs(1), window),
            None
        );
    }
}
