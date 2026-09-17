//! `http_client::redirect` against undici 6.24.1's `httpRedirectFetch` and
//! the behaviour measured on node v22.22.2 (#143). Lives in `tests/` because
//! it is URL-heavy: IPv6 literals and arbitrary hosts are exactly what the
//! published-URLs gate cannot scan meaningfully in `src/`.

use http::Method;
use http::header::{HeaderMap, HeaderName, HeaderValue};
use oam_core::http_client::redirect::{
    self, BAD_SCHEME, COUNT_EXCEEDED, CREDENTIALS, INVALID_URL, MAX_REDIRECTS, Next,
};

fn url(s: &str) -> url::Url {
    url::Url::parse(s).unwrap()
}

fn hv(s: &str) -> HeaderValue {
    HeaderValue::from_str(s).unwrap()
}

fn follow(status: u16, method: &Method, current: &str, location: &str, replayable: bool) -> Next {
    redirect::next(
        status,
        method,
        &url(current),
        Some(&hv(location)),
        0,
        replayable,
    )
}

const ALL_METHODS: &[&str] = &[
    "GET", "HEAD", "POST", "PUT", "DELETE", "PATCH", "OPTIONS", "QUERY",
];

/// Every redirect status x method x replayability: the rewrite happens only
/// for (301|302 && POST) || (303 && method not GET/HEAD) (fetch/index.js:
/// 1297-1305); otherwise method and body are kept, and a kept body that
/// cannot be replayed returns the 3xx.
#[test]
fn status_method_body_matrix() {
    for status in [301u16, 302, 303, 307, 308] {
        for m in ALL_METHODS {
            let method = Method::from_bytes(m.as_bytes()).unwrap();
            for replayable in [true, false] {
                let got = follow(status, &method, "http://a.test/x", "/y", replayable);
                let rewrite = (matches!(status, 301 | 302) && method == Method::POST)
                    || (status == 303 && method != Method::GET && method != Method::HEAD);
                let want = if rewrite {
                    Next::Follow {
                        url: url("http://a.test/y"),
                        method: Method::GET,
                        drop_body: true,
                    }
                } else if !replayable {
                    Next::ReturnResponse
                } else {
                    Next::Follow {
                        url: url("http://a.test/y"),
                        method: method.clone(),
                        drop_body: false,
                    }
                };
                assert_eq!(
                    got, want,
                    "status {status} method {m} replayable {replayable}"
                );
            }
        }
    }
}

#[test]
fn spot_checks_of_the_matrix() {
    // 301/302 rewrite POST only; PUT keeps its method (unlike reqwest, which
    // rewrote every non-GET/HEAD method on 301/302).
    assert!(matches!(
        follow(301, &Method::PUT, "http://a.test/", "/b", true),
        Next::Follow { ref method, drop_body: false, .. } if *method == Method::PUT
    ));
    // 303 on GET/HEAD: no rewrite, no body-header strip.
    for m in [Method::GET, Method::HEAD] {
        assert_eq!(
            follow(303, &m, "http://a.test/", "/b", true),
            Next::Follow {
                url: url("http://a.test/b"),
                method: m.clone(),
                drop_body: false
            }
        );
    }
    // 307/308 keep POST and its body.
    assert_eq!(
        follow(307, &Method::POST, "http://a.test/", "/b", true),
        Next::Follow {
            url: url("http://a.test/b"),
            method: Method::POST,
            drop_body: false
        }
    );
    // A streamed POST on 301 is dropped by the rewrite, so it follows.
    assert!(matches!(
        follow(301, &Method::POST, "http://a.test/", "/b", false),
        Next::Follow {
            drop_body: true,
            ..
        }
    ));
    // A streamed PUT on 308 would have to be replayed: the 3xx comes back.
    assert_eq!(
        follow(308, &Method::PUT, "http://a.test/", "/b", false),
        Next::ReturnResponse
    );
}

#[test]
fn not_a_redirect_or_no_location_is_done() {
    let cur = url("http://a.test/");
    for status in [200u16, 204, 300, 304, 305, 306, 309, 399, 404] {
        assert_eq!(
            redirect::next(status, &Method::GET, &cur, Some(&hv("/b")), 0, true),
            Next::Done,
            "{status}"
        );
    }
    for status in [301u16, 302, 303, 307, 308] {
        assert_eq!(
            redirect::next(status, &Method::GET, &cur, None, 0, true),
            Next::Done
        );
    }
}

#[test]
fn limit_boundary_is_twenty_followed_redirects() {
    let cur = url("http://a.test/loop");
    let loc = hv("/loop");
    for so_far in 0..MAX_REDIRECTS {
        assert!(
            matches!(
                redirect::next(302, &Method::GET, &cur, Some(&loc), so_far, true),
                Next::Follow { .. }
            ),
            "redirect #{} must be followed",
            so_far + 1
        );
    }
    assert_eq!(MAX_REDIRECTS, 20);
    assert_eq!(
        redirect::next(302, &Method::GET, &cur, Some(&loc), 20, true),
        Next::Fail(COUNT_EXCEEDED)
    );
}

#[test]
fn location_errors_in_undici_order() {
    let cur = url("http://a.test/x");
    let get = Method::GET;
    // Unparseable (node: cause "Invalid URL").
    for bad in [
        "http://[::1",
        "http://a b/",
        "https://exa mple.org/",
        "http://:80/",
    ] {
        assert_eq!(
            redirect::next(302, &get, &cur, Some(&hv(bad)), 0, true),
            Next::Fail(INVALID_URL),
            "{bad}"
        );
    }
    // Non-http(s) scheme.
    for bad in [
        "ftp://a.test/x",
        "file:///etc/passwd",
        "data:text/plain,hi",
        "javascript:alert(1)",
    ] {
        assert_eq!(
            redirect::next(302, &get, &cur, Some(&hv(bad)), 0, true),
            Next::Fail(BAD_SCHEME),
            "{bad}"
        );
    }
    // Parse and scheme failures win over the count, the count over the
    // credentials check.
    assert_eq!(
        redirect::next(302, &get, &cur, Some(&hv("http://[::1")), 20, true),
        Next::Fail(INVALID_URL)
    );
    assert_eq!(
        redirect::next(302, &get, &cur, Some(&hv("ftp://a.test/")), 20, true),
        Next::Fail(BAD_SCHEME)
    );
    assert_eq!(
        redirect::next(302, &get, &cur, Some(&hv("http://u:p@a.test/")), 20, true),
        Next::Fail(COUNT_EXCEEDED)
    );
}

/// Measured on node v22.22.2: a Location with userinfo fails the fetch with
/// `cross origin not allowed for request mode "cors"`, same-origin or not.
/// It is never turned into an Authorization header.
#[test]
fn location_with_credentials_is_a_network_error() {
    let cur = url("http://127.0.0.1:8080/same");
    for loc in [
        "http://u:p@127.0.0.1:8080/t1",
        "http://u:p@localhost:8080/t2",
        "http://u@127.0.0.1:8080/t3",
        "http://:p@127.0.0.1:8080/t4",
    ] {
        assert_eq!(
            redirect::next(302, &Method::GET, &cur, Some(&hv(loc)), 0, true),
            Next::Fail(CREDENTIALS),
            "{loc}"
        );
    }
    // A bare "@" is no userinfo at all once parsed.
    assert!(matches!(
        redirect::next(
            302,
            &Method::GET,
            &cur,
            Some(&hv("http://@127.0.0.1:8080/t5")),
            0,
            true
        ),
        Next::Follow { .. }
    ));
}

#[test]
fn relative_and_odd_locations_resolve_against_the_current_url() {
    let cur = url("https://a.test:8443/dir/page?q=1#frag");
    let cases = [
        ("/abs", "https://a.test:8443/abs#frag"),
        ("rel", "https://a.test:8443/dir/rel#frag"),
        ("../up", "https://a.test:8443/up#frag"),
        (
            "?only=query",
            "https://a.test:8443/dir/page?only=query#frag",
        ),
        ("#other", "https://a.test:8443/dir/page?q=1#other"),
        ("//b.test/net-path", "https://b.test/net-path#frag"),
        ("http://c.test/abs", "http://c.test/abs#frag"),
        // An empty Location is the current URL (a loop, as in node).
        ("", "https://a.test:8443/dir/page?q=1#frag"),
        // An explicit empty fragment is `URL.hash === ""`: inherits.
        ("/e#", "https://a.test:8443/e#frag"),
        // Backslashes and surrounding C0/space are WHATWG-normalized.
        ("\\\\d.test\\p", "https://d.test/p#frag"),
    ];
    for (loc, want) in cases {
        match redirect::next(302, &Method::GET, &cur, Some(&hv(loc)), 0, true) {
            Next::Follow { url: got, .. } => assert_eq!(got.as_str(), want, "Location {loc:?}"),
            other => panic!("Location {loc:?}: {other:?}"),
        }
    }
    // No fragment on the current URL: nothing to inherit.
    let plain = url("http://a.test/p");
    match redirect::next(302, &Method::GET, &plain, Some(&hv("/q")), 0, true) {
        Next::Follow { url: got, .. } => assert_eq!(got.as_str(), "http://a.test/q"),
        other => panic!("{other:?}"),
    }
}

/// undici decodes a Location with bytes outside 0x20-0x7E as UTF-8; node
/// followed `/caf\u{e9}` sent as raw UTF-8 bytes to `/caf%C3%A9` (measured).
#[test]
fn non_ascii_location_bytes_are_utf8() {
    let cur = url("http://a.test/");
    let raw = HeaderValue::from_bytes("/caf\u{e9}".as_bytes()).unwrap();
    match redirect::next(302, &Method::GET, &cur, Some(&raw), 0, true) {
        Next::Follow { url: got, .. } => assert_eq!(got.as_str(), "http://a.test/caf%C3%A9"),
        other => panic!("{other:?}"),
    }
    // Invalid UTF-8 becomes U+FFFD, as Buffer#toString('utf8') does.
    let raw = HeaderValue::from_bytes(b"/x\xff").unwrap();
    match redirect::next(302, &Method::GET, &cur, Some(&raw), 0, true) {
        Next::Follow { url: got, .. } => assert_eq!(got.as_str(), "http://a.test/x%EF%BF%BD"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn illegal_header_value_shapes_do_not_follow() {
    let cur = url("http://a.test/");
    for raw in [&b" /lead"[..], b"/trail ", b"\t/tab", b"/tab\t"] {
        let value = HeaderValue::from_bytes(raw).unwrap();
        assert_eq!(
            redirect::next(302, &Method::GET, &cur, Some(&value), 0, true),
            Next::Fail(INVALID_URL),
            "{raw:?}"
        );
    }
}

/// Two Location lines are joined with ", " before parsing: node followed
/// `/t4` + `/t5` to `/t4,%20/t5` (measured).
#[test]
fn repeated_location_lines_are_joined() {
    let mut headers = HeaderMap::new();
    assert_eq!(redirect::location(&headers), None);
    headers.append("location", hv("/t4"));
    assert_eq!(redirect::location(&headers), Some(hv("/t4")));
    headers.append("location", hv("/t5"));
    let joined = redirect::location(&headers).unwrap();
    assert_eq!(joined, hv("/t4, /t5"));
    let cur = url("http://127.0.0.1:9/two");
    match redirect::next(302, &Method::GET, &cur, Some(&joined), 0, true) {
        Next::Follow { url: got, .. } => assert_eq!(got.as_str(), "http://127.0.0.1:9/t4,%20/t5"),
        other => panic!("{other:?}"),
    }
}

fn map(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        h.append(HeaderName::from_bytes(k.as_bytes()).unwrap(), hv(v));
    }
    h
}

fn names(h: &HeaderMap) -> Vec<String> {
    let mut v: Vec<String> = h
        .iter()
        .map(|(k, v)| format!("{}: {}", k, v.to_str().unwrap()))
        .collect();
    v.sort();
    v
}

const EVERYTHING: &[(&str, &str)] = &[
    ("authorization", "Bearer t"),
    ("proxy-authorization", "Basic cA=="),
    ("cookie", "a=1"),
    ("cookie", "b=2"),
    ("cookie2", "$Version=1"),
    ("www-authenticate", "Basic"),
    ("host", "custom.example"),
    ("content-type", "application/json"),
    ("content-length", "2"),
    ("content-encoding", "gzip"),
    ("content-language", "en"),
    ("content-location", "/c"),
    ("x-custom", "kept"),
    ("referer", "http://user-set.test/"),
];

#[test]
fn apply_same_origin_keeps_everything() {
    let mut h = map(EVERYTHING);
    redirect::apply(
        &mut h,
        &url("http://a.test/x"),
        &url("http://a.test:80/y#f"),
        false,
    );
    assert_eq!(names(&h), names(&map(EVERYTHING)));
    assert!(!h.contains_key("referer") || h["referer"] == "http://user-set.test/");
}

#[test]
fn apply_drop_body_strips_request_body_headers_only() {
    let mut h = map(EVERYTHING);
    redirect::apply(
        &mut h,
        &url("http://a.test/x"),
        &url("http://a.test/y"),
        true,
    );
    for gone in [
        "content-type",
        "content-length",
        "content-encoding",
        "content-language",
        "content-location",
    ] {
        assert!(!h.contains_key(gone), "{gone}");
    }
    for kept in [
        "authorization",
        "proxy-authorization",
        "cookie",
        "host",
        "x-custom",
    ] {
        assert!(h.contains_key(kept), "{kept}");
    }
    assert_eq!(h.get_all("cookie").iter().count(), 2);
}

#[test]
fn apply_cross_origin_strips_credentials_and_host_but_forwards_cookie2() {
    let mut h = map(EVERYTHING);
    redirect::apply(
        &mut h,
        &url("http://a.test/x"),
        &url("http://b.test/y"),
        false,
    );
    for gone in ["authorization", "proxy-authorization", "cookie", "host"] {
        assert!(!h.contains_key(gone), "{gone}");
    }
    // node forwards these cross-origin (reqwest stripped them).
    assert_eq!(h["cookie2"], "$Version=1");
    assert_eq!(h["www-authenticate"], "Basic");
    // Body headers stay on a 307-style hop; no Referer is invented.
    assert_eq!(h["content-type"], "application/json");
    assert_eq!(h.get_all("referer").iter().count(), 1);
}

/// A strip is permanent: the carried map does not get the credentials back
/// on a later hop to the original origin (reqwest re-sent them there).
#[test]
fn cross_origin_strip_survives_a_hop_back() {
    let a = url("http://a.test/1");
    let b = url("http://b.test/2");
    let a2 = url("http://a.test/3");
    let mut h = map(&[("authorization", "Bearer t"), ("cookie", "c=1"), ("x", "y")]);
    redirect::apply(&mut h, &a, &b, false);
    redirect::apply(&mut h, &b, &a2, false);
    assert!(!h.contains_key("authorization"));
    assert!(!h.contains_key("cookie"));
    assert_eq!(h["x"], "y");
}

#[test]
fn origin_is_scheme_host_and_port() {
    let cross = |from: &str, to: &str| {
        let mut h = map(&[("authorization", "t")]);
        redirect::apply(&mut h, &url(from), &url(to), false);
        !h.contains_key("authorization")
    };
    // Same origin after normalization.
    assert!(!cross("http://a.test/", "http://A.TEST:80/p"));
    assert!(!cross("https://a.test/", "https://a.test:443/"));
    assert!(!cross("http://127.0.0.1/", "http://127.1/"));
    assert!(!cross(
        "http://[::1]:8080/",
        "http://[0:0:0:0:0:0:0:1]:8080/x"
    ));
    assert!(!cross(
        "http://[::ffff:7f00:1]/",
        "http://[::ffff:127.0.0.1]/"
    ));
    assert!(!cross("http://xn--caf-dma.test/", "http://caf\u{e9}.test/"));
    // Cross origin.
    assert!(cross("http://a.test/", "https://a.test/"));
    assert!(cross("http://a.test/", "http://a.test:8080/"));
    assert!(cross("http://a.test/", "http://b.a.test/"));
    assert!(cross("http://[::1]:8080/", "http://127.0.0.1:8080/"));
    assert!(cross("http://[::1]:8080/", "http://[::1]:8081/"));
    assert!(cross("http://localhost:8080/", "http://127.0.0.1:8080/"));
}

/// A full hop as the transport will run it: POST with credentials, 302 to
/// another origin -> GET with no body headers and no credentials.
#[test]
fn post_302_cross_origin_end_to_end() {
    let from = url("http://a.test/form");
    let mut h = map(&[
        ("content-type", "application/x-www-form-urlencoded"),
        ("content-length", "3"),
        ("authorization", "Bearer t"),
        ("accept", "*/*"),
    ]);
    let Next::Follow {
        url: to,
        method,
        drop_body,
    } = redirect::next(
        302,
        &Method::POST,
        &from,
        Some(&hv("http://b.test/done")),
        0,
        false,
    )
    else {
        panic!("must follow");
    };
    assert_eq!(method, Method::GET);
    assert!(drop_body);
    redirect::apply(&mut h, &from, &to, drop_body);
    assert_eq!(names(&h), vec!["accept: */*".to_string()]);
}
