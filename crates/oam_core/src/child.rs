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

/// Per-fd disposition for a child's stdin/stdout/stderr, mirroring node's
/// `stdio` option. A numeric fd or a stream in slot 0/1/2 means "hand the child
/// the parent's fd", which is `Inherit`; the JS layer collapses those before
/// they get here.
///
/// `Inherit` is a real OS-level handle hand-off, not a pump: the child writes
/// straight to the parent's fd with no copy through this process, which is both
/// node's semantics and the only way a grandchild's stdio survives an
/// intermediate launcher (npm `bin` shims do exactly this).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum StdioMode {
    /// `/dev/null` (NUL on Windows). Reads see EOF immediately; writes vanish.
    Ignore,
    /// The child gets the parent's own fd for this slot.
    Inherit,
    /// An anonymous pipe this process owns both ends of. Node's default.
    #[default]
    Pipe,
}

impl StdioMode {
    /// Decode the wire form shared with the JS layer and with `child_extra`'s
    /// direction codes: 0=ignore, 1=inherit, anything else=pipe.
    pub fn from_code(code: u8) -> Self {
        match code {
            0 => StdioMode::Ignore,
            1 => StdioMode::Inherit,
            _ => StdioMode::Pipe,
        }
    }

    fn to_stdio(self) -> std::process::Stdio {
        match self {
            StdioMode::Ignore => std::process::Stdio::null(),
            StdioMode::Inherit => std::process::Stdio::inherit(),
            StdioMode::Pipe => std::process::Stdio::piped(),
        }
    }
}

/// stdin/stdout/stderr dispositions, in fd order.
pub type StdioSpec = [StdioMode; 3];

/// Node's default when `stdio` is absent: all three piped.
pub const STDIO_PIPE_ALL: StdioSpec = [StdioMode::Pipe; 3];

/// Spawn a child and return it with its pid. SYNCHRONOUS: node's uv_spawn is
/// synchronous, so `child.pid` must be readable the instant `spawn()` returns
/// (execa, tree-kill and pidusage all read it immediately). There are no
/// awaits in this body -- `tokio::process::Command::spawn` is itself a sync
/// call -- but it MUST run inside a tokio runtime context, because on Unix it
/// registers the child with the signal-driver reactor. Callers on the V8
/// thread must hold a `Handle::enter()` guard; see `CoreRuntime::enter`.
pub fn spawn_child(
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    env: Option<Vec<(String, String)>>,
    shell: bool,
    clear_env: bool,
    stdio: StdioSpec,
) -> Result<(tokio::process::Child, u32), String> {
    let (prog, final_args) = if shell {
        #[cfg(windows)]
        {
            // cmd.exe parses its own quoting; Rust's default arg escaping
            // (MSVCRT rules) mangles a command that embeds quotes, so the
            // canonical `exec('"C:\path with spaces\app.exe" arg')` fails.
            // Node passes `/d /s /c "<command>"` with verbatim arguments --
            // /d skips AutoRun, /s + the wrapping quotes make cmd strip
            // exactly the outer pair. The raw_arg application is below.
            let mut full = command;
            for a in &args {
                full.push(' ');
                full.push_str(a);
            }
            (
                "cmd.exe".to_string(),
                vec![
                    "/d".to_string(),
                    "/s".to_string(),
                    "/c".to_string(),
                    format!("\"{full}\""),
                ],
            )
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
    #[cfg(windows)]
    {
        if shell {
            // Verbatim: hand cmd.exe the string as written (node's
            // windowsVerbatimArguments). tokio's Command has raw_arg
            // natively; the sync path below needs the std CommandExt trait.
            for a in &final_args {
                cmd.raw_arg(a);
            }
        } else {
            cmd.args(&final_args);
        }
    }
    #[cfg(not(windows))]
    cmd.args(&final_args);
    cmd.stdin(stdio[0].to_stdio());
    cmd.stdout(stdio[1].to_stdio());
    cmd.stderr(stdio[2].to_stdio());

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
        // errno rides along: node emits it as the `code` argument of the
        // 'close' event for a child that never started, so the JS layer cannot
        // reproduce node's failure shape without it.
        let errno = super::node_errno(code, &e)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "null".to_string());
        format!(
            "{{\"code\":\"{code}\",\"errno\":{errno},\"message\":\"{}\"}}",
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
    stdio: StdioSpec,
) -> SpawnSyncResult {
    let (prog, final_args) = if shell {
        #[cfg(windows)]
        {
            // Same cmd.exe verbatim-quoting contract as spawn_child above.
            let mut full = command.to_string();
            for a in args {
                full.push(' ');
                full.push_str(a);
            }
            (
                "cmd.exe".to_string(),
                vec![
                    "/d".to_string(),
                    "/s".to_string(),
                    "/c".to_string(),
                    format!("\"{full}\""),
                ],
            )
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
    #[cfg(windows)]
    {
        if shell {
            use std::os::windows::process::CommandExt;
            for a in &final_args {
                cmd.raw_arg(a);
            }
        } else {
            cmd.args(&final_args);
        }
    }
    #[cfg(not(windows))]
    cmd.args(&final_args);
    // `input` is pumped ONLY into a slot that is already pipe-shaped.
    //
    // Node's docs say `input` "will override stdio[0]", but v22 does not
    // implement that sentence: it attaches the buffer to the slot-0 descriptor
    // and leaves the descriptor's TYPE alone, and the sync spawn only feeds
    // slots typed 'pipe'. So an explicit 'ignore'/'inherit' wins and the bytes
    // are never delivered. Measured, same script both runtimes:
    //   stdio omitted / 'pipe'        -> child reads "HELLO"
    //   stdio ['ignore','pipe','pipe'] -> child reads "" on node
    // Parity is with the implementation, not with the sentence -- the
    // conformance suite diffs against real node, and matching the doc here
    // would be a divergence that only looks like correctness.
    //
    // A pipe-shaped slot stays a real PIPE even with no input to write: the
    // `drop(child.stdin.take())` below closes the write end immediately, so the
    // child still sees EOF -- but over a pipe, as node gives it. Substituting
    // the null device here would have been observably different, not merely
    // "EOF by another route": the child's fd 0 becomes a character device, so
    // `fstatSync(0).isCharacterDevice()` flips and 'pipe' stops being
    // distinguishable from 'ignore' to a child that inspects its own stdin.
    cmd.stdin(match stdio[0] {
        StdioMode::Pipe => std::process::Stdio::piped(),
        other => other.to_stdio(),
    });
    cmd.stdout(stdio[1].to_stdio());
    cmd.stderr(stdio[2].to_stdio());
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
        // The child stays REACHABLE here rather than being moved into a worker
        // that owns it. `wait_with_output()` consumes the Child, so the old
        // shape had nothing left to kill when the deadline passed: spawnSync
        // returned ETIMEDOUT while the child ran on holding its port, its lock
        // and -- now that `stdio: 'inherit'` is honored -- the parent's console.
        // The worker was also left parked on a wait that might never return.
        //
        // Pipes are drained on their own threads so a child that fills one
        // cannot deadlock against our wait, which is the reason
        // `wait_with_output` exists in the first place.
        use std::io::Read;
        let mut out_pipe = child.stdout.take();
        let mut err_pipe = child.stderr.take();
        let out_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(p) = out_pipe.as_mut() {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        });
        let err_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(p) = err_pipe.as_mut() {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        let mut timed_out = false;
        let mut wait_err = None;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        // Reap it, so the timeout path leaves no zombie.
                        let _ = child.wait();
                        timed_out = true;
                        break None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => {
                    wait_err = Some(e);
                    break None;
                }
            }
        };
        // The pipes close when the child dies, so these joins cannot outlive it.
        let mut stdout = out_thread.join().unwrap_or_default();
        let mut stderr = err_thread.join().unwrap_or_default();

        if timed_out {
            // node hands back whatever the child managed to emit before the
            // deadline, not an empty pair.
            stdout.truncate(max_buffer);
            stderr.truncate(max_buffer);
            return SpawnSyncResult {
                stdout,
                stderr,
                status: None,
                signal: Some("SIGTERM".to_string()),
                pid,
                error: Some(SpawnSyncError {
                    code: "ETIMEDOUT".to_string(),
                    message: format!("spawnSync timed out after {timeout_ms}ms"),
                }),
            };
        }
        match (status, wait_err) {
            (Some(status), _) => Ok(std::process::Output {
                status,
                stdout,
                stderr,
            }),
            (None, Some(e)) => Err(e),
            (None, None) => Err(std::io::Error::other("spawnSync: child never exited")),
        }
    } else {
        child.wait_with_output()
    };

    match output {
        Ok(output) => {
            let mut stdout = output.stdout;
            let mut stderr = output.stderr;
            // Blowing maxBuffer is an ERROR, not a quiet trim. Truncating and
            // reporting status 0 handed callers a short result that looked
            // complete -- a half JSON document that parses as valid, a log tail
            // that reads as the whole log -- while every node-shaped caller
            // (`if (result.error) throw`, or execSync's throw) saw nothing to
            // branch on. node terminates the child and reports ENOBUFS with a
            // null status; the output it kept is read-granularity dependent, so
            // the retained length is deliberately not part of the contract.
            let overflowed = stdout.len() > max_buffer || stderr.len() > max_buffer;
            stdout.truncate(max_buffer);
            stderr.truncate(max_buffer);
            if overflowed {
                return SpawnSyncResult {
                    stdout,
                    stderr,
                    status: None,
                    signal: Some("SIGTERM".to_string()),
                    pid,
                    error: Some(SpawnSyncError {
                        code: "ENOBUFS".to_string(),
                        message: format!("spawnSync {command} ENOBUFS"),
                    }),
                };
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
