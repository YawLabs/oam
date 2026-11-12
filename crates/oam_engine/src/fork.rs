//! Pre-warmed isolate pool: oam.fork() checkpoint pools.
//!
//! `ForkPool` spawns OS threads that each call `JsRuntime::new()` and then
//! block waiting for a `ForkRequest`. When `fork()` is called, the first
//! available pre-warmed slot receives the (script, worker_data) pair and
//! immediately executes it -- the cold `JsRuntime::new()` cost is already paid.
//!
//! Warming is LAZY: `ForkPool::new()` installs no threads. The prewarm
//! isolates are spawned on the FIRST `fork()` call (`ensure_warm`). A program
//! that never forks therefore pays nothing -- no extra V8 isolates, no snapshot
//! deserialization, lower idle RSS, faster startup. The cost is that the first
//! fork pays the cold `JsRuntime::new()` (its request is buffered onto a
//! still-warming slot); every fork after that is served warm.
//!
//! JsRuntime is NOT Send (holds V8 isolate-thread locals), so pre-warmed
//! isolates stay pinned to their creation thread. The pool communicates over
//! `std::sync::mpsc::SyncSender` (bounded=1: a slot leaves the pool before
//! its one request is sent, so the send never waits and nothing queues up).
//!
//! A fork served by a slot runs the routine a cold spawn runs once its
//! isolate exists (`worker::run_worker_on`), so what a forked script observes
//! does not depend on whether a slot was free.
//!
//! Drop: each thread loops on `recv()` which returns `Err` when the
//! `SyncSender` is dropped along with the pool. The thread exits cleanly.

use oam_core::worker::WorkerEvent;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

/// Request sent from the pool to a pre-warmed thread.
pub struct ForkRequest {
    pub script: PathBuf,
    pub worker_data: Option<String>,
    pub worker_id: u64,
    /// Channel back to the parent: the pre-warmed thread forwards
    /// WorkerEvents here (same shape as the cold worker path).
    pub event_tx: mpsc::Sender<WorkerEvent>,
    /// Channel from the parent to the worker for postMessage.
    pub msg_rx: mpsc::Receiver<Vec<u8>>,
}

struct ForkSlot {
    tx: mpsc::SyncSender<ForkRequest>,
}

pub struct ForkPool {
    pool: Mutex<Vec<ForkSlot>>,
    capacity: usize,
    /// The spawning runtime's permissions, handed to every pre-warmed isolate
    /// so a forked script cannot outrank its parent.
    permissions: Arc<crate::permissions::Permissions>,
    /// Flipped true the first time `ensure_warm` spawns the prewarm threads,
    /// so warming happens exactly once across concurrent `fork()` calls.
    warm_started: AtomicBool,
}

impl ForkPool {
    /// Build the pool WITHOUT spawning any prewarm threads. Warming is
    /// deferred to the first `fork()` (see `ensure_warm`), so a runtime that
    /// never forks costs nothing -- no isolates, no snapshot deserialization.
    pub fn new(capacity: usize, permissions: Arc<crate::permissions::Permissions>) -> Arc<Self> {
        Arc::new(Self {
            pool: Mutex::new(Vec::with_capacity(capacity)),
            capacity,
            permissions,
            warm_started: AtomicBool::new(false),
        })
    }

    /// Spawn the `capacity` pre-warmed threads exactly once, on first use.
    /// Idempotent and thread-safe: concurrent first forks race on the
    /// compare-exchange and only the winner spawns.
    fn ensure_warm(pool: &Arc<Self>) {
        if pool
            .warm_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            for _ in 0..pool.capacity {
                Self::add_slot(pool);
            }
        }
    }

    /// Spawn one pre-warmed thread and push its sender into the pool.
    fn add_slot(pool: &Arc<Self>) {
        // bounded=1: capacity for exactly one message, the one request this
        // slot ever serves. It is buffered if the thread is still building
        // its isolate and handed over directly once it waits in recv().
        let (tx, rx) = mpsc::sync_channel::<ForkRequest>(1);
        let pool_weak = Arc::downgrade(pool);
        let permissions = pool.permissions.clone();
        let spawned =
            super::worker::spawn_isolate_thread("oam-fork-prewarm".to_string(), move || {
                // Pre-warm: pay the JsRuntime::new() cost before a request arrives.
                // Use the worker constructor (NO fork pool) -- a prewarm
                // isolate that installed its own pool would spawn more prewarm
                // threads, recursing until the stack overflows.
                let mut rt = super::JsRuntime::new_worker_runtime_with(permissions);

                // Block until a ForkRequest arrives or the pool drops (recv Err).
                let req = match rx.recv() {
                    Ok(req) => req,
                    Err(_) => return, // pool dropped: exit cleanly
                };

                // Run the worker script on this pre-warmed isolate.
                run_fork_request(&mut rt, req);

                // Refill: spawn a fresh slot so the pool stays at `capacity`.
                // This thread's isolate has now run a script -- it may carry
                // stale JS state, so a fresh slot gets a new isolate.
                if let Some(pool) = pool_weak.upgrade() {
                    Self::add_slot(&pool);
                }
                // Thread exits; JsRuntime drops cleanly.
            });
        // The OS refused the thread (its thread or address-space limit): no
        // slot. This runs inside a V8 callback (the first fork) or on a
        // finishing prewarm thread, where a panic would end the process; the
        // pool is an optimization, and a fork that finds no slot cold-spawns
        // (and reports its own spawn failure to the caller).
        if spawned.is_err() {
            return;
        }

        // Push the slot so the pool can dispatch to it, after the spawn so a
        // slot always has a thread behind it. That thread may still be inside
        // `JsRuntime::new`: the channel holds one request, so a fork that
        // takes the slot early is buffered and served when the isolate is up.
        if let Ok(mut guard) = pool.pool.lock() {
            guard.push(ForkSlot { tx });
        }
    }

    /// Hand off a fork request to a pre-warmed slot if available, or fall
    /// back to a cold spawn (same as the existing `spawn_worker` path).
    /// `Err` only when that cold spawn's thread cannot be created.
    pub fn fork(
        self: &Arc<Self>,
        script: PathBuf,
        worker_data: Option<String>,
        worker_id: u64,
        event_tx: mpsc::Sender<WorkerEvent>,
        msg_rx: mpsc::Receiver<Vec<u8>>,
    ) -> std::io::Result<()> {
        // First fork warms the pool; later forks find slots already spawned.
        Self::ensure_warm(self);

        let slot = {
            let mut guard = self.pool.lock().unwrap_or_else(|e| e.into_inner());
            if guard.is_empty() {
                None
            } else {
                Some(guard.remove(0))
            }
        };

        match slot {
            Some(slot) => {
                let req = ForkRequest {
                    script,
                    worker_data,
                    worker_id,
                    event_tx: event_tx.clone(),
                    msg_rx,
                };
                // The slot was taken out of the pool above and this is the
                // only send its channel ever sees, so the one-request buffer
                // is empty: the send cannot block, and fails only when the
                // receiver is gone.
                match slot.tx.send(req) {
                    Ok(()) => Ok(()),
                    // Slot disconnected: the prewarm thread died (e.g. panicked
                    // before recv), so this slot's tx had no live receiver. We
                    // just removed that dead slot from the pool above, and the
                    // dead thread never reached the refill path -- so without a
                    // refill here the pool would shrink by one permanently. Spawn
                    // a fresh slot so the pool self-heals back to `capacity`.
                    Err(mpsc::SendError(req)) => {
                        let spawned = cold_spawn(
                            req.script,
                            req.worker_data,
                            req.worker_id,
                            req.msg_rx,
                            event_tx,
                            self.permissions.clone(),
                        );
                        Self::add_slot(self);
                        spawned
                    }
                }
            }
            None => {
                // Pool empty: cold spawn and let the refill path catch up.
                cold_spawn(
                    script,
                    worker_data,
                    worker_id,
                    msg_rx,
                    event_tx,
                    self.permissions.clone(),
                )
            }
        }
    }
}

/// Cold-start a worker thread (delegates to the existing `spawn_worker`).
fn cold_spawn(
    script: PathBuf,
    worker_data: Option<String>,
    worker_id: u64,
    msg_rx: mpsc::Receiver<Vec<u8>>,
    event_tx: mpsc::Sender<WorkerEvent>,
    permissions: Arc<crate::permissions::Permissions>,
) -> std::io::Result<()> {
    crate::worker::spawn_worker(
        script,
        worker_data,
        worker_id,
        msg_rx,
        event_tx,
        crate::worker::WorkerOptions {
            pipe_stdout: false,
            pipe_stderr: false,
            exec_argv: Vec::new(),
            // oam.fork() reads the process environment.
            env: None,
            inherit_flags: true,
            creator_flags: None,
        },
        permissions,
    )
    // Not joined: the registry holds a fork's channels, not its thread.
    .map(drop)
}

/// Execute a `ForkRequest` on an already-initialized `JsRuntime`.
///
/// The same routine a cold spawn runs after building its isolate
/// (`worker::run_worker_on`), so a fork behaves alike whether or not a slot
/// was free: process.argv, the creator's flags and the startup check,
/// 'online', the entry's resolution, the worker's own process 'exit'
/// listeners, and a fatal failure handed to the parent as the thrown value.
fn run_fork_request(rt: &mut super::JsRuntime, req: ForkRequest) {
    let event_tx = req.event_tx;
    let code = crate::worker::run_worker_on(
        rt,
        &req.script,
        req.worker_data,
        req.worker_id,
        req.msg_rx,
        event_tx.clone(),
        crate::worker::WorkerStart::for_fork(),
    );
    let _ = event_tx.send(WorkerEvent::Exit(code));
}
