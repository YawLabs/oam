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
/// zero-capture reason as ModuleMap.
#[derive(Default)]
pub(crate) struct RejectionLedger {
    unhandled: Vec<(v8::Global<v8::Promise>, String)>,
}

/// Uncaught exceptions V8 reports outside any TryCatch — microtask callbacks
/// are the reachable case: perform_microtask_checkpoint SWALLOWS their
/// exceptions (documented), and without a message listener V8 just printed
/// them to stdout while we exited 0 (review finding). The listener records
/// them here; the event loop fails the run after each drain, like Node's
/// uncaughtException.
#[derive(Default)]
pub(crate) struct UncaughtLedger {
    reports: Vec<String>,
}

/// V8 message listener. Zero capture, raw C ABI.
pub(crate) unsafe extern "C" fn message_listener(
    message: v8::Local<v8::Message>,
    _exception: v8::Local<v8::Value>,
) {
    v8::callback_scope!(unsafe scope, message);
    let text = message.get(scope).to_rust_string_lossy(scope);
    let line = message.get_line_number(scope).unwrap_or(0);
    let file = message
        .get_script_resource_name(scope)
        .and_then(|name| name.to_string(scope))
        .map(|name| name.to_rust_string_lossy(scope))
        .unwrap_or_else(|| "<unknown>".to_string());
    if let Some(ledger) = scope.get_slot_mut::<UncaughtLedger>() {
        ledger.reports.push(format!("{file}:{line}: {text}"));
    }
}

/// Take any uncaught-exception reports and convert them to a failing
/// diagnostic set. Called after every microtask drain in the event loop.
fn drain_uncaught(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
) -> Option<Vec<Diagnostic>> {
    let reports: Vec<String> = tc
        .get_slot_mut::<UncaughtLedger>()
        .map(|ledger| ledger.reports.drain(..).collect())
        .unwrap_or_default();
    if reports.is_empty() {
        return None;
    }
    Some(
        reports
            .into_iter()
            .map(|report| {
                Diagnostic::new(
                    "OAM-RT0001",
                    Severity::Error,
                    Origin::Runtime,
                    format!("Uncaught (in microtask) {report}"),
                )
            })
            .collect(),
    )
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

fn module_key(path: &Path) -> std::io::Result<PathBuf> {
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
        self.isolate.set_slot(ModuleMap::default());
        self.isolate.set_slot(RejectionLedger::default());
        self.isolate.set_slot(UncaughtLedger::default());
        self.isolate.set_slot(crate::timers::TimerQueue::default());
        // Fresh CoreRuntime per run: dropping the old one cancels any ops a
        // previous execute_module left in flight; PendingOps resolvers from
        // that run die with it.
        self.isolate.set_slot(
            oam_core::CoreRuntime::new()
                .map_err(|e| rt_diag("OAM-RT0002", format!("io runtime failed to start: {e}")))?,
        );
        self.isolate.set_slot(crate::ops::PendingOps::default());

        v8::scope_with_context!(let scope, &mut self.isolate, &self.context);
        v8::tc_scope!(let tc, scope);

        load_module_graph(tc, host, entry.clone())?;

        let module = {
            let map = tc.get_slot::<ModuleMap>().expect("module map installed");
            let global = map.by_path.get(&entry).expect("entry preloaded").clone();
            v8::Local::new(tc, &global)
        };

        if module.instantiate_module(tc, resolve_module_callback) != Some(true) {
            return Err(vec![catch_to_diagnostic(tc, &entry.to_string_lossy())]);
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

        // M1 blocking event loop. Each turn services at most ONE due timer
        // (so a clear issued by one callback cancels same-instant siblings)
        // AND THEN one ready op completion — never timer-only: a
        // continuously-due interval must not starve IO settlement (review
        // blocker; Node's phase model services poll after timers no matter
        // how busy timers are). Microtask drain after each, so dependent
        // code runs at the earliest correct moment. When idle, block on the
        // op channel with the next timer deadline as timeout (one blocking
        // point, no busy-wait). Exit when nothing remains; process stays
        // alive for pending timers/ops after the entry module fulfills —
        // Node semantics. Entry rejection breaks early.
        loop {
            if promise.state() == v8::PromiseState::Rejected {
                break;
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
                if callback.call(tc, recv, &args).is_none() {
                    return Err(vec![catch_to_diagnostic(tc, "timer callback")]);
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

        match promise.state() {
            v8::PromiseState::Fulfilled => {
                let unhandled: Vec<String> = tc
                    .get_slot_mut::<RejectionLedger>()
                    .map(|ledger| {
                        ledger
                            .unhandled
                            .drain(..)
                            .map(|(_, message)| message)
                            .collect()
                    })
                    .unwrap_or_default();
                if unhandled.is_empty() {
                    Ok(())
                } else {
                    Err(unhandled
                        .into_iter()
                        .map(|message| {
                            Diagnostic::new(
                                "OAM-RT0004",
                                Severity::Error,
                                Origin::Runtime,
                                format!("unhandled promise rejection: {message}"),
                            )
                        })
                        .collect())
                }
            }
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

/// Compile the module graph into the isolate's ModuleMap. Explicit worklist —
/// recursion would overflow the stack on deep (generated-code) import chains.
fn load_module_graph(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    host: &dyn ModuleHost,
    entry: PathBuf,
) -> Result<(), Vec<Diagnostic>> {
    let mut work = vec![entry];
    while let Some(path) = work.pop() {
        if tc
            .get_slot::<ModuleMap>()
            .expect("module map installed")
            .by_path
            .contains_key(&path)
        {
            continue;
        }

        let code = host.load(&path)?;
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
            let resolved = host.resolve(&specifier, &path)?;
            let resolved = module_key(&resolved).map_err(|e| {
                rt_diag(
                    "OAM-RT0002",
                    format!("bad resolved path {}: {e}", resolved.display()),
                )
            })?;
            tc.get_slot_mut::<ModuleMap>()
                .expect("module map installed")
                .edges
                .insert((path.clone(), specifier), resolved.clone());
            work.push(resolved);
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

/// V8 promise-reject callback: maintains the RejectionLedger. Zero capture,
/// raw C ABI (set_promise_reject_callback takes the fn type directly).
pub(crate) unsafe extern "C" fn promise_reject_callback(message: v8::PromiseRejectMessage) {
    v8::callback_scope!(unsafe scope, &message);
    let promise = message.get_promise();
    match message.get_event() {
        v8::PromiseRejectEvent::PromiseRejectWithNoHandler => {
            let text = message
                .get_value()
                .and_then(|v| v.to_string(scope))
                .map(|s| s.to_rust_string_lossy(scope))
                .unwrap_or_else(|| "unknown value".to_string());
            let global = v8::Global::new(scope, promise);
            if let Some(ledger) = scope.get_slot_mut::<RejectionLedger>() {
                ledger.unhandled.push((global, text));
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

fn catch_to_diagnostic(
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
