use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

use super::OpOutcome;

pub struct ChildProcess {
    /// The live child. Taken out of the registry entry by `child_wait` for the
    /// duration of the await, but the entry itself stays so a concurrent
    /// `child_kill` can still reach the kill-notifier below.
    ///
    /// stdin/stdout/stderr are stored as separate fields so that `child_wait`
    /// taking `child` (which has all three set to None) does not race with
    /// concurrent read/write ops. Without this separation a Windows Tokio
    /// scheduler that runs `spawnWait` before `spawnWrite` / `spawnReadStdout`
    /// causes those ops to see `child` as None and silently fail.
    pub child: Option<tokio::process::Child>,
    pub stdin: Option<tokio::process::ChildStdin>,
    pub stdout: Option<tokio::process::ChildStdout>,
    pub stderr: Option<tokio::process::ChildStderr>,
    pub pid: u32,
    /// Wakes the parked `child_wait` future, which owns the `Child` and is the
    /// only place that can signal it. Lets kill survive `child` being taken.
    pub kill: Arc<Notify>,
    /// Signal name the caller requested for the kill (Node reports it on exit).
    pub kill_signal: Option<String>,
}

impl ChildProcess {
    pub fn new(mut child: tokio::process::Child, pid: u32) -> Self {
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        ChildProcess {
            child: Some(child),
            stdin,
            stdout,
            stderr,
            pid,
            kill: Arc::new(Notify::new()),
            kill_signal: None,
        }
    }
}

pub type ChildRegistry = Arc<Mutex<HashMap<u64, ChildProcess>>>;

pub async fn spawn_child(
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    env: Option<Vec<(String, String)>>,
    shell: bool,
    clear_env: bool,
) -> Result<(tokio::process::Child, u32), String> {
    let (prog, final_args) = if shell {
        #[cfg(windows)]
        {
            let mut shell_args = vec!["/C".to_string(), command];
            shell_args.extend(args);
            ("cmd.exe".to_string(), shell_args)
        }
        #[cfg(not(windows))]
        {
            let mut full = command;
            for a in &args {
                full.push(' ');
                full.push_str(a);
            }
            ("/bin/sh".to_string(), vec!["-c".to_string(), full])
        }
    } else {
        (command, args)
    };

    let mut cmd = tokio::process::Command::new(&prog);
    cmd.args(&final_args);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    if clear_env {
        cmd.env_clear();
    }
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    if let Some(env) = env {
        for (k, v) in env {
            cmd.env(k, v);
        }
    }

    #[cfg(windows)]
    {
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }

    let child = cmd.spawn().map_err(|e| {
        let code = super::node_error_code(&e);
        format!(
            "{{\"code\":\"{code}\",\"message\":\"{}\"}}",
            e.to_string().replace('"', "\\\"")
        )
    })?;
    let pid = child.id().unwrap_or(0);
    Ok((child, pid))
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_sync(
    command: &str,
    args: &[String],
    cwd: Option<&str>,
    env: Option<&[(String, String)]>,
    input: Option<&[u8]>,
    shell: bool,
    clear_env: bool,
    timeout_ms: u64,
    max_buffer: usize,
) -> SpawnSyncResult {
    let (prog, final_args) = if shell {
        #[cfg(windows)]
        {
            let mut shell_args = vec!["/C".to_string(), command.to_string()];
            shell_args.extend(args.iter().cloned());
            ("cmd.exe".to_string(), shell_args)
        }
        #[cfg(not(windows))]
        {
            let mut full = command.to_string();
            for a in args {
                full.push(' ');
                full.push_str(a);
            }
            ("/bin/sh".to_string(), vec!["-c".to_string(), full])
        }
    } else {
        (command.to_string(), args.to_vec())
    };

    let mut cmd = std::process::Command::new(&prog);
    cmd.args(&final_args);
    if input.is_some() {
        cmd.stdin(std::process::Stdio::piped());
    } else {
        cmd.stdin(std::process::Stdio::null());
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }

    if clear_env {
        cmd.env_clear();
    }
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    if let Some(env) = env {
        for (k, v) in env {
            cmd.env(k, v);
        }
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let code = super::node_error_code(&e);
            return SpawnSyncResult {
                stdout: Vec::new(),
                stderr: Vec::new(),
                status: None,
                signal: None,
                pid: 0,
                error: Some(SpawnSyncError {
                    code: code.to_string(),
                    message: e.to_string(),
                }),
            };
        }
    };

    let pid = child.id();

    if let Some(input) = input
        && let Some(mut stdin) = child.stdin.take()
    {
        use std::io::Write;
        let _ = stdin.write_all(input);
    }
    drop(child.stdin.take());

    let output = if timeout_ms > 0 {
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let result = child.wait_with_output();
            let _ = tx.send(result);
        });
        match rx.recv_timeout(std::time::Duration::from_millis(timeout_ms)) {
            Ok(result) => {
                let _ = thread.join();
                result
            }
            Err(_) => {
                return SpawnSyncResult {
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    status: None,
                    signal: Some("SIGTERM".to_string()),
                    pid,
                    error: Some(SpawnSyncError {
                        code: "ETIMEDOUT".to_string(),
                        message: format!("spawnSync timed out after {timeout_ms}ms"),
                    }),
                };
            }
        }
    } else {
        child.wait_with_output()
    };

    match output {
        Ok(output) => {
            let mut stdout = output.stdout;
            let mut stderr = output.stderr;
            if stdout.len() > max_buffer {
                stdout.truncate(max_buffer);
            }
            if stderr.len() > max_buffer {
                stderr.truncate(max_buffer);
            }
            let status = output.status.code();
            SpawnSyncResult {
                stdout,
                stderr,
                status,
                signal: None,
                pid,
                error: None,
            }
        }
        Err(e) => SpawnSyncResult {
            stdout: Vec::new(),
            stderr: Vec::new(),
            status: None,
            signal: None,
            pid,
            error: Some(SpawnSyncError {
                code: super::node_error_code(&e).to_string(),
                message: e.to_string(),
            }),
        },
    }
}

pub struct SpawnSyncResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: Option<i32>,
    pub signal: Option<String>,
    pub pid: u32,
    pub error: Option<SpawnSyncError>,
}

pub struct SpawnSyncError {
    pub code: String,
    pub message: String,
}

pub async fn child_read_stdout(children: ChildRegistry, handle: u64) -> OpOutcome {
    let stdout = {
        let mut guard = children.lock().unwrap_or_else(|e| e.into_inner());
        guard.get_mut(&handle).and_then(|cp| cp.stdout.take())
    };
    match stdout {
        Some(mut stdout) => {
            use tokio::io::AsyncReadExt;
            let mut buf = vec![0u8; 65536];
            match stdout.read(&mut buf).await {
                Ok(0) => OpOutcome::Done,
                Ok(n) => {
                    buf.truncate(n);
                    let mut guard = children.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(cp) = guard.get_mut(&handle) {
                        cp.stdout = Some(stdout);
                    }
                    OpOutcome::Bytes(buf)
                }
                Err(e) => OpOutcome::Failed(format!("read stdout: {e}")),
            }
        }
        None => OpOutcome::Done,
    }
}

pub async fn child_read_stderr(children: ChildRegistry, handle: u64) -> OpOutcome {
    let stderr = {
        let mut guard = children.lock().unwrap_or_else(|e| e.into_inner());
        guard.get_mut(&handle).and_then(|cp| cp.stderr.take())
    };
    match stderr {
        Some(mut stderr) => {
            use tokio::io::AsyncReadExt;
            let mut buf = vec![0u8; 65536];
            match stderr.read(&mut buf).await {
                Ok(0) => OpOutcome::Done,
                Ok(n) => {
                    buf.truncate(n);
                    let mut guard = children.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(cp) = guard.get_mut(&handle) {
                        cp.stderr = Some(stderr);
                    }
                    OpOutcome::Bytes(buf)
                }
                Err(e) => OpOutcome::Failed(format!("read stderr: {e}")),
            }
        }
        None => OpOutcome::Done,
    }
}

pub fn child_close_stdin(children: &ChildRegistry, handle: u64) {
    let mut guard = children.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cp) = guard.get_mut(&handle) {
        drop(cp.stdin.take());
    }
}

pub async fn child_write_stdin(children: ChildRegistry, handle: u64, data: Vec<u8>) -> OpOutcome {
    let stdin = {
        let mut guard = children.lock().unwrap_or_else(|e| e.into_inner());
        guard.get_mut(&handle).and_then(|cp| cp.stdin.take())
    };
    match stdin {
        Some(mut stdin) => {
            use tokio::io::AsyncWriteExt;
            match stdin.write_all(&data).await {
                Ok(()) => {
                    let mut guard = children.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(cp) = guard.get_mut(&handle) {
                        cp.stdin = Some(stdin);
                    }
                    OpOutcome::Done
                }
                Err(e) => OpOutcome::Failed(format!("write stdin: {e}")),
            }
        }
        None => OpOutcome::Failed("stdin not available".to_string()),
    }
}

/// Signal a child to terminate. Wakes the parked `child_wait` future (the only
/// owner of the `Child`), which performs the actual kill. Survives `child_wait`
/// having already taken the `Child` out of the registry entry.
pub fn child_kill(children: &ChildRegistry, handle: u64, signal: Option<String>) {
    let mut guard = children.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cp) = guard.get_mut(&handle) {
        cp.kill_signal = signal;
        // notify_one stores a permit if the wait future has not parked yet, so
        // a kill issued before child_wait starts still lands.
        cp.kill.notify_one();
    }
}

pub async fn child_wait(children: ChildRegistry, handle: u64) -> OpOutcome {
    // Take the Child for the await but leave the registry entry in place so a
    // concurrent child_kill can still reach the kill-notifier. `pid` rides along
    // for the Unix kill path, which delivers the real POSIX signal by pid.
    let taken = {
        let mut guard = children.lock().unwrap_or_else(|e| e.into_inner());
        match guard.get_mut(&handle) {
            Some(cp) => cp.child.take().map(|c| (c, cp.kill.clone(), cp.pid)),
            None => None,
        }
    };
    let Some((mut child, kill, pid)) = taken else {
        return OpOutcome::Failed("unknown child handle".to_string());
    };

    let killed;
    let status = tokio::select! {
        s = child.wait() => { killed = false; s }
        _ = kill.notified() => {
            killed = true;
            // Read the signal child_kill recorded (stored under the lock BEFORE
            // notify_one, so it is visible here) and deliver it for real.
            let requested = {
                let guard = children.lock().unwrap_or_else(|e| e.into_inner());
                guard.get(&handle).and_then(|cp| cp.kill_signal.clone())
            };
            deliver_kill(&mut child, pid, requested.as_deref());
            child.wait().await
        }
    };

    // Drop the registry entry now that the wait is done, capturing the requested
    // kill signal (used by the Windows exit report).
    let requested_signal = {
        let mut guard = children.lock().unwrap_or_else(|e| e.into_inner());
        let sig = guard.get(&handle).and_then(|cp| cp.kill_signal.clone());
        guard.remove(&handle);
        sig
    };

    match status {
        Ok(status) => {
            let (code, signal) = exit_report(&status, killed, requested_signal);
            let json = serde_json::json!({ "code": code, "signal": signal });
            OpOutcome::Json(json.to_string())
        }
        Err(e) => OpOutcome::Failed(format!("wait: {e}")),
    }
}

/// Deliver a kill to `child`. On Unix this sends the caller's REQUESTED POSIX
/// signal (default SIGTERM) via `libc::kill` on the pid, so a child can trap
/// SIGTERM / SIGINT and shut down gracefully -- matching Node. `child.wait()`
/// still reaps the exit afterward: tokio's SIGCHLD reaper observes the death
/// regardless of which signal (or sender) caused it. On Windows there are no
/// POSIX signals, so `start_kill()` (TerminateProcess) is the only mechanism;
/// the requested signal name is surfaced by `exit_report` instead.
#[cfg(unix)]
fn deliver_kill(_child: &mut tokio::process::Child, pid: u32, signal: Option<&str>) {
    let signum = signal
        .map(crate::child_unix::signal_number)
        .unwrap_or(libc::SIGTERM);
    // SAFETY: kill(2) with a valid pid + signal number; touches no memory.
    unsafe { libc::kill(pid as libc::pid_t, signum) };
}

#[cfg(windows)]
fn deliver_kill(child: &mut tokio::process::Child, _pid: u32, _signal: Option<&str>) {
    // TerminateProcess -- the only kill Windows offers. exit_report maps the
    // requested signal name onto the exit event.
    let _ = child.start_kill();
}

/// Build the Node-shaped `(code, signal)` exit report from the child's ACTUAL
/// exit status.
///
/// Unix: a signal-terminated child reports `code:null` + the real terminating
/// signal, so a child that caught our SIGTERM and `exit(0)`ed reports
/// `{code:0, signal:null}`, while one killed by the default action reports
/// `{code:null, signal:SIGTERM}`. `killed` / `requested_signal` are unused --
/// `WIFSIGNALED` is the source of truth.
///
/// Windows: the status carries no POSIX signal, so when WE initiated the kill
/// report `code:null` + the requested name (Node's Windows convention);
/// otherwise the exit code.
#[cfg(unix)]
fn exit_report(
    status: &std::process::ExitStatus,
    _killed: bool,
    _requested_signal: Option<String>,
) -> (Option<i32>, Option<String>) {
    use std::os::unix::process::ExitStatusExt;
    if let Some(signum) = status.signal() {
        (None, Some(crate::child_unix::signal_name(signum)))
    } else {
        (status.code(), None)
    }
}

#[cfg(windows)]
fn exit_report(
    status: &std::process::ExitStatus,
    killed: bool,
    requested_signal: Option<String>,
) -> (Option<i32>, Option<String>) {
    if killed {
        (
            None,
            Some(requested_signal.unwrap_or_else(|| "SIGTERM".to_string())),
        )
    } else {
        (status.code(), None)
    }
}
