//! node's rules for an inbound HTTP/1 request head, applied on top of
//! hyper's parser, and node's `maxHeaderSize` count.
//!
//! hyper accepts some heads node's llhttp refuses, and they are the ones
//! that decide where a request body ends -- the thing a proxy in front of
//! the server and the server itself must agree on. node v22.22.2 answers
//! `400 Bad Request` (measured) for:
//!
//! - `Content-Length` together with `Transfer-Encoding`, in either order
//!   (a `Transfer-Encoding` header with an empty value does not count);
//! - a second `Content-Length` line, even with the same value;
//! - any `Transfer-Encoding` coding after `chunked`, in the same header or
//!   a later one (`chunked, chunked`; `chunked` then `gzip`);
//! - a line that ends in a bare LF instead of CRLF, anywhere in the head.
//!
//! and `431 Request Header Fields Too Large` once the head's counted bytes
//! reach the limit (see [`check_request_head`]).
//!
//! `insecureHTTPParser` (the server option, or `--insecure-http-parser`)
//! lifts every rule above except the duplicate `Content-Length` and the size
//! limit, which node keeps in that mode too.
//!
//! The server (`http_server`) runs [`check_request_head`] on the exact bytes
//! hyper parsed (hyper hands them over as `hyper::ext::RawRequestHead`,
//! oam's vendored patch) before a request reaches JS -- upgrade and CONNECT
//! requests included, whose heads [`parse_request_head`] then reads again
//! for the names as received.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// node's default `maxHeaderSize` (`--max-http-header-size`), 16 KiB.
pub const DEFAULT_MAX_HEADER_SIZE: u64 = 16 * 1024;

static MAX_HEADER_SIZE: AtomicU64 = AtomicU64::new(DEFAULT_MAX_HEADER_SIZE);
static INSECURE_PARSER: AtomicBool = AtomicBool::new(false);

/// Set the process-wide limit (`--max-http-header-size`, argv or
/// NODE_OPTIONS). It is what `http.maxHeaderSize` reports, what a server
/// without its own `maxHeaderSize` enforces, and what the fetch transport
/// applies to response heads.
pub fn set_max_http_header_size(limit: u64) {
    MAX_HEADER_SIZE.store(limit, Ordering::Relaxed);
}

/// The process-wide limit, [`DEFAULT_MAX_HEADER_SIZE`] unless a flag set it.
pub fn max_http_header_size() -> u64 {
    MAX_HEADER_SIZE.load(Ordering::Relaxed)
}

/// Set `--insecure-http-parser`.
pub fn set_insecure_http_parser(on: bool) {
    INSECURE_PARSER.store(on, Ordering::Relaxed);
}

/// Whether `--insecure-http-parser` was given.
pub fn insecure_http_parser() -> bool {
    INSECURE_PARSER.load(Ordering::Relaxed)
}

/// Parse a `--max-http-header-size` value the way node does (C `strtoull`,
/// measured on v22.22.2): leading digits only, so `12abc` is 12 and `abc` is
/// 0; a leading `-` negates modulo 2^64; overflow saturates.
pub fn parse_max_header_size_flag(raw: &str) -> u64 {
    let s = raw.trim_start();
    let (negative, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let mut value: u64 = 0;
    for b in digits.bytes() {
        if !b.is_ascii_digit() {
            break;
        }
        value = match value
            .checked_mul(10)
            .and_then(|v| v.checked_add(u64::from(b - b'0')))
        {
            Some(v) => v,
            None => return u64::MAX,
        };
    }
    if negative {
        value.wrapping_neg()
    } else {
        value
    }
}

/// How a server applies the rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadPolicy {
    /// node's `maxHeaderSize` for this server: counted bytes at or above it
    /// are refused.
    pub max_header_size: u64,
    /// `insecureHTTPParser`: the framing rules node relaxes are relaxed.
    pub lenient: bool,
}

impl HeadPolicy {
    /// The policy of a server that set neither option.
    pub fn process_default() -> Self {
        HeadPolicy {
            max_header_size: max_http_header_size(),
            lenient: insecure_http_parser(),
        }
    }

    /// hyper's read-buffer ceiling for this policy. A head that is still
    /// incomplete when the buffer reaches it is answered 431 by hyper itself,
    /// so an endless head is refused without being buffered whole (hyper's
    /// own default is ~400 KiB, which one read could overshoot to ~800 KiB).
    /// Four times the limit leaves room for every byte node does not count
    /// (CRLFs, `: `, the method and version), and hyper refuses anything
    /// under 8 KiB.
    pub fn read_buffer_limit(&self) -> usize {
        const FLOOR: u64 = 64 * 1024;
        let wanted = self.max_header_size.saturating_mul(4).max(FLOOR);
        usize::try_from(wanted).unwrap_or(usize::MAX)
    }
}

/// Why a head was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadError {
    /// `400 Bad Request`; the text names the rule, for tests and logs.
    BadRequest(&'static str),
    /// `431 Request Header Fields Too Large`.
    HeaderOverflow,
}

impl HeadError {
    pub fn status(&self) -> u16 {
        match self {
            HeadError::BadRequest(_) => 400,
            HeadError::HeaderOverflow => 431,
        }
    }
}

/// Check a request head that an HTTP/1 parser (hyper's httparse) accepted:
/// the request line through the empty line that ends it, as received.
///
/// The size count is node's (src/node_http_parser.cc `TrackHeader`, checked
/// against measurements): the request-target, plus every header name, plus
/// every header value without its leading whitespace but with its trailing
/// whitespace. Line endings, `:`, the method and the version are not
/// counted. The head is refused as soon as the running count is at or above
/// the limit, so with the 16 KiB default the largest accepted count is
/// 16383. The rules apply in the order the bytes arrive, so a head that
/// breaks two of them gets the status of the first.
pub fn check_request_head(head: &[u8], policy: HeadPolicy) -> Result<(), HeadError> {
    let limit = policy.max_header_size;
    let strict = !policy.lenient;
    let mut count: u64 = 0;
    let mut add = |n: usize| -> Result<(), HeadError> {
        count = count.saturating_add(n as u64);
        if count >= limit {
            Err(HeadError::HeaderOverflow)
        } else {
            Ok(())
        }
    };
    let bare_lf = |line: &[u8]| line.ends_with(b"\n") && !line.ends_with(b"\r\n");

    let mut lines = head.split_inclusive(|&b| b == b'\n').peekable();
    // Empty lines before the request line are skipped (RFC 9112 s2.2, and
    // both parsers do).
    while lines.peek().is_some_and(|l| *l == b"\r\n" || *l == b"\n") {
        lines.next();
    }
    let Some(request_line) = lines.next() else {
        return Ok(());
    };
    let text = strip_eol(request_line);
    let target = match (
        text.iter().position(|&b| b == b' '),
        text.iter().rposition(|&b| b == b' '),
    ) {
        (Some(first), Some(last)) if last > first => &text[first + 1..last],
        _ => &[][..],
    };
    add(target.len())?;
    if strict && bare_lf(request_line) {
        return Err(HeadError::BadRequest("bare LF line ending"));
    }

    let mut content_lengths = 0u32;
    let mut transfer_encoding = false;
    let mut chunked = false;
    for line in lines {
        if line == b"\r\n" || line == b"\n" {
            if strict && bare_lf(line) {
                return Err(HeadError::BadRequest("bare LF line ending"));
            }
            break;
        }
        let text = strip_eol(line);
        let colon = text.iter().position(|&b| b == b':').unwrap_or(0);
        let name = &text[..colon];
        let value = trim_leading_ows(text.get(colon + 1..).unwrap_or(&[]));
        add(name.len())?;
        add(value.len())?;
        if strict && bare_lf(line) {
            return Err(HeadError::BadRequest("bare LF line ending"));
        }
        if name.eq_ignore_ascii_case(b"content-length") {
            content_lengths += 1;
            if content_lengths > 1 {
                // Refused in node's insecure mode as well.
                return Err(HeadError::BadRequest("duplicate Content-Length"));
            }
            if strict && transfer_encoding {
                return Err(HeadError::BadRequest(
                    "Content-Length with Transfer-Encoding",
                ));
            }
            if !is_content_length(value) {
                return Err(HeadError::BadRequest("invalid Content-Length"));
            }
        } else if name.eq_ignore_ascii_case(b"transfer-encoding") {
            if trim_ows(value).is_empty() {
                // node does not count an empty Transfer-Encoding.
                continue;
            }
            transfer_encoding = true;
            if strict && content_lengths > 0 {
                return Err(HeadError::BadRequest(
                    "Transfer-Encoding with Content-Length",
                ));
            }
            for coding in value.split(|&b| b == b',').map(trim_ows) {
                if coding.is_empty() {
                    continue;
                }
                if strict && chunked {
                    return Err(HeadError::BadRequest("Transfer-Encoding after chunked"));
                }
                chunked = coding.eq_ignore_ascii_case(b"chunked");
            }
        }
    }
    // A request body's length must be knowable: chunked has to be the final
    // coding (RFC 9112 s6.3; node refuses `identity`, `gzip` alone, ...).
    if transfer_encoding && !chunked {
        return Err(HeadError::BadRequest(
            "Transfer-Encoding not ending in chunked",
        ));
    }
    Ok(())
}

fn strip_eol(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn trim_leading_ows(v: &[u8]) -> &[u8] {
    let start = v
        .iter()
        .position(|&b| b != b' ' && b != b'\t')
        .unwrap_or(v.len());
    &v[start..]
}

fn trim_ows(v: &[u8]) -> &[u8] {
    let v = trim_leading_ows(v);
    let end = v
        .iter()
        .rposition(|&b| b != b' ' && b != b'\t')
        .map_or(0, |i| i + 1);
    &v[..end]
}

/// `1*DIGIT` with optional surrounding whitespace, fitting a u64 -- what
/// node and hyper accept (`+3`, `-1`, `0x3`, `3, 3` and an empty value are
/// all refused by both).
fn is_content_length(v: &[u8]) -> bool {
    let v = trim_ows(v);
    !v.is_empty()
        && v.iter().all(u8::is_ascii_digit)
        && std::str::from_utf8(v).is_ok_and(|s| s.parse::<u64>().is_ok())
}

/// Parse and check a request head: httparse's grammar -- which refuses
/// obs-fold, control characters, bad tokens and more than `max_headers`
/// fields, as hyper does -- then [`check_request_head`]. `Ok` carries the
/// method, target and headers as received (names in their own case), which
/// an upgrade or CONNECT request hands to JS from the head hyper parsed.
pub fn parse_request_head(head: &[u8], policy: HeadPolicy) -> Result<ParsedHead, HeadError> {
    const MAX_HEADERS: usize = 100; // hyper's default
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(head) {
        Ok(httparse::Status::Complete(len)) if len == head.len() => {}
        // The parser found the head's end before the CRLFCRLF the caller cut
        // at (a bare-LF blank line): what follows would be a second message
        // read as part of this one.
        Ok(httparse::Status::Complete(_)) => {
            return Err(HeadError::BadRequest("bytes after the head"));
        }
        Ok(httparse::Status::Partial) => {
            return Err(HeadError::BadRequest("incomplete head"));
        }
        Err(httparse::Error::TooManyHeaders) => return Err(HeadError::HeaderOverflow),
        Err(_) => return Err(HeadError::BadRequest("malformed head")),
    }
    check_request_head(head, policy)?;
    Ok(ParsedHead {
        method: req.method.unwrap_or_default().to_string(),
        target: req.path.unwrap_or_default().to_string(),
        headers: req
            .headers
            .iter()
            .map(|h| {
                (
                    h.name.to_string(),
                    String::from_utf8_lossy(h.value).into_owned(),
                )
            })
            .collect(),
    })
}

/// Whether a request head asks for an upgrade, by node's rule: an `Upgrade`
/// header, and `upgrade` among the comma-separated tokens of a `Connection`
/// header (llhttp's F_UPGRADE and F_CONNECTION_UPGRADE). Only the head
/// decides -- never a body, or a request pipelined behind it -- and a
/// CONNECT request is always one (the caller checks the method).
pub fn is_upgrade<'a>(headers: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> bool {
    let mut upgrade = false;
    let mut connection_upgrade = false;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("upgrade") {
            upgrade = true;
        } else if name.eq_ignore_ascii_case("connection") {
            connection_upgrade |= value
                .split(|&b| b == b',')
                .any(|token| trim_ows(token).eq_ignore_ascii_case(b"upgrade"));
        }
    }
    upgrade && connection_upgrade
}

/// A request head [`parse_request_head`] accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedHead {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const STRICT: HeadPolicy = HeadPolicy {
        max_header_size: DEFAULT_MAX_HEADER_SIZE,
        lenient: false,
    };
    const LENIENT: HeadPolicy = HeadPolicy {
        max_header_size: DEFAULT_MAX_HEADER_SIZE,
        lenient: true,
    };

    fn check(head: &str, policy: HeadPolicy) -> Result<(), HeadError> {
        check_request_head(head.as_bytes(), policy)
    }

    fn bad(head: &str, policy: HeadPolicy) -> bool {
        matches!(check(head, policy), Err(HeadError::BadRequest(_)))
    }

    /// Every verdict below is node v22.22.2's, measured on the raw wire
    /// (default parser, then `--insecure-http-parser`).
    #[test]
    fn framing_rules_match_node() {
        let h = "Host: x\r\n";
        let cases: &[(&str, bool, bool)] = &[
            // (head, strict refuses, lenient refuses)
            (
                &format!("POST / HTTP/1.1\r\n{h}Content-Length: 3\r\n\r\n"),
                false,
                false,
            ),
            (
                &format!("POST / HTTP/1.1\r\n{h}Transfer-Encoding: chunked\r\n\r\n"),
                false,
                false,
            ),
            (
                &format!(
                    "POST / HTTP/1.1\r\n{h}Content-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n"
                ),
                true,
                false,
            ),
            (
                &format!(
                    "POST / HTTP/1.1\r\n{h}Transfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n"
                ),
                true,
                false,
            ),
            (
                &format!(
                    "POST / HTTP/1.1\r\n{h}Content-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n"
                ),
                true,
                false,
            ),
            (
                &format!(
                    "GET / HTTP/1.1\r\n{h}Content-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n"
                ),
                true,
                false,
            ),
            (
                &format!("POST / HTTP/1.1\r\n{h}Transfer-Encoding: \r\nContent-Length: 3\r\n\r\n"),
                false,
                false,
            ),
            (
                &format!("POST / HTTP/1.1\r\n{h}Content-Length: 3\r\nContent-Length: 3\r\n\r\n"),
                true,
                true,
            ),
            (
                &format!("POST / HTTP/1.1\r\n{h}Content-Length: 3\r\ncontent-length: 3\r\n\r\n"),
                true,
                true,
            ),
            (
                &format!(
                    "POST / HTTP/1.1\r\n{h}Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n"
                ),
                true,
                false,
            ),
            (
                &format!("POST / HTTP/1.1\r\n{h}Transfer-Encoding: chunked, chunked\r\n\r\n"),
                true,
                false,
            ),
            (
                &format!(
                    "POST / HTTP/1.1\r\n{h}Transfer-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n\r\n"
                ),
                false,
                false,
            ),
            (
                &format!("POST / HTTP/1.1\r\n{h}Transfer-Encoding: gzip, chunked\r\n\r\n"),
                false,
                false,
            ),
            (
                &format!("POST / HTTP/1.1\r\n{h}Transfer-Encoding: , chunked\r\n\r\n"),
                false,
                false,
            ),
            (
                &format!("POST / HTTP/1.1\r\n{h}Transfer-Encoding: Chunked\r\n\r\n"),
                false,
                false,
            ),
            (
                &format!("POST / HTTP/1.1\r\n{h}Transfer-Encoding: chunked \r\n\r\n"),
                false,
                false,
            ),
            (
                &format!("POST / HTTP/1.1\r\n{h}Transfer-Encoding: identity\r\n\r\n"),
                true,
                true,
            ),
            (
                &format!("POST / HTTP/1.1\r\n{h}Transfer-Encoding: chunked, gzip\r\n\r\n"),
                true,
                true,
            ),
            ("GET / HTTP/1.1\nHost: x\n\n", true, false),
            ("GET / HTTP/1.1\r\nHost: x\nX-A: 1\r\n\r\n", true, false),
            ("GET / HTTP/1.1\r\nHost: x\r\n\n", true, false),
            ("GET / HTTP/1.1\nHost: x\r\n\r\n", true, false),
            ("GET / HTTP/1.1\r\nHost: x\n\r\n", true, false),
            ("\r\nGET / HTTP/1.1\r\nHost: x\r\n\r\n", false, false),
            (
                &format!("POST / HTTP/1.1\r\n{h}Content-Length: +3\r\n\r\n"),
                true,
                true,
            ),
            (
                &format!("POST / HTTP/1.1\r\n{h}Content-Length: \r\n\r\n"),
                true,
                true,
            ),
            (
                &format!("POST / HTTP/1.1\r\n{h}Content-Length:   3  \r\n\r\n"),
                false,
                false,
            ),
        ];
        for (head, strict, lenient) in cases {
            assert_eq!(bad(head, STRICT), *strict, "strict: {head:?}");
            assert_eq!(bad(head, LENIENT), *lenient, "lenient: {head:?}");
        }
    }

    /// node's boundary, measured with the 16 KiB default: a one-header
    /// request is accepted up to a counted 16383 and refused at 16384, for a
    /// long value and for a long target alike, and trailing whitespace in a
    /// value counts while leading whitespace does not.
    #[test]
    fn size_count_matches_node() {
        // url "/" 1 + Host 4 + x 1 + Connection 10 + close 5 + X-A 3 = 24.
        let one = |n: usize| {
            format!(
                "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\nX-A: {}\r\n\r\n",
                "a".repeat(n)
            )
        };
        assert_eq!(check(&one(16359), STRICT), Ok(()));
        assert_eq!(check(&one(16360), STRICT), Err(HeadError::HeaderOverflow));
        // url 1+n + Host 4 + x 1 + Connection 10 + close 5 = 21 + n.
        let url = |n: usize| {
            format!(
                "GET /{} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
                "u".repeat(n)
            )
        };
        assert_eq!(check(&url(16362), STRICT), Ok(()));
        assert_eq!(check(&url(16363), STRICT), Err(HeadError::HeaderOverflow));
        let trailing = |n: usize| {
            format!(
                "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\nX-A: v{}\r\n\r\n",
                " ".repeat(n)
            )
        };
        assert_eq!(check(&trailing(16358), STRICT), Ok(()));
        assert_eq!(
            check(&trailing(16359), STRICT),
            Err(HeadError::HeaderOverflow)
        );
        let leading = format!(
            "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\nX-A:{}v\r\n\r\n",
            " ".repeat(100_000)
        );
        assert_eq!(check(&leading, STRICT), Ok(()));
        // insecureHTTPParser does not lift it.
        assert_eq!(check(&one(16360), LENIENT), Err(HeadError::HeaderOverflow));
        // A server limit of 1000 moves the boundary (node: 975 / 976).
        let small = HeadPolicy {
            max_header_size: 1000,
            lenient: false,
        };
        assert_eq!(check(&one(975), small), Ok(()));
        assert_eq!(check(&one(976), small), Err(HeadError::HeaderOverflow));
        // A limit of 0 refuses everything (node's `--max-http-header-size=0`).
        let zero = HeadPolicy {
            max_header_size: 0,
            lenient: false,
        };
        assert_eq!(check(&one(0), zero), Err(HeadError::HeaderOverflow));
    }

    #[test]
    fn the_first_broken_rule_decides() {
        // The duplicate comes before the size limit is reached.
        let head = format!(
            "POST / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 1\r\nX-A: {}\r\n\r\n",
            "a".repeat(20_000)
        );
        assert!(matches!(
            check(&head, STRICT),
            Err(HeadError::BadRequest(_))
        ));
        let head = format!(
            "POST / HTTP/1.1\r\nX-A: {}\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\n",
            "a".repeat(20_000)
        );
        assert_eq!(check(&head, STRICT), Err(HeadError::HeaderOverflow));
    }

    #[test]
    fn upgrade_heads_are_parsed_and_checked() {
        let ok = parse_request_head(
            b"GET /ws HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: x\r\n\r\n",
            STRICT,
        )
        .unwrap();
        assert_eq!(ok.method, "GET");
        assert_eq!(ok.target, "/ws");
        assert_eq!(ok.headers.len(), 3);
        for head in [
            // obs-fold, NUL, a space in a name, CL + TE, a duplicate CL
            &b"GET / HTTP/1.1\r\nHost: x\r\nUpgrade: x\r\nX-A: a\r\n b\r\n\r\n"[..],
            b"GET / HTTP/1.1\r\nHost: x\r\nUpgrade: x\r\nX-A: a\0b\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost: x\r\nUpgrade: x\r\nX A: b\r\n\r\n",
            b"POST / HTTP/1.1\r\nHost: x\r\nUpgrade: x\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost: x\r\nUpgrade: x\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost: x\r\nUpgrade: x\nX-A: 1\r\n\r\n",
        ] {
            assert!(
                matches!(parse_request_head(head, STRICT), Err(HeadError::BadRequest(_))),
                "{:?}",
                String::from_utf8_lossy(head)
            );
        }
        // A bare-LF blank line ends the head early for the parser; the bytes
        // after it are not part of this request, even when lenient.
        assert!(matches!(
            parse_request_head(
                b"GET / HTTP/1.1\r\nHost: x\n\nConnection: Upgrade\r\nUpgrade: x\r\n\r\n",
                LENIENT
            ),
            Err(HeadError::BadRequest(_))
        ));
        let big = format!(
            "GET / HTTP/1.1\r\nHost: x\r\nUpgrade: x\r\nX-A: {}\r\n\r\n",
            "a".repeat(2000)
        );
        let small = HeadPolicy {
            max_header_size: 1000,
            lenient: false,
        };
        assert_eq!(
            parse_request_head(big.as_bytes(), small),
            Err(HeadError::HeaderOverflow)
        );
    }

    /// Only a head with both headers is an upgrade.
    #[test]
    fn upgrade_requests_are_told_apart_by_their_headers() {
        let heads = |head: &[u8]| {
            let parsed = parse_request_head(head, STRICT).unwrap();
            is_upgrade(
                parsed
                    .headers
                    .iter()
                    .map(|(n, v)| (n.as_str(), v.as_bytes()))
                    .collect::<Vec<_>>(),
            )
        };
        assert!(heads(
            b"GET /ws HTTP/1.1\r\nHost: x\r\nConnection: keep-alive, Upgrade\r\nUpgrade: websocket\r\n\r\n"
        ));
        assert!(heads(
            b"GET /ws HTTP/1.1\r\nHost: x\r\nconnection: UPGRADE\r\nupgrade: h2c\r\n\r\n"
        ));
        assert!(heads(
            b"GET /ws HTTP/1.1\r\nHost: x\r\nConnection: close\r\nConnection: upgrade\r\nUpgrade: x\r\n\r\n"
        ));
        for not_upgrade in [
            &b"GET / HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\n\r\n"[..],
            b"GET / HTTP/1.1\r\nHost: x\r\nUpgrade: x\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost: x\r\nConnection: upgraded\r\nUpgrade: x\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost: x\r\nX-Connection: upgrade\r\nUpgrade: x\r\n\r\n",
        ] {
            assert!(
                !heads(not_upgrade),
                "{:?}",
                String::from_utf8_lossy(not_upgrade)
            );
        }
    }

    #[test]
    fn flag_values_parse_like_node() {
        assert_eq!(parse_max_header_size_flag("2000"), 2000);
        assert_eq!(parse_max_header_size_flag("12abc"), 12);
        assert_eq!(parse_max_header_size_flag("abc"), 0);
        assert_eq!(parse_max_header_size_flag("0"), 0);
        assert_eq!(parse_max_header_size_flag("-5"), 5u64.wrapping_neg());
        assert_eq!(
            parse_max_header_size_flag("99999999999999999999999"),
            u64::MAX
        );
    }

    #[test]
    fn the_read_buffer_limit_bounds_an_endless_head() {
        assert_eq!(STRICT.read_buffer_limit(), 64 * 1024);
        let big = HeadPolicy {
            max_header_size: 1 << 20,
            lenient: false,
        };
        assert_eq!(big.read_buffer_limit(), 4 << 20);
        let zero = HeadPolicy {
            max_header_size: 0,
            lenient: false,
        };
        assert!(zero.read_buffer_limit() >= 8192, "hyper's floor");
    }
}
