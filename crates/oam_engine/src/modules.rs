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
    unhandled: Vec<(v8::Global<v8::Promise>, v8::Global<v8::Value>, String)>,
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
fn emit_process_event(
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
        .map(|ledger| ledger.unhandled.drain(..).map(|(p, r, _)| (p, r)).collect())
        .unwrap_or_default();
    for (promise, reason) in drained {
        let reason_local = v8::Local::new(tc, &reason);
        let promise_local = v8::Local::new(tc, &promise);
        emit_process_event(
            tc,
            "unhandledRejection",
            &[reason_local, promise_local.into()],
        );
    }
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
    tc.reset();

    let local = v8::Local::new(tc, &value);
    if emit_process_event(tc, "uncaughtException", &[local]) {
        return None;
    }
    Some(vec![Diagnostic::new(
        "OAM-RT0001",
        Severity::Error,
        Origin::Runtime,
        message,
    )])
}

/// Take any uncaught-exception reports (from the message listener, i.e.
/// exceptions swallowed by the microtask checkpoint) and either deliver
/// them to an 'uncaughtException' listener or fail the run.
pub(crate) fn drain_uncaught(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> Option<Vec<Diagnostic>> {
    let captured: Vec<(v8::Global<v8::Value>, String)> = tc
        .get_slot_mut::<UncaughtLedger>()
        .map(|ledger| ledger.entries.drain(..).collect())
        .unwrap_or_default();
    if captured.is_empty() {
        return None;
    }
    let mut fatal = Vec::new();
    for (value, text) in captured {
        let local = v8::Local::new(tc, &value);
        if emit_process_event(tc, "uncaughtException", &[local]) {
            continue; // a listener handled it
        }
        fatal.push(Diagnostic::new(
            "OAM-RT0001",
            Severity::Error,
            Origin::Runtime,
            format!("Uncaught (in microtask) {text}"),
        ));
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

pub(crate) fn module_key(path: &Path) -> std::io::Result<PathBuf> {
    Ok(normalize_lexically(&std::path::absolute(path)?))
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
    pub(crate) fn reset_run_slots(&mut self) -> Result<(), Vec<Diagnostic>> {
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
        tc.perform_microtask_checkpoint();
        if let Some(failure) = drain_uncaught(tc) {
            return Err(failure);
        }

        let promise = v8::Local::<v8::Promise>::try_from(value)
            .map_err(|_| rt_diag("OAM-RT0001", "module evaluation did not return a promise"))?;

        pump_event_loop(tc, Some(promise))?;

        match promise.state() {
            v8::PromiseState::Fulfilled => match unhandled_rejection_failures(tc) {
                Some(failures) => Err(failures),
                None => Ok(()),
            },
            v8::PromiseState::Rejected => {
                let exception = promise.result(tc);
                let text = exception
                    .to_string(tc)
                    .map(|s| s.to_rust_string_lossy(tc))
                    .unwrap_or_else(|| "unknown exception".to_string());
                Err(rt_diag(
                    "OAM-RT0001",
                    format!("{}: Uncaught {text}", entry.display()),
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
        v8::scope_with_context!(let scope, &mut self.isolate, &self.context);
        v8::tc_scope!(let tc, scope);
        loop {
            let now = std::time::Instant::now();
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
                    // A timer body threw directly — the canonical async
                    // throw. Offer it to uncaughtException before dying.
                    if let Some(failure) = handle_uncaught_throw(tc, "timer callback") {
                        return Err(failure);
                    }
                }
                tc.perform_microtask_checkpoint();
                progressed = true;
            }
            if let Some(completion) = tc
                .get_slot_mut::<oam_core::CoreRuntime>()
                .and_then(|core| core.try_recv())
            {
                crate::ops::settle_completion(tc, completion);
                tc.perform_microtask_checkpoint();
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
            let completion = tc
                .get_slot_mut::<oam_core::CoreRuntime>()
                .and_then(|core| core.recv_deadline(Some(wait_until)));
            if let Some(completion) = completion {
                crate::ops::settle_completion(tc, completion);
                tc.perform_microtask_checkpoint();
            } else if next_timer.is_none_or(|t| t >= deadline) {
                return Ok(());
            }
        }
    }

    /// One REPL evaluation: TypeScript-stripped, await-aware, result
    /// rendered via util.inspect, `_` bound to the last value. Err is the
    /// pretty error text.
    pub fn repl_eval(&mut self, source: &str) -> Result<String, String> {
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
            tc.perform_microtask_checkpoint();
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
                if pump_event_loop(tc, Some(promise)).is_err() {
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
    /// execute_module/execute_cjs call — do NOT reset them here, the test
    /// file's module state must stay alive.
    pub fn run_registered_tests(
        &mut self,
        filter: Option<&str>,
    ) -> Result<String, Vec<Diagnostic>> {
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
        tc.perform_microtask_checkpoint();
        if let Some(failure) = drain_uncaught(tc) {
            return Err(failure);
        }
        let promise = v8::Local::<v8::Promise>::try_from(result)
            .map_err(|_| rt_diag("OAM-TEST0003", "__oamTestRun did not return a promise"))?;

        pump_event_loop(tc, Some(promise))?;

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
pub(crate) fn pump_event_loop(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    entry_promise: Option<v8::Local<'_, v8::Promise>>,
) -> Result<(), Vec<Diagnostic>> {
    loop {
        if entry_promise.is_some_and(|promise| promise.state() == v8::PromiseState::Rejected) {
            break;
        }
        // Fire 'unhandledRejection' for any rejection a listener can catch,
        // promptly (Node fires after the prior turn's microtasks settled,
        // not at end-of-run). No-listener rejections stay fatal.
        flush_handled_rejections(tc);

        // Dispatch any debugger commands that arrived since the last turn.
        // Clone the Rc out first so the isolate slot borrow is released
        // before dispatch reenters V8 (it can run JS / pause).
        let inspector = tc
            .get_slot::<crate::inspector::InspectorSlot>()
            .map(|slot| slot.0.clone());
        if let Some(shared) = inspector {
            shared.poll();
        }

        let mut progressed = false;

        let now = std::time::Instant::now();
        let due = tc
            .get_slot_mut::<crate::timers::TimerQueue>()
            .and_then(|queue| queue.pop_due(now));
        if let Some((callback, extra)) = due {
            let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
            let callback = v8::Local::new(tc, &callback);
            let args: Vec<v8::Local<v8::Value>> =
                extra.iter().map(|g| v8::Local::new(tc, g)).collect();
            if callback.call(tc, recv, &args).is_none()
                && let Some(failure) = handle_uncaught_throw(tc, "timer callback")
            {
                return Err(failure);
            }
            tc.perform_microtask_checkpoint();
            if let Some(failure) = drain_uncaught(tc) {
                return Err(failure);
            }
            progressed = true;
        }

        if let Some(completion) = tc
            .get_slot_mut::<oam_core::CoreRuntime>()
            .and_then(|core| core.try_recv())
        {
            crate::ops::settle_completion(tc, completion);
            tc.perform_microtask_checkpoint();
            if let Some(failure) = drain_uncaught(tc) {
                return Err(failure);
            }
            progressed = true;
        }
        if progressed {
            continue;
        }

        let next_deadline = tc
            .get_slot_mut::<crate::timers::TimerQueue>()
            .and_then(|queue| queue.next_deadline());
        let has_inflight = tc
            .get_slot::<oam_core::CoreRuntime>()
            .is_some_and(|core| core.has_inflight());
        match (next_deadline, has_inflight) {
            (None, false) => break,
            (deadline, true) => {
                let completion = tc
                    .get_slot_mut::<oam_core::CoreRuntime>()
                    .and_then(|core| core.recv_deadline(deadline));
                if let Some(completion) = completion {
                    crate::ops::settle_completion(tc, completion);
                    tc.perform_microtask_checkpoint();
                    if let Some(failure) = drain_uncaught(tc) {
                        return Err(failure);
                    }
                }
            }
            (Some(deadline), false) => {
                let now = std::time::Instant::now();
                if deadline > now {
                    std::thread::sleep(deadline - now);
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
    let unhandled: Vec<(v8::Global<v8::Promise>, v8::Global<v8::Value>, String)> = tc
        .get_slot_mut::<RejectionLedger>()
        .map(|ledger| ledger.unhandled.drain(..).collect())
        .unwrap_or_default();
    if unhandled.is_empty() {
        return None;
    }
    let mut fatal = Vec::new();
    for (promise, reason, message) in unhandled {
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
            format!("unhandled promise rejection: {message}"),
        ));
    }
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
            tc.perform_microtask_checkpoint();
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
        let mut source = v8::script_compiler::Source::new(source_str, Some(&origin));

        let Some(module) = v8::script_compiler::compile_module(tc, &mut source) else {
            return Err(vec![catch_to_diagnostic(tc, &file)]);
        };

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

/// Dynamic import() host callback — wave-1 interim: reject with a clear,
/// actionable message instead of V8's bare "Error: Not supported". Full
/// support (on-demand graph load through the ModuleMap) is roadmapped with
/// the M2 module work.
pub(crate) fn dynamic_import_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    _host_defined_options: v8::Local<'s, v8::Data>,
    _resource_name: v8::Local<'s, v8::Value>,
    specifier: v8::Local<'s, v8::String>,
    _import_attributes: v8::Local<'s, v8::FixedArray>,
) -> Option<v8::Local<'s, v8::Promise>> {
    let resolver = v8::PromiseResolver::new(scope)?;
    let promise = resolver.get_promise(scope);
    let spec = specifier.to_rust_string_lossy(scope);
    let message = v8::String::new(
        scope,
        &format!(
            "oam: dynamic import('{spec}') is not supported yet (lands later in M2) — \
             use a static import, or require() inside CommonJS"
        ),
    )?;
    let exception = v8::Exception::error(scope, message);
    resolver.reject(scope, exception);
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
            let text = reason
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
                .unwrap_or_else(|| "unknown value".to_string());
            let reason_global = v8::Global::new(scope, reason);
            let global = v8::Global::new(scope, promise);
            if let Some(ledger) = scope.get_slot_mut::<RejectionLedger>() {
                ledger.unhandled.push((global, reason_global, text));
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
                        .map(|(i, (g, _, _))| (i, g.clone()))
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
