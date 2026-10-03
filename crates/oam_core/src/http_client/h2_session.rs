//! An HTTP/2 client session over a [`byte_pipe`](crate::byte_pipe): what
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
//!
//! node's `session.close()` sends its GOAWAY at once, while the streams are
//! still open, and hyper's client has no way to send one before it is done:
//! [`send_goaway`] puts one between hyper's frames ([`SessionIo`]). It
//! matters: a peer that sent its own GOAWAY first may stop reading once its
//! last stream is done -- node's nghttp2 does -- and then never sees a
//! GOAWAY that comes later, nor the end of the connection, so it never
//! closes, and nor did the session (#185).
//!
//! The peer's GOAWAY frames are read off the bytes on their way to hyper
//! ([`SessionIo`]), which keeps them to itself: [`goaway`] hands each to
//! JS -- its code, last stream id and debug data, node's `'goaway'`
//! arguments -- as it arrives, and a stream it refused (one above its last
//! stream id) fails as nghttp2 closes it, with `NGHTTP2_REFUSED_STREAM`
//! (#185). hyper reported those streams as the connection failing, and a
//! graceful GOAWAY's clean end as an EOF, so a session told nothing of
//! either: every stream closed with `NGHTTP2_CANCEL`, the one the server had
//! answered included, and no `'goaway'` came.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

use base64::Engine as _;
use bytes::Bytes;
use futures_util::task::AtomicWaker;
use http::header::{HeaderName, HeaderValue};
use hyper::client::conn::http2::SendRequest;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::ReqBody;
use super::body::{FetchBodies, FetchBody, StreamSlot};
use super::transport::{channel_body, empty_body, full_body};
use crate::byte_pipe::Pipes;
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
    /// The GOAWAY frames the peer has sent, in order. The sender lives in
    /// the connection's [`SessionIo`], so it is dropped -- and no frame
    /// can follow -- once hyper lets go of the connection.
    goaways: tokio::sync::watch::Receiver<Vec<GoAwayFrame>>,
    /// Frames of the session's own, to go out between hyper's.
    outbox: Arc<Outbox>,
}

/// Frames the session sends itself, hyper knowing nothing of them: what
/// [`SessionIo`] writes at the next frame boundary of hyper's bytes.
#[derive(Default)]
struct Outbox {
    /// Set while `frames` holds any; read on every write without the lock.
    queued: AtomicBool,
    frames: Mutex<Vec<u8>>,
    /// The connection task, as last seen reading or flushing: woken so the
    /// queued frames go out though hyper has nothing to write.
    task: AtomicWaker,
}

/// One GOAWAY frame the peer sent: node's `'goaway'` arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoAwayFrame {
    /// The HTTP/2 error code.
    pub code: u32,
    /// The highest stream id the peer may have processed; the client's
    /// streams above it were not.
    pub last_stream_id: u32,
    /// The frame's additional debug data (node's `opaqueData`).
    pub debug_data: Vec<u8>,
    /// How many streams had begun their response -- a HEADERS frame -- in
    /// the bytes before this frame. node reports a response head before a
    /// GOAWAY that follows it; the two reach JS by separate ops, so JS holds
    /// the `'goaway'` until it has seen that many.
    pub heads_before: u32,
    /// How many streams had ended their response -- END_STREAM, or a
    /// reset of one begun -- in the bytes before this frame. A session
    /// already closed goes when its last stream does, and node's then reads
    /// nothing more: JS holds the `'goaway'` of a closed session until that
    /// many streams have closed, so one that came after the last stream's
    /// end is not reported, as node's is not.
    pub ends_before: u32,
}

/// Frame types (RFC 9113 6).
const DATA: u8 = 0x0;
const HEADERS: u8 = 0x1;
const RST_STREAM: u8 = 0x3;
const PUSH_PROMISE: u8 = 0x5;
const GOAWAY: u8 = 0x7;
const CONTINUATION: u8 = 0x9;
/// The END_STREAM flag of DATA and HEADERS.
const END_STREAM: u8 = 0x1;
/// The END_HEADERS flag of HEADERS, PUSH_PROMISE and CONTINUATION.
const END_HEADERS: u8 = 0x4;
/// The client connection preface (RFC 9113 3.4), before the first frame.
const PREFACE_LEN: usize = 24;

/// Follows the frames hyper writes, to know when a frame of the session's
/// own can go between them: after the preface and the SETTINGS frame that
/// must come first, never inside a frame, and never inside a header block
/// (a HEADERS or PUSH_PROMISE and the CONTINUATION frames that end it go
/// out together, RFC 9113 6.10).
struct WriteScan {
    /// Preface bytes still to go.
    preface: usize,
    head: [u8; 9],
    /// Bytes of `head` written so far.
    have: usize,
    /// Payload bytes of the current frame still to go.
    left: usize,
    /// A whole frame header has gone out.
    framed: bool,
    /// Inside a header block.
    in_block: bool,
}

impl Default for WriteScan {
    fn default() -> Self {
        WriteScan {
            preface: PREFACE_LEN,
            head: [0; 9],
            have: 0,
            left: 0,
            framed: false,
            in_block: false,
        }
    }
}

impl WriteScan {
    /// Between two frames, outside a header block.
    fn at_boundary(&self) -> bool {
        self.framed && self.have == 0 && self.left == 0 && !self.in_block
    }

    /// Follow the bytes written.
    fn feed(&mut self, mut bytes: &[u8]) {
        if self.preface > 0 {
            let n = self.preface.min(bytes.len());
            self.preface -= n;
            bytes = &bytes[n..];
        }
        while !bytes.is_empty() {
            if self.left > 0 {
                let n = self.left.min(bytes.len());
                self.left -= n;
                bytes = &bytes[n..];
                continue;
            }
            let n = (self.head.len() - self.have).min(bytes.len());
            self.head[self.have..self.have + n].copy_from_slice(&bytes[..n]);
            self.have += n;
            bytes = &bytes[n..];
            if self.have == self.head.len() {
                self.have = 0;
                self.left = usize::from(self.head[0]) << 16
                    | usize::from(self.head[1]) << 8
                    | usize::from(self.head[2]);
                self.framed = true;
                if matches!(self.head[3], HEADERS | PUSH_PROMISE | CONTINUATION) {
                    self.in_block = self.head[4] & END_HEADERS == 0;
                }
            }
        }
    }
}

/// A GOAWAY frame with `code`, last stream id 0 (a client takes no streams
/// the peer opens) and no debug data: what nghttp2 sends for node's
/// `session.close()`.
fn goaway_frame(code: u32) -> [u8; 17] {
    let mut frame = [0u8; 17];
    frame[2] = 8;
    frame[3] = GOAWAY;
    frame[13..17].copy_from_slice(&code.to_be_bytes());
    frame
}

/// Reads the frame headers of what the peer sends, and records each GOAWAY
/// frame, without changing a byte: the bytes are hyper's to parse. The
/// stream starts with a frame (the server's preface is a SETTINGS frame, no
/// magic), and every frame is a 9-byte header and the payload length it
/// names, so following the lengths is all it takes.
#[derive(Default)]
struct FrameScan {
    head: [u8; 9],
    /// Bytes of `head` read so far; 9 once the payload is being read.
    have: usize,
    /// Payload bytes of the current frame still to come.
    left: usize,
    /// The current frame's payload, when it is a GOAWAY.
    goaway: Option<Vec<u8>>,
    /// The streams whose response has begun and not yet ended: a later
    /// HEADERS frame on one is a final head after a 1xx, or its trailers.
    open_heads: std::collections::HashSet<u32>,
    /// Streams whose response has begun, ever.
    heads: u32,
    /// Streams whose response has ended, ever.
    ends: u32,
}

impl FrameScan {
    /// Feed the bytes read; `found` gets each complete GOAWAY frame.
    fn feed(&mut self, mut bytes: &[u8], found: &mut impl FnMut(GoAwayFrame)) {
        while !bytes.is_empty() {
            if self.have < self.head.len() {
                let n = (self.head.len() - self.have).min(bytes.len());
                self.head[self.have..self.have + n].copy_from_slice(&bytes[..n]);
                self.have += n;
                bytes = &bytes[n..];
                if self.have == self.head.len() {
                    self.left = usize::from(self.head[0]) << 16
                        | usize::from(self.head[1]) << 8
                        | usize::from(self.head[2]);
                    self.goaway = (self.head[3] == GOAWAY).then(Vec::new);
                    self.track_heads();
                    if self.left == 0 {
                        self.frame_done(found);
                    }
                }
                continue;
            }
            let n = self.left.min(bytes.len());
            if let Some(payload) = &mut self.goaway {
                payload.extend_from_slice(&bytes[..n]);
            }
            self.left -= n;
            bytes = &bytes[n..];
            if self.left == 0 {
                self.frame_done(found);
            }
        }
    }

    /// Count the streams whose response has begun, and ended, from the
    /// frame header just read.
    fn track_heads(&mut self) {
        let kind = self.head[3];
        let ends = self.head[4] & END_STREAM != 0;
        let stream = u32::from_be_bytes([self.head[5], self.head[6], self.head[7], self.head[8]])
            & 0x7fff_ffff;
        if kind == HEADERS && !self.open_heads.contains(&stream) {
            self.heads = self.heads.wrapping_add(1);
            if ends {
                self.ends = self.ends.wrapping_add(1);
            } else {
                self.open_heads.insert(stream);
            }
            return;
        }
        // A stream begun ends with END_STREAM, or a reset.
        let ending = match kind {
            HEADERS | DATA => ends,
            RST_STREAM => true,
            _ => false,
        };
        if ending && self.open_heads.remove(&stream) {
            self.ends = self.ends.wrapping_add(1);
        }
    }

    fn frame_done(&mut self, found: &mut impl FnMut(GoAwayFrame)) {
        self.have = 0;
        // A GOAWAY shorter than its 8 fixed bytes is a FRAME_SIZE_ERROR,
        // which hyper reports as the connection's end.
        if let Some(payload) = self.goaway.take()
            && payload.len() >= 8
        {
            let word = |at: usize| {
                u32::from_be_bytes([
                    payload[at],
                    payload[at + 1],
                    payload[at + 2],
                    payload[at + 3],
                ])
            };
            found(GoAwayFrame {
                last_stream_id: word(0) & 0x7fff_ffff,
                code: word(4),
                debug_data: payload[8..].to_vec(),
                heads_before: self.heads,
                ends_before: self.ends,
            });
        }
    }
}

/// The session's byte stream as hyper reads and writes it: [`FrameScan`]
/// reads along with hyper, and [`WriteScan`] follows what hyper writes, so
/// that the [`Outbox`]'s frames go out between two of its frames.
struct SessionIo<T> {
    io: T,
    scan: FrameScan,
    goaways: tokio::sync::watch::Sender<Vec<GoAwayFrame>>,
    written: WriteScan,
    outbox: Arc<Outbox>,
    /// Outbox bytes taken and not yet all written: they go out whole
    /// before anything more of hyper's.
    sending: Vec<u8>,
    sent: usize,
}

impl<T: AsyncWrite + Unpin> SessionIo<T> {
    /// Write the outbox's frames, if it holds any and hyper is between
    /// frames. Pending while they are half written.
    fn poll_outbox(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if self.sending.is_empty() {
            if !self.outbox.queued.load(Ordering::Acquire) || !self.written.at_boundary() {
                return Poll::Ready(Ok(()));
            }
            self.sending = std::mem::take(&mut *lock(&self.outbox.frames));
            self.outbox.queued.store(false, Ordering::Release);
            self.sent = 0;
        }
        while self.sent < self.sending.len() {
            let n = std::task::ready!(
                Pin::new(&mut self.io).poll_write(cx, &self.sending[self.sent..])
            )?;
            if n == 0 {
                return Poll::Ready(Err(std::io::ErrorKind::WriteZero.into()));
            }
            self.sent += n;
        }
        self.sending.clear();
        Poll::Ready(Ok(()))
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for SessionIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        this.outbox.task.register(cx.waker());
        let before = buf.filled().len();
        let polled = Pin::new(&mut this.io).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = polled {
            let goaways = &this.goaways;
            this.scan.feed(&buf.filled()[before..], &mut |frame| {
                goaways.send_modify(|frames| frames.push(frame));
            });
        }
        polled
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for SessionIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        std::task::ready!(this.poll_outbox(cx))?;
        let n = std::task::ready!(Pin::new(&mut this.io).poll_write(cx, buf))?;
        this.written.feed(&buf[..n]);
        Poll::Ready(Ok(n))
    }

    // h2 writes every frame it holds before it flushes, so a request made
    // before the session's GOAWAY was queued goes out ahead of it, as
    // node's does; the outbox is written here once it has.
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        this.outbox.task.register(cx.waker());
        std::task::ready!(this.poll_outbox(cx))?;
        Pin::new(&mut this.io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        std::task::ready!(this.poll_outbox(cx))?;
        Pin::new(&mut this.io).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        std::task::ready!(this.poll_outbox(cx))?;
        let n = std::task::ready!(Pin::new(&mut this.io).poll_write_vectored(cx, bufs))?;
        let mut left = n;
        for buf in bufs {
            if left == 0 {
                break;
            }
            let take = left.min(buf.len());
            this.written.feed(&buf[..take]);
            left -= take;
        }
        Poll::Ready(Ok(n))
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }
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

/// `http2SessionOpen(pipeId)`: an HTTP/2 client session over the near end of
/// pipe `pipeId`. Resolves with `{session}` once the preface and SETTINGS
/// are written into the pipe.
pub async fn open(
    sessions: H2Sessions,
    pipes: Pipes,
    pipe_id: u64,
    ids: Arc<AtomicU64>,
) -> OpOutcome {
    let Some(io) = crate::byte_pipe::take_near(&pipes, pipe_id) else {
        return OpOutcome::Failed(format!("http2SessionOpen: pipe {pipe_id} is gone"));
    };
    let (goaways_tx, goaways) = tokio::sync::watch::channel(Vec::new());
    let outbox = Arc::new(Outbox::default());
    let io = SessionIo {
        io,
        scan: FrameScan::default(),
        goaways: goaways_tx,
        written: WriteScan::default(),
        outbox: outbox.clone(),
        sending: Vec::new(),
        sent: 0,
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
            goaways,
            outbox,
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

/// nghttp2's code for a stream the peer's GOAWAY refused.
const REFUSED_STREAM: u32 = 7;

/// A stream that produced no response. A stream the peer reset is
/// `ERR_HTTP2_STREAM_ERROR` carrying the h2 code as `errno` (JS closes the
/// stream with it, as node's onStreamClose does). So is one the peer's
/// GOAWAY refused -- h2 fails a stream above the frame's last stream id
/// with the GOAWAY itself, where nghttp2 closes it with
/// `NGHTTP2_REFUSED_STREAM` -- marked `syscall: "goaway"` so JS closes it
/// after the session's `'goaway'`, in node's order. And so is a request
/// that never reached h2 (no h2 error in the chain: hyper's dispatch failed
/// it) once the peer's GOAWAY (`after_goaway`) has come. Anything else is the
/// connection failing under it, `ERR_HTTP2_SESSION_FAILED` -- JS leaves that
/// to the session, which reports the connection's end once, as node does,
/// and takes its streams down with it.
fn stream_error(error: &hyper::Error, after_goaway: bool) -> OpOutcome {
    let closed = |code: u32, syscall: Option<&str>| OpOutcome::NodeFailed {
        code: "ERR_HTTP2_STREAM_ERROR".to_string(),
        message: format!("Stream closed with error code {}", nghttp2_name(code)),
        syscall: syscall.map(str::to_string),
        path: None,
        errno: i32::try_from(code).ok(),
        hostname: None,
        address: None,
        port: None,
        dest: None,
    };
    if let Some(h2) = find_h2(error) {
        if h2.is_reset()
            && let Some(reason) = h2.reason()
        {
            return closed(u32::from(reason), None);
        }
        if h2.is_go_away() && h2.is_remote() {
            return closed(REFUSED_STREAM, Some("goaway"));
        }
    } else if after_goaway {
        return closed(REFUSED_STREAM, Some("goaway"));
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
    let (sender, goaways) = match lock(&sessions).get(&id) {
        Some(session) => (session.sender.clone(), Some(session.goaways.clone())),
        None => (None, None),
    };
    let after_goaway = || {
        goaways
            .as_ref()
            .is_some_and(|seen| !seen.borrow().is_empty())
    };
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
    // A request made once the peer's GOAWAY has come, before JS has heard of
    // it, is refused here: node's session would have refused it at
    // `request()` (it is closed from the GOAWAY on), and h2 would send it
    // past the GOAWAY's last stream id, where the peer ignores it and it
    // waits for the connection's end. As a stream above that id it closes
    // with `NGHTTP2_REFUSED_STREAM`, after the `'goaway'`.
    let refused = || OpOutcome::NodeFailed {
        code: "ERR_HTTP2_STREAM_ERROR".to_string(),
        message: format!(
            "Stream closed with error code {}",
            nghttp2_name(REFUSED_STREAM)
        ),
        syscall: Some("goaway".to_string()),
        path: None,
        errno: i32::try_from(REFUSED_STREAM).ok(),
        hostname: None,
        address: None,
        port: None,
        dest: None,
    };
    if after_goaway() {
        return fail(&mut slot, refused());
    }
    if let Err(e) = sender.ready().await {
        return fail(&mut slot, stream_error(&e, after_goaway()));
    }
    if after_goaway() {
        return fail(&mut slot, refused());
    }
    let response = match sender.send_request(request).await {
        Ok(response) => response,
        Err(e) => return fail(&mut slot, stream_error(&e, after_goaway())),
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
    // Its trailer section is kept for JS: the stream's 'trailers' event.
    lock(&bodies).insert(handle, FetchBody::coded(body).keeping_trailers(true));
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

/// `http2SessionGoaway(session, index)`: resolves with the peer's
/// `index`-th GOAWAY frame (from 0) once it has arrived, as
/// `{"goaway": {code, lastStreamId, data, headsBefore, endsBefore}}` (`data`
/// the debug data in base64, `null` when there is none; `headsBefore`
/// [`GoAwayFrame::heads_before`], `endsBefore` [`GoAwayFrame::ends_before`]),
/// or `{"goaway": null}` once the
/// connection has ended without one -- and for a session already gone.
pub async fn goaway(sessions: H2Sessions, id: u64, index: usize) -> OpOutcome {
    let receiver = lock(&sessions)
        .get(&id)
        .map(|session| session.goaways.clone());
    let none = || OpOutcome::Json(serde_json::json!({ "goaway": null }).to_string());
    let Some(mut receiver) = receiver else {
        return none();
    };
    loop {
        if let Some(frame) = receiver.borrow_and_update().get(index).cloned() {
            let data = (!frame.debug_data.is_empty())
                .then(|| base64::engine::general_purpose::STANDARD.encode(&frame.debug_data));
            return OpOutcome::Json(
                serde_json::json!({
                    "goaway": {
                        "code": frame.code,
                        "lastStreamId": frame.last_stream_id,
                        "data": data,
                        "headsBefore": frame.heads_before,
                        "endsBefore": frame.ends_before,
                    }
                })
                .to_string(),
            );
        }
        // The connection gone, no frame can follow: one that came with the
        // last change is still handed over (the next pass).
        if receiver.changed().await.is_err() && receiver.borrow().get(index).is_none() {
            return none();
        }
    }
}

/// `http2SessionSendGoaway(session)`: send a GOAWAY (NO_ERROR) now, after
/// every frame hyper holds, as nghttp2 sends node's `session.close()` one.
/// hyper sends its own once the streams are done, as node's `destroy()`
/// sends a second. False if the session is gone.
pub fn send_goaway(sessions: &H2Sessions, id: u64) -> bool {
    let sessions = lock(sessions);
    let Some(session) = sessions.get(&id) else {
        return false;
    };
    let outbox = &session.outbox;
    lock(&outbox.frames).extend_from_slice(&goaway_frame(0));
    outbox.queued.store(true, Ordering::Release);
    outbox.task.wake();
    true
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

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(kind: u8, flags: u8, id: u32, payload: &[u8]) -> Vec<u8> {
        let len = payload.len();
        let mut out = vec![(len >> 16) as u8, (len >> 8) as u8, len as u8, kind, flags];
        out.extend_from_slice(&id.to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn goaway(last: u32, code: u32, debug: &[u8]) -> Vec<u8> {
        let mut payload = last.to_be_bytes().to_vec();
        payload.extend_from_slice(&code.to_be_bytes());
        payload.extend_from_slice(debug);
        frame(GOAWAY, 0, 0, &payload)
    }

    /// What `FrameScan` finds in `bytes` fed `step` bytes at a time.
    fn scan(bytes: &[u8], step: usize) -> Vec<GoAwayFrame> {
        let mut scan = FrameScan::default();
        let mut found = Vec::new();
        for piece in bytes.chunks(step) {
            scan.feed(piece, &mut |frame| found.push(frame));
        }
        found
    }

    /// The GOAWAY frames in a server's bytes, however the reads split them,
    /// with the response heads, and ends, that came before each: a 1xx and
    /// its final head are one stream's, trailers are not a head but end
    /// the stream, a stream that ended (END_STREAM, or a reset) is done
    /// with, and a reset of one done with is not another end.
    #[test]
    fn the_scan_finds_each_goaway_and_the_heads_before_it() {
        let mut bytes = frame(0x4, 0, 0, &[0, 3, 0, 0, 0, 100]); // SETTINGS
        bytes.extend(frame(HEADERS, 0x4, 1, &[0x88])); // stream 1's head
        bytes.extend(frame(DATA, 0, 1, &[b'x'; 300])); // a long body
        bytes.extend(frame(HEADERS, 0x5, 1, &[0x40])); // its trailers, END_STREAM
        bytes.extend(frame(HEADERS, 0x4, 3, &[0x88])); // a 1xx on stream 3 (as bytes go)
        bytes.extend(frame(HEADERS, 0x4, 3, &[0x88])); // its final head
        bytes.extend(goaway(5, 0, b"")); // two heads before it
        bytes.extend(frame(RST_STREAM, 0, 3, &[0, 0, 0, 8]));
        bytes.extend(frame(RST_STREAM, 0, 1, &[0, 0, 0, 0])); // after its end: not another
        bytes.extend(frame(HEADERS, 0x5, 5, &[0x88])); // a head that ends its stream
        bytes.extend(frame(0x6, 0, 0, &[0; 8])); // PING
        bytes.extend(goaway(5, 2, b"why")); // INTERNAL_ERROR, three heads before
        let want = vec![
            GoAwayFrame {
                code: 0,
                last_stream_id: 5,
                debug_data: Vec::new(),
                heads_before: 2,
                ends_before: 1,
            },
            GoAwayFrame {
                code: 2,
                last_stream_id: 5,
                debug_data: b"why".to_vec(),
                heads_before: 3,
                ends_before: 3,
            },
        ];
        for step in [1, 2, 7, 9, 10, 64, bytes.len()] {
            assert_eq!(scan(&bytes, step), want, "fed {step} bytes at a time");
        }
    }

    /// The reserved bit of the last stream id is not part of it; a GOAWAY
    /// too short to hold its fixed fields is hyper's to refuse, not a frame.
    #[test]
    fn the_scan_reads_the_last_stream_id_without_its_reserved_bit() {
        let found = scan(&goaway(0x8000_0007, 11, b""), 3);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].last_stream_id, 7);
        assert_eq!(found[0].code, 11);
        assert!(scan(&frame(GOAWAY, 0, 0, &[0; 7]), 4).is_empty());
    }

    /// Where a frame of the session's own can go in what hyper writes: not
    /// before the preface and the first frame, not inside a frame however
    /// the writes split it, and not inside a header block.
    #[test]
    fn the_write_scan_finds_the_frame_boundaries() {
        let mut bytes = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
        assert_eq!(bytes.len(), PREFACE_LEN);
        bytes.extend(frame(0x4, 0, 0, &[0, 3, 0, 0, 0, 100])); // SETTINGS
        let settings_end = bytes.len();
        bytes.extend(frame(HEADERS, 0, 1, &[0x82])); // a header block, not ended
        let in_block = bytes.len();
        bytes.extend(frame(CONTINUATION, END_HEADERS, 1, &[0x84]));
        let block_end = bytes.len();
        bytes.extend(frame(DATA, END_STREAM, 1, b"body"));
        let boundaries = [settings_end, block_end, bytes.len()];
        assert!(!boundaries.contains(&in_block));
        // Byte by byte: a boundary exactly where a frame (or block) ends.
        let mut scan = WriteScan::default();
        assert!(!scan.at_boundary());
        for (at, b) in bytes.iter().enumerate() {
            scan.feed(&[*b]);
            assert_eq!(
                scan.at_boundary(),
                boundaries.contains(&(at + 1)),
                "after byte {}",
                at + 1
            );
        }
        // However the writes split it, a write that ends at a boundary
        // leaves the scan at one, and one that ends elsewhere does not.
        for step in [2, 7, 9, 10, 64, bytes.len()] {
            let mut scan = WriteScan::default();
            let mut at = 0;
            for piece in bytes.chunks(step) {
                scan.feed(piece);
                at += piece.len();
                assert_eq!(
                    scan.at_boundary(),
                    boundaries.contains(&at),
                    "at {at}, step {step}"
                );
            }
        }
        assert_eq!(
            goaway_frame(0).to_vec(),
            goaway(0, 0, b""),
            "the session's GOAWAY: NO_ERROR, last stream id 0"
        );
    }
}
