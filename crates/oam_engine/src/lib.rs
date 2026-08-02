//! oam_engine: the V8 embedding boundary.
//!
//! This is the ONLY crate in the workspace permitted to expose `v8` types in
//! its public API surface (enforced by review; a lint gate arrives with CI).
//! Everything above this crate speaks oam types, so the 4-week V8 bump stays
//! a one-crate affair.
//!
//! M0 scope: platform init, isolate + context lifecycle, script execution,
//! a minimal console binding, and exception -> error formatting. Snapshots,
//! code cache, ops, and the event loop land in M1.
//!
//! rusty_v8 v150 uses the pinned-scope API: scopes are created via the
//! `v8::scope!` / `v8::scope_with_context!` / `v8::tc_scope!` macros, which
//! pin a `ScopeStorage` on the stack and bind `&mut PinScope`.

use anyhow::{Result, anyhow};
use std::sync::Once;

mod cjs;
mod code_cache;
mod crash;
mod crypto_ops;
pub mod fork;
mod inspector;
mod modules;
pub mod napi;
mod node_ops;
mod ops;
pub mod permissions;
pub mod replay;
mod timers;
mod worker;
pub use crash::install_panic_hook;
pub use modules::ModuleHost;
pub use permissions::{BoolOrList, Permissions, PermissionsOptions};
pub use replay::ReplayMode;

static V8_INIT: Once = Once::new();

/// Build-time startup snapshot: js/bootstrap.js pre-parsed, pre-evaluated,
/// compiled code retained. Every Context::new materializes a fresh context
/// from this template; natives are installed after restore (the blob holds
/// pure JS only — see build.rs).
static OAM_SNAPSHOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/oam_snapshot.bin"));

/// Initialize the V8 platform exactly once per process.
pub fn init_platform() {
    V8_INIT.call_once(|| {
        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
    });
}

/// A single JavaScript execution environment: one isolate, one context.
pub struct JsRuntime {
    /// Optional V8 Inspector (CDP debugger). Declared FIRST so it drops
    /// before `isolate` — its V8Inspector/session Drop call back into V8 and
    /// need a live isolate.
    inspector: Option<inspector::InspectorState>,
    isolate: v8::OwnedIsolate,
    context: v8::Global<v8::Context>,
}

/// process.argv as declared by the embedder ([exe, script, ...script-args]).
/// Lives in an isolate slot because the reading native is zero-capture.
pub(crate) struct ProcessArgv(pub Vec<String>);

/// Parse `OAM_MAX_HEAP_MB` into a hard V8 heap cap, in megabytes.
///
/// Node parity: this is oam's `--max-old-space-size` analogue. `Some(mb)`
/// installs a cap; `None` (unset, empty, non-numeric, or `0`) leaves V8 at
/// its default (no artificial cap). A very small value is honored as-is --
/// it simply trips the OOM callback during startup, printing the clean
/// banner rather than aborting raw.
fn oam_max_heap_mb() -> Option<usize> {
    let raw = std::env::var("OAM_MAX_HEAP_MB").ok()?;
    let mb: usize = raw.trim().parse().ok()?;
    if mb == 0 { None } else { Some(mb) }
}

/// V8 `NearHeapLimitCallback`: fires when the heap approaches the
/// `OAM_MAX_HEAP_MB` cap. Prints a clean, ODIF-shaped fatal error and exits
/// deterministically instead of letting V8 raise `FatalProcessOutOfMemory`
/// (a raw abort / Windows crash dialog / core dump).
///
/// Zero-capture so it registers without a closure: the cap is recovered from
/// the heap-limit V8 passes in. The banner goes to stderr (keeping an MCP
/// sidecar's stdout protocol channel clean) and is always the pretty ODIF
/// form -- never JSON -- because this runs inside the GC callback where the
/// CLI's `--json` renderer is unreachable; Node's OOM message is likewise
/// always plain text. Exit code 134 echoes Node's Unix OOM exit
/// (128 + SIGABRT) as a recognizable "heap OOM" signal, delivered uniformly
/// on every platform as a clean exit (no signal, no core dump). It never
/// returns, so V8's fatal path is preempted entirely.
unsafe extern "C" fn near_heap_limit_oom(
    _data: *mut std::ffi::c_void,
    current_heap_limit: usize,
    _initial_heap_limit: usize,
) -> usize {
    use std::io::Write as _;
    let mb = current_heap_limit / (1024 * 1024);
    let banner = format!(
        "error[OAM-RT-OOM]: JavaScript heap out of memory -- reached the {mb} MB cap set by OAM_MAX_HEAP_MB\n"
    );
    let stderr = std::io::stderr();
    let mut lock = stderr.lock();
    let _ = lock.write_all(banner.as_bytes());
    let _ = lock.flush();
    std::process::exit(134);
}

impl JsRuntime {
    pub fn new() -> Self {
        Self::new_with_permissions(None)
    }

    /// Create a new runtime with the given permission restrictions.
    /// `None` -> all permissions granted (same as `new()`).
    pub fn new_with_permissions(opts: Option<permissions::PermissionsOptions>) -> Self {
        // Top-level runtime: install the oam.fork() pre-warm pool.
        Self::new_inner(opts, true)
    }

    /// Create a runtime for a worker / fork-prewarm thread: identical to
    /// `new_with_permissions(None)` EXCEPT it does NOT install a `ForkPool`.
    ///
    /// This is load-bearing for correctness, not just an optimization: the
    /// fork pool spawns `oam-fork-prewarm` threads that each call
    /// `JsRuntime::new()`. If those nested runtimes also installed a pool,
    /// each prewarm thread would spawn more prewarm threads without bound --
    /// the recursion overflows the thread stack and exhausts memory (snapshot
    /// deserialization per level), aborting the process. A worker isolate
    /// never needs to host its own pool: `op_fork_spawn` handles a missing
    /// pool by cold-spawning, so a nested `oam.fork()` still works.
    pub(crate) fn new_worker_runtime() -> Self {
        Self::new_inner(None, false)
    }

    fn new_inner(opts: Option<permissions::PermissionsOptions>, with_fork_pool: bool) -> Self {
        init_platform();
        // OAM_MAX_HEAP_MB: optional hard cap on the V8 heap (Node's
        // --max-old-space-size analogue). Applied at isolate creation so
        // EVERY isolate -- top-level, `oam serve` workers, and fork-prewarm
        // threads -- inherits the cap; a runaway MCP sidecar can't grow the
        // heap unbounded. Unset/0/invalid -> V8 default (no artificial cap).
        let heap_cap_mb = oam_max_heap_mb();
        let mut params =
            v8::CreateParams::default().snapshot_blob(v8::StartupData::from(OAM_SNAPSHOT));
        if let Some(mb) = heap_cap_mb {
            // initial = 0 lets V8 grow from its small default; max is the hard
            // ceiling. Saturate so an absurd MB value can't overflow usize.
            let max_bytes = mb.saturating_mul(1024 * 1024);
            params = params.heap_limits(0, max_bytes);
            // configure_defaults_from_heap_size splits `max` across the young
            // AND old generations, so the effective old-space ceiling (what the
            // near-heap-limit callback trips on) ends up well above `mb` and a
            // small cap never fires. Pin the old generation directly to the
            // requested cap -- this is the knob Node's --max-old-space-size maps
            // to -- so OAM_MAX_HEAP_MB=64 actually caps around 64 MB.
            params = params.set_max_old_generation_size_in_bytes(max_bytes);
        }
        let mut isolate = v8::Isolate::new(params);
        // EXPLICIT microtask policy (docs/design/nexttick-engine.md): under
        // the default auto policy V8 flushes the microtask queue itself
        // whenever the API call depth reaches zero -- BEFORE the host can
        // drain process.nextTick -- so already-queued promise jobs would run
        // ahead of ticks. The engine owns every checkpoint via
        // modules::run_ticks_and_microtasks (Node's tick-point loop).
        isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
        if heap_cap_mb.is_some() {
            // Preempt V8's FatalProcessOutOfMemory with a clean, deterministic
            // exit + ODIF banner (see near_heap_limit_oom).
            isolate.add_near_heap_limit_callback(near_heap_limit_oom, std::ptr::null_mut());
        }
        isolate.set_promise_reject_callback(modules::promise_reject_callback);
        isolate.add_message_listener(modules::message_listener);
        isolate.set_host_initialize_import_meta_object_callback(modules::import_meta_callback);
        isolate.set_host_import_module_dynamically_callback(modules::dynamic_import_callback);
        // Crash reporter: a V8 heap/process OOM is terminal (the isolate cannot
        // continue), so print the shared banner + crash file, then abort. This
        // is the ONLY crash-reporter path that aborts -- the panic hook only
        // prints, so catch_unwind boundaries still recover.
        isolate.set_oom_error_handler(crash::v8_oom_handler);
        isolate.set_slot(timers::TimerQueue::default());
        // CoreRuntime is NOT created here: execute_module / execute_cjs call
        // reset_run_slots() which builds a fresh CoreRuntime before any ops
        // run. Skipping the construction here avoids a wasted multi-thread
        // Tokio runtime (2 worker threads + TLS init + reqwest Client) that
        // would be dropped and rebuilt on the first module execution.
        //
        // The REPL path (which doesn't call reset_run_slots) lazily inits
        // via ensure_core_runtime() before its first tick/eval.
        isolate.set_slot(ops::PendingOps::default());
        isolate.set_slot(crypto_ops::CryptoState::default());
        isolate.set_slot(napi::AddonRegistry::new());
        isolate.set_slot(replay::ReplayState::default());
        // Fork pool: up to 2 pre-warmed isolates for oam.fork() cold-start
        // speedup. The pool lives in an isolate slot so the zero-capture
        // op_fork_spawn callback can reach it without captures.
        //
        // Constructing the pool here is cheap: warming is LAZY, so the prewarm
        // isolates are only spawned on the first oam.fork() (see ForkPool).
        // A program that never forks pays nothing -- no extra isolates, no
        // snapshot deserialization, lower idle RSS.
        //
        // Only the TOP-LEVEL runtime gets a pool. Worker / fork-prewarm
        // isolates pass `with_fork_pool = false` so they do NOT install one --
        // otherwise a prewarm thread's `JsRuntime::new()` could install a pool
        // that warms more prewarm threads, recursing until the stack overflows
        // and the process aborts.
        if with_fork_pool {
            isolate.set_slot(fork::ForkPool::new(2));
        }
        // Permissions slot: all-granted by default so existing code needs
        // no changes.  Restricted runtimes pass Some(PermissionsOptions{..}).
        isolate.set_slot(std::sync::Arc::new(permissions::Permissions::from_opts(
            opts,
        )));
        let context = {
            v8::scope!(let scope, &mut isolate);
            // Deserializes the snapshot's default context: bootstrap.js is
            // already evaluated in here — no JS parsing at startup.
            let context = v8::Context::new(scope, v8::ContextOptions::default());
            let global = v8::Global::new(scope, context);
            let scope = &mut v8::ContextScope::new(scope, context);
            // install_console is intentionally omitted: the M0 console it
            // installs (v8::Object + 5 v8::Function bindings) is immediately
            // overwritten by installRuntimeGlobals() which installs the
            // util.inspect-powered console. Skipping it avoids ~0.5-1ms of
            // dead-code V8 object allocation.
            timers::install(scope, context);
            ops::install(scope, context);
            node_ops::install(scope, context);
            cjs::install(scope, context);
            // Runtime-data globals (process, performance) + the
            // util.inspect-powered console, defined by node_compat.js in
            // the snapshot and instantiated here against live natives.
            install_runtime_globals(scope, context);
            global
        };
        Self {
            inspector: None,
            isolate,
            context,
        }
    }

    /// Attach the V8 Inspector (Chrome DevTools Protocol) on `addr`. Returns
    /// the `ws://` URL a debugger connects to. With `break_on_start`
    /// (`--inspect-brk`), the next run waits for a debugger to attach and
    /// breaks on the first statement. Call once, before executing the entry.
    pub fn attach_inspector(
        &mut self,
        addr: std::net::SocketAddr,
        break_on_start: bool,
    ) -> Result<String> {
        let (state, ws_url) =
            inspector::attach(&mut self.isolate, &self.context, addr, break_on_start)?;
        // The slot lets pump_event_loop reach the transport without the
        // runtime; the field keeps the V8 inspector alive for the run.
        self.isolate
            .set_slot(inspector::InspectorSlot(state.shared()));
        self.inspector = Some(state);
        Ok(ws_url)
    }

    /// Declare process.argv ([exe, script, ...script-args]) for this run.
    /// Call before executing the entry — process.argv reads it lazily.
    pub fn set_process_argv(&mut self, argv: Vec<String>) {
        self.isolate.set_slot(ProcessArgv(argv));
    }

    /// Lazily create the CoreRuntime if it has not been set yet. The REPL
    /// path needs this because it calls `tick()` / `repl_eval()` without
    /// going through `reset_run_slots()`. The normal run_file path skips
    /// this: `reset_run_slots()` builds a fresh CoreRuntime before any ops.
    pub fn ensure_core_runtime(&mut self) {
        if self.isolate.get_slot::<oam_core::CoreRuntime>().is_none() {
            self.isolate
                .set_slot(oam_core::CoreRuntime::new().expect("tokio runtime builds"));
        }
    }

    /// Activate record or replay mode. Call before executing the entry (after
    /// `new()` but before `execute_module` / `execute_cjs`). In off mode this
    /// is a no-op: the default `ReplayState` slot is already `Off`.
    pub fn set_replay_mode(&mut self, mode: replay::ReplayMode) {
        self.isolate.set_slot(replay::ReplayState::from_mode(mode));
    }

    /// Install JS-side monkey-patches for `Math.random`, `Date.now`, and
    /// `performance.now` that route through the record/replay native ops. Must
    /// be called AFTER `set_replay_mode` and BEFORE executing any user code.
    pub fn apply_replay_patches(&mut self) {
        let mode = self
            .isolate
            .get_slot::<replay::ReplayState>()
            .map(|s| s.mode_str())
            .unwrap_or("off");
        if mode == "off" {
            return;
        }
        let js = format!(
            r#"(function() {{
  var mode = "{}";
  if (mode === "record") {{
    var _origRandom = Math.random.bind(Math);
    Math.random = function random() {{
      var v = _origRandom();
      __oam.recordRng(v);
      return v;
    }};
    var _origDateNow = Date.now.bind(Date);
    Date.now = function now() {{
      var v = _origDateNow();
      __oam.recordDateNow(v);
      return v;
    }};
    if (typeof performance !== "undefined" && performance.now) {{
      var _origPerfNow = performance.now.bind(performance);
      performance.now = function now() {{
        var v = _origPerfNow();
        __oam.recordPerfNow(v);
        return v;
      }};
    }}
  }} else if (mode === "replay") {{
    var _origRandom = Math.random.bind(Math);
    Math.random = function random() {{
      return __oam.replayRng(_origRandom());
    }};
    var _origDateNow = Date.now.bind(Date);
    Date.now = function now() {{
      return __oam.replayDateNow(_origDateNow());
    }};
    if (typeof performance !== "undefined" && performance.now) {{
      var _origPerfNow = performance.now.bind(performance);
      performance.now = function now() {{
        return __oam.replayPerfNow(_origPerfNow());
      }};
    }}
  }}
}})();"#,
            mode
        );
        let _ = self.execute_script("__oam_replay_patch.js", &js);
    }

    /// Read `process.exitCode` after a completed run -- Node honors it at
    /// natural exit, and CI consuming exit codes depends on that.
    pub fn process_exit_code(&mut self) -> Option<i32> {
        v8::scope_with_context!(let scope, &mut self.isolate, &self.context);
        let context = scope.get_current_context();
        let global = context.global(scope);
        let process_key = v8::String::new(scope, "process")?;
        let process = global.get(scope, process_key.into())?;
        let process = v8::Local::<v8::Object>::try_from(process).ok()?;
        let key = v8::String::new(scope, "exitCode")?;
        let value = process.get(scope, key.into())?;
        if value.is_null_or_undefined() {
            return None;
        }
        value.int32_value(scope)
    }

    /// Run process 'exit' listeners (Node natural-termination semantics): sets
    /// `process._exiting` and emits `'exit'` synchronously, exactly once (the JS
    /// side guards re-entry). Call after the event loop drains, before reading
    /// the final exit code. Touches `globalThis.process` first so a lazily-built
    /// process is instantiated. A throwing listener is printed to stderr (Node
    /// prints uncaught exit-handler errors); `process.exit()` inside a handler
    /// still terminates via the native path.
    pub fn emit_process_exit(&mut self) {
        // Inline the emit (no leaked global): touch process to build it if
        // lazy, guard on process._exiting so it fires once, emit 'exit' with the
        // current exitCode. Mirrors the JS-side emitProcessExit used by
        // process.exit(); process._exiting is the shared once-guard.
        // IIFE so the locals stay function-scoped: top-level `var` in a classic
        // script would leak onto globalThis (and trip Node common's leak check).
        if let Err(e) = self.execute_script(
            "<process-exit>",
            "(function () { \
               var p = globalThis.process; \
               if (p && !p._exiting) { \
                 p._exiting = true; \
                 var c = typeof p.exitCode === 'number' ? p.exitCode : 0; \
                 p.emit('exit', c); \
               } \
             })();",
        ) {
            eprintln!("{e}");
        }
    }

    /// Compile and run `source` as a classic script. Returns the stringified
    /// completion value. Exceptions come back as `Err` with script:line info.
    pub fn execute_script(&mut self, name: &str, source: &str) -> Result<String> {
        // Plain classic scripts can still call import() -- V8 routes any
        // import() through the host_import callback set on the isolate.
        // Clear any host pointer parked by a prior execute_module so we
        // reject cleanly instead of dereffing a stale ptr.
        self.clear_active_host();
        let result = {
            v8::scope_with_context!(let scope, &mut self.isolate, &self.context);
            v8::tc_scope!(let tc, scope);

            let source_v8 = v8::String::new(tc, source)
                .ok_or_else(|| anyhow!("source too long for V8 string"))?;
            let name_v8: v8::Local<v8::Value> = v8::String::new(tc, name)
                .ok_or_else(|| anyhow!("script name too long for V8 string"))?
                .into();
            let origin =
                v8::ScriptOrigin::new(tc, name_v8, 0, 0, false, 0, None, false, false, false, None);

            let Some(script) = v8::Script::compile(tc, source_v8, Some(&origin)) else {
                return Err(exception_to_error(tc, name));
            };
            let Some(value) = script.run(tc) else {
                return Err(exception_to_error(tc, name));
            };
            let result_str = value
                .to_string(tc)
                .map(|s| s.to_rust_string_lossy(tc))
                .unwrap_or_default();
            if let Some(failures) = crate::modules::run_ticks_and_microtasks(tc) {
                let text = failures
                    .iter()
                    .map(|d| d.message.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                return Err(anyhow!("{text}"));
            }
            if tc.has_caught() {
                return Err(exception_to_error(tc, name));
            }
            result_str
        };
        Ok(result)
    }

    /// Produce a V8 bytecode blob for `source` compiled as a CommonJS module
    /// (the shape the `require()` path uses), WITHOUT executing it. Returns
    /// `None` on a syntax error or if V8 declines to produce a cache. Spins up
    /// a throwaway runtime for the isolate+context. Used by `oam compile` to
    /// embed bytecode so a compiled binary's first run skips parse+compile.
    pub fn precompile_cjs_source(source: &str) -> Option<Vec<u8>> {
        let mut rt = Self::new();
        v8::scope_with_context!(let scope, &mut rt.isolate, &rt.context);
        crate::cjs::produce_cjs_code_cache(scope, source)
    }
}

/// Seed embedded CJS bytecode (from `oam compile`) into the in-process code
/// cache so the next load of the same source consumes it without touching disk
/// -- a compiled binary then skips parse+compile on its first run even in a
/// read-only / ephemeral environment. The source is shebang-stripped to match
/// the loader's cache key. Pairs with [`JsRuntime::precompile_cjs_source`].
pub fn seed_cjs_bytecode(source: &str, blob: Vec<u8>) {
    let key = crate::cjs::strip_shebang(source.to_string());
    crate::code_cache::seed(&key, crate::code_cache::Kind::Function, blob);
}

impl Default for JsRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl JsRuntime {
    /// Load a `.node` addon directly — used by the lifecycle drop-counter
    /// test only.  Returns `true` if the addon registered without a pending
    /// exception.
    pub(crate) fn load_test_addon(&mut self, path: &std::path::Path) -> bool {
        v8::scope_with_context!(let scope, &mut self.isolate, &self.context);
        napi::load_addon(scope, path).is_some()
    }
}

impl Drop for JsRuntime {
    fn drop(&mut self) {
        // Tear the V8 inspector down explicitly, with the isolate entered and
        // the context unregistered, while the isolate is still alive. Field
        // drop order alone is not enough: the session/inspector C++
        // destructors require the isolate to be current.
        if let Some(inspector) = self.inspector.take() {
            v8::scope_with_context!(let scope, &mut self.isolate, &self.context);
            let context = scope.get_current_context();
            inspector.teardown(context);
        }
    }
}

pub(crate) fn exception_to_error(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    name: &str,
) -> anyhow::Error {
    let Some(message) = tc.message() else {
        return anyhow!("{name}: unknown exception");
    };
    let text = message.get(tc).to_rust_string_lossy(tc);
    let line = message.get_line_number(tc).unwrap_or(0);
    anyhow!("{name}:{line}: {text}")
}

/// Call `__oamNode.installRuntimeGlobals()` (snapshot JS): installs
/// process/performance and upgrades console to the util.inspect formatter.
fn install_runtime_globals(scope: &mut v8::PinScope<'_, '_>, context: v8::Local<v8::Context>) {
    let global = context.global(scope);
    let registry_key = v8::String::new(scope, "__oamNode").unwrap();
    let registry = global
        .get(scope, registry_key.into())
        .expect("__oamNode in snapshot");
    let registry = v8::Local::<v8::Object>::try_from(registry).expect("__oamNode is an object");
    let install_key = v8::String::new(scope, "installRuntimeGlobals").unwrap();
    let install = registry
        .get(scope, install_key.into())
        .expect("installRuntimeGlobals defined");
    let install =
        v8::Local::<v8::Function>::try_from(install).expect("installRuntimeGlobals is a function");
    let recv: v8::Local<v8::Value> = registry.into();
    install
        .call(scope, recv, &[])
        .expect("runtime globals install cleanly");
}

// M0 console (install_console + format_args + console_log_stdout/stderr)
// removed: installRuntimeGlobals() unconditionally overwrites
// globalThis.console with the util.inspect-powered version, so the M0
// bindings were dead code from JsRuntime::new's first call. Removing
// them saves ~0.5-1ms of V8 object allocation per runtime creation.

#[cfg(test)]
mod tests {
    use super::*;
    use napi::NAPI_ENV_DROP_COUNT;

    #[test]
    fn executes_script_and_returns_value() {
        let mut rt = JsRuntime::new();
        let result = rt.execute_script("test.js", "1 + 2").unwrap();
        assert_eq!(result, "3");
    }

    #[test]
    fn exceptions_surface_with_script_name_and_line() {
        let mut rt = JsRuntime::new();
        let err = rt
            .execute_script("boom.js", "\nthrow new Error('kaboom')")
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("boom.js:2"), "got: {msg}");
        assert!(msg.contains("kaboom"), "got: {msg}");
    }

    #[test]
    fn microtasks_run_after_script() {
        let mut rt = JsRuntime::new();
        let result = rt
            .execute_script(
                "micro.js",
                "globalThis.x = 0; Promise.resolve().then(() => { globalThis.x = 1; }); x",
            )
            .unwrap();
        assert_eq!(result, "0");
        let after = rt.execute_script("micro2.js", "globalThis.x").unwrap();
        assert_eq!(after, "1");
    }

    #[test]
    fn console_log_is_installed() {
        let mut rt = JsRuntime::new();
        rt.execute_script("log.js", "console.log('hello from oam')")
            .unwrap();
    }

    #[test]
    fn bootstrap_comes_from_the_snapshot() {
        // fetch is defined by bootstrap.js, which is only ever evaluated at
        // BUILD time into the snapshot — its presence proves the context
        // deserialized from the blob.
        let mut rt = JsRuntime::new();
        let result = rt.execute_script("snap.js", "typeof fetch").unwrap();
        assert_eq!(result, "function");
        // And a second runtime restores independently.
        let mut rt2 = JsRuntime::new();
        assert_eq!(
            rt2.execute_script("snap2.js", "typeof fetch").unwrap(),
            "function"
        );
    }

    /// Verify that NapiEnv (and its owned FnData) is dropped exactly once per
    /// JsRuntime drop when the runtime loaded an N-API addon.
    ///
    /// Without the fix (Box::leak), drop_count would stay at 0 because nothing
    /// ever drops the leaked allocation.  With the AddonRegistry fix, each
    /// runtime drop decrements its slot, and the slot drops all envs.
    ///
    /// NOTE: the test addon DLL must already be built (`cargo build
    /// -p oam_napi_test_addon`) before running this test.  The CI workflow
    /// builds all workspace members before running tests, so this is
    /// satisfied automatically.
    #[test]
    fn napienv_lifecycle_drops_with_runtime() {
        // Determine the path to the compiled test addon.
        let addon_file = if cfg!(windows) {
            "oam_napi_test_addon.dll"
        } else if cfg!(target_os = "macos") {
            "liboam_napi_test_addon.dylib"
        } else {
            "liboam_napi_test_addon.so"
        };
        let addon_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/debug")
            .join(addon_file);

        if !addon_path.is_file() {
            // Soft-skip rather than hard-fail when the addon hasn't been
            // compiled yet (e.g. a bare `cargo test -p oam_engine` without
            // first building the workspace).
            eprintln!(
                "SKIP napienv_lifecycle_drops_with_runtime: addon not found at {}",
                addon_path.display()
            );
            return;
        }

        // Reset the drop counter on this thread (tests may run on the same
        // thread as other napi tests).
        NAPI_ENV_DROP_COUNT.with(|c| c.set(0));

        const ITERATIONS: usize = 100;
        for _ in 0..ITERATIONS {
            let mut rt = JsRuntime::new();
            let loaded = rt.load_test_addon(&addon_path);
            assert!(loaded, "test addon must load without exception");
            // rt drops here -- AddonRegistry slot drops, which drops
            // the Box<NapiEnv>, which increments NAPI_ENV_DROP_COUNT.
        }

        let drops = NAPI_ENV_DROP_COUNT.with(|c| c.get());
        assert_eq!(
            drops, ITERATIONS,
            "expected {ITERATIONS} NapiEnv drops (one per runtime), got {drops}; \
             the AddonRegistry fix is not working correctly"
        );
    }

    // ---------------------------------------- permission gate integration tests

    /// Helper: build a temp file path that does not exist (for "denied" tests
    /// we never need to read it; for "allowed" tests we create it first).
    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("oam-perm-test-{name}"))
    }

    /// Returns the JSON-escaped form of `path` (forward slashes on all
    /// platforms).
    fn js_path(path: &std::path::Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    // ---- read denied
    //
    // Use __oam.node.fsReadFileSync directly (the internal op) rather than
    // going through require('fs'), which needs a module system.

    #[test]
    fn permission_read_denied_throws_err_permission_denied() {
        let opts = permissions::PermissionsOptions {
            read: permissions::BoolOrList::Bool(false),
            ..Default::default()
        };
        let mut rt = JsRuntime::new_with_permissions(Some(opts));
        // fsReadFileSync is the raw op; call it directly to avoid require().
        let err = rt
            .execute_script(
                "perm_read_deny.js",
                r#"
                try {
                    __oam.node.fsReadFileSync('/tmp/any-file');
                    'no-throw'
                } catch (e) {
                    e.code
                }
                "#,
            )
            .unwrap();
        assert_eq!(err, "ERR_PERMISSION_DENIED", "got: {err}");
    }

    // ---- read allowed for whitelisted prefix

    #[test]
    fn permission_read_allowed_for_permitted_path() {
        let path = temp_path("read-allow.txt");
        std::fs::write(&path, b"hello").expect("write temp file");
        let js = js_path(&path);
        // Only the temp dir is whitelisted.
        let tmp = std::env::temp_dir();
        let tmp_str = tmp.to_string_lossy().replace('\\', "/");
        let opts = permissions::PermissionsOptions {
            read: permissions::BoolOrList::List(vec![tmp_str]),
            ..Default::default()
        };
        let mut rt = JsRuntime::new_with_permissions(Some(opts));
        let result = rt
            .execute_script(
                "perm_read_allow.js",
                &format!(
                    r#"
                    try {{
                        __oam.node.fsReadFileSync('{js}');
                        'ok'
                    }} catch (e) {{
                        'err:' + e.code
                    }}
                    "#
                ),
            )
            .unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(result, "ok", "got: {result}");
    }

    // ---- write denied

    #[test]
    fn permission_write_denied_throws_err_permission_denied() {
        let opts = permissions::PermissionsOptions {
            write: permissions::BoolOrList::Bool(false),
            ..Default::default()
        };
        let mut rt = JsRuntime::new_with_permissions(Some(opts));
        let err = rt
            .execute_script(
                "perm_write_deny.js",
                r#"
                try {
                    __oam.node.fsWriteFileSync('/tmp/oam-perm-write-deny.txt', 'x');
                    'no-throw'
                } catch (e) {
                    e.code
                }
                "#,
            )
            .unwrap();
        assert_eq!(err, "ERR_PERMISSION_DENIED", "got: {err}");
    }

    // ---- write allowed

    #[test]
    fn permission_write_allowed_for_permitted_path() {
        let path = temp_path("write-allow.txt");
        let _ = std::fs::remove_file(&path);
        let js = js_path(&path);
        let tmp = std::env::temp_dir();
        let tmp_str = tmp.to_string_lossy().replace('\\', "/");
        let opts = permissions::PermissionsOptions {
            write: permissions::BoolOrList::List(vec![tmp_str]),
            ..Default::default()
        };
        let mut rt = JsRuntime::new_with_permissions(Some(opts));
        let result = rt
            .execute_script(
                "perm_write_allow.js",
                &format!(
                    r#"
                    try {{
                        __oam.node.fsWriteFileSync('{js}', 'oam-perm-test');
                        'ok'
                    }} catch (e) {{
                        'err:' + e.code
                    }}
                    "#
                ),
            )
            .unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(result, "ok", "got: {result}");
    }

    // ---- net denied (fetch -- synchronous gate in the __oam.fetch op)
    //
    // The fetch() global is a thin wrapper over __oam.fetch; the permission
    // check inside op_fetch throws synchronously before any promise is
    // created. That uncaught exception surfaces in execute_script as
    // execute_script returning Err, OR as the exception value in a try/catch.

    #[test]
    fn permission_net_denied_fetch_throws_err_permission_denied() {
        let opts = permissions::PermissionsOptions {
            net: permissions::BoolOrList::Bool(false),
            ..Default::default()
        };
        let mut rt = JsRuntime::new_with_permissions(Some(opts));
        // Call __oam.fetch directly with a minimal JSON payload so we bypass
        // any JS-level try/catch wrapping in the fetch() global wrapper.
        let err = rt
            .execute_script(
                "perm_net_deny.js",
                r#"
                try {
                    __oam.fetch(JSON.stringify({url: 'http://example.com/'}));
                    'no-throw'
                } catch (e) {
                    e.code
                }
                "#,
            )
            .unwrap();
        assert_eq!(err, "ERR_PERMISSION_DENIED", "got: {err}");
    }

    // ---- net allowed

    #[test]
    fn permission_net_allowed_for_listed_host_does_not_throw() {
        let opts = permissions::PermissionsOptions {
            net: permissions::BoolOrList::List(vec!["example.com".to_string()]),
            ..Default::default()
        };
        let mut rt = JsRuntime::new_with_permissions(Some(opts));
        // fetch spawns an async op via CoreRuntime; init it since
        // execute_script doesn't call reset_run_slots.
        rt.ensure_core_runtime();
        // The op IS spawned (it returns a pending promise); the permission gate
        // passes, so no exception is thrown synchronously.  We do NOT await
        // (no real HTTP connection in unit tests); just verify no throw.
        let result = rt
            .execute_script(
                "perm_net_allow.js",
                r#"
                try {
                    const p = __oam.fetch(JSON.stringify({url: 'http://example.com/'}));
                    typeof p        // "object" (Promise)
                } catch (e) {
                    'err:' + e.code
                }
                "#,
            )
            .unwrap();
        // Drop inflight ops: the future spawned in the test will be cancelled
        // when the runtime drops (shutdown_background).
        assert_eq!(result, "object", "got: {result}");
    }

    // ---- permissions query via oam:permissions JS module

    #[test]
    fn oam_permissions_module_query_returns_correct_state() {
        use crate::modules::ModuleHost;
        use oam_diagnostics::{Diagnostic, Origin, Severity};

        struct InlineHost {
            source: String,
        }
        impl ModuleHost for InlineHost {
            fn resolve(
                &self,
                specifier: &str,
                _referrer: &std::path::Path,
            ) -> Result<std::path::PathBuf, Vec<Diagnostic>> {
                if specifier == "oam:permissions" {
                    return Ok(std::path::PathBuf::from("oam:permissions"));
                }
                Err(vec![Diagnostic::new(
                    "OAM-MOD0001",
                    Severity::Error,
                    Origin::Resolve,
                    format!("cannot resolve {specifier}"),
                )])
            }
            fn load(&self, path: &std::path::Path) -> Result<String, Vec<Diagnostic>> {
                // execute_module normalizes the entry to an absolute path via
                // module_key(); match by file name only so tests don't need to
                // supply an absolute path as the entry.
                if path.file_name().and_then(|n| n.to_str()) == Some("perm_query_test.mjs") {
                    return Ok(self.source.clone());
                }
                Err(vec![Diagnostic::new(
                    "OAM-RT0002",
                    Severity::Error,
                    Origin::Runtime,
                    format!("cannot load {}", path.display()),
                )])
            }
        }

        let opts = permissions::PermissionsOptions {
            read: permissions::BoolOrList::Bool(false),
            net: permissions::BoolOrList::Bool(false),
            write: permissions::BoolOrList::Bool(true),
            ..Default::default()
        };
        let mut rt = JsRuntime::new_with_permissions(Some(opts));

        let source = r#"
            import { permissions } from 'oam:permissions';
            const readStatus  = await permissions.query({ name: 'read' });
            const writeStatus = await permissions.query({ name: 'write' });
            const netStatus   = await permissions.query({ name: 'net' });
            if (readStatus.state  !== 'denied')  throw new Error('read should be denied: '  + readStatus.state);
            if (writeStatus.state !== 'granted') throw new Error('write should be granted: ' + writeStatus.state);
            if (netStatus.state   !== 'denied')  throw new Error('net should be denied: '   + netStatus.state);
            globalThis.__perm_ok = true;
        "#
        .to_string();

        let host = InlineHost { source };
        let entry = std::path::Path::new("perm_query_test.mjs");
        rt.execute_module(entry, &host)
            .expect("permissions query module runs without error");

        // Verify the module ran to completion.
        let ok = rt
            .execute_script("check.js", "globalThis.__perm_ok")
            .unwrap();
        assert_eq!(
            ok, "true",
            "permissions query module did not complete: {ok}"
        );
    }
}
