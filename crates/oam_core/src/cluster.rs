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

    // Held from before the spawn until the worker is tracked, so a signal
    // death never runs its hooks with this fork in between (see
    // `reap_on_exit` and `signal::FORK_GATE`). Nothing is held here yet.
    #[cfg(unix)]
    let fork_gate = crate::signal::fork_gate();
    match cmd.spawn() {
        Ok(child) => {
            // kill_on_drop covers the runtime dropping; the job covers the
            // primary being KILLED, which never runs a drop. node's cluster
            // forks through uv_spawn, so its workers die that way too.
            #[cfg(windows)]
            super::job_win::adopt_tokio_child(&child);
            let pid = child.id().unwrap_or(0);
            // Neither process.exit() nor a death by signal runs a drop, so
            // kill_on_drop never fires on them; on unix nothing else would
            // end the worker (see reap_on_exit).
            #[cfg(unix)]
            {
                reap_on_exit::track(pid);
                drop(fork_gate);
            }
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
/// gone). A death by signal drops nothing either. So every forked worker is
/// tracked here until it is reaped, and one process-state hook SIGKILLs
/// those still live, which is what kill_on_drop would have done. That hook
/// runs:
///
/// * on every hard exit (`exit_process`: process.exit, a fatal banner, ...);
/// * on a death by SIGHUP, SIGINT, SIGTERM, SIGQUIT, SIGUSR1 or SIGUSR2 with
///   no JS listener. The first fork arms those signals
///   (`signal::serve_death_defaults`), so that the delivery is served by
///   `signal::die_by` -- hooks first, then the death by the signal itself --
///   instead of by the kernel, which runs nothing. `die_by` runs on an
///   ordinary thread, not in the signal handler, so the lock and the
///   syscalls below are in order there. A signal the primary was started
///   ignoring (`nohup`) is left ignored.
///
/// On a signal death no fork is in flight while the hooks run: a fork holds
/// the fork gate (`signal::FORK_GATE`) from before its `spawn()` until its
/// worker is tracked, and `die_by` holds it from before its hooks until the
/// process is gone. A fork that began first is tracked by then (and so
/// killed by the hook); one that would begin later never spawns.
///
/// NOT covered: a hard exit runs the hooks without the gate, so a fork on
/// another isolate's thread racing a `process.exit` can still leave its
/// worker running. Nor SIGKILL, which nothing can catch, nor a death by a
/// signal outside that list (SIGPIPE, SIGABRT, a fault): a primary that goes
/// that way still leaves its workers running on unix, where node's would
/// exit on the IPC disconnect; closing that needs the worker to notice its
/// parent is gone (a parent-death signal, or a channel to the primary),
/// which oam's workers do not have.
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
    use std::sync::Mutex;

    struct Live {
        pids: BTreeSet<u32>,
        /// Whether `kill_live_workers` is registered and has not run yet.
        /// A hook is one-shot: once it has run, the next fork registers it
        /// again.
        hooked: bool,
    }

    static LIVE: Mutex<Live> = Mutex::new(Live {
        pids: BTreeSet::new(),
        hooked: false,
    });

    /// Track a forked worker until it is reaped. The caller holds the fork
    /// gate (`signal::fork_gate`) from before the spawn through this call.
    pub(super) fn track(pid: u32) {
        track_unless(pid, crate::signal::dying_by_signal, true);
    }

    /// `track`, with the "is this process already dying of a signal"
    /// question handed in (`signal::dying_by_signal`; a test answers it
    /// itself), and with `arm_hooks` saying whether to arm the process-wide
    /// machinery: register `kill_live_workers` (the `hooked` state) and route
    /// the deadly signals through `die_by` (`serve_death_defaults`). Only
    /// `track` passes true. A unit test passes false, so the shared test
    /// process is left with no hook registered, no `hooked` flag up and its
    /// signal dispositions untouched -- nothing but the pid, which the test
    /// takes out again.
    ///
    /// The fork gate keeps a signal death's hooks from running while a fork
    /// is between its `spawn()` and this call (`signal::FORK_GATE`). `dying`
    /// is the second line, for a fork that gets the gate after `die_by` let
    /// go of it (only if the re-raise came back): the hooks have run by then,
    /// so a worker tracked now has no hook left to rely on. So `dying` is
    /// asked AFTER the pid is in the set and its hook registered, still under
    /// the lock, and a worker that arrived too late is killed here and now.
    /// `die_by` raises its flag before it takes the hook list, so a
    /// registration that missed the list sees the flag, and one that made the
    /// list is killed by the hook (which waits on this lock). A worker caught
    /// by both is signalled at most once: whichever side takes the pid out of
    /// the set kills it.
    pub(super) fn track_unless(pid: u32, dying: impl FnOnce() -> bool, arm_hooks: bool) {
        if pid == 0 {
            return;
        }
        let (first, too_late) = {
            let mut live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
            live.pids.insert(pid);
            let first = arm_hooks && !live.hooked;
            if first {
                live.hooked = true;
                // Under the lock, so the hook cannot run (and clear the
                // flag) before it is registered. Lock order: LIVE, then the
                // registry (see `signal::FORK_GATE`); the registry hands its
                // hooks out before running them, so the hook's own LIVE lock
                // never nests inside it.
                crate::register_process_state_hook(kill_live_workers);
            }
            let too_late = dying() && live.pids.remove(&pid);
            (first, too_late)
        };
        if too_late {
            if let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) {
                kill_if_own_unreaped_child(pid);
            }
            return;
        }
        if first {
            // Outside the lock: the first call builds a runtime.
            crate::signal::serve_death_defaults();
        }
    }

    /// Whether `pid` is in the set a dying primary kills.
    #[cfg(test)]
    pub(super) fn is_tracked(pid: u32) -> bool {
        LIVE.lock()
            .unwrap_or_else(|e| e.into_inner())
            .pids
            .contains(&pid)
    }

    pub(super) fn untrack(pid: u32) {
        LIVE.lock()
            .unwrap_or_else(|e| e.into_inner())
            .pids
            .remove(&pid);
    }

    fn kill_live_workers() {
        let pids = {
            let mut live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
            live.hooked = false;
            std::mem::take(&mut live.pids)
        };
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

    /// Whether spawning a program that does not exist fails at spawn() on
    /// this host. False under Rosetta for Linux (the release's linux leg):
    /// there glibc's posix_spawn, which std's Command uses, returns success
    /// for a missing program and the child _exits 127 -- measured in the
    /// release container on 2026-11-13. The e2e suite's probe of the same
    /// name explains the consequence for cluster.fork.
    fn exec_failure_reaches_spawn(missing: &std::path::Path) -> bool {
        match std::process::Command::new(missing)
            .env_clear()
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Err(_) => true,
            Ok(mut child) => {
                let _ = child.wait();
                false
            }
        }
    }

    /// A spawn that fails is the op's failure, in the message the JS side
    /// turns into the worker's 'error'. Skipped where the host reports a
    /// missing program as a child exiting 127 instead (see
    /// exec_failure_reaches_spawn).
    #[tokio::test]
    async fn a_fork_whose_spawn_fails_is_a_failed_outcome() {
        let missing = std::env::temp_dir().join(format!(
            "oam-cluster-no-such-exe-{}{}",
            std::process::id(),
            std::env::consts::EXE_SUFFIX
        ));
        assert!(!missing.exists(), "{} must not exist", missing.display());
        if !exec_failure_reaches_spawn(&missing) {
            eprintln!(
                "skipping: this host reports a missing program as a child exiting 127, \
                 not as a spawn error (x86_64 under Rosetta for Linux)"
            );
            return;
        }
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

    /// Set, to the path of the pid file, on the re-exec'd run that plays the
    /// primary in the test below.
    #[cfg(unix)]
    const SIGNAL_PID_FILE_VAR: &str = "OAM_CLUSTER_TEST_SIGNAL_PID_FILE";
    #[cfg(unix)]
    const SIGNAL_TEST: &str =
        "cluster::tests::a_tracked_worker_dies_when_its_primary_dies_by_a_signal";

    /// Whether `pid` names a live process, a zombie not counted. A worker
    /// its dying primary killed is re-parented unreaped, and stays a zombie
    /// for good where pid 1 does not reap.
    ///
    /// No unsafe: Linux reads the state out of `/proc/<pid>/stat`, the rest
    /// (macOS) ask `ps -o stat=`. A probe that cannot be made panics. It used
    /// to answer "not alive", which is the answer the test below is waiting
    /// for: with no `ps` on the box the test passed without having looked.
    /// The test also asks about its own pid first, for the probe that runs
    /// but does not see processes (no /proc mounted, a `ps` that prints
    /// something else).
    #[cfg(unix)]
    fn pid_alive(pid: u32) -> bool {
        #[cfg(target_os = "linux")]
        {
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Ok(stat) => proc_stat_is_running(&stat)
                    .unwrap_or_else(|| panic!("/proc/{pid}/stat has no state field: {stat:?}")),
                // No such directory: no such process. ESRCH is a process
                // that went between the open and the read.
                Err(e)
                    if e.kind() == std::io::ErrorKind::NotFound
                        || e.raw_os_error() == Some(libc::ESRCH) =>
                {
                    false
                }
                Err(e) => panic!("cannot read /proc/{pid}/stat: {e}"),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let out = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .output()
                .unwrap_or_else(|e| panic!("cannot run ps to look for pid {pid}: {e}"));
            let stat = String::from_utf8_lossy(&out.stdout);
            let stat = stat.trim();
            // ps exits non-zero, printing nothing, for a pid that is gone.
            out.status.success() && !stat.is_empty() && !stat.starts_with('Z')
        }
    }

    /// Whether a `/proc/<pid>/stat` line is a process still running: its
    /// state is neither Z (zombie) nor X (dead). The line is `pid (comm)
    /// state ...`, and `comm` may itself hold spaces and parentheses, so the
    /// state is the first field after the LAST `)`. `None` when there is no
    /// such field.
    #[cfg(any(target_os = "linux", test))]
    fn proc_stat_is_running(stat: &str) -> Option<bool> {
        let (_, after_comm) = stat.rsplit_once(')')?;
        let state = after_comm.split_whitespace().next()?;
        Some(!matches!(state, "Z" | "X"))
    }

    #[test]
    fn a_proc_stat_line_is_read_past_a_comm_with_parentheses() {
        assert_eq!(
            proc_stat_is_running("4242 (sleep) S 1 4242 4242 0 -1"),
            Some(true)
        );
        assert_eq!(
            proc_stat_is_running("4242 (sleep) Z 1 4242 4242 0 -1"),
            Some(false)
        );
        assert_eq!(proc_stat_is_running("4242 (sleep) X 1"), Some(false));
        // A comm that looks like the rest of the line.
        assert_eq!(proc_stat_is_running("7 (a) Z (b) R 1 7 7 0 -1"), Some(true));
        assert_eq!(proc_stat_is_running("7 (tmux: server) R 1"), Some(true));
        assert_eq!(proc_stat_is_running("7 (cut short)"), None);
        assert_eq!(proc_stat_is_running(""), None);
    }

    /// A worker forked while the primary is already dying of a signal is
    /// killed as it is tracked, and is not left in the set: the hooks have
    /// run (or are running) by then, so nothing later would end it. The
    /// dying flag is handed in; the real one is raised only by a real death.
    ///
    /// Both calls pass `arm_hooks: false`: this runs in the shared test
    /// process, and arming would register the real `kill_live_workers` hook,
    /// raise the module's `hooked` flag and route the deadly signals through
    /// `die_by` for every later test. Each pid is out of the set again by the
    /// end, so the test leaves no process-wide state behind.
    #[cfg(unix)]
    #[test]
    fn a_worker_tracked_while_the_primary_is_dying_is_killed_at_once() {
        use std::os::unix::process::ExitStatusExt;

        let mut late = std::process::Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("spawn sleep");
        reap_on_exit::track_unless(late.id(), || true, false);
        assert!(
            !reap_on_exit::is_tracked(late.id()),
            "a worker killed on arrival is not left for a hook"
        );
        // wait() blocks until the child ends: with no kill, for 300 s.
        let status = late.wait().expect("reap the late worker");
        assert_eq!(status.signal(), Some(9), "it died of SIGKILL: {status:?}");

        // Not dying: tracked and left running, as every fork is.
        let mut live = std::process::Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("spawn sleep");
        reap_on_exit::track_unless(live.id(), || false, false);
        assert!(reap_on_exit::is_tracked(live.id()));
        assert!(
            live.try_wait().expect("poll the worker").is_none(),
            "a worker tracked in the ordinary way is not signalled"
        );
        reap_on_exit::untrack(live.id());
        live.kill().expect("end the worker");
        live.wait().expect("reap the worker");
    }

    /// A fork does not spawn while the fork gate is held -- as `die_by`
    /// holds it across a signal death's hooks -- and goes ahead once it is
    /// let go. The spawn is one that fails (a missing program), so nothing
    /// is tracked and no process-wide state is armed; skipped where the host
    /// reports a missing program as a child exiting 127 instead.
    #[cfg(unix)]
    #[test]
    fn a_fork_waits_at_the_fork_gate() {
        use std::time::Duration;

        let missing = std::env::temp_dir().join(format!(
            "oam-cluster-gated-no-such-exe-{}",
            std::process::id()
        ));
        assert!(!missing.exists(), "{} must not exist", missing.display());
        if !exec_failure_reaches_spawn(&missing) {
            eprintln!(
                "skipping: this host reports a missing program as a child exiting 127, \
                 not as a spawn error (x86_64 under Rosetta for Linux)"
            );
            return;
        }
        let gate = crate::signal::fork_gate();
        let (done, finished) = std::sync::mpsc::channel();
        let exe = missing.to_string_lossy().into_owned();
        let forker = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime");
            let _entered = runtime.enter();
            let outcome = cluster_fork_with(
                &exe,
                Default::default(),
                Arc::new(AtomicU64::new(1)),
                "script.js".to_string(),
                "1".to_string(),
                Vec::new(),
            );
            let _ = done.send(());
            // Only a String crosses back to the test thread.
            match outcome {
                OpOutcome::Failed(msg) => Ok(msg),
                other => Err(format!("{other:?}")),
            }
        });
        assert!(
            finished.recv_timeout(Duration::from_millis(300)).is_err(),
            "a fork went through while the fork gate was held"
        );
        drop(gate);
        finished
            .recv_timeout(Duration::from_secs(60))
            .expect("the fork went ahead once the gate was let go");
        match forker.join().expect("the forking thread") {
            Ok(msg) => assert!(msg.starts_with("cluster fork: "), "{msg}"),
            Err(other) => panic!("expected Failed, got {other}"),
        }
    }

    #[cfg(unix)]
    fn kill_9(pid: u32) {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }

    /// A primary that dies of a signal nobody listens for takes its tracked
    /// workers with it, and still dies of that signal. Left at the kernel's
    /// default, SIGTERM ended the primary with no hook run and the worker
    /// ran on (the exit hook covered process.exit only).
    ///
    /// The primary is this test binary, re-exec'd: it tracks a `sleep` as
    /// its worker -- the same call `cluster_fork_with` makes -- publishes the
    /// pid, and waits to be signalled. A signal death cannot be staged
    /// inside the test process itself.
    #[cfg(unix)]
    #[test]
    fn a_tracked_worker_dies_when_its_primary_dies_by_a_signal() {
        use std::os::unix::process::ExitStatusExt;
        use std::time::{Duration, Instant};

        if let Some(pid_file) = std::env::var_os(SIGNAL_PID_FILE_VAR) {
            // The primary.
            let pid_file = std::path::PathBuf::from(pid_file);
            let mut worker = std::process::Command::new("sleep")
                .arg("300")
                .spawn()
                .expect("spawn sleep");
            reap_on_exit::track(worker.id());
            // Published only once tracked (and so armed): written whole,
            // then renamed into place.
            let tmp = pid_file.with_extension("tmp");
            std::fs::write(&tmp, worker.id().to_string()).expect("write the worker's pid");
            std::fs::rename(&tmp, &pid_file).expect("publish the worker's pid");
            std::thread::sleep(Duration::from_secs(60));
            // Never signalled: the outer run is gone. Leave nothing behind.
            let _ = worker.kill();
            let _ = worker.wait();
            panic!("the primary was not signalled within 60 s");
        }

        // The probe the verdict rests on, proved on a process known to be
        // running before it is trusted to say that one is not.
        assert!(
            pid_alive(std::process::id()),
            "pid_alive cannot see this very process"
        );

        let pid_file = std::env::temp_dir().join(format!(
            "oam-cluster-signal-death-{}.pid",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&pid_file);
        let exe = std::env::current_exe().expect("the test binary's path");
        let mut primary = std::process::Command::new(exe)
            .args(["--exact", SIGNAL_TEST, "--nocapture", "--test-threads=1"])
            .env(SIGNAL_PID_FILE_VAR, &pid_file)
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("re-exec the test binary");

        let started = Instant::now();
        let worker_pid: u32 = loop {
            if let Ok(text) = std::fs::read_to_string(&pid_file) {
                break text.trim().parse().expect("a numeric pid");
            }
            if let Some(status) = primary.try_wait().expect("poll the primary") {
                panic!("the primary ended before publishing its worker: {status}");
            }
            if started.elapsed() > Duration::from_secs(60) {
                let _ = primary.kill();
                let _ = primary.wait();
                panic!("the primary published no worker pid within 60 s");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let _ = std::fs::remove_file(&pid_file);

        // kill(1) rather than libc::kill: no unsafe in the test.
        let sent = std::process::Command::new("kill")
            .args(["-TERM", &primary.id().to_string()])
            .status()
            .expect("run kill(1)");
        let signalled = Instant::now();
        let mut overran = false;
        while primary.try_wait().expect("poll the primary").is_none() {
            if signalled.elapsed() > Duration::from_secs(30) {
                overran = true;
                let _ = primary.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let status = primary.wait().expect("reap the primary");
        if overran {
            kill_9(worker_pid);
            panic!("the primary was still running 30 s after SIGTERM (kill: {sent})");
        }

        // Bounded: a worker still alive 10 s after its primary is gone is
        // one nothing will ever end.
        let gone = Instant::now();
        while pid_alive(worker_pid) && gone.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let survived = pid_alive(worker_pid);
        if survived {
            // Never leak it past the test, whatever the verdict.
            kill_9(worker_pid);
        }
        assert!(sent.success(), "kill -TERM failed: {sent}");
        assert_eq!(
            status.signal(),
            Some(libc::SIGTERM),
            "the primary still dies of the signal itself: {status:?}"
        );
        assert!(
            !survived,
            "worker {worker_pid} outlived its signal-killed primary by 10 s"
        );
    }
}
