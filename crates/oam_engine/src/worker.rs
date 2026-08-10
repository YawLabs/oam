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
    /// Execute a worker thread entry. Sets up worker context (parentPort,
    /// isMainThread=false, workerData) before loading the script. Called on
    /// the WORKER thread, not the parent.
    pub(crate) fn execute_worker(
        &mut self,
        entry: &Path,
        ctx: WorkerContext,
    ) -> Result<(), Vec<Diagnostic>> {
        // An ESM entry has to go through the module graph. load_cjs compiles
        // it as a classic script, so `new Worker('x.mjs')` threw on its first
        // import statement -- the ESM guard lives in the require() callback,
        // which an entry never passes through.
        if oam_loader::module_kind(entry) == oam_loader::ModuleKind::Esm {
            let host = crate::worker_host().ok_or_else(|| {
                vec![Diagnostic::new(
                    "OAM-MOD0003",
                    oam_diagnostics::Severity::Error,
                    oam_diagnostics::Origin::Resolve,
                    format!(
                        "no module host registered for ESM worker entries: {}",
                        entry.display()
                    ),
                )]
            })?;
            self.reset_run_slots()?;
            self.isolate.set_slot(ctx);
            return self.execute_module(entry, host.as_ref());
        }
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
        if let Some(failure) = crate::modules::run_ticks_and_microtasks(tc) {
            return Err(failure);
        }
        if let Some(failure) = crate::modules::drain_uncaught(tc) {
            return Err(failure);
        }
        crate::modules::pump_event_loop(tc, None, true)?;
        match crate::modules::unhandled_rejection_failures(tc) {
            Some(failures) => Err(failures),
            None => Ok(()),
        }
    }
}

/// Spawn a worker thread. Returns (worker_id, thread_id). The caller
/// must store the returned handles in the parent's WorkerRegistry.
pub(crate) struct WorkerOptions {
    pub pipe_stdout: bool,
    pub pipe_stderr: bool,
    pub exec_argv: Vec<String>,
}

pub(crate) fn spawn_worker(
    script_path: PathBuf,
    worker_data: Option<String>,
    worker_id: u64,
    parent_rx: mpsc::Receiver<Vec<u8>>,
    worker_tx: mpsc::Sender<WorkerEvent>,
    options: WorkerOptions,
    permissions: std::sync::Arc<crate::permissions::Permissions>,
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
                options,
                permissions,
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
    options: WorkerOptions,
    permissions: std::sync::Arc<crate::permissions::Permissions>,
) -> i32 {
    let WorkerOptions {
        pipe_stdout,
        pipe_stderr,
        exec_argv,
    } = options;
    // Worker isolate: no fork pool (same recursion hazard as the prewarm
    // path -- a worker that installed a pool would spawn prewarm threads that
    // each spawn more pools). A nested oam.fork() inside a worker cold-spawns
    // via op_fork_spawn's None branch.
    // INHERIT the spawning isolate's permissions: a worker used to run
    // all-granted, which made `new Worker(...)` a one-line escape from
    // --permission.
    let mut rt = super::JsRuntime::new_worker_runtime_with(permissions);
    rt.set_process_argv(vec![
        "oam".to_string(),
        script_path.to_string_lossy().into_owned(),
    ]);

    let ctx = WorkerContext {
        inbox: std::sync::Arc::new(std::sync::Mutex::new(Some(from_parent))),
        outbox: to_parent.clone(),
        thread_id,
        worker_data,
        pipe_stdout,
        pipe_stderr,
        exec_argv: exec_argv.clone(),
    };

    if !exec_argv.is_empty() {
        let json = serde_json::to_string(&exec_argv).unwrap_or_else(|_| "[]".into());
        let _ = rt.execute_script(
            "<worker-exec-argv>",
            &format!("globalThis.process.execArgv = {json};"),
        );
    }
    // Inherit the parent's process-level flags (--no-warnings etc.); a
    // fresh isolate would otherwise start with none of them.
    if let Some(js) = crate::inherited_flags_js() {
        let _ = rt.execute_script("<flags>", js);
    }
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
    // Node emits 'exit' on the worker's own process object before the thread
    // ends, and re-reads exitCode after (a handler may set it).
    rt.emit_process_exit();
    rt.process_exit_code().unwrap_or(0)
}
