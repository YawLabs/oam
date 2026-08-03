//! ES module execution: host-driven graph preload, instantiate, evaluate.
//!
//! Design: the host (CLI/loader) preloads the whole module graph BEFORE
//! instantiation, so rich ODIF errors happen at load time where they can
//! carry codes and context. V8's resolve callback — which cannot fail
//! gracefully — then only looks up edges recorded during preload, so
//! callback-time resolution can never diverge from load-time resolution.
//!
//! The module map lives in an isolate slot because the resolve callback
//! must be a zero-capture function (rusty_v8 maps it to a C callback).
//!
//! Module identity: map keys are lexically-normalized absolute paths
//! ('.'/'..' collapsed — std::path::absolute keeps '..' on POSIX, which
//! would both double-instantiate modules and make '..'-spelled import
//! cycles recurse forever). Keys stay case-SENSITIVE on purpose: on
//! case-insensitive filesystems './A.ts' and './a.ts' become two module
//! instances, which matches Node's URL-keyed ESM behavior exactly.

use oam_diagnostics::{Diagnostic, Origin, Severity};
use std::collections::HashMap;
use std::num::NonZeroI32;
use std::path::{Component, Path, PathBuf};

use crate::JsRuntime;

/// What the engine needs from the embedder to load a module graph.
/// Errors are ODIF diagnostics — the engine never invents prose errors
/// for problems the host can describe precisely.
pub trait ModuleHost {
    /// Resolve `specifier` as written in the module at `referrer`.
    fn resolve(&self, specifier: &str, referrer: &Path) -> Result<PathBuf, Vec<Diagnostic>>;
    /// Read and prepare (e.g. transpile) the module at `path` as JS source.
    fn load(&self, path: &Path) -> Result<String, Vec<Diagnostic>>;
}

/// Raw pointer to the active `ModuleHost`, parked in an isolate slot so the
/// zero-capture `dynamic_import_callback` can resolve+load on demand the same
/// way the static graph does. The pointer is valid ONLY while `execute_module`
/// is on the stack (the host is borrowed for that whole call, and the callback
/// can only fire synchronously during the module evaluation it drives).
///
/// SAFETY contract -- enforced by EVERY entry point that runs JS on the
/// isolate:
///  - `execute_module` parks `Some(host_ptr)` and clears to `None` on return.
///  - `reset_run_slots` clears to `None` -- this is the inheritance path for
///    `execute_cjs` and `execute_worker` (both call `reset_run_slots` first).
///  - `tick`, `repl_eval`, `execute_script`, and `run_registered_tests` clear
///    to `None` at their head -- they accept JS that may call `import()` but
///    have no host of their own.
///
/// If a new JS-running entry point is added, it MUST clear this slot before
/// running any JS, or `dynamic_import_callback` will deref a stale pointer.
struct ActiveHost(Option<*const (dyn ModuleHost + 'static)>);

#[derive(Default)]
struct ModuleMap {
    by_path: HashMap<PathBuf, v8::Global<v8::Module>>,
    /// Identity hashes are NOT unique (V8 documents this); collisions are
    /// disambiguated in the resolve callback by module identity comparison.
    paths_by_hash: HashMap<NonZeroI32, Vec<PathBuf>>,
    /// (referrer path, specifier as written) -> resolved path, recorded at
    /// preload so the V8 resolve callback is a pure lookup.
    edges: HashMap<(PathBuf, String), PathBuf>,
}

/// Unhandled promise rejections, tracked via V8's reject callback so a
/// detached `Promise.reject(...)` cannot exit 0 in silence (Node parity:
/// ERR_UNHANDLED_REJECTION). Lives in an isolate slot for the same
/// zero-capture reason as ModuleMap. Carries the rejection VALUE (not just
/// text) so a process 'unhandledRejection' listener gets the real reason.
#[derive(Default)]
pub(crate) struct RejectionLedger {
    /// The reason VALUE only. Stringifying here would run user
    /// `toString`/`Symbol.toPrimitive` inside V8's reject callback -- which
    /// Node does not do, and which throws outright for a Symbol reason,
    /// leaving a scheduled exception that surfaces at the next check as a
    /// bogus "Cannot convert a Symbol value to a string" at the reject site.
    unhandled: Vec<(v8::Global<v8::Promise>, v8::Global<v8::Value>)>,
}

/// Uncaught exceptions V8 reports outside any TryCatch — microtask callbacks
/// are the reachable case: perform_microtask_checkpoint SWALLOWS their
/// exceptions (documented), and without a message listener V8 just printed
/// them to stdout while we exited 0 (review finding). The listener records
/// the VALUE here so a process 'uncaughtException' listener gets the real
/// Error; without a listener the event loop fails the run, like Node.
#[derive(Default)]
pub(crate) struct UncaughtLedger {
    entries: Vec<(v8::Global<v8::Value>, String)>,
}

/// V8 message listener. Zero capture, raw C ABI.
pub(crate) unsafe extern "C" fn message_listener(
    message: v8::Local<v8::Message>,
    exception: v8::Local<v8::Value>,
) {
    v8::callback_scope!(unsafe scope, message);
    let text = message.get(scope).to_rust_string_lossy(scope);
    let line = message.get_line_number(scope).unwrap_or(0);
    let file = message
        .get_script_resource_name(scope)
        .and_then(|name| name.to_string(scope))
        .map(|name| name.to_rust_string_lossy(scope))
        .unwrap_or_else(|| "<unknown>".to_string());
    let value = v8::Global::new(scope, exception);
    if let Some(ledger) = scope.get_slot_mut::<UncaughtLedger>() {
        ledger
            .entries
            .push((value, format!("{file}:{line}: {text}")));
    }
}

/// Deliver `event` to a `process` EventEmitter listener if one exists.
/// Returns true iff a listener ran without throwing (Node: a present
/// uncaughtException/unhandledRejection listener suppresses the default
/// fatal exit). A throwing handler is treated as not-handled (fatal),
/// with its own exception cleared so the original stands.
pub(crate) fn emit_process_event(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    event: &str,
    args: &[v8::Local<v8::Value>],
) -> bool {
    let lookup = (|| {
        let context = tc.get_current_context();
        let global = context.global(tc);
        let process_key = v8::String::new(tc, "process")?;
        let process = global.get(tc, process_key.into())?;
        let process = v8::Local::<v8::Object>::try_from(process).ok()?;
        let emit_key = v8::String::new(tc, "emit")?;
        let emit = v8::Local::<v8::Function>::try_from(process.get(tc, emit_key.into())?).ok()?;
        Some((process, emit))
    })();
    let Some((process, emit)) = lookup else {
        return false;
    };
    let Some(event_str) = v8::String::new(tc, event) else {
        return false;
    };
    let mut call_args: Vec<v8::Local<v8::Value>> = Vec::with_capacity(args.len() + 1);
    call_args.push(event_str.into());
    call_args.extend_from_slice(args);
    let recv: v8::Local<v8::Value> = process.into();
    match emit.call(tc, recv, &call_args) {
        // EventEmitter.emit returns true iff there were listeners.
        Some(result) => result.is_true(),
        None => {
            tc.reset(); // handler threw: clear it, the original stays fatal
            false
        }
    }
}

/// True iff `process` has at least one listener for `event` — checked
/// BEFORE draining the rejection ledger so a no-listener rejection stays
/// in the ledger and is fatal at end-of-run.
fn process_has_listener(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    event: &str,
) -> bool {
    (|| {
        let context = tc.get_current_context();
        let global = context.global(tc);
        let process_key = v8::String::new(tc, "process")?;
        let process =
            v8::Local::<v8::Object>::try_from(global.get(tc, process_key.into())?).ok()?;
        let count_key = v8::String::new(tc, "listenerCount")?;
        let count_fn =
            v8::Local::<v8::Function>::try_from(process.get(tc, count_key.into())?).ok()?;
        let event_str = v8::String::new(tc, event)?;
        let recv: v8::Local<v8::Value> = process.into();
        let result = count_fn.call(tc, recv, &[event_str.into()])?;
        Some(result.integer_value(tc).unwrap_or(0) > 0)
    })()
    .unwrap_or(false)
}

/// Fire 'unhandledRejection' for ledger entries that HAVE a listener,
/// promptly (Node fires after the microtask checkpoint, not at end-of-run).
/// No-listener rejections are left in the ledger for the fatal end check.
/// Called after each microtask checkpoint in the event loop.
pub(crate) fn flush_handled_rejections(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) {
    if !process_has_listener(tc, "unhandledRejection") {
        return;
    }
    let drained: Vec<(v8::Global<v8::Promise>, v8::Global<v8::Value>)> = tc
        .get_slot_mut::<RejectionLedger>()
        .map(|ledger| ledger.unhandled.drain(..).collect())
        .unwrap_or_default();
    let ran_listener = !drained.is_empty();
    for (promise, reason) in drained {
        let reason_local = v8::Local::new(tc, &reason);
        let promise_local = v8::Local::new(tc, &promise);
        emit_process_event(
            tc,
            "unhandledRejection",
            &[reason_local, promise_local.into()],
        );
    }
    // Explicit microtask policy: nothing flushes what the listener queued
    // (an async listener's continuation!) unless we do it here. A fatal
    // tick throw lands on the UncaughtLedger for the pump's next drain.
    if ran_listener {
        let _ = run_ticks_and_microtasks(tc);
    }
}

/// Route an uncaught exception through the JS canonical dispatcher
/// (`__oamDispatchUncaught`: uncaughtExceptionMonitor -> capture callback ->
/// 'uncaughtException' listeners, all with Node's origin second argument).
/// Returns true when JS consumed the error and the run survives. Falls back
/// to a bare 'uncaughtException' emit when the dispatcher global is
/// unavailable (early boot, stripped globals).
/// Outcome of routing an exception through the JS uncaught ladder.
pub(crate) enum Uncaught {
    /// A listener consumed it; the run survives.
    Handled,
    /// Nothing consumed it; report the ORIGINAL exception and exit.
    Fatal,
    /// A listener itself threw. Node reports the SECONDARY exception (its
    /// TryCatchScope(kFatal) around _fatalException reports what it caught),
    /// not the original, and exits 7.
    HandlerThrew {
        value: v8::Global<v8::Value>,
        frame: Option<String>,
        message: String,
    },
}

pub(crate) fn dispatch_uncaught(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    exception: v8::Local<v8::Value>,
) -> Uncaught {
    let lookup = (|| {
        let context = tc.get_current_context();
        let global = context.global(tc);
        let key = v8::String::new(tc, "__oamDispatchUncaught")?;
        v8::Local::<v8::Function>::try_from(global.get(tc, key.into())?).ok()
    })();
    let Some(dispatch) = lookup else {
        return if emit_process_event(tc, "uncaughtException", &[exception]) {
            Uncaught::Handled
        } else {
            Uncaught::Fatal
        };
    };
    let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
    match dispatch.call(tc, recv, &[exception]) {
        Some(result) => {
            if result.is_true() {
                Uncaught::Handled
            } else {
                Uncaught::Fatal
            }
        }
        None => {
            // Capture the handler's own exception BEFORE clearing -- that is
            // what Node reports.
            let thrown = tc.exception().map(|e| {
                let value = v8::Global::new(tc, e);
                let message =
                    crate::exception_to_error(tc, "uncaughtException handler").to_string();
                let frame = tc.message().and_then(|m| source_frame(tc, m));
                (value, frame, message)
            });
            tc.reset();
            // Node exits 7 when the uncaughtException handler itself throws
            // (kExceptionInFatalExceptionHandler). Stamp the sub-code for the
            // CLI's fatal path; 'exit' handlers can still mutate it after.
            let _ = (|| {
                let context = tc.get_current_context();
                let global = context.global(tc);
                let process_key = v8::String::new(tc, "process")?;
                let process =
                    v8::Local::<v8::Object>::try_from(global.get(tc, process_key.into())?).ok()?;
                let key = v8::String::new(tc, "__oamFatalCode")?;
                let seven: v8::Local<v8::Value> = v8::Number::new(tc, 7.0).into();
                process.set(tc, key.into(), seven)
            })();
            match thrown {
                Some((value, frame, message)) => Uncaught::HandlerThrew {
                    value,
                    frame,
                    message,
                },
                None => Uncaught::Fatal,
            }
        }
    }
}

/// Diagnostic code for an uncaught user exception. Rendered verbatim (see
/// `render` in oam_cli) because the message IS Node's fatal report.
pub(crate) const FATAL_CODE: &str = "OAM-RT0005";

/// Node's `<file>:<line>` + source line + caret preamble. Built from the V8
/// Message BEFORE the TryCatch is reset -- the message is gone after.
fn source_frame(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    message: v8::Local<'_, v8::Message>,
) -> Option<String> {
    let file = message
        .get_script_resource_name(tc)?
        .to_rust_string_lossy(tc);
    let line = message.get_line_number(tc)?;
    // Synthetic messages (eval wrappers) carry no usable location; Node
    // always has one, so print no frame at all rather than "undefined:0".
    if line == 0 || file.is_empty() || file == "undefined" {
        return None;
    }
    let src = message.get_source_line(tc)?.to_rust_string_lossy(tc);
    // V8 hands back a nonsense column for some synthetic messages (an ESM
    // eval wrapper reported usize::MAX, which overflowed the repeat), so
    // clamp to the line we are pointing into.
    let column = message.get_start_column().min(src.chars().count());
    let caret = format!("{}^", " ".repeat(column));
    Some(format!(
        "{file}:{line}
{src}
{caret}

"
    ))
}

/// Node prints an uncaught exception as the source frame plus
/// `util.inspect(err)` -- the stack, then any extra own properties
/// (`code`, `permission`, `syscall`, ...). oam's one-line diagnostic dropped
/// the stack entirely: unusable in production, and it hides the `^Error:`
/// line every Node test greps stderr for. The inspect half runs in JS
/// (`__oamFormatFatal`, js/bootstrap.js) so the formatting is the same
/// util.inspect userland sees.
fn fatal_report(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    exception: v8::Local<'_, v8::Value>,
    frame: Option<String>,
    fallback: &str,
) -> String {
    // An ESM top-level throw arrives as a rejected evaluation promise, with
    // no live Message on the scope -- rebuild one from the exception so the
    // report still opens with Node's source frame.
    let frame = frame.or_else(|| {
        let message = v8::Exception::create_message(tc, exception);
        source_frame(tc, message)
    });
    let body = (|| {
        let context = tc.get_current_context();
        let global = context.global(tc);
        let key = v8::String::new(tc, "__oamFormatFatal")?;
        let f = v8::Local::<v8::Function>::try_from(global.get(tc, key.into())?).ok()?;
        let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
        let out = f.call(tc, recv, &[exception])?;
        Some(out.to_rust_string_lossy(tc))
    })();
    // The formatter itself throwing would leave a pending exception that
    // poisons every later JS call on this scope.
    if tc.has_caught() {
        tc.reset();
    }
    let body = body.unwrap_or_else(|| fallback.to_string());
    // Node closes a fatal report with its runtime banner ("Node.js vX.Y.Z").
    // oam prints its OWN -- same structural role, and claiming to be Node
    // here would be a lie about which runtime produced the crash.
    format!(
        "{}{}

oam v{}
",
        frame.unwrap_or_default(),
        body,
        env!("CARGO_PKG_VERSION")
    )
}

/// `util.inspect(value)` via the JS side. ToString is wrong for a rejection
/// reason -- it throws on a Symbol and runs user `Symbol.toPrimitive` -- and
/// inspect is what Node renders these with anyway.
fn inspect_value(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    value: v8::Local<'_, v8::Value>,
) -> Option<String> {
    let context = tc.get_current_context();
    let global = context.global(tc);
    let key = v8::String::new(tc, "__oamFormatFatal")?;
    let f = v8::Local::<v8::Function>::try_from(global.get(tc, key.into())?).ok()?;
    let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
    let out = f.call(tc, recv, &[value]);
    if tc.has_caught() {
        tc.reset();
    }
    Some(out?.to_rust_string_lossy(tc))
}

/// A callback threw directly (timer/op settle). Offer it to a process
/// 'uncaughtException' listener; None = handled (continue), Some = fatal.
pub(crate) fn handle_uncaught_throw(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    context: &str,
) -> Option<Vec<Diagnostic>> {
    // The scope has a PENDING exception — V8 won't run JS in that state, so
    // capture the value + message, RESET, then try the listener.
    let Some(exception) = tc.exception() else {
        return Some(vec![catch_to_diagnostic(tc, context)]);
    };
    let value = v8::Global::new(tc, exception);
    let message = crate::exception_to_error(tc, context).to_string();
    let frame = tc.message().and_then(|m| source_frame(tc, m));
    tc.reset();

    let local = v8::Local::new(tc, &value);
    // A handler that threw replaces what gets reported (Node reports the
    // secondary exception, not the original).
    let (local, frame, message) = match dispatch_uncaught(tc, local) {
        Uncaught::Handled => return None,
        Uncaught::Fatal => (local, frame, message),
        Uncaught::HandlerThrew {
            value,
            frame,
            message,
        } => (v8::Local::new(tc, &value), frame, message),
    };
    Some(vec![Diagnostic::new(
        FATAL_CODE,
        Severity::Error,
        Origin::Runtime,
        fatal_report(tc, local, frame, &message),
    )])
}

/// Take any uncaught-exception reports (from the message listener, i.e.
/// exceptions swallowed by the microtask checkpoint) and either deliver
/// them to an 'uncaughtException' listener or fail the run.
pub(crate) fn drain_uncaught(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> Option<Vec<Diagnostic>> {
    let mut fatal = Vec::new();
    // Loop: a listener may queue microtasks/ticks (flushed below under the
    // explicit policy) whose throws land NEW ledger entries; quiesce before
    // returning. Terminates because each pass drains the ledger and only
    // fresh throws refill it.
    loop {
        let captured: Vec<(v8::Global<v8::Value>, String)> = tc
            .get_slot_mut::<UncaughtLedger>()
            .map(|ledger| ledger.entries.drain(..).collect())
            .unwrap_or_default();
        if captured.is_empty() {
            break;
        }
        for (value, text) in captured {
            let local = v8::Local::new(tc, &value);
            let (local, frame, message) = match dispatch_uncaught(tc, local) {
                Uncaught::Handled => continue,
                Uncaught::Fatal => (local, None, format!("Uncaught (in microtask) {text}")),
                Uncaught::HandlerThrew {
                    value,
                    frame,
                    message,
                } => (v8::Local::new(tc, &value), frame, message),
            };
            fatal.push(Diagnostic::new(
                FATAL_CODE,
                Severity::Error,
                Origin::Runtime,
                fatal_report(tc, local, frame, &message),
            ));
        }
        // Flush what the listeners queued -- explicit policy means nothing
        // else will until the next macrotask boundary.
        let _ = run_ticks_and_microtasks(tc);
    }
    if fatal.is_empty() { None } else { Some(fatal) }
}

/// Lexically collapse '.' and '..'. Input should already be absolute.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Strip Windows verbatim (`\\?\`) prefixes that `std::fs::canonicalize`
/// prepends, so a module identity never carries a form userland/JS never sees.
/// No-op on non-Windows and on already-plain paths.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.as_os_str().to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest.to_string());
        }
    }
    path
}

/// The canonical identity of a module file: one key regardless of how the path
/// was spelled. `std::fs::canonicalize` resolves symlinks, Windows 8.3 short
/// names (`RUNNER~1`), and drive/case aliases — the SAME resolution the loader
/// applies to imported paths (oam_loader `finalize_resolved`). This alignment
/// is load-bearing: the entry file is spelled as the CLI argument (which may be
/// a short-name temp path), while a cyclic re-import of that same file arrives
/// through the resolver as its canonical long-name path. If the two produced
/// different keys the module would be instantiated twice, breaking ESM cycle
/// linkage (a TDZ "cannot access X before initialization"). Virtual builtins
/// (`node:` / `oam:`) are not files — pass them through untouched. Canonicalize
/// failures (file removed mid-run, races) fall back to the lexical form so a
/// transient error never aborts the run.
pub(crate) fn module_key(path: &Path) -> std::io::Result<PathBuf> {
    if path
        .to_str()
        .is_some_and(|s| s.starts_with("node:") || s.starts_with("oam:"))
    {
        return Ok(path.to_path_buf());
    }
    let absolute = std::path::absolute(path)?;
    match std::fs::canonicalize(&absolute) {
        Ok(real) => Ok(strip_verbatim(real)),
        Err(_) => Ok(normalize_lexically(&absolute)),
    }
}

fn rt_diag(code: &str, message: impl Into<String>) -> Vec<Diagnostic> {
    vec![Diagnostic::new(
        code,
        Severity::Error,
        Origin::Runtime,
        message,
    )]
}

impl JsRuntime {
    /// Reset all per-run isolate slots. Shared by the ESM and CJS entry
    /// paths.
    /// Clear the dynamic-import host slot. Called by every JS-running entry
    /// point that has no host of its own (execute_script, repl_eval, tick,
    /// run_registered_tests). See the `ActiveHost` SAFETY note for why.
    pub(crate) fn clear_active_host(&mut self) {
        self.isolate.set_slot(ActiveHost(None));
    }

    /// Park the host pointer for `dynamic_import_callback`, so `import()`
    /// works from BOTH entry paths (ESM and CJS -- a CJS entry that awaits
    /// an `import()` is ordinary code, not an error). Call right after
    /// `reset_run_slots`, which clears the slot.
    ///
    /// SAFETY: `host` outlives the whole `execute_*` call, and the callback
    /// only fires synchronously during that evaluation; the slot is reset to
    /// None by reset_run_slots / tick() / repl_eval() so no later turn can
    /// read a stale pointer. The single `unsafe` for this lives here.
    pub(crate) fn park_active_host(&mut self, host: &dyn ModuleHost) {
        let host_ptr: *const (dyn ModuleHost + 'static) =
            unsafe { std::mem::transmute(host as *const dyn ModuleHost) };
        self.isolate.set_slot(ActiveHost(Some(host_ptr)));
    }

    pub(crate) fn reset_run_slots(&mut self) -> Result<(), Vec<Diagnostic>> {
        // Clear any host pointer parked by a prior execute_module on this same
        // JsRuntime. execute_cjs and execute_worker both call reset_run_slots
        // before running JS, so this single clear covers both -- a dynamic
        // import from CJS/worker code rejects cleanly instead of dereffing a
        // stale ptr. execute_module re-parks Some(ptr) right after this returns.
        self.isolate.set_slot(ActiveHost(None));
        self.isolate.set_slot(ModuleMap::default());
        self.isolate.set_slot(RejectionLedger::default());
        self.isolate.set_slot(UncaughtLedger::default());
        self.isolate.set_slot(crate::timers::TimerQueue::default());
        self.isolate.set_slot(crate::cjs::CjsCache::default());
        self.isolate
            .set_slot(crate::crypto_ops::CryptoState::default());
        // Fresh CoreRuntime per run: dropping the old one cancels any ops a
        // previous execute_module left in flight; PendingOps resolvers from
        // that run die with it.
        self.isolate.set_slot(
            oam_core::CoreRuntime::new()
                .map_err(|e| rt_diag("OAM-RT0002", format!("io runtime failed to start: {e}")))?,
        );
        self.isolate.set_slot(crate::ops::PendingOps::default());
        Ok(())
    }

    /// Load, instantiate, and evaluate the ES module graph rooted at `entry`.
    pub fn execute_module(
        &mut self,
        entry: &Path,
        host: &dyn ModuleHost,
    ) -> Result<(), Vec<Diagnostic>> {
        let entry = module_key(entry).map_err(|e| {
            rt_diag(
                "OAM-RT0002",
                format!("bad entry path {}: {e}", entry.display()),
            )
        })?;
        self.reset_run_slots()?;

        self.park_active_host(host);

        // --inspect-brk: cloned out so it owns its ref independently of the
        // upcoming &mut self.isolate borrow.
        let wait = self
            .inspector
            .as_ref()
            .filter(|state| state.break_on_start())
            .map(|state| state.shared());

        v8::scope_with_context!(let scope, &mut self.isolate, &self.context);
        v8::tc_scope!(let tc, scope);

        // Block until the debugger attaches, BEFORE any code runs, so the
        // client can set breakpoints (including in dependencies, whose bodies
        // run eagerly during graph load below).
        if let Some(shared) = &wait {
            shared.wait_for_attach();
        }

        load_module_graph(tc, host, entry.clone())?;

        let module = {
            let map = tc.get_slot::<ModuleMap>().expect("module map installed");
            let global = map.by_path.get(&entry).expect("entry preloaded").clone();
            v8::Local::new(tc, &global)
        };

        if module.instantiate_module(tc, resolve_module_callback) != Some(true) {
            return Err(vec![catch_to_diagnostic(tc, &entry.to_string_lossy())]);
        }
        // Arm break-on-start only NOW — graph load just ran any CJS
        // dependency bodies; arming earlier would land the pause inside
        // node_modules instead of the entry's first line.
        if let Some(shared) = &wait {
            shared.arm_break();
        }
        let Some(value) = module.evaluate(tc) else {
            return Err(vec![catch_to_diagnostic(tc, &entry.to_string_lossy())]);
        };
        if let Some(failure) = run_ticks_and_microtasks(tc) {
            return Err(failure);
        }
        if let Some(failure) = drain_uncaught(tc) {
            return Err(failure);
        }

        let promise = v8::Local::<v8::Promise>::try_from(value)
            .map_err(|_| rt_diag("OAM-RT0001", "module evaluation did not return a promise"))?;

        pump_event_loop(tc, Some(promise), true)?;

        match promise.state() {
            v8::PromiseState::Fulfilled => match unhandled_rejection_failures(tc) {
                Some(failures) => Err(failures),
                None => Ok(()),
            },
            v8::PromiseState::Rejected => {
                // A synchronous top-level throw (V8 rejects the module's
                // evaluation promise) carries the thrown VALUE in result().
                // Offer it to a process 'uncaughtException' listener before
                // failing (Node parity). mark_as_handled first so V8 does not
                // ALSO route this promise to the unhandled-rejection ledger.
                promise.mark_as_handled();
                let exception = promise.result(tc);
                // The full ladder, same as the CJS path: monitor -> capture
                // callback -> listeners. A bare emit here skipped
                // uncaughtExceptionMonitor entirely and mis-reported a
                // throwing handler.
                let (exception, frame) = match dispatch_uncaught(tc, exception) {
                    Uncaught::Handled => {
                        // A listener suppressed the fatal exit: continue like
                        // a clean run -- drain ticks + microtasks the handler
                        // queued (delivering any fresh throws), pump any
                        // pending timers/ops, then report unhandled
                        // rejections.
                        if let Some(failure) = run_ticks_and_microtasks(tc) {
                            return Err(failure);
                        }
                        if let Some(failure) = drain_uncaught(tc) {
                            return Err(failure);
                        }
                        pump_event_loop(tc, None, true)?;
                        return match unhandled_rejection_failures(tc) {
                            Some(failures) => Err(failures),
                            None => Ok(()),
                        };
                    }
                    Uncaught::Fatal => (exception, None),
                    Uncaught::HandlerThrew { value, frame, .. } => {
                        (v8::Local::new(tc, &value), frame)
                    }
                };
                let text = exception
                    .to_string(tc)
                    .map(|s| s.to_rust_string_lossy(tc))
                    .unwrap_or_else(|| "unknown exception".to_string());
                let fallback = format!("{}: Uncaught {text}", entry.display());
                Err(rt_diag(
                    FATAL_CODE,
                    fatal_report(tc, exception, frame, &fallback),
                ))
            }
            v8::PromiseState::Pending => Err(rt_diag(
                "OAM-RT0003",
                "top-level await never settled: no pending timers remain (deadlocked await)",
            )),
        }
    }
}

impl JsRuntime {
    /// Service the event loop for at most `budget` (REPL idle ticks):
    /// run any due timers and ready op completions, then wait out the
    /// remaining budget on the op channel. Errors are returned, not fatal.
    pub fn tick(&mut self, budget: std::time::Duration) -> Result<(), Vec<Diagnostic>> {
        let deadline = std::time::Instant::now() + budget;
        // No host on this path: clear any pointer a prior execute_module parked
        // so a dynamic import() during a tick rejects cleanly, never derefs stale.
        self.clear_active_host();
        v8::scope_with_context!(let scope, &mut self.isolate, &self.context);
        v8::tc_scope!(let tc, scope);
        loop {
            // In replay mode, advance the virtual clock so all pending timers
            // are immediately due -- no real sleep is needed.
            let is_replay = tc
                .get_slot::<crate::replay::ReplayState>()
                .is_some_and(|s| s.is_replay());
            let now = if is_replay {
                std::time::Instant::now() + std::time::Duration::from_secs(86400)
            } else {
                std::time::Instant::now()
            };
            if now >= deadline {
                return Ok(());
            }
            let mut progressed = false;
            let due = tc
                .get_slot_mut::<crate::timers::TimerQueue>()
                .and_then(|queue| queue.pop_due(now));
            if let Some((callback, extra)) = due {
                let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
                let callback = v8::Local::new(tc, &callback);
                let args: Vec<v8::Local<v8::Value>> =
                    extra.iter().map(|g| v8::Local::new(tc, g)).collect();
                if callback.call(tc, recv, &args).is_none() {
                    // A timer body threw directly -- the canonical async
                    // throw. Offer it to uncaughtException before dying.
                    if let Some(failure) = handle_uncaught_throw(tc, "timer callback") {
                        return Err(failure);
                    }
                }
                if let Some(failure) = run_ticks_and_microtasks(tc) {
                    return Err(failure);
                }
                progressed = true;
            }
            if let Some(completion) = tc
                .get_slot_mut::<oam_core::CoreRuntime>()
                .and_then(|core| core.try_recv())
            {
                let completion =
                    if let Some(state) = tc.get_slot_mut::<crate::replay::ReplayState>() {
                        crate::replay::intercept_completion(state, completion)
                    } else {
                        completion
                    };
                crate::ops::settle_completion(tc, completion);
                if let Some(failure) = run_ticks_and_microtasks(tc) {
                    return Err(failure);
                }
                progressed = true;
            }
            if let Some(failure) = drain_uncaught(tc) {
                return Err(failure);
            }
            if progressed {
                continue;
            }
            // Idle: block on the op channel for the remaining budget (or
            // the next timer, whichever is sooner).
            let next_timer = tc
                .get_slot_mut::<crate::timers::TimerQueue>()
                .and_then(|queue| queue.next_deadline());
            let wait_until = match next_timer {
                Some(timer) if timer < deadline => timer,
                _ => deadline,
            };
            // In replay mode, skip the sleep -- timers appear due via the
            // advanced virtual clock above; no real wall-clock wait needed.
            let replay_active = tc
                .get_slot::<crate::replay::ReplayState>()
                .is_some_and(|s| s.is_replay());
            let completion = if !replay_active {
                tc.get_slot_mut::<oam_core::CoreRuntime>()
                    .and_then(|core| core.recv_deadline(Some(wait_until)))
            } else {
                tc.get_slot_mut::<oam_core::CoreRuntime>()
                    .and_then(|core| core.try_recv())
            };
            if let Some(completion) = completion {
                let completion =
                    if let Some(state) = tc.get_slot_mut::<crate::replay::ReplayState>() {
                        crate::replay::intercept_completion(state, completion)
                    } else {
                        completion
                    };
                crate::ops::settle_completion(tc, completion);
                if let Some(failure) = run_ticks_and_microtasks(tc) {
                    return Err(failure);
                }
            } else if next_timer.is_none_or(|t| t >= deadline) {
                return Ok(());
            }
        }
    }

    /// One REPL evaluation: TypeScript-stripped, await-aware, result
    /// rendered via util.inspect, `_` bound to the last value. Err is the
    /// pretty error text.
    pub fn repl_eval(&mut self, source: &str) -> Result<String, String> {
        // No host on the REPL path: clear any parked pointer (defensive; the
        // REPL never calls execute_module, but keep the invariant uniform).
        self.clear_active_host();
        let has_await = source.contains("await");
        let prepared = if has_await {
            // Top-level await: evaluate as an async IIFE and pump until
            // settled. Expression form first (trailing semicolons from the
            // TS strip would break it); statement fallback below.
            let expression = source.trim_end().trim_end_matches(';');
            format!("(async () => ({expression}\n))()")
        } else {
            source.to_string()
        };

        let run = |runtime: &mut JsRuntime, text: &str| -> Result<String, (bool, String)> {
            v8::scope_with_context!(let scope, &mut runtime.isolate, &runtime.context);
            v8::tc_scope!(let tc, scope);
            let Some(code) = v8::String::new(tc, text) else {
                return Err((false, "input too long".to_string()));
            };
            let name: v8::Local<v8::Value> = v8::String::new(tc, "repl").unwrap().into();
            let origin =
                v8::ScriptOrigin::new(tc, name, 0, 0, false, 0, None, false, false, false, None);
            let Some(script) = v8::Script::compile(tc, code, Some(&origin)) else {
                let message = tc
                    .message()
                    .map(|m| m.get(tc).to_rust_string_lossy(tc))
                    .unwrap_or_else(|| "syntax error".to_string());
                return Err((true, message));
            };
            let Some(mut value) = script.run(tc) else {
                let message = tc
                    .message()
                    .map(|m| m.get(tc).to_rust_string_lossy(tc))
                    .unwrap_or_else(|| "uncaught exception".to_string());
                return Err((false, format!("Uncaught {message}")));
            };
            if let Some(failures) = run_ticks_and_microtasks(tc) {
                let text = failures
                    .iter()
                    .map(|d| d.message.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                return Err((false, text));
            }
            if let Some(entries) = drain_uncaught(tc) {
                let text = entries
                    .iter()
                    .map(|d| d.message.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                return Err((false, text));
            }

            // Settle a returned promise (the await wrapper, or any
            // promise the user typed) by pumping the loop.
            if let Ok(promise) = v8::Local::<v8::Promise>::try_from(value) {
                if pump_event_loop(tc, Some(promise), false).is_err() {
                    return Err((false, "event loop error while awaiting".to_string()));
                }
                match promise.state() {
                    v8::PromiseState::Fulfilled => value = promise.result(tc),
                    v8::PromiseState::Rejected => {
                        let text = promise
                            .result(tc)
                            .to_string(tc)
                            .map(|s| s.to_rust_string_lossy(tc))
                            .unwrap_or_default();
                        return Err((false, format!("Uncaught (in promise) {text}")));
                    }
                    v8::PromiseState::Pending => {
                        return Err((false, "promise never settled (deadlock)".to_string()));
                    }
                }
            }

            // Bind `_` and render via util.inspect (the snapshot console's
            // formatter), falling back to ToString.
            let context = tc.get_current_context();
            let global = context.global(tc);
            let underscore = v8::String::new(tc, "_").unwrap();
            global.set(tc, underscore.into(), value);

            let inspected = (|| {
                let registry_key = v8::String::new(tc, "__oamNode")?;
                let registry = global.get(tc, registry_key.into())?;
                let registry = v8::Local::<v8::Object>::try_from(registry).ok()?;
                let get_key = v8::String::new(tc, "get")?;
                let get =
                    v8::Local::<v8::Function>::try_from(registry.get(tc, get_key.into())?).ok()?;
                let util_name = v8::String::new(tc, "util")?;
                let util = get.call(tc, registry.into(), &[util_name.into()])?;
                let util = v8::Local::<v8::Object>::try_from(util).ok()?;
                let inspect_key = v8::String::new(tc, "inspect")?;
                let inspect =
                    v8::Local::<v8::Function>::try_from(util.get(tc, inspect_key.into())?).ok()?;
                let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
                let rendered = inspect.call(tc, recv, &[value])?;
                rendered.to_string(tc).map(|s| s.to_rust_string_lossy(tc))
            })();
            Ok(inspected.unwrap_or_else(|| {
                value
                    .to_string(tc)
                    .map(|s| s.to_rust_string_lossy(tc))
                    .unwrap_or_default()
            }))
        };

        match run(self, &prepared) {
            Ok(text) => Ok(text),
            Err((syntax, _message)) if has_await && syntax => {
                // Statement-shaped await input: re-wrap as a block.
                let block = format!("(async () => {{ {source}\n }})()");
                run(self, &block).map_err(|(_, m)| m)
            }
            Err((_, message)) => Err(message),
        }
    }

    /// Run the tests a just-evaluated test file registered: call
    /// `globalThis.__oamTestRun(filter)` (snapshot JS) and pump the event
    /// loop until its promise settles. Returns the results object as a
    /// JSON string. Reuses the run slots from the preceding
    /// execute_module/execute_cjs call -- do NOT reset them here, the test
    /// file's module state must stay alive.
    pub fn run_registered_tests(
        &mut self,
        filter: Option<&str>,
    ) -> Result<String, Vec<Diagnostic>> {
        // Clear ActiveHost so dynamic import() calls from inside test bodies
        // see None and reject with "not wired up on this entry path" instead
        // of dereferencing the stale host pointer left by execute_module.
        // Pattern mirrors tick() and repl_eval(): clear before creating the
        // scope so the borrow checker lets us take &mut self.isolate.
        self.isolate.set_slot(ActiveHost(None));
        v8::scope_with_context!(let scope, &mut self.isolate, &self.context);
        v8::tc_scope!(let tc, scope);

        let context = tc.get_current_context();
        let global = context.global(tc);
        let key = v8::String::new(tc, "__oamTestRun")
            .ok_or_else(|| rt_diag("OAM-RT0001", "test runner key allocation failed"))?;
        let runner = global
            .get(tc, key.into())
            .ok_or_else(|| rt_diag("OAM-TEST0003", "__oamTestRun missing from snapshot"))?;
        let runner = v8::Local::<v8::Function>::try_from(runner)
            .map_err(|_| rt_diag("OAM-TEST0003", "__oamTestRun is not a function"))?;

        let arg: v8::Local<v8::Value> = match filter {
            Some(filter) => v8::String::new(tc, filter)
                .ok_or_else(|| rt_diag("OAM-RT0001", "filter too long for V8 string"))?
                .into(),
            None => v8::undefined(tc).into(),
        };
        let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
        let Some(result) = runner.call(tc, recv, &[arg]) else {
            return Err(vec![catch_to_diagnostic(tc, "__oamTestRun")]);
        };
        if let Some(failure) = run_ticks_and_microtasks(tc) {
            return Err(failure);
        }
        if let Some(failure) = drain_uncaught(tc) {
            return Err(failure);
        }
        let promise = v8::Local::<v8::Promise>::try_from(result)
            .map_err(|_| rt_diag("OAM-TEST0003", "__oamTestRun did not return a promise"))?;

        pump_event_loop(tc, Some(promise), false)?;

        match promise.state() {
            v8::PromiseState::Fulfilled => {
                let value = promise.result(tc);
                let json = v8::json::stringify(tc, value)
                    .ok_or_else(|| rt_diag("OAM-TEST0003", "test results failed to serialize"))?;
                Ok(json.to_rust_string_lossy(tc))
            }
            v8::PromiseState::Rejected => {
                let exception = promise.result(tc);
                let text = exception
                    .to_string(tc)
                    .map(|s| s.to_rust_string_lossy(tc))
                    .unwrap_or_else(|| "unknown exception".to_string());
                Err(rt_diag(
                    "OAM-TEST0003",
                    format!("test run crashed outside any test: {text}"),
                ))
            }
            v8::PromiseState::Pending => Err(rt_diag(
                "OAM-TEST0003",
                "test run never settled: a test holds the loop open (deadlocked await?)",
            )),
        }
    }
}

/// The blocking event loop, shared by ESM and CJS entry paths. Each turn
/// services at most ONE due timer (so a clear issued by one callback
/// cancels same-instant siblings) AND THEN one ready op completion — never
/// timer-only: a continuously-due interval must not starve IO settlement
/// (review blocker; Node's phase model services poll after timers no
/// matter how busy timers are). Microtask drain after each, so dependent
/// code runs at the earliest correct moment. When idle, block on the op
/// channel with the next timer deadline as timeout (one blocking point, no
/// busy-wait). Exit when nothing remains; the process stays alive for
/// pending timers/ops after the entry settles — Node semantics. A rejected
/// entry promise breaks early (the caller reports it).
/// Node's processTicksAndRejections, host-owned (docs/design/
/// nexttick-engine.md): drain the JS nextTick queue, run a microtask
/// checkpoint, and loop while the checkpoint scheduled more ticks. This
/// replaces every bare perform_microtask_checkpoint that follows JS
/// execution, so ticks run ahead of already-queued promise jobs AND ticks
/// scheduled from promise jobs run after full microtask exhaustion. Absent
/// globals (the process factory has not materialized yet) mean no ticks can
/// exist -- the loop degrades to one plain checkpoint.
pub(crate) fn run_ticks_and_microtasks(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> Option<Vec<Diagnostic>> {
    loop {
        let drain = (|| {
            let context = tc.get_current_context();
            let global = context.global(tc);
            let key = v8::String::new(tc, "__oamDrainTicks")?;
            v8::Local::<v8::Function>::try_from(global.get(tc, key.into())?).ok()
        })();
        let have_drain = drain.is_some();
        if let Some(drain) = drain {
            let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
            if drain.call(tc, recv, &[]).is_none() {
                // The JS drain already ran Node's monitor/capture/listener
                // ladder; a throw only escapes for the fatal shapes (no
                // consumer, or the handler itself threw). Route it through
                // the uncaught LEDGER: drain_uncaught re-delivers to
                // uncaughtException (the documented oam divergence -- a
                // throwing handler re-delivers and the run survives) and
                // reports fatal only when no listener handles it. The
                // drain's finally already reset the queue, so looping on is
                // safe.
                let exception = tc.exception();
                let text = crate::exception_to_error(tc, "process.nextTick").to_string();
                if let Some(exception) = exception {
                    let value = v8::Global::new(tc, exception);
                    tc.reset();
                    if let Some(ledger) = tc.get_slot_mut::<UncaughtLedger>() {
                        ledger.entries.push((value, text));
                    }
                } else {
                    tc.reset();
                }
            }
        }
        tc.perform_microtask_checkpoint();
        if !has_pending_ticks(tc) {
            return None;
        }
        if !have_drain {
            // __oamHasTicks reports work but no drain fn resolved (globals
            // tampered/absent): looping would spin forever without running
            // any JS. Bail; the queue drains if the drain fn reappears.
            return None;
        }
    }
}

/// True when the JS nextTick queue has entries (absent globals = empty).
pub(crate) fn has_pending_ticks(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> bool {
    let lookup = (|| {
        let context = tc.get_current_context();
        let global = context.global(tc);
        let key = v8::String::new(tc, "__oamHasTicks")?;
        v8::Local::<v8::Function>::try_from(global.get(tc, key.into())?).ok()
    })();
    let Some(has) = lookup else {
        return false;
    };
    let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
    has.call(tc, recv, &[]).is_some_and(|v| v.is_true())
}

/// Marker slot: suppress 'beforeExit' emission for this runtime. Set by the
/// CLI's `oam test` path -- its module-eval pump would otherwise emit at the
/// registration drain, BEFORE the tests run (Node --test emits after).
pub struct SuppressBeforeExit;

/// Emit 'beforeExit' via the `__oamEmitBeforeExit` hook. Returns
/// Ok((emitted, handled_throw)):
/// - emitted: the hook ran (process not already exiting via process.exit);
///   the caller re-enters the loop to pick up handler-scheduled work.
/// - handled_throw: the handler threw and an uncaughtException listener
///   consumed it -- Node counts that as loop activity and RE-EMITS at the
///   next drain, so the caller must not latch its once-per-drain guard.
///
/// An unhandled handler throw is a fatal Err, like a timer callback.
fn emit_before_exit(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> Result<(bool, bool), Vec<Diagnostic>> {
    if tc.get_slot::<SuppressBeforeExit>().is_some() {
        return Ok((false, false));
    }
    let lookup = (|| {
        let context = tc.get_current_context();
        let global = context.global(tc);
        let key = v8::String::new(tc, "__oamEmitBeforeExit")?;
        v8::Local::<v8::Function>::try_from(global.get(tc, key.into())?).ok()
    })();
    let Some(hook) = lookup else {
        return Ok((false, false));
    };
    let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
    let (emitted, handled_throw) = match hook.call(tc, recv, &[]) {
        Some(result) => (result.is_true(), false),
        None => {
            if let Some(failure) = handle_uncaught_throw(tc, "beforeExit") {
                return Err(failure);
            }
            (true, true) // handler threw, a listener consumed it
        }
    };
    if let Some(failure) = run_ticks_and_microtasks(tc) {
        return Err(failure);
    }
    if let Some(failure) = drain_uncaught(tc) {
        return Err(failure);
    }
    Ok((emitted, handled_throw))
}

pub(crate) fn pump_event_loop(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    entry_promise: Option<v8::Local<'_, v8::Promise>>,
    report_unhandled: bool,
) -> Result<(), Vec<Diagnostic>> {
    // Once-per-drain 'beforeExit' guard (Node SpinEventLoop parity): set when
    // the hook fires at a drain, reset whenever the loop makes progress, so
    // handler-scheduled work re-arms the NEXT drain's emission while a
    // handler that schedules nothing exits after a single emission.
    let mut before_exit_emitted = false;
    loop {
        if entry_promise.is_some_and(|promise| promise.state() == v8::PromiseState::Rejected) {
            break;
        }
        // Surface rejections that survived the prior turn's microtask
        // checkpoint, PROMPTLY -- Node reports an unhandled rejection after the
        // microtask checkpoint of the turn it went unhandled, NOT at process
        // exit. On the run/sidecar paths (report_unhandled) a no-listener
        // rejection is fatal HERE, so a never-exiting loop (an MCP/http sidecar
        // held open by a ref'd timer or inflight op) cannot swallow it forever;
        // a listener still receives it promptly and the run continues. On the
        // REPL / test-runner paths we only flush to listeners and leave
        // no-listener rejections to the caller -- an interactive session must
        // not die on a background rejection. unhandled_rejection_failures drains
        // the ledger each turn, so the caller's post-loop end check is a no-op.
        if !report_unhandled {
            flush_handled_rejections(tc);
        } else if let Some(failures) = unhandled_rejection_failures(tc) {
            return Err(failures);
        }

        // Dispatch any debugger commands that arrived since the last turn.
        // Clone the Rc out first so the isolate slot borrow is released
        // before dispatch reenters V8 (it can run JS / pause).
        let inspector = tc
            .get_slot::<crate::inspector::InspectorSlot>()
            .map(|slot| slot.0.clone());
        if let Some(shared) = inspector {
            shared.poll();
            // CDP dispatch can run arbitrary JS (Runtime.evaluate); under
            // the explicit microtask policy nothing else flushes what it
            // queued -- a DevTools `await` would wedge until the debuggee's
            // next event.
            if let Some(failure) = run_ticks_and_microtasks(tc) {
                return Err(failure);
            }
        }

        let mut progressed = false;

        // In replay mode, advance virtual clock so timers are immediately due.
        let is_replay = tc
            .get_slot::<crate::replay::ReplayState>()
            .is_some_and(|s| s.is_replay());
        let now = if is_replay {
            std::time::Instant::now() + std::time::Duration::from_secs(86400)
        } else {
            std::time::Instant::now()
        };
        let due = tc
            .get_slot_mut::<crate::timers::TimerQueue>()
            .and_then(|queue| queue.pop_due(now));
        if let Some((callback, extra)) = due {
            let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
            let callback = v8::Local::new(tc, &callback);
            let args: Vec<v8::Local<v8::Value>> =
                extra.iter().map(|g| v8::Local::new(tc, g)).collect();
            let mut threw_handled = false;
            if callback.call(tc, recv, &args).is_none() {
                if let Some(failure) = handle_uncaught_throw(tc, "timer callback") {
                    return Err(failure);
                }
                threw_handled = true;
            }
            // Node ordering: ticks queued by a THROWING (but handled)
            // callback do not drain now -- they run after the next
            // successful callback / at queue exhaustion
            // (test-timers-immediate-queue-throw asserts stage 1 first).
            if !threw_handled {
                if let Some(failure) = run_ticks_and_microtasks(tc) {
                    return Err(failure);
                }
                if let Some(failure) = drain_uncaught(tc) {
                    return Err(failure);
                }
            }
            progressed = true;
        }

        if let Some(completion) = tc
            .get_slot_mut::<oam_core::CoreRuntime>()
            .and_then(|core| core.try_recv())
        {
            let completion = if let Some(state) = tc.get_slot_mut::<crate::replay::ReplayState>() {
                crate::replay::intercept_completion(state, completion)
            } else {
                completion
            };
            crate::ops::settle_completion(tc, completion);
            if let Some(failure) = run_ticks_and_microtasks(tc) {
                return Err(failure);
            }
            if let Some(failure) = drain_uncaught(tc) {
                return Err(failure);
            }
            progressed = true;
        }
        if progressed {
            before_exit_emitted = false;
            continue;
        }

        let next_deadline = tc
            .get_slot_mut::<crate::timers::TimerQueue>()
            .and_then(|queue| queue.next_deadline());
        let has_ref_timers = tc
            .get_slot::<crate::timers::TimerQueue>()
            .is_some_and(|queue| queue.has_ref_timers());
        let has_inflight = tc
            .get_slot::<oam_core::CoreRuntime>()
            .is_some_and(|core| core.has_inflight());
        // Belt-and-suspenders (docs/design/nexttick-engine.md): every
        // JS-running site drains through run_ticks_and_microtasks, so a
        // stranded tick here means a missed seam -- run it rather than
        // exiting or sleeping with work queued (Node never does either).
        // The drain routes fatal throws to the UncaughtLedger; deliver them
        // NOW or the `continue` -> `break` path would exit 0 on a fatal.
        if has_pending_ticks(tc) {
            if let Some(failure) = run_ticks_and_microtasks(tc) {
                return Err(failure);
            }
            if let Some(failure) = drain_uncaught(tc) {
                return Err(failure);
            }
            continue;
        }
        // Node: only ref'd timers and inflight ops keep the loop alive. When
        // the sole remaining work is unref'd timers, exit WITHOUT firing them.
        // (An unref'd timer due BEFORE this point still fired above via pop_due,
        // since `next_deadline` below still wakes us for it while other work
        // keeps the loop open.)
        if !has_ref_timers && !has_inflight {
            // Exit backstop: a fatal parked on the UncaughtLedger by any
            // seam that has no follow-up drain must fail the run, not
            // vanish into a clean exit.
            if let Some(failure) = drain_uncaught(tc) {
                return Err(failure);
            }
            // Natural drain: emit 'beforeExit' (main/sidecar/worker paths --
            // report_unhandled; never REPL/test-runner pumps) and re-enter
            // the loop so handler-scheduled work runs. The hook returns
            // false after explicit process.exit() (process._exiting).
            if report_unhandled && !before_exit_emitted {
                match emit_before_exit(tc) {
                    Err(failure) => return Err(failure),
                    Ok((true, handled_throw)) => {
                        // A handled handler-throw counts as loop activity in
                        // Node: leave the guard unlatched so the next drain
                        // re-emits (always-throw-with-listener is a legit
                        // infinite program).
                        before_exit_emitted = !handled_throw;
                        continue;
                    }
                    Ok((false, _)) => {}
                }
            }
            break;
        }
        match (next_deadline, has_inflight) {
            (None, false) => break,
            (deadline, true) => {
                // In replay mode, skip the blocking wait.
                let replay_active = tc
                    .get_slot::<crate::replay::ReplayState>()
                    .is_some_and(|s| s.is_replay());
                let completion = if replay_active {
                    tc.get_slot_mut::<oam_core::CoreRuntime>()
                        .and_then(|core| core.try_recv())
                } else {
                    tc.get_slot_mut::<oam_core::CoreRuntime>()
                        .and_then(|core| core.recv_deadline(deadline))
                };
                if let Some(completion) = completion {
                    let completion =
                        if let Some(state) = tc.get_slot_mut::<crate::replay::ReplayState>() {
                            crate::replay::intercept_completion(state, completion)
                        } else {
                            completion
                        };
                    crate::ops::settle_completion(tc, completion);
                    if let Some(failure) = run_ticks_and_microtasks(tc) {
                        return Err(failure);
                    }
                    if let Some(failure) = drain_uncaught(tc) {
                        return Err(failure);
                    }
                    // A settled completion is loop progress: re-arm the
                    // once-per-drain 'beforeExit' guard (work scheduled from
                    // a beforeExit handler settles HERE, not via try_recv).
                    before_exit_emitted = false;
                }
            }
            (Some(deadline), false) => {
                // No inflight ops -- only ref-timers keep the loop alive. We
                // wait to the timer deadline, but an inbound OS signal arrives
                // as a channel completion (SIGNAL_OP_ID) that must wake us so
                // process's 'SIGTERM'/'SIGINT' listeners fire PROMPTLY instead
                // of being deferred until the next timer. recv_deadline doubles
                // as the idle sleep AND the signal wakeup: it returns early
                // with the completion, or None when the deadline is reached.
                // In replay mode, skip the wait -- timers appear due via the
                // advanced virtual clock above.
                let replay_active = tc
                    .get_slot::<crate::replay::ReplayState>()
                    .is_some_and(|s| s.is_replay());
                if !replay_active {
                    // When the inspector is attached, cap each wait at
                    // POLL_INTERVAL so CDP messages aren't deferred until the
                    // timer fires (the same interleave the old sleep used).
                    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
                    let inspector = tc
                        .get_slot::<crate::inspector::InspectorSlot>()
                        .map(|slot| slot.0.clone());
                    loop {
                        let now = std::time::Instant::now();
                        if now >= deadline {
                            break;
                        }
                        let chunk_deadline = match &inspector {
                            Some(_) => (now + POLL_INTERVAL).min(deadline),
                            None => deadline,
                        };
                        let completion = tc
                            .get_slot_mut::<oam_core::CoreRuntime>()
                            .and_then(|core| core.recv_deadline(Some(chunk_deadline)));
                        if let Some(completion) = completion {
                            let completion = if let Some(state) =
                                tc.get_slot_mut::<crate::replay::ReplayState>()
                            {
                                crate::replay::intercept_completion(state, completion)
                            } else {
                                completion
                            };
                            crate::ops::settle_completion(tc, completion);
                            if let Some(failure) = run_ticks_and_microtasks(tc) {
                                return Err(failure);
                            }
                            if let Some(failure) = drain_uncaught(tc) {
                                return Err(failure);
                            }
                            // A signal listener may have scheduled work or called
                            // process.exit; re-evaluate the outer loop condition.
                            // Its callback was loop progress: re-arm the
                            // once-per-drain beforeExit guard like the other
                            // settle sites.
                            before_exit_emitted = false;
                            break;
                        }
                        if let Some(shared) = &inspector {
                            shared.poll();
                            // Same flush as the top-of-loop poll: CDP
                            // dispatch may have run JS mid-idle-wait.
                            if let Some(failure) = run_ticks_and_microtasks(tc) {
                                return Err(failure);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Drain the RejectionLedger into OAM-RT0004 failures (None = clean run).
pub(crate) fn unhandled_rejection_failures(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> Option<Vec<Diagnostic>> {
    let unhandled: Vec<(v8::Global<v8::Promise>, v8::Global<v8::Value>)> = tc
        .get_slot_mut::<RejectionLedger>()
        .map(|ledger| ledger.unhandled.drain(..).collect())
        .unwrap_or_default();
    if unhandled.is_empty() {
        return None;
    }
    let mut fatal = Vec::new();
    for (promise, reason) in unhandled {
        // Node: process.emit('unhandledRejection', reason, promise).
        let reason_local = v8::Local::new(tc, &reason);
        let promise_local = v8::Local::new(tc, &promise);
        if emit_process_event(
            tc,
            "unhandledRejection",
            &[reason_local, promise_local.into()],
        ) {
            continue;
        }
        fatal.push(Diagnostic::new(
            "OAM-RT0004",
            Severity::Error,
            Origin::Runtime,
            format!(
                "unhandled promise rejection: {}",
                inspect_value(tc, reason_local).unwrap_or_else(|| "unknown value".to_string())
            ),
        ));
    }
    // Flush what the listeners queued (async listener continuations) --
    // explicit policy means no one else will until the next macrotask.
    let _ = run_ticks_and_microtasks(tc);
    if fatal.is_empty() { None } else { Some(fatal) }
}

/// Compile the module graph into the isolate's ModuleMap. Explicit worklist —
/// recursion would overflow the stack on deep (generated-code) import chains.
/// Discovery is breadth-first in import-declaration order (FIFO): CJS
/// modules execute eagerly at load, so discovery order is their observable
/// side-effect order — declaration order is what Node-shaped code expects.
fn load_module_graph(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    host: &dyn ModuleHost,
    entry: PathBuf,
) -> Result<(), Vec<Diagnostic>> {
    let mut work = std::collections::VecDeque::from([entry]);
    while let Some(path) = work.pop_front() {
        if tc
            .get_slot::<ModuleMap>()
            .expect("module map installed")
            .by_path
            .contains_key(&path)
        {
            continue;
        }

        // Virtual modules (node builtins + oam: runtime modules):
        // instantiate via the snapshot registry, compile a facade module
        // under the virtual path. Registry keys are stripped for node:
        // ("fs") and full for oam: ("oam:test").
        let builtin = path.to_str().and_then(|s| {
            if let Some(stripped) = s.strip_prefix("node:") {
                Some(stripped.to_string())
            } else if s.starts_with("oam:") {
                Some(s.to_string())
            } else {
                None
            }
        });
        // CJS files take the interop path: execute eagerly through the
        // require machinery, then compile a generated ESM facade (real
        // runtime export names) as the module at this path. JSON files
        // ride the same branch when they arrive via npm resolution.
        let is_json = path.extension().and_then(|e| e.to_str()) == Some("json");
        let code = if let Some(name) = builtin {
            let Some(exports) = crate::cjs::get_builtin(tc, &name) else {
                return Err(vec![catch_to_diagnostic(tc, &path.to_string_lossy())]);
            };
            crate::cjs::builtin_facade_source(tc, &name, exports)
        } else if is_json {
            // JSON modules: parsed once through the CJS json loader (same
            // cache require() uses), exposed via the facade — default is
            // the parsed value; top-level keys are ALSO named exports, a
            // documented superset of Node's default-only shape (TS's
            // resolveJsonModule idiom expects the named form).
            let Some(exports) = crate::cjs::load_cjs(tc, &path) else {
                return Err(vec![catch_to_diagnostic(tc, &path.to_string_lossy())]);
            };
            crate::cjs::facade_source(tc, &path, exports)
        } else if oam_loader::module_kind(&path) == oam_loader::ModuleKind::Cjs {
            let Some(exports) = crate::cjs::load_cjs(tc, &path) else {
                return Err(vec![catch_to_diagnostic(tc, &path.to_string_lossy())]);
            };
            if let Some(failure) = run_ticks_and_microtasks(tc) {
                return Err(failure);
            }
            if let Some(failure) = drain_uncaught(tc) {
                return Err(failure);
            }
            crate::cjs::facade_source(tc, &path, exports)
        } else {
            host.load(&path)?
        };
        let file = path.to_string_lossy().into_owned();

        let source_str = v8::String::new(tc, &code).ok_or_else(|| {
            rt_diag(
                "OAM-RT0001",
                format!("{file}: source too long for V8 string"),
            )
        })?;
        let name: v8::Local<v8::Value> = v8::String::new(tc, &file)
            .ok_or_else(|| rt_diag("OAM-RT0001", format!("{file}: path too long for V8 string")))?
            .into();
        let origin = v8::ScriptOrigin::new(
            tc, name, 0, 0, false, 0, None, false, false, /* is_module */ true, None,
        );
        // V8 bytecode cache (B3b): consume a stored blob for this module
        // source if present (skips parse + top-level compile), else compile
        // fresh. Keyed by source + V8 version, so a foreign blob is never
        // deserialized here; V8's rejected() guards a stale one. `cached_blob`
        // is held across the compile because CachedData borrows it
        // (BufferNotOwned). Applies to every module source -- real ESM files
        // AND the generated builtin/CJS/JSON facades, all of which are
        // deterministic for a given source string (a different facade is a
        // different key, hence a miss, never a wrong hit).
        let cached_blob = crate::code_cache::load(&code, crate::code_cache::Kind::Module);
        let consuming = cached_blob.is_some();
        let mut source = match cached_blob {
            Some(ref blob) => v8::script_compiler::Source::new_with_cached_data(
                source_str,
                Some(&origin),
                v8::script_compiler::CachedData::new(blob),
            ),
            None => v8::script_compiler::Source::new(source_str, Some(&origin)),
        };
        let compile_options = if consuming {
            v8::script_compiler::CompileOptions::ConsumeCodeCache
        } else {
            v8::script_compiler::CompileOptions::NoCompileOptions
        };

        let Some(module) = v8::script_compiler::compile_module2(
            tc,
            &mut source,
            compile_options,
            v8::script_compiler::NoCacheReason::NoReason,
        ) else {
            return Err(vec![catch_to_diagnostic(tc, &file)]);
        };

        // Produce the bytecode on a miss (or if V8 rejected a stale blob).
        // Produced at compile time, so the cache captures top-level module
        // bytecode -- enough to skip the parse on the next run, which is the
        // dominant cost. Inner-function bytecode (a post-evaluation produce)
        // is a later refinement. Best-effort: a failed produce/write just
        // means the next run re-compiles. `module` is a Copy Local, so this
        // does not disturb the Global::new below.
        let refresh_cache = !consuming
            || source
                .get_cached_data()
                .map(|cd| cd.rejected())
                .unwrap_or(true);
        // enabled() skips the create_code_cache() serialize work when the cache
        // is off, not just the read/write -- a true off switch (OAM_CODE_CACHE=0).
        if refresh_cache
            && crate::code_cache::enabled()
            && let Some(cd) = module.get_unbound_module_script(tc).create_code_cache()
        {
            crate::code_cache::store(&code, crate::code_cache::Kind::Module, &cd);
        }

        let hash = module.get_identity_hash();
        let global = v8::Global::new(tc, module);
        {
            let map = tc
                .get_slot_mut::<ModuleMap>()
                .expect("module map installed");
            map.by_path.insert(path.clone(), global);
            map.paths_by_hash
                .entry(hash)
                .or_default()
                .push(path.clone());
        }

        let requests = module.get_module_requests();
        for i in 0..requests.length() {
            let request = requests.get(tc, i).expect("module request index in range");
            let request = v8::Local::<v8::ModuleRequest>::try_from(request)
                .expect("module request entry is a ModuleRequest");
            let specifier = request.get_specifier().to_rust_string_lossy(tc);

            // Import attributes: [key, value, source_offset] triples. The
            // only supported attribute is type, and the only supported
            // type is json. Spec-required behavior is to IGNORE unknown
            // keys; an unsupported type VALUE is an error.
            let attributes = request.get_import_attributes();
            let mut typed_json = false;
            let mut index = 0;
            while index + 1 < attributes.length() {
                let key = attributes
                    .get(tc, index)
                    .and_then(|d| v8::Local::<v8::Value>::try_from(d).ok())
                    .and_then(|v| v.to_string(tc))
                    .map(|s| s.to_rust_string_lossy(tc))
                    .unwrap_or_default();
                let value = attributes
                    .get(tc, index + 1)
                    .and_then(|d| v8::Local::<v8::Value>::try_from(d).ok())
                    .and_then(|v| v.to_string(tc))
                    .map(|s| s.to_rust_string_lossy(tc))
                    .unwrap_or_default();
                if key == "type" {
                    if value == "json" {
                        typed_json = true;
                    } else {
                        return Err(rt_diag(
                            "OAM-MOD0003",
                            format!(
                                "{}: import of '{specifier}' declares type \"{value}\" — \
                                 the only supported import-attribute type is \"json\"",
                                path.display()
                            ),
                        ));
                    }
                }
                index += 3;
            }

            let resolved = host.resolve(&specifier, &path)?;
            if typed_json && resolved.extension().and_then(|e| e.to_str()) != Some("json") {
                return Err(rt_diag(
                    "OAM-MOD0003",
                    format!(
                        "{}: '{specifier}' is imported with type \"json\" but resolves to {} \
                         (only .json files load as JSON modules today)",
                        path.display(),
                        resolved.display()
                    ),
                ));
            }
            // Virtual builtin paths must not go through filesystem
            // normalization (absolute() would anchor "node:fs" at cwd).
            let resolved = if resolved
                .to_str()
                .is_some_and(|s| s.starts_with("node:") || s.starts_with("oam:"))
            {
                resolved
            } else {
                module_key(&resolved).map_err(|e| {
                    rt_diag(
                        "OAM-RT0002",
                        format!("bad resolved path {}: {e}", resolved.display()),
                    )
                })?
            };
            tc.get_slot_mut::<ModuleMap>()
                .expect("module map installed")
                .edges
                .insert((path.clone(), specifier), resolved.clone());
            work.push_back(resolved);
        }
    }
    Ok(())
}

/// V8 resolve callback: pure lookup of preload-recorded edges. Zero capture.
/// Identity-hash collisions are disambiguated by comparing module identity.
/// On a lookup miss (should be unreachable) we throw a named error so the
/// user never sees a bare "unknown exception".
fn resolve_module_callback<'s>(
    context: v8::Local<'s, v8::Context>,
    specifier: v8::Local<'s, v8::String>,
    _import_attributes: v8::Local<'s, v8::FixedArray>,
    referrer: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Module>> {
    v8::callback_scope!(unsafe scope, context);
    let spec = specifier.to_rust_string_lossy(scope);

    // Identity-hash collisions: find the referrer's path by comparing module
    // identity among same-hash candidates. Slot borrows are cloned out before
    // any Local is created so they never overlap scope use.
    let candidates: Vec<(PathBuf, v8::Global<v8::Module>)> = {
        let map = scope.get_slot::<ModuleMap>()?;
        let paths = map.paths_by_hash.get(&referrer.get_identity_hash())?;
        paths
            .iter()
            .filter_map(|p| map.by_path.get(p).map(|g| (p.clone(), g.clone())))
            .collect()
    };
    let referrer_path = candidates.into_iter().find_map(|(path, global)| {
        let module = v8::Local::new(scope, &global);
        (module == referrer).then_some(path)
    });

    let target: Option<v8::Global<v8::Module>> = referrer_path.and_then(|referrer_path| {
        let map = scope.get_slot::<ModuleMap>()?;
        let resolved = map.edges.get(&(referrer_path, spec.clone()))?;
        map.by_path.get(resolved).cloned()
    });

    match target {
        Some(global) => Some(v8::Local::new(scope, &global)),
        None => {
            let message = v8::String::new(
                scope,
                &format!("oam internal: module edge not preloaded for specifier '{spec}'"),
            )
            .unwrap();
            let exception = v8::Exception::error(scope, message);
            scope.throw_exception(exception);
            None
        }
    }
}

/// Outcome of loading a dynamically-imported module, carried out of the child
/// TryCatch scope as scope-independent Globals so the parent scope can settle
/// the import promise after the child scope drops.
enum DynResult {
    /// Module evaluated cleanly; carries its namespace object.
    Namespace(v8::Global<v8::Value>),
    /// Module (or its graph) threw during evaluation; carries the reason value.
    Rejected(v8::Global<v8::Value>),
    /// oam-side failure (resolve/load/instantiate, or an unsupported shape);
    /// the message becomes an Error the import promise rejects with.
    Error(String),
    /// The import target is in kEvaluating (on our call stack). The caller
    /// (dynamic_import_callback) stores the resolver in PendingCycleImports so
    /// it can be resolved once the outer module finishes its synchronous body
    /// and transitions to kEvaluatingAsync.
    Deferred,
    /// The imported module has top-level await that is still pending because it
    /// is waiting for a cycle import (stored in PendingCycleImports). The outer
    /// dyn_import_load will flush the cycle and settle this module's resolver.
    /// Carries the module's resolved path for later lookup.
    EvalAsync(PathBuf),
}

/// Deferred import resolvers for modules currently in kEvaluating status.
/// Flushed by flush_cycle_imports once the target reaches kEvaluatingAsync.
#[derive(Default)]
struct PendingCycleImports(Vec<(PathBuf, v8::Global<v8::PromiseResolver>)>);

/// Deferred import resolvers for modules that have pending TLA.
/// Flushed by flush_tla_imports once the module reaches kEvaluated/kErrored.
#[derive(Default)]
struct PendingTLAImports(Vec<(PathBuf, v8::Global<v8::PromiseResolver>)>);

/// Parse the [key, value, source_offset] triples for the one supported import
/// attribute (type=json). Returns Ok(true) when type:"json" is set, Ok(false)
/// when no relevant attributes, Err(msg) when an unsupported type value is
/// declared. Mirrors the static-path validation in load_module_graph so dynamic
/// import has the same semantics.
fn dyn_parse_import_attributes(
    scope: &mut v8::PinScope<'_, '_>,
    attributes: v8::Local<'_, v8::FixedArray>,
    spec: &str,
) -> Result<bool, String> {
    let mut typed_json = false;
    let mut index = 0;
    while index + 1 < attributes.length() {
        let key = attributes
            .get(scope, index)
            .and_then(|d| v8::Local::<v8::Value>::try_from(d).ok())
            .and_then(|v| v.to_string(scope))
            .map(|s| s.to_rust_string_lossy(scope))
            .unwrap_or_default();
        let value = attributes
            .get(scope, index + 1)
            .and_then(|d| v8::Local::<v8::Value>::try_from(d).ok())
            .and_then(|v| v.to_string(scope))
            .map(|s| s.to_rust_string_lossy(scope))
            .unwrap_or_default();
        if key == "type" {
            if value == "json" {
                typed_json = true;
            } else {
                return Err(format!(
                    "oam: dynamic import('{spec}') declares type \"{value}\" \
                     — the only supported import-attribute type is \"json\""
                ));
            }
        }
        index += 3;
    }
    Ok(typed_json)
}

/// Reject `resolver` with a freshly-built Error carrying `msg`. Defense in
/// depth: if v8::String::new fails (message exceeds v8::String's max length),
/// fall back to an Error with an empty string rather than silently leaving the
/// resolver unsettled (which would hang the caller's awaited promise).
fn reject_with_message(
    scope: &mut v8::PinScope<'_, '_>,
    resolver: &v8::Local<'_, v8::PromiseResolver>,
    msg: &str,
) {
    let s = v8::String::new(scope, msg).unwrap_or_else(|| v8::String::empty(scope));
    let exception = v8::Exception::error(scope, s);
    resolver.reject(scope, exception);
}

/// Flush PendingCycleImports: resolve any whose target is now kEvaluatingAsync
/// or kEvaluated. Skips (puts back) items whose target is still kEvaluating
/// (we are still on its call stack). Returns (flushed, skipped) counts.
fn flush_cycle_imports(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> (usize, usize) {
    let pending = match tc.get_slot_mut::<PendingCycleImports>() {
        Some(p) => std::mem::take(&mut p.0),
        None => return (0, 0),
    };
    let mut flushed = 0;
    let mut skipped: Vec<(PathBuf, v8::Global<v8::PromiseResolver>)> = Vec::new();

    for (path, resolver_global) in pending {
        // Clone the module Global while holding an immutable slot borrow.
        let module_global: Option<v8::Global<v8::Module>> = {
            tc.get_slot::<ModuleMap>()
                .and_then(|m| m.by_path.get(&path))
                .cloned()
        };
        let resolver = v8::Local::new(tc, &resolver_global);
        match module_global {
            None => {
                let s = v8::String::new(tc, "oam: internal: cycle module not in map")
                    .unwrap_or_else(|| v8::String::empty(tc));
                let e = v8::Exception::error(tc, s);
                resolver.reject(tc, e);
                flushed += 1;
            }
            Some(mg) => {
                let module = v8::Local::new(tc, &mg);
                let status = module.get_status();
                match status {
                    v8::ModuleStatus::Evaluated => {
                        let ns = module.get_module_namespace();
                        let ns_val: v8::Local<v8::Value> = ns;
                        resolver.resolve(tc, ns_val);
                        flushed += 1;
                    }
                    v8::ModuleStatus::Errored => {
                        let exc = module.get_exception();
                        resolver.reject(tc, exc);
                        flushed += 1;
                    }
                    v8::ModuleStatus::Evaluating => {
                        // Still evaluating (sync body or TLA) -- defer.
                        skipped.push((path, resolver_global));
                    }
                    _ => {
                        let s = v8::String::new(
                            tc,
                            &format!("oam: cycle import: unexpected module status {status:?}"),
                        )
                        .unwrap_or_else(|| v8::String::empty(tc));
                        let e = v8::Exception::error(tc, s);
                        resolver.reject(tc, e);
                        flushed += 1;
                    }
                }
            }
        }
    }

    // Put back any items that couldn't be flushed yet.
    let skipped_len = skipped.len();
    if !skipped.is_empty()
        && let Some(p) = tc.get_slot_mut::<PendingCycleImports>()
    {
        p.0.extend(skipped);
    }
    (flushed, skipped_len)
}

/// Flush PendingTLAImports: resolve any whose module has reached kEvaluated
/// or kErrored. Skips (puts back) modules still in kEvaluatingAsync.
/// Returns number of items flushed.
fn flush_tla_imports(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> usize {
    let pending = match tc.get_slot_mut::<PendingTLAImports>() {
        Some(p) => std::mem::take(&mut p.0),
        None => return 0,
    };
    let mut flushed = 0;
    let mut still_pending: Vec<(PathBuf, v8::Global<v8::PromiseResolver>)> = Vec::new();

    for (path, resolver_global) in pending {
        let module_global: Option<v8::Global<v8::Module>> = {
            tc.get_slot::<ModuleMap>()
                .and_then(|m| m.by_path.get(&path))
                .cloned()
        };
        let resolver = v8::Local::new(tc, &resolver_global);
        match module_global {
            None => {
                let s = v8::String::new(tc, "oam: internal: TLA module not in map")
                    .unwrap_or_else(|| v8::String::empty(tc));
                let e = v8::Exception::error(tc, s);
                resolver.reject(tc, e);
                flushed += 1;
            }
            Some(mg) => {
                let module = v8::Local::new(tc, &mg);
                let status = module.get_status();
                match status {
                    v8::ModuleStatus::Evaluated => {
                        let ns = module.get_module_namespace();
                        let ns_val: v8::Local<v8::Value> = ns;
                        resolver.resolve(tc, ns_val);
                        flushed += 1;
                    }
                    v8::ModuleStatus::Errored => {
                        let exc = module.get_exception();
                        resolver.reject(tc, exc);
                        flushed += 1;
                    }
                    _ => {
                        // Still evaluating — keep in the queue.
                        still_pending.push((path, resolver_global));
                    }
                }
            }
        }
    }

    if !still_pending.is_empty()
        && let Some(p) = tc.get_slot_mut::<PendingTLAImports>()
    {
        p.0.extend(still_pending);
    }
    flushed
}

/// Load + instantiate + evaluate a dynamically-imported module through the same
/// machinery as the static graph, under the caller's TryCatch scope. Returns a
/// DynResult of Globals so the namespace/reason survives the scope drop.
fn dyn_import_load(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    host: &dyn ModuleHost,
    resolved: PathBuf,
    spec: &str,
) -> DynResult {
    // Pull the rest of the graph in (idempotent: already-loaded paths skip).
    if let Err(diags) = load_module_graph(tc, host, resolved.clone()) {
        let msg = if diags.is_empty() {
            format!("failed to load '{spec}'")
        } else {
            diags
                .iter()
                .map(|d| format!("{}: {}", d.code, d.message))
                .collect::<Vec<_>>()
                .join("; ")
        };
        return DynResult::Error(format!("oam: dynamic import('{spec}'): {msg}"));
    }

    let module = {
        let map = tc.get_slot::<ModuleMap>().expect("module map installed");
        match map.by_path.get(&resolved) {
            Some(global) => v8::Local::new(tc, global),
            None => {
                return DynResult::Error(format!(
                    "oam internal: dynamic import('{spec}') loaded but not registered at {}",
                    resolved.display()
                ));
            }
        }
    };

    // Handle cycle cases BEFORE calling evaluate() or get_module_namespace(),
    // both of which CHECK-fail on kEvaluating in all V8 builds:
    //
    // kEvaluatingAsync -- the module's TLA is in progress. evaluate() on this
    //   state returns the existing (pending) TLA promise, and get_module_namespace()
    //   is safe. Return the partial namespace now: bindings initialized before
    //   the TLA suspended are live; unset ones are TDZ. This is the correct
    //   ES2022 semantics for a TLA cycle (A imports B imports A-while-A's-TLA
    //   is pending).
    //
    // kEvaluating -- the module's synchronous body is currently executing
    //   (we are on its call stack). Calling evaluate() again triggers a V8
    //   CHECK failure. get_module_namespace() is also forbidden for this
    //   status. Report as a hard-sync cycle error rather than crashing.
    match module.get_status() {
        v8::ModuleStatus::Evaluated => {
            // Already complete (including any TLA): return the namespace immediately.
            return DynResult::Namespace(v8::Global::new(tc, module.get_module_namespace()));
        }
        v8::ModuleStatus::Evaluating => {
            // Either the sync body is on our call stack, or TLA is in progress.
            // In both cases defer and let flush_cycle_imports retry once the outer
            // evaluate() returns and the module reaches Evaluated.
            return DynResult::Deferred;
        }
        _ => {}
    }

    // Instantiate only from a fresh module; a statically-loaded one is already
    // (at least) Instantiated and re-instantiating would error.
    if module.get_status() == v8::ModuleStatus::Uninstantiated
        && module.instantiate_module(tc, resolve_module_callback) != Some(true)
    {
        return match dyn_caught_value(tc) {
            Some(v) => DynResult::Rejected(v),
            None => DynResult::Error(format!(
                "oam: dynamic import('{spec}'): instantiation failed"
            )),
        };
    }
    if module.get_status() == v8::ModuleStatus::Errored {
        return DynResult::Rejected(v8::Global::new(tc, module.get_exception()));
    }
    // Cycle: the module is currently being evaluated (cycle from itself or a
    // mutual dynamic-import dependency). Per the import() spec / Node parity,
    // the dynamic-import promise resolves with the module's PARTIAL namespace,
    // not "TLA not supported yet" -- the latter is what the post-evaluate
    // Pending branch below would otherwise report. get_module_namespace() is
    // safe here: status is >= Instantiated (Evaluating).
    if module.get_status() == v8::ModuleStatus::Evaluating {
        return DynResult::Namespace(v8::Global::new(tc, module.get_module_namespace()));
    }

    let Some(value) = module.evaluate(tc) else {
        return match dyn_caught_value(tc) {
            Some(v) => DynResult::Rejected(v),
            None => DynResult::Error(format!("oam: dynamic import('{spec}'): evaluation failed")),
        };
    };
    // Bare checkpoint, NO tick drain: this hook fires synchronously with
    // the importer's JS frame still on the stack, and Node never runs
    // process.nextTick callbacks mid-statement. Ticks scheduled by the
    // imported module wait for the caller's next real tick point. (The
    // mid-hook microtask flush itself is a documented pre-existing
    // divergence the sync-graph fast path depends on.)
    tc.perform_microtask_checkpoint();

    // evaluate() yields the module's top-level-await promise.
    let Ok(eval_promise) = v8::Local::<v8::Promise>::try_from(value) else {
        // Non-promise return is unexpected for a SourceText module; treat the
        // value as the namespace fallback rather than failing.
        return DynResult::Namespace(v8::Global::new(tc, module.get_module_namespace()));
    };
    match eval_promise.state() {
        v8::PromiseState::Fulfilled => {
            // The module's evaluation promise is the channel we consumed for
            // the namespace; mark it handled so V8 doesn't ALSO report it as
            // an unhandled rejection (it was Fulfilled, but a defensive
            // mark here is symmetric with the Rejected arm).
            eval_promise.mark_as_handled();
            DynResult::Namespace(v8::Global::new(tc, module.get_module_namespace()))
        }
        v8::PromiseState::Rejected => {
            // We're consuming the rejection reason into the import promise;
            // mark the eval promise handled so V8 doesn't also report it as
            // a separate unhandled rejection (the user catches via the
            // dynamically-imported promise we return).
            eval_promise.mark_as_handled();
            DynResult::Rejected(v8::Global::new(tc, eval_promise.result(tc)))
        }
        // Synchronous graphs settle within the microtask checkpoint above.
        // A pending promise means the module has TLA. If there are deferred
        // cycle imports (PendingCycleImports), run a flush loop: resolve them
        // once the target modules reach kEvaluatingAsync, then drain the
        // resulting microtasks. Repeat until quiescent. If the flush makes
        // no progress because all cycle items are still kEvaluating (we are
        // on their call stack), signal EvalAsync upward so the outer level
        // can flush after its module's evaluate() returns.
        v8::PromiseState::Pending => {
            let mut did_something = true;
            while did_something {
                did_something = false;
                let (cycle_flushed, cycle_skipped) = flush_cycle_imports(tc);
                if cycle_flushed > 0 {
                    // Bare checkpoint, no tick drain: still inside the
                    // dynamic-import host hook (importer JS on the stack).
                    tc.perform_microtask_checkpoint();
                    did_something = true;
                } else if cycle_skipped > 0 {
                    // All pending cycle imports are still kEvaluating (we're on
                    // their call stack). Tell the caller to stash our resolver
                    // in PendingTLAImports; the outer level will flush us.
                    return DynResult::EvalAsync(resolved);
                }
                let tla_flushed = flush_tla_imports(tc);
                if tla_flushed > 0 {
                    // Bare checkpoint, no tick drain (see above).
                    tc.perform_microtask_checkpoint();
                    did_something = true;
                }
            }
            // Re-check after all rounds.
            match eval_promise.state() {
                v8::PromiseState::Fulfilled => {
                    DynResult::Namespace(v8::Global::new(tc, module.get_module_namespace()))
                }
                v8::PromiseState::Rejected => {
                    DynResult::Rejected(v8::Global::new(tc, eval_promise.result(tc)))
                }
                v8::PromiseState::Pending => DynResult::Error(format!(
                    "oam: dynamic import('{spec}') of a module with top-level \
                     await is not supported yet"
                )),
            }
        }
    }
}

/// The currently-caught exception (if any) as a Global, for reporting an
/// instantiate/evaluate failure as a promise rejection with the real value.
fn dyn_caught_value(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> Option<v8::Global<v8::Value>> {
    let exception = tc.exception()?;
    Some(v8::Global::new(tc, exception))
}

/// Dynamic import() host callback. Resolves `specifier` relative to the
/// importing module, then loads/instantiates/evaluates it through the same
/// path as the static graph and settles the import promise with the module
/// namespace. Synchronous graphs (incl. node:/oam: builtins and CJS) resolve
/// inline; a module whose own top-level await is still pending is rejected
/// with a clear message (nested-pump support is a follow-up).
pub(crate) fn dynamic_import_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    _host_defined_options: v8::Local<'s, v8::Data>,
    resource_name: v8::Local<'s, v8::Value>,
    specifier: v8::Local<'s, v8::String>,
    import_attributes: v8::Local<'s, v8::FixedArray>,
) -> Option<v8::Local<'s, v8::Promise>> {
    let resolver = v8::PromiseResolver::new(scope)?;
    let promise = resolver.get_promise(scope);
    let spec = specifier.to_rust_string_lossy(scope);
    let referrer = resource_name.to_rust_string_lossy(scope);

    // Parse import attributes (type=json) the same way the static path does.
    // Unknown keys are silently ignored per spec; an unsupported type VALUE is
    // an error before we even try to resolve.
    let typed_json = match dyn_parse_import_attributes(scope, import_attributes, &spec) {
        Ok(flag) => flag,
        Err(msg) => {
            reject_with_message(scope, &resolver, &msg);
            return Some(promise);
        }
    };

    // The active host is parked by execute_module; absent on the REPL/timer
    // paths and from inside run_registered_tests (which clears the slot so
    // test bodies cannot dereference the stale pointer from execute_module).
    let Some(host_ptr) = scope.get_slot::<ActiveHost>().and_then(|h| h.0) else {
        reject_with_message(
            scope,
            &resolver,
            &format!(
                "oam: dynamic import('{spec}') is not wired up on this entry path \
                 (REPL, timer, op callback, or test body)"
            ),
        );
        return Some(promise);
    };
    // SAFETY: the pointer is valid for the whole execute_module call (the host
    // is borrowed there), and this callback only fires synchronously during the
    // evaluation that call drives. See ActiveHost.
    let host: &dyn ModuleHost = unsafe { &*host_ptr };

    // Resolve the specifier relative to the importing module.
    let referrer_path = PathBuf::from(&referrer);
    let resolved = match host.resolve(&spec, &referrer_path) {
        Ok(p) => p,
        Err(diags) => {
            let msg = diags
                .first()
                .map(|d| format!("{}: {}", d.code, d.message))
                .unwrap_or_else(|| format!("cannot resolve '{spec}'"));
            reject_with_message(
                scope,
                &resolver,
                &format!("oam: dynamic import('{spec}'): {msg}"),
            );
            return Some(promise);
        }
    };
    // Virtual builtin paths bypass filesystem normalization (module_key /
    // absolute() would anchor "node:fs" at cwd); everything else is keyed.
    let resolved = if resolved
        .to_str()
        .is_some_and(|s| s.starts_with("node:") || s.starts_with("oam:"))
    {
        resolved
    } else {
        match module_key(&resolved) {
            Ok(k) => k,
            Err(e) => {
                reject_with_message(
                    scope,
                    &resolver,
                    &format!(
                        "oam: dynamic import('{spec}'): bad resolved path {}: {e}",
                        resolved.display()
                    ),
                );
                return Some(promise);
            }
        }
    };

    // type:"json" requires a .json target (parity with the static path).
    if typed_json && resolved.extension().and_then(|e| e.to_str()) != Some("json") {
        reject_with_message(
            scope,
            &resolver,
            &format!(
                "oam: dynamic import('{spec}') is imported with type \"json\" \
                 but resolves to {} (only .json files load as JSON modules today)",
                resolved.display()
            ),
        );
        return Some(promise);
    }

    // Heavy V8 work under a child TryCatch; carry the result out as Globals.
    // Clone resolved so it is available for the Deferred/EvalAsync branches
    // after the tc scope (and borrow) drops.
    let resolved_for_defer = resolved.clone();
    let result = {
        v8::tc_scope!(let tc, scope);
        dyn_import_load(tc, host, resolved, &spec)
    };

    match result {
        DynResult::Namespace(ns) => {
            let value = v8::Local::new(scope, &ns);
            resolver.resolve(scope, value);
        }
        DynResult::Rejected(reason) => {
            let value = v8::Local::new(scope, &reason);
            resolver.reject(scope, value);
        }
        DynResult::Error(msg) => reject_with_message(scope, &resolver, &msg),
        DynResult::Deferred => {
            // The target module is kEvaluating (we are on its call stack).
            // Stash the resolver; flush_cycle_imports() will resolve it once the
            // outer module reaches kEvaluatingAsync.
            let global_resolver = v8::Global::new(scope, resolver);
            if scope.get_slot::<PendingCycleImports>().is_none() {
                scope.set_slot(PendingCycleImports::default());
            }
            scope
                .get_slot_mut::<PendingCycleImports>()
                .expect("just set")
                .0
                .push((resolved_for_defer, global_resolver));
            // Return the unsettled promise — it will be resolved by the flush.
        }
        DynResult::EvalAsync(path) => {
            // The module has pending TLA that will settle once the cycle import
            // it is waiting for is resolved by the outer level. Stash the
            // resolver in PendingTLAImports.
            let global_resolver = v8::Global::new(scope, resolver);
            if scope.get_slot::<PendingTLAImports>().is_none() {
                scope.set_slot(PendingTLAImports::default());
            }
            scope
                .get_slot_mut::<PendingTLAImports>()
                .expect("just set")
                .0
                .push((path, global_resolver));
            // Return the unsettled promise — it will be resolved by the flush.
        }
    }
    Some(promise)
}

/// Windows-safe file:// URL with minimal percent-encoding (space, #, ?, %).
fn path_to_file_url(path: &Path) -> String {
    let slashed = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file://");
    if !slashed.starts_with('/') {
        out.push('/');
    }
    for ch in slashed.chars() {
        match ch {
            ' ' => out.push_str("%20"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            '%' => out.push_str("%25"),
            _ => out.push(ch),
        }
    }
    out
}

/// import.meta initializer: url, filename, dirname (Node 20.11+ parity).
/// Builtin facades get url = "node:<name>" with no filename/dirname; CJS
/// facades get their real file path — createRequire(import.meta.url) works
/// everywhere user code can spell it. Zero capture, raw C ABI.
pub(crate) unsafe extern "C" fn import_meta_callback(
    context: v8::Local<v8::Context>,
    module: v8::Local<v8::Module>,
    meta: v8::Local<v8::Object>,
) {
    v8::callback_scope!(unsafe scope, context);
    // Module identity -> path, the same hash-then-compare dance as the
    // resolve callback (identity hashes are not unique).
    let candidates: Vec<(PathBuf, v8::Global<v8::Module>)> = {
        let Some(map) = scope.get_slot::<ModuleMap>() else {
            return;
        };
        let Some(paths) = map.paths_by_hash.get(&module.get_identity_hash()) else {
            return;
        };
        paths
            .iter()
            .filter_map(|p| map.by_path.get(p).map(|g| (p.clone(), g.clone())))
            .collect()
    };
    let Some(path) = candidates.into_iter().find_map(|(path, global)| {
        let candidate = v8::Local::new(scope, &global);
        (candidate == module).then_some(path)
    }) else {
        return;
    };

    let path_text = path.to_string_lossy().into_owned();
    let is_builtin = path_text.starts_with("node:") || path_text.starts_with("oam:");
    let url = if is_builtin {
        path_text.clone()
    } else {
        path_to_file_url(&path)
    };
    let entries: Vec<(&str, String)> = if is_builtin {
        vec![("url", url)]
    } else {
        let dirname = path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        vec![("url", url), ("filename", path_text), ("dirname", dirname)]
    };
    for (name, value) in entries {
        let Some(key) = v8::String::new(scope, name) else {
            continue;
        };
        let Some(value) = v8::String::new(scope, &value) else {
            continue;
        };
        meta.create_data_property(scope, key.into(), value.into());
    }
}

/// V8 promise-reject callback: maintains the RejectionLedger. Zero capture,
/// raw C ABI (set_promise_reject_callback takes the fn type directly).
pub(crate) unsafe extern "C" fn promise_reject_callback(message: v8::PromiseRejectMessage) {
    v8::callback_scope!(unsafe scope, &message);
    let promise = message.get_promise();
    match message.get_event() {
        v8::PromiseRejectEvent::PromiseRejectWithNoHandler => {
            let reason = message
                .get_value()
                .unwrap_or_else(|| v8::undefined(scope).into());
            let reason_global = v8::Global::new(scope, reason);
            let global = v8::Global::new(scope, promise);
            if let Some(ledger) = scope.get_slot_mut::<RejectionLedger>() {
                ledger.unhandled.push((global, reason_global));
            }
        }
        v8::PromiseRejectEvent::PromiseHandlerAddedAfterReject => {
            // A handler arrived late: un-flag the matching entry.
            let entries: Vec<(usize, v8::Global<v8::Promise>)> = scope
                .get_slot::<RejectionLedger>()
                .map(|ledger| {
                    ledger
                        .unhandled
                        .iter()
                        .enumerate()
                        .map(|(i, (g, _))| (i, g.clone()))
                        .collect()
                })
                .unwrap_or_default();
            let matched = entries.into_iter().find_map(|(i, global)| {
                let candidate = v8::Local::new(scope, &global);
                (candidate == promise).then_some(i)
            });
            if let Some(index) = matched
                && let Some(ledger) = scope.get_slot_mut::<RejectionLedger>()
            {
                ledger.unhandled.remove(index);
            }
        }
        _ => {}
    }
}

pub(crate) fn catch_to_diagnostic(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    name: &str,
) -> Diagnostic {
    let err = crate::exception_to_error(tc, name);
    Diagnostic::new(
        "OAM-RT0001",
        Severity::Error,
        Origin::Runtime,
        err.to_string(),
    )
}
