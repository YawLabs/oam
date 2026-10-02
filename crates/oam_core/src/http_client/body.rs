//! Response bodies and the outbound request-body channel lifecycle.
//!
//! A fetch resolves at the response head; its body stays in [`FetchBodies`]
//! and JS pulls it one chunk per `fetchBodyRead` op. An identity body yields
//! the frames as they arrive off the wire (a server's flush is a chunk, which
//! is what makes SSE streams work). A decoded body yields at most
//! [`super::decode::OUT_CAP`] bytes per read, decoding only as much of a frame as
//! that takes, so a small compressed frame that inflates to megabytes never
//! sits in memory whole.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::BodyExt as _;
use hyper::body::Incoming;

use super::decode::{Coding, DecodeError, Decoder};
use crate::{BodyCancelSignal, CancelledBodies, OpOutcome, OutboundBodies};

/// undici's message for a connection that ended inside a response body
/// (its `SocketError`, code [`SOCKET_CODE`]).
pub const OTHER_SIDE_CLOSED: &str = "other side closed";
pub const SOCKET_CODE: &str = "UND_ERR_SOCKET";
/// undici's `HTTPParserError` for a bad chunk-size line in a fetch body:
/// llhttp's code, and its reason in undici's sentence.
pub const BAD_CHUNK_SIZE_CODE: &str = "HPE_INVALID_CHUNK_SIZE";
pub const BAD_CHUNK_SIZE: &str =
    "Response does not match the HTTP/1.1 protocol (Invalid character in chunk size)";
/// undici's `ResponseContentLengthMismatchError`: a response the server
/// does not keep alive ended short of its content-length.
pub const LENGTH_MISMATCH_CODE: &str = "UND_ERR_RES_CONTENT_LENGTH_MISMATCH";
pub const LENGTH_MISMATCH: &str = "Response body length does not match content-length header";
/// A response the server does not keep alive, with no content-length, ended
/// at the connection's close inside a chunked body. undici takes what has
/// arrived as the whole response (its socket 'end' handler completes the
/// message), so fetch() ends the body there; node's http client aborts the
/// response. Not an undici code: JS ends or aborts on it (bootstrap.js,
/// node_compat.js) and it never reaches a caller.
pub const ENDED_AT_CLOSE_CODE: &str = "OAM_BODY_ENDED_AT_CLOSE";

/// Live response bodies by handle. A std Mutex on purpose: a reader REMOVES
/// its body under a short lock, awaits the chunk with no lock held, then
/// reinserts it -- no guard ever crosses an await. ReadableStream's reader
/// lock guarantees one reader per handle.
pub type FetchBodies = Arc<Mutex<HashMap<u64, FetchBody>>>;

/// One live response body.
pub struct FetchBody {
    /// `None` once the wire is done with: EOF, a failure, or decoding that
    /// ended before the body did. Dropping it closes an unfinished h1
    /// connection instead of returning it to the pool.
    incoming: Option<Incoming>,
    decoder: Option<Decoder>,
    /// Compressed input the decoder has not consumed yet.
    pending: Bytes,
    /// A malformed body (a bad chunk-size line) fails the read with the
    /// parse error of node's own parser instead of undici's: http.request
    /// over an agent's socket reads its body here and reports what node's
    /// http client reports.
    coded: bool,
    /// What a connection ending inside the body means
    /// ([`FetchBody::with_framing`]); `None` reads it as a keep-alive one.
    framing: Option<Framing>,
    /// Body bytes off the wire so far, before any decoding: what undici
    /// compares with the content-length.
    wire_read: u64,
    /// undici's `bodyTimeout`: the longest a read may wait for the wire's
    /// next frame (`fetch`'s and `undici.request`'s; `None` for none).
    timeout: Option<Duration>,
    /// The connection the body arrives on (the shared transport's), whose
    /// facts -- bytes written and read included -- undici's `SocketError`
    /// describes when the peer closes it mid-body (fu2/http-conn-close).
    /// `None` for a body read off an agent's socket or an h2 stream.
    conn: Option<super::connector::ConnInfo>,
    /// Keep the body's trailer section for JS to take at its end
    /// ([`take_trailers`]): http.request's response, whose `trailers` and
    /// `rawTrailers` node fills from it. A fetch has no use for them.
    keep_trailers: bool,
    /// The trailer section, once read (when `keep_trailers`).
    trailers: Option<http::HeaderMap>,
}

/// The response facts undici's parser weighs when the connection ends inside
/// a body: whether the response let the connection be kept alive (llhttp's
/// `should_keep_alive`), and its content-length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Framing {
    pub keep_alive: bool,
    pub content_length: Option<u64>,
}

impl Framing {
    /// llhttp's `should_keep_alive` for a response: HTTP/1.1 unless a
    /// `Connection` header says `close`, HTTP/1.0 only if one says
    /// `keep-alive`.
    pub fn of(version: http::Version, headers: &http::HeaderMap) -> Framing {
        let mut close = false;
        let mut keep_alive = false;
        for value in headers.get_all(http::header::CONNECTION) {
            for token in value.as_bytes().split(|b| *b == b',') {
                let token = token.trim_ascii();
                close |= token.eq_ignore_ascii_case(b"close");
                keep_alive |= token.eq_ignore_ascii_case(b"keep-alive");
            }
        }
        let keep_alive = match version {
            http::Version::HTTP_10 | http::Version::HTTP_09 => keep_alive && !close,
            _ => !close,
        };
        let content_length = headers
            .get(http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        Framing {
            keep_alive,
            content_length,
        }
    }
}

/// Why a body could not be read. Node's fetch rejects the read with
/// `TypeError: terminated` whose `cause` says which of these it was, so a
/// caller can tell a truncated download from a corrupt payload; the op
/// reports each as that cause ([`BodyReadError::to_outcome`]) and
/// bootstrap.js wraps it. Measured on node v22.22.2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyReadError {
    /// The encoding is corrupt: the decoder's error (zlib's `Z_DATA_ERROR`,
    /// brotli's `ERR__ERROR_FORMAT_*`).
    Decode(DecodeError),
    /// Malformed chunked framing (a bad chunk-size line).
    Framing,
    /// The connection ended before the body did -- inside a `content-length`
    /// body or between chunks, or with a TLS failure: undici's
    /// `SocketError: other side closed` on a kept-alive response
    /// ([`FetchBody::closed`] for the others).
    Closed,
    /// The wire sent nothing for the body's whole `bodyTimeout` while a read
    /// waited: undici's `BodyTimeoutError` (`UND_ERR_BODY_TIMEOUT`). The body
    /// is dropped, which closes its connection, as undici destroys the
    /// socket.
    TimedOut,
    /// The connection failed with an OS error (a reset): node's `read
    /// ECONNRESET`, with this code and its errno.
    Io {
        code: &'static str,
        errno: Option<i32>,
    },
    /// A response the server does not keep alive ended short of its
    /// content-length: undici's `ResponseContentLengthMismatchError`.
    LengthMismatch,
    /// A response the server does not keep alive ended inside its chunked
    /// body ([`ENDED_AT_CLOSE_CODE`]).
    EndedAtClose,
    /// Anything else (an h2 stream error), in hyper's words. Node's cause
    /// for these is not mirrored.
    Other(String),
}

impl BodyReadError {
    /// hyper reports a body failure as an error whose source chain holds the
    /// `io::Error` behind it: kind InvalidInput / InvalidData for malformed
    /// chunked framing ("Invalid chunk size line"), UnexpectedEof for a
    /// connection that ended early (h1 decode.rs, both framings), and the
    /// OS error itself for a reset.
    ///
    /// The transport's errors pass through hyper's decoder unchanged, and
    /// tokio-rustls reports every TLS failure in a read -- a fatal alert, a
    /// record that does not decrypt -- as InvalidData with the
    /// `rustls::Error` as its payload: that is not framing, and is told
    /// apart by the payload (`get_ref`, which `source()` does not reach).
    /// node's fetch reports such a failure as the connection ending there
    /// (measured on v22.22.2 with a record that fails to decrypt: `other
    /// side closed` on a kept-alive response; the content-length mismatch,
    /// or the end of the body, on one that is not kept), so it is
    /// [`BodyReadError::Closed`].
    fn from_hyper(error: &hyper::Error) -> BodyReadError {
        use std::io::ErrorKind;
        let mut current: Option<&(dyn std::error::Error + 'static)> =
            std::error::Error::source(error);
        while let Some(e) = current {
            if let Some(io) = e.downcast_ref::<std::io::Error>() {
                let tls = io
                    .get_ref()
                    .is_some_and(|inner| inner.downcast_ref::<rustls::Error>().is_some());
                if tls {
                    return BodyReadError::Closed;
                }
                return match io.kind() {
                    ErrorKind::InvalidInput | ErrorKind::InvalidData => BodyReadError::Framing,
                    ErrorKind::UnexpectedEof => BodyReadError::Closed,
                    ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe
                    | ErrorKind::TimedOut => {
                        let code = crate::node_error_code(io);
                        BodyReadError::Io {
                            code,
                            errno: crate::node_errno(code, io),
                        }
                    }
                    _ => BodyReadError::Other(error.to_string()),
                };
            }
            current = e.source();
        }
        if error.is_incomplete_message() {
            return BodyReadError::Closed;
        }
        BodyReadError::Other(error.to_string())
    }

    /// The op outcome: the error JS takes as the cause of `TypeError:
    /// terminated` (bootstrap.js bodyTerminated turns the undici codes into
    /// undici's classes). `coded` is [`FetchBody::coded`]: node's own parser's
    /// code and text for bad framing.
    fn to_outcome(&self, coded: bool) -> OpOutcome {
        match self {
            BodyReadError::Decode(error) => match error.node_code() {
                // zlib's own shape: errno, then code (node's
                // zlibOnError); no syscall.
                Some((code, errno)) => OpOutcome::NodeFailed {
                    code: code.to_string(),
                    message: error.message().to_string(),
                    syscall: None,
                    path: None,
                    errno: Some(errno),
                    hostname: None,
                    address: None,
                    port: None,
                    dest: None,
                },
                None => OpOutcome::Failed(error.message().to_string()),
            },
            // llhttp's code and text for a bad chunk-size line, the one
            // framing error hyper leaves in the body.
            BodyReadError::Framing if coded => OpOutcome::node_failed(
                BAD_CHUNK_SIZE_CODE,
                "Parse Error: Invalid character in chunk size",
            ),
            BodyReadError::Framing => OpOutcome::node_failed(BAD_CHUNK_SIZE_CODE, BAD_CHUNK_SIZE),
            BodyReadError::Closed => OpOutcome::node_failed(SOCKET_CODE, OTHER_SIDE_CLOSED),
            BodyReadError::LengthMismatch => {
                OpOutcome::node_failed(LENGTH_MISMATCH_CODE, LENGTH_MISMATCH)
            }
            BodyReadError::EndedAtClose => {
                OpOutcome::node_failed(ENDED_AT_CLOSE_CODE, OTHER_SIDE_CLOSED)
            }
            BodyReadError::Io { code, errno } => OpOutcome::NodeFailed {
                code: code.to_string(),
                message: format!("read {code}"),
                syscall: Some("read".to_string()),
                path: None,
                errno: *errno,
                hostname: None,
                address: None,
                port: None,
                dest: None,
            },
            BodyReadError::TimedOut => {
                OpOutcome::node_failed("UND_ERR_BODY_TIMEOUT", "Body Timeout Error")
            }
            BodyReadError::Other(text) => OpOutcome::Failed(text.clone()),
        }
    }
}

impl FetchBody {
    /// `codings`: the plan's codings for a decoded body, `None` for identity.
    pub fn new(incoming: Incoming, codings: Option<&[Coding]>) -> FetchBody {
        FetchBody {
            incoming: Some(incoming),
            decoder: codings.map(Decoder::new),
            pending: Bytes::new(),
            coded: false,
            framing: None,
            wire_read: 0,
            timeout: None,
            conn: None,
            keep_trailers: false,
            trailers: None,
        }
    }

    /// The body keeping its trailer section for [`take_trailers`].
    pub fn keeping_trailers(self, keep: bool) -> FetchBody {
        FetchBody {
            keep_trailers: keep,
            ..self
        }
    }

    /// The response's [`Framing`], for a connection that ends inside the
    /// body. undici's socket 'end' handler fails a kept-alive response with
    /// `SocketError: other side closed`; any other it completes with what
    /// has arrived (its parser's onMessageComplete), which fails a
    /// content-length body that came up short with
    /// `ResponseContentLengthMismatchError` and ends a chunked one there
    /// (measured on node v22.22.2).
    pub fn with_framing(mut self, framing: Framing) -> FetchBody {
        self.framing = Some(framing);
        self
    }

    /// What a connection that ended inside the body is, per [`Framing`].
    fn closed(&self) -> BodyReadError {
        match self.framing {
            Some(framing) if !framing.keep_alive => match framing.content_length {
                Some(length) if length != self.wire_read => BodyReadError::LengthMismatch,
                _ => BodyReadError::EndedAtClose,
            },
            _ => BodyReadError::Closed,
        }
    }

    /// The body with the connection it arrives on ([`FetchBody`]'s `conn`).
    pub(crate) fn on_conn(self, conn: Option<super::connector::ConnInfo>) -> FetchBody {
        FetchBody { conn, ..self }
    }

    /// The body with undici's `bodyTimeout` (`None`: no limit).
    pub fn timed(self, timeout: Option<Duration>) -> FetchBody {
        FetchBody { timeout, ..self }
    }

    /// An undecoded body whose malformed framing reads as node's coded parse
    /// error (http.request over an agent's socket).
    pub fn coded(incoming: Incoming) -> FetchBody {
        FetchBody {
            coded: true,
            ..FetchBody::new(incoming, None)
        }
    }

    /// The next chunk: `Some` bytes (never empty), `None` at the end.
    ///
    /// Cancel-safe: the only await is `Incoming::frame`, which yields nothing
    /// until it completes, and everything a frame delivers is stored in
    /// `self` before the next await. Dropping the future mid-read loses no
    /// data, which is what lets `read` race it against a cancel.
    pub async fn next_chunk(&mut self) -> Result<Option<Bytes>, BodyReadError> {
        loop {
            if let Some(decoder) = &mut self.decoder {
                if self.incoming.is_none() {
                    // The wire is over: drain what is still decodable.
                    return decoder.finish().map_err(BodyReadError::Decode);
                }
                match decoder.push(&mut self.pending) {
                    Err(e) => return Err(self.fail(BodyReadError::Decode(e))),
                    Ok(Some(chunk)) => return Ok(Some(chunk)),
                    Ok(None) if decoder.is_done() => {
                        // node ends the body where the compressed stream
                        // ends when more bytes follow it, without waiting
                        // for the wire (decode.rs `is_done`).
                        self.incoming = None;
                        self.pending = Bytes::new();
                        return Ok(None);
                    }
                    // Input consumed, more needed.
                    Ok(None) => {}
                }
            }
            let Some(incoming) = self.incoming.as_mut() else {
                return Ok(None);
            };
            // undici's bodyTimeout counts from the last bytes off the wire
            // (client-h1.js refreshes it on every socket chunk), not from the
            // start of the read: a frame the decoder takes without yielding
            // anything yet -- a gzip header trickled a byte at a time -- or an
            // empty or trailer frame still restarts it.
            let frame = match self.timeout {
                None => incoming.frame().await,
                Some(limit) => match tokio::time::timeout(limit, incoming.frame()).await {
                    Ok(frame) => frame,
                    Err(_) => return Err(self.fail(BodyReadError::TimedOut)),
                },
            };
            match frame {
                None => {
                    self.incoming = None;
                    if self.decoder.is_none() {
                        return Ok(None);
                    }
                }
                Some(Err(e)) => {
                    let mut error = BodyReadError::from_hyper(&e);
                    if error == BodyReadError::Closed {
                        error = self.closed();
                        if error == BodyReadError::EndedAtClose && self.decoder.is_some() {
                            // What has arrived is the whole response: a
                            // decoded body ends as the decoder does on it.
                            self.incoming = None;
                            continue;
                        }
                    }
                    return Err(self.fail(error));
                }
                Some(Ok(frame)) => {
                    // Trailers carry no body bytes (they are kept for a body
                    // that wants them); an empty data frame is not a chunk.
                    let data = match frame.into_data() {
                        Ok(data) => data,
                        Err(frame) => {
                            if self.keep_trailers
                                && let Ok(trailers) = frame.into_trailers()
                            {
                                self.trailers = Some(trailers);
                            }
                            continue;
                        }
                    };
                    if data.is_empty() {
                        continue;
                    }
                    self.wire_read += data.len() as u64;
                    if self.decoder.is_none() {
                        return Ok(Some(data));
                    }
                    self.pending = data;
                }
            }
        }
    }

    fn fail(&mut self, error: BodyReadError) -> BodyReadError {
        self.incoming = None;
        self.pending = Bytes::new();
        error
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// `fetchBodyRead`: one chunk of the body under `handle` (`Bytes`), `Done`
/// at the end, a [`BodyReadError`]'s outcome on a failure.
///
/// A cancel (`fetchBodyCancel`) that lands while the read is in flight --
/// the body is out of the registry then -- leaves a tombstone in `cancelled`
/// and fires `cancel_signal`; the read returns `Done` and drops the body
/// (closing its connection) instead of reinserting it. The signal is
/// broadcast, so a wake for another handle's cancel is checked against the
/// tombstones and the read carries on with its state intact.
///
/// The wake cannot be lost. `Notify::notify_waiters` stores no permit: it
/// wakes only the waiters already registered, so a cancel landing on the V8
/// thread before this task first polls `notified()` used to be dropped
/// outright -- and against a peer that stops sending without closing the
/// connection `next_chunk()` never resolves, so the op never completed,
/// `inflight` never dropped and the loop could not drain. The waiter is
/// therefore registered with [`Notified::enable`] BEFORE the tombstone is
/// read, and re-registered before the tombstone is re-read on every wake:
/// a cancel either precedes the registration, and the tombstone check sees
/// it, or follows it, and the registration catches it.
pub async fn read(
    bodies: FetchBodies,
    cancelled: CancelledBodies,
    cancel_signal: BodyCancelSignal,
    handle: u64,
) -> OpOutcome {
    let body = lock(&bodies).remove(&handle);
    let Some(mut body) = body else {
        return OpOutcome::Failed(format!("fetch: body handle {handle} is gone"));
    };
    let mut cancelled_wake = Box::pin(cancel_signal.notified());
    cancelled_wake.as_mut().enable();
    if lock(&cancelled).remove(&handle) {
        return OpOutcome::Done;
    }
    let result = loop {
        tokio::select! {
            result = body.next_chunk() => break result,
            () = cancelled_wake.as_mut() => {
                cancelled_wake.set(cancel_signal.notified());
                cancelled_wake.as_mut().enable();
                if lock(&cancelled).remove(&handle) {
                    return OpOutcome::Done;
                }
            }
        }
    };
    if lock(&cancelled).remove(&handle) {
        return OpOutcome::Done;
    }
    match result {
        // undici's bodyTimeout lapsed: the body is dropped, which closes its
        // connection, as undici destroys the socket.
        Err(BodyReadError::TimedOut) => {
            drop(body);
            OpOutcome::node_failed("UND_ERR_BODY_TIMEOUT", "Body Timeout Error")
        }
        Ok(Some(chunk)) => {
            lock(&bodies).insert(handle, body);
            // A cancel can land between the tombstone check above and the
            // insert (no tombstone yet, body not in the registry): without
            // this second look the cancelled body would revive and hold its
            // connection for the rest of the run.
            if lock(&cancelled).remove(&handle) {
                let revived = lock(&bodies).remove(&handle);
                drop(revived);
            }
            OpOutcome::Bytes(chunk.to_vec())
        }
        Ok(None) => {
            // A trailer section waits for JS to take it (take_trailers).
            if body.trailers.is_some() {
                lock(&bodies).insert(handle, body);
            }
            OpOutcome::Done
        }
        // A kept-alive response's connection the peer closed mid-body:
        // undici's SocketError, with the connection's facts as they stand.
        Err(BodyReadError::Closed) if body.conn.is_some() => match &body.conn {
            Some(conn) => OpOutcome::socket_closed(conn.socket_facts()),
            None => BodyReadError::Closed.to_outcome(body.coded),
        },
        Err(error) => error.to_outcome(body.coded),
    }
}

/// `fetchBodyTrailers`: the trailer section of the body under `handle`, read
/// to its end, as `[name, value]` pairs (each value one byte per code point),
/// and the body let go. `None` when it had none, or was not kept for them.
pub fn take_trailers(bodies: &FetchBodies, handle: u64) -> Option<Vec<(String, String)>> {
    let mut map = lock(bodies);
    if map.get(&handle).is_none_or(|body| body.trailers.is_none()) {
        return None;
    }
    let trailers = map.remove(&handle)?.trailers?;
    Some(
        trailers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    value.as_bytes().iter().map(|&b| char::from(b)).collect(),
                )
            })
            .collect(),
    )
}

/// A streamed request body's claim on its [`OutboundBodies`] entry
/// (`(sender, receiver)`, created by `fetchBodyChannelNew`).
///
/// The entry lifecycle, which used to leak an entry per streamed request:
///
/// - the fetch takes the receiver only when the request is about to go out,
///   after every step that can fail first; if the op ends before that (a bad
///   URL, a bad port, a lookup hook that failed), dropping the slot drops the
///   receiver, so pending and later writes resolve instead of blocking on a
///   full channel;
/// - once the receiver is taken, the entry is removed as soon as JS has also
///   ended the body (`fetchBodyChannelEnd`, see [`end_outbound`]), whichever
///   comes second;
/// - a request that fails after the take removes the entry;
/// - `fetchBodyChannelCancel` removes it itself.
///
/// What stays: a successful request whose body JS never ends keeps
/// `(sender, None)` -- that body is genuinely still open.
pub(crate) struct StreamSlot {
    handle: u64,
    outbound: OutboundBodies,
    state: SlotState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Untaken,
    Taken,
    Dead,
}

impl StreamSlot {
    pub(crate) fn new(handle: u64, outbound: OutboundBodies) -> StreamSlot {
        StreamSlot {
            handle,
            outbound,
            state: SlotState::Untaken,
        }
    }

    pub(crate) fn handle(&self) -> u64 {
        self.handle
    }

    /// Take the receiver (once). `None` if the channel is unknown or was
    /// already taken.
    pub(crate) fn take(&mut self) -> Option<tokio::sync::mpsc::Receiver<Result<Vec<u8>, String>>> {
        if self.state != SlotState::Untaken {
            return None;
        }
        self.state = SlotState::Taken;
        let mut outbound = lock(&self.outbound);
        let entry = outbound.get_mut(&self.handle)?;
        let receiver = entry.1.take();
        if entry.0.is_none() {
            outbound.remove(&self.handle);
        }
        receiver
    }

    /// The request failed: forget the channel. Later writes fail as an
    /// unknown stream, which the http client ignores.
    pub(crate) fn request_failed(&mut self) {
        self.state = SlotState::Dead;
        let removed = lock(&self.outbound).remove(&self.handle);
        drop(removed);
    }
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        if self.state != SlotState::Untaken {
            return;
        }
        let mut outbound = lock(&self.outbound);
        let Some(entry) = outbound.get_mut(&self.handle) else {
            return;
        };
        let receiver = entry.1.take();
        let removed = if entry.0.is_none() {
            outbound.remove(&self.handle)
        } else {
            None
        };
        drop(outbound);
        drop((receiver, removed));
    }
}

/// `fetchBodyChannelEnd`: drop the sender, which ends the request body, and
/// remove the entry if the fetch already took the receiver.
pub fn end_outbound(outbound: &OutboundBodies, handle: u64) {
    let mut map = lock(outbound);
    let Some(entry) = map.get_mut(&handle) else {
        return;
    };
    let sender = entry.0.take();
    let removed = if entry.1.is_none() {
        map.remove(&handle)
    } else {
        None
    };
    drop(map);
    drop((sender, removed));
}

// ------------------------------------------------- the server's request bodies

/// What a request body hyper failed on was, on the server side
/// (http_server.rs `request_body_failure`; fu2/http-conn-close). hyper
/// reports malformed chunked framing as a body error whose source is an
/// `io::Error` of kind InvalidInput / InvalidData ("Invalid chunk size
/// line"); a connection that ends early as one of kind UnexpectedEof ("end
/// of file before message length reached"); a reset as the read's own
/// ConnectionReset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServerBodyError {
    /// Malformed chunked framing.
    Framing,
    /// The peer reset the connection; the OS error, when there was one.
    Reset(Option<i32>),
    /// The peer closed the connection before the body's end.
    Closed,
    /// Anything else.
    Other,
}

pub(crate) fn classify_server_body(error: &hyper::Error) -> ServerBodyError {
    let mut current: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(error);
    while let Some(e) = current {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            return match io.kind() {
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData => {
                    ServerBodyError::Framing
                }
                std::io::ErrorKind::ConnectionReset => ServerBodyError::Reset(io.raw_os_error()),
                std::io::ErrorKind::UnexpectedEof => ServerBodyError::Closed,
                _ => ServerBodyError::Other,
            };
        }
        current = e.source();
    }
    if error.is_incomplete_message() {
        ServerBodyError::Closed
    } else {
        ServerBodyError::Other
    }
}

/// llhttp's code and text for a bad chunk-size line, the one framing error
/// hyper leaves in a body (measured on v22.22.2, a request's or a
/// response's).
pub(crate) fn invalid_chunk_size() -> OpOutcome {
    OpOutcome::node_failed(INVALID_CHUNK_SIZE.0, INVALID_CHUNK_SIZE.1)
}

/// [`invalid_chunk_size`]'s code and message.
pub(crate) const INVALID_CHUNK_SIZE: (&str, &str) = (
    "HPE_INVALID_CHUNK_SIZE",
    "Parse Error: Invalid character in chunk size",
);

/// [`invalid_eof_state`]'s code and message.
pub(crate) const INVALID_EOF_STATE: (&str, &str) = ("HPE_INVALID_EOF_STATE", "Parse Error");

/// llhttp's code and text for a connection that ended mid-body: node's
/// parser refuses the end of the stream there (`parser.finish()`), and the
/// message carries no reason (v22.22.2).
pub(crate) fn invalid_eof_state() -> OpOutcome {
    OpOutcome::node_failed(INVALID_EOF_STATE.0, INVALID_EOF_STATE.1)
}
