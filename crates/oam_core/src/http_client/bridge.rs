//! An HTTP/1.1 client exchange over a byte stream that JS pumps to and from
//! any socket object: how `http.request` talks over the socket an agent's
//! `createConnection` returned (node's model: the request goes over THAT
//! socket, whatever connected it and wherever it goes).
//!
//! hyper's h1 client runs over one end of an in-memory duplex pipe. JS owns
//! the other end: it takes the bytes hyper writes (the request) with
//! [`out`] and writes them to the socket, and feeds what the socket reads
//! (the response) back with [`input`], ending it with [`input_end`] at the
//! socket's EOF. Both directions are bounded by the pipe, so a response
//! nobody reads stops the socket being read, and a slow socket stops hyper
//! writing. The response head resolves [`response`] with the fetch op's
//! payload shape; its body is a [`FetchBody`] read with the fetch body ops,
//! never decoded (node's http.request delivers the bytes as sent).
//!
//! A bridge lives until [`close`]: ClientRequest closes it when the response
//! ends, fails, or the request is destroyed. Closing aborts the connection
//! task, which ends a parked [`out`] read with EOF.
//!
//! [`request_sent`] tells JS how many of the bytes [`out`] hands it make up
//! the whole request, once hyper has written all of them: node emits
//! `'finish'` when the socket has written the last of those.
//!
//! Upgrades (`Connection: upgrade`) do not come here: JS writes and parses
//! those itself so the socket can be handed over after the 101.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

use base64::Engine as _;
use bytes::Bytes;
use http::header::{HeaderName, HeaderValue};
use hyper::body::{Frame, SizeHint};
use hyper_util::rt::TokioIo;
use tokio::io::{
    AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf, ReadHalf, WriteHalf,
};
use tokio::sync::watch;

use super::body::{FetchBodies, FetchBody, StreamSlot};
use super::transport::{channel_body, empty_body, full_body};
use super::{BoxError, ReqBody};
use crate::{OpOutcome, OutboundBodies};

/// Each direction of the in-memory pipe buffers at most this much.
const PIPE: usize = 64 * 1024;

/// The most one [`out`] read returns.
const OUT_CHUNK: usize = 64 * 1024;

/// Live bridges by id (ids from the runtime's shared handle allocator).
pub type Bridges = Arc<Mutex<HashMap<u64, Bridge>>>;

/// One exchange's state.
pub struct Bridge {
    /// hyper's end of the pipe and the request, until [`response`] starts
    /// the exchange.
    pending: Option<Pending>,
    /// JS's end, reading what hyper wrote. Out of the map while a read is
    /// parked (remove-await-reinsert).
    out: Option<ReadHalf<DuplexStream>>,
    /// JS's end, writing what the socket read. Out of the map while a write
    /// is parked.
    input: Option<WriteHalf<DuplexStream>>,
    /// The connection task, once started.
    task: Option<tokio::task::AbortHandle>,
    /// Set once hyper has written the whole request into the pipe: how many
    /// bytes it wrote (see [`RequestEnd`]).
    sent: watch::Receiver<Option<u64>>,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

struct Pending {
    io: DuplexStream,
    parts: http::request::Parts,
    body: Body,
    max_header_size: u64,
    sent: watch::Sender<Option<u64>>,
}

/// How [`request_sent`] learns that the request is all written.
///
/// hyper has no such signal, so two wrappers make one. The request body
/// ([`EndWatch`]) records that hyper has seen its end -- hyper asks
/// `is_end_stream` before it encodes the last bytes, or polls the end of a
/// streamed body -- and the pipe end hyper writes to ([`Counted`]) counts
/// the bytes and, at the first completed flush after that, publishes the
/// count. hyper flushes the pipe only once its own write buffer is empty,
/// so at that flush every request byte, the body's framing end included,
/// is in the pipe. Each bridge carries one request, so no later request's
/// bytes are in the count.
struct RequestEnd {
    body_ended: Arc<AtomicBool>,
    sent: watch::Sender<Option<u64>>,
}

/// hyper's end of the pipe, counting what hyper writes (see [`RequestEnd`]).
struct Counted {
    io: DuplexStream,
    written: u64,
    end: RequestEnd,
}

impl Counted {
    fn flushed(&mut self) {
        if self.written == 0 || !self.end.body_ended.load(Ordering::Acquire) {
            return;
        }
        let written = self.written;
        self.end.sent.send_if_modified(|sent| {
            if sent.is_some() {
                return false;
            }
            *sent = Some(written);
            true
        });
    }
}

impl AsyncRead for Counted {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_read(cx, buf)
    }
}

impl AsyncWrite for Counted {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.io).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &polled {
            this.written += *n as u64;
        }
        polled
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.io).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Ok(n)) = &polled {
            this.written += *n as u64;
        }
        polled
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.io).poll_flush(cx);
        if let Poll::Ready(Ok(())) = &polled {
            this.flushed();
        }
        polled
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

/// The request body, recording when hyper has reached its end (see
/// [`RequestEnd`]).
struct EndWatch {
    body: ReqBody,
    ended: Arc<AtomicBool>,
}

impl hyper::body::Body for EndWatch {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.body).poll_frame(cx);
        if let Poll::Ready(None) = &polled {
            this.ended.store(true, Ordering::Release);
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        let end = self.body.is_end_stream();
        if end {
            self.ended.store(true, Ordering::Release);
        }
        end
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

enum Body {
    Empty,
    Full(Bytes),
    Stream(StreamSlot),
}

impl Body {
    fn build(&mut self) -> Result<super::ReqBody, String> {
        match self {
            Body::Empty => Ok(empty_body()),
            Body::Full(bytes) => Ok(full_body(bytes.clone())),
            Body::Stream(slot) => match slot.take() {
                Some(receiver) => Ok(channel_body(receiver)),
                None => Err(format!(
                    "httpBridgeResponse: unknown body stream {}",
                    slot.handle()
                )),
            },
        }
    }

    fn request_failed(&mut self) {
        if let Body::Stream(slot) = self {
            slot.request_failed();
        }
    }
}

/// The request JS sends (serde; unknown fields are ignored).
#[derive(serde::Deserialize)]
pub struct BridgeRequest {
    pub method: String,
    /// The request target exactly as the request line carries it: origin
    /// form (`/p?q`), or absolute form for a request to a proxy.
    pub target: String,
    /// In order; the caller has already added Host and Connection.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body_base64: Option<String>,
    /// Handle into `OutboundBodies`: the body streams from JS.
    #[serde(default)]
    pub body_stream: Option<u64>,
    /// node's `maxHeaderSize` for the response head: the request's option,
    /// else `http.maxHeaderSize`. Absent: the process-wide
    /// `--max-http-header-size` (16 KiB unless set).
    #[serde(default)]
    pub max_header_size: Option<u64>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// A JS string that holds bytes one per code point (the latin1 convention
/// node writes a request head in). Anything past U+00FF cannot be sent.
fn latin1_bytes(text: &str, what: &str) -> Result<Vec<u8>, String> {
    text.chars()
        .map(|c| u8::try_from(u32::from(c)).map_err(|_| format!("invalid character in {what}")))
        .collect()
}

/// The request target as a `Uri`. node writes its path verbatim, one byte
/// per code point (any byte 0x21-0xFF); `http::Uri` refuses a few of those,
/// so a target it will not hold is percent-encoded byte by byte where it has
/// to be.
fn target_uri(target: &str) -> Result<http::Uri, String> {
    let bytes = latin1_bytes(target, "the request target")?;
    if let Ok(uri) = http::Uri::from_maybe_shared(Bytes::from(bytes.clone())) {
        return Ok(uri);
    }
    let mut encoded = String::with_capacity(target.len() * 3);
    for byte in bytes {
        let plain = byte.is_ascii_graphic()
            && !matches!(
                byte,
                b'"' | b'<' | b'>' | b'\\' | b'^' | b'`' | b'{' | b'|' | b'}'
            );
        if plain {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
        .parse::<http::Uri>()
        .map_err(|e| format!("invalid request target: {e}"))
}

fn build_parts(req: &BridgeRequest) -> Result<http::request::Parts, String> {
    let method = http::Method::from_bytes(req.method.as_bytes())
        .map_err(|_| format!("invalid method {:?}", req.method))?;
    let (mut parts, ()) = http::Request::builder()
        .method(method)
        .uri(target_uri(&req.target)?)
        .version(http::Version::HTTP_11)
        .body(())
        .map_err(|e| e.to_string())?
        .into_parts();
    for (name, value) in &req.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("invalid header name {name:?}"))?;
        let value = HeaderValue::from_bytes(&latin1_bytes(value, "a header value")?)
            .map_err(|_| format!("invalid value for header {name}"))?;
        parts.headers.append(name, value);
    }
    Ok(parts)
}

/// `httpBridgeStart`: set an exchange up. Nothing runs until [`response`].
/// The error is a TypeError's message (a request hyper cannot write).
pub fn start(
    bridges: &Bridges,
    ids: &AtomicU64,
    outbound: OutboundBodies,
    request: &str,
) -> Result<u64, String> {
    let req: BridgeRequest = serde_json::from_str(request)
        .map_err(|e| format!("httpBridgeStart: malformed request: {e}"))?;
    let parts = build_parts(&req)?;
    let body = if let Some(handle) = req.body_stream {
        Body::Stream(StreamSlot::new(handle, outbound))
    } else if let Some(encoded) = &req.body_base64 {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| "httpBridgeStart: malformed base64 body".to_string())?;
        Body::Full(Bytes::from(bytes))
    } else {
        Body::Empty
    };
    let (near, far) = tokio::io::duplex(PIPE);
    let (out, input) = tokio::io::split(far);
    let (sent_tx, sent_rx) = watch::channel(None);
    let id = ids.fetch_add(1, Ordering::Relaxed);
    lock(bridges).insert(
        id,
        Bridge {
            pending: Some(Pending {
                io: near,
                parts,
                body,
                max_header_size: req
                    .max_header_size
                    .unwrap_or_else(crate::http_head::max_http_header_size),
                sent: sent_tx,
            }),
            out: Some(out),
            input: Some(input),
            task: None,
            sent: sent_rx,
        },
    );
    Ok(id)
}

/// node's reading of a response the connection could not deliver: a parse
/// failure is llhttp's coded `Parse Error` (the code is the closest llhttp
/// has for what hyper reports), anything else -- the peer closed or reset
/// before a complete head -- `socket hang up`.
fn exchange_error(error: &hyper::Error) -> OpOutcome {
    if error.is_parse_too_large() {
        OpOutcome::node_failed("HPE_HEADER_OVERFLOW", "Parse Error: Header overflow")
    } else if error.is_parse_status() {
        OpOutcome::node_failed("HPE_INVALID_STATUS", "Parse Error: Invalid status code")
    } else if error.is_parse() {
        OpOutcome::node_failed("HPE_INVALID_CONSTANT", "Parse Error: Expected HTTP/")
    } else {
        OpOutcome::node_failed("ECONNRESET", "socket hang up")
    }
}

/// A header value as JS sees it: latin1, one code point per byte.
fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// `httpBridgeResponse`: run the exchange and resolve at the response head
/// with `{status, statusText, httpVersion, headers, bodyHandle}` -- the fetch
/// payload's shape; `statusText` is the reason phrase on the wire.
pub async fn response(
    bridges: Bridges,
    id: u64,
    bodies: FetchBodies,
    ids: Arc<AtomicU64>,
) -> OpOutcome {
    let pending = lock(&bridges)
        .get_mut(&id)
        .and_then(|bridge| bridge.pending.take());
    let Some(Pending {
        io,
        parts,
        mut body,
        max_header_size,
        sent,
    }) = pending
    else {
        return OpOutcome::Failed(format!("httpBridgeResponse: bridge {id} is gone"));
    };
    let request_body = match body.build() {
        Ok(request_body) => request_body,
        Err(text) => return OpOutcome::Failed(text),
    };
    let body_ended = Arc::new(AtomicBool::new(false));
    let request = http::Request::from_parts(
        parts,
        EndWatch {
            body: request_body,
            ended: body_ended.clone(),
        },
    );
    let io = Counted {
        io,
        written: 0,
        end: RequestEnd { body_ended, sent },
    };
    let (mut sender, connection) = match hyper::client::conn::http1::Builder::new()
        .handshake(TokioIo::new(io))
        .await
    {
        Ok(handshake) => handshake,
        Err(e) => {
            body.request_failed();
            return exchange_error(&e);
        }
    };
    let task = tokio::spawn(async move {
        let _ = connection.await;
    });
    {
        let mut map = lock(&bridges);
        match map.get_mut(&id) {
            Some(bridge) => bridge.task = Some(task.abort_handle()),
            None => {
                // Closed while the exchange was being set up.
                task.abort();
                body.request_failed();
                return OpOutcome::node_failed("ECONNRESET", "socket hang up");
            }
        }
    }
    let response = match sender.send_request(request).await {
        Ok(response) => response,
        Err(e) => {
            body.request_failed();
            return exchange_error(&e);
        }
    };
    // node's parser's response-head limit, counted as `http.request` counts
    // it: the reason phrase, every header name and every value. A head too
    // large for hyper's own read buffer already failed as
    // `HPE_HEADER_OVERFLOW`.
    if let Some(refusal) = super::send::response_head_overflow(&response, max_header_size, false) {
        drop(response);
        body.request_failed();
        return refusal;
    }
    let status = response.status();
    let reason = response
        .extensions()
        .get::<hyper::ext::ReasonPhrase>()
        .map(|reason| latin1(reason.as_bytes()))
        .unwrap_or_else(|| status.canonical_reason().unwrap_or_default().to_string());
    let version = match response.version() {
        http::Version::HTTP_10 => "1.0",
        _ => "1.1",
    };
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(name, value)| (name.as_str().to_string(), latin1(value.as_bytes())))
        .collect();
    let handle = ids.fetch_add(1, Ordering::Relaxed);
    lock(&bodies).insert(handle, FetchBody::coded(response.into_body()));
    OpOutcome::Json(
        serde_json::json!({
            "status": status.as_u16(),
            "statusText": reason,
            "httpVersion": version,
            "headers": headers,
            "bodyHandle": handle,
        })
        .to_string(),
    )
}

/// `httpBridgeOut`: the next bytes hyper wrote, for the socket; `Done` at
/// the end (hyper finished with the connection, or the bridge closed).
pub async fn out(bridges: Bridges, id: u64) -> OpOutcome {
    let half = lock(&bridges)
        .get_mut(&id)
        .and_then(|bridge| bridge.out.take());
    let Some(mut half) = half else {
        return OpOutcome::Done;
    };
    let mut buf = vec![0u8; OUT_CHUNK];
    let read = half.read(&mut buf).await;
    if let Some(bridge) = lock(&bridges).get_mut(&id) {
        bridge.out = Some(half);
    }
    match read {
        Ok(0) | Err(_) => OpOutcome::Done,
        Ok(n) => {
            buf.truncate(n);
            OpOutcome::Bytes(buf)
        }
    }
}

/// `httpBridgeRequestSent`: once hyper has written the whole request, how
/// many bytes that was -- the count of [`out`] bytes the socket has to have
/// written for node's `'finish'`. `Done` if the exchange ends first (a
/// failed or destroyed request is never finished).
pub async fn request_sent(bridges: Bridges, id: u64) -> OpOutcome {
    let sent = lock(&bridges).get(&id).map(|bridge| bridge.sent.clone());
    let Some(mut sent) = sent else {
        return OpOutcome::Done;
    };
    let count = match sent.wait_for(Option::is_some).await {
        Ok(count) => *count,
        Err(_) => None,
    };
    match count {
        Some(count) => OpOutcome::Json(count.to_string()),
        None => OpOutcome::Done,
    }
}

/// `httpBridgeIn`: hand hyper bytes the socket read. Resolves once the pipe
/// took them all, which is the socket's read backpressure. Bytes for a
/// closed bridge are dropped.
pub async fn input(bridges: Bridges, id: u64, bytes: Vec<u8>) -> OpOutcome {
    let half = lock(&bridges)
        .get_mut(&id)
        .and_then(|bridge| bridge.input.take());
    let Some(mut half) = half else {
        return OpOutcome::Done;
    };
    let wrote = half.write_all(&bytes).await;
    if let Some(bridge) = lock(&bridges).get_mut(&id) {
        bridge.input = Some(half);
    }
    match wrote {
        Ok(()) => OpOutcome::Done,
        Err(e) => OpOutcome::Failed(format!("httpBridgeIn: {e}")),
    }
}

/// `httpBridgeInEnd`: the socket reached EOF -- hyper reads the end of the
/// stream (a response cut short fails as `socket hang up` or in its body).
pub async fn input_end(bridges: Bridges, id: u64) -> OpOutcome {
    let half = lock(&bridges)
        .get_mut(&id)
        .and_then(|bridge| bridge.input.take());
    if let Some(mut half) = half {
        let _ = half.shutdown().await;
    }
    OpOutcome::Done
}

/// `httpBridgeClose`: drop the bridge. True if it was there.
pub fn close(bridges: &Bridges, id: u64) -> bool {
    let removed = lock(bridges).remove(&id);
    removed.is_some()
}

/// How many bridges are open.
pub fn open(bridges: &Bridges) -> usize {
    lock(bridges).len()
}
