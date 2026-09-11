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
//!   `SignalHandle` holds the recv task's `AbortHandle`; removing the last
//!   listener leaves it DORMANT (`set_watched(false)`) so a later listener
//!   re-arms the same task. tokio leaves its process-global handler installed
//!   for the process lifetime and offers no way to restore `SIG_DFL`, so
//!   something has to reproduce the OS default for every delivery that no JS
//!   listener is watching -- after `removeAllListeners('SIGINT')`, after the
//!   run that installed the handler is gone (`oam test` builds a runtime per
//!   file; a Worker or `oam.fork` isolate has its own), and for the SIGINT
//!   and SIGTERM raw mode arms with no listener at all. That is
//!   `serve_default_action`: ONE task per signal, on a runtime that is never
//!   dropped, which drains the exit hooks and then dies by the signal
//!   (restore `SIG_DFL`, re-raise), so the parent sees the right terminating
//!   signal. "No JS listener" is decided per PROCESS, from a count of watched
//!   handles across every isolate: tokio broadcasts each delivery to every
//!   receiver, so no one handle can decide alone. (It used to: each handle
//!   re-raised once its own listeners were gone, which killed the process
//!   under another isolate's listener, and a dropped run took the default
//!   with it -- the signal was then caught and discarded, and the process
//!   unkillable by it. Before that, `removeAllListeners('SIGINT')` followed
//!   by a SIGINT hung forever.)
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
    signum: i32,
    /// False once the last JS listener on this run is removed. The recv task
    /// stays alive while dormant, so a listener added later re-arms it.
    watched: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(unix)]
impl SignalHandle {
    pub fn set_watched(&self, on: bool) {
        if self.watched.swap(on, std::sync::atomic::Ordering::SeqCst) != on {
            count_watcher(self.signum, on);
        }
    }
}

#[cfg(unix)]
impl Drop for SignalHandle {
    fn drop(&mut self) {
        self.abort.abort();
        if self
            .watched
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            count_watcher(self.signum, false);
        }
    }
}

/// Watched handles per signal, across every isolate in the process. A
/// delivery that finds none gets the OS default from `serve_default_action`.
#[cfg(unix)]
static WATCHERS: std::sync::Mutex<std::collections::BTreeMap<i32, usize>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

#[cfg(unix)]
fn count_watcher(signum: i32, on: bool) {
    let mut watchers = WATCHERS.lock().unwrap_or_else(|e| e.into_inner());
    let count = watchers.entry(signum).or_insert(0);
    if on {
        *count += 1;
    } else {
        *count = count.saturating_sub(1);
    }
}

#[cfg(unix)]
fn watched_anywhere(signum: i32) -> bool {
    WATCHERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&signum)
        .is_some_and(|count| *count > 0)
}

/// Does the OS default action for this signal TERMINATE or STOP the process?
/// Only those need the restore-and-re-raise dance when no listener is
/// attached; for an ignore-by-default signal (SIGWINCH) simply dropping the
/// delivery already reproduces the default exactly.
///
/// SIGTSTP is in this set: its default action stops the process, and after
/// SIG_DFL is restored the re-raise reproduces exactly that (see `die_by` for
/// what a stop does differently from a death). SIGCONT is NOT: its default
/// is "continue", so re-raising after restore would be a no-op anyway, and
/// treating it as terminating could kill a process whose stop default had
/// been dropped -- a dormant SIGCONT must stay a no-op.
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
    let signum = kind.as_raw_value();
    // The default action for a delivery nobody watches is served process-wide
    // (see serve_default_action), and has to be in place before this handle
    // exists: it outlives the handle, the run and the listener.
    if default_terminates(signum) {
        serve_default_action(signum);
    }
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
    let watched = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    count_watcher(signum, true);
    let join = runtime.spawn(async move {
        while sig.recv().await.is_some() {
            // Forwarded whether or not this run still has a listener: a
            // `process.emit` with none is a no-op, and whether the delivery
            // instead takes the OS default is decided for the whole process
            // by serve_default_action's task, from the watcher count alone.
            // Gating on this run's flag as well opened a window, between the
            // flag and the count on the last removeListener, in which neither
            // task acted and the signal was lost.
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
        signum,
        watched,
    })
}

/// Serve `signum`'s OS default for the rest of the process: a delivery that no
/// JS listener anywhere in the process is watching drains the exit hooks and
/// then dies by the signal. Idempotent, and it returns only once tokio has
/// replaced `SIG_DFL`, so a caller that arms it can rely on the next delivery
/// being served.
///
/// One task per signal, on a runtime of its own that is never dropped. A task
/// on a run's runtime died with that run -- `oam test` builds one per file, a
/// Worker or `oam.fork` isolate one each -- while tokio's handler stayed
/// installed with nobody receiving, so every later delivery was caught and
/// discarded. Raw mode arms SIGINT and SIGTERM here with no listener at all,
/// which is node's `SignalExit`: a terminal left raw is restored by the exit
/// hooks on the way down.
#[cfg(unix)]
pub fn serve_default_action(signum: i32) {
    static SERVED: std::sync::Mutex<std::collections::BTreeSet<i32>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    static RUNTIME: std::sync::OnceLock<Option<tokio::runtime::Runtime>> =
        std::sync::OnceLock::new();
    let mut served = SERVED.lock().unwrap_or_else(|e| e.into_inner());
    if served.contains(&signum) {
        return;
    }
    let runtime = RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("oam-signal-default")
            .enable_all()
            .build()
            .ok()
    });
    let Some(runtime) = runtime.as_ref() else {
        return;
    };
    let mut sig = {
        let _guard = runtime.enter();
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(signum)) {
            Ok(sig) => sig,
            Err(_) => return,
        }
    };
    served.insert(signum);
    runtime.spawn(async move {
        while sig.recv().await.is_some() {
            // Ignore-by-default signals need nothing: dropping the delivery
            // already IS the default action.
            if default_terminates(signum) && !watched_anywhere(signum) {
                die_by(signum);
            }
        }
    });
}

/// The OS default for a signal whose default terminates or stops the process.
///
/// A death first runs the process-state hooks: a signal death emits no JS
/// 'exit', so the listeners that would normally put the terminal back into
/// cooked mode never run, and a shell left raw by a killed TUI does not
/// recover on its own. Those hooks only -- not the report-shaped exit hooks,
/// which print, and not the artifact sweep: what a terminating signal should
/// do about on-disk artifacts is a separate policy question this does not
/// answer.
///
/// A STOP (SIGTSTP) is not a death, and gets neither: the process comes back
/// on SIGCONT with its raw mode intact, as node's does (`SignalExit` covers
/// SIGINT and SIGTERM only, and the shell's job control restores the tty per
/// job). Draining the one-shot restore on a stop left the program running
/// cooked with `isRaw` still true, and its next signal death with no restore
/// left to run. And once `raise` returns -- the process was continued -- the
/// handler tokio installed goes back in: tokio registers each signal's
/// sigaction once for the process, so leaving SIG_DFL behind would make every
/// later SIGTSTP listener dead on arrival.
#[cfg(unix)]
fn die_by(signum: i32) {
    let stops = signum == libc::SIGTSTP;
    if !stops {
        crate::run_process_state_hooks();
    }
    // SAFETY: `sigaction` with a null new action only reads the current
    // disposition into `installed`, a live zeroed stack struct; `signal` and
    // `raise` take only the integer signal number and the well-known SIG_DFL
    // constant; the final `sigaction` writes back the struct just read, with
    // a null old-action pointer, which it permits. This runs only when the
    // signal's default action terminates or stops and no JS listener is
    // watching it, so restoring SIG_DFL and re-raising reproduces exactly that
    // default action; after a stop, the read-back disposition is reinstalled
    // unchanged.
    unsafe {
        let mut installed: libc::sigaction = std::mem::zeroed();
        libc::sigaction(signum, std::ptr::null(), &mut installed);
        libc::signal(signum, libc::SIG_DFL);
        libc::raise(signum);
        if stops {
            libc::sigaction(signum, &installed, std::ptr::null_mut());
        }
    }
}

/// Stop delivering `name` to JS.
///
/// Unix keeps the handle DORMANT rather than dropping it, so a listener added
/// later re-arms the same task. The OS default for a delivery that nobody is
/// watching is `serve_default_action`'s job, not this handle's.
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
