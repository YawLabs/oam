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
//!   file; a Worker or `oam.fork` isolate has its own), and for the signals
//!   armed with no listener at all: SIGINT and SIGTERM by raw mode, and the
//!   deadly ones by a cluster primary (`serve_death_defaults`). That is
//!   `serve_default_action`: ONE task per signal, on a runtime that is never
//!   dropped, which runs the process-state hooks (`die_by`) and then dies by
//!   the signal (restore `SIG_DFL`, re-raise), so the parent sees the right
//!   terminating signal. "No JS listener" is decided per PROCESS, from a count of watched
//!   handles across every isolate: tokio broadcasts each delivery to every
//!   receiver, so no one handle can decide alone. (It used to: each handle
//!   re-raised once its own listeners were gone, which killed the process
//!   under another isolate's listener, and a dropped run took the default
//!   with it -- the signal was then caught and discarded, and the process
//!   unkillable by it. Before that, `removeAllListeners('SIGINT')` followed
//!   by a SIGINT hung forever.)
//!
//! * Windows — `SetConsoleCtrlHandler`. The handler is a zero-capture
//!   `extern "system"` fn, so state lives in a process-global: the list of
//!   watchers across every isolate, each with its own op channel. It maps
//!   console control events to Node signal names as libuv does (Ctrl+C
//!   SIGINT, Ctrl+Break SIGBREAK, console close SIGHUP), hands the delivery
//!   to every watcher of the name, and returns TRUE (suppress the OS default)
//!   only when one took it. CAVEAT: Windows ends the process the moment a
//!   CTRL_CLOSE handler returns, so after dispatching SIGHUP the handler
//!   thread does not return; the listener then has the system's close
//!   timeout (~5s) before a forced kill, so SIGHUP-graceful shutdown is
//!   best-effort on Windows. CTRL_LOGOFF / CTRL_SHUTDOWN are not signals (as
//!   in libuv) and take the default. SIGTERM is accepted as a name but the
//!   OS never produces it.

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

/// Map a Node signal name to a tokio `SignalKind`, for the nine signals oam
/// watches. `None` for every other name: the JS listener is allowed but
/// never fires, and the signal keeps its OS default. That matches Node only
/// for a name that is no Unix signal ("SIGBREAK", which is Windows-only).
/// For a real one outside the table -- SIGPIPE, SIGALRM, SIGCHLD, SIGTTIN,
/// SIGXCPU, ... -- it is a gap: Node delivers every signal libuv can catch,
/// so `process.on('SIGALRM', fn)` there runs `fn` where oam still takes the
/// default action. The JS side's `SIGNAL_NAMES` is the same set, and
/// docs/node-divergences.md records the gap. Widening it means deciding each
/// added signal's unwatched default (`default_terminates`, and the stop
/// handling in `die_by`), not only adding a row here.
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
/// JS listener anywhere in the process is watching runs the process-state
/// hooks and then dies by the signal (`die_by`). Idempotent, and it returns only once tokio has
/// replaced `SIG_DFL`, so a caller that arms it can rely on the next delivery
/// being served.
///
/// One task per signal, on a runtime of its own that is never dropped. A task
/// on a run's runtime died with that run -- `oam test` builds one per file, a
/// Worker or `oam.fork` isolate one each -- while tokio's handler stayed
/// installed with nobody receiving, so every later delivery was caught and
/// discarded. Raw mode arms SIGINT and SIGTERM here with no listener at all,
/// which is node's `SignalExit`: a terminal left raw is restored by the
/// process-state hooks on the way down.
///
/// It replaces whatever disposition the signal had, an inherited `SIG_IGN`
/// included: a caller arming a signal nobody asked to listen for checks
/// `ignored` first (`serve_death_defaults`).
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

/// The signals whose unwatched default is a death, of the nine oam watches:
/// `default_terminates` without the stop.
#[cfg(unix)]
const DEATH_SIGNALS: [i32; 6] = [
    libc::SIGHUP,
    libc::SIGINT,
    libc::SIGTERM,
    libc::SIGQUIT,
    libc::SIGUSR1,
    libc::SIGUSR2,
];

/// Whether `signum` is ignored (`SIG_IGN`) right now -- as `nohup` leaves
/// SIGHUP, and a non-interactive shell leaves SIGINT and SIGQUIT for a job
/// it starts with `&`. False when the disposition cannot be read.
#[cfg(unix)]
fn ignored(signum: i32) -> bool {
    // SAFETY: `sigaction` with a null new action changes nothing; it only
    // writes the current disposition into `current`, a live zeroed stack
    // struct (all-zero is a valid `libc::sigaction`), which is read only
    // after the call reported success.
    unsafe {
        let mut current: libc::sigaction = std::mem::zeroed();
        libc::sigaction(signum, std::ptr::null(), &mut current) == 0
            && current.sa_sigaction == libc::SIG_IGN
    }
}

/// Route every deadly signal's default through `die_by`, for a process that
/// owes something on a signal death with no JS listener and no raw mode to
/// have armed it: a cluster primary, whose workers must die with it
/// (`cluster::reap_on_exit`). Left at `SIG_DFL` the kernel ends the process
/// with no code of ours run at all.
///
/// A signal that is ignored stays ignored. Arming it would install tokio's
/// handler over the `SIG_IGN`, and the next delivery would then be served
/// the DEFAULT -- death -- for a signal the process was started to survive
/// (`nohup oam primary.js`, closing the terminal).
///
/// Covers `DEATH_SIGNALS` only. SIGKILL cannot be caught, and the signals
/// outside oam's table (SIGPIPE, SIGALRM, SIGABRT, a fault, ...) keep the
/// kernel default, so a primary ended by one of those still leaves its
/// workers behind on unix.
#[cfg(unix)]
pub fn serve_death_defaults() {
    for signum in DEATH_SIGNALS {
        if !ignored(signum) {
            serve_default_action(signum);
        }
    }
}

/// Set once `die_by` has begun a death, before it runs the hooks.
#[cfg(unix)]
static DYING_BY_SIGNAL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether this process has begun to die of a signal: `die_by` is running the
/// process-state hooks, or has run them and is about to re-raise. Work that
/// counts on such a hook to undo it asks this once it has registered, and
/// undoes itself when the answer is yes: the hooks were taken just after the
/// flag went up, so a hook registered since may never run. A cluster worker
/// forked on another thread is the case (`cluster`'s `track_unless`): nothing
/// else would end it. With the fork gate (`fork_gate`) no fork can be in
/// flight while the hooks run, so this is the second line: it decides only
/// for a fork that took the gate after `die_by` let go of it, which happens
/// only if the re-raise came back.
#[cfg(unix)]
pub(crate) fn dying_by_signal() -> bool {
    DYING_BY_SIGNAL.load(std::sync::atomic::Ordering::SeqCst)
}

/// The fork gate: held by a cluster fork from before its `spawn()` until its
/// worker is tracked (`cluster::cluster_fork_with`), and by `die_by` from
/// before it raises `DYING_BY_SIGNAL` until after the re-raise. So when the
/// hooks of a signal death run, every fork is either wholly before them --
/// tracked, and so killed by `cluster`'s hook -- or wholly after: blocked at
/// the gate, never spawned, while the process dies. Without it a fork that
/// was still inside `spawn()` when the hooks ran tracked its worker only
/// afterwards, and a fork between `spawn()` and the track was missed by both
/// the hook and the flag.
///
/// Lock order, process-wide: this gate first, then `cluster`'s `LIVE`, then
/// the process-state hook registry. A fork takes all three in that order
/// (gate, then `track`: `LIVE`, and the registry under it); `die_by` takes the
/// gate and then the registry (`drain` lets go of it before any hook runs),
/// and the hook takes `LIVE`. Nothing takes the gate while holding either of
/// the others, and a thread waiting at the gate holds neither. `die_by` runs
/// on `serve_default_action`'s runtime thread, an ordinary thread, never in
/// a signal handler, so blocking on a mutex there is in order; and a fork
/// holding the gate never waits on that thread (`serve_death_defaults` only
/// spawns onto its runtime).
///
/// NOT covered: a hard exit (`exit_process`) runs the same hooks without the
/// gate, so a fork on another isolate's thread that races a `process.exit`
/// can still leave its worker running.
#[cfg(unix)]
static FORK_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the fork gate (`FORK_GATE`). A poisoned gate is taken all the same:
/// it guards no data, only an ordering.
#[cfg(unix)]
pub(crate) fn fork_gate() -> std::sync::MutexGuard<'static, ()> {
    FORK_GATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// The OS default for a signal whose default terminates or stops the process.
///
/// A death first runs the process-state hooks: a signal death emits no JS
/// 'exit', so the listeners that would normally put the terminal back into
/// cooked mode never run, and a shell left raw by a killed TUI does not
/// recover on its own; and nothing else would end a cluster primary's
/// workers. Those hooks only -- not the report-shaped exit hooks,
/// which print, and not the artifact sweep: what a terminating signal should
/// do about on-disk artifacts is a separate policy question this does not
/// answer.
///
/// This is NOT signal-handler context, so the hooks need not be
/// async-signal-safe (they take locks): the handler tokio installs only
/// writes a byte to its wake-up pipe, and `die_by` runs later, on an
/// ordinary thread -- a task of `serve_default_action`'s runtime, woken by
/// that pipe.
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
    // A death holds the fork gate from here to the end, the re-raise
    // included: no cluster fork can be mid-flight while the hooks run, and
    // none starts before the process is gone (`FORK_GATE`). A stop runs no
    // hooks and leaves forks alone. Were the re-raise to come back, the gate
    // is let go at the return and the flag below answers for later forks.
    let _fork_gate = (!stops).then(fork_gate);
    if !stops {
        // Raised before the hooks are taken, never lowered: whatever would
        // have registered a hook from here on has missed them
        // (`dying_by_signal`).
        DYING_BY_SIGNAL.store(true, std::sync::atomic::Ordering::SeqCst);
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
use std::sync::Mutex;
#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};

/// One live `SignalHandle`: a signal name some isolate is watching, and that
/// isolate's op channel.
#[cfg(windows)]
struct Watcher {
    id: u64,
    name: String,
    tx: Sender<OpCompletion>,
}

/// Every watcher in the process, across every isolate -- the Windows twin of
/// the unix half's per-process watcher count. A delivery goes to each watcher
/// of its name, and the OS default is suppressed while at least one of them
/// took it. (This used to be ONE sender, overwritten by each `start_signal`,
/// plus a set of names: a second isolate watching SIGINT took the deliveries
/// away from the first, and its handle going took the name out of the set, so
/// Ctrl-C then killed the process under the first isolate's listener.)
#[cfg(windows)]
struct SigState {
    watchers: Vec<Watcher>,
    next_id: u64,
}

#[cfg(windows)]
static SIGNAL_STATE: Mutex<SigState> = Mutex::new(SigState {
    watchers: Vec::new(),
    next_id: 0,
});
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
    /// This handle's entry in `SIGNAL_STATE.watchers`.
    id: u64,
}

#[cfg(windows)]
impl Drop for SignalHandle {
    fn drop(&mut self) {
        let mut guard = SIGNAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
        guard.watchers.retain(|watcher| watcher.id != self.id);
    }
}

/// Add a watcher of `name` delivering to `tx`. Its handle removes exactly
/// this watcher when dropped, whatever else watches the same name.
#[cfg(windows)]
fn register(tx: &Sender<OpCompletion>, name: &str) -> SignalHandle {
    let mut guard = SIGNAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
    let id = guard.next_id;
    guard.next_id += 1;
    guard.watchers.push(Watcher {
        id,
        name: name.to_string(),
        tx: tx.clone(),
    });
    SignalHandle { id }
}

/// Hand one delivery of `name` to every watcher of it. True when at least
/// one took it -- a watcher whose op channel is gone (its run is over) does
/// not count, so a delivery nobody can act on gets the OS default.
#[cfg(windows)]
fn deliver(name: &str) -> bool {
    let guard = SIGNAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
    let mut taken = false;
    for watcher in guard.watchers.iter().filter(|w| w.name == name) {
        taken |= watcher
            .tx
            .send(OpCompletion {
                id: SIGNAL_OP_ID,
                outcome: OpOutcome::Signal(name.to_string()),
            })
            .is_ok();
    }
    taken
}

#[cfg(windows)]
unsafe extern "system" fn ctrl_handler(ctrl_type: u32) -> i32 {
    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    const CTRL_CLOSE_EVENT: u32 = 2;
    // TRUE suppresses the OS default; FALSE lets the next handler / default run.
    const TRUE: i32 = 1;
    const FALSE: i32 = 0;
    // libuv's uv__signal_control_handler. CTRL_LOGOFF_EVENT and
    // CTRL_SHUTDOWN_EVENT are not signals there (they reach services only,
    // which have their own notification), so they take the default.
    let name = match ctrl_type {
        CTRL_C_EVENT => "SIGINT",
        CTRL_BREAK_EVENT => "SIGBREAK",
        CTRL_CLOSE_EVENT => "SIGHUP",
        _ => return FALSE,
    };
    // `deliver` has released the state lock by the time it returns, which
    // the park below depends on: the isolate thread takes that lock to drop
    // its handles on the way out.
    if !deliver(name) {
        return FALSE;
    }
    if ctrl_type == CTRL_CLOSE_EVENT {
        // Windows terminates the process as soon as a CTRL_CLOSE handler
        // returns, whatever it returns; while the handler has not returned,
        // the process keeps running until the system's close timeout (about
        // 5 s) ends it. So this thread -- one Windows creates for the
        // handler -- stays here, as libuv's does (`Sleep(INFINITE)`), and the
        // SIGHUP listener gets that time: it ends the process by exiting, or
        // the timeout does.
        loop {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    }
    TRUE
}

#[cfg(windows)]
pub fn start_signal(
    _runtime: &tokio::runtime::Runtime,
    tx: &Sender<OpCompletion>,
    name: &str,
) -> Option<SignalHandle> {
    let handle = register(tx, name);
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
    Some(handle)
}

#[cfg(windows)]
impl SignalHandle {
    /// No-op: Windows suppression is driven by the ctrl handler's watcher
    /// list, which `Drop` maintains, so dropping the handle already restores
    /// the OS default once no other isolate watches the name.
    pub fn set_watched(&self, _on: bool) {}
}

/// Windows drops the handle, which removes this run's watcher: the next event
/// falls through to the OS default unless another isolate still watches it.
#[cfg(windows)]
pub fn stop_signal(map: &mut std::collections::HashMap<String, SignalHandle>, name: &str) {
    map.remove(name);
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    fn took(rx: &std::sync::mpsc::Receiver<OpCompletion>, name: &str) -> bool {
        match rx.try_recv() {
            Ok(completion) => {
                completion.id == SIGNAL_OP_ID
                    && matches!(completion.outcome, OpOutcome::Signal(got) if got == name)
            }
            Err(_) => false,
        }
    }

    /// Two isolates watching one name each get the delivery, and one of them
    /// going leaves the other watching: the name stays suppressed until its
    /// last watcher is gone. Names no console event maps to, so a real
    /// Ctrl+C in the test run cannot interfere, and no handler is installed.
    #[test]
    fn watchers_are_per_isolate_and_counted() {
        const NAME: &str = "TEST-SIGNAL-COUNTED";
        let (main_tx, main_rx) = std::sync::mpsc::channel();
        let (worker_tx, worker_rx) = std::sync::mpsc::channel();
        assert!(!deliver(NAME), "nobody watches yet");

        let main = register(&main_tx, NAME);
        let worker = register(&worker_tx, NAME);
        assert!(deliver(NAME));
        assert!(took(&main_rx, NAME), "the first watcher keeps its delivery");
        assert!(took(&worker_rx, NAME));

        drop(worker);
        assert!(deliver(NAME), "the remaining watcher still suppresses");
        assert!(took(&main_rx, NAME));
        assert!(!took(&worker_rx, NAME));

        drop(main);
        assert!(!deliver(NAME), "the last watcher is gone: OS default");
    }

    /// A watcher whose op channel is gone cannot act on a delivery, so it
    /// does not suppress the default.
    #[test]
    fn a_watcher_with_no_receiver_does_not_suppress() {
        const NAME: &str = "TEST-SIGNAL-NO-RECEIVER";
        let (tx, rx) = std::sync::mpsc::channel();
        let _handle = register(&tx, NAME);
        drop(rx);
        assert!(!deliver(NAME));
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;

    /// Set on the re-exec'd run, which sh starts with SIGHUP ignored.
    const IGNORED_ROLE_VAR: &str = "OAM_SIGNAL_TEST_STARTED_IGNORING_HUP";
    const IGNORED_TEST: &str =
        "signal::unix_tests::arming_the_death_defaults_leaves_an_ignored_signal_ignored";

    /// A process started with a signal ignored (`nohup`, a shell's `&` job)
    /// keeps ignoring it when the deadly signals are armed: only an unguarded
    /// arm puts tokio's handler over the SIG_IGN, after which a delivery is
    /// served the default and kills the process. Dispositions are
    /// process-wide and cannot be put back from here, so the test re-execs
    /// itself under `sh` with SIGHUP trapped to ignore (an ignored signal
    /// stays ignored across exec) and checks in that process.
    #[test]
    fn arming_the_death_defaults_leaves_an_ignored_signal_ignored() {
        if std::env::var_os(IGNORED_ROLE_VAR).is_none() {
            let exe = std::env::current_exe().expect("the test binary's path");
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg("trap '' HUP; exec \"$0\" \"$@\"")
                .arg(exe)
                .args(["--exact", IGNORED_TEST, "--nocapture", "--test-threads=1"])
                .env(IGNORED_ROLE_VAR, "1")
                .output()
                .expect("re-exec the test binary under sh");
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
        assert!(
            ignored(libc::SIGHUP),
            "the precondition: sh's trap left SIGHUP ignored across the exec"
        );
        serve_death_defaults();
        assert!(
            ignored(libc::SIGHUP),
            "arming the death defaults installed a handler over SIG_IGN"
        );
        // What the guard prevents, and proof that `ignored` tells the two
        // apart: an unguarded arm replaces the SIG_IGN.
        serve_default_action(libc::SIGHUP);
        assert!(
            !ignored(libc::SIGHUP),
            "an unguarded arm leaves tokio's handler installed, not SIG_IGN"
        );
    }
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
