//! Worker thread lifecycle: spawn, execute, teardown.
//!
//! Each worker gets its own OS thread, V8 isolate (from the shared
//! snapshot), and CoreRuntime. Communication with the parent is via
//! std::sync::mpsc channels bridged into the op channel.

use oam_core::worker::{WorkerContext, WorkerEvent};
use oam_diagnostics::Diagnostic;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

impl super::JsRuntime {
    /// Execute a CJS script as a worker thread entry. Sets up worker
    /// context (parentPort, isMainThread=false, workerData) before
    /// loading the script. Called on the WORKER thread, not the parent.
    pub(crate) fn execute_worker(
        &mut self,
        entry: &Path,
        ctx: WorkerContext,
    ) -> Result<(), Vec<Diagnostic>> {
        self.reset_run_slots()?;
        self.isolate.set_slot(ctx);

        v8::scope_with_context!(let scope, &mut self.isolate, &self.context);
        v8::tc_scope!(let tc, scope);

        if crate::cjs::load_cjs(tc, entry).is_none() {
            return Err(vec![crate::modules::catch_to_diagnostic(
                tc,
                &entry.to_string_lossy(),
            )]);
        }
        tc.perform_microtask_checkpoint();
        if let Some(failure) = crate::modules::drain_uncaught(tc) {
            return Err(failure);
        }
        crate::modules::pump_event_loop(tc, None)?;
        match crate::modules::unhandled_rejection_failures(tc) {
            Some(failures) => Err(failures),
            None => Ok(()),
        }
    }
}

/// Spawn a worker thread. Returns (worker_id, thread_id). The caller
/// must store the returned handles in the parent's WorkerRegistry.
pub(crate) fn spawn_worker(
    script_path: PathBuf,
    worker_data: Option<String>,
    worker_id: u64,
    parent_rx: mpsc::Receiver<Vec<u8>>,
    worker_tx: mpsc::Sender<WorkerEvent>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("oam-worker-{worker_id}"))
        .spawn(move || {
            let exit_code = run_worker(
                &script_path,
                worker_data,
                worker_id,
                parent_rx,
                worker_tx.clone(),
            );
            let _ = worker_tx.send(WorkerEvent::Exit(exit_code));
        })
        .expect("worker thread spawns")
}

fn run_worker(
    script_path: &Path,
    worker_data: Option<String>,
    thread_id: u64,
    from_parent: mpsc::Receiver<Vec<u8>>,
    to_parent: mpsc::Sender<WorkerEvent>,
) -> i32 {
    let mut rt = super::JsRuntime::new();
    rt.set_process_argv(vec![
        "oam".to_string(),
        script_path.to_string_lossy().into_owned(),
    ]);

    let ctx = WorkerContext {
        inbox: std::sync::Arc::new(std::sync::Mutex::new(Some(from_parent))),
        outbox: to_parent.clone(),
        thread_id,
        worker_data,
    };

    if let Err(diagnostics) = rt.execute_worker(script_path, ctx) {
        for d in &diagnostics {
            eprintln!("worker {thread_id}: {}", d.message);
        }
        let _ = to_parent.send(WorkerEvent::Error(
            diagnostics
                .first()
                .map(|d| d.message.clone())
                .unwrap_or_else(|| "unknown worker error".to_string()),
        ));
        return 1;
    }
    rt.process_exit_code().unwrap_or(0)
}
