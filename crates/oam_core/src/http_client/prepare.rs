//! Request preparation for the `fetch` op: the URL, the method and the header
//! list a request goes out with, before anything dials.
//!
//! The rules are today's (reqwest 0.13.4 + tower-http 0.6.11 as oam 0.16.1
//! configured them), kept so the switch to oam's own transport changes no
//! request on the wire and no error text `http.request` maps:
//!
//! - the method is checked first and fails as `fetch: invalid method '{m}'`;
//! - an unparseable URL, a scheme other than http/https, and a user header
//!   whose name or value is not a legal header fail as `builder error`;
//! - user headers go out in the order given, a repeated name as separate
//!   lines;
//! - more distinct header names than `http::HeaderMap` holds fail as
//!   `fetch: too many request headers` (reqwest panicked; node has no cap);
//! - URL userinfo becomes `authorization: Basic base64(user:pass)`
//!   (percent-decoded, password optional -- reqwest request.rs:582-606 and
//!   util.rs:4-25) and never reaches the wire as userinfo;
//! - the defaults are added only when absent and after the user's headers,
//!   in reqwest's wire order: `accept: */*`, `user-agent`, then tower-http's
//!   `accept-encoding: gzip,deflate` (no space: compression_utils.rs
//!   `to_header_value`; `br` is not advertised).
//!
//! `host` is left to hyper-util's `set_host`, which adds it only if absent.

use base64::Engine as _;
use http::header::{ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use percent_encoding::percent_decode_str;

/// reqwest's Display for a `Kind::Builder` error with no URL attached. The
/// scheme case used to read `builder error for url (ftp://...)`; nothing
/// parses the suffix (node_compat.js keys on "error sending request" only).
pub const BUILDER_ERROR: &str = "builder error";

/// `accept-encoding` as oam has always advertised it. Node's fetch sends
/// `gzip, deflate` over http and `br, gzip, deflate` over https (undici
/// fetch/index.js:1517-1522); matching that is a separate decision from owning
/// the transport, so the value on the wire does not move here.
pub const DEFAULT_ACCEPT_ENCODING: &str = "gzip,deflate";

/// A request ready for the transport.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// The target, without userinfo (moved into `authorization`) and without
    /// a fragment (never sent).
    pub url: url::Url,
    pub method: http::Method,
    pub headers: HeaderMap,
}

/// Why a request could not be prepared. `Display` is the text the op fails
/// with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareError {
    /// Bad URL, non-http(s) scheme, or an illegal header name or value.
    Builder,
    /// A method that is not an HTTP token, carrying the method as given.
    InvalidMethod(String),
    /// More distinct header names than `http::HeaderMap` can hold (24576 at
    /// most, fewer once its hash-flooding defence rebuilds the table). Node
    /// has no such cap -- 25000 distinct headers get a 200 -- and reqwest
    /// panicked ("size overflows MAX_SIZE"), so this text is oam's own.
    TooManyHeaders,
}

impl std::fmt::Display for PrepareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrepareError::Builder => f.write_str(BUILDER_ERROR),
            PrepareError::InvalidMethod(m) => write!(f, "fetch: invalid method '{m}'"),
            PrepareError::TooManyHeaders => f.write_str("fetch: too many request headers"),
        }
    }
}

impl std::error::Error for PrepareError {}

/// Parse a request URL the way reqwest's `IntoUrl` + `execute_request` did:
/// WHATWG parsing (the `url` crate), then http or https with a host, then
/// convertible to the `http::Uri` the transport sends (see [`to_uri`]).
pub fn parse_url(raw: &str) -> Result<url::Url, &'static str> {
    let url = url::Url::parse(raw).map_err(|_| BUILDER_ERROR)?;
    match url.scheme() {
        "http" | "https" if url.has_host() => {
            to_uri(&url)?;
            Ok(url)
        }
        _ => Err(BUILDER_ERROR),
    }
}

/// Validate and assemble a request. `method` is the method as JS sent it
/// (bootstrap.js upper-cases it); `user_headers` are in the caller's order;
/// `default_headers` false sends the user's headers alone (the knob #148's raw
/// `http.request` needs); `user_agent` is the process-wide `oam/<ver>` value.
pub fn prepare(
    raw_url: &str,
    method: &str,
    user_headers: &[(String, String)],
    default_headers: bool,
    user_agent: &HeaderValue,
) -> Result<Prepared, PrepareError> {
    // Method before URL: reqwest's op parsed the method before it built the
    // request, so a bad method on a bad URL has always named the method.
    let method = http::Method::from_bytes(method.as_bytes())
        .map_err(|_| PrepareError::InvalidMethod(method.to_string()))?;
    let mut url = parse_url(raw_url).map_err(|_| PrepareError::Builder)?;
    let basic = take_userinfo(&mut url);
    url.set_fragment(None);

    // Every map operation here is the fallible `try_*` form: the infallible
    // ones panic past `HeaderMap`'s size cap, and JS picks the header count.
    // The capacity is only a hint -- one name repeated 30000 times is a single
    // entry -- so a hint too large for the map falls back to growing on demand.
    let mut headers =
        HeaderMap::try_with_capacity(user_headers.len().saturating_add(4)).unwrap_or_default();
    let too_many = |_: http::header::MaxSizeReached| PrepareError::TooManyHeaders;
    // reqwest appended the userinfo credential when the request was built,
    // BEFORE the user's headers, so it led the wire order. It appended it
    // even next to a user `authorization`, sending two; a request carries one
    // credential, so an explicit header wins and the userinfo one is dropped.
    if let Some(basic) = basic
        && !user_headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(AUTHORIZATION.as_str()))
    {
        headers.try_append(AUTHORIZATION, basic).map_err(too_many)?;
    }
    for (name, value) in user_headers {
        // `HeaderName::from_bytes` lower-cases; `HeaderValue::from_bytes`
        // admits HTAB, visible ASCII and obs-text (0x80-0xFF) -- the same
        // `TryFrom<&String>` conversions reqwest's `RequestBuilder::header`
        // ran.
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| PrepareError::Builder)?;
        let value = HeaderValue::from_bytes(value.as_bytes()).map_err(|_| PrepareError::Builder)?;
        headers.try_append(name, value).map_err(too_many)?;
    }
    if default_headers {
        if !headers.contains_key(ACCEPT) {
            headers
                .try_insert(ACCEPT, HeaderValue::from_static("*/*"))
                .map_err(too_many)?;
        }
        if !headers.contains_key(http::header::USER_AGENT) {
            headers
                .try_insert(http::header::USER_AGENT, user_agent.clone())
                .map_err(too_many)?;
        }
        if !headers.contains_key(ACCEPT_ENCODING) {
            headers
                .try_insert(
                    ACCEPT_ENCODING,
                    HeaderValue::from_static(DEFAULT_ACCEPT_ENCODING),
                )
                .map_err(too_many)?;
        }
    }
    Ok(Prepared {
        url,
        method,
        headers,
    })
}

/// Remove the userinfo from `url` and return it as a Basic credential.
///
/// reqwest's `extract_authority`: both parts are percent-decoded; a username
/// that is not UTF-8 once decoded yields no credential, a password that is
/// not yields `user:` with no password; an empty username with no password
/// is no credential at all. Unlike reqwest (which left an undecodable
/// userinfo in the URL for hyper to put in its pool key), the userinfo is
/// always removed: it is never sent, and two credentials must not share a
/// pooled connection key by accident of spelling.
fn take_userinfo(url: &mut url::Url) -> Option<HeaderValue> {
    if url.username().is_empty() && url.password().is_none() {
        return None;
    }
    let username = percent_decode_str(url.username())
        .decode_utf8()
        .ok()
        .map(|u| u.into_owned());
    let password = url.password().and_then(|p| {
        percent_decode_str(p)
            .decode_utf8()
            .ok()
            .map(|p| p.into_owned())
    });
    // Cannot fail: http(s) URLs always have a host, so they can carry
    // credentials (`cannot_be_a_base` / `file:` are the only refusals).
    let _ = url.set_username("");
    let _ = url.set_password(None);
    let username = username?;
    if username.is_empty() && password.is_none() {
        return None;
    }
    let mut raw = format!("{username}:");
    if let Some(password) = password {
        raw.push_str(&password);
    }
    let mut value = HeaderValue::try_from(format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(raw)
    ))
    .ok()?;
    value.set_sensitive(true);
    Some(value)
}

/// The request-target URI for hyper: scheme, host, port, path and query --
/// no userinfo (hyper-util keys its pool on the authority, and it would
/// otherwise go out in an absolute-form proxy request) and no fragment.
///
/// Fails with [`BUILDER_ERROR`] for the one shape WHATWG parsing accepts and
/// `http::Uri` does not: a domain holding `"`, `` ` ``, `{` or `}` (legal
/// domain code points in the URL Standard, outside RFC 3986's reg-name).
/// reqwest failed the same URLs the same way (`url_invalid_uri`, a builder
/// error); node resolves the name and fails `getaddrinfo ENOTFOUND`. Every
/// path, query and other host byte converts -- `tests/http_client_prepare.rs`
/// checks all 256 bytes in each position.
pub fn to_uri(url: &url::Url) -> Result<http::Uri, &'static str> {
    let head = &url[..url::Position::BeforeUsername];
    let tail = &url[url::Position::BeforeHost..url::Position::AfterQuery];
    let mut s = String::with_capacity(head.len() + tail.len());
    s.push_str(head);
    s.push_str(tail);
    http::Uri::try_from(s).map_err(|_| BUILDER_ERROR)
}

/// The host to resolve and dial for `uri`, with IPv6 brackets removed
/// (`http::Uri::host` keeps them: `"[::1]"`).
pub fn host_for_connect(uri: &http::Uri) -> Option<String> {
    let host = uri.host()?;
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    Some(host.to_string())
}

/// WHATWG "same origin" for two http(s) URLs: scheme, host and port equal.
/// Hosts compare after the parser's normalization (lower-cased and punycoded
/// domains, canonical IPv4 and IPv6), and an elided default port equals the
/// explicit one -- undici fetch/util.js:761-775 compares `protocol`,
/// `hostname` and `port` of URL objects that were normalized the same way.
pub fn origin_eq(a: &url::Url, b: &url::Url) -> bool {
    a.scheme() == b.scheme()
        && a.host() == b.host()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// The Fetch Standard's "bad port" list as undici 6.24.1 ships it
/// (fetch/constants.js:14-21, `badPortsSet`), sorted. Port 0 is not on it.
const BAD_PORTS: [u16; 82] = [
    1, 7, 9, 11, 13, 15, 17, 19, 20, 21, 22, 23, 25, 37, 42, 43, 53, 69, 77, 79, 87, 95, 101, 102,
    103, 104, 109, 110, 111, 113, 115, 117, 119, 123, 135, 137, 139, 143, 161, 179, 389, 427, 465,
    512, 513, 514, 515, 526, 530, 531, 532, 540, 548, 554, 556, 563, 587, 601, 636, 989, 990, 993,
    995, 1719, 1720, 1723, 2049, 3659, 4045, 4190, 5060, 5061, 6000, 6566, 6665, 6666, 6667, 6668,
    6669, 6679, 6697, 10080,
];

/// undici's `requestBadPort` (fetch/util.js:104-116): an http(s) URL whose
/// port is on the bad-port list. The port is the URL's explicit one -- an
/// elided default (`url.port === ""` in JS, `None` here) is never bad, so
/// `http://host:443/` is allowed and `http://host:25/` is not.
///
/// Node's fetch runs this in `mainFetch` for the first request and for every
/// redirect hop, failing with a network error whose cause is `bad port`
/// before anything dials (measured on node v22.22.2: a 302 to
/// `http://127.0.0.1:25/` sends no second request). `http.request` has no
/// such check, so it is not part of [`prepare`].
pub fn is_bad_port(url: &url::Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url
            .port()
            .is_some_and(|port| BAD_PORTS.binary_search(&port).is_ok())
}
