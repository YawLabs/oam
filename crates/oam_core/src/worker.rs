//! Worker thread communication substrate.
//!
//! Channels and registries for parent<->worker message passing. V8-free:
//! the engine maps these primitives to isolate-thread ops. Uses
//! std::sync::mpsc because both sides are single-reader (one isolate
//! thread per worker). Async recv ops use spawn_blocking to bridge
//! into the tokio op channel.

use crate::OpOutcome;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc;

/// Events flowing from a worker to its parent.
#[derive(Debug)]
pub enum WorkerEvent {
    Message(Vec<u8>),
    Error(String),
    Exit(i32),
    /// Worker stdout/stderr, when the parent asked for it with
    /// `new Worker(..., { stdout: true })`. Without this the worker writes
    /// straight to the shared process fd, so the parent cannot capture it.
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

/// Parent-side handle to one worker.
pub struct WorkerHandle {
    pub to_worker: mpsc::Sender<Vec<u8>>,
    pub thread: Option<std::thread::JoinHandle<()>>,
}

/// Parent-side state: all spawned workers.
pub struct WorkerState {
    pub handles: HashMap<u64, WorkerHandle>,
    pub receivers: HashMap<u64, mpsc::Receiver<WorkerEvent>>,
    pub closed: HashSet<u64>,
    next_id: u64,
}

impl Default for WorkerState {
    fn default() -> Self {
        Self {
            handles: HashMap::new(),
            receivers: HashMap::new(),
            closed: HashSet::new(),
            next_id: 1,
        }
    }
}

impl WorkerState {
    pub fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

pub type WorkerRegistry = std::sync::Arc<std::sync::Mutex<WorkerState>>;

/// Worker-side channels. Stored as an isolate slot on the worker.
pub struct WorkerContext {
    pub inbox: std::sync::Arc<std::sync::Mutex<Option<mpsc::Receiver<Vec<u8>>>>>,
    pub outbox: mpsc::Sender<WorkerEvent>,
    pub thread_id: u64,
    pub worker_data: Option<String>,
    /// Route this worker's stdout/stderr to the parent instead of fd 1/2.
    pub pipe_stdout: bool,
    pub pipe_stderr: bool,
    /// process.execArgv inside the worker (`{ execArgv: [...] }`).
    pub exec_argv: Vec<String>,
}

fn reinsert_receiver(
    registry: &WorkerRegistry,
    worker_id: u64,
    rx: mpsc::Receiver<WorkerEvent>,
) -> bool {
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    if guard.closed.contains(&worker_id) {
        false
    } else {
        guard.receivers.insert(worker_id, rx);
        true
    }
}

/// Receive the next event from a worker (parent side). Async:
/// blocks on the channel via spawn_blocking, then returns the event.
pub async fn parent_recv(registry: WorkerRegistry, worker_id: u64) -> OpOutcome {
    let rx = registry
        .lock()
        .expect("worker registry lock")
        .receivers
        .remove(&worker_id);
    let Some(rx) = rx else {
        return OpOutcome::Done;
    };

    let result = tokio::task::spawn_blocking(move || {
        let event = rx.recv();
        (rx, event)
    })
    .await;

    match result {
        Ok((rx, Ok(event))) => {
            reinsert_receiver(&registry, worker_id, rx);
            match event {
                WorkerEvent::Message(data) => OpOutcome::Bytes(data),
                WorkerEvent::Error(msg) => OpOutcome::Json(
                    serde_json::json!({"type": "error", "message": msg}).to_string(),
                ),
                WorkerEvent::Exit(code) => {
                    OpOutcome::Json(serde_json::json!({"type": "exit", "code": code}).to_string())
                }
                // Latin-1 keeps the round-trip lossless for arbitrary bytes:
                // the JS side maps each code unit straight back to a byte.
                WorkerEvent::Stdout(bytes) => OpOutcome::Json(
                    serde_json::json!({
                        "type": "stdout",
                        "data": bytes.iter().map(|b| *b as char).collect::<String>(),
                    })
                    .to_string(),
                ),
                WorkerEvent::Stderr(bytes) => OpOutcome::Json(
                    serde_json::json!({
                        "type": "stderr",
                        "data": bytes.iter().map(|b| *b as char).collect::<String>(),
                    })
                    .to_string(),
                ),
            }
        }
        Ok((_, Err(_))) => OpOutcome::Done,
        Err(e) => OpOutcome::Failed(format!("worker recv task: {e}")),
    }
}

/// Send a message to a worker (parent -> worker). Synchronous.
pub fn parent_post(registry: &WorkerRegistry, worker_id: u64, data: Vec<u8>) -> Result<(), String> {
    let guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    let Some(handle) = guard.handles.get(&worker_id) else {
        return Err(format!("worker {worker_id} not found"));
    };
    handle
        .to_worker
        .send(data)
        .map_err(|_| format!("worker {worker_id} channel closed"))
}

/// Terminate a worker. Removes all state.
pub fn parent_terminate(registry: &WorkerRegistry, worker_id: u64) {
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    guard.handles.remove(&worker_id);
    guard.receivers.remove(&worker_id);
    guard.closed.insert(worker_id);
}

/// Receive a message from the parent (worker side). Async: blocks
/// on the inbox via spawn_blocking.
pub async fn worker_recv(
    inbox: std::sync::Arc<std::sync::Mutex<Option<mpsc::Receiver<Vec<u8>>>>>,
) -> OpOutcome {
    let rx = inbox.lock().unwrap_or_else(|e| e.into_inner()).take();
    let Some(rx) = rx else {
        return OpOutcome::Done;
    };

    let result = tokio::task::spawn_blocking(move || {
        let event = rx.recv();
        (rx, event)
    })
    .await;

    match result {
        Ok((rx, Ok(data))) => {
            *inbox.lock().unwrap_or_else(|e| e.into_inner()) = Some(rx);
            OpOutcome::Bytes(data)
        }
        Ok((_, Err(_))) => OpOutcome::Done,
        Err(e) => OpOutcome::Failed(format!("worker inbox recv task: {e}")),
    }
}

/// Send a message to the parent (worker -> parent). Synchronous.
pub fn worker_post(outbox: &mpsc::Sender<WorkerEvent>, data: Vec<u8>) -> Result<(), String> {
    outbox
        .send(WorkerEvent::Message(data))
        .map_err(|_| "parent channel closed".to_string())
}
