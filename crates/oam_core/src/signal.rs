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
//!   `SignalHandle` holds the recv task's `AbortHandle`; dropping it stops
//!   delivery. CAVEAT: tokio leaves its process-global handler installed on
//!   drop, so after the LAST listener is removed a later signal is caught and
//!   dropped (the process will not terminate) — this diverges from Node's
//!   "no listener -> OS default" only on the *remove* path and is benign for
//!   the k8s graceful-shutdown shape (the listener lives for the process).
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
}

#[cfg(unix)]
impl Drop for SignalHandle {
    fn drop(&mut self) {
        self.abort.abort();
    }
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
    let join = runtime.spawn(async move {
        while sig.recv().await.is_some() {
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
    })
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
        unsafe {
            SetConsoleCtrlHandler(Some(ctrl_handler), 1);
        }
    }
    Some(SignalHandle {
        name: name.to_string(),
    })
}

// ================================================ fallback (no known backend)
// Every tier-1 target is unix or windows; this keeps the crate compiling on a
// hypothetical other target (signals simply never fire there).

#[cfg(not(any(unix, windows)))]
pub struct SignalHandle;

#[cfg(not(any(unix, windows)))]
pub fn start_signal(
    _runtime: &tokio::runtime::Runtime,
    _tx: &Sender<OpCompletion>,
    _name: &str,
) -> Option<SignalHandle> {
    None
}
