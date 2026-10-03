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
//! | unparseable Location | network error, cause the `ERR_INVALID_URL` TypeError | returned the 3xx |
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

/// The message of a Location undici cannot use. One that does not parse
/// against the current URL is [`Next::InvalidLocation`], whose cause is the
/// `new URL` TypeError with this message; a header value undici never hands
/// to `new URL` (see `resolve_location`) fails with this text alone.
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
/// fetch/index.js `httpFetch`: a redirect status under `redirect: "error"`
/// is `makeNetworkError('unexpected redirect')`, Location or not.
pub const UNEXPECTED_REDIRECT: &str = "unexpected redirect";
/// fetch/index.js httpRedirectFetch step 11: a redirect other than a 303
/// that would have to resend a streamed body is a network error with no
/// reason, so the cause is an `Error` with an empty message (measured on node
/// v22.22.2).
pub const STREAMED_BODY: &str = "";

/// A request's body, as a redirect sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectBody {
    /// None, or a buffered one, on a `fetch`: it can be sent again.
    Replayable,
    /// A Readable, on a request that is not a fetch (`undici.request` with
    /// `maxRedirections`): every 3xx comes back as the response, as undici's
    /// RedirectHandler hands it back for a stream it has already read
    /// (`util.isDisturbed`).
    Streamed,
    /// Streamed, on a `fetch`: a network error, unless the redirect is a 303
    /// (which drops the body).
    StreamedFetch,
    /// None, or a buffered one, on `undici.request`: undici's
    /// RedirectHandler rule, where only a 303 turns the request into a
    /// body-less GET and every other redirect resends method and body.
    UndiciReplayable,
    /// An iterable or a web stream, on `undici.request`: undici's rule as for
    /// [`RedirectBody::UndiciReplayable`], but what it sends again is the
    /// spent iterable -- no bytes, `content-length: 0` (the caller empties
    /// the body).
    UndiciIterable,
}

impl From<bool> for RedirectBody {
    /// `true`: [`RedirectBody::Replayable`]; `false`: [`RedirectBody::Streamed`].
    fn from(replayable: bool) -> RedirectBody {
        if replayable {
            RedirectBody::Replayable
        } else {
            RedirectBody::Streamed
        }
    }
}

/// [`STREAMED_BODY`] by the name fetch-body-headers' tests use for it.
pub const UNREPLAYABLE_BODY: &str = STREAMED_BODY;

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
    /// The response is a redirect that would have to send a streamed body
    /// again, on a request that is not a fetch (`undici.request`): the 3xx is
    /// the response, as undici's RedirectHandler hands it back. A fetch's
    /// fails with [`STREAMED_BODY`] instead ([`RedirectBody::StreamedFetch`]).
    ReturnResponse,
    /// A network error with this message as the cause.
    Fail(&'static str),
    /// The Location does not parse against the current URL. Node's cause is
    /// the error `new URL(location, currentURL)` throws -- a `TypeError` with
    /// `code` `ERR_INVALID_URL`, `input` and `base` (measured on v22.22.2) --
    /// so the text handed to the parser travels with the failure: the header
    /// value read as UTF-8, as undici reads it. The base is the current URL,
    /// fragment included, which the caller already holds.
    InvalidLocation { input: String },
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
/// `body` says whether the request's body can be sent again (a bool: `true`
/// for none or a buffered one).
///
/// The checks run in undici's order: Location parse, scheme, count,
/// credentials, a fetch's streamed body, then the method/body rewrite, and
/// last the bad-port check `mainFetch` runs on the new URL before it dials.
pub fn next(
    status: u16,
    method: &http::Method,
    current: &url::Url,
    location: Option<&HeaderValue>,
    redirects_so_far: u32,
    body: impl Into<RedirectBody>,
) -> Next {
    let body = body.into();
    if !is_redirect_status(status) {
        return Next::Done;
    }
    let Some(location) = location else {
        return Next::Done;
    };
    // undici's RedirectHandler: a body it has read makes no redirect at all.
    if body == RedirectBody::Streamed {
        return Next::ReturnResponse;
    }
    let mut target = match resolve_location(location.as_bytes(), current) {
        Ok(url) => url,
        Err(Some(input)) => return Next::InvalidLocation { input },
        Err(None) => return Next::Fail(INVALID_URL),
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
    // fetch/index.js:1273-1279, before the rewrite and before the next hop's
    // bad-port check: only a 303 lets a fetch's streamed body go -- a 301 or
    // 302 that would turn a POST into a body-less GET still refuses.
    if body == RedirectBody::StreamedFetch && status != 303 {
        return Next::Fail(STREAMED_BODY);
    }
    let rules = if matches!(
        body,
        RedirectBody::UndiciReplayable | RedirectBody::UndiciIterable
    ) {
        Rules::Undici
    } else {
        Rules::Fetch
    };
    let rewrite = rewrites_to_get(status, method, rules);
    if !rewrite && body == RedirectBody::StreamedFetch {
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

/// Whose rules a followed redirect goes by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rules {
    /// fetch's (`httpRedirectFetch`).
    Fetch,
    /// undici's RedirectHandler, which `undici.request` follows with.
    Undici,
}

/// The redirect `status` turns the request into a body-less GET.
/// fetch/index.js:1297-1305: only a 301 or 302 answering a POST, and a 303
/// answering anything but GET or HEAD (a 303 answering GET or HEAD keeps its
/// method and headers, content-type included -- reqwest dropped it).
/// undici's RedirectHandler (`undici.request`): only a 303, for any method
/// but HEAD (a GET stays a GET).
pub fn rewrites_to_get(status: u16, method: &http::Method, rules: Rules) -> bool {
    match rules {
        Rules::Undici => status == 303 && *method != http::Method::HEAD,
        Rules::Fetch => {
            (matches!(status, 301 | 302) && *method == http::Method::POST)
                || (status == 303 && *method != http::Method::GET && *method != http::Method::HEAD)
        }
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
        // `transfer-encoding` too: undici.request's streamed body carries
        // one oam sets for it, which frames the body that is going.
        for name in [
            CONTENT_ENCODING,
            CONTENT_LANGUAGE,
            CONTENT_LOCATION,
            CONTENT_TYPE,
            CONTENT_LENGTH,
            http::header::TRANSFER_ENCODING,
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
///
/// The error is the text that did not parse, or `None` for a value that was
/// never parsed.
fn resolve_location(raw: &[u8], current: &url::Url) -> Result<url::Url, Option<String>> {
    let bad_edge = |b: Option<&u8>| matches!(b, Some(b'\t' | b' '));
    if bad_edge(raw.first())
        || bad_edge(raw.last())
        || raw.iter().any(|b| matches!(b, 0 | b'\r' | b'\n'))
    {
        return Err(None);
    }
    let text = String::from_utf8_lossy(raw);
    current.join(&text).map_err(|_| Some(text.into_owned()))
}
