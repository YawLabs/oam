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
//! than flipping once, and also says whether a connection has the request
//! now: a reused connection that hands the request back unsent takes it off
//! ([`Dispatched::unsent`]), which stops the limit while the pool dials a
//! fresh one, and that connection's checkout starts it again from zero. undici
//! re-queues such a request with no timer and arms a new one on the socket
//! that next carries it, so the re-dial counts for nothing there either.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::watch;

use crate::OpOutcome;

/// Open signals by handle (ids from the runtime's shared handle allocator).
pub type SentSignals = Arc<Mutex<HashMap<u64, Signal>>>;

/// Where a request stands with the pool: how many times a connection was
/// checked out for it, and whether one has it now.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Checkouts {
    count: u64,
    on_connection: bool,
    /// The connection the last checkout put the request on, when the
    /// connector named it (a plain-TCP h1 connection): what JS closes, or
    /// resets, for `req.socket.destroy()` / `resetAndDestroy()` before the
    /// response head ([`super::connector::ConnCloser`]).
    connection: Option<u64>,
}

/// One request's signal. The sender is here until the fetch takes it.
pub struct Signal {
    sender: Option<watch::Sender<Checkouts>>,
    receiver: watch::Receiver<Checkouts>,
}

/// The sending half, carried by the request (an `http::Request` extension).
/// Clones share one signal: a redirect hop or a resend fires the same one.
#[derive(Clone, Debug)]
pub struct Dispatched(Arc<watch::Sender<Checkouts>>);

impl Dispatched {
    /// A signal no JS listens to: the send loop's own, for a headers
    /// timeout ([`headers_deadline`]).
    pub(crate) fn new() -> Dispatched {
        Dispatched(Arc::new(watch::Sender::new(Checkouts::default())))
    }

    /// A connection has the request: one more checkout.
    pub fn fire(&self) {
        self.fire_on(None);
    }

    /// [`fire`](Dispatched::fire), naming the connection that has the
    /// request when the connector gave it an id.
    pub(crate) fn fire_on(&self, connection: Option<u64>) {
        self.0.send_modify(|state| {
            state.count += 1;
            state.on_connection = true;
            state.connection = connection;
        });
    }

    /// The connection handed the request back unsent, and the pool is
    /// dialling another: until that one's [`fire`](Dispatched::fire), no
    /// connection has it. The checkout count stays -- node's request on a
    /// stale keep-alive socket has already finished.
    pub(crate) fn unsent(&self) {
        self.0.send_modify(|state| state.on_connection = false);
    }

    /// A receiver that hears the next change, not any before it.
    pub(crate) fn subscribe(&self) -> watch::Receiver<Checkouts> {
        self.0.subscribe()
    }
}

/// The next change on `state`: `Some(true)` for a checkout, `Some(false)` for
/// a request handed back unsent, `None` once the sender is gone (no change
/// can follow). An unsent and a re-checkout the receiver had no time to tell
/// apart read as the checkout, which is what they add up to.
async fn next_change(state: &mut watch::Receiver<Checkouts>) -> Option<bool> {
    state.changed().await.ok()?;
    Some(state.borrow_and_update().on_connection)
}

/// undici's `headersTimeout` for one send: resolves `limit` after the pool
/// checks out a connection for the request, and never if none comes. A later
/// checkout starts it again from zero, and a connection that hands the
/// request back unsent stops it until the next checkout, so only time on a
/// connection that has the request counts. undici arms it in client-h1.js's
/// resumeH1, once the request is on a connected socket, so DNS (a replaced
/// `dns.lookup` or a `connect.lookup` hook, which park the fetch before it
/// gets here), the TCP connect (a re-dial's too), a proxy tunnel and the
/// TLS handshake count for nothing. The caller races it against the
/// send, so the head that arrives in time drops it.
pub(crate) async fn headers_deadline(mut state: watch::Receiver<Checkouts>, limit: Duration) {
    loop {
        // No connection has the request: wait for one.
        match next_change(&mut state).await {
            Some(true) => {}
            Some(false) => continue,
            None => return std::future::pending().await,
        }
        let sleep = tokio::time::sleep(limit);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                () = &mut sleep => return,
                change = next_change(&mut state) => match change {
                    Some(true) => sleep.as_mut().reset(tokio::time::Instant::now() + limit),
                    // Handed back unsent: stop until the re-dial's checkout.
                    Some(false) => break,
                    // The sender went with the request: no checkout can follow.
                    None => return (&mut sleep).await,
                },
            }
        }
    }
}

fn lock(signals: &SentSignals) -> MutexGuard<'_, HashMap<u64, Signal>> {
    signals.lock().unwrap_or_else(|e| e.into_inner())
}

/// `fetchSentOpen`: a new signal under `handle`.
pub fn open(signals: &SentSignals, handle: u64) {
    let (sender, receiver) = watch::channel(Checkouts::default());
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

/// `fetchSentWait`: once the request has a connection, `Json` of that
/// connection's id when the connector named one, else `Json("true")`; `Done`
/// if it ended without one, or the signal was closed first.
pub async fn wait(signals: SentSignals, handle: u64) -> OpOutcome {
    let receiver = lock(&signals)
        .get(&handle)
        .map(|signal| signal.receiver.clone());
    let Some(mut receiver) = receiver else {
        return OpOutcome::Done;
    };
    match receiver.wait_for(|state| state.count > 0).await {
        // The connection's id when the connector named it, else `true`.
        Ok(state) => OpOutcome::Json(match state.connection {
            Some(connection) => connection.to_string(),
            None => "true".to_string(),
        }),
        Err(_) => OpOutcome::Done,
    }
}

/// `fetchSentClose`: drop `handle`'s signal and say whether it had fired. A
/// sender no fetch took goes with it, which ends a parked [`wait`].
pub fn close(signals: &SentSignals, handle: u64) -> bool {
    lock(signals)
        .remove(&handle)
        .is_some_and(|signal| signal.receiver.borrow().count > 0)
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
    async fn a_request_handed_back_unsent_stops_the_headers_deadline_until_the_next_checkout() {
        let dispatched = Dispatched::new();
        let deadline = headers_deadline(dispatched.subscribe(), LIMIT);
        tokio::pin!(deadline);
        // A reused connection has it, then hands it back unsent.
        dispatched.fire();
        assert!(
            tokio::time::timeout(LIMIT * 2 / 3, &mut deadline)
                .await
                .is_err()
        );
        dispatched.unsent();
        // The re-dial takes longer than the limit: none of it counts.
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
    async fn an_unsent_hand_back_still_counts_as_sent_for_http_request() {
        let signals: SentSignals = Arc::default();
        open(&signals, 7);
        let dispatched = take(&signals, 7).expect("an open signal");
        dispatched.fire();
        dispatched.unsent();
        assert!(matches!(
            wait(signals.clone(), 7).await,
            OpOutcome::Json(ref json) if json == "true"
        ));
        assert!(close(&signals, 7));
    }

    #[tokio::test]
    async fn a_checkout_before_the_subscription_does_not_start_it() {
        let dispatched = Dispatched::new();
        dispatched.fire();
        let deadline = headers_deadline(dispatched.subscribe(), LIMIT);
        assert!(tokio::time::timeout(LIMIT * 3, deadline).await.is_err());
    }
}
