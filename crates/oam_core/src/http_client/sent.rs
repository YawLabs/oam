//! "This request has a connection": the one thing `http.request` needs to
//! hear from oam's own transport between dispatching a request and its
//! response head (#193).
//!
//! node emits a request's `'finish'` -- and calls its `write()` callbacks --
//! once the socket has written it, which cannot be before the socket
//! connected: a request whose connection is refused gets neither. On oam's own
//! transport the fetch op resolves only at the response head, so JS had no
//! way to tell a request that left from one that never could, and reported
//! every request as written.
//!
//! JS opens a signal ([`open`]) and names it in the request
//! (`FetchRequest::sent_signal`). The engine's fetch op takes the signal's
//! sending half ([`take`]) and the request carries it, as a request
//! extension, down to the pool, which fires it the moment a connection --
//! reused or freshly dialled -- is checked out for the request
//! ([`Dispatched::fire`]). hyper writes the head as soon as it is handed the
//! request, so that is node's moment: a connected socket and the request
//! queued on it. A pooled connection the server had already closed fires too,
//! and the request then fails or is resent -- as node's request on a stale
//! keep-alive socket has finished before it hears `ECONNRESET`.
//!
//! [`wait`] resolves when it fires, or with `Done` when the request ended
//! without a connection (every sender is dropped with the fetch). [`close`]
//! is the synchronous read JS makes when the fetch settles: the wait's
//! completion and the fetch's travel the same completion channel from
//! separate tasks, so the wait may be the later one (#190 was this race on
//! the agent path).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::watch;

use crate::OpOutcome;

/// Open signals by handle (ids from the runtime's shared handle allocator).
pub type SentSignals = Arc<Mutex<HashMap<u64, Signal>>>;

/// One request's signal. The sender is here until the fetch takes it.
pub struct Signal {
    sender: Option<watch::Sender<bool>>,
    receiver: watch::Receiver<bool>,
}

/// The sending half, carried by the request (an `http::Request` extension).
/// Clones share one signal: a redirect hop or a resend fires the same one.
#[derive(Clone, Debug)]
pub struct Dispatched(Arc<watch::Sender<bool>>);

impl Dispatched {
    /// A connection has the request. Idempotent.
    pub fn fire(&self) {
        self.0
            .send_if_modified(|fired| !std::mem::replace(fired, true));
    }
}

fn lock(signals: &SentSignals) -> MutexGuard<'_, HashMap<u64, Signal>> {
    signals.lock().unwrap_or_else(|e| e.into_inner())
}

/// `fetchSentOpen`: a new signal under `handle`.
pub fn open(signals: &SentSignals, handle: u64) {
    let (sender, receiver) = watch::channel(false);
    lock(signals).insert(
        handle,
        Signal {
            sender: Some(sender),
            receiver,
        },
    );
}

/// The sending half of `handle`'s signal, for the fetch that names it. `None`
/// for a handle that is not open or whose sender a fetch already took.
pub fn take(signals: &SentSignals, handle: u64) -> Option<Dispatched> {
    let sender = lock(signals).get_mut(&handle)?.sender.take()?;
    Some(Dispatched(Arc::new(sender)))
}

/// `fetchSentWait`: `Json("true")` once the request has a connection; `Done`
/// if it ended without one, or the signal was closed first.
pub async fn wait(signals: SentSignals, handle: u64) -> OpOutcome {
    let receiver = lock(&signals)
        .get(&handle)
        .map(|signal| signal.receiver.clone());
    let Some(mut receiver) = receiver else {
        return OpOutcome::Done;
    };
    match receiver.wait_for(|fired| *fired).await {
        Ok(_) => OpOutcome::Json("true".to_string()),
        Err(_) => OpOutcome::Done,
    }
}

/// `fetchSentClose`: drop `handle`'s signal and say whether it had fired. A
/// sender no fetch took goes with it, which ends a parked [`wait`].
pub fn close(signals: &SentSignals, handle: u64) -> bool {
    lock(signals)
        .remove(&handle)
        .is_some_and(|signal| *signal.receiver.borrow())
}
