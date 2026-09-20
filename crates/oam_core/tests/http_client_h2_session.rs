//! `byte_pipe` and `http_client::h2_session`: an HTTP/2 client
//! session over bytes a socket pumps, as `http2.connect` runs one over the
//! socket it connected (or `createConnection` returned). The "socket" here is
//! a TCP stream the test pumps to and from the pipe exactly as JS does; the
//! origin is a real h2 server. Also: `tls.connect`'s ALPN offer.

mod common;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use common::*;
use http_body_util::BodyExt as _;
use oam_core::byte_pipe::{self as pipe, Pipes};
use oam_core::http_client::body::{self, FetchBodies};
use oam_core::http_client::h2_session::{self, H2Sessions};
use oam_core::{BodyCancelSignal, CancelledBodies, OpOutcome, OutboundBodies};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

struct Reg {
    pipes: Pipes,
    sessions: H2Sessions,
    bodies: FetchBodies,
    ids: Arc<AtomicU64>,
    outbound: OutboundBodies,
    cancelled: CancelledBodies,
    signal: BodyCancelSignal,
}

impl Reg {
    fn new() -> Reg {
        Reg {
            pipes: Arc::new(Mutex::new(HashMap::new())),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            bodies: Arc::new(Mutex::new(HashMap::new())),
            ids: Arc::new(AtomicU64::new(1)),
            outbound: Arc::new(Mutex::new(HashMap::new())),
            cancelled: Arc::new(Mutex::new(HashSet::new())),
            signal: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// A pipe pumped to and from `stream`, the way js/node_compat.js's
    /// pipeSocket pumps a socket: what the consumer writes goes to the
    /// stream, what the stream reads goes to the consumer, EOF included.
    fn pump(&self, stream: TcpStream) -> u64 {
        let id = pipe::open(&self.pipes, &self.ids);
        let (mut read, mut write) = stream.into_split();
        let pipes = self.pipes.clone();
        tokio::spawn(async move {
            loop {
                match pipe::out(pipes.clone(), id).await {
                    OpOutcome::Bytes(bytes) => {
                        if write.write_all(&bytes).await.is_err() {
                            return;
                        }
                    }
                    _ => {
                        let _ = write.shutdown().await;
                        return;
                    }
                }
            }
        });
        let pipes = self.pipes.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                match read.read(&mut buf).await {
                    Ok(0) | Err(_) => {
                        pipe::input_end(pipes.clone(), id).await;
                        return;
                    }
                    Ok(n) => {
                        pipe::input(pipes.clone(), id, buf[..n].to_vec()).await;
                    }
                }
            }
        });
        id
    }

    async fn open(&self, pipe_id: u64) -> u64 {
        let opened = payload(
            h2_session::open(
                self.sessions.clone(),
                self.pipes.clone(),
                pipe_id,
                self.ids.clone(),
            )
            .await,
        );
        opened["session"].as_u64().unwrap()
    }

    async fn request(&self, session: u64, request: Value) -> OpOutcome {
        h2_session::request(
            self.sessions.clone(),
            session,
            request.to_string(),
            self.bodies.clone(),
            self.ids.clone(),
            self.outbound.clone(),
        )
        .await
    }

    async fn text(&self, handle: u64) -> String {
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
                OpOutcome::Done => return String::from_utf8(out).unwrap(),
                other => panic!("{other:?}"),
            }
        }
    }

    /// A streamed-body channel, as `fetchBodyChannelNew` makes one.
    fn channel(&self) -> (u64, tokio::sync::mpsc::Sender<Result<Vec<u8>, String>>) {
        let handle = self.ids.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        self.outbound
            .lock()
            .unwrap()
            .insert(handle, (Some(tx.clone()), Some(rx)));
        (handle, tx)
    }
}

fn payload(outcome: OpOutcome) -> Value {
    match outcome {
        OpOutcome::Json(text) => serde_json::from_str(&text).unwrap(),
        other => panic!("expected a payload, got {other:?}"),
    }
}

/// What the origin saw of one request.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    uri: String,
    header: Option<String>,
    body: String,
}

/// An h2c origin (prior knowledge) on 127.0.0.1: `/echo` answers 201 with
/// the request body, `/empty` 204 with no body, anything else 200 naming
/// the path. Records what it saw and counts connections.
async fn h2c_origin() -> (u16, Arc<Mutex<Vec<Seen>>>, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let accepts = Arc::new(AtomicUsize::new(0));
    let (record, count) = (seen.clone(), accepts.clone());
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            count.fetch_add(1, Ordering::SeqCst);
            let record = record.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(
                    move |request: http::Request<hyper::body::Incoming>| {
                        let record = record.clone();
                        async move {
                            let (parts, incoming) = request.into_parts();
                            let bytes = incoming
                                .collect()
                                .await
                                .map(|c| c.to_bytes())
                                .unwrap_or_default();
                            let body = String::from_utf8_lossy(&bytes).into_owned();
                            record.lock().unwrap().push(Seen {
                                method: parts.method.to_string(),
                                uri: parts.uri.to_string(),
                                header: parts
                                    .headers
                                    .get("x-probe")
                                    .map(|v| v.to_str().unwrap().to_string()),
                                body: body.clone(),
                            });
                            let response = match parts.uri.path() {
                                "/echo" => http::Response::builder()
                                    .status(201)
                                    .header("x-len", body.len().to_string())
                                    .body(http_body_util::Full::new(Bytes::from(format!(
                                        "echo:{body}"
                                    )))),
                                "/empty" => http::Response::builder()
                                    .status(204)
                                    .body(http_body_util::Full::new(Bytes::new())),
                                path => http::Response::builder()
                                    .header("set-cookie", "a=1")
                                    .header("set-cookie", "b=2")
                                    .body(http_body_util::Full::new(Bytes::from(format!(
                                        "path={path}"
                                    )))),
                            };
                            Ok::<_, std::convert::Infallible>(response.unwrap())
                        }
                    },
                );
                let _ =
                    hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                        .await;
            });
        }
    });
    (port, seen, accepts)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_carries_its_streams_over_the_pumped_socket() {
    within(async {
        let reg = Reg::new();
        let (port, seen, accepts) = h2c_origin().await;
        let socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let pipe_id = reg.pump(socket);
        let session = reg.open(pipe_id).await;
        assert!(pipe::take_near(&reg.pipes, pipe_id).is_none(), "the session took the pipe's end");

        // Two streams at once, on the one connection.
        let first = reg.request(
            session,
            json!({
                "method": "GET", "scheme": "http", "authority": "guard.test:8443", "path": "/a?q=1",
                "headers": [["x-probe", "one"]],
            }),
        );
        let second = reg.request(
            session,
            json!({ "method": "GET", "scheme": "http", "authority": "guard.test:8443", "path": "/b" }),
        );
        let (first, second) = tokio::join!(first, second);
        let first = payload(first);
        let second = payload(second);
        assert_eq!(first["status"], 200);
        assert_eq!(first["endStream"], false);
        let cookies: Vec<&str> = first["headers"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|pair| pair[0] == "set-cookie")
            .map(|pair| pair[1].as_str().unwrap())
            .collect();
        assert_eq!(cookies, ["a=1", "b=2"], "each field line, in order");
        assert_eq!(reg.text(first["bodyHandle"].as_u64().unwrap()).await, "path=/a");
        assert_eq!(reg.text(second["bodyHandle"].as_u64().unwrap()).await, "path=/b");

        // A streamed request body; the response is the echo.
        let (handle, tx) = reg.channel();
        let posting = reg.request(
            session,
            json!({
                "method": "POST", "scheme": "http", "authority": "guard.test:8443", "path": "/echo",
                "body_stream": handle,
            }),
        );
        let outbound = reg.outbound.clone();
        let writer = tokio::spawn(async move {
            tx.send(Ok(b"hello ".to_vec())).await.unwrap();
            tx.send(Ok(b"world".to_vec())).await.unwrap();
            drop(tx);
            // fetchBodyChannelEnd: the body ends.
            body::end_outbound(&outbound, handle);
        });
        let posted = payload(posting.await);
        writer.await.unwrap();
        assert_eq!(posted["status"], 201);
        assert_eq!(reg.text(posted["bodyHandle"].as_u64().unwrap()).await, "echo:hello world");

        // No body: the response ended with its headers.
        let empty = payload(
            reg.request(
                session,
                json!({ "method": "GET", "scheme": "http", "authority": "guard.test:8443", "path": "/empty" }),
            )
            .await,
        );
        assert_eq!(empty["status"], 204);
        assert_eq!(empty["endStream"], true);

        let seen = seen.lock().unwrap().clone();
        assert_eq!(accepts.load(Ordering::SeqCst), 1, "one connection for every stream");
        assert_eq!(seen.len(), 4);
        let a = seen.iter().find(|s| s.uri.ends_with("/a?q=1")).unwrap();
        assert_eq!(a.method, "GET");
        assert_eq!(a.uri, "http://guard.test:8443/a?q=1", ":scheme, :authority, :path");
        assert_eq!(a.header.as_deref(), Some("one"));
        let echo = seen.iter().find(|s| s.uri.ends_with("/echo")).unwrap();
        assert_eq!((echo.method.as_str(), echo.body.as_str()), ("POST", "hello world"));

        // A graceful close: no new streams, and the connection ends cleanly.
        assert!(h2_session::close(&reg.sessions, session));
        let ended = payload(h2_session::wait(reg.sessions.clone(), session).await);
        assert_eq!(ended, json!({ "error": null }));
        let refused = reg
            .request(
                session,
                json!({ "method": "GET", "scheme": "http", "authority": "a", "path": "/" }),
            )
            .await;
        match refused {
            OpOutcome::NodeFailed { code, .. } => assert_eq!(code, "ERR_HTTP2_INVALID_SESSION"),
            other => panic!("{other:?}"),
        }
        assert!(h2_session::destroy(&reg.sessions, session));
        assert_eq!(h2_session::open_count(&reg.sessions), 0);
    })
    .await;
}

/// An origin that resets every stream with REFUSED_STREAM.
async fn refusing_origin() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Ok(mut connection) = h2::server::handshake(stream).await else {
                    return;
                };
                while let Some(Ok((_request, mut respond))) = connection.accept().await {
                    respond.send_reset(h2::Reason::REFUSED_STREAM);
                }
            });
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_the_peer_resets_fails_with_its_code() {
    within(async {
        let reg = Reg::new();
        let port = refusing_origin().await;
        let socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let session = reg.open(reg.pump(socket)).await;
        let outcome = reg
            .request(
                session,
                json!({ "method": "GET", "scheme": "http", "authority": "x", "path": "/" }),
            )
            .await;
        match outcome {
            OpOutcome::NodeFailed {
                code,
                message,
                errno,
                ..
            } => {
                assert_eq!(code, "ERR_HTTP2_STREAM_ERROR");
                assert_eq!(
                    message,
                    "Stream closed with error code NGHTTP2_REFUSED_STREAM"
                );
                assert_eq!(errno, Some(7), "the h2 code, for the stream's rstCode");
            }
            other => panic!("{other:?}"),
        }
        assert!(h2_session::destroy(&reg.sessions, session));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_that_is_not_h2_ends_the_session_with_the_h2_error() {
    within(async {
        let reg = Reg::new();
        // An HTTP/1.1 server: its answer to the preface is not a frame.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            let _ = stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
                .await;
            // Held open: a close with unread bytes resets the connection,
            // and a reset can overtake the answer. The h2 layer's own
            // verdict on the answer is what is under test.
            while matches!(stream.read(&mut buf).await, Ok(n) if n > 0) {}
        });
        let socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let session = reg.open(reg.pump(socket)).await;
        let outcome = reg
            .request(
                session,
                json!({ "method": "GET", "scheme": "http", "authority": "x", "path": "/" }),
            )
            .await;
        match outcome {
            OpOutcome::NodeFailed { code, .. } => assert_eq!(code, "ERR_HTTP2_SESSION_FAILED"),
            other => panic!("{other:?}"),
        }
        let ended = payload(h2_session::wait(reg.sessions.clone(), session).await);
        assert!(ended["error"]["code"].is_u64(), "{ended}");
        assert_eq!(ended["error"]["remote"], false, "{ended}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gone_pipe_or_session_fails_without_dialling_anything() {
    within(async {
        let reg = Reg::new();
        match h2_session::open(reg.sessions.clone(), reg.pipes.clone(), 999, reg.ids.clone()).await {
            OpOutcome::Failed(text) => assert!(text.contains("pipe 999 is gone"), "{text}"),
            other => panic!("{other:?}"),
        }
        // A streamed body for a session that does not exist is released.
        let (handle, _tx) = reg.channel();
        match reg
            .request(
                42,
                json!({ "method": "POST", "scheme": "http", "authority": "x", "path": "/", "body_stream": handle }),
            )
            .await
        {
            OpOutcome::NodeFailed { code, .. } => assert_eq!(code, "ERR_HTTP2_INVALID_SESSION"),
            other => panic!("{other:?}"),
        }
        assert!(reg.outbound.lock().unwrap().get(&handle).is_none(), "the channel was released");
        assert_eq!(payload(h2_session::wait(reg.sessions.clone(), 42).await), json!({ "error": null }));
        assert!(!h2_session::close(&reg.sessions, 42));
        assert!(!h2_session::destroy(&reg.sessions, 42));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn destroying_a_session_ends_its_connection_and_the_pipe() {
    within(async {
        let reg = Reg::new();
        let (port, _, _) = h2c_origin().await;
        let socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let pipe_id = pipe::open(&reg.pipes, &reg.ids);
        // Pump only the inbound side here; the test reads what the session
        // writes itself.
        let session = reg.open(pipe_id).await;
        drop(socket);
        let waiting = tokio::spawn(h2_session::wait(reg.sessions.clone(), session));
        assert!(h2_session::destroy(&reg.sessions, session));
        assert_eq!(payload(waiting.await.unwrap()), json!({ "error": null }));
        // The consumer end went with the session: out drains the preface
        // hyper wrote, then ends.
        loop {
            match pipe::out(reg.pipes.clone(), pipe_id).await {
                OpOutcome::Bytes(_) => continue,
                OpOutcome::Done => break,
                other => panic!("{other:?}"),
            }
        }
        assert!(pipe::close(&reg.pipes, pipe_id));
    })
    .await;
}

// ---------------------------------------------------------------- ALPN

async fn tls_origin(alpn: &'static [&'static [u8]]) -> u16 {
    let acceptor = tls_acceptor(alpn);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Ok(mut tls) = acceptor.accept(stream).await {
                    let mut buf = [0u8; 16];
                    let _ = tls.read(&mut buf).await;
                }
            });
        }
    });
    port
}

async fn tls_alpn(port: u16, alpn: Vec<Vec<u8>>) -> Value {
    install_provider();
    let registry: oam_core::tls::TlsRegistry = Arc::new(Mutex::new(Default::default()));
    payload(
        oam_core::tls::tls_connect_pinned(
            registry,
            Arc::new(AtomicU64::new(1)),
            "localhost".to_string(),
            port,
            None,
            Some(TLS_TEST_CA_CERT.to_string()),
            true,
            None,
            true,
            None,
            None,
            std::time::Duration::from_millis(250),
            None,
            None,
            alpn,
        )
        .await,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn tls_connect_offers_the_alpn_protocols_it_is_given() {
    within(async {
        let h2_only = tls_origin(&[b"h2"]).await;
        let info = tls_alpn(h2_only, vec![b"h2".to_vec(), b"http/1.1".to_vec()]).await;
        assert_eq!(info["alpnProtocol"], "h2");
        assert_eq!(info["authorized"], true);

        // Nothing offered, nothing selected (node's alpnProtocol false).
        let info = tls_alpn(h2_only, Vec::new()).await;
        assert_eq!(info["alpnProtocol"], "");

        // The server's preference among what the client offered.
        let h1 = tls_origin(&[b"http/1.1"]).await;
        let info = tls_alpn(h1, vec![b"h2".to_vec(), b"http/1.1".to_vec()]).await;
        assert_eq!(info["alpnProtocol"], "http/1.1");
    })
    .await;
}

#[test]
fn alpn_names_are_bytes_of_one_to_255() {
    use oam_core::tls::parse_alpn_protocols;
    assert_eq!(
        parse_alpn_protocols(r#"["h2","http/1.1"]"#).unwrap(),
        vec![b"h2".to_vec(), b"http/1.1".to_vec()]
    );
    assert_eq!(
        parse_alpn_protocols("[\"\u{ff}x\"]").unwrap(),
        vec![vec![0xff, b'x']]
    );
    assert!(parse_alpn_protocols(r#"[""]"#).is_err());
    assert!(parse_alpn_protocols(&format!("[\"{}\"]", "a".repeat(256))).is_err());
    assert!(parse_alpn_protocols("[\"\u{100}\"]").is_err());
    assert!(parse_alpn_protocols("{}").is_err());
}
