use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use super::OpOutcome;
use super::child::ChildRegistry;

pub async fn cluster_fork(
    children: ChildRegistry,
    ids: Arc<AtomicU64>,
    script_path: String,
    worker_id: String,
    env_pairs: Vec<(String, String)>,
) -> OpOutcome {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "oam".to_string());

    let mut cmd = tokio::process::Command::new(&exe);
    cmd.arg("run");
    cmd.arg(&script_path);
    // node's cluster.fork hands the worker {...process.env, ...env}, and that
    // is its whole environment: the JS layer builds it (childProcessEnv), so
    // nothing of this process's own environment is added here.
    cmd.env_clear();
    for (k, v) in &env_pairs {
        cmd.env(k, v);
    }
    cmd.env(WORKER_ID_VAR, &worker_id);
    cmd.stdout(std::process::Stdio::inherit());
    cmd.stderr(std::process::Stdio::inherit());
    // Reap workers if the primary's runtime drops -- Node leaves orphans, but
    // here a leaked worker would outlive its only manager (and wedge tests).
    cmd.kill_on_drop(true);

    // The flags every other child gets (libuv's table): not detached, no
    // windowsHide, stdio inherited. A worker shares this process's console;
    // CREATE_NO_WINDOW is added only when this process has none. Setting it
    // unconditionally started a fresh hidden conhost for every worker while
    // its stdio slots were inherited, the combination libuv rules out.
    // And libuv's show state for a child without windowsHide (SW_SHOWDEFAULT
    // under STARTF_USESHOWWINDOW), as spawn_child gives every other child.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(super::child::windows_creation_flags(false, false, true));
        cmd.as_std_mut()
            .show_window(super::child_win::show_window_for(false));
    }

    match cmd.spawn() {
        Ok(child) => {
            // kill_on_drop covers the runtime dropping; the job covers the
            // primary being KILLED, which never runs a drop. node's cluster
            // forks through uv_spawn, so its workers die that way too.
            #[cfg(windows)]
            super::job_win::adopt_tokio_child(&child);
            let pid = child.id().unwrap_or(0);
            let handle = ids.fetch_add(1, Ordering::Relaxed);
            let mut guard = children.lock().unwrap_or_else(|e| e.into_inner());
            guard.insert(handle, super::child::ChildProcess::new(child, pid));
            OpOutcome::Json(
                serde_json::json!({
                    "handle": handle,
                    "pid": pid,
                    "workerId": worker_id,
                })
                .to_string(),
            )
        }
        Err(e) => OpOutcome::Failed(format!("cluster fork: {e}")),
    }
}

pub async fn cluster_worker_wait(children: ChildRegistry, handle: u64) -> OpOutcome {
    super::child::child_wait(children, handle).await
}

pub fn cluster_worker_kill(children: &ChildRegistry, handle: u64, signal: Option<String>) {
    super::child::child_kill(children, handle, signal);
}

/// The variable `cluster_fork` names a worker's id in (node's
/// `NODE_UNIQUE_ID`).
pub const WORKER_ID_VAR: &str = "OAM_CLUSTER_WORKER";

/// The cluster worker id this process was forked with, if any. The engine
/// reads it once at startup and takes the variable out of the environment
/// (as node deletes `NODE_UNIQUE_ID` during worker bootstrap), so a worker's
/// own children do not report themselves as workers of this primary.
pub fn worker_id_from_env() -> Option<String> {
    std::env::var(WORKER_ID_VAR).ok().filter(|v| !v.is_empty())
}

pub fn cluster_is_worker() -> bool {
    worker_id_from_env().is_some()
}
