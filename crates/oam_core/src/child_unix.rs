//! Unix extra-fd stdio: `std::process::Command` + a `pre_exec` hook that
//! `dup2()`s pre-arranged child fd ends onto their numbered targets, so a child
//! can receive fds beyond 0/1/2 (Chromium `--remote-debugging-pipe` reads CDP
//! off fd 3 and writes fd 4). The Windows analogue is `child_win.rs`; this
//! mirrors its public surface (`StdioFd` / `RawChild` / `RawChildRegistry` /
//! `spawn_extra` / `raw_read` / `raw_write` / `raw_close_fd` / `raw_kill` /
//! `raw_wait`) so the engine ops are `cfg(any(windows, unix))` over one alias
//! (`oam_core::child_extra`).
//!
//! Per-fd direction (same convention as Windows):
//!   `ChildRead`  (e.g. fd 3): child gets the pipe READ end;  parent keeps WRITE.
//!   `ChildWrite` (e.g. fd 4): child gets the pipe WRITE end;  parent keeps READ.
//!
//! Collision-free by construction: every child-end fd is relocated (in the
//! PARENT, pre-fork) to a high fd via `F_DUPFD_CLOEXEC`, strictly above any
//! target index, so the async-signal-safe `pre_exec` hook only ever does
//! `dup2(high_src, target)` -- targets never alias a source, so ordering is
//! irrelevant. The high sources are `CLOEXEC` (auto-closed on the child's
//! `exec`), and the parent-kept ends are `CLOEXEC` too, so the child's copies
//! vanish on `exec`. No `open()`/`close()` runs in the signal context; all fd
//! allocation happens in the parent (mirrors what `std`'s own spawn does).
//!
//! A raw child is a `std::process::Child` (not `tokio::process`), so -- like the
//! Windows side -- it carries its own registry and op set: wait via a blocking
//! `Child::wait` on the blocking pool, kill via `libc::kill` (a real signal,
//! unlike Windows' hard `TerminateProcess`), pipe I/O via blocking
//! `read`/`write` on the blocking pool. tokio's SIGCHLD reaper only waits on its
//! own registered (`tokio::process`) children, so reaping this child ourselves
//! does not race it. Anonymous pipes are unidirectional, so extra fds carry one
//! direction each (sufficient for CDP); full-duplex extra fds are a future
//! enhancement no current consumer needs.

use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use super::OpOutcome;

/// What to do with one child fd index.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StdioFd {
    /// No stream: child fd points at /dev/null.
    Ignore,
    /// Child inherits the parent's fd for this index (0/1/2 in practice).
    Inherit,
    /// Pipe the child READS (parent writes). e.g. stdin, CDP-in (fd 3).
    ChildRead,
    /// Pipe the child WRITES (parent reads). e.g. stdout/stderr, CDP-out (fd 4).
    ChildWrite,
    /// A descriptor the CALLER named -- `stdio: ['ignore','pipe','pipe', logFd]`.
    ///
    /// Without this a numbered slot fell through to `Inherit`, which leaves the
    /// child holding whatever the parent had at that index rather than the
    /// descriptor the caller asked for.
    Descriptor(RawFd),
}

/// Parent-side end of one piped fd. `RawFd` is `Send`/`Copy`, so no wrapper is
/// needed (unlike the Windows `HANDLE`).
struct FdEnd {
    fd: Option<RawFd>,
    /// true => parent reads this end (child writes); false => parent writes.
    readable: bool,
}

pub struct RawChild {
    child: Option<Child>,
    pub pid: u32,
    fds: HashMap<u32, FdEnd>,
    kill_signal: Option<String>,
}

pub type RawChildRegistry = Arc<Mutex<HashMap<u64, RawChild>>>;

/// Create an anonymous pipe with both ends `CLOEXEC`. Returns `(read, write)`.
/// Uses `pipe2(O_CLOEXEC)` on Linux (atomic); elsewhere (macOS lacks `pipe2`)
/// falls back to `pipe` + `fcntl`, exactly as `std`'s own spawn does.
fn make_pipe() -> Result<(RawFd, RawFd), String> {
    let mut fds = [0 as RawFd; 2];
    let rc = unsafe {
        #[cfg(target_os = "linux")]
        {
            libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let r = libc::pipe(fds.as_mut_ptr());
            if r == 0 {
                libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC);
                libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
            }
            r
        }
    };
    if rc != 0 {
        return Err(format!("pipe: {}", std::io::Error::last_os_error()));
    }
    Ok((fds[0], fds[1]))
}

/// Open `/dev/null` read-write, `CLOEXEC`.
fn open_dev_null() -> Result<RawFd, String> {
    let fd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(format!(
            "open(/dev/null): {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(fd)
}

/// Relocate `fd` to the lowest free fd `>= base`, `CLOEXEC`. ALWAYS consumes
/// `fd` (closes it on both success and failure), so callers never re-close it.
fn relocate(fd: RawFd, base: RawFd) -> Result<RawFd, String> {
    let high = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, base) };
    let err = if high < 0 {
        Some(std::io::Error::last_os_error())
    } else {
        None
    };
    unsafe { libc::close(fd) };
    match err {
        Some(e) => Err(format!("fcntl(F_DUPFD_CLOEXEC): {e}")),
        None => Ok(high),
    }
}

/// Map a Node signal name to this platform's signal number (libc constants are
/// platform-correct, unlike hardcoded numbers which diverge on macOS). Shared
/// with the main `child.rs` kill path (`pub(crate)`).
pub(crate) fn signal_number(name: &str) -> Option<i32> {
    match name {
        "SIGHUP" => Some(libc::SIGHUP),
        "SIGINT" => Some(libc::SIGINT),
        "SIGQUIT" => Some(libc::SIGQUIT),
        "SIGABRT" => Some(libc::SIGABRT),
        "SIGKILL" => Some(libc::SIGKILL),
        "SIGUSR1" => Some(libc::SIGUSR1),
        "SIGUSR2" => Some(libc::SIGUSR2),
        "SIGTERM" => Some(libc::SIGTERM),
        "SIGCONT" => Some(libc::SIGCONT),
        "SIGSTOP" => Some(libc::SIGSTOP),
        _ => None,
    }
}

/// Reverse of `signal_number` for reporting a child that died of a signal we
/// did not initiate. Falls back to `SIG<n>` for anything uncommon. Shared with
/// the main `child.rs` kill path (`pub(crate)`).
pub(crate) fn signal_name(num: i32) -> String {
    let name = match num {
        x if x == libc::SIGHUP => "SIGHUP",
        x if x == libc::SIGINT => "SIGINT",
        x if x == libc::SIGQUIT => "SIGQUIT",
        x if x == libc::SIGABRT => "SIGABRT",
        x if x == libc::SIGKILL => "SIGKILL",
        x if x == libc::SIGUSR1 => "SIGUSR1",
        x if x == libc::SIGUSR2 => "SIGUSR2",
        x if x == libc::SIGTERM => "SIGTERM",
        x if x == libc::SIGSEGV => "SIGSEGV",
        x if x == libc::SIGPIPE => "SIGPIPE",
        _ => return format!("SIG{num}"),
    };
    name.to_string()
}

/// Spawn a child with an arbitrary stdio fd layout. Pipes are created per the
/// `stdio` spec; the child inherits the appropriate ends as numbered fds, and
/// the parent keeps the other ends for I/O.
pub fn spawn_extra(
    command: &str,
    args: &[String],
    cwd: Option<&str>,
    env: Option<&[(String, String)]>,
    clear_env: bool,
    stdio: &[StdioFd],
) -> Result<RawChild, String> {
    // Relocate child ends above every target index so the pre_exec dup2 cannot
    // alias a source onto another target.
    let base = stdio.len() as RawFd + 3;

    // (high_src, target) pairs the child applies; parent keeps the other ends.
    let mut dup_plan: Vec<(RawFd, RawFd)> = Vec::new();
    let mut parent_fds: HashMap<u32, FdEnd> = HashMap::new();

    // Close everything allocated so far, plus any extra transient fds, on error.
    macro_rules! bail {
        ($($extra:expr),* ; $e:expr) => {{
            $( unsafe { libc::close($extra) }; )*
            for &(src, _) in &dup_plan { unsafe { libc::close(src) }; }
            for end in parent_fds.values() {
                if let Some(fd) = end.fd { unsafe { libc::close(fd) }; }
            }
            return Err($e);
        }};
    }

    for (i, spec) in stdio.iter().enumerate() {
        let fd = i as u32;
        let target = i as RawFd;
        match spec {
            // Child inherits the parent's fd; nothing to set up (Command is
            // configured to inherit 0/1/2, and pre_exec leaves this one alone).
            StdioFd::Inherit => {}
            StdioFd::Ignore => {
                let nul = match open_dev_null() {
                    Ok(f) => f,
                    Err(e) => bail!(; e),
                };
                // relocate consumes `nul`.
                let high = match relocate(nul, base) {
                    Ok(h) => h,
                    Err(e) => bail!(; e),
                };
                dup_plan.push((high, target));
            }
            StdioFd::ChildRead => {
                let (r, w) = match make_pipe() {
                    Ok(p) => p,
                    Err(e) => bail!(; e),
                };
                // child reads -> child gets READ end; parent keeps WRITE.
                let high = match relocate(r, base) {
                    Ok(h) => h,
                    Err(e) => bail!(w; e), // relocate already closed `r`
                };
                dup_plan.push((high, target));
                parent_fds.insert(
                    fd,
                    FdEnd {
                        fd: Some(w),
                        readable: false,
                    },
                );
            }
            StdioFd::ChildWrite => {
                let (r, w) = match make_pipe() {
                    Ok(p) => p,
                    Err(e) => bail!(; e),
                };
                // child writes -> child gets WRITE end; parent keeps READ.
                let high = match relocate(w, base) {
                    Ok(h) => h,
                    Err(e) => bail!(r; e), // relocate already closed `w`
                };
                dup_plan.push((high, target));
                parent_fds.insert(
                    fd,
                    FdEnd {
                        fd: Some(r),
                        readable: true,
                    },
                );
            }
            StdioFd::Descriptor(src) => {
                // Duplicated before relocating, so the caller keeps its own
                // descriptor: `relocate` consumes what it is handed.
                let copy = unsafe { libc::dup(*src) };
                if copy == -1 {
                    bail!(; format!("dup(fd{fd}) failed: {}", std::io::Error::last_os_error()));
                }
                // Above every target for the same reason the pipe ends are:
                // a source sitting on another slot's target index would be
                // clobbered mid-plan.
                let high = match relocate(copy, base) {
                    Ok(h) => h,
                    Err(e) => bail!(; e),
                };
                dup_plan.push((high, target));
            }
        }
    }

    let mut cmd = Command::new(command);
    cmd.args(args);
    // 0/1/2 inherit by default; pre_exec overrides them when the spec says so.
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    if clear_env {
        cmd.env_clear();
    }
    if let Some(pairs) = env {
        for (k, v) in pairs {
            cmd.env(k, v);
        }
    }

    // Apply the fd plan in the child just before exec. Only dup2 (async-signal-
    // safe); the plan Vec is pre-allocated, so the hook allocates nothing.
    let plan = dup_plan.clone();
    unsafe {
        cmd.pre_exec(move || {
            for &(src, dst) in &plan {
                if libc::dup2(src, dst) < 0 {
                    // MUST stay `last_os_error()` (a non-allocating raw-errno
                    // Error): std reads the errno and _exit()s without unwinding.
                    // A custom/boxed Error here would heap-allocate post-fork --
                    // illegal in this async-signal-safe context.
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let code = if e.kind() == std::io::ErrorKind::NotFound {
                "ENOENT"
            } else {
                "UNKNOWN"
            };
            let msg = format!(
                "{{\"code\":\"{code}\",\"message\":\"spawn failed ({}) for {}\"}}",
                e,
                command.replace('"', "\\\"")
            );
            bail!(; msg);
        }
    };
    let pid = child.id();

    // Parent no longer needs the child-end (relocated) sources; the child holds
    // its dup2'd copies at the target fds.
    for &(src, _) in &dup_plan {
        unsafe { libc::close(src) };
    }

    Ok(RawChild {
        child: Some(child),
        pid,
        fds: parent_fds,
        kill_signal: None,
    })
}

/// Read up to 64 KiB from a parent-readable fd end. EOF (0 bytes) -> Done.
pub async fn raw_read(reg: RawChildRegistry, id: u64, fd: u32) -> OpOutcome {
    let raw = {
        let mut guard = reg.lock().unwrap_or_else(|e| e.into_inner());
        match guard.get_mut(&id).and_then(|c| c.fds.get_mut(&fd)) {
            Some(end) if end.readable => end.fd.take(),
            Some(_) => return OpOutcome::Failed(format!("fd {fd} is not readable")),
            None => return OpOutcome::Done,
        }
    };
    let Some(rawfd) = raw else {
        return OpOutcome::Done;
    };
    let result = tokio::task::spawn_blocking(move || {
        // Poll with NO timeout before the read: poll(-1) blocks until the fd
        // is readable OR the child's write end closes. Child death guarantees
        // readability (POLLIN|POLLHUP, and the read then returns 0 = EOF), so
        // this thread cannot park forever on a dead child -- and a
        // live-but-quiet child simply waits, instead of a poll timeout
        // masquerading as EOF and silently truncating the stream.
        let mut pfd = libc::pollfd {
            fd: rawfd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: pfd is a valid stack pollfd for the live rawfd; touches no
        // other memory.
        let pr = unsafe { libc::poll(&mut pfd, 1, -1) };
        if pr < 0 {
            return Err(format!("poll: {}", std::io::Error::last_os_error()));
        }
        let mut buf = vec![0u8; 65536];
        // SAFETY: the fd is ready (POLLIN, or HUP when the write end closed),
        // so this read cannot block; on HUP it returns 0 (EOF). buf is a live
        // 64 KiB buffer for the live rawfd.
        let n = unsafe { libc::read(rawfd, buf.as_mut_ptr() as *mut libc::c_void, 65536) };
        if n < 0 {
            Err(format!("read: {}", std::io::Error::last_os_error()))
        } else if n == 0 {
            Ok(None)
        } else {
            buf.truncate(n as usize);
            Ok(Some(buf))
        }
    })
    .await;

    match result {
        Ok(Ok(Some(bytes))) => {
            let mut guard = reg.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(end) = guard.get_mut(&id).and_then(|c| c.fds.get_mut(&fd)) {
                end.fd = Some(rawfd);
            } else {
                unsafe { libc::close(rawfd) };
            }
            OpOutcome::Bytes(bytes)
        }
        Ok(Ok(None)) => {
            unsafe { libc::close(rawfd) };
            OpOutcome::Done
        }
        Ok(Err(e)) => {
            unsafe { libc::close(rawfd) };
            OpOutcome::Failed(e)
        }
        Err(e) => {
            unsafe { libc::close(rawfd) };
            OpOutcome::Failed(format!("read join: {e}"))
        }
    }
}

/// Write all bytes to a parent-writable fd end.
pub async fn raw_write(reg: RawChildRegistry, id: u64, fd: u32, data: Vec<u8>) -> OpOutcome {
    let raw = {
        let mut guard = reg.lock().unwrap_or_else(|e| e.into_inner());
        match guard.get_mut(&id).and_then(|c| c.fds.get_mut(&fd)) {
            Some(end) if !end.readable => end.fd.take(),
            Some(_) => return OpOutcome::Failed(format!("fd {fd} is not writable")),
            None => return OpOutcome::Failed(format!("unknown fd {fd}")),
        }
    };
    let Some(rawfd) = raw else {
        return OpOutcome::Failed(format!("fd {fd} not available"));
    };
    let result = tokio::task::spawn_blocking(move || {
        let mut off = 0usize;
        while off < data.len() {
            let n = unsafe {
                libc::write(
                    rawfd,
                    data[off..].as_ptr() as *const libc::c_void,
                    data.len() - off,
                )
            };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(format!("write: {e}"));
            }
            if n == 0 {
                return Err("write wrote 0".to_string());
            }
            off += n as usize;
        }
        Ok(())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            let mut guard = reg.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(end) = guard.get_mut(&id).and_then(|c| c.fds.get_mut(&fd)) {
                end.fd = Some(rawfd);
            } else {
                unsafe { libc::close(rawfd) };
            }
            OpOutcome::Done
        }
        Ok(Err(e)) => {
            unsafe { libc::close(rawfd) };
            OpOutcome::Failed(e)
        }
        Err(e) => {
            unsafe { libc::close(rawfd) };
            OpOutcome::Failed(format!("write join: {e}"))
        }
    }
}

/// Close one parent-side fd end (e.g. end stdin / a CDP pipe).
pub fn raw_close_fd(reg: &RawChildRegistry, id: u64, fd: u32) {
    let mut guard = reg.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(end) = guard.get_mut(&id).and_then(|c| c.fds.get_mut(&fd))
        && let Some(rawfd) = end.fd.take()
    {
        unsafe { libc::close(rawfd) };
    }
}

/// Record the kill signal and deliver it. `raw_wait`'s blocking `Child::wait`
/// then returns and reports the exit.
pub fn raw_kill(reg: &RawChildRegistry, id: u64, signal: Option<String>) {
    let mut guard = reg.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(child) = guard.get_mut(&id) {
        // Only signal while the Child handle is still present. raw_wait take()s
        // it under this same lock *before* the blocking reap, so `child.is_none()`
        // means the pid may already be reaped -- and the kernel can recycle a
        // reaped pid immediately, so a kill then could hit an unrelated process.
        if child.child.is_some() {
            // Unknown signal names resolve to SIGTERM -- same as the None case.
            let signum = signal
                .as_deref()
                .map(signal_number)
                .unwrap_or(Some(libc::SIGTERM))
                .unwrap_or(libc::SIGTERM);
            let name = signal.unwrap_or_else(|| signal_name(signum));
            // Record the kill only when the signal actually fired. A failed
            // kill (EPERM) must not masquerade as a signal death in raw_wait.
            // SAFETY: kill(2) with a valid pid + signal number; touches no memory.
            if unsafe { libc::kill(child.pid as libc::pid_t, signum) } == 0 {
                child.kill_signal = Some(name);
            }
        }
    }
}

/// Wait for exit; returns Node-shaped {code, signal}. Closes all remaining
/// parent-side fds and drops the registry entry.
pub async fn raw_wait(reg: RawChildRegistry, id: u64) -> OpOutcome {
    let taken = {
        let mut guard = reg.lock().unwrap_or_else(|e| e.into_inner());
        match guard.get_mut(&id) {
            Some(c) => c.child.take(),
            None => return OpOutcome::Failed("unknown child handle".to_string()),
        }
    };
    let Some(mut child) = taken else {
        return OpOutcome::Failed("child already awaited".to_string());
    };

    let status = tokio::task::spawn_blocking(move || child.wait()).await;

    // Drop the entry, capture the kill signal, close every fd we still own.
    let (kill_signal, fds) = {
        let mut guard = reg.lock().unwrap_or_else(|e| e.into_inner());
        match guard.remove(&id) {
            Some(c) => (c.kill_signal, c.fds),
            None => (None, HashMap::new()),
        }
    };
    for end in fds.values() {
        if let Some(rawfd) = end.fd {
            unsafe { libc::close(rawfd) };
        }
    }

    match status {
        Ok(Ok(st)) => {
            // A kill we initiated -> code:null + the recorded signal (Node's
            // convention). A signal we did not initiate -> derive the name.
            // Otherwise the normal exit code.
            let json = if let Some(sig) = kill_signal {
                serde_json::json!({ "code": null, "signal": sig })
            } else if let Some(signum) = st.signal() {
                serde_json::json!({ "code": null, "signal": signal_name(signum) })
            } else {
                serde_json::json!({ "code": st.code().unwrap_or(0), "signal": null })
            };
            OpOutcome::Json(json.to_string())
        }
        Ok(Err(e)) => OpOutcome::Failed(format!("wait: {e}")),
        Err(e) => OpOutcome::Failed(format!("wait join: {e}")),
    }
}
