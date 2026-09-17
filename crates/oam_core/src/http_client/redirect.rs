//! The `redirect: "follow"` step of Node's fetch: undici 6.24.1
//! `httpRedirectFetch` (fetch/index.js:1210-1318), as a pure function of one
//! 3xx response plus the header rewrite applied before the next hop.
//!
//! Measured against node v22.22.2 (review-behaviour b2/rnode.txt and the
//! #143 slice-B probes), and where oam 0.16.1 (reqwest) differed:
//!
//! | rule | node | reqwest |
//! |---|---|---|
//! | redirect limit | 21 requests, then `redirect count exceeded` | 11 requests |
//! | `Referer` | never added | added on every hop |
//! | cross-origin hop | drops authorization, proxy-authorization, cookie, host -- for good | re-sent them on the next same-host hop; kept a user `host` |
//! | 303 on GET/HEAD | method and content-type kept | content-type dropped |
//! | cookie2 / www-authenticate | forwarded cross-origin | dropped |
//! | unparseable Location | network error `Invalid URL` | returned the 3xx |
//! | non-http(s) Location | `URL scheme must be a HTTP(S) scheme` | `builder error for url (...)` |
//! | Location with userinfo | network error (see [`CREDENTIALS`]) | converted to Basic auth |
//! | Location on a bad port (e.g. 25) | network error `bad port`, not sent | followed |

use http::header::{
    AUTHORIZATION, CONTENT_ENCODING, CONTENT_LANGUAGE, CONTENT_LENGTH, CONTENT_LOCATION,
    CONTENT_TYPE, COOKIE, HOST, HeaderMap, HeaderValue, LOCATION, PROXY_AUTHORIZATION,
};

use super::prepare::{is_bad_port, origin_eq};

/// undici fetch/index.js:1247: `if (request.redirectCount === 20)` is checked
/// before the increment, so twenty redirects are followed and the 21st 3xx
/// fails -- 21 requests go out.
pub const MAX_REDIRECTS: u32 = 20;

/// A Location that does not parse against the current URL. undici wraps the
/// `new URL` TypeError, whose message is this.
pub const INVALID_URL: &str = "Invalid URL";
/// fetch/index.js:1242.
pub const BAD_SCHEME: &str = "URL scheme must be a HTTP(S) scheme";
/// fetch/index.js:1249.
pub const COUNT_EXCEEDED: &str = "redirect count exceeded";
/// fetch/index.js:1258-1264. Node's fetch runs with request mode "cors" and an
/// opaque client origin, so a Location carrying userinfo is refused whether
/// or not it points at the same origin -- measured on node v22.22.2 for both
/// `http://u:p@<same host:port>/` and a different host.
pub const CREDENTIALS: &str = "cross origin not allowed for request mode \"cors\"";
/// fetch/index.js:541-543, run by `mainFetch` for the hop the redirect starts
/// (fetch/index.js:1351). See [`super::prepare::is_bad_port`].
pub const BAD_PORT: &str = "bad port";

/// What to do with a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// Not a redirect to follow (not a redirect status, or no Location): this
    /// response is the result.
    Done,
    /// Issue the next request. `url` keeps the fragment undici carries along
    /// the URL list (a Location without one inherits the current URL's);
    /// [`super::prepare::to_uri`] drops it for the wire, and Node's
    /// `Response.url` never shows it. `drop_body` means the next request has
    /// no body and [`apply`] must strip the body headers.
    Follow {
        url: url::Url,
        method: http::Method,
        drop_body: bool,
    },
    /// The next request must resend a body that cannot be replayed (a
    /// streamed upload): the 3xx is returned as the response. undici fails
    /// here instead (fetch/index.js:1273-1279); oam keeps reqwest's behaviour
    /// until #149/#148 (design-143 decision, section 0).
    ReturnResponse,
    /// A network error with this message as the cause.
    Fail(&'static str),
}

/// True for the statuses undici follows (constants.js:8).
pub fn is_redirect_status(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// The Location value undici reads: `headersList.get('location', true)` joins
/// repeated lines with ", " -- node follows `Location: /a` + `Location: /b`
/// to the relative URL `/a, /b` (measured, `/a,%20/b`).
pub fn location(headers: &HeaderMap) -> Option<HeaderValue> {
    let mut values = headers.get_all(LOCATION).iter();
    let first = values.next()?;
    let Some(second) = values.next() else {
        return Some(first.clone());
    };
    let mut joined = first.as_bytes().to_vec();
    for value in std::iter::once(second).chain(values) {
        joined.extend_from_slice(b", ");
        joined.extend_from_slice(value.as_bytes());
    }
    // Joining legal values with ", " yields a legal value.
    HeaderValue::from_bytes(&joined).ok()
}

/// Decide the next step for a response with `status` to a request with
/// `method` for `current`. `location` is [`location`]'s value;
/// `redirects_so_far` counts the redirects already followed for this fetch;
/// `body_replayable` is true when the request has no body or a buffered one.
///
/// The checks run in undici's order: Location parse, scheme, count,
/// credentials, then the method/body rewrite, and last the bad-port check
/// `mainFetch` runs on the new URL before it dials.
pub fn next(
    status: u16,
    method: &http::Method,
    current: &url::Url,
    location: Option<&HeaderValue>,
    redirects_so_far: u32,
    body_replayable: bool,
) -> Next {
    if !is_redirect_status(status) {
        return Next::Done;
    }
    let Some(location) = location else {
        return Next::Done;
    };
    let mut target = match resolve_location(location.as_bytes(), current) {
        Some(url) => url,
        None => return Next::Fail(INVALID_URL),
    };
    if !matches!(target.scheme(), "http" | "https") {
        return Next::Fail(BAD_SCHEME);
    }
    if redirects_so_far >= MAX_REDIRECTS {
        return Next::Fail(COUNT_EXCEEDED);
    }
    if !target.username().is_empty() || target.password().is_some() {
        return Next::Fail(CREDENTIALS);
    }
    // util.js responseLocationURL step 4: a Location without a fragment (or
    // with an empty one -- `URL.hash` is "" for both) takes the current URL's.
    if target.fragment().is_none_or(str::is_empty) {
        target.set_fragment(current.fragment().filter(|f| !f.is_empty()));
    }
    // fetch/index.js:1297-1305: only these two cases turn the request into a
    // body-less GET. A 303 answering GET or HEAD keeps its method and
    // headers (content-type included -- reqwest dropped it).
    let rewrite = (matches!(status, 301 | 302) && *method == http::Method::POST)
        || (status == 303 && *method != http::Method::GET && *method != http::Method::HEAD);
    if !rewrite && !body_replayable {
        return Next::ReturnResponse;
    }
    // fetch/index.js:1351 hands the hop to `mainFetch`, whose first network
    // decision is the bad-port block (index.js:541-543): the fetch fails
    // before the next request is sent.
    if is_bad_port(&target) {
        return Next::Fail(BAD_PORT);
    }
    if rewrite {
        return Next::Follow {
            url: target,
            method: http::Method::GET,
            drop_body: true,
        };
    }
    Next::Follow {
        url: target,
        method: method.clone(),
        drop_body: false,
    }
}

/// Rewrite the carried request headers for the hop `from` -> `to`.
///
/// `headers` is the map the loop carries from hop to hop, so a strip here is
/// permanent: after a cross-origin hop, a later hop back to the first origin
/// does not get the credentials back (fetch spec; reqwest re-derived them per
/// hop and re-sent them). No `Referer` is ever added, and cookie2 and
/// www-authenticate are forwarded, as node does.
pub fn apply(headers: &mut HeaderMap, from: &url::Url, to: &url::Url, drop_body: bool) {
    if drop_body {
        // undici constants.js:61-71 `requestBodyHeader`.
        for name in [
            CONTENT_ENCODING,
            CONTENT_LANGUAGE,
            CONTENT_LOCATION,
            CONTENT_TYPE,
            CONTENT_LENGTH,
        ] {
            headers.remove(name);
        }
    }
    if !origin_eq(from, to) {
        // fetch/index.js:1308-1318. `host` goes too: a user Host header must
        // not follow the request to a different server.
        for name in [AUTHORIZATION, PROXY_AUTHORIZATION, COOKIE, HOST] {
            headers.remove(name);
        }
    }
}

/// util.js responseLocationURL step 3: a header value with leading or
/// trailing HTAB/SP, or containing NUL, CR or LF, is not parsed (undici then
/// throws assigning `.hash` to the string -- a network error; no response off
/// the wire gets here, as httparse trims OWS and refuses NUL, CR and LF in a
/// value, so the message is not worth mirroring); one with a byte outside
/// 0x20-0x7E is taken as UTF-8
/// (`Buffer.from(value, 'binary').toString('utf8')`, replacement characters
/// for invalid sequences); the result is parsed against the current URL.
fn resolve_location(raw: &[u8], current: &url::Url) -> Option<url::Url> {
    let bad_edge = |b: Option<&u8>| matches!(b, Some(b'\t' | b' '));
    if bad_edge(raw.first())
        || bad_edge(raw.last())
        || raw.iter().any(|b| matches!(b, 0 | b'\r' | b'\n'))
    {
        return None;
    }
    let text = String::from_utf8_lossy(raw);
    current.join(&text).ok()
}
