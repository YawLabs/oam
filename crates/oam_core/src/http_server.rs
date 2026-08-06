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
use tokio::io::AsyncReadExt;
use tokio::sync::{Semaphore, mpsc, oneshot};

/// Per-request body cap (wave-1 buffered bodies).
const MAX_REQUEST_BODY: usize = 100 * 1024 * 1024;
/// After the per-request cap is hit we drain (discard) up to this many
/// additional bytes before returning the 413 response.  The drain serves
/// a single purpose: on Windows, dropping a TcpStream that still has
/// unread data in the kernel recv-buffer causes an immediate TCP RST.
/// That RST races with the 413 response bytes in the send-buffer, so the
/// client reads "connection reset" instead of "413".  Draining clears the
/// recv-buffer, the connection closes with a clean FIN, and the client
/// reads the 413 first.  The drain is capped so an adversarial client
/// cannot pin the connection indefinitely; anything beyond DRAIN_BUDGET
/// bytes above the cap is still rejected with RST (and the test body is
/// sized to stay under the drain ceiling -- see e2e.rs).
const DRAIN_BUDGET: usize = 16 * 1024 * 1024; // 16 MB post-cap drain
/// Aggregate cap on RETAINED request-body bytes across all in-flight
/// requests of the run — bounds the body-buffer memory a flood of uploads
/// can pin.
///
/// This is the SECOND line of defence, not the first. Streaming bodies are
/// bounded per-request by the chunk channel's capacity, so N concurrent
/// unread uploads pin N * capacity chunks regardless of how large the
/// bodies are. The budget is what bounds the AGGREGATE once N itself grows
/// large. Both are needed: backpressure alone scales with connection count,
/// and `MAX_CONNECTIONS` alone says nothing about bytes.
const DEFAULT_GLOBAL_BODY_BUDGET: usize = 512 * 1024 * 1024;

/// Parse `OAM_MAX_BODY_BYTES` into the aggregate body budget.
///
/// Same shape as `OAM_MAX_HEAP_MB`: unset, empty, non-numeric or `0` means
/// "use the default". Set it to match a container memory limit, or low to
/// exercise the load-shedding path.
fn global_body_budget() -> usize {
    static CACHED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("OAM_MAX_BODY_BYTES")
            .ok()
            .and_then(|raw| raw.trim().parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT_GLOBAL_BODY_BUDGET)
    })
}
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
    pub is_upgrade: bool,
    pub socket_handle: Option<u64>,
    pub remote_addr: Option<String>,
    pub remote_port: Option<u16>,
}

/// An inbound request body as the JS side will consume it.
///
/// Slice 1 of docs/design/streaming-bodies.md: every request is still
/// `Full`, so behavior is unchanged. The variant exists so the streaming
/// path has a seam to land on -- `Stream` will carry a chunk receiver and
/// the handler will be dispatched on headers rather than on last byte.
/// A queued request-body chunk that carries its own reservation against
/// GLOBAL_BODY_BUDGET and refunds it when dropped.
///
/// A drop guard rather than manual refunds: a chunk leaves the queue several
/// ways -- read by JS, discarded by `_dump`, dropped when the consumer
/// cancels, or dropped with the whole channel when the connection dies -- and
/// a refund missed on any one path leaks budget until the process restarts,
/// which fails CLOSED (every later upload rejected as busy).
pub struct BudgetedChunk {
    data: Vec<u8>,
    /// Held separately: `into_data` moves `data` out, and Drop still has to
    /// refund the original size.
    len: usize,
    state: std::sync::Arc<HttpState>,
}

impl BudgetedChunk {
    fn new(data: Vec<u8>, state: std::sync::Arc<HttpState>) -> Self {
        let len = data.len();
        Self { data, len, state }
    }

    /// Take the bytes. The reservation is still refunded when the husk drops.
    pub fn into_data(mut self) -> Vec<u8> {
        std::mem::take(&mut self.data)
    }
}

impl Drop for BudgetedChunk {
    fn drop(&mut self) {
        self.state.body_bytes.fetch_sub(self.len, Ordering::AcqRel);
    }
}

pub enum RequestBody {
    /// Collected up front, subject to MAX_REQUEST_BODY + GLOBAL_BODY_BUDGET.
    Full(Vec<u8>),
    /// Chunks as they arrive. The handler is dispatched on headers, so 413
    /// is no longer available -- exceeding MAX_REQUEST_BODY delivers Err on
    /// this channel instead, and the connection is torn down.
    ///
    /// Taken out for the duration of each read await and reinserted after,
    /// the same remove-await-reinsert the accept queue uses; the JS side is
    /// the single consumer.
    Stream(mpsc::Receiver<Result<BudgetedChunk, String>>),
    /// The receiver is checked out by an in-flight read. The entry STAYS in
    /// the registry so a miss can be told apart from "no such body" -- a
    /// bare removal made a checked-out stream look absent, and the buffered
    /// fallback then reported EOF while frames were still queued.
    StreamPending,
}

/// Outcome of checking out a streamed body receiver.
pub enum BodyCheckout {
    Ready(mpsc::Receiver<Result<BudgetedChunk, String>>),
    /// Another read holds it; the caller must NOT treat this as EOF.
    InFlight,
    /// No streamed body for this id (buffered, or already finished).
    Absent,
}

pub enum ResponseBody {
    Full(Vec<u8>),
    /// Chunk channel plus a drop-signal: the oneshot sender rides inside
    /// ChannelBody, so dropping the body (finished OR connection lost)
    /// resolves the paired stream_watch receiver.
    Stream(mpsc::Receiver<Vec<u8>>, oneshot::Sender<()>),
    /// JS destroyed the request without responding (req.destroy()): the
    /// connection is torn down instead of synthesizing a response, so the
    /// client observes a connection error (Node's socket-destroy semantics).
    Abort,
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
    /// request id -> request body (fetched once by JS).
    bodies: Mutex<HashMap<u64, RequestBody>>,
    /// response-stream id -> chunk sender (JS pushes, hyper drains).
    streams: Mutex<HashMap<u64, mpsc::Sender<Vec<u8>>>>,
    /// response-stream id -> resolves when hyper drops the response body
    /// (normal completion OR connection loss). httpStreamClosed takes the
    /// receiver; JS tells the two cases apart via its own finished flag.
    stream_watch: Mutex<HashMap<u64, oneshot::Receiver<()>>>,
    /// Retained request-body bytes across all in-flight requests; reserved
    /// when a body is buffered, refunded when the request finishes.
    body_bytes: AtomicUsize,
}

impl HttpState {
    fn next_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Sync (isolate-thread) helpers consumed by the engine natives.
    /// Take the fully-collected body. Returns None for a request whose body
    /// is being streamed (no such request yet -- see slice 2), so callers
    /// keep working unchanged when that variant lands.
    pub fn take_request_body(&self, id: u64) -> Option<Vec<u8>> {
        let taken = self
            .bodies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)?;
        match taken {
            RequestBody::Full(bytes) => Some(bytes),
            // Streamed: put it back, the caller wants the chunk ops.
            other => {
                self.bodies
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id, other);
                None
            }
        }
    }

    /// Take the chunk receiver for a streamed body (remove-await-reinsert).
    pub fn take_body_stream(&self, id: u64) -> BodyCheckout {
        let mut guard = self.bodies.lock().unwrap_or_else(|e| e.into_inner());
        match guard.remove(&id) {
            Some(RequestBody::Stream(rx)) => {
                // Leave a marker so the entry still EXISTS while we await --
                // a bare removal made a checked-out stream look absent, and
                // the buffered fallback then reported EOF mid-body.
                guard.insert(id, RequestBody::StreamPending);
                BodyCheckout::Ready(rx)
            }
            Some(RequestBody::StreamPending) => {
                guard.insert(id, RequestBody::StreamPending);
                BodyCheckout::InFlight
            }
            Some(other) => {
                guard.insert(id, other);
                BodyCheckout::Absent
            }
            None => BodyCheckout::Absent,
        }
    }

    /// Reinsert a receiver taken by `take_body_stream`. Dropped silently if
    /// the request finished while the read was in flight.
    pub fn put_body_stream(&self, id: u64, rx: mpsc::Receiver<Result<BudgetedChunk, String>>) {
        let mut guard = self.bodies.lock().unwrap_or_else(|e| e.into_inner());
        // Reinsert ONLY onto the StreamPending marker left by
        // take_body_stream. A missing entry means the body was cancelled or
        // the request torn down while this read was in flight; resurrecting
        // it here would leak the stream past the request's lifetime, and
        // dropping rx instead is what stops the pump.
        if let Some(slot @ RequestBody::StreamPending) = guard.get_mut(&id) {
            *slot = RequestBody::Stream(rx);
        }
    }

    /// Drop a streamed body outright (JS cancelled / destroyed the request).
    pub fn cancel_body_stream(&self, id: u64) {
        self.bodies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }

    pub fn respond_full(
        &self,
        id: u64,
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> bool {
        let Some(responder) = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
        else {
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
        let (closed_tx, closed_rx) = oneshot::channel::<()>();
        let stream_id = self.next_id();
        self.streams
            .lock()
            .expect("http streams lock")
            .insert(stream_id, tx);
        self.stream_watch
            .lock()
            .expect("http stream_watch lock")
            .insert(stream_id, closed_rx);
        let ok = responder
            .send(ResponseSpec {
                status,
                headers,
                body: ResponseBody::Stream(rx, closed_tx),
            })
            .is_ok();
        if ok {
            Some(stream_id)
        } else {
            self.end_stream(stream_id);
            None
        }
    }

    /// req.destroy() before a response was sent: hand hyper an Abort spec so
    /// the connection is dropped without a response. Returns false when the
    /// exchange already responded (or never existed) -- a harmless no-op.
    pub fn abort_request(&self, id: u64) -> bool {
        let Some(responder) = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
        else {
            return false;
        };
        responder
            .send(ResponseSpec {
                status: 0,
                headers: Vec::new(),
                body: ResponseBody::Abort,
            })
            .is_ok()
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
        // Reap the watch entry if no httpStreamClosed op ever took it (the
        // oam.serve path and pre-watcher callers never do).
        self.stream_watch
            .lock()
            .expect("http stream_watch lock")
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

/// hyper Body over the JS-pushed chunk channel. `_closed_tx` is never sent
/// on: its DROP (body finished or connection torn down) is the signal the
/// paired stream_watch receiver resolves on.
struct ChannelBody {
    rx: mpsc::Receiver<Vec<u8>>,
    _closed_tx: oneshot::Sender<()>,
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
        ResponseBody::Stream(rx, closed_tx) => ChannelBody {
            rx,
            _closed_tx: closed_tx,
        }
        .boxed(),
        // Intercepted in handle_request (returns Err(RequestAborted) before
        // building a response); never reaches the spec-to-response path.
        ResponseBody::Abort => http_body_util::Empty::new().boxed(),
    };
    builder.body(body).unwrap_or_else(|_| {
        hyper::Response::builder()
            .status(500)
            .body(http_body_util::Full::new(Bytes::from_static(b"oam: bad response spec")).boxed())
            .expect("static 500 builds")
    })
}

// ---- HTTP upgrade detection (raw-TCP peek, bypasses hyper) ----

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn is_connection_upgrade(buf: &[u8]) -> bool {
    let text = match std::str::from_utf8(buf) {
        Ok(t) => t,
        Err(_) => return false,
    };
    for line in text.split("\r\n") {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("connection:") && lower.contains("upgrade") {
            return true;
        }
    }
    false
}

type UpgradeHeaders = (String, String, Vec<(String, String)>);

fn parse_upgrade_headers(buf: &[u8]) -> Option<UpgradeHeaders> {
    let text = std::str::from_utf8(buf).ok()?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let uri = parts.next()?.to_string();

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some(colon) = line.find(':') {
            let name = line[..colon].to_string();
            let value = line[colon + 1..].trim().to_string();
            headers.push((name, value));
        }
    }
    Some((method, uri, headers))
}

/// Bind + spawn the accept loop. Resolves Json {serverId, port}.
pub async fn http_serve(
    state: Arc<HttpState>,
    tcp: super::tcp::TcpRegistry,
    tcp_ids: Arc<std::sync::atomic::AtomicU64>,
    host: String,
    port: u16,
    // Dispatch the JS handler on headers and stream the body (slice 2 of
    // docs/design/streaming-bodies.md). Off = today's buffered behavior.
    stream_request_body: bool,
) -> super::OpOutcome {
    let listener = match tokio::net::TcpListener::bind((host.as_str(), port)).await {
        Ok(listener) => listener,
        Err(e) => return super::OpOutcome::Failed(format!("listen {host}:{port}: {e}")),
    };
    let local_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    let server_id = state.next_id();
    let (queue_tx, queue_rx) = mpsc::channel::<IncomingRequest>(64);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    state
        .servers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            server_id,
            ServerEntry {
                queue: Some(queue_rx),
                shutdown: Some(shutdown_tx),
            },
        );

    let accept_state = state.clone();
    let accept_tcp = tcp;
    let accept_tcp_ids = tcp_ids;
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                accepted = listener.accept() => {
                    let Ok((mut stream, peer)) = accepted else { continue };
                    let Ok(permit) = connections.clone().try_acquire_owned() else {
                        drop(stream);
                        continue;
                    };

                    // Peek for Connection: Upgrade before hyper takes ownership.
                    let mut peek_buf = [0u8; 8192];
                    let is_upgrade = match stream.peek(&mut peek_buf).await {
                        Ok(n) if n > 16 => {
                            find_header_end(&peek_buf[..n]).is_some()
                                && is_connection_upgrade(&peek_buf[..n])
                        }
                        _ => false,
                    };

                    if is_upgrade {
                        let n = match stream.peek(&mut peek_buf).await {
                            Ok(n) => n,
                            Err(_) => continue,
                        };
                        let Some(hdr_end) = find_header_end(&peek_buf[..n]) else {
                            continue;
                        };
                        let consume = hdr_end + 4;
                        let mut discard = vec![0u8; consume];
                        if stream.read_exact(&mut discard).await.is_err() {
                            continue;
                        }
                        if let Some((method, uri, headers)) =
                            parse_upgrade_headers(&discard[..hdr_end])
                        {
                            let id = accept_state.next_id();
                            let handle =
                                accept_tcp_ids.fetch_add(1, Ordering::Relaxed);
                            let (reader, writer) = stream.into_split();
                            accept_tcp
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .register_stream(handle, reader, writer);
                            let _ = queue_tx
                                .send(IncomingRequest {
                                    id,
                                    method,
                                    uri,
                                    headers,
                                    is_upgrade: true,
                                    socket_handle: Some(handle),
                                    remote_addr: Some(peer.ip().to_string()),
                                    remote_port: Some(peer.port()),
                                })
                                .await;
                        }
                        drop(permit);
                        continue;
                    }

                    // Normal HTTP: hand to hyper.
                    let conn_state = accept_state.clone();
                    let conn_queue = queue_tx.clone();
                    let conn_stream_bodies = stream_request_body;
                    let mut conn_shutdown = shutdown_rx.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        let io = hyper_util::rt::TokioIo::new(stream);
                        let service = hyper::service::service_fn(move |req| {
                            handle_request(
                                conn_state.clone(),
                                conn_queue.clone(),
                                req,
                                conn_stream_bodies, // per-server opt-in
                            )
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
    /// Set once the request has been handed to the JS accept queue. Until
    /// then no JS reap path can exist, so Drop must remove the body entry
    /// itself (queue send failed, or the future was cancelled mid-send).
    dispatched: bool,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.state
            .pending
            .lock()
            .expect("http pending lock")
            .remove(&self.id);
        let mut bodies = self.state.bodies.lock().expect("http bodies lock");
        match bodies.get(&self.id) {
            // Streamed bodies on a DISPATCHED request outlive this guard:
            // handle_request returns the moment JS sends response headers --
            // or is cancelled by a client disconnect -- while the handler
            // may still be mid-read, and an aborted upload must surface the
            // pump's queued error to the consumer rather than vanish into a
            // clean EOF. The JS side reaps instead: a terminal read
            // (EOF/error) in the read op, _dump for a never-consumed body,
            // and _destroy's cancel. A request JS never learned about has
            // no JS reaper, so it IS removed here.
            Some(RequestBody::Stream(_) | RequestBody::StreamPending) if self.dispatched => {}
            Some(_) => {
                bodies.remove(&self.id);
            }
            None => {}
        }
        drop(bodies);
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

/// Collect up to `MAX_REQUEST_BODY` bytes from `body`, then drain up to
/// `DRAIN_BUDGET` more (discarding them) before returning.
///
/// The drain step is what makes 413 reliable on Windows.  Without it,
/// closing a TcpStream with unread kernel recv-buffer data sends a TCP RST
/// instead of FIN.  The RST races with the 413 bytes in the send-buffer;
/// the client may read "connection reset" instead of "413".  Draining the
/// recv-buffer lets the OS close the connection gracefully (FIN) so the
/// 413 response lands first.
///
/// Returns `Ok(bytes)` when the body fit, `Err(())` when it exceeded the
/// cap (the drain has already completed by the time `Err` is returned).
/// Feed request chunks to the JS side as they arrive, enforcing
/// MAX_REQUEST_BODY cumulatively. Exceeding it sends Err (the JS request
/// stream errors) rather than a 413, because the handler was dispatched on
/// headers and may already have responded. Ends by dropping the sender, which
/// the reader sees as EOF.
async fn pump_request_body(
    mut body: hyper::body::Incoming,
    chunk_tx: mpsc::Sender<Result<BudgetedChunk, String>>,
    state: std::sync::Arc<HttpState>,
) {
    use http_body_util::BodyExt;
    let mut total: usize = 0;
    while let Some(frame) = body.frame().await {
        let frame = match frame {
            Ok(f) => f,
            Err(e) => {
                let _ = chunk_tx.send(Err(format!("request body: {e}"))).await;
                return;
            }
        };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        total += data.len();
        if total > MAX_REQUEST_BODY {
            let _ = chunk_tx
                .send(Err("request body too large".to_string()))
                .await;
            return;
        }
        // Charge the GLOBAL budget for the bytes about to be queued. This is
        // the per-chunk accounting the buffered path's comment promised and
        // that did not exist: streaming is always on, so the buffered
        // reservation was never reached and GLOBAL_BODY_BUDGET was charged
        // NOWHERE. Each request was capped at MAX_REQUEST_BODY, but N
        // concurrent uploads were bounded only by N.
        let len = data.len();
        let prev = state.body_bytes.fetch_add(len, Ordering::AcqRel);
        if prev + len > global_body_budget() {
            state.body_bytes.fetch_sub(len, Ordering::AcqRel);
            // The handler was dispatched on headers, so 503 is no longer
            // available; the consumer sees the error instead.
            let _ = chunk_tx.send(Err("server is busy".to_string())).await;
            return;
        }
        // send() awaits when the channel is full: that IS the backpressure.
        // Err means the consumer is gone, so stop reading the socket -- the
        // chunk drops here and refunds itself.
        if chunk_tx
            .send(Ok(BudgetedChunk::new(
                data.to_vec(),
                std::sync::Arc::clone(&state),
            )))
            .await
            .is_err()
        {
            return;
        }
    }
}

async fn collect_body(mut body: hyper::body::Incoming) -> Result<bytes::Bytes, ()> {
    use bytes::BufMut;
    let mut buf = bytes::BytesMut::new();
    let mut over_cap = false;
    let mut drained: usize = 0;

    loop {
        let frame = match body.frame().await {
            Some(Ok(f)) => f,
            Some(Err(_)) | None => break,
        };
        let Some(chunk) = frame.into_data().ok() else {
            // Trailers and other frame types: ignore, keep going.
            continue;
        };
        if over_cap {
            // Drain phase: discard bytes up to DRAIN_BUDGET to let the
            // kernel recv-buffer clear so close() can use FIN, not RST.
            drained += chunk.len();
            if drained >= DRAIN_BUDGET {
                break;
            }
        } else {
            buf.put(chunk);
            if buf.len() > MAX_REQUEST_BODY {
                over_cap = true;
                buf.clear(); // release buffered memory immediately
            }
        }
    }

    if over_cap { Err(()) } else { Ok(buf.freeze()) }
}

/// Service-level error returned only for ResponseBody::Abort: hyper drops
/// the connection without writing a response, so the client sees a reset --
/// Node's req.destroy() semantics. Every other path still answers with a
/// synthesized status.
#[derive(Debug)]
struct RequestAborted;

impl std::fmt::Display for RequestAborted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request aborted by handler")
    }
}

impl std::error::Error for RequestAborted {}

async fn handle_request(
    state: Arc<HttpState>,
    queue: mpsc::Sender<IncomingRequest>,
    req: hyper::Request<hyper::body::Incoming>,
    stream_request_body: bool,
) -> Result<hyper::Response<BoxedBody>, RequestAborted> {
    let (parts, body) = req.into_parts();
    // Collect the body, enforcing MAX_REQUEST_BODY.  When the cap is hit we
    // drain up to DRAIN_BUDGET additional bytes before returning 413.
    //
    // Why drain?  On Windows, dropping a TcpStream that still has unread
    // data in the kernel recv-buffer triggers an immediate TCP RST.  The RST
    // races with the 413 response bytes sitting in the send-buffer, so the
    // client reads "connection reset" instead of "413 Request Entity Too
    // Large".  Draining empties the recv-buffer so the connection can close
    // with a clean FIN and the client reliably reads the status line first.
    // Streamed: dispatch on headers and pump chunks behind the request.
    // 413 is unavailable from here on (the handler may already be
    // responding), so the cap becomes an Err delivered on the chunk channel.
    // A body that DECLARES itself over the cap is rejected before the
    // handler is dispatched, so 413 (and the Windows drain behind it) still
    // works exactly as it did. Only an undeclared body -- chunked, or a
    // lying Content-Length -- can exceed mid-stream, and that is the case
    // the pump turns into a stream error because the handler may already
    // have responded.
    let declared_oversize = parts
        .headers
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|len| len > MAX_REQUEST_BODY);
    let (collected, body_stream) = if stream_request_body && !declared_oversize {
        (bytes::Bytes::new(), Some(body))
    } else {
        let collected = match collect_body(body).await {
            Ok(bytes) => bytes,
            Err(()) => return Ok(status_body(413, b"request body too large")),
        };
        (collected, None)
    };
    // Reserve the retained bytes; refund (RequestGuard) on completion. A
    // streamed body reserves nothing here -- it is charged per outstanding
    // chunk instead, so a prompt consumer is not billed for the whole upload.
    let body_len = collected.len();
    let prev = state.body_bytes.fetch_add(body_len, Ordering::AcqRel);
    if prev + body_len > global_body_budget() {
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
    if let Some(body) = body_stream {
        // Bounded: an unconsumed body applies backpressure to hyper rather
        // than growing without limit. This is the memory ceiling that
        // replaces the buffered path's byte reservation.
        let (chunk_tx, chunk_rx) = mpsc::channel::<Result<BudgetedChunk, String>>(8);
        state
            .bodies
            .lock()
            .expect("http bodies lock")
            .insert(id, RequestBody::Stream(chunk_rx));
        tokio::spawn(pump_request_body(
            body,
            chunk_tx,
            std::sync::Arc::clone(&state),
        ));
    } else {
        state
            .bodies
            .lock()
            .expect("http bodies lock")
            .insert(id, RequestBody::Full(collected.to_vec()));
    }
    // From here on, every exit cleans up — including a cancelled future
    // if the client disconnects while the handler runs — and refunds the
    // reserved body bytes.
    let mut guard = RequestGuard {
        state: state.clone(),
        id,
        reserved: body_len,
        dispatched: false,
    };

    let sent = queue
        .send(IncomingRequest {
            id,
            method: parts.method.as_str().to_string(),
            uri,
            headers,
            is_upgrade: false,
            socket_handle: None,
            remote_addr: None,
            remote_port: None,
        })
        .await;
    if sent.is_err() {
        return Ok(hyper::Response::builder()
            .status(503)
            .body(http_body_util::Full::new(Bytes::from_static(b"server is closing")).boxed())
            .expect("static 503 builds"));
    }
    guard.dispatched = true;

    match rx.await {
        Ok(ResponseSpec {
            body: ResponseBody::Abort,
            ..
        }) => Err(RequestAborted),
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

/// Bind a TLS-wrapped HTTP server. Same as http_serve but each accepted
/// connection goes through a TLS handshake before reaching hyper. The
/// request/response lifecycle is identical (shared HttpState, same ops).
pub async fn https_serve(
    state: Arc<HttpState>,
    host: String,
    port: u16,
    cert_pem: String,
    key_pem: String,
) -> super::OpOutcome {
    let tls_config = match crate::tls::build_server_config(&cert_pem, &key_pem) {
        Ok(c) => c,
        Err(e) => return super::OpOutcome::Failed(format!("https tls config: {e}")),
    };
    let acceptor = tokio_rustls::TlsAcceptor::from(tls_config);

    let listener = match tokio::net::TcpListener::bind((host.as_str(), port)).await {
        Ok(listener) => listener,
        Err(e) => return super::OpOutcome::Failed(format!("listen {host}:{port}: {e}")),
    };
    let local_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    let server_id = state.next_id();
    let (queue_tx, queue_rx) = mpsc::channel::<IncomingRequest>(64);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    state
        .servers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
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
                    let Ok(permit) = connections.clone().try_acquire_owned() else {
                        drop(stream);
                        continue;
                    };
                    let conn_acceptor = acceptor.clone();
                    let conn_state = accept_state.clone();
                    let conn_queue = queue_tx.clone();
                    let mut conn_shutdown = shutdown_rx.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        let tls_stream = match conn_acceptor.accept(stream).await {
                            Ok(s) => s,
                            Err(_) => return, // handshake failed, drop connection
                        };
                        let io = hyper_util::rt::TokioIo::new(tls_stream);
                        let service = hyper::service::service_fn(move |req| {
                            handle_request(
                                conn_state.clone(),
                                conn_queue.clone(),
                                req,
                                false, // TLS: buffered until a later slice
                            )
                        });
                        let conn = hyper::server::conn::http1::Builder::new()
                            .serve_connection(io, service);
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
    });

    super::OpOutcome::Json(
        serde_json::json!({ "serverId": server_id, "port": local_port }).to_string(),
    )
}

/// Long-poll the next request. Json metadata, or Done when the server
/// closed (queue drained + senders dropped).
pub async fn http_accept(state: Arc<HttpState>, server_id: u64) -> super::OpOutcome {
    let (queue, mut shutdown_rx) = {
        let mut guard = state.servers.lock().expect("http servers lock");
        match guard.get_mut(&server_id) {
            Some(entry) => (
                entry.queue.take(),
                entry.shutdown.as_ref().map(|tx| tx.subscribe()),
            ),
            None => (None, None),
        }
    };
    let Some(mut queue) = queue else {
        // Server was already closed (close_server removed the entry) or
        // another accept is in flight.  Either way, signal Done so the JS
        // accept loop exits cleanly instead of surfacing an unhandled
        // rejection.
        return super::OpOutcome::Done;
    };
    // Wait for the next request OR a server.close() shutdown. Idle keep-alive
    // connection tasks hold queue_tx clones that can outlive graceful_shutdown,
    // so queue.recv() alone may park forever after close(); the shutdown watch
    // lets accept return Done promptly, matching Node's closeIdleConnections.
    let next = match shutdown_rx {
        Some(ref mut sd) => {
            tokio::select! {
                n = queue.recv() => n,
                _ = sd.changed() => None,
            }
        }
        None => queue.recv().await,
    };
    if let Some(entry) = state
        .servers
        .lock()
        .expect("http servers lock")
        .get_mut(&server_id)
    {
        entry.queue = Some(queue);
    }
    match next {
        Some(request) => {
            let mut meta = serde_json::json!({
                "requestId": request.id,
                "method": request.method,
                "uri": request.uri,
                "headers": request.headers,
            });
            if request.is_upgrade {
                meta["isUpgrade"] = serde_json::json!(true);
                meta["socketHandle"] = serde_json::json!(request.socket_handle);
                if let Some(addr) = &request.remote_addr {
                    meta["remoteAddress"] = serde_json::json!(addr);
                }
                if let Some(port) = request.remote_port {
                    meta["remotePort"] = serde_json::json!(port);
                }
            }
            super::OpOutcome::Json(meta.to_string())
        }
        None => super::OpOutcome::Done,
    }
}

/// HTTP/2 connection preface: `PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n` (24 bytes).
/// If the first bytes on the wire match this, the connection is h2c
/// (prior-knowledge HTTP/2). Otherwise, treat it as HTTP/1.1 — exactly
/// what Node's `http2.createServer()` does: accept both protocols.
const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// Bind + spawn an HTTP/2 cleartext (h2c) accept loop. Same request/response
/// lifecycle as http_serve — shared HttpState, same accept/respond ops.
///
/// Each connection auto-detects the protocol: if the client sends the HTTP/2
/// connection preface it runs through hyper's http2 builder, otherwise it
/// falls back to the http1 builder. This matches Node's `http2.createServer()`
/// semantics: h2c with prior knowledge AND HTTP/1.1 clients both work.
pub async fn http2_serve(state: Arc<HttpState>, host: String, port: u16) -> super::OpOutcome {
    let listener = match tokio::net::TcpListener::bind((host.as_str(), port)).await {
        Ok(listener) => listener,
        Err(e) => return super::OpOutcome::Failed(format!("listen {host}:{port}: {e}")),
    };
    let local_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    let server_id = state.next_id();
    let (queue_tx, queue_rx) = mpsc::channel::<IncomingRequest>(64);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    state
        .servers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
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
                    let Ok(permit) = connections.clone().try_acquire_owned() else {
                        drop(stream);
                        continue;
                    };
                    let conn_state = accept_state.clone();
                    let conn_queue = queue_tx.clone();
                    let mut conn_shutdown = shutdown_rx.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        // Peek the first bytes to detect HTTP/2 prior-knowledge.
                        // The preface is 24 bytes; TCP segmentation may deliver
                        // fewer on the first peek. Retry a few times with a
                        // short wait before falling back to HTTP/1.1.
                        let mut peek_buf = [0u8; 24];
                        let mut is_h2 = false;
                        for _ in 0..3u8 {
                            match stream.peek(&mut peek_buf).await {
                                Ok(n) if n >= H2_PREFACE.len() => {
                                    is_h2 = peek_buf[..H2_PREFACE.len()] == *H2_PREFACE;
                                    break;
                                }
                                Ok(n) if n > 0 && n < H2_PREFACE.len() => {
                                    // Partial read: if the first few bytes match
                                    // the preface prefix, wait for more data.
                                    if peek_buf[..n] == H2_PREFACE[..n] {
                                        tokio::time::sleep(Duration::from_millis(5)).await;
                                        continue;
                                    }
                                    // Definitely not h2.
                                    break;
                                }
                                _ => break,
                            }
                        }

                        let io = hyper_util::rt::TokioIo::new(stream);
                        let service = hyper::service::service_fn(move |req| {
                            handle_request(
                                conn_state.clone(),
                                conn_queue.clone(),
                                req,
                                false, // http2: buffered until a later slice
                            )
                        });

                        if is_h2 {
                            let conn = hyper::server::conn::http2::Builder::new(
                                hyper_util::rt::TokioExecutor::new(),
                            )
                            .serve_connection(io, service);
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
                        } else {
                            let conn = hyper::server::conn::http1::Builder::new()
                                .serve_connection(io, service);
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
                        }
                    });
                }
            }
        }
    });

    super::OpOutcome::Json(
        serde_json::json!({ "serverId": server_id, "port": local_port }).to_string(),
    )
}

/// Backpressured chunk push for streaming responses. A bounded timeout is
/// the half-open backstop: when a client stops reading (or vanishes
/// without a reset the OS has noticed yet), hyper stops draining the body,
/// the channel fills, and `send` would park forever — wedging the JS pump.
/// The timeout ends the stream so the pump always makes progress.
/// Resolves when hyper drops the response body for `stream_id` -- normal
/// completion after end_stream, OR the client tearing the connection down
/// mid-stream. JS distinguishes the cases by whether it already finished
/// the response; the unfinished case surfaces Node's 'close'-without-
/// 'finish' shape on the ServerResponse.
pub async fn http_stream_closed(state: Arc<HttpState>, stream_id: u64) -> super::OpOutcome {
    let watch = state
        .stream_watch
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&stream_id);
    let Some(rx) = watch else {
        // Stream already ended (or never existed): closed by definition.
        return super::OpOutcome::Done;
    };
    // Err(RecvError) IS the signal: the sender rides inside ChannelBody and
    // is dropped, never sent on.
    let _ = rx.await;
    super::OpOutcome::Done
}

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

#[cfg(test)]
mod budget_tests {
    use super::*;

    /// The global body budget is the only thing bounding queued request-body
    /// bytes ACROSS requests -- per-request backpressure caps each channel at
    /// 8 chunks, but nothing capped the aggregate. These assert the accounting
    /// the streaming path now performs, because the failure modes are silent
    /// in both directions: under-charging leaves the flood protection off (how
    /// it shipped), and a missed refund fails CLOSED, rejecting every later
    /// upload as "busy" until the process restarts.
    fn state() -> std::sync::Arc<HttpState> {
        std::sync::Arc::new(HttpState::default())
    }

    #[test]
    fn chunk_charges_on_create_and_refunds_on_drop() {
        let state = state();
        assert_eq!(state.body_bytes.load(Ordering::Acquire), 0);
        // The pump charges before constructing; the chunk owns the refund.
        state.body_bytes.fetch_add(1024, Ordering::AcqRel);
        let chunk = BudgetedChunk::new(vec![7u8; 1024], std::sync::Arc::clone(&state));
        assert_eq!(state.body_bytes.load(Ordering::Acquire), 1024);
        drop(chunk);
        assert_eq!(
            state.body_bytes.load(Ordering::Acquire),
            0,
            "dropping a queued chunk must refund its reservation"
        );
    }

    #[test]
    fn into_data_yields_the_bytes_and_still_refunds() {
        let state = state();
        state.body_bytes.fetch_add(4, Ordering::AcqRel);
        let chunk = BudgetedChunk::new(vec![1, 2, 3, 4], std::sync::Arc::clone(&state));
        let data = chunk.into_data();
        assert_eq!(data, vec![1, 2, 3, 4], "the reader must get the real bytes");
        assert_eq!(
            state.body_bytes.load(Ordering::Acquire),
            0,
            "into_data moves the bytes out; the husk still refunds"
        );
    }

    #[test]
    fn budget_rejects_only_past_the_ceiling() {
        let state = state();
        // Just under: admitted.
        let under = global_body_budget() - 1;
        let prev = state.body_bytes.fetch_add(under, Ordering::AcqRel);
        assert!(
            prev + under <= global_body_budget(),
            "under budget is admitted"
        );
        // The next byte crosses it, which is what the pump refuses.
        let prev = state.body_bytes.fetch_add(2, Ordering::AcqRel);
        assert!(
            prev + 2 > global_body_budget(),
            "crossing GLOBAL_BODY_BUDGET must be detectable by the pump"
        );
        state.body_bytes.fetch_sub(2, Ordering::AcqRel);
        state.body_bytes.fetch_sub(under, Ordering::AcqRel);
        assert_eq!(state.body_bytes.load(Ordering::Acquire), 0);
    }
}
