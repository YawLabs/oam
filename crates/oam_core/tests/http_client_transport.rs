//! `http_client::transport` over real loopback sockets: node's connect errors
//! through hyper-util, the environment proxy (absolute form, CONNECT, h2
//! inside a tunnel, socks refusal), a lookup-hooked route, lazy TLS failure,
//! connection reuse and the request-body wire shapes (#143 slice C).

mod common;

use oam_core::net_connect::PinAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt as _;

use bytes::Bytes;
use common::*;
use http::header::{HOST, HeaderValue, PROXY_AUTHORIZATION};
use http_body_util::BodyExt;
use hyper_util::client::proxy::matcher::Matcher;
use oam_core::OpOutcome;
use oam_core::http_client::transport::{channel_body, empty_body, full_body};
use oam_core::http_client::{
    Alpn, HttpTransport, ProxySource, ReqBody, Route, SendError, TlsRange, TlsSource,
};

const ATTEMPT: Duration = Duration::from_millis(250);

fn get(uri: &str) -> http::Request<ReqBody> {
    request("GET", uri, empty_body())
}

fn request(method: &str, uri: &str, body: ReqBody) -> http::Request<ReqBody> {
    http::Request::builder()
        .method(method)
        .uri(uri)
        .body(body)
        .unwrap()
}

async fn send(
    transport: &HttpTransport,
    route: &Route,
    request: http::Request<ReqBody>,
) -> Result<http::Response<hyper::body::Incoming>, SendError> {
    transport.send(route, request).await
}

async fn body_text(response: http::Response<hyper::body::Incoming>) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn url(s: &str) -> url::Url {
    url::Url::parse(s).unwrap()
}

fn ok_reply(_: &Received) -> Vec<u8> {
    response("200 OK", &[], b"ok")
}

/// A refused IP literal is node's `ExceptionWithHostPort`, found through
/// hyper-util's error.
#[tokio::test(flavor = "multi_thread")]
async fn refused_ip_literal_is_node_s_connect_error() {
    within(async {
        let transport = transport(ProxySource::None);
        let route = transport.route(false, ATTEMPT, oam_core::http_client::TlsRange::Both);
        let port = closed_port().await;
        let target = format!("http://127.0.0.1:{port}/");
        let err = send(&transport, &route, get(&target)).await.unwrap_err();
        match err.to_outcome(&url(&target)) {
            OpOutcome::NodeFailed {
                code,
                message,
                syscall,
                errno,
                address,
                port: got_port,
                hostname,
                path,
                dest,
            } => {
                assert_eq!(dest, None);
                assert_eq!(code, "ECONNREFUSED");
                assert_eq!(message, format!("connect ECONNREFUSED 127.0.0.1:{port}"));
                assert_eq!(syscall.as_deref(), Some("connect"));
                assert!(errno.is_some());
                assert_eq!(address.as_deref(), Some("127.0.0.1"));
                assert_eq!(got_port, Some(port));
                assert_eq!(hostname, None);
                assert_eq!(path, None);
            }
            other => panic!("{other:?} ({err})"),
        }
    })
    .await;
}

/// A hooked route dials the hook's addresses in order; two refusals are
/// node's aggregate, through the unpooled client too.
#[tokio::test(flavor = "multi_thread")]
async fn hooked_route_two_refused_addresses_is_an_aggregate() {
    within(async {
        let transport = transport(ProxySource::None);
        let route = transport.route(true, ATTEMPT, oam_core::http_client::TlsRange::Both);
        let port = closed_port().await;
        let target = format!("http://pinned.test:{port}/");
        let uri: http::Uri = target.parse().unwrap();
        let (key, host) = route.lookup_needed(&uri).unwrap();
        assert_eq!(host, "pinned.test");
        // The key is the AUTHORITY, and the host half of it is lowercased, so
        // a hook answer files under the same key however the URL spelled the
        // name.
        assert_eq!(key, format!("pinned.test:{port}"));
        route.set_addrs(
            &key,
            vec![
                "127.0.0.1".parse::<PinAddr>().unwrap(),
                "::1".parse::<PinAddr>().unwrap(),
            ],
        );
        assert_eq!(route.lookup_needed(&uri), None);
        let err = send(&transport, &route, get(&target)).await.unwrap_err();
        match err.to_outcome(&url(&target)) {
            OpOutcome::NodeAggregateFailed { errors } => {
                let addresses: Vec<_> = errors.iter().map(|e| e.address.as_deref()).collect();
                assert_eq!(addresses, [Some("127.0.0.1"), Some("::1")]);
                assert_eq!(errors[0].code, "ECONNREFUSED");
                assert!(errors.iter().all(|e| e.port == Some(port)));
            }
            other => panic!("{other:?} ({err})"),
        }
    })
    .await;
}

/// A hooked route never falls back to system DNS: a host its hook did not
/// answer for is not dialled at all, even when it would resolve to a live
/// server. IP literals need no answer.
#[tokio::test(flavor = "multi_thread")]
async fn hooked_route_dials_only_hosts_the_hook_resolved() {
    within(async {
        let server = serve_replies(ok_reply).await;
        let transport = transport(ProxySource::None);
        let route = transport.route(true, ATTEMPT, oam_core::http_client::TlsRange::Both);
        route.set_addrs("a.test:80", vec!["127.0.0.1".parse().unwrap()]);
        let local = format!("http://localhost:{}/", server.port);
        let literal = format!("http://127.0.0.1:{}/", server.port);
        assert_eq!(
            route.lookup_needed(&local.parse().unwrap()),
            Some((
                format!("localhost:{}", server.port),
                "localhost".to_string()
            ))
        );
        assert_eq!(route.lookup_needed(&literal.parse().unwrap()), None);
        let err = send(&transport, &route, get(&local)).await.unwrap_err();
        match err.to_outcome(&url(&local)) {
            OpOutcome::Failed(text) => {
                assert_eq!(text, format!("error sending request for url ({local})"))
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(server.accepts(), 0);
        let response = send(&transport, &route, get(&literal)).await.unwrap();
        assert_eq!(response.status(), 200);
        // A pooled route never needs a lookup.
        let pooled = transport.route(false, ATTEMPT, oam_core::http_client::TlsRange::Both);
        assert!(!pooled.is_hooked());
        assert_eq!(pooled.lookup_needed(&local.parse().unwrap()), None);
    })
    .await;
}

/// The hook is asked per AUTHORITY, not per host: the same name on another
/// port parks again, and its answer does not dial on the first port's
/// addresses. A guard whose policy turns on the port (allow 443 on an
/// internal name, refuse 22 or 6379) is consulted for every port a redirect
/// chain reaches, which is what node does -- it asks per connection.
#[tokio::test(flavor = "multi_thread")]
async fn a_hooked_route_asks_the_hook_again_for_the_same_host_on_another_port() {
    within(async {
        let transport = transport(ProxySource::None);
        let route = transport.route(true, ATTEMPT, oam_core::http_client::TlsRange::Both);
        let first: http::Uri = "http://guarded.test:8443/a".parse().unwrap();
        let second: http::Uri = "http://guarded.test:6379/b".parse().unwrap();
        let default_port: http::Uri = "http://guarded.test/c".parse().unwrap();
        let explicit_80: http::Uri = "http://guarded.test:80/d".parse().unwrap();
        let https_default: http::Uri = "https://guarded.test/e".parse().unwrap();

        let (key, host) = route.lookup_needed(&first).unwrap();
        assert_eq!(
            (key.as_str(), host.as_str()),
            ("guarded.test:8443", "guarded.test")
        );
        route.set_addrs(&key, vec!["127.0.0.1".parse::<PinAddr>().unwrap()]);
        assert_eq!(route.lookup_needed(&first), None);

        // Same host, different port: the hook must be asked again.
        let (second_key, second_host) = route.lookup_needed(&second).unwrap();
        assert_eq!(second_key, "guarded.test:6379");
        assert_eq!(second_host, "guarded.test");

        // An elided default port is the same authority as the explicit one,
        // so answering one covers the other and the hook is not asked twice.
        let (default_key, _) = route.lookup_needed(&default_port).unwrap();
        assert_eq!(default_key, "guarded.test:80");
        route.set_addrs(&default_key, vec!["127.0.0.1".parse::<PinAddr>().unwrap()]);
        assert_eq!(route.lookup_needed(&explicit_80), None);
        // ... and https:443 is a third authority of its own.
        assert_eq!(
            route.lookup_needed(&https_default),
            Some(("guarded.test:443".to_string(), "guarded.test".to_string()))
        );
    })
    .await;
}

/// Send `request` on the hooked `route`, which must first ask for a hook
/// answer for `host` (`key` its authority): the send hands the request back
/// unsent, and goes out once the route has `127.0.0.1` for it.
async fn send_after_lookup(
    transport: &HttpTransport,
    route: &Route,
    request: http::Request<ReqBody>,
    key: &str,
    host: &str,
) -> http::Response<hyper::body::Incoming> {
    let mut err = send(transport, route, request).await.unwrap_err();
    assert_eq!(
        err.lookup_needed(),
        Some((key.to_string(), host.to_string())),
        "{err}"
    );
    let returned = err.take_request().expect("the request comes back unsent");
    route.set_addrs(key, vec!["127.0.0.1".parse::<PinAddr>().unwrap()]);
    send(transport, route, returned).await.unwrap()
}

/// #179: a lookup-hooked dispatcher's connections are pooled across its
/// fetches, as undici's Agent pools its sockets, and the hook is asked once
/// per connection the pool opens -- not once per fetch, which opened (and
/// closed) a connection for every request.
///
/// The security half: a pooled connection is only ever reused for the
/// authority whose hook answer opened it (another name on the same address
/// and port asks the hook again and opens its own), and never by another
/// dispatcher. Dropping the dispatcher's pool (its `close()`, `destroy()`
/// or collection) closes the idle connection and forgets the pool.
#[tokio::test(flavor = "multi_thread")]
async fn a_dispatcher_pool_reuses_connections_and_asks_the_hook_once_per_connection() {
    within(async {
        let closes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let closes_seen = closes.clone();
        let server = serve(move |mut conn, _, seen| {
            let closes = closes_seen.clone();
            async move {
                while let Some(request) = conn.request().await {
                    seen.lock().unwrap().push(request);
                    if !conn.send(&response("200 OK", &[], b"ok")).await {
                        break;
                    }
                }
                closes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        })
        .await;
        let transport = transport(ProxySource::None);
        let a = format!("http://a.test:{}/", server.port);
        let a_key = format!("a.test:{}", server.port);

        // The first fetch through dispatcher 7 asks for an answer, and its
        // connection goes back to 7's pool.
        let route = transport.agent_route(7, ATTEMPT, TlsRange::Both);
        assert_eq!(
            route.lookup_needed(&a.parse().unwrap()),
            Some((a_key.clone(), "a.test".to_string()))
        );
        let response = send_after_lookup(&transport, &route, get(&a), &a_key, "a.test").await;
        assert_eq!(body_text(response).await, "ok");
        let_the_pool_settle().await;
        // Every later fetch through 7 takes it: no answer asked, no connect.
        for _ in 0..3 {
            let route = transport.agent_route(7, ATTEMPT, TlsRange::Both);
            assert_eq!(route.lookup_needed(&a.parse().unwrap()), None);
            let response = send(&transport, &route, get(&a)).await.unwrap();
            assert_eq!(body_text(response).await, "ok");
            let_the_pool_settle().await;
        }
        assert_eq!(server.accepts(), 1);

        // Another name on the same address and port is another authority:
        // the hook is asked, and the answer opens a connection of its own.
        let b = format!("http://b.test:{}/", server.port);
        let route = transport.agent_route(7, ATTEMPT, TlsRange::Both);
        let b_key = format!("b.test:{}", server.port);
        let response = send_after_lookup(&transport, &route, get(&b), &b_key, "b.test").await;
        assert_eq!(body_text(response).await, "ok");
        assert_eq!(server.accepts(), 2);

        // Another dispatcher never takes 7's connections.
        let other = transport.agent_route(8, ATTEMPT, TlsRange::Both);
        let response = send_after_lookup(&transport, &other, get(&a), &a_key, "a.test").await;
        assert_eq!(body_text(response).await, "ok");
        assert_eq!(server.accepts(), 3);
        let_the_pool_settle().await;

        // Dropping 7's pool closes its two idle connections; 8's stays.
        assert!(transport.drop_agent(7));
        assert!(!transport.drop_agent(7), "dropped once");
        tokio::time::timeout(Duration::from_secs(10), async {
            while closes.load(std::sync::atomic::Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the dropped pool's connections close");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(closes.load(std::sync::atomic::Ordering::SeqCst), 2);
        // A fetch through 7 afterwards starts a pool of its own again.
        let route = transport.agent_route(7, ATTEMPT, TlsRange::Both);
        let response = send_after_lookup(&transport, &route, get(&a), &a_key, "a.test").await;
        assert_eq!(body_text(response).await, "ok");
        assert_eq!(server.accepts(), 4);
        let hosts: Vec<_> = server
            .seen()
            .iter()
            .map(|r| r.head.get("host").unwrap_or_default().to_string())
            .collect();
        let (a_host, b_host) = (
            format!("a.test:{}", server.port),
            format!("b.test:{}", server.port),
        );
        assert_eq!(
            hosts,
            [
                &a_host, &a_host, &a_host, &a_host, &b_host, &a_host, &a_host
            ]
            .map(String::clone)
        );
    })
    .await;
}

/// A hook answer opens the connection it was asked for and is spent there:
/// the request that carries it never takes an idle connection instead, and
/// the next connection to the same authority asks again -- the hook's call
/// count is node's, one per connection. An answer for one authority is
/// never spent on another.
#[tokio::test(flavor = "multi_thread")]
async fn a_hook_answer_opens_one_connection_for_its_own_authority() {
    within(async {
        let server = serve_replies(ok_reply).await;
        let transport = transport(ProxySource::None);
        let a = format!("http://a.test:{}/", server.port);
        let a_key = format!("a.test:{}", server.port);
        let route = transport.agent_route(3, ATTEMPT, TlsRange::Both);
        let response = send_after_lookup(&transport, &route, get(&a), &a_key, "a.test").await;
        assert_eq!(body_text(response).await, "ok");
        let_the_pool_settle().await;
        // With an idle connection there, nothing needs the hook ...
        assert_eq!(route.lookup_needed(&a.parse().unwrap()), None);
        // ... but an answer in hand opens a connection of its own.
        route.set_addrs(&a_key, vec!["127.0.0.1".parse::<PinAddr>().unwrap()]);
        let response = send(&transport, &route, get(&a)).await.unwrap();
        assert_eq!(body_text(response).await, "ok");
        assert_eq!(server.accepts(), 2);
        // ... and is spent on it: the next send takes an idle connection.
        let_the_pool_settle().await;
        let response = send(&transport, &route, get(&a)).await.unwrap();
        assert_eq!(body_text(response).await, "ok");
        assert_eq!(server.accepts(), 2);

        // An answer for b.test is not spent on a.test, which takes an idle
        // connection.
        let b_key = format!("b.test:{}", server.port);
        route.set_addrs(&b_key, vec!["127.0.0.1".parse::<PinAddr>().unwrap()]);
        let_the_pool_settle().await;
        let response = send(&transport, &route, get(&a)).await.unwrap();
        assert_eq!(body_text(response).await, "ok");
        assert_eq!(server.accepts(), 2);
        assert_eq!(
            route.lookup_needed(&format!("http://b.test:{}/", server.port).parse().unwrap()),
            None,
            "the b.test answer is still held"
        );
        assert!(transport.drop_agent(3));
    })
    .await;
}

/// A send that needs the hook hands its request back as it was built --
/// its URI not rewritten, a streamed body unread -- and that request, sent
/// again with the answer, delivers every byte: the fetch loop carries it
/// across the park when a send's idle connection was taken from under it.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_handed_back_for_a_lookup_goes_out_whole() {
    within(async {
        let server = serve_replies(ok_reply).await;
        let transport = transport(ProxySource::None);
        let route = transport.agent_route(5, ATTEMPT, TlsRange::Both);
        let target = format!("http://streamed.test:{}/up?q=1", server.port);
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tx.send(oam_core::outbound_data(b"ab".to_vec()))
            .await
            .unwrap();
        tx.send(oam_core::outbound_data(b"cde".to_vec()))
            .await
            .unwrap();
        drop(tx);
        let mut err = send(
            &transport,
            &route,
            request("POST", &target, channel_body(rx)),
        )
        .await
        .unwrap_err();
        let (key, host) = err.lookup_needed().expect("needs the hook");
        assert_eq!(host, "streamed.test");
        assert_eq!(server.accepts(), 0);
        let returned = err.take_request().unwrap();
        assert!(err.take_request().is_none(), "handed back once");
        assert_eq!(returned.uri().to_string(), target);
        route.set_addrs(&key, vec!["127.0.0.1".parse::<PinAddr>().unwrap()]);
        let response = send(&transport, &route, returned).await.unwrap();
        assert_eq!(body_text(response).await, "ok");
        let seen = server.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].head.target, "/up?q=1");
        assert_eq!(seen[0].body, b"abcde");
        transport.drop_agent(5);
    })
    .await;
}

/// An http destination goes to the proxy in absolute form; the credentials
/// are the caller's to add, per hop.
#[tokio::test(flavor = "multi_thread")]
async fn http_proxy_gets_absolute_form_and_caller_adds_proxy_authorization() {
    within(async {
        let proxy = proxy(None, b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok").await;
        let rules = Matcher::builder()
            .http(format!("http://u:p@127.0.0.1:{}", proxy.port))
            .build();
        let transport = transport(ProxySource::Fixed(Box::new(rules)));
        let route = transport.route(false, ATTEMPT, oam_core::http_client::TlsRange::Both);
        let target = "http://origin.test:81/p?q=1";
        let uri: http::Uri = target.parse().unwrap();
        let auth = transport.proxy_authorization(&route, &uri).unwrap();
        assert_eq!(auth, "Basic dTpw");
        assert_eq!(
            transport.proxy_authorization(&route, &"https://origin.test/".parse().unwrap()),
            None
        );
        assert_eq!(
            transport.proxy_authorization(
                &transport.route(true, ATTEMPT, oam_core::http_client::TlsRange::Both),
                &uri
            ),
            None
        );

        let mut with_auth = get(target);
        with_auth.headers_mut().insert(PROXY_AUTHORIZATION, auth);
        let response = send(&transport, &route, with_auth).await.unwrap();
        assert_eq!(body_text(response).await, "ok");
        let_the_pool_settle().await;
        let response = send(&transport, &route, get(target)).await.unwrap();
        assert_eq!(body_text(response).await, "ok");

        let seen = proxy.seen();
        assert_eq!(seen.len(), 2);
        for request in &seen {
            assert_eq!(request.head.method, "GET");
            assert_eq!(request.head.target, target);
            assert_eq!(request.head.get("host"), Some("origin.test:81"));
        }
        assert_eq!(seen[0].head.get("proxy-authorization"), Some("Basic dTpw"));
        assert!(!seen[1].head.has("proxy-authorization"));
        // Both went over one pooled connection to the proxy.
        assert_eq!(proxy.accepts(), 1);
    })
    .await;
}

/// An https destination behind a proxy is a CONNECT carrying the proxy
/// credentials and oam's user-agent; a refused CONNECT is the uncoded send
/// failure.
#[tokio::test(flavor = "multi_thread")]
async fn https_via_proxy_sends_connect_with_user_agent_and_auth() {
    within(async {
        let proxy = proxy(Some(b"HTTP/1.1 403 Forbidden\r\n\r\n"), b"").await;
        let rules = Matcher::builder()
            .all(format!("http://u:p@127.0.0.1:{}", proxy.port))
            .build();
        let transport = transport(ProxySource::Fixed(Box::new(rules)));
        let route = transport.route(false, ATTEMPT, oam_core::http_client::TlsRange::Both);
        let target = "https://example.test:8443/";
        let err = send(&transport, &route, get(target)).await.unwrap_err();
        match err.to_outcome(&url(target)) {
            OpOutcome::Failed(text) => {
                assert_eq!(text, format!("error sending request for url ({target})"))
            }
            other => panic!("{other:?}"),
        }
        let seen = proxy.seen();
        assert_eq!(seen.len(), 1);
        let head = &seen[0].head;
        assert_eq!(head.method, "CONNECT");
        assert_eq!(head.target, "example.test:8443");
        assert_eq!(head.get("host"), Some("example.test:8443"));
        assert_eq!(head.get("user-agent"), Some(USER_AGENT));
        assert_eq!(head.get("proxy-authorization"), Some("Basic dTpw"));
    })
    .await;
}

/// h2 is negotiated with the origin inside a CONNECT tunnel, for a route that
/// offers it (`allowH2`).
#[tokio::test(flavor = "multi_thread")]
async fn https_through_connect_tunnel_negotiates_h2() {
    within(async {
        let origin = serve_h2_tls("h2 ok").await;
        let proxy = proxy(None, b"").await;
        let rules = Matcher::builder()
            .https(format!("http://127.0.0.1:{}", proxy.port))
            .build();
        let transport = transport(ProxySource::Fixed(Box::new(rules)));
        let route = transport
            .route(false, ATTEMPT, TlsRange::Both)
            .with_alpn(Alpn::AllowH2);
        let target = format!("https://localhost:{}/x", origin.port);
        let response = send(&transport, &route, get(&target)).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.version(), http::Version::HTTP_2);
        assert_eq!(response.headers().get("x-version").unwrap(), "HTTP/2.0");
        assert_eq!(body_text(response).await, "h2 ok");
        let seen = proxy.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].head.method, "CONNECT");
        assert_eq!(seen[0].head.target, format!("localhost:{}", origin.port));
    })
    .await;
}

/// A request sent over h2 carries its authority in `:authority` and NO `host`
/// field beside it (RFC 9113 8.3.1). `ClientRequest` sets a `host` header in
/// its constructor, as node does, so every `http.request` / `https.request`
/// reaches the transport carrying one; over h2 that would be a second copy of
/// `:authority`, which Google's frontends answer by resetting the connection.
#[tokio::test(flavor = "multi_thread")]
async fn an_h2_request_sends_the_authority_without_a_host_field() {
    within(async {
        let origin = serve_h2_tls("h2 ok").await;
        let transport = transport(ProxySource::None);
        let route = transport
            .route(false, ATTEMPT, TlsRange::Both)
            .with_alpn(Alpn::AllowH2);
        let target = format!("https://localhost:{}/x", origin.port);
        let mut request = get(&target);
        // What ClientRequest writes: the host, and the port unless it is the
        // scheme's default (which is also how hyper-util spells the header it
        // adds for HTTP/1.1, so a request that names its own origin carries
        // the authority the URI does).
        request.headers_mut().insert(
            HOST,
            HeaderValue::from_str(&format!("localhost:{}", origin.port)).unwrap(),
        );
        let response = send(&transport, &route, request).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.version(), http::Version::HTTP_2);
        let seen = origin.seen();
        assert_eq!(seen.len(), 1);
        assert!(!seen[0].head.has("host"), "{:?}", seen[0].head);
        assert_eq!(
            seen[0].head.target,
            format!("https://localhost:{}/x", origin.port)
        );
    })
    .await;
}

/// A `host` header naming a DIFFERENT authority is the caller's HTTP/1.1
/// routing override; `:authority` is what carries it on h2, so it becomes
/// that -- one field, not two -- and the request still goes to the server the
/// URL dialled.
#[tokio::test(flavor = "multi_thread")]
async fn an_h2_host_header_that_overrides_the_authority_becomes_it() {
    within(async {
        let origin = serve_h2_tls("h2 ok").await;
        let transport = transport(ProxySource::None);
        let route = transport
            .route(false, ATTEMPT, TlsRange::Both)
            .with_alpn(Alpn::AllowH2);
        let target = format!("https://localhost:{}/x", origin.port);
        let mut request = get(&target);
        request
            .headers_mut()
            .insert(HOST, HeaderValue::from_static("vhost.example"));
        let response = send(&transport, &route, request).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.version(), http::Version::HTTP_2);
        let seen = origin.seen();
        assert_eq!(seen.len(), 1);
        assert!(!seen[0].head.has("host"), "{:?}", seen[0].head);
        assert_eq!(seen[0].head.target, "https://vhost.example/x");
    })
    .await;
}

/// A request the caller wrote as an HTTP/2 one -- what `http2.connect` sends,
/// where the pseudo-headers are the caller's to write -- keeps the
/// `:authority` it authored; only the `host` field goes, so the pair the RFC
/// calls malformed still never reaches the wire.
#[tokio::test(flavor = "multi_thread")]
async fn an_authored_h2_request_keeps_the_authority_it_wrote() {
    within(async {
        let origin = serve_h2_tls("h2 ok").await;
        let transport = transport(ProxySource::None);
        let route = transport
            .route(false, ATTEMPT, TlsRange::Both)
            .with_alpn(Alpn::AllowH2);
        let target = format!("https://localhost:{}/x", origin.port);
        let mut request = request("GET", &target, empty_body());
        *request.version_mut() = http::Version::HTTP_2;
        request
            .headers_mut()
            .insert(HOST, HeaderValue::from_static("vhost.example"));
        let response = send(&transport, &route, request).await.unwrap();
        assert_eq!(response.status(), 200);
        let seen = origin.seen();
        assert_eq!(seen.len(), 1);
        assert!(!seen[0].head.has("host"), "{:?}", seen[0].head);
        assert_eq!(
            seen[0].head.target,
            format!("https://localhost:{}/x", origin.port)
        );
    })
    .await;
}

/// The same request over HTTP/1.1 keeps its `Host` header: there is no
/// `:authority` there, and an override is how a caller reaches a named
/// virtual host on an address it chose.
#[tokio::test(flavor = "multi_thread")]
async fn an_h1_request_keeps_the_host_header_it_was_given() {
    within(async {
        let server = serve_replies(ok_reply).await;
        let transport = transport(ProxySource::None);
        let route = transport.route(false, ATTEMPT, oam_core::http_client::TlsRange::Both);
        let target = format!("http://127.0.0.1:{}/x", server.port);
        let mut request = get(&target);
        request
            .headers_mut()
            .insert(HOST, HeaderValue::from_static("vhost.example"));
        let response = send(&transport, &route, request).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.version(), http::Version::HTTP_11);
        let seen = server.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].head.all("host"), vec!["vhost.example"]);
        assert_eq!(seen[0].head.target, "/x");
    })
    .await;
}

/// A proxy that refuses the connection fails with node's error naming the
/// PROXY's address, for an http destination and through a tunnel.
#[tokio::test(flavor = "multi_thread")]
async fn refused_proxy_names_the_proxy() {
    within(async {
        let port = closed_port().await;
        let rules = Matcher::builder()
            .all(format!("http://127.0.0.1:{port}"))
            .build();
        let transport = transport(ProxySource::Fixed(Box::new(rules)));
        let route = transport.route(false, ATTEMPT, oam_core::http_client::TlsRange::Both);
        for target in ["http://origin.test/", "https://origin.test/"] {
            let err = send(&transport, &route, get(target)).await.unwrap_err();
            match err.to_outcome(&url(target)) {
                OpOutcome::NodeFailed {
                    code,
                    address,
                    port: got_port,
                    ..
                } => {
                    assert_eq!(code, "ECONNREFUSED", "{target}");
                    assert_eq!(address.as_deref(), Some("127.0.0.1"), "{target}");
                    assert_eq!(got_port, Some(port), "{target}");
                }
                other => panic!("{target}: {other:?} ({err})"),
            }
        }
    })
    .await;
}

/// A socks proxy is not supported: the uncoded send failure, as reqwest's
/// was.
#[tokio::test(flavor = "multi_thread")]
async fn socks_proxy_keeps_the_uncoded_text() {
    within(async {
        let server = serve_replies(ok_reply).await;
        let rules = Matcher::builder().all("socks5://127.0.0.1:1").build();
        let transport = transport(ProxySource::Fixed(Box::new(rules)));
        let route = transport.route(false, ATTEMPT, oam_core::http_client::TlsRange::Both);
        let target = format!("http://127.0.0.1:{}/", server.port);
        let err = send(&transport, &route, get(&target)).await.unwrap_err();
        match err.to_outcome(&url(&target)) {
            OpOutcome::Failed(text) => {
                assert_eq!(text, format!("error sending request for url ({target})"))
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(server.accepts(), 0);
    })
    .await;
}

/// A hooked route never goes through the environment proxy.
#[tokio::test(flavor = "multi_thread")]
async fn hooked_route_bypasses_the_proxy() {
    within(async {
        let server = serve_replies(ok_reply).await;
        let dead_proxy = closed_port().await;
        let rules = Matcher::builder()
            .all(format!("http://u:p@127.0.0.1:{dead_proxy}"))
            .build();
        let transport = transport(ProxySource::Fixed(Box::new(rules)));
        let route = transport.route(true, ATTEMPT, oam_core::http_client::TlsRange::Both);
        let target = format!("http://origin.test:{}/h", server.port);
        let uri: http::Uri = target.parse().unwrap();
        let (key, _) = route.lookup_needed(&uri).unwrap();
        route.set_addrs(&key, vec!["127.0.0.1".parse().unwrap()]);
        assert_eq!(transport.proxy_authorization(&route, &uri), None);
        let response = send(&transport, &route, get(&target)).await.unwrap();
        assert_eq!(body_text(response).await, "ok");
        let seen = server.seen();
        assert_eq!(seen[0].head.target, "/h");
        assert_eq!(
            seen[0].head.get("host"),
            Some(format!("origin.test:{}", server.port).as_str())
        );
    })
    .await;
}

/// TLS that cannot be configured fails https requests with its own text and
/// leaves http alone.
#[tokio::test(flavor = "multi_thread")]
async fn tls_unavailable_fails_https_only() {
    within(async {
        let server = serve_replies(ok_reply).await;
        let transport =
            transport_with_tls(TlsSource::Unavailable("x".to_string()), ProxySource::None);
        let route = transport.route(false, ATTEMPT, oam_core::http_client::TlsRange::Both);
        let https = format!("https://127.0.0.1:{}/", server.port);
        let err = send(&transport, &route, get(&https)).await.unwrap_err();
        match err.to_outcome(&url(&https)) {
            OpOutcome::Failed(text) => assert_eq!(text, "tls configuration error: x"),
            other => panic!("{other:?}"),
        }
        let http = format!("http://127.0.0.1:{}/", server.port);
        let response = send(&transport, &route, get(&http)).await.unwrap();
        assert_eq!(response.status(), 200);
    })
    .await;
}

/// Bodies read to the end leave the connection in the pool; a response
/// dropped unread closes its connection.
#[tokio::test(flavor = "multi_thread")]
async fn h1_idle_connection_is_reused_and_unread_body_closes_it() {
    within(async {
        let (closed_tx, mut closed_rx) = tokio::sync::mpsc::unbounded_channel::<bool>();
        let server = serve(move |mut conn, _, _| {
            let closed_tx = closed_tx.clone();
            async move {
                while let Some(request) = conn.request().await {
                    if request.head.target == "/kept" {
                        if !conn.send(&response("200 OK", &[], b"kept")).await {
                            return;
                        }
                        continue;
                    }
                    // Promise far more than is sent: the client cannot have
                    // read the body to its end when it drops the response.
                    conn.send(
                        b"HTTP/1.1 200 OK
content-length: 1000000

partial",
                    )
                    .await;
                    let closed = conn.closed_within(Duration::from_secs(10)).await;
                    let _ = closed_tx.send(closed);
                    return;
                }
            }
        })
        .await;
        let transport = transport(ProxySource::None);
        let route = transport.route(false, ATTEMPT, oam_core::http_client::TlsRange::Both);
        let kept = format!("http://127.0.0.1:{}/kept", server.port);
        for _ in 0..2 {
            let response = send(&transport, &route, get(&kept)).await.unwrap();
            assert_eq!(body_text(response).await, "kept");
            let_the_pool_settle().await;
        }
        assert_eq!(server.accepts(), 1);

        let partial = format!("http://127.0.0.1:{}/partial", server.port);
        let response = send(&transport, &route, get(&partial)).await.unwrap();
        assert_eq!(response.status(), 200);
        // The idle connection was reused for it...
        assert_eq!(server.accepts(), 1);
        drop(response);
        // ...and dropping the unread body closed it.
        assert_eq!(closed_rx.recv().await, Some(true));
    })
    .await;
}

/// Request bodies go out as reqwest sent them (measured with the reqwest op
/// before it was removed): a buffered body with `content-length`, a POST
/// with no body or an empty one with neither `content-length` nor
/// `transfer-encoding`, a streamed body chunked.
#[tokio::test(flavor = "multi_thread")]
async fn request_body_shapes_match_today() {
    within(async {
        let server = serve_replies(ok_reply).await;
        let transport = transport(ProxySource::None);
        let route = transport.route(false, ATTEMPT, oam_core::http_client::TlsRange::Both);
        let target = format!("http://127.0.0.1:{}/", server.port);

        let full = request("POST", &target, full_body(Bytes::from_static(b"xyz")));
        body_text(send(&transport, &route, full).await.unwrap()).await;
        let empty = request("POST", &target, empty_body());
        body_text(send(&transport, &route, empty).await.unwrap()).await;
        let empty_full = request("POST", &target, full_body(Bytes::new()));
        body_text(send(&transport, &route, empty_full).await.unwrap()).await;
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let streamed = request("POST", &target, channel_body(rx));
        let writer = tokio::spawn(async move {
            tx.send(oam_core::outbound_data(b"ab".to_vec()))
                .await
                .unwrap();
            tx.send(oam_core::outbound_data(b"cde".to_vec()))
                .await
                .unwrap();
        });
        body_text(send(&transport, &route, streamed).await.unwrap()).await;
        writer.await.unwrap();

        let seen = server.seen();
        assert_eq!(seen.len(), 4);
        assert_eq!(seen[0].head.get("content-length"), Some("3"));
        assert!(!seen[0].head.has("transfer-encoding"));
        assert_eq!(seen[0].body, b"xyz");
        for empty in &seen[1..3] {
            assert!(!empty.head.has("transfer-encoding"));
            assert!(!empty.head.has("content-length"));
            assert_eq!(empty.body, b"");
        }
        assert_eq!(seen[3].head.get("transfer-encoding"), Some("chunked"));
        assert!(!seen[3].head.has("content-length"));
        assert_eq!(seen[3].body, b"abcde");
    })
    .await;
}

/// An https origin that records the ALPN list each client hello offers
/// (`None`: no ALPN extension), selects none, and answers one HTTP/1.1
/// request per connection.
async fn alpn_recording_origin() -> (Server, Arc<Mutex<Vec<Option<Vec<String>>>>>) {
    let offers = Arc::new(Mutex::new(Vec::new()));
    let certs = rustls_pemfile::certs(&mut TLS_TEST_LEAF_CERT.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = rustls_pemfile::private_key(&mut TLS_TEST_LEAF_KEY.as_bytes())
        .unwrap()
        .unwrap();
    let config = Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(rustls::ALL_VERSIONS)
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap(),
    );
    let recorded = offers.clone();
    let server = serve(move |conn, _, _| {
        let config = config.clone();
        let offers = recorded.clone();
        async move {
            let lazy =
                tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), conn.io);
            let Ok(start) = lazy.await else {
                return;
            };
            let offered = start.client_hello().alpn().map(|names| {
                names
                    .map(|name| String::from_utf8_lossy(name).into_owned())
                    .collect()
            });
            offers.lock().unwrap().push(offered);
            let Ok(tls) = start.into_stream(config).await else {
                return;
            };
            let mut conn = Conn::new(tls);
            if conn.head().await.is_some() {
                let _ = conn
                    .io
                    .write_all(&response("200 OK", &[("connection", "close")], b"ok"))
                    .await;
            }
        }
    })
    .await;
    (server, offers)
}

/// What a route offers an https origin by ALPN (#176): `http/1.1` alone
/// until told -- node's fetch, whose undici connector offers that unless a
/// dispatcher sets `allowH2` -- `http/1.1, h2` with `Alpn::AllowH2`, in
/// undici's order, and no ALPN extension at all with `Alpn::None`, as
/// node's https.request offers none. The transport used to offer
/// `h2, http/1.1` to every origin, whichever client asked.
#[tokio::test(flavor = "multi_thread")]
async fn a_route_offers_the_alpn_list_it_was_given() {
    within(async {
        let (origin, offers) = alpn_recording_origin().await;
        let transport = transport(ProxySource::None);
        let target = format!("https://localhost:{}/", origin.port);
        for alpn in [
            None,
            Some(Alpn::Http1),
            Some(Alpn::AllowH2),
            Some(Alpn::None),
        ] {
            let route = transport.route(false, ATTEMPT, TlsRange::Both);
            let route = match alpn {
                Some(alpn) => route.with_alpn(alpn),
                None => route,
            };
            let response = send(&transport, &route, get(&target)).await.unwrap();
            assert_eq!(response.version(), http::Version::HTTP_11);
            assert_eq!(body_text(response).await, "ok");
        }
        let h1 = Some(vec!["http/1.1".to_string()]);
        assert_eq!(
            *offers.lock().unwrap(),
            vec![
                h1.clone(),
                h1,
                Some(vec!["http/1.1".to_string(), "h2".to_string()]),
                None,
            ]
        );
    })
    .await;
}

/// A pooled connection is reused only by a request that offers what it was
/// opened with: after an `allowH2` request leaves its h2 session to an
/// h2-only origin in the pool, a request offering `http/1.1` alone dials
/// its own connection -- which that origin refuses, as it refuses node's
/// fetch -- rather than riding the h2 session.
#[tokio::test(flavor = "multi_thread")]
async fn a_pooled_h2_session_is_not_reused_by_a_request_that_did_not_offer_h2() {
    within(async {
        let origin = serve_h2_tls("h2 ok").await;
        let transport = transport(ProxySource::None);
        let target = format!("https://localhost:{}/x", origin.port);
        let allow_h2 = transport
            .route(false, ATTEMPT, TlsRange::Both)
            .with_alpn(Alpn::AllowH2);
        let response = send(&transport, &allow_h2, get(&target)).await.unwrap();
        assert_eq!(response.version(), http::Version::HTTP_2);
        assert_eq!(body_text(response).await, "h2 ok");
        let plain = transport.route(false, ATTEMPT, TlsRange::Both);
        let err = send(&transport, &plain, get(&target)).await.unwrap_err();
        match err.to_outcome(&url(&target)) {
            OpOutcome::NodeFailed { code, .. } => {
                assert_eq!(code, "ERR_SSL_TLSV1_ALERT_NO_APPLICATION_PROTOCOL", "{err}");
            }
            other => panic!("{other:?} ({err})"),
        }
        assert_eq!(origin.accepts(), 2, "the h1-only request dialled its own");
        // The h2 session is still there for the next allowH2 request.
        let response = send(&transport, &allow_h2, get(&target)).await.unwrap();
        assert_eq!(response.version(), http::Version::HTTP_2);
        assert_eq!(origin.accepts(), 2);
    })
    .await;
}

/// The connection a followed redirect came on is not pooled when the next
/// hop keeps a method that is not idempotent (#155): such a hop -- a POST a
/// 307 or 308 sends on, a PATCH a 302 does, a POST a 302 does under
/// undici.request's rules -- is never written onto the connection a server
/// may be closing right behind its 3xx, as it could not be sent again. A hop
/// that may be resent keeps the pooled connection (no cost on that path),
/// and so does a 3xx nobody follows or one without a Location. The server
/// keeps every connection open, so whether the 3xx's connection was pooled
/// is whether the next request needed a second one.
#[tokio::test(flavor = "multi_thread")]
async fn a_redirect_whose_next_hop_cannot_be_resent_does_not_pool_its_connection() {
    use oam_core::http_client::redirect::Rules;
    within(async {
        let cases: [(&str, &str, bool, Option<Rules>, bool); 11] = [
            (
                "POST",
                "307 Temporary Redirect",
                true,
                Some(Rules::Fetch),
                false,
            ),
            (
                "POST",
                "308 Permanent Redirect",
                true,
                Some(Rules::Fetch),
                false,
            ),
            ("PATCH", "302 Found", true, Some(Rules::Fetch), false),
            ("POST", "302 Found", true, Some(Rules::Undici), false),
            // The hop is a GET.
            ("POST", "302 Found", true, Some(Rules::Fetch), true),
            ("POST", "303 See Other", true, Some(Rules::Undici), true),
            // Idempotent hops.
            (
                "GET",
                "307 Temporary Redirect",
                true,
                Some(Rules::Fetch),
                true,
            ),
            (
                "PUT",
                "308 Permanent Redirect",
                true,
                Some(Rules::Fetch),
                true,
            ),
            // Not followed: `redirect: 'manual'`, http.request.
            ("POST", "307 Temporary Redirect", true, None, true),
            // No Location: the 3xx is the response.
            (
                "POST",
                "307 Temporary Redirect",
                false,
                Some(Rules::Fetch),
                true,
            ),
            // Not a redirect.
            ("POST", "200 OK", true, Some(Rules::Fetch), true),
        ];
        for (method, status, location, follows, pooled) in cases {
            let server = serve_replies(move |_| {
                let headers: &[(&str, &str)] = if location {
                    &[("location", "/next")]
                } else {
                    &[]
                };
                response(status, headers, b"")
            })
            .await;
            let transport = transport(ProxySource::None);
            let route = transport.route(false, ATTEMPT, TlsRange::Both);
            let target = format!("http://127.0.0.1:{}/x", server.port);
            let first = transport
                .send_following(&route, request(method, &target, empty_body()), follows)
                .await
                .unwrap();
            body_text(first).await;
            let_the_pool_settle().await;
            let second = send(&transport, &route, get(&target)).await.unwrap();
            body_text(second).await;
            let label = format!("{method} answered {status} (location {location}, {follows:?})");
            assert_eq!(server.seen().len(), 2, "{label}");
            assert_eq!(
                server.accepts(),
                if pooled { 1 } else { 2 },
                "{label}: the next request {} the connection",
                if pooled { "reuses" } else { "does not reuse" }
            );
        }
        // And a 3xx -- any response -- that says `Connection: close` is the
        // last on its connection, whatever the hop's method (RFC 9112 s9.6).
        let server = serve_replies(|_| {
            response(
                "302 Found",
                &[("location", "/next"), ("connection", "close")],
                b"",
            )
        })
        .await;
        let transport = transport(ProxySource::None);
        let route = transport.route(false, ATTEMPT, TlsRange::Both);
        let target = format!("http://127.0.0.1:{}/x", server.port);
        for _ in 0..2 {
            let response = transport
                .send_following(&route, get(&target), Some(Rules::Fetch))
                .await
                .unwrap();
            body_text(response).await;
            let_the_pool_settle().await;
        }
        assert_eq!(
            server.accepts(),
            2,
            "a connection the server closes is not reused"
        );
    })
    .await;
}

/// `env_proxied` answers what the pooled route's rules would do with a URI:
/// http.request asks it to keep a request node would dial directly off the
/// proxy.
#[test]
fn env_proxied_reports_what_the_rules_intercept() {
    let rules = Matcher::builder()
        .http("http://127.0.0.1:9".to_string())
        .no("skip.test".to_string())
        .build();
    let t = transport(ProxySource::Fixed(Box::new(rules)));
    assert!(t.env_proxied("http://a.test/x"));
    assert!(!t.env_proxied("http://skip.test/x"));
    assert!(!t.env_proxied("https://a.test/x"), "no https rule");
    let none = transport(ProxySource::None);
    assert!(!none.env_proxied("http://a.test/x"));
    // Not a URI: the caller is told to keep off the proxy's transport.
    assert!(none.env_proxied("http://a b/"));
}
