//! `http_client::bridge`: an HTTP/1.1 exchange over bytes the caller pumps,
//! as `http.request` runs one over an agent's socket. Driven here the way JS
//! drives it: the request is read off `out`, the response fed to `input`.

mod common;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::within;
use oam_core::http_client::body::{self, FetchBodies};
use oam_core::http_client::bridge::{self, Bridges};
use oam_core::{BodyCancelSignal, CancelledBodies, OpOutcome, OutboundBodies};
use serde_json::{Value, json};

struct Reg {
    bridges: Bridges,
    bodies: FetchBodies,
    ids: Arc<AtomicU64>,
    outbound: OutboundBodies,
    cancelled: CancelledBodies,
    signal: BodyCancelSignal,
}

impl Reg {
    fn new() -> Reg {
        Reg {
            bridges: Arc::new(Mutex::new(HashMap::new())),
            bodies: Arc::new(Mutex::new(HashMap::new())),
            ids: Arc::new(AtomicU64::new(1)),
            outbound: Arc::new(Mutex::new(HashMap::new())),
            cancelled: Arc::new(Mutex::new(HashSet::new())),
            signal: Arc::new(tokio::sync::Notify::new()),
        }
    }

    fn start(&self, request: Value) -> u64 {
        bridge::start(
            &self.bridges,
            &self.ids,
            self.outbound.clone(),
            &request.to_string(),
        )
        .unwrap()
    }

    /// The exchange's response head, run in the background.
    fn response(&self, id: u64) -> tokio::task::JoinHandle<OpOutcome> {
        tokio::spawn(bridge::response(
            self.bridges.clone(),
            id,
            self.bodies.clone(),
            self.ids.clone(),
        ))
    }

    /// Everything hyper writes until `until` appears in it.
    async fn written_until(&self, id: u64, until: &str) -> String {
        let mut wire = Vec::new();
        while !String::from_utf8_lossy(&wire).contains(until) {
            match bridge::out(self.bridges.clone(), id).await {
                OpOutcome::Bytes(bytes) => wire.extend_from_slice(&bytes),
                other => panic!("out ended early: {other:?} after {wire:?}"),
            }
        }
        String::from_utf8(wire).unwrap()
    }

    async fn feed(&self, id: u64, bytes: &[u8]) {
        match bridge::input(self.bridges.clone(), id, bytes.to_vec()).await {
            OpOutcome::Done => {}
            other => panic!("input failed: {other:?}"),
        }
    }

    async fn body(&self, handle: u64) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        loop {
            match body::read(
                self.bodies.clone(),
                self.cancelled.clone(),
                self.signal.clone(),
                handle,
            )
            .await
            {
                OpOutcome::Bytes(chunk) => out.extend_from_slice(&chunk),
                OpOutcome::Done => return Ok(out),
                OpOutcome::Failed(text) => return Err(text),
                other => panic!("{other:?}"),
            }
        }
    }
}

fn payload(outcome: OpOutcome) -> Value {
    match outcome {
        OpOutcome::Json(text) => serde_json::from_str(&text).unwrap(),
        other => panic!("expected a payload, got {other:?}"),
    }
}

fn node_failure(outcome: OpOutcome) -> (String, String) {
    match outcome {
        OpOutcome::NodeFailed { code, message, .. } => (code, message),
        other => panic!("expected a coded failure, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_request_goes_out_as_given_and_the_head_comes_back() {
    within(async {
        let reg = Reg::new();
        let id = reg.start(json!({
            "method": "GET",
            "target": "/p?q=1",
            "headers": [["x-first", "1"], ["Host", "guard.test:8080"], ["Connection", "close"]],
        }));
        let head = reg.response(id);
        let wire = reg.written_until(id, "\r\n\r\n").await;
        assert_eq!(
            wire,
            "GET /p?q=1 HTTP/1.1\r\nx-first: 1\r\nhost: guard.test:8080\r\nconnection: close\r\n\r\n"
        );
        reg.feed(
            id,
            b"HTTP/1.1 200 Fine By Me\r\nSet-Cookie: a=1\r\nset-cookie: b=2\r\nx-l: caf\xe9\r\ncontent-length: 2\r\n\r\nhi",
        )
        .await;
        let p = payload(head.await.unwrap());
        assert_eq!(p["status"], 200);
        assert_eq!(p["statusText"], "Fine By Me");
        assert_eq!(p["httpVersion"], "1.1");
        assert_eq!(
            p["headers"],
            json!([["set-cookie", "a=1"], ["set-cookie", "b=2"], ["x-l", "caf\u{e9}"], ["content-length", "2"]])
        );
        let body = reg.body(p["bodyHandle"].as_u64().unwrap()).await.unwrap();
        assert_eq!(body, b"hi");
        assert!(bridge::close(&reg.bridges, id));
        assert!(!bridge::close(&reg.bridges, id));
        assert_eq!(bridge::open(&reg.bridges), 0);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_buffered_body_is_sent_with_its_length() {
    within(async {
        let reg = Reg::new();
        let id = reg.start(json!({
            "method": "POST",
            "target": "/up",
            "headers": [["host", "h"]],
            "body_base64": "aGVsbG8=",
        }));
        let head = reg.response(id);
        let wire = reg.written_until(id, "hello").await;
        assert_eq!(
            wire,
            "POST /up HTTP/1.1\r\nhost: h\r\ncontent-length: 5\r\n\r\nhello"
        );
        reg.feed(id, b"HTTP/1.1 204 No Content\r\n\r\n").await;
        let p = payload(head.await.unwrap());
        assert_eq!(p["status"], 204);
        assert_eq!(
            reg.body(p["bodyHandle"].as_u64().unwrap()).await.unwrap(),
            b""
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_streamed_body_is_sent_chunked_and_a_chunked_response_is_read() {
    within(async {
        let reg = Reg::new();
        let handle = 9000;
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        reg.outbound
            .lock()
            .unwrap()
            .insert(handle, (Some(tx.clone()), Some(rx)));
        let id = reg.start(json!({
            "method": "PUT",
            "target": "/s",
            "headers": [["host", "h"]],
            "body_stream": handle,
        }));
        let head = reg.response(id);
        tx.send(Ok(b"ab".to_vec())).await.unwrap();
        tx.send(Ok(b"cd".to_vec())).await.unwrap();
        drop(tx);
        body::end_outbound(&reg.outbound, handle);
        let wire = reg.written_until(id, "0\r\n\r\n").await;
        assert_eq!(
            wire,
            "PUT /s HTTP/1.1\r\nhost: h\r\ntransfer-encoding: chunked\r\n\r\n2\r\nab\r\n2\r\ncd\r\n0\r\n\r\n"
        );
        reg.feed(
            id,
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
        )
        .await;
        let p = payload(head.await.unwrap());
        assert_eq!(reg.body(p["bodyHandle"].as_u64().unwrap()).await.unwrap(), b"abc");
        assert!(reg.outbound.lock().unwrap().is_empty());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn eof_before_the_head_is_socket_hang_up() {
    within(async {
        let reg = Reg::new();
        let id = reg.start(json!({ "method": "GET", "target": "/", "headers": [["host", "h"]] }));
        let head = reg.response(id);
        reg.written_until(id, "\r\n\r\n").await;
        reg.feed(id, b"HTTP/1.1 200 O").await;
        bridge::input_end(reg.bridges.clone(), id).await;
        assert_eq!(
            node_failure(head.await.unwrap()),
            ("ECONNRESET".to_string(), "socket hang up".to_string())
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn eof_mid_body_fails_the_body() {
    within(async {
        let reg = Reg::new();
        let id = reg.start(json!({ "method": "GET", "target": "/", "headers": [["host", "h"]] }));
        let head = reg.response(id);
        reg.written_until(id, "\r\n\r\n").await;
        reg.feed(id, b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\nabc")
            .await;
        let p = payload(head.await.unwrap());
        bridge::input_end(reg.bridges.clone(), id).await;
        assert!(reg.body(p["bodyHandle"].as_u64().unwrap()).await.is_err());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_status_line_is_a_coded_parse_error() {
    within(async {
        let reg = Reg::new();
        let id = reg.start(json!({ "method": "GET", "target": "/", "headers": [["host", "h"]] }));
        let head = reg.response(id);
        reg.written_until(id, "\r\n\r\n").await;
        reg.feed(id, b"NOT HTTP AT ALL\r\n\r\n").await;
        let (code, message) = node_failure(head.await.unwrap());
        assert!(code.starts_with("HPE_"), "{code}");
        assert!(message.starts_with("Parse Error: "), "{message}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_head_or_304_response_has_no_body() {
    within(async {
        for (method, status) in [("HEAD", "200 OK"), ("GET", "304 Not Modified")] {
            let reg = Reg::new();
            let id =
                reg.start(json!({ "method": method, "target": "/", "headers": [["host", "h"]] }));
            let head = reg.response(id);
            reg.written_until(id, "\r\n\r\n").await;
            let reply = format!("HTTP/1.1 {status}\r\ncontent-length: 5\r\n\r\n");
            reg.feed(id, reply.as_bytes()).await;
            let p = payload(head.await.unwrap());
            assert_eq!(
                reg.body(p["bodyHandle"].as_u64().unwrap()).await.unwrap(),
                b""
            );
        }
    })
    .await;
}

/// Both directions are bounded: a response body nobody reads stops taking
/// the socket's bytes once the pipe is full.
#[tokio::test(flavor = "multi_thread")]
async fn an_unread_response_body_pushes_back_on_the_socket() {
    within(async {
        let reg = Reg::new();
        let id = reg.start(json!({ "method": "GET", "target": "/", "headers": [["host", "h"]] }));
        let head = reg.response(id);
        reg.written_until(id, "\r\n\r\n").await;
        let size = 4 * 1024 * 1024;
        reg.feed(
            id,
            format!("HTTP/1.1 200 OK\r\ncontent-length: {size}\r\n\r\n").as_bytes(),
        )
        .await;
        let p = payload(head.await.unwrap());
        let chunk = vec![b'x'; 256 * 1024];
        let feeding = tokio::spawn(bridge::input(reg.bridges.clone(), id, chunk.repeat(16)));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!feeding.is_finished(), "the pipe took 4 MiB nobody read");
        let body = reg.body(p["bodyHandle"].as_u64().unwrap()).await.unwrap();
        assert_eq!(body.len(), size);
        assert!(matches!(feeding.await.unwrap(), OpOutcome::Done));
    })
    .await;
}

/// Closing a bridge ends a parked `out` read (EOF) and fails the exchange;
/// later calls on the id are inert.
#[tokio::test(flavor = "multi_thread")]
async fn closing_ends_everything_parked() {
    within(async {
        let reg = Reg::new();
        let id = reg.start(json!({ "method": "GET", "target": "/", "headers": [["host", "h"]] }));
        let head = reg.response(id);
        reg.written_until(id, "\r\n\r\n").await;
        let parked = tokio::spawn(bridge::out(reg.bridges.clone(), id));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(bridge::close(&reg.bridges, id));
        assert!(matches!(parked.await.unwrap(), OpOutcome::Done));
        assert!(matches!(head.await.unwrap(), OpOutcome::NodeFailed { .. }));
        assert!(matches!(
            bridge::input(reg.bridges.clone(), id, b"x".to_vec()).await,
            OpOutcome::Done
        ));
        assert!(matches!(
            bridge::out(reg.bridges.clone(), id).await,
            OpOutcome::Done
        ));
    })
    .await;
}

/// A request hyper cannot write is refused at start, before anything runs.
#[test]
fn an_unwritable_request_is_refused_at_start() {
    let reg = Reg::new();
    for request in [
        json!({ "method": "G ET", "target": "/", "headers": [] }),
        json!({ "method": "GET", "target": "/", "headers": [["bad name", "v"]] }),
        json!({ "method": "GET", "target": "/", "headers": [["x", "a\u{100}"]] }),
        json!({ "method": "GET", "target": "/", "headers": [["x", "a\r\nb"]] }),
    ] {
        assert!(
            bridge::start(
                &reg.bridges,
                &reg.ids,
                reg.outbound.clone(),
                &request.to_string()
            )
            .is_err(),
            "{request}"
        );
    }
    assert_eq!(bridge::open(&reg.bridges), 0);
}

/// The request target goes out as node writes it -- verbatim, one byte per
/// code point -- where `http::Uri` holds it; a byte it will not hold is
/// percent-encoded (node sends `/café` as `/caf\xe9`, oam as `/caf%E9`); an
/// absolute-form target (a request to a proxy) goes out as written.
#[tokio::test(flavor = "multi_thread")]
async fn request_targets_are_sent_as_close_to_verbatim_as_hyper_allows() {
    within(async {
        for (target, line) in [
            ("/a{b}|c", &b"GET /a{b}|c HTTP/1.1\r\n"[..]),
            ("/q\"<>\\^`", b"GET /q%22%3C%3E%5C%5E%60 HTTP/1.1\r\n"),
            ("/caf\u{e9}", b"GET /caf%E9 HTTP/1.1\r\n"),
            (
                "http://origin.test:81/x",
                b"GET http://origin.test:81/x HTTP/1.1\r\n",
            ),
        ] {
            let reg = Reg::new();
            let id =
                reg.start(json!({ "method": "GET", "target": target, "headers": [["host", "h"]] }));
            let _head = reg.response(id);
            let mut wire = Vec::new();
            while !wire.windows(4).any(|w| w == b"\r\n\r\n") {
                match bridge::out(reg.bridges.clone(), id).await {
                    OpOutcome::Bytes(bytes) => wire.extend_from_slice(&bytes),
                    other => panic!("{other:?}"),
                }
            }
            assert!(
                wire.starts_with(line),
                "{target}: {}",
                String::from_utf8_lossy(&wire)
            );
            bridge::close(&reg.bridges, id);
        }
    })
    .await;
}
