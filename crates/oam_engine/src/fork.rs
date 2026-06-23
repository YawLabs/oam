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
//! `std::sync::mpsc::SyncSender` (bounded=1 so try_send succeeds immediately
//! when the thread is waiting, preventing runaway buffering).
//!
//! Drop: each thread loops on `recv()` which returns `Err` when the
//! `SyncSender` is dropped along with the pool. The thread exits cleanly.

use oam_core::worker::{WorkerContext, WorkerEvent};
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
    /// Flipped true the first time `ensure_warm` spawns the prewarm threads,
    /// so warming happens exactly once across concurrent `fork()` calls.
    warm_started: AtomicBool,
}

impl ForkPool {
    /// Build the pool WITHOUT spawning any prewarm threads. Warming is
    /// deferred to the first `fork()` (see `ensure_warm`), so a runtime that
    /// never forks costs nothing -- no isolates, no snapshot deserialization.
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            pool: Mutex::new(Vec::with_capacity(capacity)),
            capacity,
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
        // bounded=1: capacity for exactly one message. The pre-warmed thread
        // calls recv() and blocks immediately, so try_send succeeds the moment
        // we have a ForkRequest without any extra buffering.
        let (tx, rx) = mpsc::sync_channel::<ForkRequest>(1);
        let pool_weak = Arc::downgrade(pool);
        std::thread::Builder::new()
            .name("oam-fork-prewarm".to_string())
            .spawn(move || {
                // Pre-warm: pay the JsRuntime::new() cost before a request arrives.
                // Use the worker constructor (NO fork pool) -- a prewarm
                // isolate that installed its own pool would spawn more prewarm
                // threads, recursing until the stack overflows.
                super::init_platform();
                let mut rt = super::JsRuntime::new_worker_runtime();

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
            })
            .expect("fork prewarm thread spawns");

        // Push the slot so the pool can dispatch to it. We push after
        // spawning (not before) so the slot is only visible once the thread
        // is running and has called recv().
        if let Ok(mut guard) = pool.pool.lock() {
            guard.push(ForkSlot { tx });
        }
    }

    /// Hand off a fork request to a pre-warmed slot if available, or fall
    /// back to a cold spawn (same as the existing `spawn_worker` path).
    pub fn fork(
        self: &Arc<Self>,
        script: PathBuf,
        worker_data: Option<String>,
        worker_id: u64,
        event_tx: mpsc::Sender<WorkerEvent>,
        msg_rx: mpsc::Receiver<Vec<u8>>,
    ) {
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
                match slot.tx.try_send(req) {
                    Ok(()) => (),
                    // Slot full (thread still alive but busy): cold spawn.
                    Err(mpsc::TrySendError::Full(req)) => {
                        cold_spawn(
                            req.script,
                            req.worker_data,
                            req.worker_id,
                            req.msg_rx,
                            event_tx,
                        );
                    }
                    // Slot disconnected: the prewarm thread died (e.g. panicked
                    // before recv), so this slot's tx had no live receiver. We
                    // just removed that dead slot from the pool above, and the
                    // dead thread never reached the refill path -- so without a
                    // refill here the pool would shrink by one permanently. Spawn
                    // a fresh slot so the pool self-heals back to `capacity`.
                    Err(mpsc::TrySendError::Disconnected(req)) => {
                        cold_spawn(
                            req.script,
                            req.worker_data,
                            req.worker_id,
                            req.msg_rx,
                            event_tx,
                        );
                        Self::add_slot(self);
                    }
                }
            }
            None => {
                // Pool empty: cold spawn and let the refill path catch up.
                cold_spawn(script, worker_data, worker_id, msg_rx, event_tx);
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
) {
    crate::worker::spawn_worker(script, worker_data, worker_id, msg_rx, event_tx);
}

/// Execute a `ForkRequest` on an already-initialized `JsRuntime`.
fn run_fork_request(rt: &mut super::JsRuntime, req: ForkRequest) -> i32 {
    // Match the cold-fork path (worker::run_worker): set process.argv so a
    // forked worker sees process.argv[1] == script path. Without this, argv is
    // present on cold spawns but ABSENT on warm spawns -- non-deterministic by
    // pool state. Per-request: the script path isn't known until the request.
    rt.set_process_argv(vec![
        "oam".to_string(),
        req.script.to_string_lossy().into_owned(),
    ]);

    let ctx = WorkerContext {
        inbox: std::sync::Arc::new(Mutex::new(Some(req.msg_rx))),
        outbox: req.event_tx.clone(),
        thread_id: req.worker_id,
        worker_data: req.worker_data,
    };

    if let Err(diagnostics) = rt.execute_worker(&req.script, ctx) {
        for d in &diagnostics {
            eprintln!("fork worker {}: {}", req.worker_id, d.message);
        }
        let _ = req.event_tx.send(WorkerEvent::Error(
            diagnostics
                .first()
                .map(|d| d.message.clone())
                .unwrap_or_else(|| "unknown fork worker error".to_string()),
        ));
        let _ = req.event_tx.send(WorkerEvent::Exit(1));
        return 1;
    }

    let code = rt.process_exit_code().unwrap_or(0);
    let _ = req.event_tx.send(WorkerEvent::Exit(code));
    code
}
