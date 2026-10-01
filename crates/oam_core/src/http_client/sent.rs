//! "This request has a connection": the one thing a client needs to hear from
//! oam's own transport between dispatching a request and its response head.
//! Two clients listen for it: `http.request`, for node's `'finish'` (#193),
//! and undici's `headersTimeout`, which starts there.
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
//!
//! undici's `headersTimeout` needs the same moment and no JS round trip: the
//! send loop makes its own [`Dispatched`] ([`Dispatched::new`]) when the
//! request has a headers timeout, and [`headers_deadline`] runs the timer
//! from each checkout, in the op itself. A signal counts checkouts rather
//! than flipping once, so a request the pool re-sends on a fresh connection
//! (a reused one handed it back unsent) restarts the limit there, as
//! undici's does on the socket that next carries a request it re-queued.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::watch;

use crate::OpOutcome;

/// Open signals by handle (ids from the runtime's shared handle allocator).
pub type SentSignals = Arc<Mutex<HashMap<u64, Signal>>>;

/// One request's signal: how many times a connection was checked out for
/// it. The sender is here until the fetch takes it.
pub struct Signal {
    sender: Option<watch::Sender<u64>>,
    receiver: watch::Receiver<u64>,
}

/// The sending half, carried by the request (an `http::Request` extension).
/// Clones share one signal: a redirect hop or a resend fires the same one.
#[derive(Clone, Debug)]
pub struct Dispatched(Arc<watch::Sender<u64>>);

impl Dispatched {
    /// A signal no JS listens to: the send loop's own, for a headers
    /// timeout ([`headers_deadline`]).
    pub(crate) fn new() -> Dispatched {
        Dispatched(Arc::new(watch::Sender::new(0)))
    }

    /// A connection has the request: one more checkout.
    pub fn fire(&self) {
        self.0.send_modify(|checkouts| *checkouts += 1);
    }

    /// A receiver that hears the next checkout, not any before it.
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.0.subscribe()
    }
}

/// undici's `headersTimeout` for one send: resolves `limit` after the pool
/// checks out a connection for the request, restarting at each later
/// checkout, and never if none comes. undici arms it in client-h1.js's
/// resumeH1, once the request is on a connected socket, so DNS (a replaced
/// `dns.lookup` or a `connect.lookup` hook, which park the fetch before it
/// gets here), the TCP connect, a proxy tunnel and the TLS handshake count
/// for nothing. The caller races it against the send, so the head that
/// arrives in time drops it.
pub(crate) async fn headers_deadline(mut checkouts: watch::Receiver<u64>, limit: Duration) {
    if checkouts.changed().await.is_err() {
        return std::future::pending().await;
    }
    let sleep = tokio::time::sleep(limit);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            () = &mut sleep => return,
            changed = checkouts.changed() => match changed {
                Ok(()) => sleep.as_mut().reset(tokio::time::Instant::now() + limit),
                // The sender went with the request: no checkout can follow.
                Err(_) => return (&mut sleep).await,
            },
        }
    }
}

fn lock(signals: &SentSignals) -> MutexGuard<'_, HashMap<u64, Signal>> {
    signals.lock().unwrap_or_else(|e| e.into_inner())
}

/// `fetchSentOpen`: a new signal under `handle`.
pub fn open(signals: &SentSignals, handle: u64) {
    let (sender, receiver) = watch::channel(0);
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
    match receiver.wait_for(|checkouts| *checkouts > 0).await {
        Ok(_) => OpOutcome::Json("true".to_string()),
        Err(_) => OpOutcome::Done,
    }
}

/// `fetchSentClose`: drop `handle`'s signal and say whether it had fired. A
/// sender no fetch took goes with it, which ends a parked [`wait`].
pub fn close(signals: &SentSignals, handle: u64) -> bool {
    lock(signals)
        .remove(&handle)
        .is_some_and(|signal| *signal.receiver.borrow() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real time (the workspace's tokio has no test-util): a deadline is
    // never early, and the upper bounds are loose.
    const LIMIT: Duration = Duration::from_millis(150);

    #[tokio::test]
    async fn the_headers_deadline_waits_for_a_checkout() {
        let dispatched = Dispatched::new();
        let deadline = headers_deadline(dispatched.subscribe(), LIMIT);
        tokio::pin!(deadline);
        // No connection yet: however long the dial takes, nothing is due.
        assert!(
            tokio::time::timeout(LIMIT * 3, &mut deadline)
                .await
                .is_err()
        );
        dispatched.fire();
        let start = std::time::Instant::now();
        deadline.await;
        let elapsed = start.elapsed();
        assert!(elapsed >= LIMIT && elapsed < LIMIT * 3, "{elapsed:?}");
    }

    #[tokio::test]
    async fn a_later_checkout_restarts_the_headers_deadline() {
        let dispatched = Dispatched::new();
        let deadline = headers_deadline(dispatched.subscribe(), LIMIT);
        tokio::pin!(deadline);
        dispatched.fire();
        assert!(
            tokio::time::timeout(LIMIT * 2 / 3, &mut deadline)
                .await
                .is_err()
        );
        dispatched.fire();
        let start = std::time::Instant::now();
        deadline.await;
        let elapsed = start.elapsed();
        assert!(elapsed >= LIMIT && elapsed < LIMIT * 3, "{elapsed:?}");
    }

    #[tokio::test]
    async fn a_checkout_before_the_subscription_does_not_start_it() {
        let dispatched = Dispatched::new();
        dispatched.fire();
        let deadline = headers_deadline(dispatched.subscribe(), LIMIT);
        assert!(tokio::time::timeout(LIMIT * 3, deadline).await.is_err());
    }
}
