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
    cluster_fork_with(&exe, children, ids, script_path, worker_id, env_pairs)
}

/// `cluster_fork` with the executable named: the seam a test uses to make the
/// spawn itself fail (a fork of this process's own exe never does).
pub(crate) fn cluster_fork_with(
    exe: &str,
    children: ChildRegistry,
    ids: Arc<AtomicU64>,
    script_path: String,
    worker_id: String,
    env_pairs: Vec<(String, String)>,
) -> OpOutcome {
    let mut cmd = worker_command(exe, &["run"], &script_path, &worker_id, &env_pairs);

    match cmd.spawn() {
        Ok(child) => {
            // kill_on_drop covers the runtime dropping; the job covers the
            // primary being KILLED, which never runs a drop. node's cluster
            // forks through uv_spawn, so its workers die that way too.
            #[cfg(windows)]
            super::job_win::adopt_tokio_child(&child);
            let pid = child.id().unwrap_or(0);
            // process.exit() runs no drop, so kill_on_drop never fires on it;
            // on unix nothing else would end the worker (see reap_on_exit).
            #[cfg(unix)]
            reap_on_exit::track(pid);
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

/// The worker's command: `<exe> <leading_args...> <script_path>` with the
/// worker's whole environment, inherited stdout/stderr, kill-on-drop, and on
/// Windows libuv's creation flags and show state. `cluster_fork` passes
/// `["run"]`; split out so a test can aim the SAME construction at a probe
/// binary (a unit test cannot run `<test binary> run <script>`: libtest would
/// read `run` as a test-name filter).
pub(crate) fn worker_command(
    exe: &str,
    leading_args: &[&str],
    script_path: &str,
    worker_id: &str,
    env_pairs: &[(String, String)],
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(leading_args);
    cmd.arg(script_path);
    // node's cluster.fork hands the worker {...process.env, ...env}, and that
    // is its whole environment: the JS layer builds it (childProcessEnv), so
    // nothing of this process's own environment is added here.
    cmd.env_clear();
    for (k, v) in env_pairs {
        cmd.env(k, v);
    }
    cmd.env(WORKER_ID_VAR, worker_id);
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
    cmd
}

pub async fn cluster_worker_wait(children: ChildRegistry, handle: u64) -> OpOutcome {
    #[cfg(unix)]
    let pid = {
        let guard = children.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(&handle).map(|c| c.pid)
    };
    let outcome = super::child::child_wait(children, handle).await;
    // Reaped: its pid is free for reuse now, so the exit hook must not
    // signal it any more.
    #[cfg(unix)]
    if let Some(pid) = pid {
        reap_on_exit::untrack(pid);
    }
    outcome
}

/// A cluster worker must die with its primary, as node's do -- there a worker
/// exits when its IPC channel to the primary disconnects. oam's workers have
/// no such channel: they die by `kill_on_drop` when the primary's runtime
/// drops, and on Windows by the kill-on-close job when the primary dies. But
/// `process.exit()` ends the process WITHOUT dropping the runtime
/// (`exit_process`), and on unix nothing else would end the worker: measured,
/// a worker still alive 10 s after its primary's process.exit(0) (node's is
/// gone). So every forked worker is tracked here until it is reaped, and one
/// exit hook -- run by `exit_process`, i.e. on process.exit and on every
/// other hard exit -- SIGKILLs those still live, which is what kill_on_drop
/// would have done.
///
/// The kill is by pid, guarded against pid reuse: a pid is signalled only
/// while `waitid(P_PID, WEXITED | WNOHANG | WNOWAIT)` confirms it is still
/// THIS process's own unreaped child (live, or a zombie) -- an unreaped child
/// holds its pid, so it cannot have been handed to another process. A worker
/// tokio has already reaped answers ECHILD and is skipped.
#[cfg(unix)]
mod reap_on_exit {
    use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process, waitid};
    use std::collections::BTreeSet;
    use std::sync::{Mutex, Once};

    static LIVE: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());
    static HOOK: Once = Once::new();

    pub(super) fn track(pid: u32) {
        if pid == 0 {
            return;
        }
        LIVE.lock().unwrap_or_else(|e| e.into_inner()).insert(pid);
        HOOK.call_once(|| crate::register_exit_hook(kill_live_workers));
    }

    pub(super) fn untrack(pid: u32) {
        LIVE.lock().unwrap_or_else(|e| e.into_inner()).remove(&pid);
    }

    fn kill_live_workers() {
        let pids = std::mem::take(&mut *LIVE.lock().unwrap_or_else(|e| e.into_inner()));
        for raw in pids {
            if let Some(pid) = i32::try_from(raw).ok().and_then(Pid::from_raw) {
                kill_if_own_unreaped_child(pid);
            }
        }
    }

    /// SIGKILL `pid` if, and only if, it is still this process's own unreaped
    /// child (see the module doc). Returns whether the signal was sent.
    pub(super) fn kill_if_own_unreaped_child(pid: Pid) -> bool {
        let still_ours = waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )
        .is_ok();
        still_ours && kill_process(pid, Signal::KILL).is_ok()
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A variable in THIS process's environment. The outer test re-execs the
    /// test binary with it set on that child's command (no `set_var`, so no
    /// new unsafe and no race with other tests reading the environment); the
    /// re-exec'd test is the one that builds the worker command.
    const INHERITED_VAR: &str = "OAM_CLUSTER_TEST_INHERITED";
    const INHERITED_VALUE: &str = "from-the-primary-os-env";
    const ENV_TEST: &str = "cluster::tests::a_worker_environment_is_env_pairs_and_its_id_only";

    /// A shell that prints its environment, as `(exe, leading args, script)`.
    fn env_printer() -> (String, Vec<&'static str>, &'static str) {
        #[cfg(windows)]
        {
            // The full path: the worker command clears PATH, so a bare `cmd`
            // must not depend on how a lookup treats the cleared environment.
            let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
            (
                format!(r"{system_root}\System32\cmd.exe"),
                vec!["/d", "/c"],
                "set",
            )
        }
        #[cfg(not(windows))]
        {
            ("/bin/sh".to_string(), vec!["-c"], "env")
        }
    }

    /// The worker's environment is exactly what the JS layer handed over
    /// (env_pairs) plus its id: a variable set in the primary's own OS
    /// environment does not leak in on top.
    #[tokio::test]
    async fn a_worker_environment_is_env_pairs_and_its_id_only() {
        if std::env::var_os(INHERITED_VAR).is_none() {
            // Outer run: re-exec just this test with the variable set.
            let exe = std::env::current_exe().expect("the test binary's path");
            let out = std::process::Command::new(exe)
                .args(["--exact", ENV_TEST, "--nocapture", "--test-threads=1"])
                .env(INHERITED_VAR, INHERITED_VALUE)
                .output()
                .expect("re-exec the test binary");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success(),
                "re-exec'd test failed:\n{stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            // It ran (a filter matching nothing also exits 0).
            assert!(stdout.contains("1 passed"), "{stdout}");
            return;
        }
        assert_eq!(
            std::env::var(INHERITED_VAR).as_deref(),
            Ok(INHERITED_VALUE),
            "the precondition: the variable is in this process's environment"
        );
        let (exe, leading, script) = env_printer();
        // `mut` is used only by the Windows push below.
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut pairs = vec![(
            "OAM_CLUSTER_TEST_PAIR".to_string(),
            "pair-value".to_string(),
        )];
        #[cfg(windows)]
        if let Ok(root) = std::env::var("SystemRoot") {
            // cmd.exe needs nothing more to run `set`; SystemRoot is what the
            // JS layer always adds on Windows (WINDOWS_REQUIRED_ENV).
            pairs.push(("SystemRoot".to_string(), root));
        }
        let mut cmd = worker_command(&exe, &leading, script, "7", &pairs);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let out = tokio::time::timeout(std::time::Duration::from_secs(60), cmd.output())
            .await
            .expect("the env printer did not exit within 60 s")
            .expect("run the env printer");
        let env = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{env}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let lines: Vec<&str> = env.lines().map(|l| l.trim_end_matches('\r')).collect();
        assert!(
            lines.contains(&"OAM_CLUSTER_TEST_PAIR=pair-value"),
            "an env_pairs variable reaches the worker:\n{env}"
        );
        assert!(
            lines.contains(&format!("{WORKER_ID_VAR}=7").as_str()),
            "the worker id reaches the worker:\n{env}"
        );
        assert!(
            !lines.iter().any(|l| l
                .to_ascii_uppercase()
                .starts_with(&format!("{INHERITED_VAR}="))),
            "a variable of the primary's own environment leaked into the worker:\n{env}"
        );
    }

    /// A spawn that fails is the op's failure, in the message the JS side
    /// turns into the worker's 'error'.
    #[tokio::test]
    async fn a_fork_whose_spawn_fails_is_a_failed_outcome() {
        let missing = std::env::temp_dir().join(format!(
            "oam-cluster-no-such-exe-{}{}",
            std::process::id(),
            std::env::consts::EXE_SUFFIX
        ));
        assert!(!missing.exists(), "{} must not exist", missing.display());
        let children: ChildRegistry = Default::default();
        let ids = Arc::new(AtomicU64::new(1));
        let outcome = cluster_fork_with(
            &missing.to_string_lossy(),
            children.clone(),
            ids.clone(),
            "script.js".to_string(),
            "1".to_string(),
            Vec::new(),
        );
        match outcome {
            OpOutcome::Failed(msg) => {
                assert!(msg.starts_with("cluster fork: "), "{msg}");
                assert!(
                    msg.len() > "cluster fork: ".len(),
                    "the cause is kept: {msg}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        // Nothing was registered and no handle was spent.
        assert!(children.lock().unwrap().is_empty());
        assert_eq!(ids.load(Ordering::Relaxed), 1);
    }

    /// The exit hook's pid-reuse guard: it signals a pid only while that pid
    /// is still this process's own unreaped child. A live child is killed; a
    /// child already reaped, and a process that is not ours (pid 1), are
    /// refused -- signalling either could hit an unrelated process.
    #[cfg(unix)]
    #[test]
    fn the_exit_reaper_kills_only_its_own_unreaped_children() {
        use rustix::process::Pid;
        use std::os::unix::process::ExitStatusExt;

        let mut live = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let live_pid = Pid::from_child(&live);
        assert!(
            reap_on_exit::kill_if_own_unreaped_child(live_pid),
            "a live child of ours is signalled"
        );
        let status = live.wait().expect("reap the killed child");
        assert_eq!(status.signal(), Some(9), "it died of SIGKILL: {status:?}");

        // Reaped above: the pid is no longer ours and may be anyone's now.
        assert!(
            !reap_on_exit::kill_if_own_unreaped_child(live_pid),
            "a reaped child is never signalled"
        );

        let init = Pid::from_raw(1).expect("pid 1");
        assert!(
            !reap_on_exit::kill_if_own_unreaped_child(init),
            "a process that is not our child is never signalled"
        );
    }
}
