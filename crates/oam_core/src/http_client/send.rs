//! The fetch op on oam's transport: the request JS sends, the redirect loop,
//! the `connect.lookup` continuation, and the payload a fetch resolves with
//! at the response head.
//!
//! A fetch whose dispatcher carries a `connect.lookup` hook runs in hook
//! mode. undici calls the hook for every connection it opens to a host name
//! -- the first request and every redirect hop to another host -- so an
//! SSRF or DNS-rebind guard vets a 302 to an internal name too. The op cannot
//! call JS while it runs, so before dialling a host name it has no addresses
//! for, the loop PARKS: it stores its whole state in [`FetchContinuations`]
//! and resolves with `{"lookup": {token, host, port}}`. JS runs the hook and
//! resumes the fetch with [`fetch_continue`], or drops it with
//! [`fetch_abandon`] when the hook failed or the fetch was aborted.
//!
//! The order of the checks is undici's: the initial bad-port block runs
//! before the first park (a bad-port URL never reaches the hook), and a
//! hop's redirect checks, bad port included, run before that hop parks.
//!
//! A fetch whose dispatcher carries a `connect` FUNCTION (a custom undici
//! connector) runs in connector mode, which parks the same way for a
//! different answer: before every send it resolves with
//! `{"connect": {token, host, hostname, protocol, port}}` -- the parameters
//! undici calls its connector with -- and JS resumes it with the socket that
//! function returned, piped ([`fetch_supply`]). The request then goes over
//! that socket and nowhere else; no hop is ever dialled here.
//!
//! A fetch can be CANCELLED while its request is on the wire and no response
//! head has come ([`FetchCancel`], `fetchCancel`): an aborted `fetch()` or a
//! destroyed `http.request` leaves, as node's does, instead of waiting for a
//! response nobody will read. The loop drops the request it is sending, which
//! closes its h1 connection (or resets its h2 stream), and the op fails as
//! [`ABORTED`].
//!
//! Under `--permission`, the engine's net grant ([`NetCheck`]) is applied to
//! every hop's host at the top of the loop: before the hop parks for its
//! lookup hook, before anything dials, and whichever route (pooled, hooked,
//! proxied) will carry it. The engine's synchronous gate only ever saw the
//! URL JS passed in; the loop is where a redirect picks the next host, so the
//! loop is where the grant has to be asked again.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use http::header::{CONTENT_ENCODING, CONTENT_LENGTH, HeaderMap, PROXY_AUTHORIZATION};
use hyper::body::Incoming;

use super::body::{FetchBodies, FetchBody, StreamSlot};
use super::connector::{ConnInfo, SuppliedConn};
use super::decode::{self, MAX_CODINGS, Plan};
use super::prepare::{self, PrepareError};
use super::redirect::{self, Next};
use super::sent::Dispatched;
use super::tls_config::{Alpn, TlsRange};
use super::transport::{channel_body, channel_body_then, empty_body, full_body};
use super::{HttpTransport, NetCheck, NetTarget, ReqBody, Route};
use crate::OpOutcome;
use crate::OutboundBodies;
use crate::net_connect::attempt_timeout_from_ms;

/// [`FetchRequest::defer_head`].
#[derive(serde::Deserialize, Debug, Clone, Copy, Default)]
pub struct DeferHead {
    /// A body that ends with no chunk goes out as `content-length: 0` (a
    /// method undici expects a payload for, with no length of its own).
    #[serde(default)]
    pub empty_content_length: bool,
}

/// The request `js/bootstrap.js` sends (serde; unknown fields are ignored).
#[derive(serde::Deserialize)]
pub struct FetchRequest {
    pub url: String,
    #[serde(default)]
    pub method: Option<String>,
    /// In the caller's order; a repeated name is sent as separate lines.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub body_base64: Option<String>,
    /// Handle into `OutboundBodies`: the body streams from JS instead of
    /// being sent whole. Mutually exclusive with `body` / `body_base64`.
    #[serde(default)]
    pub body_stream: Option<u64>,
    /// The dispatcher carries a `connect.lookup` hook: run in hook mode (see
    /// the module docs).
    #[serde(default)]
    pub lookup_hook: bool,
    /// The dispatcher carries a `connect` FUNCTION: run in connector mode
    /// (see the module docs). Wins over `lookup_hook`.
    #[serde(default)]
    pub connect_hook: bool,
    /// `net.getDefaultAutoSelectFamilyAttemptTimeout()` at call time.
    /// Missing or not a positive number: node's 250 ms.
    #[serde(default)]
    pub attempt_timeout_ms: Option<f64>,
    /// undici's connect timeout, in ms: how long each connection this
    /// request opens has to be connected (for https, handshaken) before the
    /// request fails with undici's `ConnectTimeoutError`. JS sends it for
    /// what goes through undici in node -- `fetch()` and `undici.request`,
    /// 10 s unless the dispatcher says otherwise. Absent, 0 or not a
    /// positive number: no timeout, which is `http.request` (node has none
    /// there) and a dispatcher that turned it off.
    #[serde(default)]
    pub connect_timeout_ms: Option<f64>,
    /// True for `fetch()`: undici's bad-port block on the initial URL.
    /// `http.request`, `undici.request` and the http2 compat client leave it
    /// false -- node's http.request has no such check. Redirect hops are
    /// checked either way (redirect::next).
    #[serde(default)]
    pub fetch_semantics: bool,
    /// True for `undici.request` (and implied by `fetch_semantics`): the
    /// request goes through undici's dispatcher in node, so an oversized
    /// response head is counted and refused as undici does
    /// ([`response_head_overflow`]), not as node's own http parser.
    #[serde(default)]
    pub dispatch_semantics: bool,
    /// `fetch`'s `redirect` option; `http.request` always sends `manual`
    /// (node's http client never follows a redirect). Absent: follow.
    #[serde(default)]
    pub redirect: RedirectMode,
    /// False delivers the body as received and keeps the encoding headers:
    /// what `http.request` sends, node's http client decoding nothing (#148).
    #[serde(default = "yes")]
    pub decode: bool,
    /// False sends the caller's headers alone: what `http.request` sends,
    /// node's http client adding no `accept` / `user-agent` /
    /// `accept-encoding` of its own (#148).
    #[serde(default = "yes")]
    pub default_headers: bool,
    /// node's `maxHeaderSize` for the response heads of this request:
    /// `http.request`'s per-request option, when it sets one. Absent: the
    /// process-wide `--max-http-header-size` (16384 unless set), which is
    /// what node's fetch and `http.request` use by default.
    #[serde(default)]
    pub max_header_size: Option<u64>,
    /// For an https URL, the TLS version range: node's live
    /// `tls.DEFAULT_MIN_VERSION` / `DEFAULT_MAX_VERSION` as `node:tls`
    /// resolves them (bootstrap.js tlsDefaultVersions), "" or absent for a
    /// side with no bound of its own. Node's undici connects through
    /// tls.connect, which reads them for every connection.
    #[serde(default)]
    pub tls_min_version: Option<String>,
    #[serde(default)]
    pub tls_max_version: Option<String>,
    /// For an https URL, what the handshake offers by ALPN: `"http1"`
    /// (absent: undici's `http/1.1`, for `fetch` and `undici.request`),
    /// `"allow_h2"` (a dispatcher with undici's `allowH2`) or `"none"`
    /// (`https.request`, which offers nothing in node). See [`Alpn`].
    #[serde(default)]
    pub alpn: Alpn,
    /// Handle of a [`super::sent`] signal to fire once the request has a
    /// connection: `http.request`'s, for node's `'finish'` (#193).
    #[serde(default)]
    pub sent_signal: Option<u64>,
    /// A streamed body (`body_stream`) whose head waits for it, as undici's
    /// AsyncWriter writes it: the request is dispatched -- the bad-port
    /// check, the lookup and the connect run at once -- but nothing goes on
    /// the wire before the body's first non-empty chunk, and a body that
    /// ends with none goes as no body at all ([`super::transport::HeadAwaitsBody`]).
    #[serde(default)]
    pub defer_head: Option<DeferHead>,
    /// `undici.request`'s streamed body is an iterable or a web stream, not
    /// a Readable: undici follows a redirect with it, sending the spent
    /// iterable ([`redirect::RedirectBody::UndiciIterable`]).
    #[serde(default)]
    pub iterable_body: bool,
    /// That signal's sending half. Never from JS: the engine's fetch op
    /// takes it from the runtime's registry and puts it here.
    #[serde(skip)]
    pub dispatched: Option<Dispatched>,
    /// undici's `headersTimeout` in ms (`fetch`'s and `undici.request`'s;
    /// fractional allowed, absent or 0 for no limit): the longest a hop's
    /// response head may take once a connection has the request
    /// ([`super::sent`]).
    #[serde(default)]
    pub headers_timeout_ms: Option<f64>,
    /// undici's `bodyTimeout` in ms (absent or 0 for no limit): the longest
    /// one read of the response body may wait for bytes
    /// ([`super::body::read`]).
    #[serde(default)]
    pub body_timeout_ms: Option<f64>,
}

/// The longest timer JS can set (2^31-1 ms, about 24.8 days). A
/// `headersTimeout` or `bodyTimeout` above it is held to it, as
/// `undici.request`'s timers always were in oam (docs/node-divergences.md).
const MAX_TIMER_MS: f64 = 2_147_483_647.0;

/// A `headers_timeout_ms` / `body_timeout_ms` as the time it takes to lapse:
/// `None` for none (absent, 0, or not a number), else the limit -- at least
/// 1 ms, `setTimeout`'s floor, and at most [`MAX_TIMER_MS`] -- on undici's
/// clock ([`fast_timer`]).
fn timeout_limit(ms: Option<f64>) -> Option<Duration> {
    let ms = ms.filter(|ms| *ms > 0.0)?;
    Some(fast_timer(ms.clamp(1.0, MAX_TIMER_MS)))
}

/// undici's FastTimer tick (lib/util/timers.js TICK_MS).
const FAST_TIMER_TICK_MS: f64 = 499.0;

/// How long a `limit_ms` timer on undici's FastTimer takes to lapse. undici
/// 6.29.0 runs `headersTimeout` and `bodyTimeout` on it (client-h1.js
/// TIMEOUT_HEADERS / TIMEOUT_BODY | USE_FAST_TIMER): a timer armed, or
/// refreshed, is taken up at the clock's next 499 ms tick and fires at the
/// first tick at least `limit` after the one before that. So any limit up
/// to 998 ms lapses after 998 ms, and longer ones in 499 ms steps -- 1500
/// after 1996 (measured on node v22.22.2 + undici 6.29.0: about 1010 ms for
/// 1, 100 and 300, about 2050 ms for 1500). Modelled from the arm, as with
/// no other undici timer running; with one, undici's next tick can come
/// sooner (docs/node-divergences.md).
fn fast_timer(limit_ms: f64) -> Duration {
    let ticks = ((limit_ms - FAST_TIMER_TICK_MS) / FAST_TIMER_TICK_MS)
        .ceil()
        .max(1.0);
    Duration::from_secs_f64((ticks + 1.0) * FAST_TIMER_TICK_MS / 1000.0)
}

/// undici's `HeadersTimeoutError`, raised when a hop's head is late. JS
/// gives it that class (bootstrap.js `undiciCause`).
fn headers_timed_out() -> OpOutcome {
    OpOutcome::node_failed("UND_ERR_HEADERS_TIMEOUT", "Headers Timeout Error")
}

fn yes() -> bool {
    true
}

/// [`FetchRequest::connect_timeout_ms`] as a duration; `None` is no timeout.
fn connect_timeout_from_ms(ms: Option<f64>) -> Option<Duration> {
    let ms = ms.filter(|ms| ms.is_finite() && *ms > 0.0)?;
    // A JS number far past any real timeout saturates rather than wrapping;
    // `from_secs_f64` would panic on an overflow.
    Some(Duration::from_secs_f64(ms.min(u32::MAX as f64) / 1000.0))
}

/// What a 3xx does.
#[derive(serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RedirectMode {
    /// Follow it (undici's "follow").
    #[default]
    Follow,
    /// Return it as the response.
    Manual,
    /// Fail the fetch on a redirect status (undici's "error").
    Error,
}

/// Parked hook-mode fetches by continuation token (ids from the runtime's
/// shared handle allocator).
///
/// An entry lives until JS continues or abandons it -- bootstrap.js does one
/// or the other on every path -- or until the runtime drops. A hook that
/// never calls back parks one entry for the life of the run: the same
/// resource a never-settling lookup pins in node, a socket left connecting.
pub type FetchContinuations = Arc<Mutex<HashMap<u64, PendingFetch>>>;

/// A fetch waiting for its `connect.lookup` hook to resolve `host`. Dropping
/// it (abandon, or the runtime going away) releases an untaken streamed
/// body's receiver.
pub struct PendingFetch {
    state: LoopState,
    host: String,
    /// The authority key the hook's answer is filed under
    /// (`connector::authority_key`), which is not the host: a hop to the same
    /// name on another port is a separate authority and parks again.
    key: String,
    /// What the fetch waits for: a lookup answer, or a connection.
    wants: Wants,
}

/// The answer a parked fetch takes; the other is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wants {
    /// `fetchContinue` with the hook's addresses.
    Addresses,
    /// `fetchSupply` with a connection from the dispatcher's `connect`.
    Connection,
}

impl PendingFetch {
    /// The host the hook is resolving.
    pub fn host(&self) -> &str {
        &self.host
    }
}

/// In-flight fetches that can be cancelled, by the id JS gave each one.
pub type FetchCancels = Arc<Mutex<HashMap<u64, Arc<tokio::sync::Notify>>>>;

/// What a cancelled fetch's op fails with. Nothing reads it: JS has already
/// settled the fetch with the abort reason (or the destroyed request's
/// error) by the time it cancels.
pub const ABORTED: &str = "fetch: aborted";

/// One fetch's entry in [`FetchCancels`], registered before the op starts --
/// synchronously, in the op's native call -- so a cancel can never arrive
/// ahead of it (`fetch(url, { signal }); controller.abort()` in one tick).
/// The fetch carries it for as long as it has no response head, parks
/// included; dropping it takes the entry out, so a cancel after the head (or
/// after a failure) finds nothing and does nothing.
pub struct FetchCancel {
    id: u64,
    registry: FetchCancels,
    signal: Arc<tokio::sync::Notify>,
}

impl FetchCancel {
    pub fn register(registry: &FetchCancels, id: u64) -> FetchCancel {
        let signal = Arc::new(tokio::sync::Notify::new());
        lock(registry).insert(id, signal.clone());
        FetchCancel {
            id,
            registry: registry.clone(),
            signal,
        }
    }

    /// Resolves once the fetch has been cancelled. A cancel that came while
    /// nothing was waiting (between hops, or while the fetch was parked) is
    /// kept: `notify_one` stores a permit for the next wait.
    async fn cancelled(&self) {
        self.signal.notified().await;
    }
}

impl Drop for FetchCancel {
    fn drop(&mut self) {
        let mut registry = lock(&self.registry);
        // Only its own entry: an id JS reused belongs to the later fetch.
        if registry
            .get(&self.id)
            .is_some_and(|signal| Arc::ptr_eq(signal, &self.signal))
        {
            registry.remove(&self.id);
        }
    }
}

/// `fetchCancel`: cancel the in-flight fetch registered under `id`. True if
/// it was still without a response head (and so was there to cancel).
pub fn fetch_cancel(id: u64, cancels: &FetchCancels) -> bool {
    match lock(cancels).get(&id) {
        Some(signal) => {
            signal.notify_one();
            true
        }
        None => false,
    }
}

/// reqwest's h2 retry allowance (retry.rs, `max_retries_per_request`).
pub const MAX_H2_RETRIES: u32 = 2;

/// Everything the loop needs between hops, and across a park.
struct LoopState {
    transport: HttpTransport,
    route: Route,
    method: http::Method,
    /// This hop's URL (no userinfo; a fragment is carried, never sent).
    current: url::Url,
    /// The headers every hop starts from. A redirect's strip is applied here,
    /// so it is permanent for the rest of the fetch.
    carried: HeaderMap,
    source: BodySource,
    hops: u32,
    redirect: RedirectMode,
    decode: bool,
    /// The `--permission` net grant, asked about every hop's host before it
    /// is dialled. Carried across a park, so a resumed fetch keeps enforcing
    /// it. `None`: every host is granted.
    net_check: Option<NetCheck>,
    /// The response-head limit, and which of node's two counts applies (see
    /// [`response_head_overflow`]).
    max_header_size: u64,
    undici_head: bool,
    /// A `fetch` (not `undici.request` or `http.request`): a redirect that
    /// would resend a streamed body fails it ([`redirect::RedirectBody`]).
    fetch_rules: bool,
    /// Fired by the pool when a connection has a hop's request (see
    /// [`super::sent`]); carried across a park, dropped with the fetch.
    /// `http.request`'s, or the loop's own when only `headers_timeout` needs
    /// it.
    dispatched: Option<Dispatched>,
    /// The fetch's cancel registration, if JS can cancel it.
    cancel: Option<FetchCancel>,
    /// undici's `headersTimeout`, run from each checkout
    /// ([`super::sent::headers_deadline`]). `None`: no limit, and no timer.
    headers_timeout: Option<Duration>,
    /// undici's `bodyTimeout`, handed to the response body. `None`: no limit.
    body_timeout: Option<Duration>,
    /// [`FetchRequest::defer_head`], for the first send of a streamed body.
    defer_head: Option<DeferHead>,
    /// [`FetchRequest::iterable_body`].
    iterable_body: bool,
}

enum BodySource {
    Empty,
    Full(Bytes),
    Stream(StreamSlot),
}

impl BodySource {
    /// A body that can be sent again (a redirect that keeps it, an h2 retry).
    fn replayable(&self) -> bool {
        !matches!(self, BodySource::Stream(_))
    }

    /// The body for the next send. A stream's receiver is taken here, on the
    /// first send, and never again. The error is the op's failure text.
    ///
    /// `timed`: the send's signal when a headers timeout runs on it, which
    /// hears whether this body is still being written -- a streamed one
    /// until its last chunk goes ([`Dispatched::body_open`]).
    ///
    /// `declared`: the request's `content-length`, if it has one.
    fn build(
        &mut self,
        timed: Option<&Dispatched>,
        declared: Option<u64>,
    ) -> Result<ReqBody, String> {
        if let Some(dispatched) = timed {
            dispatched.body_open(matches!(self, BodySource::Stream(_)));
        }
        match self {
            BodySource::Empty => Ok(empty_body()),
            BodySource::Full(bytes) => Ok(full_body(bytes.clone())),
            BodySource::Stream(slot) => match (slot.take(), timed) {
                (Some(receiver), Some(dispatched)) => {
                    let dispatched = dispatched.clone();
                    Ok(channel_body_then(receiver, declared, move || {
                        dispatched.body_open(false)
                    }))
                }
                (Some(receiver), None) => Ok(channel_body(receiver)),
                (None, _) => Err(format!("fetch: unknown body stream {}", slot.handle())),
            },
        }
    }

    fn request_failed(&mut self) {
        if let BodySource::Stream(slot) = self {
            slot.request_failed();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// RFC 9110 s9.2.2's idempotent methods: sending the request twice has the
/// same effect on the server as sending it once, so a request whose response
/// never started may be sent again.
pub(super) fn is_idempotent(method: &http::Method) -> bool {
    *method == http::Method::GET
        || *method == http::Method::HEAD
        || *method == http::Method::PUT
        || *method == http::Method::DELETE
        || *method == http::Method::OPTIONS
        || *method == http::Method::TRACE
}

/// The fetch op. Resolves at the response head with the payload JSON (the
/// body stays in `bodies` under `bodyHandle`), or in hook mode possibly with
/// a lookup request (see the module docs).
///
/// `net_check` is the `--permission` net grant (`None` when it covers every
/// host). It is a parameter rather than a [`FetchRequest`] field because the
/// request is JSON from JS, and nothing JS sends may widen or drop it.
///
/// `cancel` is the fetch's [`FetchCancel`], for a fetch JS may cancel before
/// its response head.
#[allow(clippy::too_many_arguments)]
pub async fn fetch(
    transport: HttpTransport,
    req: FetchRequest,
    bodies: FetchBodies,
    ids: Arc<AtomicU64>,
    outbound: OutboundBodies,
    continuations: FetchContinuations,
    net_check: Option<NetCheck>,
    cancel: Option<FetchCancel>,
) -> OpOutcome {
    // Claimed first, so every early return below releases the receiver.
    let stream = req
        .body_stream
        .map(|handle| StreamSlot::new(handle, outbound));
    let method = req.method.as_deref().unwrap_or("GET");
    let prepared = match prepare::prepare(
        &req.url,
        method,
        &req.headers,
        req.default_headers,
        transport.user_agent(),
    ) {
        Ok(prepared) => prepared,
        Err(e) => return OpOutcome::Failed(e.to_string()),
    };
    // undici mainFetch (fetch/index.js:541-543): before anything dials, and
    // before a lookup hook is consulted.
    if req.fetch_semantics && prepare::is_bad_port(&prepared.url) {
        return OpOutcome::Failed(redirect::BAD_PORT.to_string());
    }
    let source = if let Some(slot) = stream {
        BodySource::Stream(slot)
    } else if let Some(body) = req.body {
        BodySource::Full(Bytes::from(body))
    } else if let Some(encoded) = req.body_base64 {
        match base64::engine::general_purpose::STANDARD.decode(&encoded) {
            Ok(bytes) => BodySource::Full(Bytes::from(bytes)),
            Err(_) => return OpOutcome::Failed("fetch: malformed base64 body".to_string()),
        }
    } else {
        BodySource::Empty
    };
    let attempt_timeout = attempt_timeout_from_ms(req.attempt_timeout_ms);
    let tls_range = TlsRange::from_versions(
        req.tls_min_version.as_deref().filter(|v| !v.is_empty()),
        req.tls_max_version.as_deref().filter(|v| !v.is_empty()),
    );
    let route = if req.connect_hook {
        transport.supplied_route(attempt_timeout, tls_range)
    } else {
        transport
            .route(req.lookup_hook, attempt_timeout, tls_range)
            .with_connect_timeout(connect_timeout_from_ms(req.connect_timeout_ms))
            .with_alpn(req.alpn)
    };
    let headers_timeout = timeout_limit(req.headers_timeout_ms);
    let dispatched = match req.dispatched {
        Some(dispatched) => Some(dispatched),
        None => headers_timeout.map(|_| Dispatched::new()),
    };
    let state = LoopState {
        transport,
        route,
        method: prepared.method,
        current: prepared.url,
        carried: prepared.headers,
        source,
        hops: 0,
        redirect: req.redirect,
        decode: req.decode,
        net_check,
        max_header_size: req
            .max_header_size
            .unwrap_or_else(crate::http_head::max_http_header_size),
        undici_head: req.fetch_semantics || req.dispatch_semantics,
        fetch_rules: req.fetch_semantics,
        dispatched,
        cancel,
        headers_timeout,
        body_timeout: timeout_limit(req.body_timeout_ms),
        defer_head: req.defer_head,
        iterable_body: req.iterable_body,
    };
    run(state, &bodies, &ids, &continuations).await
}

/// `fetchContinue`: resume the fetch parked under `token` with its hook's
/// answer, `{"ips": ["addr", ...]}` in the hook's order (JS has already
/// applied node's address filtering). Resolves like [`fetch`]. The parked
/// fetch is consumed whatever happens: a malformed answer fails it.
pub async fn fetch_continue(
    token: u64,
    lookup: String,
    bodies: FetchBodies,
    ids: Arc<AtomicU64>,
    continuations: FetchContinuations,
) -> OpOutcome {
    let pending = take_parked(&continuations, token, Wants::Addresses);
    let Some(PendingFetch { state, key, .. }) = pending else {
        return OpOutcome::Failed(format!("fetch: lookup continuation {token} is gone"));
    };
    #[derive(serde::Deserialize)]
    struct Lookup {
        ips: Vec<String>,
    }
    let lookup: Lookup = match serde_json::from_str(&lookup) {
        Ok(lookup) => lookup,
        Err(e) => return OpOutcome::Failed(format!("fetch: malformed lookup result: {e}")),
    };
    let mut addrs = Vec::with_capacity(lookup.ips.len());
    for ip in &lookup.ips {
        // A zone id (`fe80::1%eth0`) is part of the address, as node's
        // `net.isIP` has it, and is dialled as its scope id.
        match ip.parse::<crate::net_connect::PinAddr>() {
            Ok(addr) => addrs.push(addr),
            Err(e) => {
                return OpOutcome::Failed(format!(
                    "fetch: connect pin failed: pin ip '{ip}' is not an IP: {e}"
                ));
            }
        }
    }
    // An empty list reaches the connector, which fails it as node's
    // ERR_INVALID_IP_ADDRESS (JS refuses one before it gets here).
    state.route.set_addrs(&key, addrs);
    run(state, &bodies, &ids, &continuations).await
}

/// The fetch parked under `token`, if it waits for `wants`. One that waits
/// for the other answer stays parked: a connector-mode fetch is never
/// resumed with addresses (it would park again for its connection), nor a
/// lookup-mode fetch with a connection.
fn take_parked(
    continuations: &FetchContinuations,
    token: u64,
    wants: Wants,
) -> Option<PendingFetch> {
    let mut map = lock(continuations);
    if map.get(&token)?.wants != wants {
        return None;
    }
    map.remove(&token)
}

/// `fetchSupply`: resume the connector-mode fetch parked under `token` with
/// the connection its dispatcher's `connect` function returned -- the
/// consumer end of the pipe JS pumps that socket through (`h2`: the socket
/// negotiated h2 by ALPN). Resolves like [`fetch`]. A fetch that is not
/// parked for a connection is left alone and the op fails.
pub async fn fetch_supply(
    token: u64,
    io: tokio::io::DuplexStream,
    h2: bool,
    bodies: FetchBodies,
    ids: Arc<AtomicU64>,
    continuations: FetchContinuations,
) -> OpOutcome {
    let pending = take_parked(&continuations, token, Wants::Connection);
    let Some(PendingFetch { state, key, .. }) = pending else {
        return OpOutcome::Failed(format!("fetch: connect continuation {token} is gone"));
    };
    state.route.supply(&key, SuppliedConn { io, h2 });
    run(state, &bodies, &ids, &continuations).await
}

/// The port the fetch parked under `token` for its lookup hook will dial --
/// the hop's URL's own, or the scheme's default, the port [`NetTarget`] gave
/// the net grant before it parked -- or `None` when no such fetch is parked.
/// The engine checks each address the hook answers as `address:port` with
/// it, so a port-scoped grant that admitted the hop admits its answer too.
/// Read from the parked state, never from JS.
pub fn fetch_parked_port(token: u64, continuations: &FetchContinuations) -> Option<u16> {
    let map = lock(continuations);
    let pending = map.get(&token)?;
    (pending.wants == Wants::Addresses)
        .then(|| pending.state.current.port_or_known_default())
        .flatten()
}

/// `fetchAbandon`: drop the fetch parked under `token`. True if it was there.
pub fn fetch_abandon(token: u64, continuations: &FetchContinuations) -> bool {
    let pending = lock(continuations).remove(&token);
    pending.is_some()
}

/// The request loop, from the hop in `state` to a response, a failure, or a
/// park.
async fn run(
    mut state: LoopState,
    bodies: &FetchBodies,
    ids: &AtomicU64,
    continuations: &FetchContinuations,
) -> OpOutcome {
    let response = loop {
        // The net grant, before this hop parks for its lookup hook or dials.
        // Every hop: the first (which the engine's synchronous gate also
        // checked, on its own parse of the URL JS sent) and each redirect
        // target. A hop resumed after a park is asked again -- the grant
        // cannot have changed, and asking is cheaper than tracking it.
        if let Some(denial) = net_denial(&state) {
            state.source.request_failed();
            return OpOutcome::AccessDenied(denial);
        }
        // A Follow URL can hold a host `http::Uri` refuses (`"`, `` ` ``,
        // `{`, `}`): reqwest failed those as a builder error too.
        let uri = match prepare::to_uri(&state.current) {
            Ok(uri) => uri,
            Err(text) => return OpOutcome::Failed(text.to_string()),
        };
        if let Some(key) = state.route.connection_needed(&uri) {
            // undici's connector parameters (dispatcher/client.js connect):
            // `host` / `hostname` / `protocol` / `port` of the origin URL --
            // the host with its port when the URL names one, the hostname
            // unbracketed, the port a string, '' for the scheme's default.
            let url = &state.current;
            let hostname = url.host_str().unwrap_or_default();
            let unbracketed = hostname
                .strip_prefix('[')
                .and_then(|h| h.strip_suffix(']'))
                .unwrap_or(hostname)
                .to_string();
            let port = url.port().map(|p| p.to_string()).unwrap_or_default();
            let host = if port.is_empty() {
                hostname.to_string()
            } else {
                format!("{hostname}:{port}")
            };
            let token = ids.fetch_add(1, Ordering::Relaxed);
            let payload = serde_json::json!({
                "connect": {
                    "token": token,
                    "host": host,
                    "hostname": unbracketed,
                    "protocol": format!("{}:", url.scheme()),
                    "port": port,
                },
            });
            lock(continuations).insert(
                token,
                PendingFetch {
                    state,
                    host: unbracketed,
                    key,
                    wants: Wants::Connection,
                },
            );
            return OpOutcome::Json(payload.to_string());
        }
        if let Some((key, host)) = state.route.lookup_needed(&uri) {
            // The port as undici's connector hands it to net.connect, which
            // is what node's ERR_INVALID_ADDRESS_FAMILY carries: the URL's
            // port STRING when the URL names one, else the number 80 / 443
            // (`port || 80`, undici core/connect.js; measured: `port` is
            // "4567" for `:4567` and 80 for no port). A default port never
            // survives URL parsing, so an explicit one is never 80 on http.
            let port = match uri.port_u16() {
                Some(port) => serde_json::Value::from(port.to_string()),
                None if uri.scheme_str() == Some("https") => serde_json::Value::from(443),
                None => serde_json::Value::from(80),
            };
            let token = ids.fetch_add(1, Ordering::Relaxed);
            let payload = serde_json::json!({
                "lookup": { "token": token, "host": host, "port": port },
            });
            lock(continuations).insert(
                token,
                PendingFetch {
                    state,
                    host,
                    key,
                    wants: Wants::Addresses,
                },
            );
            return OpOutcome::Json(payload.to_string());
        }
        // proxy-authorization is per hop and never carried: it is computed
        // after the cross-origin strip, for this hop's proxy. Only a hop that
        // adds it gets a header map of its own; every other hop sends the
        // carried map, copied once per attempt below and not once more here
        // (#183: a plain request copied its headers twice).
        let with_proxy_auth = match state.transport.proxy_authorization(&state.route, &uri) {
            Some(auth) if !state.carried.contains_key(PROXY_AUTHORIZATION) => {
                let mut headers = state.carried.clone();
                if headers.try_insert(PROXY_AUTHORIZATION, auth).is_err() {
                    return OpOutcome::Failed(PrepareError::TooManyHeaders.to_string());
                }
                Some(headers)
            }
            _ => None,
        };
        let hop_headers = with_proxy_auth.as_ref().unwrap_or(&state.carried);

        // The rules a 3xx answer to this hop is followed by, if it is: the
        // pool then keeps a next hop that could not be sent twice off the
        // connection the 3xx came on (#155).
        let follows = (state.redirect == RedirectMode::Follow).then_some(
            if state.undici_head && !state.fetch_rules {
                redirect::Rules::Undici
            } else {
                redirect::Rules::Fetch
            },
        );
        let mut retries = 0;
        let mut stale_resent = false;
        let response = loop {
            let timed = state.headers_timeout.and(state.dispatched.as_ref());
            let declared = hop_headers
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok()?.trim().parse::<u64>().ok());
            let body = match state.source.build(timed, declared) {
                Ok(body) => body,
                Err(text) => return OpOutcome::Failed(text),
            };
            let mut request = http::Request::new(body);
            *request.method_mut() = state.method.clone();
            *request.uri_mut() = uri.clone();
            *request.headers_mut() = hop_headers.clone();
            if let Some(dispatched) = &state.dispatched {
                request.extensions_mut().insert(dispatched.clone());
            }
            if matches!(state.source, BodySource::Stream(_))
                && let Some(defer) = state.defer_head.take()
            {
                request
                    .extensions_mut()
                    .insert(super::transport::HeadAwaitsBody {
                        empty_content_length: defer.empty_content_length,
                    });
            }
            // The one place the loop waits. Each send (a hop, a resend) gets
            // its own headersTimeout, started by its own checkout: a head that
            // is late fails the hop, and dropping the send drops the request
            // (hyper closes its connection, as undici destroys the socket). A
            // cancel drops the send too: a connect in progress is abandoned,
            // an h1 connection the request went out on is closed (hyper shuts
            // a connection whose response nobody waits for), an h2 stream is
            // reset -- which is how the server learns the client left.
            let send = state
                .transport
                .send_following(&state.route, request, follows);
            let headers_timeout = state.headers_timeout;
            let dispatched = state.dispatched.as_ref();
            let timed = async move {
                match (headers_timeout, dispatched) {
                    (Some(limit), Some(dispatched)) => {
                        let deadline = super::sent::headers_deadline(dispatched.subscribe(), limit);
                        tokio::select! {
                            biased;
                            sent = send => Some(sent),
                            () = deadline => None,
                        }
                    }
                    _ => Some(send.await),
                }
            };
            // Outer None: cancelled. Inner None: the head was late.
            let sent = match &state.cancel {
                Some(cancel) => tokio::select! {
                    biased;
                    () = cancel.cancelled() => None,
                    sent = timed => Some(sent),
                },
                None => Some(timed.await),
            };
            let Some(sent) = sent else {
                state.source.request_failed();
                return OpOutcome::Failed(ABORTED.to_string());
            };
            let Some(sent) = sent else {
                state.source.request_failed();
                return headers_timed_out();
            };
            match sent {
                Ok(response) => break response,
                Err(e)
                    if retries < MAX_H2_RETRIES
                        && state.source.replayable()
                        && e.is_h2_retryable() =>
                {
                    retries += 1;
                }
                // A pooled connection the server had already closed: no part
                // of a response arrived, so the request may go out again
                // (RFC 9112 s9.6). Only for a body that can be sent twice,
                // and only for an idempotent method -- oam DID put the
                // request on the wire and cannot know the server ignored it.
                //
                // Only on a REUSED connection: a fresh one that dies before
                // the response is the server's answer to this request, not a
                // stale pool entry, and node sends such a request once (a
                // server that closes every connection unanswered saw a GET
                // twice from oam when this retried on any connection).
                //
                // Only when NOT A BYTE of a response arrived: hyper reports a
                // connection that closed halfway through a response head with
                // the same IncompleteMessage, and a server that had started
                // answering did not ignore the request. RFC 9110 s9.2.2's
                // example of a guess worth making is a connection that
                // "closed before any part of a response is received".
                //
                // Once per hop: RFC 9110 s9.2.2 "SHOULD NOT automatically
                // retry a failed automatic retry", and node sends it once and
                // rejects (undici fails the request on the wire with
                // UND_ERR_SOCKET, http.Agent with "socket hang up"). A resend
                // that meets another closing connection fails the fetch.
                // oam needs this one resend at all only because it can write
                // into a FIN that is still in flight sooner than node does;
                // a FIN the kernel already holds is read before the write
                // (`connector::EagerTcp`), and the request goes back UNSENT.
                // A redirect hop that may not be resent never meets the FIN
                // the 3xx's connection is closing with: it never takes that
                // connection (`pool::retires_for_the_hop`).
                Err(e)
                    if !stale_resent
                        && state.source.replayable()
                        && is_idempotent(&state.method)
                        && e.is_incomplete_message()
                        && e.on_reused_connection()
                        && !e.response_started() =>
                {
                    stale_resent = true;
                }
                Err(e) => {
                    state.source.request_failed();
                    // A malformed response head is the parser's error, as
                    // bridge.rs exchange_error reports it on the agent paths.
                    if let Some(parse) = e.head_parse_outcome(state.undici_head) {
                        return parse;
                    }
                    // The URL a failure names, without the fragment. Built
                    // here, on the failure, rather than for every hop (#183).
                    let mut hop_url = state.current.clone();
                    hop_url.set_fragment(None);
                    return e.to_outcome(&hop_url);
                }
            }
        };
        // Every hop's head, a redirect's included: node's parser refuses an
        // oversized head before anything looks at its status.
        if let Some(refusal) =
            response_head_overflow(&response, state.max_header_size, state.undici_head)
        {
            drop(response);
            state.source.request_failed();
            return refusal;
        }

        match state.redirect {
            RedirectMode::Manual => break response,
            // undici checks the status alone: a 3xx without a Location fails
            // too, and nothing is requested from where it points.
            RedirectMode::Error if redirect::is_redirect_status(response.status().as_u16()) => {
                drop(response);
                state.source.request_failed();
                return OpOutcome::Failed(redirect::UNEXPECTED_REDIRECT.to_string());
            }
            RedirectMode::Error | RedirectMode::Follow => {}
        }
        let location = redirect::location(response.headers());
        // undici.request: undici's dispatcher, and not a fetch.
        let undici_request = state.undici_head && !state.fetch_rules;
        let redirect_body = match (state.source.replayable(), state.fetch_rules) {
            (true, false) if undici_request => redirect::RedirectBody::UndiciReplayable,
            (true, _) => redirect::RedirectBody::Replayable,
            (false, true) => redirect::RedirectBody::StreamedFetch,
            (false, false) if state.iterable_body => redirect::RedirectBody::UndiciIterable,
            (false, false) => redirect::RedirectBody::Streamed,
        };
        match redirect::next(
            response.status().as_u16(),
            &state.method,
            &state.current,
            location.as_ref(),
            state.hops,
            redirect_body,
        ) {
            // ReturnResponse: the hop must resend a streamed body, which
            // cannot be replayed -- the 3xx is the result, as undici's
            // RedirectHandler returns it for `undici.request`. (A fetch's
            // streamed body fails the hop instead: RedirectBody::StreamedFetch.)
            Next::Done | Next::ReturnResponse => break response,
            Next::Fail(text) => {
                state.source.request_failed();
                return OpOutcome::Failed(text.to_string());
            }
            // Not a rejection of the op: the cause is the error node's
            // `new URL(location, base)` throws, which carries both strings,
            // and JS builds it from this payload (bootstrap.js settleRaw).
            Next::InvalidLocation { input } => {
                state.source.request_failed();
                let payload = serde_json::json!({
                    "invalidLocation": { "input": input, "base": state.current.as_str() },
                });
                return OpOutcome::Json(payload.to_string());
            }
            Next::Follow {
                url,
                method,
                drop_body,
            } => {
                // An unread 3xx body: its h1 connection is closed, not
                // pooled (reqwest did the same).
                drop(response);
                redirect::apply(&mut state.carried, &state.current, &url, drop_body);
                if drop_body {
                    state.source = BodySource::Empty;
                } else if redirect_body == redirect::RedirectBody::UndiciIterable {
                    // undici sends the spent iterable again: no bytes, with
                    // the length that says so.
                    state.source = BodySource::Empty;
                    state.carried.remove(http::header::TRANSFER_ENCODING);
                    state
                        .carried
                        .insert(CONTENT_LENGTH, http::HeaderValue::from_static("0"));
                }
                state.method = method;
                state.current = url;
                state.hops += 1;
            }
        }
    };
    respond(state, response, bodies, ids)
}

/// node's response-head limit (`maxHeaderSize`, 16 KiB by default): the
/// refusal when `response`'s head is at or over `limit`, counted the way the
/// API that sent the request counts it (measured on node v22.22.2):
///
/// - `fetch` and `undici.request` (undici, `undici_head`): header names plus
///   values. It fails with `UND_ERR_HEADERS_OVERFLOW` / `Headers Overflow
///   Error` (undici's `HeadersOverflowError`): `fetch` as the cause of
///   `TypeError: fetch failed`, `undici.request` with the error itself.
/// - `http.request` (node's own parser): the status line's reason phrase
///   too. It fails with `Parse Error: Header overflow`, code
///   `HPE_HEADER_OVERFLOW`.
///
/// The count comes from the parsed head, so trailing whitespace in a value,
/// which node counts and the parser trims, is not counted. A head too large
/// for hyper's read buffer (~400 KiB) already fails the request as a
/// connection error. The agent-socket exchange ([`super::bridge`]) applies
/// the `http.request` count through this too.
pub(super) fn response_head_overflow(
    response: &http::Response<Incoming>,
    limit: u64,
    undici_head: bool,
) -> Option<OpOutcome> {
    let mut count: u64 = response
        .headers()
        .iter()
        .map(|(name, value)| (name.as_str().len() + value.as_bytes().len()) as u64)
        .sum();
    if !undici_head && response.version() < http::Version::HTTP_2 {
        count += match response.extensions().get::<hyper::ext::ReasonPhrase>() {
            Some(reason) => reason.as_bytes().len(),
            None => response
                .status()
                .canonical_reason()
                .unwrap_or_default()
                .len(),
        } as u64;
    }
    if count < limit {
        return None;
    }
    Some(if undici_head {
        OpOutcome::node_failed("UND_ERR_HEADERS_OVERFLOW", "Headers Overflow Error")
    } else {
        OpOutcome::node_failed("HPE_HEADER_OVERFLOW", "Parse Error: Header overflow")
    })
}

/// The net grant's verdict on the hop in `state`, or `None` when it may be
/// dialled (or no grant restricts it).
///
/// The host is the one the connector will resolve: `host_str` of the same
/// `url::Url` [`prepare::to_uri`] turns into the request URI, so no spelling
/// of the Location (case, percent-encoding, a non-canonical IPv4 or IPv6
/// literal, userinfo) can put one host in front of the check and another in
/// front of the dialler. An http(s) URL always has a host; were one ever to
/// lack it, the check is asked about the empty host, which no allow-list
/// grants.
fn net_denial(state: &LoopState) -> Option<crate::AccessDenial> {
    let check = state.net_check.as_ref()?;
    let url = &state.current;
    let target = NetTarget {
        host: url.host_str().unwrap_or_default(),
        port: url.port_or_known_default().unwrap_or_default(),
    };
    check(&target).err()
}

/// A response header value as JS sees it: latin1, one code point per byte.
///
/// undici holds header values as latin1 (`value.toString('latin1')`), so a
/// `content-disposition` filename or a legacy vendor header carrying obs-text
/// (0x80-0xFF) reads back byte for byte. Measured on node v22.22.2 with a raw
/// `x-latin1: caf\xe9` header: node reports `café` (code point 0xE9). A lossy
/// UTF-8 decode turned that byte into U+FFFD, which is IRREVERSIBLE -- the
/// caller could not recover it -- and a value whose bytes happened to be
/// valid UTF-8 (`a=\xe2\x82\xac`) came back as a different string than node
/// reports.
///
/// `redirect::resolve_location` keeps its UTF-8-lossy decode on purpose:
/// undici really does read `Location` as
/// `Buffer.from(location, 'binary').toString('utf8')`.
fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// The reason phrase of `response`'s status line, as node reports it in
/// `statusText` / `statusMessage`: the one the server sent, an empty one
/// included (#160). hyper keeps a phrase that is not the status code's
/// canonical one as an extension, so without the extension the canonical
/// phrase IS what was on the wire. HTTP/2 has no reason phrase, and node's
/// fetch over it -- a dispatcher with `allowH2` -- reports `''` (measured on
/// v22.22.2 + undici 6.24.1).
pub(super) fn reason_phrase<B>(response: &http::Response<B>) -> String {
    if response.version() == http::Version::HTTP_2 {
        return String::new();
    }
    match response.extensions().get::<hyper::ext::ReasonPhrase>() {
        Some(reason) => latin1(reason.as_bytes()),
        None => response
            .status()
            .canonical_reason()
            .unwrap_or_default()
            .to_string(),
    }
}

/// The payload for the final response; its body goes into `bodies`.
fn respond(
    mut state: LoopState,
    response: http::Response<Incoming>,
    bodies: &FetchBodies,
    ids: &AtomicU64,
) -> OpOutcome {
    let status = response.status();
    let reason = reason_phrase(&response);
    let conn = response.extensions().get::<ConnInfo>().cloned();
    let codings = if state.decode {
        let encodings: Vec<&[u8]> = response
            .headers()
            .get_all(CONTENT_ENCODING)
            .iter()
            .map(|v| v.as_bytes())
            .collect();
        match decode::plan(&state.method, status.as_u16(), &encodings) {
            Plan::TooMany(n) => {
                state.source.request_failed();
                return OpOutcome::Failed(format!(
                    "too many content-encodings in response: {n}, maximum allowed is {MAX_CODINGS}"
                ));
            }
            Plan::Identity => None,
            Plan::Decode(codings) => Some(codings),
        }
    } else {
        None
    };
    // Divergence #32: a decoded body loses content-encoding and
    // content-length (node's fetch keeps both). http.request shares this op
    // but asks for no decoding, so it sees both headers and the encoded
    // bytes, as node's does.
    let strip = codings.is_some();
    // Each value without the whitespace around it (RFC 9110 section 5.5), on
    // either protocol. Over HTTP/1 the parser under hyper has already trimmed
    // it, so this finds nothing to cut; an HTTP/2 value arrives as sent, and
    // without the trim `x-ows:   a<TAB>b   ` read `"   a\tb   "` over h2 and
    // `"a\tb"` over HTTP/1 from the same server (#182).
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter(|(name, _)| !(strip && (*name == CONTENT_ENCODING || *name == CONTENT_LENGTH)))
        .map(|(name, value)| {
            let value = crate::http_head::trim_ows(value.as_bytes());
            (name.as_str().to_string(), latin1(value))
        })
        .collect();
    // node's Response.url never carries the fragment.
    let mut url = state.current;
    url.set_fragment(None);
    let handle = ids.fetch_add(1, Ordering::Relaxed);
    let framing = super::body::Framing::of(response.version(), response.headers());
    let body = FetchBody::new(response.into_body(), codings.as_deref())
        .with_framing(framing)
        .timed(state.body_timeout)
        // A close mid-body carries the connection's facts (undici's
        // SocketError; http.request makes it node's `aborted`).
        .on_conn(conn.clone())
        // http.request (the one caller that reads the body undecoded) fills
        // its response's trailers from the body's trailer section.
        .keeping_trailers(!state.decode);
    lock(bodies).insert(handle, body);
    let mut payload = serde_json::json!({
        "status": status.as_u16(),
        "statusText": reason,
        "url": url.as_str(),
        "redirected": state.hops > 0,
        "headers": headers,
        "bodyHandle": handle,
    });
    if let Some(conn) = &conn {
        conn_payload(&mut payload, conn);
    }
    OpOutcome::Json(payload.to_string())
}

/// The connection a response arrived on, as `http.request` reports it on
/// `req.socket` / `res.socket`: `socket` holds the dialled peer and the
/// local end (`{address, port, family}`, the net.connect shape), `tls` an
/// https origin's session in `tls.connect`'s spelling. fetch() ignores both.
fn conn_payload(payload: &mut serde_json::Value, conn: &ConnInfo) {
    let mut socket = serde_json::Map::new();
    if let Some(peer) = conn.peer {
        socket.insert("remoteAddr".to_string(), crate::tcp::addr_to_json(peer));
    }
    if let Some(local) = conn.local {
        socket.insert("localAddr".to_string(), crate::tcp::addr_to_json(local));
    }
    // What `req.socket.destroy()` / `resetAndDestroy()` close it by
    // (`__oam.fetchConnClose`).
    if let Some(connection) = conn.connection {
        socket.insert("connection".to_string(), connection.into());
    }
    // Which checkout of it this response came on: the close is refused once
    // another request has taken the connection.
    if let Some(lease) = conn.lease {
        socket.insert("lease".to_string(), lease.into());
    }
    payload["socket"] = serde_json::Value::Object(socket);
    if let Some(tls) = &conn.tls {
        payload["tls"] = serde_json::json!({
            "protocol": tls.protocol,
            "cipher": tls.cipher,
            "cipherStandardName": tls.cipher_standard_name,
            "alpnProtocol": tls.alpn,
            "peerCertificates": tls.peer_certificates,
        });
    }
}
