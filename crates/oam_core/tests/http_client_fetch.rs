//! `http_client::send` and `http_client::body` over real loopback sockets:
//! the payload, undici's redirect rules on the wire, the per-hop proxy
//! credential, content decoding and its streaming bounds, cancellation, the
//! h2 retry, the outbound body channel lifecycle, and the `connect.lookup`
//! continuation (#143 slice C, design-143-C-pinhook).

mod common;

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::sync::atomic::{AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use common::*;
use hyper_util::client::proxy::matcher::Matcher;
use oam_core::http_client::body::{self, BODY_READ_FAILED, FetchBodies};
use oam_core::http_client::decode::OUT_CAP;
use oam_core::http_client::redirect::{BAD_SCHEME, CREDENTIALS, INVALID_URL};
use oam_core::http_client::send::{self, FetchContinuations, FetchRequest};
use oam_core::http_client::{HttpTransport, NetCheck, NetTarget, ProxySource};
use oam_core::{AccessDenial, BodyCancelSignal, CancelledBodies, OpOutcome, OutboundBodies};
use serde_json::{Value, json};

// ---------------------------------------------------------------- harness

/// The registries a CoreRuntime would own.
#[derive(Clone)]
struct Reg {
    bodies: FetchBodies,
    ids: Arc<AtomicU64>,
    outbound: OutboundBodies,
    continuations: FetchContinuations,
    cancelled: CancelledBodies,
    signal: BodyCancelSignal,
    /// The `--permission` net check every fetch gets (`None`: unrestricted).
    net_check: Option<NetCheck>,
}

impl Reg {
    fn new() -> Reg {
        Reg {
            bodies: Arc::new(Mutex::new(HashMap::new())),
            ids: Arc::new(AtomicU64::new(100)),
            outbound: Arc::new(Mutex::new(HashMap::new())),
            continuations: Arc::new(Mutex::new(HashMap::new())),
            cancelled: Arc::new(Mutex::new(HashSet::new())),
            signal: Arc::new(tokio::sync::Notify::new()),
            net_check: None,
        }
    }

    /// A registry whose fetches carry `check`.
    fn with_net_check(check: NetCheck) -> Reg {
        Reg {
            net_check: Some(check),
            ..Reg::new()
        }
    }

    async fn fetch(&self, transport: &HttpTransport, request: Value) -> OpOutcome {
        let request: FetchRequest = serde_json::from_value(request).unwrap();
        send::fetch(
            transport.clone(),
            request,
            self.bodies.clone(),
            self.ids.clone(),
            self.outbound.clone(),
            self.continuations.clone(),
            self.net_check.clone(),
        )
        .await
    }

    async fn resume(&self, token: u64, ips: &[&str]) -> OpOutcome {
        send::fetch_continue(
            token,
            json!({ "ips": ips }).to_string(),
            self.bodies.clone(),
            self.ids.clone(),
            self.continuations.clone(),
        )
        .await
    }

    async fn read(&self, handle: u64) -> OpOutcome {
        body::read(
            self.bodies.clone(),
            self.cancelled.clone(),
            self.signal.clone(),
            handle,
        )
        .await
    }

    /// Every chunk to the end, or the failure text.
    async fn chunks(&self, handle: u64) -> Result<Vec<Vec<u8>>, String> {
        let mut chunks = Vec::new();
        loop {
            match self.read(handle).await {
                OpOutcome::Bytes(chunk) => {
                    assert!(!chunk.is_empty(), "an empty chunk");
                    chunks.push(chunk);
                }
                OpOutcome::Done => return Ok(chunks),
                OpOutcome::Failed(text) => return Err(text),
                other => panic!("{other:?}"),
            }
        }
    }

    async fn text(&self, handle: u64) -> String {
        let chunks = self.chunks(handle).await.unwrap();
        String::from_utf8(chunks.concat()).unwrap()
    }

    /// A streamed-body channel, as `fetchBodyChannelNew` makes one.
    fn channel(
        &self,
        capacity: usize,
    ) -> (u64, tokio::sync::mpsc::Sender<Result<Vec<u8>, String>>) {
        let handle = self.ids.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::mpsc::channel(capacity);
        self.outbound
            .lock()
            .unwrap()
            .insert(handle, (Some(tx.clone()), Some(rx)));
        (handle, tx)
    }

    /// (sender present, receiver present) for an outbound entry.
    fn entry(&self, handle: u64) -> Option<(bool, bool)> {
        self.outbound
            .lock()
            .unwrap()
            .get(&handle)
            .map(|(tx, rx)| (tx.is_some(), rx.is_some()))
    }

    fn parked(&self) -> usize {
        self.continuations.lock().unwrap().len()
    }
}

fn payload(outcome: OpOutcome) -> Value {
    match outcome {
        OpOutcome::Json(text) => serde_json::from_str(&text).unwrap(),
        other => panic!("expected a payload, got {other:?}"),
    }
}

fn failed(outcome: OpOutcome) -> String {
    match outcome {
        OpOutcome::Failed(text) => text,
        other => panic!("expected a failure, got {other:?}"),
    }
}

fn handle_of(payload: &Value) -> u64 {
    payload["bodyHandle"].as_u64().unwrap()
}

/// The payload's headers as (name, value) pairs.
fn headers_of(payload: &Value) -> Vec<(String, String)> {
    payload["headers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pair| {
            (
                pair[0].as_str().unwrap().to_string(),
                pair[1].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// A lookup request: (token, host, port). The port is a string when the URL
/// names one and a number for the scheme default, as undici passes it.
fn lookup_of(outcome: OpOutcome) -> (u64, String, u16) {
    let value = payload(outcome);
    let lookup = &value["lookup"];
    assert!(lookup.is_object(), "expected a lookup request, got {value}");
    let port = match &lookup["port"] {
        Value::String(port) => port.parse().unwrap(),
        Value::Number(port) => {
            let port = port.as_u64().unwrap() as u16;
            assert!(
                port == 80 || port == 443,
                "a numeric port is a default: {value}"
            );
            port
        }
        other => panic!("port {other} in {value}"),
    };
    (
        lookup["token"].as_u64().unwrap(),
        lookup["host"].as_str().unwrap().to_string(),
        port,
    )
}

fn plain() -> HttpTransport {
    transport(ProxySource::None)
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn raw_deflate(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn br(data: &[u8]) -> Vec<u8> {
    let mut w = brotli::CompressorWriter::new(Vec::new(), 4096, 5, 22);
    w.write_all(data).unwrap();
    w.into_inner()
}

fn chunk(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:x}\r\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
    out
}

// ---------------------------------------------------------------- payload

#[tokio::test(flavor = "multi_thread")]
async fn payload_shape() {
    within(async {
        let server = serve_replies(|_| {
            b"HTTP/1.1 200 Custom Reason\r\nset-cookie: a=1\r\nset-cookie: b=2\r\ncontent-length: 2\r\n\r\nhi"
                .to_vec()
        })
        .await;
        let reg = Reg::new();
        let url = format!("http://u:p@127.0.0.1:{}/x?y#frag", server.port);
        let p = payload(reg.fetch(&plain(), json!({ "url": url })).await);
        assert_eq!(p["status"], 200);
        assert_eq!(p["statusText"], "OK");
        assert_eq!(
            p["url"],
            format!("http://127.0.0.1:{}/x?y", server.port).as_str()
        );
        assert_eq!(p["redirected"], false);
        let headers = headers_of(&p);
        let cookies: Vec<_> = headers
            .iter()
            .filter(|(n, _)| n == "set-cookie")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(cookies, ["a=1", "b=2"]);
        assert_eq!(header(&headers, "content-length"), Some("2"));
        assert_eq!(reg.text(handle_of(&p)).await, "hi");
        let seen = server.seen();
        assert_eq!(seen[0].head.target, "/x?y");
        assert_eq!(seen[0].head.get("authorization"), Some("Basic dTpw"));
        assert_eq!(seen[0].head.get("user-agent"), Some(USER_AGENT));
        assert_eq!(seen[0].head.get("accept"), Some("*/*"));
        assert_eq!(seen[0].head.get("accept-encoding"), Some("gzip,deflate"));
    })
    .await;
}

/// Header values reach JS as latin1, one code point per byte, as undici holds
/// them (`value.toString('latin1')`). Measured on node v22.22.2 against a raw
/// server sending these exact bytes: `x-latin1` reads `café` (code points 63
/// 61 66 e9), `content-disposition` keeps its 0xEF, and the `set-cookie`
/// bytes `e2 82 ac` read as THREE code points, not as the single `€` a UTF-8
/// decode produces. The lossy UTF-8 decode this replaces turned 0xE9 into
/// U+FFFD, destroying the byte with no way for the caller to recover it.
#[tokio::test(flavor = "multi_thread")]
async fn response_header_values_are_latin1_not_lossy_utf8() {
    within(async {
        let server = serve_replies(|_| {
            b"HTTP/1.1 200 OK\r\nx-latin1: caf\xe9-\xff\r\ncontent-disposition: attachment; filename=\"na\xefve.txt\"\r\nset-cookie: a=\xe2\x82\xac\r\ncontent-length: 2\r\n\r\nhi".to_vec()
        })
        .await;
        let reg = Reg::new();
        let url = format!("http://127.0.0.1:{}/", server.port);
        let p = payload(reg.fetch(&plain(), json!({ "url": url })).await);
        let headers = headers_of(&p);
        assert_eq!(header(&headers, "x-latin1"), Some("caf\u{e9}-\u{ff}"));
        assert_eq!(
            header(&headers, "content-disposition"),
            Some("attachment; filename=\"na\u{ef}ve.txt\"")
        );
        assert_eq!(header(&headers, "set-cookie"), Some("a=\u{e2}\u{82}\u{ac}"));
    })
    .await;
}

/// A request on a pooled connection the server closes without answering is
/// sent again on a fresh one, so long as it is idempotent. This is the shape
/// a server that hit its idle timeout leaves behind, and the one a redirect
/// hop meets: the 3xx's connection goes back into the pool a moment before
/// its FIN is processed, then the hop writes into a dead socket. Measured on
/// node v22.22.2: 20 of 20 such redirects succeed on all five server shapes,
/// where oam failed the whole fetch with `error sending request for url (...)`
/// on most iterations.
///
/// Deterministic here: connection 0 answers the first request (so it is
/// pooled, healthy) and closes on the second without a byte of response.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_on_a_closed_pooled_connection_is_sent_again() {
    within(async {
        // Every connection answers its first request and FINs on its second
        // without a byte of response.
        let server = serve(|mut conn, _, seen| async move {
            if let Some(request) = conn.request().await {
                seen.lock().unwrap().push(request);
                conn.send(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                    .await;
            }
            if let Some(request) = conn.request().await {
                seen.lock().unwrap().push(request);
            }
        })
        .await;
        let transport = plain();
        let reg = Reg::new();
        let url = format!("http://127.0.0.1:{}/x", server.port);
        // Warm the pool, then let hyper-util put the connection back into it.
        let p = payload(reg.fetch(&transport, json!({ "url": &url })).await);
        assert_eq!(reg.text(handle_of(&p)).await, "ok");
        let_the_pool_settle().await;
        assert_eq!(server.accepts(), 1);

        let p = payload(reg.fetch(&transport, json!({ "url": &url })).await);
        assert_eq!(p["status"], 200);
        assert_eq!(reg.text(handle_of(&p)).await, "ok");
        assert_eq!(server.accepts(), 2, "the retry dialled afresh");
        assert_eq!(
            server.seen().len(),
            3,
            "the dead connection read the first try"
        );

        // A POST is NOT replayed: oam DID put the request on the wire and
        // cannot know the server ignored it (RFC 9110 s9.2.2, idempotent
        // methods only).
        let_the_pool_settle().await;
        let text = failed(
            reg.fetch(&transport, json!({ "url": &url, "method": "POST" }))
                .await,
        );
        assert_eq!(text, format!("error sending request for url ({url})"));
        assert_eq!(server.accepts(), 2, "no fresh connection for a POST");
    })
    .await;
}

/// A server whose connections each answer their first request and close on
/// their second without a byte of response, and whose first `pooled`
/// connections hold that first answer until all of them are open -- so as
/// many concurrent fetches leave exactly that many connections in the pool.
async fn closes_on_second_request(pooled: usize) -> Server {
    let all_open = Arc::new(tokio::sync::Barrier::new(pooled));
    serve(move |mut conn, n, seen| {
        let all_open = all_open.clone();
        async move {
            if let Some(request) = conn.request().await {
                seen.lock().unwrap().push(request);
                if n < pooled {
                    all_open.wait().await;
                }
                conn.send(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                    .await;
            }
            if let Some(request) = conn.request().await {
                seen.lock().unwrap().push(request);
            }
        }
    })
    .await
}

/// `pooled` concurrent fetches of `url`, read to the end, then time for
/// hyper-util to pool every connection.
async fn fill_the_pool(reg: &Reg, transport: &HttpTransport, url: &str, pooled: usize) {
    let fetches = (0..pooled).map(|_| reg.fetch(transport, json!({ "url": url })));
    for outcome in futures_util::future::join_all(fetches).await {
        assert_eq!(reg.text(handle_of(&payload(outcome))).await, "ok");
    }
    let_the_pool_settle().await;
}

/// The stale-connection resend happens once per hop. When the resend meets
/// a second pooled connection the server is closing, the fetch fails -- the
/// request has then reached the server twice, and RFC 9110 s9.2.2 says a
/// client "SHOULD NOT automatically retry a failed automatic retry". node
/// never resends a written request at all: undici fails it with
/// UND_ERR_SOCKET and http.Agent with "socket hang up", and the server sees
/// it once. (An earlier cut resent up to three times, which delivered one
/// GET four times and reported success where node rejected.)
#[tokio::test(flavor = "multi_thread")]
async fn a_stale_resend_is_sent_once() {
    within(async {
        let server = closes_on_second_request(2).await;
        let transport = plain();
        let reg = Reg::new();
        let url = format!("http://127.0.0.1:{}/x", server.port);
        fill_the_pool(&reg, &transport, &url, 2).await;
        assert_eq!(server.accepts(), 2);

        let text = failed(reg.fetch(&transport, json!({ "url": &url })).await);
        assert_eq!(text, format!("error sending request for url ({url})"));
        assert_eq!(server.accepts(), 2, "no connection was dialled");
        assert_eq!(
            server.seen().len(),
            2 + 2,
            "the first try and one resend, each read by a dead connection"
        );
    })
    .await;
}

/// A pooled connection that closes halfway through a response head is not a
/// connection that ignored the request: the server had started answering.
/// hyper reports it with the same IncompleteMessage as a connection that
/// died before a byte arrived, so the send path counts the response bytes
/// itself, and does not resend (RFC 9110 s9.2.2's example of a retry worth
/// guessing at is a connection that "closed before any part of a response is
/// received"). node sends such a request once and rejects.
#[tokio::test(flavor = "multi_thread")]
async fn a_pooled_connection_that_closes_mid_head_is_not_resent() {
    within(async {
        let server = serve(|mut conn, _, seen| async move {
            if let Some(request) = conn.request().await {
                seen.lock().unwrap().push(request);
                conn.send(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                    .await;
            }
            if let Some(request) = conn.request().await {
                seen.lock().unwrap().push(request);
                conn.send(b"HTTP/1.1 200 OK\r\ncontent-le").await;
            }
        })
        .await;
        let transport = plain();
        let reg = Reg::new();
        let url = format!("http://127.0.0.1:{}/x", server.port);
        let p = payload(reg.fetch(&transport, json!({ "url": &url })).await);
        assert_eq!(reg.text(handle_of(&p)).await, "ok");
        let_the_pool_settle().await;
        assert_eq!(server.accepts(), 1);

        let text = failed(reg.fetch(&transport, json!({ "url": &url })).await);
        assert_eq!(text, format!("error sending request for url ({url})"));
        assert_eq!(server.accepts(), 1, "no resend dialled afresh");
        assert_eq!(server.seen().len(), 2, "the GET reached the server once");
    })
    .await;
}

/// A blocking HTTP/1 request read: the head and a content-length body.
fn read_request_blocking(stream: &mut std::net::TcpStream) -> Option<String> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).into_owned();
            let len = head
                .lines()
                .find_map(|l| {
                    let (name, value) = l.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            if buf.len() >= end + 4 + len {
                return Some(head);
            }
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// A pooled connection whose server has closed it, with the FIN already in
/// the kernel but not yet seen by tokio's reactor, is not written to: the
/// connection reads the EOF before it takes the request, hands the request
/// back unsent, and hyper-util sends it on a fresh connection. So a POST,
/// which is never resent, succeeds with one delivery, as node's does (node's
/// event loop reads the FIN before it writes).
///
/// Deterministic: the client runs on a current-thread runtime and blocks
/// that thread while the server closes, so the reactor has not been polled
/// since the FIN arrived when the POST goes out. Before `EagerTcp` the read
/// came back `Pending` from tokio's readiness cache, the POST was written
/// into the dead connection, and the fetch failed with `error sending
/// request for url (...)`.
#[test]
fn a_pooled_connection_whose_fin_has_arrived_is_not_written_to() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (close_tx, close_rx) = std::sync::mpsc::channel::<()>();
    let (closed_tx, closed_rx) = std::sync::mpsc::channel::<()>();
    let seen = Arc::new(Mutex::new(Vec::<(usize, String)>::new()));
    let server = {
        let seen = seen.clone();
        std::thread::spawn(move || {
            use std::io::Write as _;
            let answer = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
            // Connection 0: answer one request (so it is pooled), then close
            // on the test's signal.
            let (mut first, _) = listener.accept().unwrap();
            if let Some(head) = read_request_blocking(&mut first) {
                seen.lock().unwrap().push((0, head));
                first.write_all(answer).unwrap();
            }
            close_rx.recv().unwrap();
            drop(first);
            closed_tx.send(()).unwrap();
            // Connection 1: whatever comes next, answered, until EOF.
            let (mut second, _) = listener.accept().unwrap();
            while let Some(head) = read_request_blocking(&mut second) {
                seen.lock().unwrap().push((1, head));
                second.write_all(answer).unwrap();
            }
        })
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .event_interval(1024)
        .build()
        .unwrap();
    runtime.block_on(within(async {
        let transport = plain();
        let reg = Reg::new();
        let url = format!("http://127.0.0.1:{port}/p");
        let post = json!({ "url": &url, "method": "POST", "body": "x" });
        let p = payload(reg.fetch(&transport, post.clone()).await);
        assert_eq!(reg.text(handle_of(&p)).await, "ok");
        let_the_pool_settle().await;

        // The server closes; this thread blocks, so nothing polls the
        // reactor until the POST below has been handed to the connection.
        close_tx.send(()).unwrap();
        closed_rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let outcome = reg.fetch(&transport, post).await;
        let p = payload(outcome);
        assert_eq!(p["status"], 200);
        assert_eq!(reg.text(handle_of(&p)).await, "ok");
    }));
    // Closes connection 1, which ends the server thread.
    drop(runtime);
    server.join().unwrap();
    let conns: Vec<usize> = seen.lock().unwrap().iter().map(|(c, _)| *c).collect();
    assert_eq!(
        conns,
        [0, 1],
        "one POST per connection: the closed one never read the second"
    );
}

// ---------------------------------------------------------------- redirects

#[tokio::test(flavor = "multi_thread")]
async fn redirect_limit_is_twenty() {
    within(async {
        let server = serve_replies(|r| {
            let n: u32 = r
                .head
                .target
                .trim_start_matches("/loop?n=")
                .parse()
                .unwrap_or(0);
            let next = format!("/loop?n={}", n + 1);
            response("302 Found", &[("location", &next)], b"")
        })
        .await;
        let reg = Reg::new();
        let url = format!("http://127.0.0.1:{}/loop?n=0", server.port);
        let text = failed(reg.fetch(&plain(), json!({ "url": url })).await);
        assert_eq!(text, "redirect count exceeded");
        assert_eq!(server.seen().len(), 21);
    })
    .await;
}

/// A -> B (cross-origin) -> A: the credentials and a user Host are stripped
/// at the cross-origin hop and stay stripped on the way back; cookie2 and
/// www-authenticate are forwarded; no Referer anywhere.
#[tokio::test(flavor = "multi_thread")]
async fn cross_origin_strip_is_permanent_and_no_referer() {
    within(async {
        let a_port = Arc::new(AtomicU16::new(0));
        let b = {
            let a_port = a_port.clone();
            serve_replies(move |_| {
                let back = format!("http://127.0.0.1:{}/end", a_port.load(Ordering::SeqCst));
                response("302 Found", &[("location", &back)], b"")
            })
            .await
        };
        let b_port = b.port;
        let a = serve_replies(move |r| {
            if r.head.target == "/start" {
                let away = format!("http://127.0.0.1:{b_port}/middle");
                response("301 Moved Permanently", &[("location", &away)], b"")
            } else {
                response("200 OK", &[], b"end")
            }
        })
        .await;
        a_port.store(a.port, Ordering::SeqCst);
        let reg = Reg::new();
        let p = payload(
            reg.fetch(
                &plain(),
                json!({
                    "url": format!("http://127.0.0.1:{}/start", a.port),
                    "headers": [
                        ["authorization", "Bearer x"],
                        ["cookie", "c=1"],
                        ["cookie2", "c2"],
                        ["www-authenticate", "w"],
                        ["proxy-authorization", "p"],
                        ["host", "custom.test"],
                    ],
                }),
            )
            .await,
        );
        assert_eq!(p["status"], 200);
        assert_eq!(p["redirected"], true);
        assert_eq!(
            p["url"],
            format!("http://127.0.0.1:{}/end", a.port).as_str()
        );
        assert_eq!(reg.text(handle_of(&p)).await, "end");

        let a_seen = a.seen();
        let b_seen = b.seen();
        let first = &a_seen[0].head;
        assert_eq!(first.get("authorization"), Some("Bearer x"));
        assert_eq!(first.get("cookie"), Some("c=1"));
        assert_eq!(first.get("proxy-authorization"), Some("p"));
        assert_eq!(first.get("host"), Some("custom.test"));
        let middle = &b_seen[0].head;
        let last = &a_seen[1].head;
        for (head, port) in [(middle, b.port), (last, a.port)] {
            assert!(!head.has("authorization"), "{head:?}");
            assert!(!head.has("cookie"), "{head:?}");
            assert!(!head.has("proxy-authorization"), "{head:?}");
            assert_eq!(head.get("host"), Some(format!("127.0.0.1:{port}").as_str()));
        }
        for head in [first, middle, last] {
            assert_eq!(head.get("cookie2"), Some("c2"));
            assert_eq!(head.get("www-authenticate"), Some("w"));
            assert!(!head.has("referer"), "{head:?}");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn method_rewrite_matrix_on_the_wire() {
    within(async {
        let server = serve_replies(|r| match r.head.target.as_str() {
            "/302" => response("302 Found", &[("location", "/after")], b""),
            "/303" => response("303 See Other", &[("location", "/after")], b""),
            "/307" => response("307 Temporary Redirect", &[("location", "/after")], b""),
            _ => response("200 OK", &[], b"ok"),
        })
        .await;
        let reg = Reg::new();
        let base = format!("http://127.0.0.1:{}", server.port);
        let t = plain();
        let content_type = json!([["content-type", "text/plain"]]);
        for (method, status, body) in [
            ("POST", "302", true),
            ("GET", "303", false),
            ("POST", "307", true),
        ] {
            let mut request = json!({
                "url": format!("{base}/{status}"),
                "method": method,
                "headers": content_type,
            });
            if body {
                request["body"] = json!("data");
            }
            let p = payload(reg.fetch(&t, request).await);
            assert_eq!(p["status"], 200);
            reg.text(handle_of(&p)).await;
        }
        let seen = server.seen();
        assert_eq!(seen.len(), 6);
        // POST 302 -> GET, no body, no body headers.
        let after_302 = &seen[1];
        assert_eq!(after_302.head.method, "GET");
        assert!(after_302.body.is_empty());
        assert!(!after_302.head.has("content-type"));
        assert!(!after_302.head.has("content-length"));
        // GET 303 keeps its method and content-type.
        let after_303 = &seen[3];
        assert_eq!(after_303.head.method, "GET");
        assert_eq!(after_303.head.get("content-type"), Some("text/plain"));
        // POST 307 keeps method and body.
        let after_307 = &seen[5];
        assert_eq!(after_307.head.method, "POST");
        assert_eq!(after_307.body, b"data");
        assert_eq!(after_307.head.get("content-length"), Some("4"));
        assert_eq!(after_307.head.get("content-type"), Some("text/plain"));
    })
    .await;
}

/// A streamed body cannot be replayed: a 307 is returned as the response; a
/// 302 turns the POST into a body-less GET and is followed.
#[tokio::test(flavor = "multi_thread")]
async fn streamed_body_redirects() {
    within(async {
        let server = serve_replies(|r| match r.head.target.as_str() {
            "/307" => response("307 Temporary Redirect", &[("location", "/after")], b""),
            "/302" => response("302 Found", &[("location", "/after")], b""),
            _ => response("200 OK", &[], b"ok"),
        })
        .await;
        let reg = Reg::new();
        let t = plain();
        for (status, want_status, want_redirected) in [("307", 307, false), ("302", 200, true)] {
            let (handle, tx) = reg.channel(8);
            let outbound = reg.outbound.clone();
            let writer = tokio::spawn(async move {
                tx.send(Ok(b"abc".to_vec())).await.unwrap();
                drop(tx);
                body::end_outbound(&outbound, handle);
            });
            let p = payload(
                reg.fetch(
                    &t,
                    json!({
                        "url": format!("http://127.0.0.1:{}/{status}", server.port),
                        "method": "POST",
                        "body_stream": handle,
                    }),
                )
                .await,
            );
            writer.await.unwrap();
            assert_eq!(p["status"], want_status);
            assert_eq!(p["redirected"], want_redirected);
            reg.text(handle_of(&p)).await;
            assert_eq!(reg.entry(handle), None, "entry left behind");
        }
        let seen = server.seen();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].head.get("transfer-encoding"), Some("chunked"));
        assert_eq!(seen[0].body, b"abc");
        assert_eq!(seen[2].head.method, "GET");
        assert!(seen[2].body.is_empty());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn redirect_to_bad_port_is_not_sent() {
    within(async {
        let server =
            serve_replies(|_| response("302 Found", &[("location", "http://127.0.0.1:25/")], b""))
                .await;
        let reg = Reg::new();
        let url = format!("http://127.0.0.1:{}/", server.port);
        assert_eq!(
            failed(reg.fetch(&plain(), json!({ "url": url })).await),
            "bad port"
        );
        assert_eq!(server.seen().len(), 1);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn initial_bad_port_only_under_fetch_semantics() {
    within(async {
        let reg = Reg::new();
        let t = plain();
        let url = "http://127.0.0.1:1/";
        let text = failed(
            reg.fetch(&t, json!({ "url": url, "fetch_semantics": true }))
                .await,
        );
        assert_eq!(text, "bad port");
        match reg.fetch(&t, json!({ "url": url })).await {
            OpOutcome::NodeFailed { code, port, .. } => {
                assert_eq!(code, "ECONNREFUSED");
                assert_eq!(port, Some(1));
            }
            other => panic!("{other:?}"),
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn redirect_manual_returns_the_3xx() {
    within(async {
        let server =
            serve_replies(|_| response("302 Found", &[("location", "/elsewhere")], b"moved")).await;
        let reg = Reg::new();
        let p = payload(
            reg.fetch(
                &plain(),
                json!({
                    "url": format!("http://127.0.0.1:{}/", server.port),
                    "redirect": "manual",
                }),
            )
            .await,
        );
        assert_eq!(p["status"], 302);
        assert_eq!(p["redirected"], false);
        assert_eq!(header(&headers_of(&p), "location"), Some("/elsewhere"));
        assert_eq!(reg.text(handle_of(&p)).await, "moved");
        assert_eq!(server.seen().len(), 1);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_and_non_http_locations() {
    within(async {
        let port = Arc::new(AtomicU16::new(0));
        let server = {
            let port = port.clone();
            serve_replies(move |r| {
                let location = match r.head.target.as_str() {
                    "/invalid" => "http://[::1".to_string(),
                    "/ftp" => "ftp://example.test/x".to_string(),
                    _ => format!("http://u:p@127.0.0.1:{}/", port.load(Ordering::SeqCst)),
                };
                response("302 Found", &[("location", &location)], b"")
            })
            .await
        };
        port.store(server.port, Ordering::SeqCst);
        let reg = Reg::new();
        let t = plain();
        for (path, want) in [
            ("invalid", INVALID_URL),
            ("ftp", BAD_SCHEME),
            ("credentials", CREDENTIALS),
        ] {
            let url = format!("http://127.0.0.1:{}/{path}", server.port);
            assert_eq!(failed(reg.fetch(&t, json!({ "url": url })).await), want);
        }
        assert_eq!(server.seen().len(), 3);
    })
    .await;
}

/// proxy-authorization is recomputed for every proxied http hop -- after a
/// cross-origin strip too -- and a caller's own value wins on its hop.
#[tokio::test(flavor = "multi_thread")]
async fn proxy_authorization_recomputed_per_hop() {
    within(async {
        let proxy = serve_replies(|r| {
            if r.head.target.contains("/start") {
                response("302 Found", &[("location", "http://b.test/next")], b"")
            } else {
                response("200 OK", &[], b"ok")
            }
        })
        .await;
        let rules = Matcher::builder()
            .http(format!("http://u:p@127.0.0.1:{}", proxy.port))
            .build();
        let t = transport(ProxySource::Fixed(Box::new(rules)));
        let reg = Reg::new();
        for user in [None, Some("Custom")] {
            let mut request = json!({ "url": "http://a.test/start" });
            if let Some(user) = user {
                request["headers"] = json!([["proxy-authorization", user]]);
            }
            let p = payload(reg.fetch(&t, request).await);
            assert_eq!(p["status"], 200);
            reg.text(handle_of(&p)).await;
        }
        let seen = proxy.seen();
        let got: Vec<_> = seen
            .iter()
            .map(|r| (r.head.target.as_str(), r.head.all("proxy-authorization")))
            .collect();
        assert_eq!(
            got,
            [
                ("http://a.test/start", vec!["Basic dTpw"]),
                ("http://b.test/next", vec!["Basic dTpw"]),
                ("http://a.test/start", vec!["Custom"]),
                ("http://b.test/next", vec!["Basic dTpw"]),
            ]
        );
    })
    .await;
}

// ---------------------------------------------------------------- decoding

#[tokio::test(flavor = "multi_thread")]
async fn decoding_on_the_wire() {
    within(async {
        let text = b"hello world, hello world, hello world";
        let multi = [gzip(b"hello "), gzip(b"world")].concat();
        struct Case {
            name: &'static str,
            encodings: Vec<&'static str>,
            sent: Vec<u8>,
            want: Vec<u8>,
        }
        let case = |name, encodings: &[&'static str], sent: Vec<u8>, want: &[u8]| Case {
            name,
            encodings: encodings.to_vec(),
            sent,
            want: want.to_vec(),
        };
        let cases = vec![
            case("gzip-multi", &["gzip"], multi, b"hello world"),
            case("x-gzip", &["x-gzip"], gzip(text), text),
            case("GZIP", &["GZIP"], gzip(text), text),
            case("raw-deflate", &["deflate"], raw_deflate(text), text),
            case("zlib", &["deflate"], zlib(text), text),
            case("br", &["br"], br(text), text),
            case("stacked", &["gzip, br"], br(&gzip(text)), text),
            case("two-lines", &["gzip", "br"], br(&gzip(text)), text),
            // Undecoded: an empty coding or `identity` turns decoding off.
            case("gzip-comma", &["gzip,"], gzip(text), &gzip(text)),
            case(
                "gzip-identity",
                &["gzip, identity"],
                gzip(text),
                &gzip(text),
            ),
        ];
        let routes: HashMap<String, Vec<u8>> = cases
            .iter()
            .map(|c| {
                let headers: Vec<_> = c
                    .encodings
                    .iter()
                    .map(|e| ("content-encoding", *e))
                    .collect();
                (
                    format!("/{}", c.name),
                    response("200 OK", &headers, &c.sent),
                )
            })
            .collect();
        let server = serve_replies(move |r| routes[&r.head.target].clone()).await;
        let reg = Reg::new();
        let t = plain();
        for Case {
            name, sent, want, ..
        } in cases
        {
            let decoded = sent != want;
            let url = format!("http://127.0.0.1:{}/{name}", server.port);
            let p = payload(reg.fetch(&t, json!({ "url": url })).await);
            let headers = headers_of(&p);
            let got = reg.chunks(handle_of(&p)).await.unwrap().concat();
            assert_eq!(got, want, "{name}");
            if decoded {
                assert_eq!(header(&headers, "content-encoding"), None, "{name}");
                assert_eq!(header(&headers, "content-length"), None, "{name}");
            } else {
                assert!(header(&headers, "content-encoding").is_some(), "{name}");
                assert_eq!(
                    header(&headers, "content-length"),
                    Some(sent.len().to_string().as_str()),
                    "{name}"
                );
            }
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn too_many_codings_fails_at_headers() {
    within(async {
        let server = serve_replies(|_| {
            response(
                "200 OK",
                &[("content-encoding", "gzip, gzip, gzip, gzip, gzip, gzip")],
                b"x",
            )
        })
        .await;
        let reg = Reg::new();
        let url = format!("http://127.0.0.1:{}/", server.port);
        assert_eq!(
            failed(reg.fetch(&plain(), json!({ "url": url })).await),
            "too many content-encodings in response: 6, maximum allowed is 5"
        );
        assert!(reg.bodies.lock().unwrap().is_empty());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn exempt_responses_keep_encoding_headers() {
    within(async {
        let compressed = gzip(b"payload");
        let length = compressed.len().to_string();
        let server =
            serve_replies(
                move |r| {
                    match (r.head.method.as_str(), r.head.target.as_str()) {
            ("HEAD", _) => format!(
                "HTTP/1.1 200 OK\r\ncontent-encoding: gzip\r\ncontent-length: {length}\r\n\r\n"
            )
            .into_bytes(),
            (_, "/204") => b"HTTP/1.1 204 No Content\r\ncontent-encoding: gzip\r\n\r\n".to_vec(),
            _ => response("200 OK", &[("content-encoding", "gzip")], &compressed),
        }
                },
            )
            .await;
        let reg = Reg::new();
        let t = plain();
        let url = format!("http://127.0.0.1:{}/gz", server.port);
        let sent_len = gzip(b"payload").len().to_string();

        let head = payload(reg.fetch(&t, json!({ "url": url, "method": "HEAD" })).await);
        let headers = headers_of(&head);
        assert_eq!(header(&headers, "content-encoding"), Some("gzip"));
        assert_eq!(header(&headers, "content-length"), Some(sent_len.as_str()));
        assert_eq!(reg.text(handle_of(&head)).await, "");

        let url_204 = format!("http://127.0.0.1:{}/204", server.port);
        let no_content = payload(reg.fetch(&t, json!({ "url": url_204 })).await);
        assert_eq!(
            header(&headers_of(&no_content), "content-encoding"),
            Some("gzip")
        );

        let decoded = payload(reg.fetch(&t, json!({ "url": url })).await);
        let headers = headers_of(&decoded);
        assert_eq!(header(&headers, "content-encoding"), None);
        assert_eq!(header(&headers, "content-length"), None);
        assert_eq!(reg.text(handle_of(&decoded)).await, "payload");

        let raw = payload(reg.fetch(&t, json!({ "url": url, "decode": false })).await);
        let headers = headers_of(&raw);
        assert_eq!(header(&headers, "content-encoding"), Some("gzip"));
        assert_eq!(header(&headers, "content-length"), Some(sent_len.as_str()));
        assert_eq!(
            reg.chunks(handle_of(&raw)).await.unwrap().concat(),
            gzip(b"payload")
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_gzip_200_and_202_zero_length_are_empty_bodies() {
    within(async {
        let server = serve_replies(|r| {
            let status = if r.head.target == "/202" {
                "202 Accepted"
            } else {
                "200 OK"
            };
            response(
                status,
                &[("content-encoding", "gzip"), ("content-length", "0")],
                b"",
            )
        })
        .await;
        let reg = Reg::new();
        let t = plain();
        for path in ["200", "202"] {
            let url = format!("http://127.0.0.1:{}/{path}", server.port);
            let p = payload(reg.fetch(&t, json!({ "url": url })).await);
            assert_eq!(reg.chunks(handle_of(&p)).await, Ok(vec![]), "{path}");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn truncated_gzip_is_partial_data_not_an_error() {
    within(async {
        let whole = gzip(b"hello world");
        let truncated = whole[..whole.len() - 8].to_vec();
        let server =
            serve_replies(move |_| response("200 OK", &[("content-encoding", "gzip")], &truncated))
                .await;
        let reg = Reg::new();
        let url = format!("http://127.0.0.1:{}/", server.port);
        let p = payload(reg.fetch(&plain(), json!({ "url": url })).await);
        assert_eq!(reg.text(handle_of(&p)).await, "hello world");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn gzip_then_junk_is_body_read_failed() {
    within(async {
        let body = [gzip(b"hi"), b"junk!".to_vec()].concat();
        let server =
            serve_replies(move |_| response("200 OK", &[("content-encoding", "gzip")], &body))
                .await;
        let reg = Reg::new();
        let url = format!("http://127.0.0.1:{}/", server.port);
        let p = payload(reg.fetch(&plain(), json!({ "url": url })).await);
        assert_eq!(
            reg.chunks(handle_of(&p)).await,
            Err(BODY_READ_FAILED.to_string())
        );
    })
    .await;
}

/// gzip followed by a zero byte ends the body there (node), even though the
/// server promised more and holds the connection open.
#[tokio::test(flavor = "multi_thread")]
async fn decoder_done_ends_the_body_before_the_wire() {
    within(async {
        let server = serve(|mut conn, _, _| async move {
            if conn.request().await.is_none() {
                return;
            }
            let body = [gzip(b"abc"), vec![0]].concat();
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-encoding: gzip\r\ncontent-length: {}\r\n\r\n",
                body.len() + 100
            );
            conn.send(&[head.into_bytes(), body].concat()).await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        })
        .await;
        let reg = Reg::new();
        let url = format!("http://127.0.0.1:{}/", server.port);
        let p = payload(reg.fetch(&plain(), json!({ "url": url })).await);
        let text = tokio::time::timeout(Duration::from_secs(2), reg.text(handle_of(&p)))
            .await
            .expect("the body did not end at the end of the gzip stream");
        assert_eq!(text, "abc");
    })
    .await;
}

/// A sync-flushed gzip unit is delivered as soon as it arrives, not when
/// the next one does (an SSE stream through a compressing proxy).
#[tokio::test(flavor = "multi_thread")]
async fn sync_flushed_gzip_units_arrive_one_by_one() {
    within(async {
        let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
        let go_rx = Mutex::new(Some(go_rx));
        let server = serve(move |mut conn, _, _| {
            let go = go_rx.lock().unwrap().take();
            async move {
                let Some(go) = go else { return };
                if conn.request().await.is_none() {
                    return;
                }
                let mut encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                conn.send(
                    b"HTTP/1.1 200 OK\r\ncontent-encoding: gzip\r\ntransfer-encoding: chunked\r\n\r\n",
                )
                .await;
                encoder.write_all(b"data: tok1\n\n").unwrap();
                encoder.flush().unwrap();
                let unit = std::mem::take(encoder.get_mut());
                conn.send(&chunk(&unit)).await;
                if go.await.is_err() {
                    return;
                }
                encoder.write_all(b"data: tok2\n\n").unwrap();
                encoder.flush().unwrap();
                let unit = std::mem::take(encoder.get_mut());
                conn.send(&chunk(&unit)).await;
                let rest = encoder.finish().unwrap();
                conn.send(&[chunk(&rest), b"0\r\n\r\n".to_vec()].concat()).await;
            }
        })
        .await;
        let reg = Reg::new();
        let url = format!("http://127.0.0.1:{}/", server.port);
        let p = payload(reg.fetch(&plain(), json!({ "url": url })).await);
        let handle = handle_of(&p);
        let first = tokio::time::timeout(Duration::from_secs(5), reg.read(handle))
            .await
            .expect("the first unit waited for the second");
        assert!(matches!(&first, OpOutcome::Bytes(b) if b == b"data: tok1\n\n"), "{first:?}");
        go_tx.send(()).unwrap();
        let second = reg.read(handle).await;
        assert!(matches!(&second, OpOutcome::Bytes(b) if b == b"data: tok2\n\n"), "{second:?}");
        assert!(matches!(reg.read(handle).await, OpOutcome::Done));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn bombs_never_exceed_out_cap() {
    within(async {
        const SIZE: usize = 64 * 1024 * 1024;
        let zeros = vec![0u8; SIZE];
        let gz = gzip(&zeros);
        let brotli = br(&zeros);
        drop(zeros);
        let server = serve_replies(move |r| {
            if r.head.target == "/br" {
                response("200 OK", &[("content-encoding", "br")], &brotli)
            } else {
                response("200 OK", &[("content-encoding", "gzip")], &gz)
            }
        })
        .await;
        let reg = Reg::new();
        let t = plain();
        for path in ["gz", "br"] {
            let url = format!("http://127.0.0.1:{}/{path}", server.port);
            let p = payload(reg.fetch(&t, json!({ "url": url })).await);
            let handle = handle_of(&p);
            let mut total = 0usize;
            loop {
                match reg.read(handle).await {
                    OpOutcome::Bytes(chunk) => {
                        assert!(chunk.len() <= OUT_CAP, "{path}: chunk of {}", chunk.len());
                        assert!(chunk.iter().all(|&b| b == 0));
                        total += chunk.len();
                    }
                    OpOutcome::Done => break,
                    other => panic!("{path}: {other:?}"),
                }
            }
            assert_eq!(total, SIZE, "{path}");
        }
    })
    .await;
}

// ---------------------------------------------------------------- cancel

/// A server that sends the response head, then the body only when told.
async fn gated_server(
    body: Vec<u8>,
    encoding: Option<&'static str>,
) -> (Server, tokio::sync::mpsc::UnboundedSender<()>) {
    let (go_tx, go_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let go_rx = Arc::new(tokio::sync::Mutex::new(go_rx));
    let server = serve(move |mut conn, _, _| {
        let go_rx = go_rx.clone();
        let body = body.clone();
        async move {
            if conn.request().await.is_none() {
                return;
            }
            let mut head = "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n".to_string();
            if let Some(encoding) = encoding {
                head.push_str(&format!("content-encoding: {encoding}\r\n"));
            }
            head.push_str("\r\n");
            conn.send(head.as_bytes()).await;
            if go_rx.lock().await.recv().await.is_none() {
                return;
            }
            conn.send(&chunk(&body)).await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    })
    .await;
    (server, go_tx)
}

fn sync_flushed_gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).unwrap();
    encoder.flush().unwrap();
    std::mem::take(encoder.get_mut())
}

/// Wait until a read has taken `handle` out of the registry (it is parked).
async fn parked_read(reg: &Reg, handle: u64) {
    for _ in 0..500 {
        if !reg.bodies.lock().unwrap().contains_key(&handle) {
            // Give the read a moment to reach its await.
            tokio::time::sleep(Duration::from_millis(50)).await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the read never started");
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_for_another_handle_loses_nothing() {
    within(async {
        for (encoding, sent) in [
            (None, b"payload".to_vec()),
            (Some("gzip"), sync_flushed_gzip(b"payload")),
        ] {
            let (server, go) = gated_server(sent, encoding).await;
            let reg = Reg::new();
            let url = format!("http://127.0.0.1:{}/", server.port);
            let p = payload(reg.fetch(&plain(), json!({ "url": url })).await);
            let handle = handle_of(&p);
            let read = tokio::spawn({
                let reg = reg.clone();
                async move { reg.read(handle).await }
            });
            parked_read(&reg, handle).await;
            let other = handle + 1000;
            reg.cancelled.lock().unwrap().insert(other);
            // Several wakes, so at least one lands while the read waits.
            for _ in 0..5 {
                reg.signal.notify_waiters();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(!read.is_finished());
            go.send(()).unwrap();
            let outcome = read.await.unwrap();
            assert!(
                matches!(&outcome, OpOutcome::Bytes(b) if b == b"payload"),
                "{encoding:?}: {outcome:?}"
            );
            assert!(reg.bodies.lock().unwrap().contains_key(&handle));
            assert!(reg.cancelled.lock().unwrap().contains(&other));
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_tombstone_mid_read_is_done_and_not_reinserted() {
    within(async {
        let (server, _go) = gated_server(b"never".to_vec(), None).await;
        let reg = Reg::new();
        let url = format!("http://127.0.0.1:{}/", server.port);
        let p = payload(reg.fetch(&plain(), json!({ "url": url })).await);
        let handle = handle_of(&p);
        let read = tokio::spawn({
            let reg = reg.clone();
            async move { reg.read(handle).await }
        });
        parked_read(&reg, handle).await;
        reg.cancelled.lock().unwrap().insert(handle);
        // `notify_waiters` wakes only a read already waiting: repeat it
        // until the read has seen one (the tombstone stays until then).
        for _ in 0..250 {
            reg.signal.notify_waiters();
            if read.is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(matches!(read.await.unwrap(), OpOutcome::Done));
        assert!(!reg.bodies.lock().unwrap().contains_key(&handle));
        assert!(reg.cancelled.lock().unwrap().is_empty());
    })
    .await;
}

// ---------------------------------------------------------------- h2 retry

/// A TLS h2 origin (ALPN h2) whose first stream is refused with
/// REFUSED_STREAM; later streams get `200 ok` after their body is read.
async fn refusing_h2_origin() -> (Server, Arc<AtomicUsize>) {
    let streams = Arc::new(AtomicUsize::new(0));
    let acceptor = tls_acceptor(&[b"h2"]);
    let server = {
        let streams = streams.clone();
        serve(move |conn, _, _| {
            let acceptor = acceptor.clone();
            let streams = streams.clone();
            async move {
                let Ok(tls) = acceptor.accept(conn.io).await else {
                    return;
                };
                let Ok(mut h2) = h2::server::handshake(tls).await else {
                    return;
                };
                while let Some(Ok((request, mut respond))) = h2.accept().await {
                    if streams.fetch_add(1, Ordering::SeqCst) == 0 {
                        respond.send_reset(h2::Reason::REFUSED_STREAM);
                        continue;
                    }
                    tokio::spawn(async move {
                        let mut body = request.into_body();
                        while let Some(Ok(data)) = body.data().await {
                            let _ = body.flow_control().release_capacity(data.len());
                        }
                        let response = http::Response::new(());
                        if let Ok(mut send) = respond.send_response(response, false) {
                            let _ = send.send_data(Bytes::from_static(b"ok"), true);
                        }
                    });
                }
            }
        })
        .await
    };
    (server, streams)
}

#[tokio::test(flavor = "multi_thread")]
async fn h2_refused_stream_retry() {
    within(async {
        let reg = Reg::new();
        let t = plain();

        let (server, streams) = refusing_h2_origin().await;
        let url = format!("https://127.0.0.1:{}/", server.port);
        let p = payload(
            reg.fetch(&t, json!({ "url": url, "method": "POST", "body": "abc" }))
                .await,
        );
        assert_eq!(p["status"], 200);
        assert_eq!(reg.text(handle_of(&p)).await, "ok");
        assert_eq!(streams.load(Ordering::SeqCst), 2);

        let (server, streams) = refusing_h2_origin().await;
        let url = format!("https://127.0.0.1:{}/", server.port);
        let (handle, tx) = reg.channel(8);
        tx.send(Ok(b"abc".to_vec())).await.unwrap();
        drop(tx);
        body::end_outbound(&reg.outbound, handle);
        let text = failed(
            reg.fetch(
                &t,
                json!({ "url": url, "method": "POST", "body_stream": handle }),
            )
            .await,
        );
        assert_eq!(text, format!("error sending request for url ({url})"));
        assert_eq!(streams.load(Ordering::SeqCst), 1);
        assert_eq!(reg.entry(handle), None);
    })
    .await;
}

// ---------------------------------------------------------------- outbound bodies

#[tokio::test(flavor = "multi_thread")]
async fn outbound_entry_lifecycle() {
    within(async {
        let server = serve_replies(|_| response("200 OK", &[], b"ok")).await;
        let reg = Reg::new();
        let t = plain();
        let url = format!("http://127.0.0.1:{}/", server.port);

        // A failure before the take drops the receiver: a write blocked on a
        // full channel resolves.
        let (handle, tx) = reg.channel(1);
        tx.try_send(Ok(b"fill".to_vec())).unwrap();
        let blocked = tokio::spawn({
            let tx = tx.clone();
            async move { tx.send(Ok(b"more".to_vec())).await.is_err() }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!blocked.is_finished());
        let text = failed(
            reg.fetch(
                &t,
                json!({ "url": url, "method": "BAD METHOD", "body_stream": handle }),
            )
            .await,
        );
        assert_eq!(text, "fetch: invalid method 'BAD METHOD'");
        let released = tokio::time::timeout(Duration::from_secs(2), blocked)
            .await
            .expect("the blocked write never resolved")
            .unwrap();
        assert!(released);
        assert_eq!(reg.entry(handle), Some((true, false)));
        drop(tx);
        body::end_outbound(&reg.outbound, handle);
        assert_eq!(reg.entry(handle), None);

        // Taken, then ended: removed at the end.
        let (handle, tx) = reg.channel(8);
        let fetch = tokio::spawn({
            let reg = reg.clone();
            let t = t.clone();
            let url = url.clone();
            async move {
                reg.fetch(
                    &t,
                    json!({ "url": url, "method": "POST", "body_stream": handle }),
                )
                .await
            }
        });
        for _ in 0..400 {
            if reg.entry(handle) == Some((true, false)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(reg.entry(handle), Some((true, false)), "never taken");
        tx.send(Ok(b"x".to_vec())).await.unwrap();
        drop(tx);
        body::end_outbound(&reg.outbound, handle);
        assert_eq!(reg.entry(handle), None);
        let p = payload(fetch.await.unwrap());
        assert_eq!(reg.text(handle_of(&p)).await, "ok");

        // Ended before the take: the take removes it.
        let (handle, tx) = reg.channel(8);
        tx.send(Ok(b"y".to_vec())).await.unwrap();
        drop(tx);
        body::end_outbound(&reg.outbound, handle);
        assert_eq!(reg.entry(handle), Some((false, true)));
        let p = payload(
            reg.fetch(
                &t,
                json!({ "url": url, "method": "POST", "body_stream": handle }),
            )
            .await,
        );
        assert_eq!(reg.entry(handle), None);
        reg.text(handle_of(&p)).await;

        // A send failure after the take removes it.
        let (handle, tx) = reg.channel(8);
        let dead = format!("http://127.0.0.1:{}/", closed_port().await);
        let outcome = reg
            .fetch(
                &t,
                json!({ "url": dead, "method": "POST", "body_stream": handle }),
            )
            .await;
        assert!(
            matches!(outcome, OpOutcome::NodeFailed { .. }),
            "{outcome:?}"
        );
        assert_eq!(reg.entry(handle), None);
        drop(tx);

        let seen = server.seen();
        assert_eq!(seen[0].body, b"x");
        assert_eq!(seen[1].body, b"y");

        // A redirect that fails the fetch after the take removes it too.
        let bad =
            serve_replies(|_| response("302 Found", &[("location", "http://[::1")], b"")).await;
        let (handle, tx) = reg.channel(8);
        tx.send(Ok(b"z".to_vec())).await.unwrap();
        let outbound = reg.outbound.clone();
        let fetch = reg.fetch(
            &t,
            json!({
                "url": format!("http://127.0.0.1:{}/", bad.port),
                "method": "POST",
                "body_stream": handle,
            }),
        );
        let ender = async move {
            // The server answers once the body ends; end it after the take.
            for _ in 0..400 {
                if outbound
                    .lock()
                    .unwrap()
                    .get(&handle)
                    .is_some_and(|e| e.1.is_none())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            drop(tx);
            let sender = outbound
                .lock()
                .unwrap()
                .get_mut(&handle)
                .and_then(|e| e.0.take());
            drop(sender);
        };
        let (outcome, ()) = tokio::join!(fetch, ender);
        assert_eq!(failed(outcome), INVALID_URL);
        assert_eq!(reg.entry(handle), None);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn early_failure_texts() {
    within(async {
        let reg = Reg::new();
        let t = plain();
        assert_eq!(
            failed(
                reg.fetch(
                    &t,
                    json!({ "url": "http://127.0.0.1:1/", "method": "POST", "body_base64": "!!!" }),
                )
                .await
            ),
            "fetch: malformed base64 body"
        );
        assert_eq!(
            failed(reg.fetch(&t, json!({ "url": "not a url" })).await),
            "builder error"
        );
        assert_eq!(
            failed(
                reg.fetch(
                    &t,
                    json!({ "url": "http://127.0.0.1:1/", "body_stream": 424242 })
                )
                .await
            ),
            "fetch: unknown body stream 424242"
        );
    })
    .await;
}

// ---------------------------------------------------------------- connect.lookup

/// The hook is asked for the initial host and again for a cross-host
/// redirect target; each hop reaches the address the hook gave, with the
/// host name in `Host`. The env proxy is never used.
#[tokio::test(flavor = "multi_thread")]
async fn lookup_park_continue_round_trip_across_a_cross_host_redirect() {
    within(async {
        let b = serve_replies(|_| response("200 OK", &[], b"done")).await;
        let b_port = b.port;
        let a = serve_replies(move |_| {
            let next = format!("http://b.test:{b_port}/next#keep");
            response("302 Found", &[("location", &next)], b"")
        })
        .await;
        let dead_proxy = closed_port().await;
        let rules = Matcher::builder()
            .all(format!("http://127.0.0.1:{dead_proxy}"))
            .build();
        let t = transport(ProxySource::Fixed(Box::new(rules)));
        let reg = Reg::new();

        let first = reg
            .fetch(
                &t,
                json!({ "url": format!("http://A.test:{}/start", a.port), "lookup_hook": true }),
            )
            .await;
        let (token, host, port) = lookup_of(first);
        assert_eq!((host.as_str(), port), ("a.test", a.port));
        assert_eq!(reg.parked(), 1);
        assert_eq!(a.accepts(), 0);

        let (token, host, port) = lookup_of(reg.resume(token, &["127.0.0.1"]).await);
        assert_eq!((host.as_str(), port), ("b.test", b.port));
        assert_eq!(reg.parked(), 1);
        assert_eq!(
            a.seen()[0].head.get("host"),
            Some(format!("a.test:{}", a.port).as_str())
        );
        assert_eq!(b.accepts(), 0);

        let p = payload(reg.resume(token, &["127.0.0.1"]).await);
        assert_eq!(p["status"], 200);
        assert_eq!(p["redirected"], true);
        assert_eq!(p["url"], format!("http://b.test:{}/next", b.port).as_str());
        assert_eq!(reg.text(handle_of(&p)).await, "done");
        assert_eq!(
            b.seen()[0].head.get("host"),
            Some(format!("b.test:{}", b.port).as_str())
        );
        assert_eq!(reg.parked(), 0);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn lookup_unknown_token_and_bad_answers() {
    within(async {
        let reg = Reg::new();
        assert_eq!(
            failed(reg.resume(12345, &["127.0.0.1"]).await),
            "fetch: lookup continuation 12345 is gone"
        );
        assert!(!send::fetch_abandon(12345, &reg.continuations));

        let t = plain();
        let request = json!({ "url": "http://pinned.test:8080/", "lookup_hook": true });
        let (token, _, _) = lookup_of(reg.fetch(&t, request.clone()).await);
        assert_eq!(
            failed(reg.resume(token, &["not-an-ip"]).await),
            "fetch: connect pin failed: pin ip 'not-an-ip' is not an IP: invalid IP address syntax"
        );
        assert_eq!(reg.parked(), 0);

        let (token, _, _) = lookup_of(reg.fetch(&t, request.clone()).await);
        match reg.resume(token, &[]).await {
            OpOutcome::NodeFailed { code, message, .. } => {
                assert_eq!(code, "ERR_INVALID_IP_ADDRESS");
                assert_eq!(message, "Invalid IP address: undefined");
            }
            other => panic!("{other:?}"),
        }

        // A malformed answer consumes the parked fetch.
        let (token, _, _) = lookup_of(reg.fetch(&t, request).await);
        let text = failed(
            send::fetch_continue(
                token,
                "[]".to_string(),
                reg.bodies.clone(),
                reg.ids.clone(),
                reg.continuations.clone(),
            )
            .await,
        );
        assert!(
            text.starts_with("fetch: malformed lookup result: "),
            "{text}"
        );
        assert_eq!(reg.parked(), 0);
    })
    .await;
}

/// A parked fetch holds its streamed body untaken; abandoning it releases
/// the receiver, so a writer blocked on a full channel resolves.
#[tokio::test(flavor = "multi_thread")]
async fn lookup_abandon_drops_the_stream_slot() {
    within(async {
        let server = serve_replies(|_| response("200 OK", &[], b"ok")).await;
        let reg = Reg::new();
        let (handle, tx) = reg.channel(1);
        tx.try_send(Ok(b"fill".to_vec())).unwrap();
        let blocked = tokio::spawn({
            let tx = tx.clone();
            async move { tx.send(Ok(b"more".to_vec())).await.is_err() }
        });
        let (token, _, _) = lookup_of(
            reg.fetch(
                &plain(),
                json!({
                    "url": format!("http://a.test:{}/", server.port),
                    "method": "POST",
                    "body_stream": handle,
                    "lookup_hook": true,
                }),
            )
            .await,
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!blocked.is_finished());
        assert_eq!(reg.entry(handle), Some((true, true)));
        assert!(send::fetch_abandon(token, &reg.continuations));
        assert_eq!(reg.parked(), 0);
        let released = tokio::time::timeout(Duration::from_secs(2), blocked)
            .await
            .expect("the blocked write never resolved")
            .unwrap();
        assert!(released);
        drop(tx);
        body::end_outbound(&reg.outbound, handle);
        assert_eq!(reg.entry(handle), None);
        assert_eq!(server.accepts(), 0);
    })
    .await;
}

/// undici's order: a bad port fails before the hook is consulted, on the
/// first request and on a redirect hop.
#[tokio::test(flavor = "multi_thread")]
async fn lookup_bad_port_fails_before_the_park() {
    within(async {
        let reg = Reg::new();
        let t = plain();
        let text = failed(
            reg.fetch(
                &t,
                json!({ "url": "http://a.test:25/", "lookup_hook": true, "fetch_semantics": true }),
            )
            .await,
        );
        assert_eq!(text, "bad port");
        assert_eq!(reg.parked(), 0);

        let server =
            serve_replies(|_| response("302 Found", &[("location", "http://b.test:25/")], b""))
                .await;
        let (token, _, _) = lookup_of(
            reg.fetch(
                &t,
                json!({
                    "url": format!("http://a.test:{}/", server.port),
                    "lookup_hook": true,
                    "fetch_semantics": true,
                }),
            )
            .await,
        );
        assert_eq!(failed(reg.resume(token, &["127.0.0.1"]).await), "bad port");
        assert_eq!(reg.parked(), 0);
    })
    .await;
}

/// A same-host hop reuses the addresses the hook gave; an IP-literal hop
/// needs none. One park for the whole fetch.
#[tokio::test(flavor = "multi_thread")]
async fn lookup_same_host_and_ip_literal_hops_do_not_park() {
    within(async {
        let port = Arc::new(AtomicU16::new(0));
        let server = {
            let port = port.clone();
            serve_replies(move |r| match r.head.target.as_str() {
                "/one" => response("302 Found", &[("location", "/two")], b""),
                "/two" => {
                    let literal = format!("http://127.0.0.1:{}/three", port.load(Ordering::SeqCst));
                    response("307 Temporary Redirect", &[("location", &literal)], b"")
                }
                _ => response("200 OK", &[], b"three"),
            })
            .await
        };
        port.store(server.port, Ordering::SeqCst);
        let reg = Reg::new();
        let (token, host, _) = lookup_of(
            reg.fetch(
                &plain(),
                json!({ "url": format!("http://a.test:{}/one", server.port), "lookup_hook": true }),
            )
            .await,
        );
        assert_eq!(host, "a.test");
        let p = payload(reg.resume(token, &["127.0.0.1"]).await);
        assert_eq!(p["status"], 200);
        assert_eq!(reg.text(handle_of(&p)).await, "three");
        let hosts: Vec<_> = server
            .seen()
            .iter()
            .map(|r| r.head.get("host").unwrap().to_string())
            .collect();
        assert_eq!(
            hosts,
            [
                format!("a.test:{}", server.port),
                format!("a.test:{}", server.port),
                format!("127.0.0.1:{}", server.port),
            ]
        );
        assert_eq!(reg.parked(), 0);
    })
    .await;
}

// ---------------------------------------------------------------- net permission
//
// The engine hands the loop its `--permission` net grant as a closure; these
// drive the loop with a stand-in that grants a fixed set of hosts and records
// every target it is asked about. What it proves is the plumbing: the loop
// asks about every hop, before the hop is dialled and before it parks for a
// lookup hook, about the destination and not a proxy, and a refusal fails the
// fetch with the verdict. The grant's own matching is the engine's (tested
// there and end to end).

type Asked = Arc<Mutex<Vec<(String, u16)>>>;

/// A check granting exactly `allowed`, and the targets it was asked about.
fn recording_check(allowed: &'static [&'static str]) -> (NetCheck, Asked) {
    let asked: Asked = Arc::new(Mutex::new(Vec::new()));
    let record = asked.clone();
    let check: NetCheck = Arc::new(move |target: &NetTarget<'_>| {
        record
            .lock()
            .unwrap()
            .push((target.host.to_string(), target.port));
        if allowed.contains(&target.host) {
            Ok(())
        } else {
            Err(net(target.host))
        }
    });
    (check, asked)
}

fn net(resource: &str) -> AccessDenial {
    AccessDenial {
        permission: "Net".to_string(),
        resource: resource.to_string(),
    }
}

fn refused(outcome: OpOutcome) -> AccessDenial {
    match outcome {
        OpOutcome::AccessDenied(denial) => denial,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn asked(record: &Asked) -> Vec<(String, u16)> {
    record.lock().unwrap().clone()
}

/// A redirect to a host the grant does not cover fails the fetch with the
/// refusal, and the host is never dialled: not for a GET, and not for the
/// re-sent body of a 307 POST. The check was asked about the first hop too.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_redirect_hop_is_never_dialled() {
    within(async {
        let target = serve_replies(|_| response("200 OK", &[], b"secret")).await;
        let target_port = target.port;
        let origin = serve_replies(move |r| {
            let status = if r.head.method == "POST" {
                "307 Temporary Redirect"
            } else {
                "302 Found"
            };
            let next = format!("http://localhost:{target_port}/secret");
            response(status, &[("location", &next)], b"")
        })
        .await;
        let (check, record) = recording_check(&["127.0.0.1"]);
        let reg = Reg::with_net_check(check);
        let url = format!("http://127.0.0.1:{}/start", origin.port);

        let get = reg.fetch(&plain(), json!({ "url": url })).await;
        assert_eq!(refused(get), net("localhost"));
        let post = reg
            .fetch(
                &plain(),
                json!({ "url": url, "method": "POST", "body": "payload" }),
            )
            .await;
        assert_eq!(refused(post), net("localhost"));

        assert_eq!(target.accepts(), 0, "a refused hop was dialled");
        assert_eq!(origin.seen().len(), 2);
        let hop = ("localhost".to_string(), target.port);
        let first = ("127.0.0.1".to_string(), origin.port);
        assert_eq!(asked(&record), [first.clone(), hop.clone(), first, hop]);
    })
    .await;
}

/// Granted hops are followed, and the check is asked about each one, with
/// the port it will be dialled on (the scheme default when the URL names
/// none). With no check, the same chain is followed without one.
#[tokio::test(flavor = "multi_thread")]
async fn granted_hops_are_followed_and_each_is_checked() {
    within(async {
        let end = serve_replies(|_| response("200 OK", &[], b"end")).await;
        let end_port = end.port;
        let mid = serve_replies(move |_| {
            let next = format!("http://127.0.0.1:{end_port}/end");
            response("302 Found", &[("location", &next)], b"")
        })
        .await;
        let mid_port = mid.port;
        let origin = serve_replies(move |_| {
            let next = format!("http://127.0.0.1:{mid_port}/mid");
            response("301 Moved Permanently", &[("location", &next)], b"")
        })
        .await;
        let url = format!("http://127.0.0.1:{}/", origin.port);
        let (check, record) = recording_check(&["127.0.0.1"]);
        for reg in [Reg::with_net_check(check), Reg::new()] {
            let p = payload(reg.fetch(&plain(), json!({ "url": url })).await);
            assert_eq!(p["status"], 200);
            assert_eq!(p["redirected"], true);
            assert_eq!(reg.text(handle_of(&p)).await, "end");
        }
        let ip = |port| ("127.0.0.1".to_string(), port);
        assert_eq!(
            asked(&record),
            [ip(origin.port), ip(mid.port), ip(end.port)]
        );

        // No port in the URL: the scheme's default is what gets dialled.
        let (check, record) = recording_check(&[]);
        let reg = Reg::with_net_check(check);
        for (url, host, port) in [
            ("http://example.test/", "example.test", 80),
            ("https://example.test/", "example.test", 443),
        ] {
            assert_eq!(
                refused(reg.fetch(&plain(), json!({ "url": url })).await),
                net(host)
            );
            assert_eq!(asked(&record).pop(), Some((host.to_string(), port)));
        }
    })
    .await;
}

/// The check sees the host the dialler will resolve: the URL parser's
/// normalisation of the Location, never its raw spelling. Case,
/// percent-encoding, IDNA and non-canonical IP literals are folded before the
/// check is asked; a trailing dot is not (`localhost.` is its own name). A
/// spelling that normalises to a granted address is followed.
#[tokio::test(flavor = "multi_thread")]
async fn the_check_sees_the_normalised_host() {
    within(async {
        let target = serve_replies(|_| response("200 OK", &[], b"target")).await;
        let port = target.port;
        let origin = serve_replies(move |r| {
            // The Location's authority is the request path, verbatim.
            let authority = r.head.target.trim_start_matches('/');
            let next = format!("http://{authority}/");
            response("302 Found", &[("location", &next)], b"")
        })
        .await;
        let (check, record) = recording_check(&["127.0.0.1"]);
        let reg = Reg::with_net_check(check);
        let refusals = [
            (format!("LOCALHOST:{port}"), "localhost"),
            (format!("LocalHost:{port}"), "localhost"),
            (format!("%6c%6fcalhost:{port}"), "localhost"),
            (format!("localhost.:{port}"), "localhost."),
            (format!("[0:0:0:0:0:0:0:1]:{port}"), "[::1]"),
            (format!("[::FFFF:127.0.0.1]:{port}"), "[::ffff:7f00:1]"),
            (format!("xn--bcher-kva.test:{port}"), "xn--bcher-kva.test"),
            (format!("B%C3%BCcher.test:{port}"), "xn--bcher-kva.test"),
        ];
        for (authority, host) in &refusals {
            let url = format!("http://127.0.0.1:{}/{authority}", origin.port);
            let outcome = reg.fetch(&plain(), json!({ "url": url })).await;
            assert_eq!(refused(outcome), net(host), "{authority}");
            assert_eq!(asked(&record).pop().unwrap().0, *host, "{authority}");
        }
        assert_eq!(target.accepts(), 0, "a refused spelling was dialled");

        // The same address, spelled another way, IS the granted host.
        for authority in [format!("0x7f.1:{port}"), format!("2130706433:{port}")] {
            let url = format!("http://127.0.0.1:{}/{authority}", origin.port);
            let p = payload(reg.fetch(&plain(), json!({ "url": url })).await);
            assert_eq!(p["status"], 200, "{authority}");
            reg.text(handle_of(&p)).await;
            assert_eq!(
                asked(&record).pop(),
                Some(("127.0.0.1".to_string(), port)),
                "{authority}"
            );
        }
        assert_eq!(target.accepts(), 2);
    })
    .await;
}

/// On the hooked route the check comes before the park: a refused hop fails
/// without asking the hook to resolve its name (the hook only ever sees
/// granted names), and so does a refused first host. The check travels with
/// the parked fetch, so the resumed loop still enforces it.
#[tokio::test(flavor = "multi_thread")]
async fn the_check_runs_before_a_hop_parks_for_its_lookup_hook() {
    within(async {
        let target = serve_replies(|_| response("200 OK", &[], b"b")).await;
        let target_port = target.port;
        let origin = serve_replies(move |_| {
            let next = format!("http://b.test:{target_port}/");
            response("302 Found", &[("location", &next)], b"")
        })
        .await;
        let (check, record) = recording_check(&["a.test"]);
        let reg = Reg::with_net_check(check);
        let t = plain();

        let first = reg
            .fetch(
                &t,
                json!({ "url": format!("http://a.test:{}/", origin.port), "lookup_hook": true }),
            )
            .await;
        let (token, host, _) = lookup_of(first);
        assert_eq!(host, "a.test");
        assert_eq!(reg.parked(), 1);
        let outcome = reg.resume(token, &["127.0.0.1"]).await;
        assert_eq!(refused(outcome), net("b.test"));
        assert_eq!(reg.parked(), 0);
        assert_eq!(origin.seen().len(), 1);
        assert_eq!(target.accepts(), 0);

        // A refused first host never parks.
        let outcome = reg
            .fetch(
                &t,
                json!({ "url": format!("http://b.test:{target_port}/"), "lookup_hook": true }),
            )
            .await;
        assert_eq!(refused(outcome), net("b.test"));
        assert_eq!(reg.parked(), 0);
        assert_eq!(target.accepts(), 0);

        let hosts: Vec<String> = asked(&record).into_iter().map(|(h, _)| h).collect();
        // a.test before its park and again when the loop resumes, then the
        // refused hop; then the refused first host.
        assert_eq!(hosts, ["a.test", "a.test", "b.test", "b.test"]);
    })
    .await;
}

/// Behind an environment proxy the check is asked about the destination, not
/// the proxy: the proxy carries the first request, answers it with a
/// redirect, and the refused hop never reaches the proxy either.
#[tokio::test(flavor = "multi_thread")]
async fn the_check_is_asked_about_the_destination_not_the_proxy() {
    within(async {
        let next = closed_port().await;
        let reply: &'static [u8] = Box::leak(
            response(
                "302 Found",
                &[("location", &format!("http://localhost:{next}/"))],
                b"",
            )
            .into_boxed_slice(),
        );
        let proxy = proxy(None, reply).await;
        let rules = Matcher::builder()
            .http(format!("http://127.0.0.1:{}", proxy.port))
            .build();
        let t = transport(ProxySource::Fixed(Box::new(rules)));
        let (check, record) = recording_check(&["dest.test"]);
        let reg = Reg::with_net_check(check);

        let outcome = reg
            .fetch(&t, json!({ "url": "http://dest.test:8080/start" }))
            .await;
        assert_eq!(refused(outcome), net("localhost"));
        let seen: Vec<String> = proxy.seen().into_iter().map(|r| r.head.target).collect();
        assert_eq!(seen, ["http://dest.test:8080/start"]);
        assert_eq!(
            asked(&record),
            [
                ("dest.test".to_string(), 8080),
                ("localhost".to_string(), next)
            ]
        );
    })
    .await;
}

/// A refusal releases a streamed request body like any other failure: a
/// producer blocked on a full channel is let go instead of hanging.
#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_releases_a_streamed_body() {
    within(async {
        let (check, _) = recording_check(&[]);
        let reg = Reg::with_net_check(check);
        let (handle, tx) = reg.channel(1);
        tx.try_send(Ok(b"fill".to_vec())).unwrap();
        let blocked = tokio::spawn({
            let tx = tx.clone();
            async move { tx.send(Ok(b"more".to_vec())).await.is_err() }
        });
        let outcome = reg
            .fetch(
                &plain(),
                json!({ "url": "http://refused.test/", "method": "POST", "body_stream": handle }),
            )
            .await;
        assert_eq!(refused(outcome), net("refused.test"));
        let released = tokio::time::timeout(Duration::from_secs(2), blocked)
            .await
            .expect("the blocked write never resolved")
            .unwrap();
        assert!(released);
        assert_eq!(reg.entry(handle), None);
    })
    .await;
}

// ---------------------------------------------------------------- static

fn assert_send<T: Send>() {}

fn assert_op_future<F: std::future::Future + Send + 'static>(_: F) {}

/// The op futures are what `spawn_op` needs (Send + 'static).
#[test]
fn static_checks() {
    assert_send::<body::FetchBody>();
    assert_send::<send::PendingFetch>();
    let reg = Reg::new();
    let request: FetchRequest = serde_json::from_value(json!({ "url": "http://x/" })).unwrap();
    assert_op_future(send::fetch(
        plain(),
        request,
        reg.bodies.clone(),
        reg.ids.clone(),
        reg.outbound.clone(),
        reg.continuations.clone(),
        reg.net_check.clone(),
    ));
    assert_op_future(send::fetch_continue(
        1,
        String::from("{}"),
        reg.bodies.clone(),
        reg.ids.clone(),
        reg.continuations.clone(),
    ));
    assert_op_future(body::read(reg.bodies, reg.cancelled, reg.signal, 1));
}
