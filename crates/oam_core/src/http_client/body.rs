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

use super::decode::{Coding, Decoder};
use crate::{BodyCancelSignal, CancelledBodies, OpOutcome, OutboundBodies};

/// reqwest's text for every failure while reading a body (it reported wire
/// errors as decode errors too); node_compat.js and user code have only ever
/// seen this one.
pub const BODY_READ_FAILED: &str = "fetch: body read failed: error decoding response body";

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
    /// A malformed body (a bad chunk-size line) fails the read with node's
    /// coded parse error instead of [`BODY_READ_FAILED`]: http.request over
    /// an agent's socket reads its body here and reports what node's parser
    /// reports. fetch keeps the one text it has always had.
    coded: bool,
}

/// The body could not be read (a wire failure or a corrupt encoding). The op
/// reports [`BODY_READ_FAILED`], or for a coded body whose framing was
/// malformed (`parse`), node's parse error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyReadError {
    pub parse: bool,
}

/// hyper reports malformed chunked framing as a body error whose source is an
/// `io::Error` of kind InvalidInput / InvalidData ("Invalid chunk size
/// line"); a connection that ends early is a different kind.
fn is_framing_error(error: &hyper::Error) -> bool {
    let mut current: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(error);
    while let Some(e) = current {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            return matches!(
                io.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData
            );
        }
        current = e.source();
    }
    false
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
                    return decoder.finish().map_err(|_| BodyReadError { parse: false });
                }
                match decoder.push(&mut self.pending) {
                    Err(_) => return Err(self.fail(false)),
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
                Some(Err(e)) => return Err(self.fail(is_framing_error(&e))),
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

    fn fail(&mut self, parse: bool) -> BodyReadError {
        self.incoming = None;
        self.pending = Bytes::new();
        BodyReadError { parse }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// `fetchBodyRead`: one chunk of the body under `handle` (`Bytes`), `Done`
/// at the end, [`BODY_READ_FAILED`] on a failure.
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
        // llhttp's code and text for a bad chunk-size line, the one framing
        // error hyper leaves in the body.
        Err(BodyReadError { parse: true }) if body.coded => OpOutcome::node_failed(
            "HPE_INVALID_CHUNK_SIZE",
            "Parse Error: Invalid character in chunk size",
        ),
        Err(_) => OpOutcome::Failed(BODY_READ_FAILED.to_string()),
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
