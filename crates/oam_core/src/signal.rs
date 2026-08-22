//! Inbound OS signal delivery.
//!
//! `CoreRuntime::start_signal(name)` installs a native handler that, on each
//! delivery of the named signal, sends an `OpCompletion { id: SIGNAL_OP_ID,
//! outcome: OpOutcome::Signal(name) }` down the op channel. The engine's
//! event loop wakes on that completion and calls `process.emit(name)`.
//!
//! Two backends behind one surface (`start_signal` + `SignalHandle`):
//!
//! * Unix — `tokio::signal::unix::signal`. Creating the `Signal` (under the
//!   runtime enter guard) replaces `SIG_DFL` immediately, so the default
//!   terminate is suppressed the moment the first JS listener attaches. The
//!   `SignalHandle` holds the recv task's `AbortHandle`. Removing the last
//!   listener does NOT drop the handle: tokio leaves its process-global
//!   handler installed for the process lifetime and offers no way to restore
//!   `SIG_DFL`, so a dropped stream would leave the signal caught and
//!   silently discarded — the process becomes unkillable by it. Instead the
//!   handle goes DORMANT (`set_watched(false)`) and the still-running task
//!   reproduces the OS default itself: restore `SIG_DFL` and re-raise, so the
//!   process dies exactly as it would have and the parent sees the right
//!   terminating signal. Re-adding a listener re-arms the same task.
//!   (This was previously documented as a benign divergence. It is not:
//!   `removeAllListeners('SIGINT')` followed by a SIGINT hung forever.)
//!
//! * Windows — `SetConsoleCtrlHandler`. The handler is a zero-capture
//!   `extern "system"` fn, so state lives in a process-global. It maps
//!   console control events to Node signal names, and returns TRUE (suppress
//!   the OS default) only when that name currently has a watcher. CAVEAT:
//!   CTRL_CLOSE / LOGOFF / SHUTDOWN give only ~5s before a forced kill even
//!   when the handler returns TRUE, so SIGHUP-graceful shutdown is best-effort
//!   on Windows. SIGTERM is accepted as a name but the OS never produces it.

use crate::{OpCompletion, OpOutcome, SIGNAL_OP_ID};
use std::sync::mpsc::Sender;

// ============================================================ Unix backend

#[cfg(unix)]
pub struct SignalHandle {
    abort: tokio::task::AbortHandle,
    /// False once the last JS listener is removed. The recv task stays
    /// ALIVE while dormant -- see `stop_signal` for why it must.
    watched: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(unix)]
impl SignalHandle {
    pub fn set_watched(&self, on: bool) {
        self.watched.store(on, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(unix)]
impl Drop for SignalHandle {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

/// Does the OS default action for this signal TERMINATE the process?
/// Only those need the restore-and-re-raise dance when no listener is
/// attached; for an ignore-by-default signal (SIGWINCH) simply dropping the
/// delivery already reproduces the default exactly.
///
/// SIGTSTP is in this set: its default action stops the process, and after
/// SIG_DFL is restored the re-raise reproduces exactly that. SIGCONT is NOT:
/// its default is "continue", so re-raising after restore would be a no-op
/// anyway, and treating it as terminating could kill a process whose stop
/// default had been dropped -- a dormant SIGCONT must stay a no-op.
#[cfg(unix)]
fn default_terminates(signum: i32) -> bool {
    matches!(
        signum,
        libc::SIGHUP
            | libc::SIGINT
            | libc::SIGTERM
            | libc::SIGQUIT
            | libc::SIGUSR1
            | libc::SIGUSR2
            | libc::SIGTSTP
    )
}

/// Map a Node signal name to a tokio `SignalKind`. Returns `None` for names
/// that are not deliverable Unix signals (e.g. "SIGBREAK", which is
/// Windows-only) — the JS listener is allowed but simply never fires, matching
/// Node's behavior for such names on Unix.
#[cfg(unix)]
fn signal_kind(name: &str) -> Option<tokio::signal::unix::SignalKind> {
    use tokio::signal::unix::SignalKind;
    Some(match name {
        "SIGHUP" => SignalKind::hangup(),
        "SIGINT" => SignalKind::interrupt(),
        "SIGTERM" => SignalKind::terminate(),
        "SIGQUIT" => SignalKind::quit(),
        "SIGUSR1" => SignalKind::user_defined1(),
        "SIGUSR2" => SignalKind::user_defined2(),
        "SIGWINCH" => SignalKind::window_change(),
        // tokio has no named constructors for these two; build from the libc
        // number (platform-correct on both Linux and macOS).
        "SIGCONT" => SignalKind::from_raw(libc::SIGCONT),
        "SIGTSTP" => SignalKind::from_raw(libc::SIGTSTP),
        _ => return None,
    })
}

#[cfg(unix)]
pub fn start_signal(
    runtime: &tokio::runtime::Runtime,
    tx: &Sender<OpCompletion>,
    name: &str,
) -> Option<SignalHandle> {
    let kind = signal_kind(name)?;
    // Enter the runtime so `signal()` registers with the runtime's signal
    // driver synchronously (SIG_DFL is replaced before this returns), closing
    // the startup race where a signal could hit the default handler between
    // attach and the recv task's first poll.
    let mut sig = {
        let _guard = runtime.enter();
        tokio::signal::unix::signal(kind).ok()?
    };
    let tx = tx.clone();
    let nm = name.to_string();
    let signum = kind.as_raw_value();
    let watched = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let task_watched = watched.clone();
    let join = runtime.spawn(async move {
        while sig.recv().await.is_some() {
            if !task_watched.load(std::sync::atomic::Ordering::SeqCst) {
                // No JS listener left. tokio's handler CANNOT be uninstalled
                // (its registration is a one-time global), so without this
                // the signal would be caught and silently discarded and the
                // process would be unkillable by it -- e.g. SIGINT after
                // removeAllListeners('SIGINT'). Restore the default and
                // re-raise so the process dies exactly as it would have,
                // and the parent observes the right terminating signal.
                if default_terminates(signum) {
                    // SAFETY: `libc::signal`/`libc::raise` take only the integer
                    // signal number and the well-known SIG_DFL constant -- no
                    // pointers into our memory. This runs only when the signal's
                    // default action terminates and no JS listener remains, so
                    // restoring SIG_DFL and re-raising reproduces exactly that
                    // default action.
                    unsafe {
                        libc::signal(signum, libc::SIG_DFL);
                        libc::raise(signum);
                    }
                }
                // Ignore-by-default signals need nothing: dropping the
                // delivery already IS the default action.
                continue;
            }
            let sent = tx.send(OpCompletion {
                id: SIGNAL_OP_ID,
                outcome: OpOutcome::Signal(nm.clone()),
            });
            // The receiver (op channel) is gone: the run is over, stop.
            if sent.is_err() {
                break;
            }
        }
    });
    Some(SignalHandle {
        abort: join.abort_handle(),
        watched,
    })
}

/// Stop delivering `name` to JS.
///
/// Unix keeps the handle DORMANT rather than dropping it: the recv task owns
/// the only path back to the OS default action (see `start_signal`), so
/// aborting it would leave the signal permanently swallowed.
#[cfg(unix)]
pub fn stop_signal(map: &mut std::collections::HashMap<String, SignalHandle>, name: &str) {
    if let Some(handle) = map.get(name) {
        handle.set_watched(false);
    }
}

// ========================================================== Windows backend

#[cfg(windows)]
use std::collections::HashSet;
#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use std::sync::{Mutex, OnceLock};

#[cfg(windows)]
struct SigState {
    /// Sender for the current run's op channel. Refreshed on every
    /// `start_signal` so the latest run wins after a per-run reset.
    tx: Option<Sender<OpCompletion>>,
    /// Node signal names that currently have a watcher. The ctrl handler only
    /// suppresses the OS default (returns TRUE) for names in this set.
    active: HashSet<String>,
}

#[cfg(windows)]
static SIGNAL_STATE: OnceLock<Mutex<SigState>> = OnceLock::new();
#[cfg(windows)]
static HANDLER_INSTALLED: AtomicBool = AtomicBool::new(false);

// BOOL is i32; a console ctrl handler is `BOOL WINAPI HandlerRoutine(DWORD)`.
// Declared inline (kernel32, linked by default on MSVC) to match the FFI style
// used elsewhere in the tree (node_ops.rs' console helpers) and avoid a
// windows-sys type dependency here.
#[cfg(windows)]
type PhandlerRoutine = unsafe extern "system" fn(ctrl_type: u32) -> i32;
#[cfg(windows)]
unsafe extern "system" {
    fn SetConsoleCtrlHandler(handler: Option<PhandlerRoutine>, add: i32) -> i32;
}

#[cfg(windows)]
pub struct SignalHandle {
    name: String,
}

#[cfg(windows)]
impl Drop for SignalHandle {
    fn drop(&mut self) {
        if let Some(state) = SIGNAL_STATE.get() {
            let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
            guard.active.remove(&self.name);
        }
    }
}

#[cfg(windows)]
unsafe extern "system" fn ctrl_handler(ctrl_type: u32) -> i32 {
    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    const CTRL_CLOSE_EVENT: u32 = 2;
    const CTRL_LOGOFF_EVENT: u32 = 5;
    const CTRL_SHUTDOWN_EVENT: u32 = 6;
    // TRUE suppresses the OS default; FALSE lets the next handler / default run.
    const TRUE: i32 = 1;
    const FALSE: i32 = 0;
    let name = match ctrl_type {
        CTRL_C_EVENT => "SIGINT",
        CTRL_BREAK_EVENT => "SIGBREAK",
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => "SIGHUP",
        _ => return FALSE,
    };
    let Some(state) = SIGNAL_STATE.get() else {
        return FALSE;
    };
    let guard = state.lock().unwrap_or_else(|e| e.into_inner());
    if guard.active.contains(name) {
        if let Some(tx) = &guard.tx {
            let _ = tx.send(OpCompletion {
                id: SIGNAL_OP_ID,
                outcome: OpOutcome::Signal(name.to_string()),
            });
        }
        TRUE
    } else {
        FALSE
    }
}

#[cfg(windows)]
pub fn start_signal(
    _runtime: &tokio::runtime::Runtime,
    tx: &Sender<OpCompletion>,
    name: &str,
) -> Option<SignalHandle> {
    let state = SIGNAL_STATE.get_or_init(|| {
        Mutex::new(SigState {
            tx: None,
            active: HashSet::new(),
        })
    });
    {
        let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
        guard.tx = Some(tx.clone());
        guard.active.insert(name.to_string());
    }
    // Install the console ctrl handler exactly once (adding the same routine
    // twice would have it invoked twice per event).
    if !HANDLER_INSTALLED.swap(true, Ordering::SeqCst) {
        // SAFETY: passes a valid pointer to our zero-capture `extern "system"`
        // handler plus the add=1 flag; the call registers the handler and
        // dereferences no memory of ours. The swap guard ensures it runs once.
        unsafe {
            SetConsoleCtrlHandler(Some(ctrl_handler), 1);
        }
    }
    Some(SignalHandle {
        name: name.to_string(),
    })
}

#[cfg(windows)]
impl SignalHandle {
    /// No-op: Windows suppression is driven by the ctrl handler's `active`
    /// set, which `Drop` maintains, so dropping the handle already restores
    /// the OS default.
    pub fn set_watched(&self, _on: bool) {}
}

/// Windows drops the handle, which removes the name from the ctrl handler's
/// active set so the next event falls through to the OS default.
#[cfg(windows)]
pub fn stop_signal(map: &mut std::collections::HashMap<String, SignalHandle>, name: &str) {
    map.remove(name);
}

// ================================================ fallback (no known backend)
// Every tier-1 target is unix or windows; this keeps the crate compiling on a
// hypothetical other target (signals simply never fire there).

#[cfg(not(any(unix, windows)))]
pub struct SignalHandle;

#[cfg(not(any(unix, windows)))]
impl SignalHandle {
    pub fn set_watched(&self, _on: bool) {}
}

#[cfg(not(any(unix, windows)))]
pub fn stop_signal(map: &mut std::collections::HashMap<String, SignalHandle>, name: &str) {
    map.remove(name);
}

#[cfg(not(any(unix, windows)))]
pub fn start_signal(
    _runtime: &tokio::runtime::Runtime,
    _tx: &Sender<OpCompletion>,
    _name: &str,
) -> Option<SignalHandle> {
    None
}
