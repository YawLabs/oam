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
    cmd.env("OAM_CLUSTER_WORKER", &worker_id);
    for (k, v) in &env_pairs {
        cmd.env(k, v);
    }
    cmd.stdout(std::process::Stdio::inherit());
    cmd.stderr(std::process::Stdio::inherit());
    // Reap workers if the primary's runtime drops -- Node leaves orphans, but
    // here a leaked worker would outlive its only manager (and wedge tests).
    cmd.kill_on_drop(true);

    #[cfg(windows)]
    {
        cmd.creation_flags(0x0800_0000);
    }

    match cmd.spawn() {
        Ok(child) => {
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

pub fn cluster_is_worker() -> bool {
    std::env::var("OAM_CLUSTER_WORKER")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}
