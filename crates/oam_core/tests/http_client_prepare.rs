//! `http_client::prepare`: URL, method and header preparation for the fetch
//! op (#143). URL-heavy, so it lives in `tests/`, outside the published-URLs
//! gate's scan.

use http::header::HeaderValue;
use oam_core::http_client::prepare::{self, PrepareError};

fn ua() -> HeaderValue {
    HeaderValue::from_static("oam/test")
}

fn prep(
    url: &str,
    method: &str,
    headers: &[(&str, &str)],
    defaults: bool,
) -> Result<prepare::Prepared, PrepareError> {
    let owned: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    prepare::prepare(url, method, &owned, defaults, &ua())
}

fn wire(p: &prepare::Prepared) -> Vec<String> {
    p.headers
        .iter()
        .map(|(k, v)| format!("{}: {}", k, String::from_utf8_lossy(v.as_bytes())))
        .collect()
}

#[test]
fn parse_url_accepts_http_and_https_only() {
    for ok in [
        "http://a.test/",
        "https://a.test:8443/x?y#z",
        "HTTP://A.TEST",
        "http://[::1]:80/",
    ] {
        assert!(prepare::parse_url(ok).is_ok(), "{ok}");
    }
    for bad in [
        "",
        "not a url",
        "/relative",
        "ftp://a.test/",
        "file:///c:/x",
        "ws://a.test/",
        "data:,x",
        "http://",
        "http://[::1/",
        "http://a b/",
        // WHATWG-legal domain, not an http::Uri authority (reqwest:
        // url_invalid_uri, a builder error too).
        "http://a\"b.test/",
        "http://a{b}.test/",
    ] {
        assert_eq!(prepare::parse_url(bad), Err("builder error"), "{bad:?}");
    }
}

#[test]
fn error_texts_are_todays() {
    assert_eq!(
        prep("http://a.test/", "GE T", &[], true)
            .unwrap_err()
            .to_string(),
        "fetch: invalid method 'GE T'"
    );
    // The method is checked before the URL.
    assert_eq!(
        prep("::nope::", "B@D", &[], true).unwrap_err(),
        PrepareError::InvalidMethod("B@D".into())
    );
    assert_eq!(
        prep("ftp://a.test/", "GET", &[], true)
            .unwrap_err()
            .to_string(),
        "builder error"
    );
    assert_eq!(
        prep("http://a.test/", "GET", &[("bad name", "v")], true).unwrap_err(),
        PrepareError::Builder
    );
    assert_eq!(
        prep("http://a.test/", "GET", &[("x", "a\nb")], true).unwrap_err(),
        PrepareError::Builder
    );
    assert_eq!(
        prep("http://a.test/", "GET", &[("", "v")], true).unwrap_err(),
        PrepareError::Builder
    );
}

#[test]
fn methods_are_tokens_taken_verbatim() {
    for m in ["GET", "POST", "PATCH", "QUERY", "PROPFIND", "get"] {
        assert_eq!(
            prep("http://a.test/", m, &[], true)
                .unwrap()
                .method
                .as_str(),
            m
        );
    }
}

#[test]
fn user_headers_keep_order_and_duplicates_then_defaults() {
    let p = prep(
        "http://a.test/p",
        "POST",
        &[
            ("X-B", "2"),
            ("x-a", "1"),
            ("X-B", "3"),
            ("Content-Type", "text/plain"),
        ],
        true,
    )
    .unwrap();
    assert_eq!(
        wire(&p),
        vec![
            "x-b: 2",
            "x-b: 3",
            "x-a: 1",
            "content-type: text/plain",
            "accept: */*",
            "user-agent: oam/test",
            "accept-encoding: gzip,deflate",
        ]
    );
    assert!(
        !p.headers.contains_key("host"),
        "host is hyper-util's to add"
    );
}

#[test]
fn defaults_never_override_user_values() {
    let p = prep(
        "http://a.test/",
        "GET",
        &[
            ("Accept", "text/html"),
            ("User-Agent", "me"),
            ("Accept-Encoding", "identity"),
        ],
        true,
    )
    .unwrap();
    assert_eq!(
        wire(&p),
        vec![
            "accept: text/html",
            "user-agent: me",
            "accept-encoding: identity"
        ]
    );
}

#[test]
fn default_headers_off_sends_only_the_users() {
    let p = prep("http://a.test/", "GET", &[("x", "1")], false).unwrap();
    assert_eq!(wire(&p), vec!["x: 1"]);
    let p = prep("http://a.test/", "GET", &[], false).unwrap();
    assert!(p.headers.is_empty());
}

#[test]
fn obs_text_header_values_pass() {
    let owned = vec![("x-latin".to_string(), "caf\u{e9}".to_string())];
    let p = prepare::prepare("http://a.test/", "GET", &owned, false, &ua()).unwrap();
    assert_eq!(p.headers["x-latin"].as_bytes(), "caf\u{e9}".as_bytes());
}

#[test]
fn userinfo_becomes_basic_auth_and_leaves_the_url() {
    let cases = [
        ("http://user:pass@a.test/", Some("Basic dXNlcjpwYXNz")), // user:pass
        ("http://user@a.test/", Some("Basic dXNlcjo=")),          // user:
        ("http://:pass@a.test/", Some("Basic OnBhc3M=")),         // :pass
        (
            "http://us%40er:p%3Ass@a.test/",
            Some("Basic dXNAZXI6cDpzcw=="),
        ), // us@er:p:ss
        ("http://caf%C3%A9:x@a.test/", Some("Basic Y2Fmw6k6eA==")), // café:x
        ("http://a.test/", None),
        ("http://@a.test/", None),
        // An undecodable username yields no credential (reqwest); the
        // userinfo is still removed.
        ("http://%FF:x@a.test/", None),
    ];
    for (raw, want) in cases {
        let p = prep(raw, "GET", &[], false).unwrap();
        assert_eq!(
            p.headers.get("authorization").map(|v| v.to_str().unwrap()),
            want,
            "{raw}"
        );
        assert_eq!(p.url.username(), "", "{raw}");
        assert_eq!(p.url.password(), None, "{raw}");
        if let Some(v) = p.headers.get("authorization") {
            assert!(v.is_sensitive());
        }
    }
    // An undecodable password yields `user:` with no password.
    let p = prep("http://u:%FF@a.test/", "GET", &[], false).unwrap();
    assert_eq!(p.headers["authorization"], "Basic dTo=");
}

#[test]
fn userinfo_credential_leads_and_an_explicit_authorization_wins() {
    let p = prep("http://u:p@a.test/", "GET", &[("x", "1")], true).unwrap();
    assert_eq!(wire(&p)[0], "authorization: Basic dTpw");
    let p = prep(
        "http://u:p@a.test/",
        "GET",
        &[("Authorization", "Bearer t")],
        false,
    )
    .unwrap();
    assert_eq!(wire(&p), vec!["authorization: Bearer t"]);
}

#[test]
fn prepared_url_has_no_fragment() {
    let p = prep("https://a.test/x?q=1#frag", "GET", &[], true).unwrap();
    assert_eq!(p.url.as_str(), "https://a.test/x?q=1");
}

#[test]
fn to_uri_drops_userinfo_and_fragment() {
    let cases = [
        ("http://u:p@a.test:8080/x?y=1#z", "http://a.test:8080/x?y=1"),
        ("https://a.test/", "https://a.test/"),
        ("https://a.test:443/p", "https://a.test/p"),
        ("http://[::1]:3000/", "http://[::1]:3000/"),
        ("http://caf\u{e9}.test/", "http://xn--caf-dma.test/"),
        (
            "http://a.test/%7B%7D?a={b}|c",
            "http://a.test/%7B%7D?a={b}|c",
        ),
    ];
    for (raw, want) in cases {
        let uri = prepare::to_uri(&url::Url::parse(raw).unwrap()).unwrap();
        assert_eq!(uri.to_string(), want, "{raw}");
        assert!(!uri.authority().unwrap().as_str().contains('@'));
    }
}

/// `to_uri` converts every URL `parse_url` accepts except a domain holding
/// one of the four WHATWG-legal, RFC 3986-illegal bytes. Checked for all 256
/// byte values (as the UTF-8 encoding of U+0000..U+00FF) in the host, the
/// userinfo, the path, the query and the fragment.
#[test]
fn to_uri_converts_every_parseable_url_byte_by_byte() {
    let mut checked = 0;
    let mut refused = Vec::new();
    for b in 0u32..=0xff {
        let ch = char::from_u32(b).unwrap();
        for (i, template) in [
            "http://a{}b.test/",
            "http://a.test/p{}q",
            "http://a.test/p?q{}r",
            "http://a.test/p?q#f{}g",
            "https://u{}v:w{}x@a.test/",
            "http://a.test:8080/{}",
        ]
        .into_iter()
        .enumerate()
        {
            let raw = template.replace("{}", &ch.to_string());
            if let Ok(url) = url::Url::parse(&raw) {
                checked += 1;
                match prepare::to_uri(&url) {
                    Ok(uri) => {
                        assert_eq!(uri.scheme_str(), Some(url.scheme()));
                        assert!(!uri.to_string().contains('#'), "{raw:?}");
                    }
                    Err(e) => {
                        assert_eq!(e, "builder error");
                        refused.push((i, ch));
                    }
                }
            }
        }
    }
    assert!(checked > 1000, "only {checked} URLs parsed");
    assert_eq!(refused, vec![(0, '"'), (0, '`'), (0, '{'), (0, '}')]);
}

#[test]
fn host_for_connect_strips_brackets() {
    let host = |s: &str| {
        prepare::host_for_connect(&prepare::to_uri(&url::Url::parse(s).unwrap()).unwrap())
    };
    assert_eq!(host("http://[::1]:8080/").as_deref(), Some("::1"));
    assert_eq!(host("http://[fe80::1]/").as_deref(), Some("fe80::1"));
    assert_eq!(host("http://127.0.0.1/").as_deref(), Some("127.0.0.1"));
    assert_eq!(host("https://A.Test/").as_deref(), Some("a.test"));
    assert_eq!(
        prepare::host_for_connect(&http::Uri::from_static("/path")),
        None
    );
}

#[test]
fn origin_eq_normalizes() {
    let eq = |a: &str, b: &str| {
        prepare::origin_eq(&url::Url::parse(a).unwrap(), &url::Url::parse(b).unwrap())
    };
    assert!(eq("http://a.test/x", "http://A.test:80/y?z#w"));
    assert!(eq("http://u:p@a.test/", "http://a.test/"));
    assert!(eq("http://[::1]/", "http://[0::1]:80/"));
    assert!(!eq("http://a.test/", "https://a.test/"));
    assert!(!eq("https://a.test/", "https://a.test:444/"));
    assert!(!eq("http://[::1]/", "http://127.0.0.1/"));
}
