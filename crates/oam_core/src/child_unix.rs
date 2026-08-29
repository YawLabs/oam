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
//! `Child::wait` on the blocking pool, kill via `kill(2)` (a real signal,
//! unlike Windows' hard `TerminateProcess`), pipe I/O via blocking
//! `read`/`write` on the blocking pool. tokio's SIGCHLD reaper only waits on its
//! own registered (`tokio::process`) children, so reaping this child ourselves
//! does not race it. Anonymous pipes are unidirectional, so extra fds carry one
//! direction each (sufficient for CDP); full-duplex extra fds are a future
//! enhancement no current consumer needs.

use std::collections::HashMap;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
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

/// Parent-side end of one piped fd.
///
/// `OwnedFd`, not `RawFd`: the descriptor's lifetime IS the invariant this
/// module used to hand-maintain. Every in-flight op `take()`s the end out of
/// the registry, so ownership is unique by construction, and every path that
/// used to end in an explicit `libc::close` -- EOF, a read error, a join
/// failure, an entry that vanished mid-op, teardown in `raw_wait` -- is now
/// just the value going out of scope. A missed close leaks and a doubled close
/// can hand a recycled fd to an unrelated op; neither is expressible now.
struct FdEnd {
    fd: Option<OwnedFd>,
    /// true => parent reads this end (child writes); false => parent writes.
    readable: bool,
}

pub struct RawChild {
    child: Option<Child>,
    pub pid: u32,
    fds: HashMap<u32, FdEnd>,
    /// True once the pid has been reaped (`raw_wait`'s blocking wait returned).
    /// A reaped pid can be recycled by the kernel immediately, so `raw_kill`
    /// must never signal it. This -- not the presence of `child`, which
    /// `raw_wait` take()s at its first poll -- is the liveness signal.
    reaped: bool,
}

pub type RawChildRegistry = Arc<Mutex<HashMap<u64, RawChild>>>;

/// Create an anonymous pipe with both ends `CLOEXEC`. Returns `(read, write)`.
///
/// `std::io::pipe` rather than a hand-rolled `pipe2`/`pipe`+`fcntl` pair: std
/// makes exactly the same platform split internally (`pipe2(O_CLOEXEC)` where
/// it exists, `pipe` plus a `FD_CLOEXEC` `fcntl` on macOS, which lacks it) and
/// documents both ends as close-on-exec, so the CLOEXEC guarantee this module
/// depends on is unchanged -- it is just no longer restated here.
fn make_pipe() -> Result<(OwnedFd, OwnedFd), String> {
    let (r, w) = std::io::pipe().map_err(|e| format!("pipe: {e}"))?;
    Ok((OwnedFd::from(r), OwnedFd::from(w)))
}

/// Open `/dev/null` read-write, `CLOEXEC`.
///
/// std opens every file `O_CLOEXEC` on unix, so the flag does not have to be
/// asked for -- only relied on, exactly as before.
fn open_dev_null() -> Result<OwnedFd, String> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .map(OwnedFd::from)
        .map_err(|e| format!("open(/dev/null): {e}"))
}

/// Relocate `fd` to the lowest free fd `>= base`, `CLOEXEC`.
///
/// Still ALWAYS consumes `fd` on both success and failure -- but that is now
/// the signature saying so rather than a comment: taking it by value drops
/// (closes) it here on every path, so a caller physically cannot re-close it.
fn relocate(fd: OwnedFd, base: RawFd) -> Result<OwnedFd, String> {
    rustix::io::fcntl_dupfd_cloexec(&fd, base)
        .map_err(|e| format!("fcntl(F_DUPFD_CLOEXEC): {}", super::io_from_errno(e)))
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
    //
    // The sources are OWNED here and stay owned until the spawn succeeds, which
    // is what retires the old hand-written `bail!` macro: every early return
    // below drops `child_ends` and `parent_fds`, and dropping them closes
    // exactly the descriptors this function had allocated so far -- no more, no
    // less. The macro had to enumerate that set by hand at each call site (and
    // take an `$extra` argument for the ends `relocate` had not consumed yet),
    // which is the kind of bookkeeping that goes wrong when an arm is added.
    let mut child_ends: Vec<(OwnedFd, RawFd)> = Vec::new();
    let mut parent_fds: HashMap<u32, FdEnd> = HashMap::new();

    for (i, spec) in stdio.iter().enumerate() {
        let fd = i as u32;
        let target = i as RawFd;
        match spec {
            // Child inherits the parent's fd; nothing to set up (Command is
            // configured to inherit 0/1/2, and pre_exec leaves this one alone).
            StdioFd::Inherit => {}
            StdioFd::Ignore => {
                // relocate consumes `nul`.
                let high = relocate(open_dev_null()?, base)?;
                child_ends.push((high, target));
            }
            StdioFd::ChildRead => {
                let (r, w) = make_pipe()?;
                // child reads -> child gets READ end; parent keeps WRITE.
                // If `relocate` fails, `w` is still a live local and closes as
                // it goes out of scope on the `?`.
                let high = relocate(r, base)?;
                child_ends.push((high, target));
                parent_fds.insert(
                    fd,
                    FdEnd {
                        fd: Some(w),
                        readable: false,
                    },
                );
            }
            StdioFd::ChildWrite => {
                let (r, w) = make_pipe()?;
                // child writes -> child gets WRITE end; parent keeps READ.
                let high = relocate(w, base)?;
                child_ends.push((high, target));
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
                //
                // This is the one call in the module that stays raw. rustix's
                // `dup` wants an `AsFd`, and building one means
                // `BorrowedFd::borrow_raw`, whose contract is that the
                // descriptor is ALREADY OPEN -- which is precisely what cannot
                // be asserted about an integer that arrived from JS. `dup(2)`
                // itself has no such precondition: it is defined for any
                // integer and answers EBADF for one that is not a descriptor.
                // Wrapping it would trade a real check for an unprovable claim.
                //
                // SAFETY: two obligations, both discharged inside this block.
                // `libc::dup` takes no pointers and duplicates the integer fd
                // `*src`, returning either -1 or a FRESH descriptor. The -1 is
                // rejected here, and a descriptor straight out of `dup(2)` is
                // held by nothing else -- so `OwnedFd::from_raw_fd`'s
                // requirement of sole ownership of an open fd is met.
                let copy = unsafe {
                    match libc::dup(*src) {
                        -1 => None,
                        raw => Some(OwnedFd::from_raw_fd(raw)),
                    }
                };
                let Some(copy) = copy else {
                    return Err(format!(
                        "dup(fd{fd}) failed: {}",
                        std::io::Error::last_os_error()
                    ));
                };
                // Above every target for the same reason the pipe ends are:
                // a source sitting on another slot's target index would be
                // clobbered mid-plan.
                let high = relocate(copy, base)?;
                child_ends.push((high, target));
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
    //
    // Raw integers, deliberately: the hook runs post-fork where nothing may
    // allocate or drop, so it must not capture anything with a destructor. The
    // `OwnedFd`s the integers name stay alive in `child_ends` until after the
    // spawn below, which is what keeps them valid for the hook.
    let plan: Vec<(RawFd, RawFd)> = child_ends
        .iter()
        .map(|(src, target)| (src.as_raw_fd(), *target))
        .collect();
    // SAFETY: `pre_exec` runs the closure in the forked child before exec, where
    // only async-signal-safe operations are permitted. The closure does exactly
    // one thing per plan entry -- `dup2(src, dst)` -- allocating nothing (the
    // `plan` Vec is pre-built), touching no lock or heap; on error it returns a
    // raw-errno `io::Error` that std reads and _exit()s without unwinding. dup2
    // is on the POSIX async-signal-safe list, so the hook is sound.
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
            let code = super::node_error_code(&e);
            // errno rides along: node emits it as the `code` argument of the
            // 'close' event for a child that never started, so the JS layer
            // cannot reproduce node's failure shape without it (same contract
            // as the non-raw spawn path in child.rs).
            let errno = super::node_errno(code, &e)
                .map(|n| n.to_string())
                .unwrap_or_else(|| "null".to_string());
            let msg = format!(
                "{{\"code\":\"{code}\",\"errno\":{errno},\"message\":\"spawn failed ({}) for {}\"}}",
                e,
                command.replace('"', "\\\"")
            );
            return Err(msg);
        }
    };
    let pid = child.id();

    // Parent no longer needs the child-end (relocated) sources; the child holds
    // its dup2'd copies at the target fds. Dropping the owned ends closes them,
    // once each, and only after the spawn has read their raw numbers.
    drop(child_ends);

    Ok(RawChild {
        child: Some(child),
        pid,
        fds: parent_fds,
        reaped: false,
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
    let Some(end_fd) = raw else {
        return OpOutcome::Done;
    };
    // The owned end travels INTO the blocking task and comes back out on
    // success. That is what retires the four explicit closes this match used to
    // carry: every path that does not hand it back -- read error, EOF, a
    // panicking blocking task -- drops it, which is the close.
    let result = tokio::task::spawn_blocking(move || {
        // Poll with NO timeout before the read: it blocks until the fd is
        // readable OR the child's write end closes. Child death guarantees
        // readability (POLLIN|POLLHUP, and the read then returns 0 = EOF), so
        // this thread cannot park forever on a dead child -- and a
        // live-but-quiet child simply waits, instead of a poll timeout
        // masquerading as EOF and silently truncating the stream.
        {
            let mut pfd = [rustix::event::PollFd::new(
                &end_fd,
                rustix::event::PollFlags::IN,
            )];
            // `None` is the infinite timeout the raw `-1` meant.
            rustix::event::poll(&mut pfd, None)
                .map_err(|e| format!("poll: {}", super::io_from_errno(e)))?;
        }
        let mut buf = vec![0u8; 65536];
        // The fd is ready (POLLIN, or HUP when the write end closed), so this
        // read cannot block; on HUP it returns 0 (EOF).
        let n = rustix::io::read(&end_fd, &mut buf[..])
            .map_err(|e| format!("read: {}", super::io_from_errno(e)))?;
        if n == 0 {
            Ok((end_fd, None))
        } else {
            buf.truncate(n);
            Ok((end_fd, Some(buf)))
        }
    })
    .await;

    match result {
        Ok(Ok((end_fd, Some(bytes)))) => {
            let mut guard = reg.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(end) = guard.get_mut(&id).and_then(|c| c.fds.get_mut(&fd)) {
                end.fd = Some(end_fd);
            }
            // else: the registry entry vanished mid-op, so `end_fd` has no other
            // owner and closes as it goes out of scope here.
            OpOutcome::Bytes(bytes)
        }
        // EOF: the end is not re-inserted, so it closes here.
        Ok(Ok((_end_fd, None))) => OpOutcome::Done,
        Ok(Err(e)) => OpOutcome::Failed(e),
        // The blocking task took ownership of the end, so a join failure closed
        // it while unwinding.
        Err(e) => OpOutcome::Failed(format!("read join: {e}")),
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
    let Some(end_fd) = raw else {
        return OpOutcome::Failed(format!("fd {fd} not available"));
    };
    // Same ownership shape as `raw_read`: the end goes into the blocking task
    // and only comes back on success, so failure paths close by dropping.
    let result = tokio::task::spawn_blocking(move || {
        let mut off = 0usize;
        while off < data.len() {
            match rustix::io::write(&end_fd, &data[off..]) {
                // EINTR before any progress -- retry, as before.
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(format!("write: {}", super::io_from_errno(e))),
                Ok(0) => return Err("write wrote 0".to_string()),
                Ok(n) => off += n,
            }
        }
        Ok(end_fd)
    })
    .await;

    match result {
        Ok(Ok(end_fd)) => {
            let mut guard = reg.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(end) = guard.get_mut(&id).and_then(|c| c.fds.get_mut(&fd)) {
                end.fd = Some(end_fd);
            }
            // else: the registry entry vanished mid-op; `end_fd` closes here.
            OpOutcome::Done
        }
        Ok(Err(e)) => OpOutcome::Failed(e),
        Err(e) => OpOutcome::Failed(format!("write join: {e}")),
    }
}

/// Close one parent-side fd end (e.g. end stdin / a CDP pipe).
pub fn raw_close_fd(reg: &RawChildRegistry, id: u64, fd: u32) {
    let mut guard = reg.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(end) = guard.get_mut(&id).and_then(|c| c.fds.get_mut(&fd)) {
        // Taking the end out of the registry drops it here, which IS the close.
        drop(end.fd.take());
    }
}

/// Deliver the kill signal. `raw_wait`'s blocking `Child::wait` then returns
/// and reports the exit (derived from the real `ExitStatus`).
pub fn raw_kill(reg: &RawChildRegistry, id: u64, signal: Option<String>) {
    let mut guard = reg.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(child) = guard.get_mut(&id) {
        // Liveness comes from the `reaped` flag, NOT from the presence of the
        // `Child` handle: raw_wait take()s the handle at its first poll, and
        // the JS side starts the wait op the moment the child spawns -- so by
        // the time any kill() arrives the handle is long gone. (Gating on the
        // handle made every live extra-fd kill a silent no-op; conformance
        // case 68's sigterm leg timed out on exactly that.)
        //
        // Until raw_wait's blocking reap returns, the pid is ours to signal in
        // every state: alive (the signal is delivered and the wait observes
        // whatever comes of it) or a zombie (kill(2) accepts the pid and
        // discards the signal -- it cannot alter the recorded exit status).
        // After the reap the kernel may recycle the pid immediately, so a kill
        // then could hit an unrelated process; raw_wait flips `reaped` under
        // this same lock the moment its blocking wait returns, closing that
        // window (to the same sliver Node/libuv accept: waitpid has returned
        // but the flag-flip has not yet taken the lock).
        if child.reaped {
            return;
        }
        // Unknown signal names resolve to SIGTERM -- same as the None case.
        let signum = signal
            .as_deref()
            .map(signal_number)
            .unwrap_or(Some(libc::SIGTERM))
            .unwrap_or(libc::SIGTERM);
        // raw_wait derives the report from the real ExitStatus, so we do not
        // record the signal here -- a trapped/survived signal must not be
        // misreported as a death.
        //
        // The result is discarded exactly as the raw `libc::kill`'s was: a
        // failed signal is not an error the JS side can act on, and the wait
        // observes whatever actually happens to the child. `Pid::from_raw` and
        // `Signal::from_named_raw` cannot fail for the values reaching here --
        // a real child pid, and one of `signal_number`'s libc constants -- so
        // `deliver_signal` returning without acting is unreachable in practice.
        deliver_signal(child.pid, signum);
    }
}

/// `kill(2)` on a live child pid, via rustix's typed `Pid` + `Signal`.
///
/// Shared by this module and `child.rs` (`pub(crate)`), and the whole reason
/// the two sites no longer carry an `unsafe` block each. rustix will not take
/// an untyped pair, and neither constructor is fallible-by-surprise: both
/// return `Option`, so a nonsensical pid or an unnamed signal number is a
/// no-op here rather than something handed to the kernel.
///
/// The caller is responsible for the liveness question -- signalling a reaped
/// pid can hit an unrelated process, and that check has to happen under the
/// registry lock, which is not this function's business.
pub(crate) fn deliver_signal(pid: u32, signum: i32) {
    let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return;
    };
    let Some(sig) = rustix::process::Signal::from_named_raw(signum) else {
        return;
    };
    let _ = rustix::process::kill_process(pid, sig);
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

    let status = {
        let reg = reg.clone();
        tokio::task::spawn_blocking(move || {
            let st = child.wait();
            // From waitpid's return onward the kernel may hand this pid to an
            // unrelated process, so flip `reaped` (under the registry lock,
            // atomically w.r.t. any in-flight raw_kill) before the status is
            // visible anywhere else. Set even on a wait error: an unknown pid
            // state must fail safe toward "do not signal it again".
            let mut guard = reg.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(c) = guard.get_mut(&id) {
                c.reaped = true;
            }
            st
        })
        .await
    };

    // Drop the entry, close every fd we still own. The recorded kill_signal is
    // deliberately NOT consulted below: on Unix the real ExitStatus is the
    // source of truth (mirrors `exit_report` in child.rs), so a child that
    // caught our signal and exited 0 reports {code:0, signal:null} like Node,
    // not a phantom signal death.
    let fds = {
        let mut guard = reg.lock().unwrap_or_else(|e| e.into_inner());
        match guard.remove(&id) {
            Some(c) => c.fds,
            None => HashMap::new(),
        }
    };
    // Removed from the registry above, so this fn is the sole owner of every
    // remaining pipe end -- dropping the map closes each exactly once.
    drop(fds);

    match status {
        Ok(Ok(st)) => {
            // Derive the report entirely from the child's real ExitStatus. A
            // signal-terminated child (ours or not) -> code:null + the real
            // terminating signal; otherwise the normal exit code. WIFSIGNALED
            // is the source of truth, so a signal the child trapped and
            // survived is not misreported as a death.
            let json = if let Some(signum) = st.signal() {
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

#[cfg(test)]
mod tests {
    use super::signal_number;

    /// Signal-name mapping covers the known names and rejects unknown/empty
    /// ones (the `None` arm is the divergence fix: unknown no longer silently
    /// becomes SIGTERM).
    #[test]
    fn signal_number_maps_known_and_unknown() {
        assert_eq!(signal_number("SIGTERM"), Some(libc::SIGTERM));
        assert_eq!(signal_number("SIGKILL"), Some(libc::SIGKILL));
        assert_eq!(signal_number("SIGFOO"), None);
        assert_eq!(signal_number(""), None);
    }
}
