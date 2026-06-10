//! oam_core: the engine-agnostic async substrate.
//!
//! Owns the tokio runtime and the op-completion channel. Deliberately
//! v8-free: ops are spawned as plain futures producing [`OpOutcome`]
//! payloads; oam_engine bridges completions to V8 promises on the isolate
//! thread. Completions travel over a std::sync::mpsc channel because the
//! event loop consumes them from synchronous code (recv_timeout doubles as
//! the loop's idle sleep).
//!
//! Roadmap: the #[op] macro, the io_uring/IOCP completion IoDriver, and
//! per-workload tokio tuning land here as the op surface grows.

use std::future::Future;
use std::sync::mpsc;
use std::time::Instant;

pub use oam_diagnostics as diagnostics;

pub type OpId = u64;

/// What an async op produced. v8-free by design; the engine maps these to
/// promise resolutions (Done -> undefined, Text -> string, Failed -> reject
/// with Error(message)).
#[derive(Debug)]
pub enum OpOutcome {
    Done,
    Text(String),
    Failed(String),
}

#[derive(Debug)]
pub struct OpCompletion {
    pub id: OpId,
    pub outcome: OpOutcome,
}

pub struct CoreRuntime {
    tokio: tokio::runtime::Runtime,
    tx: mpsc::Sender<OpCompletion>,
    rx: mpsc::Receiver<OpCompletion>,
    next_id: OpId,
    inflight: usize,
}

impl CoreRuntime {
    pub fn new() -> std::io::Result<Self> {
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("oam-io")
            .enable_all()
            .build()?;
        let (tx, rx) = mpsc::channel();
        Ok(Self {
            tokio,
            tx,
            rx,
            next_id: 1,
            inflight: 0,
        })
    }

    /// Spawn an async op; its completion will surface via try_recv /
    /// recv_deadline tagged with the returned id.
    pub fn spawn_op<F>(&mut self, op: F) -> OpId
    where
        F: Future<Output = OpOutcome> + Send + 'static,
    {
        let id = self.next_id;
        self.next_id += 1;
        self.inflight += 1;
        let tx = self.tx.clone();
        self.tokio.spawn(async move {
            let outcome = op.await;
            // Receiver dropped means the runtime is shutting down: fine.
            let _ = tx.send(OpCompletion { id, outcome });
        });
        id
    }

    pub fn has_inflight(&self) -> bool {
        self.inflight > 0
    }

    pub fn try_recv(&mut self) -> Option<OpCompletion> {
        let completion = self.rx.try_recv().ok();
        if completion.is_some() {
            self.inflight -= 1;
        }
        completion
    }

    /// Block until a completion arrives or `deadline` passes (None = wait
    /// indefinitely — only call that way when has_inflight() is true, or
    /// it blocks forever).
    pub fn recv_deadline(&mut self, deadline: Option<Instant>) -> Option<OpCompletion> {
        let completion = match deadline {
            Some(deadline) => {
                let now = Instant::now();
                if deadline <= now {
                    return self.try_recv();
                }
                self.rx.recv_timeout(deadline - now).ok()
            }
            None => self.rx.recv().ok(),
        };
        if completion.is_some() {
            self.inflight -= 1;
        }
        completion
    }
}

/// Built-in op implementations. Plain futures; the engine decides how their
/// outcomes surface in JS.
pub mod ops {
    use super::OpOutcome;
    use std::time::Duration;

    pub async fn sleep(ms: u64) -> OpOutcome {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        OpOutcome::Done
    }

    pub async fn read_text_file(path: String) -> OpOutcome {
        match tokio::fs::read_to_string(&path).await {
            Ok(text) => OpOutcome::Text(text),
            Err(e) => OpOutcome::Failed(format!("could not read {path}: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn ops_complete_and_inflight_tracks() {
        let mut core = CoreRuntime::new().unwrap();
        assert!(!core.has_inflight());
        let id = core.spawn_op(ops::sleep(5));
        assert!(core.has_inflight());
        let completion = core
            .recv_deadline(Some(Instant::now() + Duration::from_secs(5)))
            .expect("op completes");
        assert_eq!(completion.id, id);
        assert!(matches!(completion.outcome, OpOutcome::Done));
        assert!(!core.has_inflight());
    }

    #[test]
    fn recv_deadline_times_out_without_completion() {
        let mut core = CoreRuntime::new().unwrap();
        let _id = core.spawn_op(ops::sleep(60_000));
        let start = Instant::now();
        let completion = core.recv_deadline(Some(start + Duration::from_millis(30)));
        assert!(completion.is_none());
        assert!(core.has_inflight());
    }

    #[test]
    fn read_text_file_fails_cleanly_on_missing_path() {
        let mut core = CoreRuntime::new().unwrap();
        core.spawn_op(ops::read_text_file("/definitely/not/here.txt".into()));
        let completion = core
            .recv_deadline(Some(Instant::now() + Duration::from_secs(5)))
            .expect("op completes");
        assert!(matches!(completion.outcome, OpOutcome::Failed(_)));
    }
}
