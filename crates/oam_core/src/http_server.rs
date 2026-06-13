//! Inbound HTTP/1.1 server on hyper (already in-tree via reqwest).
//!
//! Flow: `http_serve` binds a tokio listener and spawns an accept loop;
//! each hyper request collects its body (wave-1: BUFFERED, capped — most
//! API payloads are small; streaming request bodies land later), parks a
//! oneshot responder, and pushes metadata onto the server's queue. JS
//! long-polls `http_accept` — the pending accept op is what keeps the
//! event loop alive while listening, exactly Node's semantics. Responses
//! come back either as one full buffer or as a CHANNEL BODY the JS side
//! pushes chunks into (the SSE/token-streaming path).
//!
//! Hardening (after the M2 safety fleet): a per-server CONNECTION CAP
//! refuses floods, a global RETAINED-BODY BUDGET bounds buffered upload
//! memory, the streaming push has a STALL TIMEOUT so a half-open client
//! can't wedge the pump, and close() does a GRACEFUL shutdown (in-flight
//! requests finish, keep-alive is disabled) instead of resetting live
//! connections.

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Frame;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc, oneshot};

/// Per-request body cap (wave-1 buffered bodies).
const MAX_REQUEST_BODY: usize = 100 * 1024 * 1024;
/// Aggregate cap on RETAINED request-body bytes across all in-flight
/// requests of the run — bounds the body-buffer memory a flood of uploads
/// can pin. (Per-request transient during collection is bounded by the
/// connection cap; truly bounding it needs streaming request bodies, the
/// next wave.)
const GLOBAL_BODY_BUDGET: usize = 512 * 1024 * 1024;
/// Concurrent-connection cap per server. New connections past this are
/// dropped (refused), not queued — a flood can't spawn unbounded tasks or
/// buffer unbounded bodies.
const MAX_CONNECTIONS: usize = 256;
/// A single streaming-response chunk that cannot be delivered within this
/// window means the consumer is gone or wedged (a half-open socket the OS
/// hasn't reset yet): end the stream so the JS pump never parks forever.
const STREAM_PUSH_TIMEOUT: Duration = Duration::from_secs(60);

pub struct IncomingRequest {
    pub id: u64,
    pub method: String,
    /// Path + query, as received.
    pub uri: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub enum ResponseBody {
    Full(Vec<u8>),
    Stream(mpsc::Receiver<Vec<u8>>),
}

pub struct ResponseSpec {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: ResponseBody,
}

struct ServerEntry {
    /// Taken out for the duration of each accept await (remove-await-
    /// reinsert; the JS accept loop is the single consumer).
    queue: Option<mpsc::Receiver<IncomingRequest>>,
    /// watch (not oneshot): every CONNECTION task selects on it too.
    /// Keep-alive sockets (a client pool can idle one for 90s) would
    /// otherwise hold queue_tx clones long after close, leaving the
    /// pending accept op pinning the event loop open.
    shutdown: Option<tokio::sync::watch::Sender<bool>>,
}

#[derive(Default)]
pub struct HttpState {
    next: AtomicU64,
    servers: Mutex<HashMap<u64, ServerEntry>>,
    /// request id -> the hyper-side responder waiting for JS.
    pending: Mutex<HashMap<u64, oneshot::Sender<ResponseSpec>>>,
    /// request id -> buffered request body (fetched once by JS).
    bodies: Mutex<HashMap<u64, Vec<u8>>>,
    /// response-stream id -> chunk sender (JS pushes, hyper drains).
    streams: Mutex<HashMap<u64, mpsc::Sender<Vec<u8>>>>,
    /// Retained request-body bytes across all in-flight requests; reserved
    /// when a body is buffered, refunded when the request finishes.
    body_bytes: AtomicUsize,
}

impl HttpState {
    fn next_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Sync (isolate-thread) helpers consumed by the engine natives.
    pub fn take_request_body(&self, id: u64) -> Option<Vec<u8>> {
        self.bodies.lock().expect("http bodies lock").remove(&id)
    }

    pub fn respond_full(
        &self,
        id: u64,
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> bool {
        let Some(responder) = self.pending.lock().expect("http pending lock").remove(&id) else {
            return false;
        };
        responder
            .send(ResponseSpec {
                status,
                headers,
                body: ResponseBody::Full(body),
            })
            .is_ok()
    }

    /// Start a streaming response; returns the stream handle JS pushes to.
    pub fn respond_stream(
        &self,
        id: u64,
        status: u16,
        headers: Vec<(String, String)>,
    ) -> Option<u64> {
        let responder = self
            .pending
            .lock()
            .expect("http pending lock")
            .remove(&id)?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>(16);
        let stream_id = self.next_id();
        self.streams
            .lock()
            .expect("http streams lock")
            .insert(stream_id, tx);
        let ok = responder
            .send(ResponseSpec {
                status,
                headers,
                body: ResponseBody::Stream(rx),
            })
            .is_ok();
        if ok { Some(stream_id) } else { None }
    }

    pub fn stream_sender(&self, stream_id: u64) -> Option<mpsc::Sender<Vec<u8>>> {
        self.streams
            .lock()
            .expect("http streams lock")
            .get(&stream_id)
            .cloned()
    }

    /// Dropping the sender ends the hyper body (clean EOF / final chunk).
    pub fn end_stream(&self, stream_id: u64) {
        self.streams
            .lock()
            .expect("http streams lock")
            .remove(&stream_id);
    }

    pub fn close_server(&self, server_id: u64) {
        if let Some(mut entry) = self
            .servers
            .lock()
            .expect("http servers lock")
            .remove(&server_id)
            && let Some(shutdown) = entry.shutdown.take()
        {
            let _ = shutdown.send(true);
        }
    }
}

/// hyper Body over the JS-pushed chunk channel.
struct ChannelBody {
    rx: mpsc::Receiver<Vec<u8>>,
}

impl hyper::body::Body for ChannelBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.rx.poll_recv(cx) {
            std::task::Poll::Ready(Some(chunk)) => {
                std::task::Poll::Ready(Some(Ok(Frame::data(Bytes::from(chunk)))))
            }
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

type BoxedBody = http_body_util::combinators::BoxBody<Bytes, std::convert::Infallible>;

fn spec_to_response(spec: ResponseSpec) -> hyper::Response<BoxedBody> {
    let mut builder = hyper::Response::builder().status(spec.status);
    for (name, value) in &spec.headers {
        builder = builder.header(name, value);
    }
    let body: BoxedBody = match spec.body {
        ResponseBody::Full(bytes) => http_body_util::Full::new(Bytes::from(bytes)).boxed(),
        ResponseBody::Stream(rx) => ChannelBody { rx }.boxed(),
    };
    builder.body(body).unwrap_or_else(|_| {
        hyper::Response::builder()
            .status(500)
            .body(http_body_util::Full::new(Bytes::from_static(b"oam: bad response spec")).boxed())
            .expect("static 500 builds")
    })
}

/// Bind + spawn the accept loop. Resolves Json {serverId, port}.
pub async fn http_serve(state: Arc<HttpState>, host: String, port: u16) -> super::OpOutcome {
    let listener = match tokio::net::TcpListener::bind((host.as_str(), port)).await {
        Ok(listener) => listener,
        Err(e) => return super::OpOutcome::Failed(format!("listen {host}:{port}: {e}")),
    };
    let local_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    let server_id = state.next_id();
    let (queue_tx, queue_rx) = mpsc::channel::<IncomingRequest>(64);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    state.servers.lock().expect("http servers lock").insert(
        server_id,
        ServerEntry {
            queue: Some(queue_rx),
            shutdown: Some(shutdown_tx),
        },
    );

    let accept_state = state.clone();
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                accepted = listener.accept() => {
                    let Ok((stream, _peer)) = accepted else { continue };
                    // Connection cap: refuse (drop) past the ceiling rather
                    // than queue — a flood can't spawn unbounded tasks or
                    // buffer unbounded bodies. The permit lives in the conn
                    // task and frees when the connection ends.
                    let Ok(permit) = connections.clone().try_acquire_owned() else {
                        drop(stream);
                        continue;
                    };
                    let conn_state = accept_state.clone();
                    let conn_queue = queue_tx.clone();
                    let mut conn_shutdown = shutdown_rx.clone();
                    tokio::spawn(async move {
                        let _permit = permit; // released when the task ends
                        let io = hyper_util::rt::TokioIo::new(stream);
                        let service = hyper::service::service_fn(move |req| {
                            handle_request(conn_state.clone(), conn_queue.clone(), req)
                        });
                        let conn = hyper::server::conn::http1::Builder::new()
                            .serve_connection(io, service);
                        // GRACEFUL shutdown on close(): disable keep-alive and
                        // let the IN-FLIGHT request finish (Node's
                        // server.close() semantics), instead of resetting it.
                        // An idle keep-alive connection just closes — so the
                        // queue_tx clone still drops promptly and the accept
                        // op isn't pinned.
                        let mut conn = std::pin::pin!(conn);
                        let mut shutting_down = false;
                        loop {
                            tokio::select! {
                                result = conn.as_mut() => {
                                    let _ = result;
                                    break;
                                }
                                _ = conn_shutdown.changed(), if !shutting_down => {
                                    shutting_down = true;
                                    conn.as_mut().graceful_shutdown();
                                }
                            }
                        }
                    });
                }
            }
        }
        // Last queue_tx drops with the loop + finished connections: the
        // pending accept op resolves Done and the JS loop exits.
    });

    super::OpOutcome::Json(
        serde_json::json!({ "serverId": server_id, "port": local_port }).to_string(),
    )
}

/// RAII cleanup tied to the REQUEST lifetime, not the body read. When
/// handle_request returns — handler responded, client disconnected (future
/// cancelled at any await), or queue-send failed — both the buffered body
/// and the pending responder are removed. Without this, a handler that
/// never reads the body (a GET route, a 401, a webhook branching on
/// headers) leaked the body forever: a remote, attacker-controlled DoS.
struct RequestGuard {
    state: Arc<HttpState>,
    id: u64,
    /// Body bytes this request reserved against the global budget; refunded
    /// on drop along with the map cleanup.
    reserved: usize,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.state
            .bodies
            .lock()
            .expect("http bodies lock")
            .remove(&self.id);
        self.state
            .pending
            .lock()
            .expect("http pending lock")
            .remove(&self.id);
        if self.reserved > 0 {
            self.state
                .body_bytes
                .fetch_sub(self.reserved, Ordering::AcqRel);
        }
    }
}

fn status_body(status: u16, text: &'static [u8]) -> hyper::Response<BoxedBody> {
    hyper::Response::builder()
        .status(status)
        .body(http_body_util::Full::new(Bytes::from_static(text)).boxed())
        .expect("static status body builds")
}

async fn handle_request(
    state: Arc<HttpState>,
    queue: mpsc::Sender<IncomingRequest>,
    req: hyper::Request<hyper::body::Incoming>,
) -> Result<hyper::Response<BoxedBody>, std::convert::Infallible> {
    let (parts, body) = req.into_parts();
    // Enforce per-request size cap; the global budget gate below is the
    // single source of truth for aggregate concurrency limits. Limited::new
    // erroring means "client sent more than MAX_REQUEST_BODY" -- 413, NOT
    // 503: the server isn't busy, the client's body is just too big.
    let collected = match http_body_util::Limited::new(body, MAX_REQUEST_BODY)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(_) => return Ok(status_body(413, b"request body too large")),
    };
    // Reserve the retained bytes; refund (RequestGuard) on completion.
    let body_len = collected.len();
    let prev = state.body_bytes.fetch_add(body_len, Ordering::AcqRel);
    if prev + body_len > GLOBAL_BODY_BUDGET {
        state.body_bytes.fetch_sub(body_len, Ordering::AcqRel);
        return Ok(status_body(503, b"server is busy"));
    }
    let id = state.next_id();
    let headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let uri = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| parts.uri.path().to_string());

    let (tx, rx) = oneshot::channel::<ResponseSpec>();
    state
        .pending
        .lock()
        .expect("http pending lock")
        .insert(id, tx);
    state
        .bodies
        .lock()
        .expect("http bodies lock")
        .insert(id, collected.to_vec());
    // From here on, every exit cleans up — including a cancelled future
    // if the client disconnects while the handler runs — and refunds the
    // reserved body bytes.
    let _guard = RequestGuard {
        state: state.clone(),
        id,
        reserved: body_len,
    };

    let sent = queue
        .send(IncomingRequest {
            id,
            method: parts.method.as_str().to_string(),
            uri,
            headers,
            body: Vec::new(), // delivered via take_request_body
        })
        .await;
    if sent.is_err() {
        return Ok(hyper::Response::builder()
            .status(503)
            .body(http_body_util::Full::new(Bytes::from_static(b"server is closing")).boxed())
            .expect("static 503 builds"));
    }

    match rx.await {
        Ok(spec) => Ok(spec_to_response(spec)),
        Err(_) => Ok(hyper::Response::builder()
            .status(500)
            .body(
                http_body_util::Full::new(Bytes::from_static(b"handler dropped the request"))
                    .boxed(),
            )
            .expect("static 500 builds")),
    }
}

/// Long-poll the next request. Json metadata, or Done when the server
/// closed (queue drained + senders dropped).
pub async fn http_accept(state: Arc<HttpState>, server_id: u64) -> super::OpOutcome {
    let queue = state
        .servers
        .lock()
        .expect("http servers lock")
        .get_mut(&server_id)
        .and_then(|entry| entry.queue.take());
    let Some(mut queue) = queue else {
        return super::OpOutcome::Failed(format!(
            "http server {server_id} is gone or accept is already pending"
        ));
    };
    let next = queue.recv().await;
    if let Some(entry) = state
        .servers
        .lock()
        .expect("http servers lock")
        .get_mut(&server_id)
    {
        entry.queue = Some(queue);
    }
    match next {
        Some(request) => super::OpOutcome::Json(
            serde_json::json!({
                "requestId": request.id,
                "method": request.method,
                "uri": request.uri,
                "headers": request.headers,
            })
            .to_string(),
        ),
        None => super::OpOutcome::Done,
    }
}

/// Backpressured chunk push for streaming responses. A bounded timeout is
/// the half-open backstop: when a client stops reading (or vanishes
/// without a reset the OS has noticed yet), hyper stops draining the body,
/// the channel fills, and `send` would park forever — wedging the JS pump.
/// The timeout ends the stream so the pump always makes progress.
pub async fn http_body_push(
    state: Arc<HttpState>,
    stream_id: u64,
    bytes: Vec<u8>,
) -> super::OpOutcome {
    let Some(sender) = state.stream_sender(stream_id) else {
        return super::OpOutcome::Failed(format!("http stream {stream_id} is gone"));
    };
    match tokio::time::timeout(STREAM_PUSH_TIMEOUT, sender.send(bytes)).await {
        Ok(Ok(())) => super::OpOutcome::Done,
        Ok(Err(_)) => {
            // Receiver dropped (hyper ended the response / connection gone).
            state.end_stream(stream_id);
            super::OpOutcome::Failed("client disconnected".to_string())
        }
        Err(_elapsed) => {
            // Stalled: consumer not reading. Drop the stream; the lingering
            // socket is reaped by the OS / graceful close.
            state.end_stream(stream_id);
            super::OpOutcome::Failed("stream stalled: client is not reading".to_string())
        }
    }
}
