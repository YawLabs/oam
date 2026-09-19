//! An HTTP/2 client session over a [`pipe`](super::pipe): what
//! `http2.connect` runs over the socket `net.connect` / `tls.connect` or the
//! caller's `createConnection` returned. The socket is the session's, as in
//! node: every stream of the session travels over it, so a `lookup` hook, a
//! 'lookup' / 'connect' listener or a `createConnection` that vetted (or
//! refused) the connection decides where every request goes.
//!
//! hyper's h2 client runs over the consumer end of the pipe; JS pumps the
//! other end to and from the socket. [`open`] handshakes (the preface and
//! SETTINGS go into the pipe; nothing waits for the peer) and spawns the
//! connection task; [`request`] opens one stream and resolves at its response
//! head with the fetch payload's shape, the body under a [`FetchBody`]
//! handle read with the fetch body ops (never decoded: node's http2 client
//! delivers the bytes as sent). A request body streams from JS through an
//! outbound body channel, so a response can arrive while the request is
//! still being written.
//!
//! A session lives until [`destroy`]. [`close`] drops the request sender, and
//! hyper then ends the connection gracefully (GOAWAY) once the open streams
//! are done; [`wait`] resolves when the connection task ends, with how it
//! ended.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use base64::Engine as _;
use bytes::Bytes;
use http::header::{HeaderName, HeaderValue};
use hyper::client::conn::http2::SendRequest;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};

use super::ReqBody;
use super::body::{FetchBodies, FetchBody, StreamSlot};
use super::pipe::{self, Pipes};
use super::transport::{channel_body, empty_body, full_body};
use crate::{OpOutcome, OutboundBodies};

/// Live sessions by id (ids from the runtime's shared handle allocator).
pub type H2Sessions = Arc<Mutex<HashMap<u64, H2Session>>>;

/// One session's state.
pub struct H2Session {
    /// Opens streams; `None` once [`close`] dropped it.
    sender: Option<SendRequest<ReqBody>>,
    /// The connection task.
    task: tokio::task::AbortHandle,
    /// How the connection ended, once it has.
    ended: tokio::sync::watch::Receiver<Option<serde_json::Value>>,
}

impl Drop for H2Session {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// nghttp2's name for an HTTP/2 error code, as node reports it
/// (`NGHTTP2_REFUSED_STREAM`); the number for a code it has no name for.
pub fn nghttp2_name(code: u32) -> String {
    let name = match code {
        0 => "NGHTTP2_NO_ERROR",
        1 => "NGHTTP2_PROTOCOL_ERROR",
        2 => "NGHTTP2_INTERNAL_ERROR",
        3 => "NGHTTP2_FLOW_CONTROL_ERROR",
        4 => "NGHTTP2_SETTINGS_TIMEOUT",
        5 => "NGHTTP2_STREAM_CLOSED",
        6 => "NGHTTP2_FRAME_SIZE_ERROR",
        7 => "NGHTTP2_REFUSED_STREAM",
        8 => "NGHTTP2_CANCEL",
        9 => "NGHTTP2_COMPRESSION_ERROR",
        10 => "NGHTTP2_CONNECT_ERROR",
        11 => "NGHTTP2_ENHANCE_YOUR_CALM",
        12 => "NGHTTP2_INADEQUATE_SECURITY",
        13 => "NGHTTP2_HTTP_1_1_REQUIRED",
        other => return other.to_string(),
    };
    name.to_string()
}

/// The h2 error in `error`'s source chain, if there is one.
fn find_h2<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a h2::Error> {
    let mut current = Some(error);
    while let Some(e) = current {
        if let Some(found) = e.downcast_ref::<h2::Error>() {
            return Some(found);
        }
        current = e.source();
    }
    None
}

/// How a connection ended, for [`wait`]: `{"error": null}` for a clean end,
/// else `{"error": {message, code?, goAway?, remote}}` with the h2 error code
/// when the peer (or hyper) named one.
fn ended_payload(result: Result<(), hyper::Error>) -> serde_json::Value {
    let error = match result {
        Ok(()) => return serde_json::json!({ "error": null }),
        Err(error) => error,
    };
    let mut detail = serde_json::json!({ "message": error.to_string() });
    if let Some(h2) = find_h2(&error) {
        if let Some(reason) = h2.reason() {
            detail["code"] = serde_json::Value::from(u32::from(reason));
        }
        detail["goAway"] = serde_json::Value::from(h2.is_go_away());
        detail["remote"] = serde_json::Value::from(h2.is_remote());
    }
    serde_json::json!({ "error": detail })
}

/// `http2SessionOpen(pipeId)`: an HTTP/2 client session over the consumer end
/// of pipe `pipeId`. Resolves with `{session}` once the preface and SETTINGS
/// are written into the pipe.
pub async fn open(
    sessions: H2Sessions,
    pipes: Pipes,
    pipe_id: u64,
    ids: Arc<AtomicU64>,
) -> OpOutcome {
    let Some(io) = pipe::take(&pipes, pipe_id) else {
        return OpOutcome::Failed(format!("http2SessionOpen: pipe {pipe_id} is gone"));
    };
    let mut builder = hyper::client::conn::http2::Builder::new(TokioExecutor::new());
    builder.timer(TokioTimer::new());
    let (sender, connection) = match builder.handshake::<_, ReqBody>(TokioIo::new(io)).await {
        Ok(handshake) => handshake,
        Err(e) => return OpOutcome::node_failed("ERR_HTTP2_ERROR", e.to_string()),
    };
    let (ended_tx, ended_rx) = tokio::sync::watch::channel(None);
    let task = tokio::spawn(async move {
        let result = connection.await;
        let _ = ended_tx.send(Some(ended_payload(result)));
    });
    let id = ids.fetch_add(1, Ordering::Relaxed);
    lock(&sessions).insert(
        id,
        H2Session {
            sender: Some(sender),
            task: task.abort_handle(),
            ended: ended_rx,
        },
    );
    OpOutcome::Json(serde_json::json!({ "session": id }).to_string())
}

/// The request JS sends for one stream (serde; unknown fields are ignored).
#[derive(serde::Deserialize)]
pub struct H2Request {
    pub method: String,
    pub scheme: String,
    pub authority: String,
    pub path: String,
    /// Regular headers, in order (no pseudo-headers).
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Handle into `OutboundBodies`: the body streams from JS.
    #[serde(default)]
    pub body_stream: Option<u64>,
    #[serde(default)]
    pub body_base64: Option<String>,
}

/// A JS string that holds bytes one per code point. Anything past U+00FF
/// cannot be sent.
fn latin1_bytes(text: &str, what: &str) -> Result<Vec<u8>, String> {
    text.chars()
        .map(|c| u8::try_from(u32::from(c)).map_err(|_| format!("invalid character in {what}")))
        .collect()
}

/// A header value as JS sees it: latin1, one code point per byte.
fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

fn build_request(req: &H2Request, body: ReqBody) -> Result<http::Request<ReqBody>, String> {
    let method = http::Method::from_bytes(req.method.as_bytes())
        .map_err(|_| format!("invalid method {:?}", req.method))?;
    let uri = http::Uri::builder()
        .scheme(req.scheme.as_str())
        .authority(req.authority.as_str())
        .path_and_query(req.path.as_str())
        .build()
        .map_err(|e| format!("invalid request target: {e}"))?;
    let mut request = http::Request::builder()
        .method(method)
        .uri(uri)
        .version(http::Version::HTTP_2)
        .body(body)
        .map_err(|e| e.to_string())?;
    for (name, value) in &req.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("invalid header name {name:?}"))?;
        let value = HeaderValue::from_bytes(&latin1_bytes(value, "a header value")?)
            .map_err(|_| format!("invalid value for header {name}"))?;
        request.headers_mut().append(name, value);
    }
    Ok(request)
}

/// A stream that produced no response. A stream the peer reset is
/// `ERR_HTTP2_STREAM_ERROR` carrying the h2 code as `errno` (JS closes the
/// stream with it, as node's onStreamClose does); anything else is the
/// connection failing under it, `ERR_HTTP2_SESSION_FAILED` -- JS leaves that
/// to the session, which reports the connection's end once, as node does,
/// and takes its streams down with it.
fn stream_error(error: &hyper::Error) -> OpOutcome {
    if let Some(h2) = find_h2(error)
        && h2.is_reset()
        && let Some(reason) = h2.reason()
    {
        let code = u32::from(reason);
        return OpOutcome::NodeFailed {
            code: "ERR_HTTP2_STREAM_ERROR".to_string(),
            message: format!("Stream closed with error code {}", nghttp2_name(code)),
            syscall: None,
            path: None,
            errno: i32::try_from(code).ok(),
            hostname: None,
            address: None,
            port: None,
        };
    }
    OpOutcome::node_failed("ERR_HTTP2_SESSION_FAILED", error.to_string())
}

/// `http2SessionRequest(session, requestJson)`: open one stream and resolve
/// at its response head with `{status, headers, bodyHandle, endStream}`.
pub async fn request(
    sessions: H2Sessions,
    id: u64,
    request: String,
    bodies: FetchBodies,
    ids: Arc<AtomicU64>,
    outbound: OutboundBodies,
) -> OpOutcome {
    let req: H2Request = match serde_json::from_str(&request) {
        Ok(req) => req,
        Err(e) => return OpOutcome::Failed(format!("http2SessionRequest: malformed request: {e}")),
    };
    // Claimed first, so every early return below releases the receiver.
    let mut slot = req
        .body_stream
        .map(|handle| StreamSlot::new(handle, outbound));
    let fail = |slot: &mut Option<StreamSlot>, outcome: OpOutcome| {
        if let Some(slot) = slot {
            slot.request_failed();
        }
        outcome
    };
    let sender = lock(&sessions)
        .get(&id)
        .and_then(|session| session.sender.clone());
    let Some(mut sender) = sender else {
        return fail(
            &mut slot,
            OpOutcome::node_failed(
                "ERR_HTTP2_INVALID_SESSION",
                "The session has been destroyed",
            ),
        );
    };
    let body = if let Some(slot) = &mut slot {
        match slot.take() {
            Some(receiver) => channel_body(receiver),
            None => {
                return OpOutcome::Failed(format!(
                    "http2SessionRequest: unknown body stream {}",
                    slot.handle()
                ));
            }
        }
    } else if let Some(encoded) = &req.body_base64 {
        match base64::engine::general_purpose::STANDARD.decode(encoded) {
            Ok(bytes) => full_body(Bytes::from(bytes)),
            Err(_) => {
                return OpOutcome::Failed("http2SessionRequest: malformed base64 body".into());
            }
        }
    } else {
        empty_body()
    };
    let request = match build_request(&req, body) {
        Ok(request) => request,
        Err(text) => return fail(&mut slot, OpOutcome::Failed(text)),
    };
    if let Err(e) = sender.ready().await {
        return fail(&mut slot, stream_error(&e));
    }
    let response = match sender.send_request(request).await {
        Ok(response) => response,
        Err(e) => return fail(&mut slot, stream_error(&e)),
    };
    let status = response.status();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(name, value)| (name.as_str().to_string(), latin1(value.as_bytes())))
        .collect();
    let body = response.into_body();
    let end_stream = hyper::body::Body::is_end_stream(&body);
    let handle = ids.fetch_add(1, Ordering::Relaxed);
    lock(&bodies).insert(handle, FetchBody::coded(body));
    OpOutcome::Json(
        serde_json::json!({
            "status": status.as_u16(),
            "headers": headers,
            "bodyHandle": handle,
            "endStream": end_stream,
        })
        .to_string(),
    )
}

/// `http2SessionWait(session)`: resolves when the connection ends, with
/// [`ended_payload`]'s shape; `{"error": null}` for a session already gone.
pub async fn wait(sessions: H2Sessions, id: u64) -> OpOutcome {
    let receiver = lock(&sessions)
        .get(&id)
        .map(|session| session.ended.clone());
    let Some(mut receiver) = receiver else {
        return OpOutcome::Json(serde_json::json!({ "error": null }).to_string());
    };
    loop {
        if let Some(ended) = receiver.borrow_and_update().clone() {
            return OpOutcome::Json(ended.to_string());
        }
        if receiver.changed().await.is_err() {
            // The session was destroyed: its task will never report.
            return OpOutcome::Json(serde_json::json!({ "error": null }).to_string());
        }
    }
}

/// `http2SessionClose(session)`: open no more streams; hyper ends the
/// connection with a GOAWAY once the open ones are done. True if the session
/// was open.
pub fn close(sessions: &H2Sessions, id: u64) -> bool {
    lock(sessions)
        .get_mut(&id)
        .and_then(|session| session.sender.take())
        .is_some()
}

/// `http2SessionDestroy(session)`: drop the session and abort its
/// connection (the pipe's consumer end goes with it). True if it was there.
pub fn destroy(sessions: &H2Sessions, id: u64) -> bool {
    let removed = lock(sessions).remove(&id);
    removed.is_some()
}

/// How many sessions are open.
pub fn open_count(sessions: &H2Sessions) -> usize {
    lock(sessions).len()
}
