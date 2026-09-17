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

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use base64::Engine as _;
use bytes::Bytes;
use http::header::{CONTENT_ENCODING, CONTENT_LENGTH, HeaderMap, PROXY_AUTHORIZATION};
use hyper::body::Incoming;

use super::body::{FetchBodies, FetchBody, StreamSlot};
use super::decode::{self, MAX_CODINGS, Plan};
use super::prepare::{self, PrepareError};
use super::redirect::{self, Next};
use super::transport::{channel_body, empty_body, full_body};
use super::{HttpTransport, ReqBody, Route};
use crate::OpOutcome;
use crate::OutboundBodies;
use crate::net_connect::attempt_timeout_from_ms;

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
    /// `net.getDefaultAutoSelectFamilyAttemptTimeout()` at call time.
    /// Missing or not a positive number: node's 250 ms.
    #[serde(default)]
    pub attempt_timeout_ms: Option<f64>,
    /// True for `fetch()`: undici's bad-port block on the initial URL.
    /// `http.request`, `undici.request` and the http2 compat client leave it
    /// false -- node's http.request has no such check. Redirect hops are
    /// checked either way (redirect::next).
    #[serde(default)]
    pub fetch_semantics: bool,
    /// #149's knob; JS does not send it yet.
    #[serde(default)]
    pub redirect: RedirectMode,
    /// #148's knob: false delivers the body as received and keeps the
    /// encoding headers. JS does not send it yet.
    #[serde(default = "yes")]
    pub decode: bool,
    /// #148's knob: false sends the caller's headers alone. JS does not send
    /// it yet.
    #[serde(default = "yes")]
    pub default_headers: bool,
}

fn yes() -> bool {
    true
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
}

impl PendingFetch {
    /// The host the hook is resolving.
    pub fn host(&self) -> &str {
        &self.host
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
    fn build(&mut self) -> Result<ReqBody, String> {
        match self {
            BodySource::Empty => Ok(empty_body()),
            BodySource::Full(bytes) => Ok(full_body(bytes.clone())),
            BodySource::Stream(slot) => match slot.take() {
                Some(receiver) => Ok(channel_body(receiver)),
                None => Err(format!("fetch: unknown body stream {}", slot.handle())),
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
fn is_idempotent(method: &http::Method) -> bool {
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
pub async fn fetch(
    transport: HttpTransport,
    req: FetchRequest,
    bodies: FetchBodies,
    ids: Arc<AtomicU64>,
    outbound: OutboundBodies,
    continuations: FetchContinuations,
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
    let route = transport.route(
        req.lookup_hook,
        attempt_timeout_from_ms(req.attempt_timeout_ms),
    );
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
    let pending = lock(&continuations).remove(&token);
    let Some(PendingFetch {
        state,
        host: _,
        key,
    }) = pending
    else {
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
        match ip.parse::<IpAddr>() {
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
        // A Follow URL can hold a host `http::Uri` refuses (`"`, `` ` ``,
        // `{`, `}`): reqwest failed those as a builder error too.
        let uri = match prepare::to_uri(&state.current) {
            Ok(uri) => uri,
            Err(text) => return OpOutcome::Failed(text.to_string()),
        };
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
            lock(continuations).insert(token, PendingFetch { state, host, key });
            return OpOutcome::Json(payload.to_string());
        }
        let mut hop_url = state.current.clone();
        hop_url.set_fragment(None);

        // proxy-authorization is per hop and never carried: it is computed
        // after the cross-origin strip, for this hop's proxy.
        let mut hop_headers = state.carried.clone();
        if let Some(auth) = state.transport.proxy_authorization(&state.route, &uri)
            && !hop_headers.contains_key(PROXY_AUTHORIZATION)
            && hop_headers.try_insert(PROXY_AUTHORIZATION, auth).is_err()
        {
            return OpOutcome::Failed(PrepareError::TooManyHeaders.to_string());
        }

        let mut retries = 0;
        let mut stale_retried = false;
        let response = loop {
            let body = match state.source.build() {
                Ok(body) => body,
                Err(text) => return OpOutcome::Failed(text),
            };
            let mut request = http::Request::new(body);
            *request.method_mut() = state.method.clone();
            *request.uri_mut() = uri.clone();
            *request.headers_mut() = hop_headers.clone();
            match state.transport.send(&state.route, request).await {
                Ok(response) => break response,
                Err(e)
                    if retries < MAX_H2_RETRIES
                        && state.source.replayable()
                        && e.is_h2_retryable() =>
                {
                    retries += 1;
                }
                // A pooled connection the server had already closed: no part
                // of a response arrived, so the request may go out again on a
                // fresh one (RFC 9112 s9.6). Once per hop, only for a body
                // that can be sent twice, and only for an idempotent method
                // -- oam DID put the request on the wire and cannot know the
                // server ignored it. node loses this race far less often
                // because its event loop reads the FIN before it writes.
                Err(e)
                    if !stale_retried
                        && state.source.replayable()
                        && is_idempotent(&state.method)
                        && e.is_incomplete_message() =>
                {
                    stale_retried = true;
                }
                Err(e) => {
                    state.source.request_failed();
                    return e.to_outcome(&hop_url);
                }
            }
        };

        if state.redirect == RedirectMode::Manual {
            break response;
        }
        let location = redirect::location(response.headers());
        match redirect::next(
            response.status().as_u16(),
            &state.method,
            &state.current,
            location.as_ref(),
            state.hops,
            state.source.replayable(),
        ) {
            // ReturnResponse: the hop must resend a streamed body, which
            // cannot be replayed -- the 3xx is the result (reqwest's
            // behaviour, kept until #149/#148).
            Next::Done | Next::ReturnResponse => break response,
            Next::Fail(text) => {
                state.source.request_failed();
                return OpOutcome::Failed(text.to_string());
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
                }
                state.method = method;
                state.current = url;
                state.hops += 1;
            }
        }
    };
    respond(state, response, bodies, ids)
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

/// The payload for the final response; its body goes into `bodies`.
fn respond(
    mut state: LoopState,
    response: http::Response<Incoming>,
    bodies: &FetchBodies,
    ids: &AtomicU64,
) -> OpOutcome {
    let status = response.status();
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
    // content-length (node keeps both). http.request shares this op and
    // would decode a second time from the header.
    let strip = codings.is_some();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter(|(name, _)| !(strip && (*name == CONTENT_ENCODING || *name == CONTENT_LENGTH)))
        .map(|(name, value)| (name.as_str().to_string(), latin1(value.as_bytes())))
        .collect();
    // node's Response.url never carries the fragment.
    let mut url = state.current;
    url.set_fragment(None);
    let handle = ids.fetch_add(1, Ordering::Relaxed);
    let body = FetchBody::new(response.into_body(), codings.as_deref());
    lock(bodies).insert(handle, body);
    let payload = serde_json::json!({
        "status": status.as_u16(),
        "statusText": status.canonical_reason().unwrap_or_default(),
        "url": url.as_str(),
        "redirected": state.hops > 0,
        "headers": headers,
        "bodyHandle": handle,
    });
    OpOutcome::Json(payload.to_string())
}
