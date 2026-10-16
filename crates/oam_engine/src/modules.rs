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
use std::collections::{HashMap, HashSet, VecDeque};
use std::num::NonZeroI32;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use crate::JsRuntime;

/// What the engine needs from the embedder to load a module graph.
/// Errors are ODIF diagnostics — the engine never invents prose errors
/// for problems the host can describe precisely.
///
/// `Sync` because the graph loader overlaps module preparation: a small
/// worker pool calls `load` for paths the isolate thread has not popped yet
/// (see [`GraphPrefetcher`]), so `load` must tolerate concurrent calls for
/// DIFFERENT paths — the loader never loads one path twice concurrently.
/// `resolve` is only ever called from the isolate thread.
pub trait ModuleHost: Sync {
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
    /// Modules compiled this run whose bytecode-cache lookup missed (or whose
    /// blob V8 rejected), with the key to store under. Drained by
    /// `store_pending_code_cache` AFTER the graph evaluates, so the produced
    /// blob carries the inner functions evaluation compiled -- producing at
    /// compile time captured the top level only.
    pending_code_cache: Vec<(PathBuf, crate::code_cache::CacheKey)>,
    /// Default facades `require()` has built ([`default_facade`]), which
    /// numbers each one's map key.
    facades: usize,
    /// CommonJS dependencies of an ES graph whose body threw while a graph
    /// load ran them, with what the load reported. node runs such a module
    /// once, as part of the graph's evaluation, and the graph stays errored:
    /// every later `import()` or `require()` of it sees that same error. oam
    /// runs the module while the graph loads and drops it from the CommonJS
    /// cache when it throws (a direct `require()` of it runs it again, as
    /// node's does), so the graph load replays this record instead of
    /// running the body a second time.
    failed_cjs: HashMap<PathBuf, (v8::Global<v8::Value>, Diagnostic)>,
}

/// The module graph loads on the stack, innermost last. A load can nest: a
/// CommonJS dependency runs while its graph is loading (oam runs it at load
/// time, where node runs it while the graph evaluates), and a `require()` of
/// an ES module, or an `import()`, inside it loads a graph of its own. What
/// `require()` of an ES module needs from the outer loads is which of their
/// modules node would have in evaluation at that point
/// ([`evaluating_in_loads`]), so a load records its entry and the CommonJS
/// dependency it is running.
#[derive(Default)]
struct GraphLoads(Vec<GraphLoad>);

struct GraphLoad {
    entry: PathBuf,
    /// A graph a `require()` loads ([`require_esm`]): node's
    /// getModuleJobForRequire refuses an import in it of a module that is
    /// in evaluation, or of a CommonJS module whose body is running.
    for_require: bool,
    /// The CommonJS dependency whose body is running, if one is.
    executing_cjs: Option<PathBuf>,
    /// [`evaluating_in_loads`] as last computed while this load was the
    /// innermost one. The walk covers every outer load's graph, and a
    /// `require()` graph asks it once per import of an already-compiled
    /// module, so recomputing it each time costs edges x outer graph size.
    /// It stays valid while nothing it read changes: the outer loads are
    /// suspended, so only this load can change it, by compiling a module the
    /// walk visited (that adds its imports) or by running a CommonJS
    /// dependency (whose nested loads can evaluate modules). Both clear it.
    evaluating_memo: Option<EvaluatingInLoads>,
}

/// What [`evaluating_in_loads`] found: the modules node would have in
/// evaluation, and every module the walk read the imports or status of.
struct EvaluatingInLoads {
    evaluating: HashSet<PathBuf>,
    visited: HashSet<PathBuf>,
}

/// How many synchronous `require()`s of an ES module are running
/// ([`require_esm_sync`], from its load to the end of evaluation). node's
/// require() runs no ticks and no promise jobs before it returns, so no
/// load nested anywhere inside one -- an `import()` a CommonJS dependency or
/// the required module's body makes -- may drain them, whichever load is
/// innermost.
#[derive(Default)]
struct RequireDepth(usize);

/// Whether a synchronous `require()` of an ES module is on the stack.
fn inside_require(scope: &mut v8::PinScope<'_, '_>) -> bool {
    scope.get_slot::<RequireDepth>().is_some_and(|d| d.0 > 0)
}

/// The import a graph load could not resolve, kept beside the diagnostic so
/// `require()` can raise node's ERR_MODULE_NOT_FOUND for it, which names the
/// importer and the specifier.
#[derive(Default)]
struct UnresolvedDuringLoad(Option<(String, PathBuf)>);

/// The exception a failed graph load caught, as catch_to_diagnostic last
/// saw it. A diagnostic carries only text; `require()` of an ES module
/// rethrows the error itself -- a CommonJS dependency's throw, a syntax
/// error -- as node's does.
#[derive(Default)]
struct CaughtDuringLoad(Option<v8::Global<v8::Value>>);

/// The diagnostic code a load uses for node's ERR_REQUIRE_CYCLE_MODULE, which
/// [`require_esm`] turns back into that error.
const REQUIRE_CYCLE_CODE: &str = "ERR_REQUIRE_CYCLE_MODULE";

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
/// The ES module entry's evaluation promise while the event loop runs it: a
/// tick point stops at the microtask checkpoint that rejects it. node
/// reports an entry's throw from inside the drain that resumed it (a
/// top-level await a timer or an op settled), so the ticks its top level
/// queued before the throw never run (#345). Cleared once the entry has
/// settled, so the tick points after that run in full.
#[derive(Default)]
pub(crate) struct EntryEvaluation(Option<v8::Global<v8::Promise>>);

/// Whether the parked entry evaluation ([`EntryEvaluation`]) has rejected.
fn entry_rejected(scope: &mut v8::PinScope<'_, '_>) -> bool {
    let Some(entry) = scope
        .get_slot::<EntryEvaluation>()
        .and_then(|e| e.0.clone())
    else {
        return false;
    };
    v8::Local::new(scope, &entry).state() == v8::PromiseState::Rejected
}

/// Whether node would run the entry `path` as an ES module, which decides
/// when the ticks its top level queues run (#345): declared by extension or
/// package.json "type" ([`oam_loader::node_declared_module_kind`]), or, for a
/// typeless file, by node's own test of its syntax
/// ([`crate::cjs::contains_module_syntax`]). The source node tests is the
/// file as written -- for TypeScript, the code oam runs, which node's type
/// stripping matches but for an import whose every name is a type (node keeps
/// it as `import {} from ...`; oam drops it).
fn node_runs_as_esm(scope: &mut v8::PinScope<'_, '_>, host: &dyn ModuleHost, path: &Path) -> bool {
    if let Some(kind) = oam_loader::node_declared_module_kind(path) {
        return kind == oam_loader::ModuleKind::Esm;
    }
    // A typeless .js oam has already decided as node does.
    let key = module_key(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(kind) = crate::cjs::detected_kind(scope, &key) {
        return kind == oam_loader::ModuleKind::Esm;
    }
    let typescript = matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("ts" | "tsx" | "mts" | "cts")
    );
    let source = if typescript {
        host.load(path).ok()
    } else {
        std::fs::read_to_string(path).ok()
    };
    source.is_some_and(|source| {
        crate::cjs::contains_module_syntax(scope, &source, &path.to_string_lossy())
    })
}

/// Park (or, with None, clear) the entry evaluation for [`entry_rejected`].
fn park_entry_evaluation(
    scope: &mut v8::PinScope<'_, '_>,
    promise: Option<v8::Local<'_, v8::Promise>>,
) {
    let parked = promise.map(|p| v8::Global::new(scope, p));
    scope.set_slot(EntryEvaluation(parked));
}

#[derive(Default)]
pub(crate) struct UncaughtLedger {
    entries: Vec<UncaughtEntry>,
    /// Set by the queueMicrotask wrapper ([`dispatch_microtask_throw`]) just
    /// before its fatal throw reaches [`message_listener`].
    next_fatal: Option<FatalThrow>,
    /// The throw-site frame for a `next_fatal` that a listener threw.
    next_frame: Option<String>,
    /// The listener ended a checkpoint at a fatal throw
    /// ([`host_checkpoint`] lifts the termination).
    terminated: bool,
}

/// One throw V8 reported outside any TryCatch.
pub(crate) struct UncaughtEntry {
    value: v8::Global<v8::Value>,
    /// `file:line: message`, the fallback report text.
    text: String,
    /// Already dispatched by the queueMicrotask wrapper, and fatal: the
    /// drain reports it without running the ladder a second time.
    fatal: Option<FatalThrow>,
    /// The throw site's frame, for a listener's own throw: its report
    /// names the listener's line, as node's does.
    frame: Option<String>,
}

/// Why a microtask throw the wrapper already dispatched is fatal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FatalThrow {
    /// Nothing consumed it.
    NoConsumer,
    /// An 'uncaughtException' (or monitor) listener threw; node exits 7.
    ListenerThrew,
}

/// `natives.dispatchMicrotaskThrow(err)`, from the queueMicrotask wrapper's
/// catch: run the uncaught ladder on a microtask's throw. Ok(true): a
/// listener took it. Ok(false): nothing did; the wrapper rethrows `err`,
/// noted here as an already-dispatched fatal (#398). Err(thrown): a
/// listener threw; the caller throws that, noted the same way, with its
/// throw-site frame kept here -- by the time V8 reports a microtask's throw
/// it has rebuilt the Message from the error's stack, which points at
/// `new Error` rather than the `throw` node's report shows.
pub(crate) fn dispatch_microtask_throw(
    scope: &mut v8::PinScope<'_, '_>,
    err: v8::Local<'_, v8::Value>,
) -> Result<bool, v8::Global<v8::Value>> {
    v8::tc_scope!(let tc, scope);
    let (fatal, frame, outcome) = match dispatch_uncaught(tc, err) {
        Uncaught::Handled => return Ok(true),
        Uncaught::Fatal => (FatalThrow::NoConsumer, None, Ok(false)),
        Uncaught::HandlerThrew { value, frame, .. } => {
            (FatalThrow::ListenerThrew, frame, Err(value))
        }
    };
    if let Some(ledger) = tc.get_slot_mut::<UncaughtLedger>() {
        ledger.next_fatal = Some(fatal);
        ledger.next_frame = frame;
    }
    outcome
}

/// A microtask checkpoint the host runs with no JS on the stack. A fatal
/// microtask throw ends it where it happens: node exits at the throw, so no
/// later promise job of the same drain runs (#398). The message listener
/// terminates execution for that; this lifts the termination again, so the
/// report and the 'exit' listeners can run. The fatal entry stays on the
/// ledger for the caller's drain ([`drain_uncaught`]).
pub(crate) fn host_checkpoint(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) {
    tc.perform_microtask_checkpoint();
    lift_fatal_termination(tc);
}

/// Undo the termination [`message_listener`] requested for a fatal throw.
fn lift_fatal_termination(tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>) {
    let terminated = tc
        .get_slot_mut::<UncaughtLedger>()
        .is_some_and(|ledger| std::mem::take(&mut ledger.terminated));
    if terminated {
        tc.cancel_terminate_execution();
        tc.reset();
    }
}

/// Whether the ledger holds a fatal throw the wrapper already dispatched.
fn fatal_pending(scope: &mut v8::PinScope<'_, '_>) -> bool {
    scope
        .get_slot::<UncaughtLedger>()
        .is_some_and(|ledger| ledger.entries.iter().any(|entry| entry.fatal.is_some()))
}

/// V8 message listener. Zero capture, raw C ABI.
///
/// # Safety
/// Only V8 invokes this, as the isolate message listener, with the exact
/// C ABI signature; `message`/`exception` are V8 Locals valid for the call and
/// touched only under the callback scope opened below.
pub(crate) unsafe extern "C" fn message_listener(
    message: v8::Local<v8::Message>,
    exception: v8::Local<v8::Value>,
) {
    v8::callback_scope!(unsafe scope, message);
    let text = message.get(scope).to_rust_string_lossy(scope);
    let mut line = message.get_line_number(scope).unwrap_or(0);
    let file = message
        .get_script_resource_name(scope)
        .and_then(|name| name.to_string(scope))
        .map(|name| name.to_rust_string_lossy(scope))
        .unwrap_or_else(|| "<unknown>".to_string());
    // Transpiled files: remap the codegen line to the source line before
    // the position is frozen into the report string.
    if line > 0
        && let Some((src_line, _)) =
            oam_loader::sourcemap::lookup(&file, line as u32, message.get_start_column() as u32)
    {
        line = src_line as usize;
    }
    let value = v8::Global::new(scope, exception);
    let (fatal, frame) = scope
        .get_slot_mut::<UncaughtLedger>()
        .map(|ledger| (ledger.next_fatal.take(), ledger.next_frame.take()))
        .unwrap_or_default();
    // End the checkpoint at a fatal throw, when the host runs it with no JS
    // on the stack ([`host_checkpoint`] lifts the termination): the next
    // microtask stops at its first instruction and V8 drops the rest of the
    // queue. With JS on the stack (a checkpoint inside a dynamic import) the
    // termination would unwind that JS too, so the throw is only recorded
    // there and reported at the next drain. Not in a worker: node's worker
    // still runs the rest of the drain before it stops (conformance case
    // 474).
    let terminate = fatal.is_some()
        && scope
            .get_slot::<oam_core::worker::WorkerContext>()
            .is_none()
        && v8::StackTrace::current_stack_trace(scope, 1).is_none_or(|t| t.get_frame_count() == 0);
    if let Some(ledger) = scope.get_slot_mut::<UncaughtLedger>() {
        ledger.entries.push(UncaughtEntry {
            value,
            text: format!("{file}:{line}: {text}"),
            fatal,
            frame,
        });
        ledger.terminated |= terminate;
    }
    if terminate {
        scope.terminate_execution();
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
/// Called after each microtask checkpoint in the event loop. Returns
/// failures when the listener's queued ticks threw fatally -- the pump must
/// fail the run, not swallow them.
pub(crate) fn flush_handled_rejections(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> Option<Vec<Diagnostic>> {
    if !process_has_listener(tc, "unhandledRejection") {
        return None;
    }
    let drained: Vec<(v8::Global<v8::Promise>, v8::Global<v8::Value>)> = tc
        .get_slot_mut::<RejectionLedger>()
        .map(|ledger| std::mem::take(&mut ledger.unhandled))
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
    if ran_listener && let Some(failure) = run_ticks_and_microtasks(tc) {
        return Some(failure);
    }
    None
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
    dispatch_uncaught_from(tc, exception, "uncaughtException")
}

/// [`dispatch_uncaught`] with the `origin` the listeners see: node's
/// `'unhandledRejection'` for an ES module entry's own throw, which reaches
/// it as the rejection of the module's evaluation.
pub(crate) fn dispatch_uncaught_from(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    exception: v8::Local<v8::Value>,
    origin: &str,
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
    let Some(origin) = v8::String::new(tc, origin) else {
        return Uncaught::Fatal;
    };
    match dispatch.call(tc, recv, &[exception, origin.into()]) {
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
/// Takes the pin-erased scope: a TryCatch pin derefs to it, and every
/// Message method takes `&PinScope`.
fn source_frame(
    scope: &v8::PinScope<'_, '_>,
    message: v8::Local<'_, v8::Message>,
) -> Option<String> {
    let file = message
        .get_script_resource_name(scope)?
        .to_rust_string_lossy(scope);
    let line = message.get_line_number(scope)?;
    // Synthetic messages (eval wrappers) carry no usable location; Node
    // always has one, so print no frame at all rather than "undefined:0".
    if line == 0 || file.is_empty() || file == "undefined" {
        return None;
    }
    let src = message.get_source_line(scope)?.to_rust_string_lossy(scope);
    // Transpiled files: V8's position AND source line are codegen text.
    // With a source map registered, point at the line the user wrote --
    // position from the registry, text from the file on disk. Any failure
    // (no map, no mapping for this position, unreadable file) falls back
    // to the generated frame below, which is at least self-consistent.
    if let Some((src_line, src_col)) =
        oam_loader::sourcemap::lookup(&file, line as u32, message.get_start_column() as u32)
        && let Some(original) = read_source_line(&file, src_line)
    {
        let column = (src_col as usize).min(original.chars().count());
        let caret = format!("{}^", " ".repeat(column));
        return Some(format!("{file}:{src_line}\n{original}\n{caret}\n\n"));
    }
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

/// Line `line` (1-based) of `file` on disk, CR trimmed. The uncaught
/// report's code frame for a remapped position must show the SOURCE text
/// -- V8 only holds the codegen line.
fn read_source_line(file: &str, line: u32) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    text.lines()
        .nth(line.checked_sub(1)? as usize)
        .map(|l| l.trim_end_matches('\r').to_string())
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
    // A value that is not a native Error: node prints the source frame
    // itself, opened by an empty line and with the value right under the
    // caret, and a primitive as String() renders it (a Symbol, which has
    // no string conversion, as nothing). An object is still inspected.
    // Measured on node v22.22.2; conformance case 473.
    let (frame, body) = match frame {
        Some(frame) if !exception.is_native_error() => {
            let body = if exception.is_object() {
                body
            } else if exception.is_symbol() {
                String::new()
            } else {
                exception
                    .to_string(tc)
                    .map(|s| s.to_rust_string_lossy(tc))
                    .unwrap_or(body)
            };
            let frame = frame.strip_suffix('\n').unwrap_or(&frame);
            (Some(format!("\n{frame}")), body)
        }
        frame => (frame, body),
    };
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

/// Emit process `'exit'` once (`process._exiting` is the guard shared with
/// `process.exit()`), with `process.exitCode` or 0.
///
/// `natural`: the loop drained, or a top-level await never settled -- node's
/// `EmitProcessExit`. A listener's throw is an uncaught exception like any
/// other: offered to the 'uncaughtException' listeners, reported when
/// nothing takes it, and the code is the `process.exitCode` already set or
/// else 1 (7 when a listener of the ladder threw). Either way the emit ends
/// there -- later listeners, and the promise jobs the listeners queued, do
/// not run. Without a throw node drains those promise jobs, and a throw from
/// one of them goes the same way (#399 items 1 and 7), as does a promise
/// they reject and leave unhandled. The tick queue takes nothing once the
/// process is exiting, so there are no ticks to run.
///
/// Not natural: past a fatal exception, where node's `_fatalException`
/// emits `'exit'` inside a try/catch that drops a listener's throw -- the
/// original report stays the only one (#399 item 9) -- and runs nothing
/// after it.
///
/// A report raised here goes straight to stderr: node prints it at this
/// point of the exit.
pub(crate) fn emit_exit_event(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    natural: bool,
) {
    // A function expression compiled and called, so the locals stay function
    // scoped (a top-level `var` would land on globalThis) and the call's
    // exception is ours to read.
    const EMIT: &str = "(function () { \
        var p = globalThis.__oamProcessRef || globalThis.process; \
        if (!p || p._exiting) return; \
        p._exiting = true; \
        p.emit('exit', typeof p.exitCode === 'number' ? p.exitCode : 0); \
      })";
    let Some(emit) = compile_host_function(tc, "<process-exit>", EMIT) else {
        tc.reset();
        return;
    };
    let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
    if emit.call(tc, recv, &[]).is_some() {
        if natural {
            host_checkpoint(tc);
            if let Some(failures) = drain_uncaught(tc) {
                report_at_exit(tc, &failures);
                return;
            }
            // node's rejection processing runs after the listeners too: a
            // promise they reject and leave unhandled is fatal.
            if let Some(failures) = unhandled_rejection_failures(tc) {
                report_at_exit(tc, &failures);
            }
        }
        return;
    }
    if !natural {
        tc.reset();
        return;
    }
    let Some(exception) = tc.exception() else {
        tc.reset();
        return;
    };
    let value = v8::Global::new(tc, exception);
    let message = crate::exception_to_error(tc, "'exit' listener").to_string();
    let frame = tc.message().and_then(|m| source_frame(tc, m));
    tc.reset();
    let local = v8::Local::new(tc, &value);
    let (local, frame, message) = match dispatch_uncaught(tc, local) {
        Uncaught::Handled => return,
        Uncaught::Fatal => (local, frame, message),
        Uncaught::HandlerThrew {
            value,
            frame,
            message,
        } => (v8::Local::new(tc, &value), frame, message),
    };
    let report = Diagnostic::new(
        FATAL_CODE,
        Severity::Error,
        Origin::Runtime,
        fatal_report(tc, local, frame, &message),
    );
    report_at_exit(tc, &[report]);
}

/// Print fatal reports raised while exiting, and set the code node exits
/// with: 7 when a listener of the uncaught ladder threw
/// (kExceptionInFatalExceptionHandler), else `process.exitCode` when set,
/// else 1 -- node's `env->exit_code(kGenericUserError)`. Its
/// `_fatalException` does not force 1 once `process._exiting` is set.
fn report_at_exit(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    failures: &[Diagnostic],
) {
    // As the CLI renders them: a fatal report verbatim, anything else (an
    // unhandled rejection) as `error[CODE]: message`.
    for failure in failures {
        if failure.code == FATAL_CODE {
            eprint!("{}", failure.message);
        } else {
            eprintln!("error[{}]: {}", failure.code, failure.message);
        }
    }
    const CODE: &str = "(function () { \
        var p = globalThis.__oamProcessRef || globalThis.process; \
        if (!p) return; \
        if (p.__oamFatalCode === 7) p.exitCode = 7; \
        else if (p.exitCode == null) p.exitCode = 1; \
      })";
    if let Some(set_code) = compile_host_function(tc, "<exit-code>", CODE) {
        let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
        let _ = set_code.call(tc, recv, &[]);
    }
    tc.reset();
}

/// Compile `source` (a parenthesized function expression) as a classic
/// script and return the function it evaluates to.
fn compile_host_function<'s>(
    tc: &mut v8::PinnedRef<'s, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    name: &str,
    source: &str,
) -> Option<v8::Local<'s, v8::Function>> {
    let source = v8::String::new(tc, source)?;
    let name: v8::Local<v8::Value> = v8::String::new(tc, name)?.into();
    let origin = v8::ScriptOrigin::new(tc, name, 0, 0, false, 0, None, false, false, false, None);
    let script = v8::Script::compile(tc, source, Some(&origin))?;
    let value = script.run(tc)?;
    v8::Local::<v8::Function>::try_from(value).ok()
}

/// Take any uncaught-exception reports (from the message listener, i.e.
/// exceptions swallowed by the microtask checkpoint) and either deliver
/// them to an 'uncaughtException' listener or fail the run.
pub(crate) fn drain_uncaught(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> Option<Vec<Diagnostic>> {
    lift_fatal_termination(tc);
    let mut fatal = Vec::new();
    // Loop: a listener may queue microtasks/ticks (flushed below under the
    // explicit policy) whose throws land NEW ledger entries; quiesce before
    // returning. Terminates because each pass drains the ledger and only
    // fresh throws refill it.
    loop {
        let captured: Vec<UncaughtEntry> = tc
            .get_slot_mut::<UncaughtLedger>()
            .map(|ledger| std::mem::take(&mut ledger.entries))
            .unwrap_or_default();
        if captured.is_empty() {
            break;
        }
        for entry in captured {
            let UncaughtEntry {
                value,
                text,
                fatal: already,
                frame,
            } = entry;
            // A later entry than a fatal one is not reached: node exited.
            if !fatal.is_empty() {
                break;
            }
            let local = v8::Local::new(tc, &value);
            let outcome = match already {
                // The wrapper ran the ladder already; running it again would
                // fire 'uncaughtExceptionMonitor' twice.
                Some(FatalThrow::NoConsumer) => Uncaught::Fatal,
                Some(FatalThrow::ListenerThrew) => Uncaught::HandlerThrew {
                    value: value.clone(),
                    frame,
                    message: format!("Uncaught {text}"),
                },
                None => dispatch_uncaught(tc, local),
            };
            let (local, frame, message) = match outcome {
                Uncaught::Handled => continue,
                // No stored frame: for a microtask-thrown error V8 has no
                // throw-site Message at all (probe-verified -- file
                // "undefined", line 0, for a stack-stripped throw; node
                // prints `<anonymous_script>:0` the same way). The report's
                // create_message fallback derives the position from the
                // error's own stack, which is exactly node's derivation.
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
        // A fatal throw ends the run here: node exits at it, and nothing
        // queued since -- a tick, a promise job -- runs (#345 review).
        if !fatal.is_empty() {
            break;
        }
        // Flush what the listeners queued -- explicit policy means nothing
        // else will until the next macrotask boundary. A fatal tick throw
        // from that flush fails the run: merge its report and stop quiescing.
        if let Some(failure) = run_ticks_and_microtasks(tc) {
            fatal.extend(failure);
            break;
        }
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
///
/// On Windows the path is first resolved as node's loader resolves it
/// (`oam_core::FsPath`: lexically against the cwd, `\\?\`-prefixed), so
/// `app.js.` is the file named `app.js.` -- missing, keyed by that name and
/// failing its read -- not `app.js`, which `std::path::absolute`
/// (GetFullPathNameW) would make of it.
pub(crate) fn module_key(path: &Path) -> std::io::Result<PathBuf> {
    if path
        .to_str()
        .is_some_and(|s| s.starts_with("node:") || s.starts_with("oam:"))
    {
        return Ok(path.to_path_buf());
    }
    #[cfg(windows)]
    if let Some(given) = path.to_str().filter(|s| !s.is_empty()) {
        let os = oam_core::FsPath::new(given.to_string());
        return match std::fs::canonicalize(os.os()) {
            Ok(real) => Ok(strip_verbatim(real)),
            Err(_) => Ok(PathBuf::from(os.shown().into_owned())),
        };
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

/// OAM-TEST* failures from the test-runner bridge. Same shape as
/// [`rt_diag`], but the origin must match the code's namespace (`test`, not
/// `runtime`) -- agents route on `origin`.
fn test_diag(code: &str, message: impl Into<String>) -> Vec<Diagnostic> {
    vec![Diagnostic::new(
        code,
        Severity::Error,
        Origin::Test,
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
        // SAFETY: a lifetime-only transmute of a fat pointer -- only the erased
        // lifetime changes, the (data, vtable) halves keep identical layout, so
        // it is sound. The `'static` is never observed: per this fn's SAFETY
        // contract above, the slot is cleared to None before any later JS turn,
        // so no deref of this pointer outlives `host`.
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
        crate::attach_loop_waker(&mut self.isolate);
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

        if let Err(diags) = load_module_graph(tc, host, entry.clone(), false) {
            // Modules compiled before the failure still have unbound scripts;
            // cache them so the corrected run skips their compile.
            store_pending_code_cache(tc);
            return Err(diags);
        }

        let module = {
            let map = tc.get_slot::<ModuleMap>().expect("module map installed");
            let global = map.by_path.get(&entry).expect("entry preloaded").clone();
            v8::Local::new(tc, &global)
        };

        if module.instantiate_module(tc, resolve_module_callback) != Some(true) {
            let diag = catch_to_diagnostic(tc, &entry.to_string_lossy());
            store_pending_code_cache(tc);
            return Err(vec![diag]);
        }
        // Arm break-on-start only NOW — graph load just ran any CJS
        // dependency bodies; arming earlier would land the pause inside
        // node_modules instead of the entry's first line.
        if let Some(shared) = &wait {
            shared.arm_break();
        }
        // Bytecode is produced right after evaluate, on both arms -- see
        // store_pending_code_cache. On the throw arm the diagnostic is built
        // first so the TryCatch is read before any further V8 call.
        let Some(value) = module.evaluate(tc) else {
            let diag = catch_to_diagnostic(tc, &entry.to_string_lossy());
            store_pending_code_cache(tc);
            return Err(vec![diag]);
        };
        store_pending_code_cache(tc);
        let promise = v8::Local::<v8::Promise>::try_from(value)
            .map_err(|_| rt_diag("OAM-RT0001", "module evaluation did not return a promise"))?;
        let esm_in_node = node_runs_as_esm(tc, host, &entry);
        if esm_in_node {
            // node evaluates an ES module inside a microtask drain (its
            // loader's promise job), so the promise jobs the module's top
            // level queued -- and a top-level await they settle -- run
            // before the tick queue is reached: `process.nextTick` there
            // runs after them, not ahead (#345). A bare checkpoint first,
            // then the tick point.
            host_checkpoint(tc);
            // A promise job in that drain that threw: fatal, it ends the run
            // before any tick, as node exits at it; handled, it is delivered
            // before the ticks.
            if let Some(failure) = drain_uncaught(tc) {
                return Err(failure);
            }
        }
        // An entry node would run as CommonJS (a typeless file with no
        // module syntax) keeps CommonJS's order: its ticks first, at the tick
        // point below, and a synchronous throw reported at once, with
        // nothing queued run.

        // The entry's own throw -- at its top level, or after a top-level
        // await the drain settled -- is reported from inside node's drain,
        // so no tick runs before the uncaughtException ladder does.
        if promise.state() != v8::PromiseState::Rejected {
            park_entry_evaluation(tc, Some(promise));
            let ran = (|| {
                if let Some(failure) = run_ticks_and_microtasks(tc) {
                    return Err(failure);
                }
                if let Some(failure) = drain_uncaught(tc) {
                    return Err(failure);
                }
                pump_event_loop(tc, Some(promise), true)
            })();
            park_entry_evaluation(tc, None);
            ran?;
        }

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
                // throwing handler. node's origin for an ES module's is
                // 'unhandledRejection': its loader sees the evaluation's
                // rejection.
                let origin = if esm_in_node {
                    "unhandledRejection"
                } else {
                    "uncaughtException"
                };
                let (exception, frame) = match dispatch_uncaught_from(tc, exception, origin) {
                    Uncaught::Handled => {
                        // A listener suppressed the fatal exit: continue like
                        // a clean run -- drain ticks + microtasks the handler
                        // queued (delivering any fresh throws), pump any
                        // pending timers/ops, then report unhandled
                        // rejections. For an ES module the handler's
                        // promise jobs first: in node it runs inside the
                        // drain, ahead of the ticks.
                        if esm_in_node {
                            host_checkpoint(tc);
                            if let Some(failure) = drain_uncaught(tc) {
                                return Err(failure);
                            }
                        }
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
            let mut progressed = run_platform_tasks(tc)?;
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
                if let Some(queue) = tc.get_slot_mut::<crate::timers::TimerQueue>() {
                    queue.callback_done();
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
                match crate::ops::settle_completion(tc, completion) {
                    crate::ops::Settled::Fatal(failure) => return Err(failure),
                    crate::ops::Settled::ThrewHandled => {}
                    crate::ops::Settled::Ran => {
                        if let Some(failure) = run_ticks_and_microtasks(tc) {
                            return Err(failure);
                        }
                    }
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
                match crate::ops::settle_completion(tc, completion) {
                    crate::ops::Settled::Fatal(failure) => return Err(failure),
                    crate::ops::Settled::ThrewHandled => {}
                    crate::ops::Settled::Ran => {
                        if let Some(failure) = run_ticks_and_microtasks(tc) {
                            return Err(failure);
                        }
                    }
                }
            } else if next_timer.is_none_or(|t| t >= deadline) {
                return Ok(());
            }
        }
    }

    /// One REPL evaluation: TypeScript-stripped, await-aware, result
    /// rendered via util.inspect, `_` bound to the last value. Err is the
    /// pretty error text.
    ///
    /// `top_level_await` comes from the transpiler's parse (real top-level
    /// await usage -- never the identifier, never an await inside a nested
    /// function), and `hoisted_bindings` are the names the line declares at
    /// the top level. An awaiting line runs inside an async IIFE, which
    /// would swallow let/const/function bindings; the block wrapper
    /// re-publishes each one onto globalThis (Node's processTopLevelAwait
    /// hoists the same way), so the next line still sees them.
    pub fn repl_eval(
        &mut self,
        source: &str,
        top_level_await: bool,
        hoisted_bindings: &[String],
    ) -> Result<String, String> {
        // No host on the REPL path: clear any parked pointer (defensive; the
        // REPL never calls execute_module, but keep the invariant uniform).
        self.clear_active_host();
        let block_wrap = |source: &str| {
            let hoist: String = hoisted_bindings
                .iter()
                .map(|name| format!("\nglobalThis.{name} = {name};"))
                .collect();
            format!("(async () => {{ {source}\n{hoist} }})()")
        };
        let prepared = if top_level_await {
            if hoisted_bindings.is_empty() {
                // Top-level await: evaluate as an async IIFE and pump until
                // settled. Expression form first (trailing semicolons from
                // the TS strip would break it); statement fallback below.
                let expression = source.trim_end().trim_end_matches(';');
                format!("(async () => ({expression}\n))()")
            } else {
                // Declarations are never expressions: go straight to the
                // block form that re-publishes the bindings.
                block_wrap(source)
            }
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
            Err((syntax, _message)) if top_level_await && hoisted_bindings.is_empty() && syntax => {
                // Statement-shaped await input: re-wrap as a block. (With
                // hoisted bindings, `prepared` already was the block form.)
                run(self, &block_wrap(source)).map_err(|(_, m)| m)
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
            .ok_or_else(|| test_diag("OAM-TEST0003", "__oamTestRun missing from snapshot"))?;
        let runner = v8::Local::<v8::Function>::try_from(runner)
            .map_err(|_| test_diag("OAM-TEST0003", "__oamTestRun is not a function"))?;

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
            .map_err(|_| test_diag("OAM-TEST0003", "__oamTestRun did not return a promise"))?;

        pump_event_loop(tc, Some(promise), false)?;

        match promise.state() {
            v8::PromiseState::Fulfilled => {
                let value = promise.result(tc);
                let json = v8::json::stringify(tc, value)
                    .ok_or_else(|| test_diag("OAM-TEST0003", "test results failed to serialize"))?;
                Ok(json.to_rust_string_lossy(tc))
            }
            v8::PromiseState::Rejected => {
                let exception = promise.result(tc);
                let text = exception
                    .to_string(tc)
                    .map(|s| s.to_rust_string_lossy(tc))
                    .unwrap_or_else(|| "unknown exception".to_string());
                Err(test_diag(
                    "OAM-TEST0003",
                    format!("test run crashed outside any test: {text}"),
                ))
            }
            v8::PromiseState::Pending => Err(test_diag(
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
    // The entry already rejected: it is reported next, before anything else
    // runs ([`EntryEvaluation`]).
    if entry_rejected(tc) {
        return None;
    }
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
                // A fatal tick throw unwound natively from the drain (node's
                // try/finally shape -- no JS catch/rethrow), so the Message
                // still carries the USER's throw position; running the JS
                // ladder again here would reposition the report to
                // node_compat.js and double-fire 'uncaughtExceptionMonitor'
                // (#167 item 2). The ladder runs HERE, once, exactly like a
                // timer callback's throw (handle_uncaught_throw): handled
                // continues the run, fatal/handler-threw fails it. Handled
                // ALSO defers the interrupted batch -- node keeps the
                // remaining ticks queued and drains them at its next tick
                // point: after the next callback, or at its check phase
                // (pump_event_loop places that), so do not checkpoint-drain
                // them in place.
                if let Some(failure) = handle_uncaught_throw(tc, "process.nextTick") {
                    return Some(failure);
                }
                // Node's RunNextTicks skips its runMicrotasks() while the
                // interrupted batch stays queued -- the NEXT tick point
                // drains the batch and checkpoints after it. When the queue
                // is EMPTY, though, the do-while's runMicrotasks() is the
                // very next thing node does: checkpoint now so a
                // listener-queued microtask flushes instead of stranding
                // until some unrelated JS-running site checkpoints.
                if !has_pending_ticks(tc) {
                    host_checkpoint(tc);
                    if fatal_pending(tc) {
                        return drain_uncaught(tc);
                    }
                }
                return None;
            }
        }
        host_checkpoint(tc);
        // A fatal microtask throw ended the checkpoint: node exits at it, so
        // no tick queued since runs either (#398).
        if fatal_pending(tc) {
            return drain_uncaught(tc);
        }
        if !has_pending_ticks(tc) || entry_rejected(tc) {
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

/// Run every V8 foreground task that is ready (platform.rs), in posting
/// order, each followed by a tick + microtask drain -- node runs each inside
/// an InternalCallbackScope, which drains the same way when it closes.
/// Returns whether any ran. The common case is one slot read and one atomic
/// load.
pub(crate) fn run_platform_tasks(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> Result<bool, Vec<Diagnostic>> {
    let tasks = match tc.get_slot::<crate::platform::PlatformTasks>() {
        Some(slot) if slot.0.has_ready() => slot.0.take_ready(),
        _ => return Ok(false),
    };
    let ran = !tasks.is_empty();
    for task in tasks {
        task.run();
        if let Some(failure) = run_ticks_and_microtasks(tc) {
            return Err(failure);
        }
        if let Some(failure) = drain_uncaught(tc) {
            return Err(failure);
        }
        // This task's drain rejected the ES module entry: it is reported
        // next, and node runs nothing before that -- not the rest of this
        // batch, whose tasks (another async WebAssembly compile's
        // continuation) would run user code first (#399 item 11). The
        // tasks left are dropped with the run.
        if entry_rejected(tc) {
            break;
        }
    }
    Ok(ran)
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

/// The kind of callback the event loop ran last ([`pump_event_loop`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum LastRan {
    /// Nothing yet: the entry (or whatever ran before the loop).
    Entry,
    /// A timer, with the instant its timers phase began: the timers due by
    /// then run in that phase.
    Timer(std::time::Instant),
    /// An immediate, with the instant its check phase began.
    Immediate(std::time::Instant),
    /// An I/O callback, a V8 foreground task, or a pending callback.
    Other,
}

/// Run the ticks a handled throw left queued, as node's check phase does:
/// its callback scope's close runs the whole queue, delivering each handled
/// throw and going on with the rest (#366).
fn drain_deferred_ticks(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> Result<(), Vec<Diagnostic>> {
    while has_pending_ticks(tc) && !entry_rejected(tc) {
        if let Some(failure) = run_ticks_and_microtasks(tc) {
            return Err(failure);
        }
        if let Some(failure) = drain_uncaught(tc) {
            return Err(failure);
        }
    }
    Ok(())
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
    // What the loop ran last, for where a deferred tick batch drains.
    let mut last_ran = LastRan::Entry;
    loop {
        // One HandleScope per turn. Every Local a turn creates -- the
        // resolver a completion settles and the value it settles with, a
        // timer's callback and arguments, the hooks looked up by name --
        // must die with the turn. Created in the caller's scope instead,
        // they lived as long as the loop, so every settled op and every
        // timer stayed reachable for the life of the process: the JS heap
        // grew by about half a KiB per callback, on every server request.
        v8::scope!(let turn, &mut **tc);
        v8::tc_scope!(let tc, turn);
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
            if let Some(failures) = flush_handled_rejections(tc) {
                return Err(failures);
            }
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
        // Node runs a due timer only while SOMETHING keeps the loop alive.
        // When the only work left is unref'd timers the loop exits and they
        // never fire -- which for a 0ms timer (that is what setImmediate is
        // here) is always the interesting case, since it is due the instant
        // it is created. Draining unconditionally meant
        // `setImmediate(...).unref()` still ran at exit, so `-p` printed the
        // promise AND then executed work node would have dropped.
        let only_unref_timers_left = {
            let has_ref = tc
                .get_slot::<crate::timers::TimerQueue>()
                .is_some_and(|queue| queue.has_ref_timers());
            let has_inflight = tc
                .get_slot::<oam_core::CoreRuntime>()
                .is_some_and(|core| core.has_inflight());
            !has_ref && !has_inflight && !has_pending_ticks(tc)
        };
        // Ticks left queued by a handled throw run at node's next tick
        // point, which falls before this turn's callback when that callback
        // opens a phase node reaches only after its check phase (an
        // immediate of a new check phase, or a timer after an I/O callback:
        // the poll phase is followed by the check phase), or when it opens
        // a new timers phase (node runs the queue as a timers phase ends).
        // A timer due in the phase of the timer before it runs first: after
        // a throw node's timers phase goes on to its next due timer (#399
        // item 8).
        if !only_unref_timers_left && has_pending_ticks(tc) {
            let next = tc
                .get_slot_mut::<crate::timers::TimerQueue>()
                .and_then(|queue| queue.peek_due(now));
            let check_phase_first = match next {
                Some(crate::timers::Due::Immediate { queued }) => {
                    !matches!(last_ran, LastRan::Immediate(began) if queued <= began)
                }
                Some(crate::timers::Due::Timer { deadline }) => match last_ran {
                    LastRan::Other => true,
                    LastRan::Timer(began) => deadline > began,
                    LastRan::Entry | LastRan::Immediate(_) => false,
                },
                _ => false,
            };
            if check_phase_first {
                drain_deferred_ticks(tc)?;
            }
        }
        let due = if only_unref_timers_left {
            None
        } else {
            tc.get_slot_mut::<crate::timers::TimerQueue>()
                .and_then(|queue| queue.pop_due(now))
        };
        if let Some((callback, extra)) = due {
            last_ran = match tc
                .get_slot::<crate::timers::TimerQueue>()
                .and_then(|queue| queue.popped())
            {
                Some(crate::timers::Due::Timer { deadline }) => match last_ran {
                    LastRan::Timer(began) if deadline <= began => LastRan::Timer(began),
                    _ => LastRan::Timer(now),
                },
                Some(crate::timers::Due::Immediate { queued }) => LastRan::Immediate(queued),
                _ => LastRan::Other,
            };
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
            // A pending callback queued from here on (an op completion's
            // callback below) is in no timer's or immediate's phase.
            if let Some(queue) = tc.get_slot_mut::<crate::timers::TimerQueue>() {
                queue.callback_done();
            }
            progressed = true;
        }
        // The entry rejected in that callback's drain (a top-level await it
        // resumed, then a throw): node reports it from inside the drain, so
        // no other callback of this turn runs first (#345).
        if entry_promise.is_some_and(|promise| promise.state() == v8::PromiseState::Rejected) {
            break;
        }

        // A timer of the timers phase that just ran is still due: node
        // finishes that phase before its poll phase, so the V8 tasks and
        // the op completion below wait for the next turn -- their tick
        // drains would otherwise run a batch a handled throw left queued
        // ahead of that timer (#399 item 8). The phase is bounded: only
        // timers due when it began belong to it.
        if let LastRan::Timer(began) = last_ran
            && progressed
            && tc
                .get_slot_mut::<crate::timers::TimerQueue>()
                .and_then(|queue| queue.peek_due(now))
                .is_some_and(|next| {
                    matches!(next, crate::timers::Due::Timer { deadline } if deadline <= began)
                })
        {
            before_exit_emitted = false;
            continue;
        }

        // V8's foreground tasks (platform.rs) -- node runs them from its
        // loop's poll phase, each followed by a tick + microtask drain.
        if run_platform_tasks(tc)? {
            last_ran = LastRan::Other;
            progressed = true;
        }
        if entry_promise.is_some_and(|promise| promise.state() == v8::PromiseState::Rejected) {
            break;
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
            match crate::ops::settle_completion(tc, completion) {
                crate::ops::Settled::Fatal(failure) => return Err(failure),
                crate::ops::Settled::ThrewHandled => {}
                crate::ops::Settled::Ran => {
                    if let Some(failure) = run_ticks_and_microtasks(tc) {
                        return Err(failure);
                    }
                }
            }
            if let Some(failure) = drain_uncaught(tc) {
                return Err(failure);
            }
            last_ran = LastRan::Other;
            progressed = true;
        }
        if progressed {
            before_exit_emitted = false;
            continue;
        }
        // Nothing is due. Ticks a handled throw left queued in a callback
        // run at the check phase node passes through before it waits again
        // (#399 item 8: a 1 ms timer's batch drains before a 5 ms timer).
        // Not after the entry: node's first timers phase comes before that
        // check phase, and a timer the entry set is due in it by node's
        // loop clock -- draining here put "after" ahead of a 1 ms timer.
        if last_ran != LastRan::Entry && has_pending_ticks(tc) {
            drain_deferred_ticks(tc)?;
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
        // exiting with work queued (Node never does either). Only when
        // nothing else holds the loop open: a batch deferred by a HANDLED
        // throw drains at the NEXT tick point, which is the callback the
        // loop is about to wait for -- draining it here first would print
        // the remainder ahead of a pending timer (node probe: 1ms timer,
        // "done" then "after"; draining here raced the deadline and put
        // "after" first, flaking on the release legs).
        // The drain routes fatal throws to the UncaughtLedger; deliver them
        // NOW or the `continue` -> `break` path would exit 0 on a fatal.
        if has_pending_ticks(tc) && !has_ref_timers && !has_inflight {
            if let Some(failure) = run_ticks_and_microtasks(tc) {
                return Err(failure);
            }
            if let Some(failure) = drain_uncaught(tc) {
                return Err(failure);
            }
            continue;
        }
        if !has_ref_timers && !has_inflight && tc.has_pending_background_tasks() {
            // node's DrainTasks: background work V8 still owes this isolate a
            // foreground task for (an async WebAssembly compile) holds the
            // loop open until it posts, and the post wakes the op channel.
            // A post that lands before the wait is not lost (the wake sits
            // in the channel); the slice only bounds how long V8's answer
            // goes unasked again.
            const DRAIN_SLICE: std::time::Duration = std::time::Duration::from_millis(50);
            let completion = tc
                .get_slot_mut::<oam_core::CoreRuntime>()
                .and_then(|core| core.recv_deadline(Some(std::time::Instant::now() + DRAIN_SLICE)));
            if let Some(completion) = completion {
                let completion =
                    if let Some(state) = tc.get_slot_mut::<crate::replay::ReplayState>() {
                        crate::replay::intercept_completion(state, completion)
                    } else {
                        completion
                    };
                match crate::ops::settle_completion(tc, completion) {
                    crate::ops::Settled::Fatal(failure) => return Err(failure),
                    crate::ops::Settled::ThrewHandled => {}
                    crate::ops::Settled::Ran => {
                        if let Some(failure) = run_ticks_and_microtasks(tc) {
                            return Err(failure);
                        }
                    }
                }
                if let Some(failure) = drain_uncaught(tc) {
                    return Err(failure);
                }
            }
            continue;
        }
        // Past that, node: only ref'd timers and inflight ops keep the loop alive. When
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
                    match crate::ops::settle_completion(tc, completion) {
                        crate::ops::Settled::Fatal(failure) => return Err(failure),
                        crate::ops::Settled::ThrewHandled => {}
                        crate::ops::Settled::Ran => {
                            if let Some(failure) = run_ticks_and_microtasks(tc) {
                                return Err(failure);
                            }
                        }
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
                            match crate::ops::settle_completion(tc, completion) {
                                crate::ops::Settled::Fatal(failure) => return Err(failure),
                                crate::ops::Settled::ThrewHandled => {}
                                crate::ops::Settled::Ran => {
                                    if let Some(failure) = run_ticks_and_microtasks(tc) {
                                        return Err(failure);
                                    }
                                }
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
        .map(|ledger| std::mem::take(&mut ledger.unhandled))
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
    // A fatal tick throw from the flush fails the run: merge its report
    // with any rejection reports (all are fatal diagnostics).
    if let Some(failure) = run_ticks_and_microtasks(tc) {
        fatal.extend(failure);
    }
    if fatal.is_empty() { None } else { Some(fatal) }
}

/// Produce + store bytecode for every module compiled this run whose cache
/// lookup missed or was rejected (`ModuleMap::pending_code_cache`). Runs AFTER
/// the graph evaluates: V8 serializes only what has been compiled at produce
/// time, so a post-evaluate blob also carries the inner functions evaluation
/// compiled (class setup, registration code, IIFEs V8 did not compile
/// eagerly), where a compile-time blob held the top level only. Called on the
/// success AND failure arms of evaluate, and after a load or instantiate
/// failure: the unbound module script exists in every module status (an
/// Errored module holds its SharedFunctionInfo again), and a blob for a module
/// that threw is as valid as any other for that source text. Functions first
/// run after a top-level `await` resumes, or later in the event loop, are not
/// captured -- that is the residual lazy compile. Best-effort: a failed
/// produce/write just means the next run pays the compile. Drains the list,
/// so a repeat call is a no-op.
fn store_pending_code_cache(tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>) {
    let pending = match tc.get_slot_mut::<ModuleMap>() {
        Some(map) => std::mem::take(&mut map.pending_code_cache),
        None => return,
    };
    for (path, key) in pending {
        let Some(global) = tc
            .get_slot::<ModuleMap>()
            .and_then(|map| map.by_path.get(&path).cloned())
        else {
            continue;
        };
        let module = v8::Local::new(tc, &global);
        if let Some(cd) = module.get_unbound_module_script(tc).create_code_cache() {
            crate::code_cache::store(&key, &cd);
        }
    }
}

// ------------------------------------------------------------- prefetch

/// Graph-prefetch off switch: `OAM_GRAPH_PREFETCH=0|off|false|no` falls back
/// to the strictly serial load path (one inline `host.load` per pop). On by
/// default. Read once; the env is fixed for a process lifetime.
fn prefetch_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        !matches!(
            std::env::var("OAM_GRAPH_PREFETCH").as_deref(),
            Ok("0") | Ok("off") | Ok("false") | Ok("no")
        )
    })
}

/// One off-thread module preparation: everything the graph loop needs for a
/// path BEFORE V8 gets involved. Workers produce these; the loop consumes
/// them at pop time in place of the inline `host.load` + cache probe.
enum Prefetched {
    /// `host.load` succeeded; the bytecode-cache probe for that exact source
    /// rides along (the same gated `key_for` + `load` calls the inline path
    /// makes -- `key` is None when the cache is off, so `OAM_CODE_CACHE=0`
    /// skips the SHA-256 keying identically on both paths).
    Loaded {
        code: String,
        key: Option<crate::code_cache::CacheKey>,
        cached: Option<crate::code_cache::Loaded>,
    },
    /// `host.load` failed. Parked until the path is popped so the error is
    /// reported exactly where and when the serial loop would have reported
    /// it — an error for a path that is never popped (an earlier pop failed
    /// first) is dropped, matching serial behavior.
    Failed(Vec<Diagnostic>),
    /// The host panicked on the worker. Re-raised on the isolate thread at
    /// pop time, where the inline call would have panicked.
    Panicked(Box<dyn std::any::Any + Send>),
}

/// State shared between the graph loop and its prefetch workers.
#[derive(Default)]
struct PrefetchShared {
    state: Mutex<PrefetchState>,
    /// Wakes workers: a job was queued, or the loop is done (`closed`).
    work: Condvar,
    /// Wakes the graph loop: a result was parked.
    done: Condvar,
}

#[derive(Default)]
struct PrefetchState {
    queue: VecDeque<PathBuf>,
    results: HashMap<PathBuf, Prefetched>,
    closed: bool,
}

/// Lock that shrugs off poison: workers catch host panics before parking, so
/// a poisoned mutex can only mean a panic in this file's own bookkeeping —
/// the state is still coherent enough to shut down through, and refusing the
/// lock would just trade one panic for a deadlocked scope join.
fn prefetch_lock(shared: &PrefetchShared) -> std::sync::MutexGuard<'_, PrefetchState> {
    shared.state.lock().unwrap_or_else(|p| p.into_inner())
}

/// Worker loop: pull a path, run `host.load` + the bytecode-cache probe,
/// park the result. No V8 handle is touched here — `host.load` is fs + oxc,
/// and the cache probe is SHA-256 plus a disk/seed lookup.
fn prefetch_worker(host: &dyn ModuleHost, shared: &PrefetchShared) {
    loop {
        let path = {
            let mut state = prefetch_lock(shared);
            loop {
                if let Some(path) = state.queue.pop_front() {
                    break path;
                }
                if state.closed {
                    return;
                }
                state = shared.work.wait(state).unwrap_or_else(|p| p.into_inner());
            }
        };
        // AssertUnwindSafe: the payload is re-raised at the consuming pop,
        // so nothing observes state the panic may have torn.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            host.load(&path).map(|code| {
                let key = crate::code_cache::enabled()
                    .then(|| crate::code_cache::key_for(&code, crate::code_cache::Kind::Module));
                let cached = key.as_ref().and_then(crate::code_cache::load);
                (code, key, cached)
            })
        }));
        let result = match outcome {
            Ok(Ok((code, key, cached))) => Prefetched::Loaded { code, key, cached },
            Ok(Err(diags)) => Prefetched::Failed(diags),
            Err(payload) => Prefetched::Panicked(payload),
        };
        prefetch_lock(shared).results.insert(path, result);
        shared.done.notify_all();
    }
}

/// Overlapped module preparation for [`load_module_graph`]: discovered import
/// paths are handed to a small worker pool that runs `host.load` (fs read +
/// transpile — thread-safe, no V8 handles) and the bytecode-cache probe ahead
/// of the loop, parking results the loop consumes at pop time. Cold-run cost
/// then tracks graph depth rather than module count. Scoped to ONE graph
/// load: workers spawn lazily (a graph with no prefetchable imports spawns
/// none) and exit when the loop finishes — Drop closes the queue,
/// `std::thread::scope` joins.
struct GraphPrefetcher<'scope, 'env> {
    scope: &'scope std::thread::Scope<'scope, 'env>,
    host: &'env dyn ModuleHost,
    shared: Arc<PrefetchShared>,
    /// Paths handed to the pool and not yet consumed: the dedupe set (a
    /// diamond import is loaded once) and the `take` guard (never block on a
    /// path that was not enqueued).
    requested: HashSet<PathBuf>,
    workers: usize,
    max_workers: usize,
}

impl<'scope, 'env> GraphPrefetcher<'scope, 'env> {
    fn new(scope: &'scope std::thread::Scope<'scope, 'env>, host: &'env dyn ModuleHost) -> Self {
        // Small on purpose: transpile jobs are short and the isolate thread
        // consumes serially; more threads would just contend.
        let max_workers = if prefetch_enabled() {
            std::thread::available_parallelism()
                .map(std::num::NonZeroUsize::get)
                .unwrap_or(1)
                .min(4)
        } else {
            0
        };
        Self {
            scope,
            host,
            shared: Arc::default(),
            requested: HashSet::new(),
            workers: 0,
            max_workers,
        }
    }

    /// Whether the pop-time branch for `path` is the `host.load` one — the
    /// only branch prefetch may front-run. Mirrors the loop's own branch
    /// conditions: virtual builtins, JSON modules and CJS interop all need
    /// the isolate and load inline.
    fn eligible(path: &Path) -> bool {
        if path
            .to_str()
            .is_some_and(|s| s.starts_with("node:") || s.starts_with("oam:"))
        {
            return false;
        }
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            return false;
        }
        // A file node decides by its syntax is only known to be an ES module
        // once the isolate has tested it, so it is front-run on a guess: a
        // typeless project file an ES module imports is usually one, a
        // typeless file under node_modules usually CommonJS. A guess that
        // loses costs a read nobody takes (the CommonJS branch loads it).
        oam_loader::module_kind(path) != oam_loader::ModuleKind::Cjs
            || (oam_loader::kind_from_syntax(path)
                && !path.components().any(|c| c.as_os_str() == "node_modules"))
    }

    /// Hand `path` to the pool if it is eligible and not already requested.
    fn enqueue(&mut self, path: &Path) {
        if self.max_workers == 0 || self.requested.contains(path) || !Self::eligible(path) {
            return;
        }
        self.requested.insert(path.to_path_buf());
        prefetch_lock(&self.shared)
            .queue
            .push_back(path.to_path_buf());
        self.shared.work.notify_one();
        if self.workers < self.max_workers {
            self.workers += 1;
            let host = self.host;
            let shared = Arc::clone(&self.shared);
            self.scope.spawn(move || prefetch_worker(host, &shared));
        }
    }

    /// The parked result for `path`, blocking until its worker finishes.
    /// `None` when the path was never enqueued (the caller loads inline —
    /// the entry, and anything `eligible` declined). Every requested path is
    /// eventually parked: at least one worker exists once anything is
    /// requested, and workers park every job they take, panics included.
    fn take(&mut self, path: &Path) -> Option<Prefetched> {
        if !self.requested.remove(path) {
            return None;
        }
        let mut state = prefetch_lock(&self.shared);
        loop {
            if let Some(result) = state.results.remove(path) {
                return Some(result);
            }
            state = self
                .shared
                .done
                .wait(state)
                .unwrap_or_else(|p| p.into_inner());
        }
    }
}

impl Drop for GraphPrefetcher<'_, '_> {
    fn drop(&mut self) {
        let mut state = prefetch_lock(&self.shared);
        state.closed = true;
        // Still-queued paths will never be consumed (early error return);
        // drop them so workers exit instead of loading modules nobody pops.
        state.queue.clear();
        drop(state);
        self.shared.work.notify_all();
    }
}

/// Compile the module graph into the isolate's ModuleMap. Explicit worklist —
/// recursion would overflow the stack on deep (generated-code) import chains.
/// Discovery is breadth-first in import-declaration order (FIFO): CJS
/// modules execute eagerly at load, so discovery order is their observable
/// side-effect order — declaration order is what Node-shaped code expects.
/// What IS overlapped: `host.load` (fs read + transpile) and the bytecode-
/// cache probe for discovered-but-not-yet-popped paths run on a small worker
/// pool (see [`GraphPrefetcher`]); everything V8 stays on this thread.
fn load_module_graph(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    host: &dyn ModuleHost,
    entry: PathBuf,
    for_require: bool,
) -> Result<(), Vec<Diagnostic>> {
    if tc.get_slot::<GraphLoads>().is_none() {
        tc.set_slot(GraphLoads::default());
    }
    tc.get_slot_mut::<GraphLoads>()
        .expect("graph loads installed")
        .0
        .push(GraphLoad {
            entry: entry.clone(),
            for_require,
            executing_cjs: None,
            evaluating_memo: None,
        });
    // The prefetch pool borrows `host`, so it lives inside a thread scope:
    // workers spawn lazily as prefetchable imports are discovered, and the
    // scope joins them on every exit path (the prefetcher's Drop closes the
    // queue, error returns included).
    let loaded = std::thread::scope(|scope| {
        let mut prefetch = GraphPrefetcher::new(scope, host);
        load_module_graph_overlapped(tc, host, entry, &mut prefetch)
    });
    if let Some(loads) = tc.get_slot_mut::<GraphLoads>() {
        loads.0.pop();
    }
    loaded
}

/// Set (or, with None, clear) the CommonJS dependency the innermost graph
/// load is running.
fn set_executing_cjs(scope: &mut v8::PinScope<'_, '_>, path: Option<PathBuf>) {
    if let Some(load) = scope
        .get_slot_mut::<GraphLoads>()
        .and_then(|loads| loads.0.last_mut())
    {
        load.executing_cjs = path;
    }
}

/// Whether the innermost graph load is one a `require()` started.
fn loading_for_require(scope: &mut v8::PinScope<'_, '_>) -> bool {
    scope
        .get_slot::<GraphLoads>()
        .and_then(|loads| loads.0.last())
        .is_some_and(|load| load.for_require)
}

/// The paths `path`'s imports resolved to, in source order, as the load that
/// compiled it recorded them. A path is absent when its edge was never
/// recorded (the load failed before reaching it).
fn recorded_imports(
    scope: &mut v8::PinScope<'_, '_>,
    path: &Path,
    module: v8::Local<'_, v8::Module>,
) -> Vec<(String, PathBuf)> {
    let requests = module.get_module_requests();
    let mut specifiers = Vec::with_capacity(requests.length());
    for i in 0..requests.length() {
        let Some(request) = requests
            .get(scope, i)
            .and_then(|r| v8::Local::<v8::ModuleRequest>::try_from(r).ok())
        else {
            continue;
        };
        specifiers.push(request.get_specifier().to_rust_string_lossy(scope));
    }
    let Some(map) = scope.get_slot::<ModuleMap>() else {
        return Vec::new();
    };
    specifiers
        .into_iter()
        .filter_map(|specifier| {
            let resolved = map
                .edges
                .get(&(path.to_path_buf(), specifier.clone()))?
                .clone();
            Some((specifier, resolved))
        })
        .collect()
}

/// The modules node would have in evaluation while each graph load on the
/// stack runs its CommonJS dependency: those V8 reports as evaluating, and,
/// for a load running one, the modules node's depth-first evaluation of that
/// load's graph has entered and not left when it reaches the dependency.
///
/// node runs a CommonJS dependency while the graph evaluates: V8 walks the
/// imports depth first, in source order, so at that point the modules on the
/// walk's path from the entry are evaluating, and so are the ones it already
/// finished that sit in a cycle with that path (a strongly connected
/// component leaves evaluation as a whole). oam runs the dependency while the
/// graph loads, before anything evaluates, so the walk is replayed here over
/// the imports the load has recorded so far. An import of a module the load
/// has not reached yet is a leaf to it, which is where the two can part: when
/// that module would itself have led to the dependency (a sibling import
/// discovered later), node has it evaluating and this does not.
fn evaluating_in_loads(scope: &mut v8::PinScope<'_, '_>) -> EvaluatingInLoads {
    let loads: Vec<(PathBuf, PathBuf)> = scope
        .get_slot::<GraphLoads>()
        .map(|loads| {
            loads
                .0
                .iter()
                .filter_map(|l| l.executing_cjs.clone().map(|c| (l.entry.clone(), c)))
                .collect()
        })
        .unwrap_or_default();
    // The imports of a module the walk would descend into: one compiled and
    // not yet evaluated (an evaluated or errored module is not re-entered).
    fn imports_of(scope: &mut v8::PinScope<'_, '_>, path: &Path) -> Vec<PathBuf> {
        let global = scope
            .get_slot::<ModuleMap>()
            .and_then(|map| map.by_path.get(path).cloned());
        let Some(global) = global else {
            return Vec::new();
        };
        let module = v8::Local::new(scope, &global);
        if matches!(
            module.get_status(),
            v8::ModuleStatus::Evaluated | v8::ModuleStatus::Errored
        ) {
            return Vec::new();
        }
        recorded_imports(scope, path, module)
            .into_iter()
            .map(|(_, resolved)| resolved)
            .collect()
    }
    let mut evaluating = HashSet::new();
    let mut all_visited = HashSet::new();
    for (entry, running) in loads {
        // Iterative depth-first walk; `on_path` is the walk's current path.
        let mut visited: HashSet<PathBuf> = HashSet::from([entry.clone()]);
        let mut children: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        let first = imports_of(scope, &entry);
        children.insert(entry.clone(), first.clone());
        let mut stack: Vec<(PathBuf, std::vec::IntoIter<PathBuf>)> =
            vec![(entry.clone(), first.into_iter())];
        let mut on_path: Option<Vec<PathBuf>> = None;
        if entry == running {
            on_path = Some(Vec::new());
        }
        while on_path.is_none() {
            let Some((_, next)) = stack.last_mut() else {
                break;
            };
            match next.next() {
                Some(child) => {
                    if child == running {
                        on_path = Some(stack.iter().map(|(p, _)| p.clone()).collect());
                    } else if visited.insert(child.clone()) {
                        let grandchildren = imports_of(scope, &child);
                        children.insert(child.clone(), grandchildren.clone());
                        stack.push((child, grandchildren.into_iter()));
                    }
                }
                None => {
                    stack.pop();
                }
            }
        }
        all_visited.extend(visited);
        let Some(on_path) = on_path else {
            continue;
        };
        // The finished modules that reach the path stay in evaluation with
        // it: everything that reaches the path over the recorded imports,
        // found by one breadth-first walk of the reversed edges.
        let mut importers: HashMap<&Path, Vec<&Path>> = HashMap::new();
        for (module, imports) in &children {
            for import in imports {
                importers
                    .entry(import.as_path())
                    .or_default()
                    .push(module.as_path());
            }
        }
        let mut held: HashSet<PathBuf> = on_path.iter().cloned().collect();
        let mut queue: VecDeque<PathBuf> = on_path.into_iter().collect();
        while let Some(module) = queue.pop_front() {
            for importer in importers.get(module.as_path()).into_iter().flatten() {
                if held.insert(importer.to_path_buf()) {
                    queue.push_back(importer.to_path_buf());
                }
            }
        }
        evaluating.extend(held);
    }
    EvaluatingInLoads {
        evaluating,
        visited: all_visited,
    }
}

/// Whether `path` is in [`evaluating_in_loads`], answered from the innermost
/// load's memo when it has one (see [`GraphLoad::evaluating_memo`]). For the
/// checks a load makes of its own imports ([`check_required_import`]), while
/// it is the innermost load and so the only one that can change the answer.
fn in_evaluation_by_loads(scope: &mut v8::PinScope<'_, '_>, path: &Path) -> bool {
    if let Some(memo) = scope
        .get_slot::<GraphLoads>()
        .and_then(|loads| loads.0.last())
        .and_then(|load| load.evaluating_memo.as_ref())
    {
        return memo.evaluating.contains(path);
    }
    let found = evaluating_in_loads(scope);
    let answer = found.evaluating.contains(path);
    if let Some(load) = scope
        .get_slot_mut::<GraphLoads>()
        .and_then(|loads| loads.0.last_mut())
    {
        load.evaluating_memo = Some(found);
    }
    answer
}

/// Drop the innermost load's [`evaluating_in_loads`] memo -- all of it, or,
/// with `compiled`, only when the walk read that module (whose imports a
/// compile has just recorded).
fn invalidate_evaluating_memo(scope: &mut v8::PinScope<'_, '_>, compiled: Option<&Path>) {
    if let Some(load) = scope
        .get_slot_mut::<GraphLoads>()
        .and_then(|loads| loads.0.last_mut())
    {
        let stale = match (&load.evaluating_memo, compiled) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(memo), Some(path)) => memo.visited.contains(path),
        };
        if stale {
            load.evaluating_memo = None;
        }
    }
}

/// node's refusals for an import inside a graph a `require()` loads
/// (getModuleJobForRequire): of a CommonJS module whose body is running, and
/// of an ES module in evaluation. `importer` imports `specifier`, which
/// resolved to `resolved`. Both are ERR_REQUIRE_CYCLE_MODULE, worded with the
/// specifier as written and the importer's path.
fn check_required_import(
    scope: &mut v8::PinScope<'_, '_>,
    importer: &Path,
    specifier: &str,
    resolved: &Path,
) -> Result<(), Vec<Diagnostic>> {
    if !loading_for_require(scope) {
        return Ok(());
    }
    if crate::cjs::is_executing(scope, resolved) {
        return Err(rt_diag(
            REQUIRE_CYCLE_CODE,
            format!(
                "Cannot import CommonJS Module {specifier} in a cycle. (from {})",
                importer.display()
            ),
        ));
    }
    let global = scope
        .get_slot::<ModuleMap>()
        .and_then(|map| map.by_path.get(resolved).cloned());
    let Some(global) = global else {
        return Ok(());
    };
    let status = v8::Local::new(scope, &global).get_status();
    let in_evaluation = match status {
        v8::ModuleStatus::Evaluating => true,
        v8::ModuleStatus::Evaluated | v8::ModuleStatus::Errored => false,
        _ => in_evaluation_by_loads(scope, resolved),
    };
    if in_evaluation {
        return Err(rt_diag(
            REQUIRE_CYCLE_CODE,
            format!(
                "Cannot import Module {specifier} in a cycle. (from {})",
                importer.display()
            ),
        ));
    }
    Ok(())
}

/// The worklist loop behind [`load_module_graph`], with `prefetch` running
/// `host.load` + the bytecode-cache probe ahead of the pops. V8 compile /
/// instantiate stays here, on the isolate thread.
fn load_module_graph_overlapped(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    host: &dyn ModuleHost,
    entry: PathBuf,
    prefetch: &mut GraphPrefetcher<'_, '_>,
) -> Result<(), Vec<Diagnostic>> {
    let mut work = VecDeque::from([entry]);
    let mut seen: HashSet<PathBuf> = HashSet::new();
    while let Some(path) = work.pop_front() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let present = tc
            .get_slot::<ModuleMap>()
            .expect("module map installed")
            .by_path
            .get(&path)
            .cloned();
        if let Some(present) = present {
            // Compiled by an earlier load, whose recorded imports stand. One
            // not yet instantiated may belong to a load still on the stack --
            // a `require()` or `import()` from a CommonJS dependency it is
            // running -- whose worklist still holds that module's imports
            // unloaded; this load needs them now, so it walks on through the
            // recorded edges. (An instantiated module's graph is complete.)
            let module = v8::Local::new(tc, &present);
            if module.get_status() == v8::ModuleStatus::Uninstantiated {
                let recorded = recorded_imports(tc, &path, module);
                if recorded.len() < module.get_module_requests().length() {
                    // An import with no edge: the load that compiled the
                    // module failed on it (a specifier that did not
                    // resolve). Resolve them all again, so this load fails
                    // the same way -- or, if the file has appeared since,
                    // goes on -- instead of reaching instantiation with an
                    // edge the resolve callback cannot find.
                    resolve_imports(tc, host, &path, module, prefetch, &mut work)?;
                    continue;
                }
                for (specifier, resolved) in recorded {
                    check_required_import(tc, &path, &specifier, &resolved)?;
                    if !tc
                        .get_slot::<ModuleMap>()
                        .expect("module map installed")
                        .by_path
                        .contains_key(&resolved)
                    {
                        prefetch.enqueue(&resolved);
                    }
                    work.push_back(resolved);
                }
            }
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
        // Set when the pool loaded this path: the worker's cache probe
        // (key + blob) rides along so nothing is hashed or read twice.
        let mut prefetched = None;
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
        } else if oam_loader::module_kind(&path) == oam_loader::ModuleKind::Cjs
            // A typeless file inside node_modules, which oam takes for
            // CommonJS, that a `require()` is loading as an ES module: the
            // require() already compiled it as CommonJS and found module
            // syntax (node's detection), so it is not run as CommonJS here --
            // that would answer from the placeholder `require.cache` entry
            // the require() made and build a facade around its empty
            // exports.
            && !crate::cjs::is_requiring_esm(tc, &path)
        {
            if let Some((value, diagnostic)) = tc
                .get_slot::<ModuleMap>()
                .and_then(|map| map.failed_cjs.get(&path))
                .map(|(value, diagnostic)| (value.clone(), diagnostic.clone()))
            {
                // Its body threw in an earlier load: that error again, the
                // body not run again (see ModuleMap::failed_cjs).
                tc.set_slot(CaughtDuringLoad(Some(value)));
                return Err(vec![diagnostic]);
            }
            // Recorded for the duration of the body, so a `require()` of an
            // ES module from it can tell which of this graph's modules node
            // would have in evaluation (evaluating_in_loads).
            set_executing_cjs(tc, Some(path.clone()));
            let loaded = crate::cjs::load_cjs_or_detect(tc, &path);
            set_executing_cjs(tc, None);
            // The body may have loaded and evaluated modules (a require() or
            // import() of its own), which the memo's walk read.
            invalidate_evaluating_memo(tc, None);
            match loaded {
                crate::cjs::CjsLoad::Exports(exports) => {
                    // A graph a `require()` loads runs inside the requiring
                    // statement, and node's require never runs ticks or
                    // promise jobs before it returns -- nor does a load
                    // nested inside one (an import() from a CommonJS
                    // dependency of the required graph). Only a top-level or
                    // import() load outside any require() drains here.
                    if !inside_require(tc) {
                        if let Some(failure) = run_ticks_and_microtasks(tc) {
                            return Err(failure);
                        }
                        if let Some(failure) = drain_uncaught(tc) {
                            return Err(failure);
                        }
                    }
                    crate::cjs::facade_source(tc, &path, exports)
                }
                // node decides the file by its syntax and it has module
                // syntax: an ES module, not run. node's
                // MODULE_TYPELESS_PACKAGE_JSON warning goes out, then it
                // loads as the module it is.
                crate::cjs::CjsLoad::Esm => {
                    let wkey = module_key(&path).unwrap_or_else(|_| path.clone());
                    crate::cjs::warn_typeless_package_json(tc, &wkey);
                    host.load(&path)?
                }
                crate::cjs::CjsLoad::Threw => {
                    let caught = tc.exception().map(|value| v8::Global::new(tc, value));
                    let diagnostic = catch_to_diagnostic(tc, &path.to_string_lossy());
                    if let Some(value) = caught
                        && !tc.has_terminated()
                    {
                        tc.get_slot_mut::<ModuleMap>()
                            .expect("module map installed")
                            .failed_cjs
                            .insert(path.clone(), (value, diagnostic.clone()));
                    }
                    return Err(vec![diagnostic]);
                }
            }
        } else {
            match prefetch.take(&path) {
                // A parked Err surfaces HERE — when the path is popped, not
                // when the worker hit it — so error content and ordering are
                // byte-identical to the inline call below.
                Some(Prefetched::Loaded { code, key, cached }) => {
                    prefetched = Some((key, cached));
                    code
                }
                Some(Prefetched::Failed(diags)) => return Err(diags),
                Some(Prefetched::Panicked(payload)) => std::panic::resume_unwind(payload),
                None => host.load(&path)?,
            }
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
        // fresh. Keyed by source + V8 version + oam format, so a foreign blob
        // is never deserialized here; V8's rejected() guards a stale one. The
        // key is computed once and kept for the post-evaluate store. `cached`
        // is held across the compile because CachedData borrows it
        // (BufferNotOwned). Applies to every module source -- real ESM files
        // AND the generated builtin/CJS/JSON facades, all of which are
        // deterministic for a given source string (a different facade is a
        // different key, hence a miss, never a wrong hit).
        let (cache_key, cached) = match prefetched {
            // The pool already derived the key and probed the cache for this
            // exact source; reuse both.
            Some((key, cached)) => (key, cached),
            None => {
                let key = crate::code_cache::enabled()
                    .then(|| crate::code_cache::key_for(&code, crate::code_cache::Kind::Module));
                let cached = key.as_ref().and_then(crate::code_cache::load);
                (key, cached)
            }
        };
        let consuming = cached.is_some();
        let seeded = cached.as_ref().is_some_and(|c| c.seeded);
        let mut source = match cached {
            Some(ref c) => v8::script_compiler::Source::new_with_cached_data(
                source_str,
                Some(&origin),
                v8::script_compiler::CachedData::new(&c.bytes),
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

        // On a miss (or if V8 rejected a stale blob) remember the key: the
        // blob is produced AFTER the graph evaluates (store_pending_code_cache)
        // so it carries bytecode for the functions evaluation compiled, not
        // just the top level -- the same produce-after-run shape as the CJS
        // site. Nothing is serialized here. A rejected SEED is dropped AND
        // recorded on disk (tombstone) so neither a later load in this
        // process nor a later run of the same binary is handed the rejected
        // bytes again. A None key means the cache is off (OAM_CODE_CACHE=0),
        // which leaves no residual bookkeeping.
        let rejected = consuming
            && source
                .get_cached_data()
                .map(|cd| cd.rejected())
                .unwrap_or(true);
        if rejected
            && seeded
            && let Some(cache_key) = &cache_key
        {
            crate::code_cache::record_seed_rejection(cache_key);
        }
        let refresh_cache = !consuming || rejected;

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
            if refresh_cache && let Some(cache_key) = cache_key {
                map.pending_code_cache.push((path.clone(), cache_key));
            }
        }

        invalidate_evaluating_memo(tc, Some(&path));
        resolve_imports(tc, host, &path, module, prefetch, &mut work)?;
    }
    Ok(())
}

/// Resolve every import of `module` (compiled from `path`), record each edge
/// for the resolve callback, and queue the resolved paths on `work`. A
/// resolve failure is also kept in [`UnresolvedDuringLoad`].
fn resolve_imports(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    host: &dyn ModuleHost,
    path: &Path,
    module: v8::Local<'_, v8::Module>,
    prefetch: &mut GraphPrefetcher<'_, '_>,
    work: &mut VecDeque<PathBuf>,
) -> Result<(), Vec<Diagnostic>> {
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

        let resolved = host.resolve(&specifier, path).inspect_err(|_| {
            tc.set_slot(UnresolvedDuringLoad(Some((
                specifier.clone(),
                path.to_path_buf(),
            ))));
        })?;
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
            .insert((path.to_path_buf(), specifier.clone()), resolved.clone());
        check_required_import(tc, path, &specifier, &resolved)?;
        // Paths already in the map pop straight into the `continue`
        // above; don't load them ahead either (dyn-import re-entry,
        // diamonds resolved by an earlier pop).
        if !tc
            .get_slot::<ModuleMap>()
            .expect("module map installed")
            .by_path
            .contains_key(&resolved)
        {
            prefetch.enqueue(&resolved);
        }
        work.push_back(resolved);
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
    // SAFETY: V8 calls this synchronously during instantiate_module with the
    // live `context` it is resolving in, so opening a callback scope on that
    // context is valid for the duration of the call.
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
    tc.set_slot(CaughtDuringLoad(None));
    if let Err(diags) = load_module_graph(tc, host, resolved.clone(), false) {
        store_pending_code_cache(tc);
        let caught = tc
            .get_slot_mut::<CaughtDuringLoad>()
            .and_then(|c| c.0.take());
        // The load caught a JS value -- a CommonJS dependency's throw, a
        // syntax error -- which is what node's import() rejects with. A
        // CommonJS dependency's throw is replayed on a later load
        // (ModuleMap::failed_cjs), so a second import() rejects with the
        // same object, as node's does.
        if let Some(value) = caught
            && diags.first().is_some_and(|d| d.code == "OAM-RT0001")
        {
            return DynResult::Rejected(value);
        }
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
        let caught = dyn_caught_value(tc);
        store_pending_code_cache(tc);
        return match caught {
            Some(v) => DynResult::Rejected(v),
            None => DynResult::Error(format!(
                "oam: dynamic import('{spec}'): instantiation failed"
            )),
        };
    }
    if module.get_status() == v8::ModuleStatus::Errored {
        store_pending_code_cache(tc);
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

    // Bytecode for the freshly compiled subgraph is produced right after
    // evaluate, on both arms (see store_pending_code_cache); the caught value
    // is read first so the TryCatch is consulted before any further V8 call.
    let Some(value) = module.evaluate(tc) else {
        let caught = dyn_caught_value(tc);
        store_pending_code_cache(tc);
        return match caught {
            Some(v) => DynResult::Rejected(v),
            None => DynResult::Error(format!("oam: dynamic import('{spec}'): evaluation failed")),
        };
    };
    store_pending_code_cache(tc);
    // Bare checkpoint, NO tick drain: this hook fires synchronously with
    // the importer's JS frame still on the stack, and Node never runs
    // process.nextTick callbacks mid-statement. Ticks scheduled by the
    // imported module wait for the caller's next real tick point. (The
    // mid-hook microtask flush itself is a documented pre-existing
    // divergence the sync-graph fast path depends on.)
    //
    // Inside a synchronous `require()` of an ES module (an import() a
    // CommonJS dependency of the required graph, or its body, makes) not
    // even that: node's require runs no promise jobs before it returns, and
    // the checkpoint would run the requiring code's own. A graph without
    // top-level await has settled within evaluate() and needs none; one
    // with it still takes the checkpoint, which it cannot settle without.
    let settled = v8::Local::<v8::Promise>::try_from(value)
        .is_ok_and(|p| p.state() != v8::PromiseState::Pending);
    if !(settled && inside_require(tc)) {
        tc.perform_microtask_checkpoint();
    }

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

/// The host parked by the running `execute_*` call (see [`ActiveHost`]), for
/// the callbacks that load modules on demand: `import()` and `require()` of
/// an ES module.
fn active_host<'a>(scope: &mut v8::PinScope<'_, '_>) -> Option<&'a dyn ModuleHost> {
    let host_ptr = scope.get_slot::<ActiveHost>().and_then(|h| h.0)?;
    // SAFETY: the pointer is valid for the whole execute_* call that parked
    // it (the host is borrowed there), and the callbacks that read it only
    // fire synchronously during the evaluation that call drives; every entry
    // point without a host clears the slot first. See ActiveHost.
    Some(unsafe { &*host_ptr })
}

/// Why a `require()` of an ES module failed, carried out of its TryCatch
/// scope as scope-independent values.
enum RequireFailure {
    /// Rethrow this value: the module's own throw, or one a CommonJS
    /// dependency or the compile of its graph raised.
    Throw(v8::Global<v8::Value>),
    /// node's coded error `code` (crate::cjs::throw_node_error) from these
    /// arguments.
    Node {
        code: &'static str,
        args: Vec<String>,
    },
    /// An oam-side failure with no JS value behind it (a module that cannot
    /// be resolved or read): an Error carrying the diagnostic's code.
    Diagnostic { code: String, message: String },
}

/// `require()` of the ES module at `key` (a module key) from `parent` (the
/// requiring module's filename): node's importSyncForRequire and
/// ModuleJobSync.runSync (v22.22.2). The graph is loaded, instantiated and
/// evaluated synchronously, inside the requiring statement, and what
/// `require()` returns is chosen as loadESMFromCJS chooses it
/// ([`required_exports`]). It shares the module map with `import`, so a
/// module both load is evaluated once and its namespace is one object.
///
/// Refused, as node refuses them:
/// - a graph with a top-level await anywhere (`is_graph_async`, checked
///   after instantiation and before anything is evaluated):
///   ERR_REQUIRE_ASYNC_MODULE. The module stays instantiated, so `import()`
///   can still evaluate it;
/// - a module in evaluation, the requested one or one its graph imports
///   (V8's status, or for a graph whose CommonJS dependency is running the
///   replay in [`evaluating_in_loads`]), and a CommonJS module whose body is
///   running imported by the graph: ERR_REQUIRE_CYCLE_MODULE.
///
/// A module whose evaluation threw rethrows that same error on every later
/// `require()`. No promise jobs or ticks run: evaluation of a graph without
/// top-level await settles synchronously, as node's evaluateSync expects.
/// None: an exception is pending on `scope`.
pub(crate) fn require_esm(
    scope: &mut v8::PinScope<'_, '_>,
    key: &Path,
    parent: &str,
) -> Option<v8::Global<v8::Value>> {
    if scope.get_slot::<ModuleMap>().is_none() {
        scope.set_slot(ModuleMap::default());
    }
    // The running entry's host, or -- on a path that parks none (the REPL,
    // a timer after the entry returned) -- a fresh one from the embedder's
    // factory, which reads modules the same way.
    let owned;
    let host: &dyn ModuleHost = match active_host(scope) {
        Some(host) => host,
        None => match crate::worker_host() {
            Some(host) => {
                owned = host;
                &*owned
            }
            None => {
                let message = format!(
                    "require() of ES Module {} from {parent}: no module host on this entry path",
                    key.display()
                );
                let text =
                    v8::String::new(scope, &message).unwrap_or_else(|| v8::String::empty(scope));
                let error = v8::Exception::error(scope, text);
                scope.throw_exception(error);
                return None;
            }
        },
    };
    let outcome = {
        v8::tc_scope!(let tc, scope);
        require_esm_sync(tc, host, key, parent)
    };
    match outcome {
        Ok(value) => Some(value),
        Err(RequireFailure::Throw(value)) => {
            let value = v8::Local::new(scope, &value);
            scope.throw_exception(value);
            None
        }
        Err(RequireFailure::Node { code, args }) => {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            crate::cjs::throw_node_error(scope, code, &args);
            None
        }
        Err(RequireFailure::Diagnostic { code, message }) => {
            let text = v8::String::new(scope, &message).unwrap_or_else(|| v8::String::empty(scope));
            let error = v8::Exception::error(scope, text);
            if let (Ok(object), Some(field), Some(value)) = (
                v8::Local::<v8::Object>::try_from(error),
                v8::String::new(scope, "code"),
                v8::String::new(scope, &code),
            ) {
                object.set(scope, field.into(), value.into());
            }
            scope.throw_exception(error);
            None
        }
    }
}

/// node's long-form cycle refusal, for a required module in evaluation.
fn require_cycle(key: &Path, parent: &str) -> RequireFailure {
    RequireFailure::Node {
        code: REQUIRE_CYCLE_CODE,
        args: vec![format!(
            "Cannot require() ES Module {} in a cycle. (from {parent}) A cycle involving \
             require(esm) is not allowed to maintain invariants mandated by the ECMAScript \
             specification. Try making at least part of the dependency in the graph lazily \
             loaded.",
            key.display()
        )],
    }
}

/// node's refusal of a graph with a top-level await (runSync, and
/// getNamespaceSync for a module an import() has already evaluated).
fn require_async(key: &Path, parent: &str) -> RequireFailure {
    RequireFailure::Node {
        code: "ERR_REQUIRE_ASYNC_MODULE",
        args: vec![key.to_string_lossy().into_owned(), parent.to_string()],
    }
}

/// What `require()` returns for a module V8 reports as evaluated, as node's
/// getNamespaceSync decides it: a graph with a top-level await is refused
/// even once it has finished, and V8 reports one still part-way through its
/// await as evaluated too, so the test comes before anything is read from
/// the namespace (whose bindings may still be uninitialized).
fn evaluated_exports(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    key: &Path,
    parent: &str,
    module: v8::Local<'_, v8::Module>,
) -> Result<v8::Global<v8::Value>, RequireFailure> {
    if module.is_graph_async() {
        return Err(require_async(key, parent));
    }
    required_exports(tc, key, module)
}

fn require_esm_sync(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    host: &dyn ModuleHost,
    key: &Path,
    parent: &str,
) -> Result<v8::Global<v8::Value>, RequireFailure> {
    if tc.get_slot::<RequireDepth>().is_none() {
        tc.set_slot(RequireDepth::default());
    }
    tc.get_slot_mut::<RequireDepth>()
        .expect("require depth installed")
        .0 += 1;
    let required = require_esm_sync_inner(tc, host, key, parent);
    if let Some(depth) = tc.get_slot_mut::<RequireDepth>() {
        depth.0 -= 1;
    }
    required
}

/// The module-not-found failures of a `require()` graph's load, as node's
/// ERR_MODULE_NOT_FOUND: a relative or absolute specifier names the file it
/// would have been (`Cannot find module '<path>' imported from <importer>`,
/// with the file URL as `url`), a bare one the package (`Cannot find package
/// '<name>' ...`). A bare specifier whose package was found but whose file
/// was not keeps oam's message, which says which candidates were tried, under
/// node's code.
fn module_not_found(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    diagnostic: Diagnostic,
) -> RequireFailure {
    let unresolved = tc
        .get_slot_mut::<UnresolvedDuringLoad>()
        .and_then(|u| u.0.take());
    let Some((specifier, importer)) = unresolved else {
        return RequireFailure::Diagnostic {
            code: "ERR_MODULE_NOT_FOUND".to_string(),
            message: diagnostic.message,
        };
    };
    let base = importer.to_string_lossy().into_owned();
    let relative = specifier.starts_with("./")
        || specifier.starts_with("../")
        || specifier == "."
        || specifier == "..";
    if relative || Path::new(&specifier).is_absolute() || specifier.starts_with('/') {
        let joined = importer
            .parent()
            .map(|dir| dir.join(&specifier))
            .unwrap_or_else(|| PathBuf::from(&specifier));
        // Lexically: the file does not exist, so there is nothing to
        // canonicalize against.
        let mut file = PathBuf::new();
        for component in joined.components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    file.pop();
                }
                other => file.push(other),
            }
        }
        let url = path_to_file_url(&file);
        return RequireFailure::Node {
            code: "ERR_MODULE_NOT_FOUND",
            args: vec![file.to_string_lossy().into_owned(), base, url],
        };
    }
    if diagnostic.code == "OAM-MOD0002" && !specifier.contains(':') {
        let mut segments = specifier.split('/');
        let first = segments.next().unwrap_or_default();
        let package = match (first.starts_with('@'), segments.next()) {
            (true, Some(name)) => format!("{first}/{name}"),
            _ => first.to_string(),
        };
        return RequireFailure::Node {
            code: "ERR_MODULE_NOT_FOUND",
            args: vec![package, base],
        };
    }
    RequireFailure::Diagnostic {
        code: "ERR_MODULE_NOT_FOUND".to_string(),
        message: diagnostic.message,
    }
}

fn require_esm_sync_inner(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    host: &dyn ModuleHost,
    key: &Path,
    parent: &str,
) -> Result<v8::Global<v8::Value>, RequireFailure> {
    let compiled = |tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>| {
        tc.get_slot::<ModuleMap>()
            .and_then(|map| map.by_path.get(key).cloned())
    };
    // Already compiled -- by an import, an earlier require() that failed, or
    // a load still on the stack.
    if let Some(global) = compiled(tc) {
        let module = v8::Local::new(tc, &global);
        match module.get_status() {
            v8::ModuleStatus::Evaluated => return evaluated_exports(tc, key, parent, module),
            v8::ModuleStatus::Errored => {
                return Err(RequireFailure::Throw(v8::Global::new(
                    tc,
                    module.get_exception(),
                )));
            }
            v8::ModuleStatus::Evaluating => return Err(require_cycle(key, parent)),
            _ => {
                // Fresh, not the memo: this runs from a CommonJS body in the
                // caller's load, where earlier require()s and import()s may
                // have evaluated modules the memo's walk read.
                if evaluating_in_loads(tc).evaluating.contains(key) {
                    return Err(require_cycle(key, parent));
                }
            }
        }
    }

    tc.set_slot(CaughtDuringLoad(None));
    tc.set_slot(UnresolvedDuringLoad(None));
    if let Err(diags) = load_module_graph(tc, host, key.to_path_buf(), true) {
        store_pending_code_cache(tc);
        let caught = tc
            .get_slot_mut::<CaughtDuringLoad>()
            .and_then(|c| c.0.take());
        let Some(first) = diags.into_iter().next() else {
            return Err(RequireFailure::Diagnostic {
                code: "OAM-RT0001".to_string(),
                message: format!("cannot load {}", key.display()),
            });
        };
        return Err(match caught {
            _ if first.code == REQUIRE_CYCLE_CODE => RequireFailure::Node {
                code: REQUIRE_CYCLE_CODE,
                args: vec![first.message],
            },
            // The load caught a JS value -- a CommonJS dependency's throw, a
            // compile error -- and that is what node's require() throws.
            Some(value) if first.code == "OAM-RT0001" => RequireFailure::Throw(value),
            _ if first.code == "OAM-MOD0001" || first.code == "OAM-MOD0002" => {
                module_not_found(tc, first)
            }
            _ => RequireFailure::Diagnostic {
                code: first.code,
                message: first.message,
            },
        });
    }
    let Some(global) = compiled(tc) else {
        return Err(RequireFailure::Diagnostic {
            code: "OAM-RT0001".to_string(),
            message: format!(
                "oam internal: require() loaded {} but did not register it",
                key.display()
            ),
        });
    };
    let module = v8::Local::new(tc, &global);
    // A CommonJS dependency the load ran may have required this module and
    // so settled it already.
    match module.get_status() {
        v8::ModuleStatus::Evaluated => return evaluated_exports(tc, key, parent, module),
        v8::ModuleStatus::Errored => {
            return Err(RequireFailure::Throw(v8::Global::new(
                tc,
                module.get_exception(),
            )));
        }
        v8::ModuleStatus::Evaluating => return Err(require_cycle(key, parent)),
        _ => {}
    }
    let caught = |tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
                  what: &str| {
        match tc.exception() {
            Some(value) => RequireFailure::Throw(v8::Global::new(tc, value)),
            None => RequireFailure::Diagnostic {
                code: "OAM-RT0001".to_string(),
                message: format!("require() of {}: {what} failed", key.display()),
            },
        }
    };
    if module.get_status() == v8::ModuleStatus::Uninstantiated
        && module.instantiate_module(tc, resolve_module_callback) != Some(true)
    {
        store_pending_code_cache(tc);
        return Err(caught(tc, "instantiation"));
    }
    // node's ModuleJobSync.runSync: instantiate, then refuse a graph with a
    // top-level await before running any of it.
    if module.is_graph_async() {
        return Err(require_async(key, parent));
    }
    let Some(value) = module.evaluate(tc) else {
        store_pending_code_cache(tc);
        return Err(caught(tc, "evaluation"));
    };
    store_pending_code_cache(tc);
    let Ok(promise) = v8::Local::<v8::Promise>::try_from(value) else {
        return required_exports(tc, key, module);
    };
    // The evaluation promise is consumed here, so it is no one's unhandled
    // rejection.
    promise.mark_as_handled();
    match promise.state() {
        v8::PromiseState::Fulfilled => required_exports(tc, key, module),
        v8::PromiseState::Rejected => {
            forget_rejection(tc, promise);
            Err(RequireFailure::Throw(v8::Global::new(
                tc,
                promise.result(tc),
            )))
        }
        // Unreachable for a graph without top-level await, which settles
        // within evaluate().
        v8::PromiseState::Pending => Err(require_async(key, parent)),
    }
}

/// Drop `promise` from the unhandled-rejection ledger: V8 reported it as
/// rejected with no handler while evaluate() ran, before anyone could attach
/// one, and the rejection is delivered as a throw instead.
fn forget_rejection(scope: &mut v8::PinScope<'_, '_>, promise: v8::Local<'_, v8::Promise>) {
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
    let matched = entries
        .into_iter()
        .find_map(|(i, global)| (v8::Local::new(scope, &global) == promise).then_some(i));
    if let Some(index) = matched
        && let Some(ledger) = scope.get_slot_mut::<RejectionLedger>()
    {
        ledger.unhandled.remove(index);
    }
}

/// What `require()` of an evaluated ES module returns (loadESMFromCJS): the
/// value of an export named `module.exports` when the module has one; else
/// the namespace itself when it has no default export, or an own
/// `__esModule` export; else -- a default export and no `__esModule` -- the
/// namespace of a facade module that re-exports everything and adds
/// `__esModule: true`, so code transpiled from ESM (`m.__esModule ?
/// m.default : m`) finds the default. The facade is node's
/// required_module_default_facade_source_string; its bindings stay live and
/// its namespace is a real one (Module tag, not extensible).
fn required_exports(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    key: &Path,
    module: v8::Local<'_, v8::Module>,
) -> Result<v8::Global<v8::Value>, RequireFailure> {
    let namespace = module.get_module_namespace();
    let Ok(object) = v8::Local::<v8::Object>::try_from(namespace) else {
        return Ok(v8::Global::new(tc, namespace));
    };
    let has_own = |tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
                   name: &str|
     -> Result<bool, RequireFailure> {
        let Some(name) = v8::String::new(tc, name) else {
            return Ok(false);
        };
        match object.has_own_property(tc, name.into()) {
            Some(has) => Ok(has),
            None => Err(match tc.exception() {
                Some(value) => RequireFailure::Throw(v8::Global::new(tc, value)),
                None => RequireFailure::Diagnostic {
                    code: "OAM-RT0001".to_string(),
                    message: format!("require() of {}: namespace read failed", key.display()),
                },
            }),
        }
    };
    if has_own(tc, "module.exports")? {
        let name = v8::String::new(tc, "module.exports").expect("short string");
        return match object.get(tc, name.into()) {
            Some(value) => Ok(v8::Global::new(tc, value)),
            None => Err(match tc.exception() {
                Some(value) => RequireFailure::Throw(v8::Global::new(tc, value)),
                None => RequireFailure::Diagnostic {
                    code: "OAM-RT0001".to_string(),
                    message: format!("require() of {}: namespace read failed", key.display()),
                },
            }),
        };
    }
    if !has_own(tc, "default")? || has_own(tc, "__esModule")? {
        return Ok(v8::Global::new(tc, namespace));
    }
    default_facade(tc, key).map(|facade| v8::Global::new(tc, facade))
}

/// Compile, link and evaluate a new default facade for the evaluated module
/// at `key` and return its namespace. node builds one per require() that
/// loads the module (createRequiredModuleFacade); require()'s cache is what
/// makes a repeated require() return the same object. The facade is
/// registered in the module map under a path no file can have, with its one
/// edge, `original`, recorded to `key`, so the ordinary resolve callback
/// links it.
fn default_facade<'s>(
    tc: &mut v8::PinnedRef<'s, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    key: &Path,
) -> Result<v8::Local<'s, v8::Value>, RequireFailure> {
    const SOURCE: &str = "export * from 'original';\n\
                          export { default } from 'original';\n\
                          export const __esModule = true;\n";
    let serial = {
        let map = tc
            .get_slot_mut::<ModuleMap>()
            .expect("module map installed");
        map.facades += 1;
        map.facades
    };
    // A NUL cannot appear in a file path on any platform oam runs on.
    let facade_path = PathBuf::from(format!("{}\0require-facade-{serial}", key.display()));
    let failed = |tc: &mut v8::PinnedRef<'s, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>| match tc
        .exception()
    {
        Some(value) => RequireFailure::Throw(v8::Global::new(tc, value)),
        None => RequireFailure::Diagnostic {
            code: "OAM-RT0001".to_string(),
            message: format!("require() of {}: default facade failed", key.display()),
        },
    };
    let (Some(source), Some(name)) = (
        v8::String::new(tc, SOURCE),
        v8::String::new(tc, "node:internal/require_module_default_facade"),
    ) else {
        return Err(failed(tc));
    };
    let origin = v8::ScriptOrigin::new(
        tc,
        name.into(),
        0,
        0,
        false,
        0,
        None,
        false,
        false,
        /* is_module */ true,
        None,
    );
    let mut source = v8::script_compiler::Source::new(source, Some(&origin));
    let Some(facade) = v8::script_compiler::compile_module(tc, &mut source) else {
        return Err(failed(tc));
    };
    {
        let global = v8::Global::new(tc, facade);
        let map = tc
            .get_slot_mut::<ModuleMap>()
            .expect("module map installed");
        map.by_path.insert(facade_path.clone(), global);
        map.paths_by_hash
            .entry(facade.get_identity_hash())
            .or_default()
            .push(facade_path.clone());
        map.edges
            .insert((facade_path, "original".to_string()), key.to_path_buf());
    }
    if facade.instantiate_module(tc, resolve_module_callback) != Some(true) {
        return Err(failed(tc));
    }
    let Some(evaluated) = facade.evaluate(tc) else {
        return Err(failed(tc));
    };
    if let Ok(promise) = v8::Local::<v8::Promise>::try_from(evaluated) {
        promise.mark_as_handled();
    }
    Ok(facade.get_module_namespace())
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
    let Some(host) = active_host(scope) else {
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

/// Windows-safe file:// URL, percent-encoding exactly the characters node's
/// `ada` path encoder does -- the set `url.pathToFileURL` produces on
/// v22.22.2: `"`, `#`, `%`, `<`, `>`, `?`, `[`, `]`, `^`, `` ` ``, `{`, `|`,
/// `}`, `~` and a space, plus every non-ASCII code point as its UTF-8 bytes.
/// (`[`/`]`/`~` matter in real names -- a `[id].js` route file -- not just the
/// non-ASCII case.) Backslashes are already separate-normalized to `/` above.
pub(crate) fn path_to_file_url(path: &Path) -> String {
    let slashed = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file://");
    if !slashed.starts_with('/') {
        out.push('/');
    }
    for ch in slashed.chars() {
        match ch {
            ' ' => out.push_str("%20"),
            '"' => out.push_str("%22"),
            '#' => out.push_str("%23"),
            '%' => out.push_str("%25"),
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            '?' => out.push_str("%3F"),
            '[' => out.push_str("%5B"),
            ']' => out.push_str("%5D"),
            '^' => out.push_str("%5E"),
            '`' => out.push_str("%60"),
            '{' => out.push_str("%7B"),
            '|' => out.push_str("%7C"),
            '}' => out.push_str("%7D"),
            '~' => out.push_str("%7E"),
            c if !c.is_ascii() => {
                let mut buf = [0u8; 4];
                for byte in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("%{byte:02X}"));
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

/// import.meta initializer: url, filename, dirname (Node 20.11+ parity).
/// Builtin facades get url = "node:<name>" with no filename/dirname; CJS
/// facades get their real file path — createRequire(import.meta.url) works
/// everywhere user code can spell it. Zero capture, raw C ABI.
///
/// # Safety
/// Only V8 invokes this, as the import.meta initializer callback, with
/// the exact C ABI signature; `context`/`module`/`meta` are V8 Locals valid for
/// the call and touched only under the callback scope opened below.
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
///
/// # Safety
/// Only V8 invokes this, as the promise-reject callback, with the exact
/// C ABI signature; `message` is a V8-provided struct valid for the call and
/// touched only under the callback scope opened below.
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
    if let Some(exception) = tc.exception() {
        let exception = v8::Global::new(tc, exception);
        tc.set_slot(CaughtDuringLoad(Some(exception)));
    }
    let err = crate::exception_to_error(tc, name);
    Diagnostic::new(
        "OAM-RT0001",
        Severity::Error,
        Origin::Runtime,
        annotate_missing_builtin_export(&err.to_string()),
    )
}

/// Turn V8's bare "does not provide an export named X" into something the
/// reader can act on, when the module in question is a `node:` builtin.
///
/// A builtin's ESM named exports are its module object's own enumerable keys,
/// so an export oam has not implemented fails at LINK time and takes the whole
/// program with it -- even when the importer never calls the function. On its
/// own the V8 message reads like the user's mistake and gives no next step;
/// this says whose gap it is and where the tracked list lives.
///
/// Deliberately scoped to `node:` specifiers. The identical error for a
/// userland module is the user's own bug, and pointing them at oam's
/// divergence list would send them somewhere useless.
fn annotate_missing_builtin_export(message: &str) -> String {
    const MARKER: &str = "does not provide an export named";
    if !message.contains(MARKER) {
        return message.to_string();
    }
    // "The requested module 'node:fs' does not provide an export named 'x'"
    let Some(rest) = message.split_once("requested module '").map(|(_, r)| r) else {
        return message.to_string();
    };
    let Some((specifier, _)) = rest.split_once('\'') else {
        return message.to_string();
    };
    if !specifier.starts_with("node:") {
        return message.to_string();
    }
    format!(
        "{message}\n  {specifier} is a Node builtin oam implements only in part, and a \
         missing name fails at import time even if it is never called. See \
         docs/node-divergences.md (\"Builtin export names\") for the tracked list, or \
         conformance/surface-gaps.json for the machine-readable one. If the name is not \
         listed there, it is a bug worth reporting."
    )
}

/// The module loader's `oam_loader::fs_os_path` (the path its resolver
/// probes) and the fs ops' `oam_core::fs_os_path_with` are two
/// implementations of node's `ToNamespacedPath` for an absolute path; this
/// crate depends on both, so it holds them to one table.
#[cfg(all(test, windows))]
mod fs_os_path_agreement_tests {
    #[test]
    fn loader_and_fs_ops_namespace_an_absolute_path_alike() {
        let rows = [
            r"C:\app\mod.js",
            r"C:\app\mod.js.",
            r"C:\app\mod.js ",
            r"C:\app\NUL",
            r"C:\app\.\sub\..\x.js",
            r"C:/app/sub/",
            r"C:\app\\sub",
            r"C:\",
            r"C:\..\x",
            r"D:\a\b.",
            r"\\srv\sh\x\..\y.",
            r"\\srv\sh\",
            r"\\srv\sh\x",
        ];
        for row in rows {
            let loader = oam_loader::fs_os_path(std::path::Path::new(row));
            let ops = oam_core::fs_os_path_with(
                row,
                || r"C:\unused".to_string(),
                |_| None,
                oam_core::Win32ResolveMode::Cpp,
            );
            assert_eq!(loader.to_str(), Some(&*ops), "{row:?}");
        }
    }
}

/// `path_to_file_url` must percent-encode exactly what node's `ada` path
/// encoder does, since it feeds `import.meta.url` and the
/// MODULE_TYPELESS_PACKAGE_JSON warning. The expected set is the one node
/// v22.22.2's `url.pathToFileURL` produces (measured over every printable
/// ASCII character plus é/中/😀); the `[`/`]`/`~` entries are the ones a
/// minimal "space/#/?/%" encoder missed, and `[`/`]` appear in real names (a
/// Next.js `[id].js` route). Backslashes are normalized to `/` before the
/// percent pass, so a rooted Windows path yields no `%5C`.
#[cfg(test)]
mod path_to_file_url_tests {
    use super::path_to_file_url;
    use std::path::Path;

    #[test]
    fn encodes_ada_s_ascii_set_and_not_the_rest() {
        // (char, expected encoded form) for the full ada set. A backslash is
        // NOT here: it is normalized to `/` before the percent pass (see
        // `normalizes_backslashes_without_percent_encoding_them`).
        let encoded = [
            (' ', "%20"),
            ('"', "%22"),
            ('#', "%23"),
            ('%', "%25"),
            ('<', "%3C"),
            ('>', "%3E"),
            ('?', "%3F"),
            ('[', "%5B"),
            (']', "%5D"),
            ('^', "%5E"),
            ('`', "%60"),
            ('{', "%7B"),
            ('|', "%7C"),
            ('}', "%7D"),
            ('~', "%7E"),
        ];
        for (ch, want) in encoded {
            let url = path_to_file_url(Path::new(&format!("/p/a{ch}b.js")));
            assert!(
                url.ends_with(&format!("a{want}b.js")),
                "{ch:?} must encode to {want}, got {url}"
            );
        }
        // Characters ada leaves literal must NOT be encoded.
        for ch in [
            '!', '$', '&', '\'', '(', ')', '*', '+', ',', '-', '.', '/', ':', ';', '=', '@', '_',
        ] {
            let url = path_to_file_url(Path::new(&format!("/p/a{ch}b.js")));
            assert!(
                url.ends_with(&format!("a{ch}b.js")),
                "{ch:?} must stay literal, got {url}"
            );
        }
    }

    #[test]
    fn encodes_non_ascii_as_utf8_bytes() {
        assert!(path_to_file_url(Path::new("/p/café.js")).ends_with("caf%C3%A9.js"));
        assert!(path_to_file_url(Path::new("/p/中.js")).ends_with("%E4%B8%AD.js"));
        assert!(path_to_file_url(Path::new("/p/😀.js")).ends_with("%F0%9F%98%80.js"));
    }

    #[test]
    fn normalizes_backslashes_without_percent_encoding_them() {
        // A Windows path: separators become `/`, and the drive is prefixed so a
        // bare `C:` does not read as a URL scheme.
        let url = path_to_file_url(Path::new(r"C:\Users\app\mod.js"));
        assert_eq!(url, "file:///C:/Users/app/mod.js");
        assert!(!url.contains("%5C"), "no encoded backslash: {url}");
    }
}
