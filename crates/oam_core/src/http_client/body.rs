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
    /// body or between chunks: undici's `SocketError: other side closed`.
    Closed,
    /// The connection failed with an OS error (a reset): node's `read
    /// ECONNRESET`, with this code and its errno.
    Io {
        code: &'static str,
        errno: Option<i32>,
    },
    /// Anything else (an h2 stream error, a TLS failure mid-body), in
    /// hyper's words. Node's cause for these is not mirrored.
    Other(String),
}

impl BodyReadError {
    /// hyper reports a body failure as an error whose source chain holds the
    /// `io::Error` behind it: kind InvalidInput / InvalidData for malformed
    /// chunked framing ("Invalid chunk size line"), UnexpectedEof for a
    /// connection that ended early (h1 decode.rs, both framings), and the
    /// OS error itself for a reset.
    fn from_hyper(error: &hyper::Error) -> BodyReadError {
        use std::io::ErrorKind;
        let mut current: Option<&(dyn std::error::Error + 'static)> =
            std::error::Error::source(error);
        while let Some(e) = current {
            if let Some(io) = e.downcast_ref::<std::io::Error>() {
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
            BodyReadError::Io { code, errno } => OpOutcome::NodeFailed {
                code: code.to_string(),
                message: format!("read {code}"),
                syscall: Some("read".to_string()),
                path: None,
                errno: *errno,
                hostname: None,
                address: None,
                port: None,
            },
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
        }
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
            match incoming.frame().await {
                None => {
                    self.incoming = None;
                    if self.decoder.is_none() {
                        return Ok(None);
                    }
                }
                Some(Err(e)) => return Err(self.fail(BodyReadError::from_hyper(&e))),
                Some(Ok(frame)) => {
                    // Trailers carry no body bytes; an empty data frame is
                    // not a chunk.
                    let Ok(data) = frame.into_data() else {
                        continue;
                    };
                    if data.is_empty() {
                        continue;
                    }
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
        Ok(None) => OpOutcome::Done,
        Err(error) => error.to_outcome(body.coded),
    }
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
