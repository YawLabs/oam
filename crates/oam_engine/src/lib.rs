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
mod vm_context;
mod vm_module;
mod worker;
pub use crash::install_panic_hook;
pub use modules::ModuleHost;
// Re-exported so the CLI can register its `-e` artifact without taking a direct
// oam_core dependency; the hard-exit paths that drain it live in this crate.
pub use oam_core::{exit_process, register_exit_cleanup, run_exit_cleanup, snapshot_inherited_fds};
pub use permissions::{BoolOrList, Permissions, PermissionsOptions};
pub use replay::ReplayMode;

static V8_INIT: Once = Once::new();

/// Build-time startup snapshot: js/bootstrap.js pre-parsed, pre-evaluated,
/// Index of the runtime context inside the startup snapshot. Must match
/// `RUNTIME_CONTEXT_INDEX` in build.rs.
pub(crate) const RUNTIME_CONTEXT_INDEX: usize = 0;

/// compiled code retained. Every Context::new materializes a fresh context
/// from this template; natives are installed after restore (the blob holds
/// pure JS only — see build.rs).
static OAM_SNAPSHOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/oam_snapshot.bin"));

/// Initialize the V8 platform exactly once per process.
pub fn init_platform() {
    init_platform_with_flags(&[]);
}

/// Builds the module host a WORKER thread uses for an ESM entry. The host
/// lives in oam_cli (filesystem + oxc + oam_loader rules) and workers are
/// spawned inside the engine, so the embedder registers a constructor here
/// rather than the engine duplicating it. Only the fn pointer crosses
/// threads; the host itself is built on the worker.
pub type WorkerHostFactory = fn() -> Box<dyn ModuleHost>;
static WORKER_HOST_FACTORY: std::sync::OnceLock<WorkerHostFactory> = std::sync::OnceLock::new();

/// Process-level flag state (`--no-warnings` and friends) as the JS snippet
/// that installs it. Workers are OS threads in this process with their own
/// isolates, so they need it re-run per isolate to inherit -- Node's worker
/// flags are inherited the same way.
static INHERITED_FLAGS_JS: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub fn set_inherited_flags_js(js: String) {
    let _ = INHERITED_FLAGS_JS.set(js);
}

pub(crate) fn inherited_flags_js() -> Option<&'static str> {
    INHERITED_FLAGS_JS.get().map(String::as_str)
}

pub fn set_worker_host_factory(factory: WorkerHostFactory) {
    let _ = WORKER_HOST_FACTORY.set(factory);
}

pub(crate) fn worker_host() -> Option<Box<dyn ModuleHost>> {
    WORKER_HOST_FACTORY.get().map(|f| f())
}

/// V8 flags must be set BEFORE `V8::initialize`, so the embedder passes any
/// node-style flags that map to V8 ones (currently `--expose-gc`) here.
pub fn init_platform_with_flags(v8_flags: &[&str]) {
    V8_INIT.call_once(|| {
        if !v8_flags.is_empty() {
            v8::V8::set_flags_from_string(&v8_flags.join(" "));
        }
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

/// Default V8 heap cap when `OAM_MAX_HEAP_MB` is unset. 4 GiB matches Node's
/// built-in default for `--max-old-space-size` on 64-bit hosts. Without a
/// cap, a single runaway sync op -- a glob over a large tree, an unbuffered
/// file read, a parser that pins its inputs -- can grow the heap past 1.4 GiB
/// and trip V8's "Ineffective mark-compacts near heap limit" path, which has
/// no graceful exit and aborts the process (see crash.rs::v8_oom_handler).
/// Capping at startup converts that into the deterministic `near_heap_limit_oom`
/// exit instead. 4 GiB matches the upper bound of the user-visible
/// `OAM_MAX_HEAP_MB` knob: raising it past 4 GiB gives back roughly nothing
/// on a 64-bit host (V8 compresses pointers below that threshold) and is
/// almost always the user misreading "MB" as "GiB".
const DEFAULT_HEAP_MB: usize = 4096;

/// Provenance of the active heap cap, surfaced in the OOM banner. The cap is
/// captured at `JsRuntime::new()` (one thread per isolate), so the banner
/// reads the source from a thread-local rather than re-reading the env var --
/// re-reading at OOM time would lie if the user `export`ed the var between
/// startup and the OOM, because the cap itself was already baked into V8's
/// resource constraints.
#[derive(Copy, Clone)]
enum HeapCapSource {
    /// User set `OAM_MAX_HEAP_MB` to a positive number.
    User,
    /// Built-in 4 GiB default (env var unset, empty, or "0").
    Default,
}

thread_local! {
    /// Provenance for the current isolate's heap cap. Set in `JsRuntime::new`
    /// immediately before `add_near_heap_limit_callback` so V8's callback
    /// (which runs on the same thread as the isolate that registered it)
    /// picks up the right label. V8 never invokes the callback from another
    /// thread, so the thread-local is the right scope: a worker thread that
    /// creates its own isolate writes its own value before any OOM there.
    /// `Cell` because the OOM callback may run later on the same thread and
    /// observe a stale label if we replaced `static` with a non-`Copy` type.
    static HEAP_CAP_SOURCE: std::cell::Cell<HeapCapSource> = const { std::cell::Cell::new(HeapCapSource::Default) };
}

/// Resolved heap-cap configuration. The `Option<usize>` is the cap in MiB
/// (`None` -> V8 default, no cap). The [`source`] tag is the provenance, used
/// by the OOM banner so it attributes the cap to the user-set env var or the
/// built-in default truthfully -- re-reading the env at OOM time would lie
/// if the user mutated it after startup.
struct HeapCap {
    mb: Option<usize>,
    source: HeapCapSource,
}

/// Parse `OAM_MAX_HEAP_MB` into a hard V8 heap cap, in megabytes.
///
/// Node parity: this is oam's `--max-old-space-size` analogue. Resolution:
///   * unset or empty -> [`DEFAULT_HEAP_MB`] (4 GiB on 64-bit).
///   * `0` -> no cap (V8 default; explicit opt-out).
///   * non-numeric -> no cap, matching the pre-default behavior so a typo
///     never silently pins the heap to a tiny ceiling.
///   * `n > 0` -> n MB cap. A very small value is honored as-is; it trips
///     the OOM callback during startup, printing the clean banner rather
///     than aborting raw.
fn resolve_heap_cap() -> HeapCap {
    let raw = std::env::var("OAM_MAX_HEAP_MB").ok();
    let raw = match raw {
        Some(s) if !s.is_empty() => s,
        _ => {
            return HeapCap {
                mb: Some(DEFAULT_HEAP_MB),
                source: HeapCapSource::Default,
            };
        }
    };
    match raw.trim().parse::<usize>() {
        Ok(0) => HeapCap {
            mb: None,
            source: HeapCapSource::Default,
        },
        Ok(_mb) => HeapCap {
            mb: Some(raw.trim().parse().unwrap()),
            source: HeapCapSource::User,
        },
        Err(_) => HeapCap {
            mb: None,
            source: HeapCapSource::Default,
        },
    }
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
// SAFETY: a raw V8 `NearHeapLimitCallback` -- only V8 invokes it, with the exact
// C ABI signature; `_data` is the null pointer we registered and the heap-limit
// args are plain usizes (nothing is dereferenced). It exits the process and
// never returns to V8.
unsafe extern "C" fn near_heap_limit_oom(
    _data: *mut std::ffi::c_void,
    current_heap_limit: usize,
    _initial_heap_limit: usize,
) -> usize {
    use std::io::Write as _;
    let mb = current_heap_limit / (1024 * 1024);
    // Provenance comes from a thread-local set by `JsRuntime::new`, NOT from
    // re-reading OAM_MAX_HEAP_MB. The cap is captured into V8's resource
    // constraints at isolate creation, so re-reading the env here would lie
    // if the user mutated it after startup. The callback only fires on the
    // same thread that registered it (V8 invariant), so the thread-local is
    // the right scope.
    let source = HEAP_CAP_SOURCE.with(|s| match s.get() {
        HeapCapSource::User => "set by OAM_MAX_HEAP_MB",
        HeapCapSource::Default => "default 4 GiB",
    });
    let banner = format!(
        "error[OAM-RT-OOM]: JavaScript heap out of memory -- reached the {mb} MB cap ({source})\n"
    );
    let stderr = std::io::stderr();
    let mut lock = stderr.lock();
    let _ = lock.write_all(banner.as_bytes());
    let _ = lock.flush();
    oam_core::exit_process(134);
}

impl JsRuntime {
    pub fn new() -> Self {
        Self::new_with_permissions(None)
    }

    /// Create a new runtime with the given permission restrictions.
    /// `None` -> all permissions granted (same as `new()`).
    pub fn new_with_permissions(opts: Option<permissions::PermissionsOptions>) -> Self {
        // Top-level runtime: install the oam.fork() pre-warm pool.
        Self::new_inner(
            std::sync::Arc::new(permissions::Permissions::from_opts(opts)),
            true,
        )
    }

    /// A worker / fork isolate that INHERITS the spawning isolate's
    /// permissions.
    ///
    /// Without this a child ran all-granted no matter what the parent was
    /// launched with, so `new Worker(...)` (and `oam.fork()`) was a one-line
    /// escape from `--permission`: the child could read, write and spawn
    /// freely. Every path that creates a child isolate must use this and pass
    /// the parent's set down.
    pub(crate) fn new_worker_runtime_with(
        permissions: std::sync::Arc<permissions::Permissions>,
    ) -> Self {
        Self::new_inner(permissions, false)
    }

    fn new_inner(
        permissions: std::sync::Arc<permissions::Permissions>,
        with_fork_pool: bool,
    ) -> Self {
        init_platform();
        // OAM_MAX_HEAP_MB: hard cap on the V8 heap (Node's --max-old-space-size
        // analogue). Applied at isolate creation so EVERY isolate -- top-level,
        // `oam serve` workers, and fork-prewarm threads -- inherits the cap;
        // a runaway MCP sidecar can't grow the heap unbounded. The default
        // (4 GiB) is set by `resolve_heap_cap`; only `OAM_MAX_HEAP_MB=0` opts
        // out to the V8 default.
        let heap_cap = resolve_heap_cap();
        let heap_cap_mb = heap_cap.mb;
        // Publish the cap's provenance to the OOM callback before registering
        // it. V8 invokes the callback on the same thread that owns the
        // isolate, so the thread-local is the right scope -- a worker thread
        // building its own isolate writes its own value here.
        HEAP_CAP_SOURCE.with(|s| s.set(heap_cap.source));
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
            // The pool's pre-warmed isolates run user code via oam.fork(), so
            // they inherit this runtime's permissions too.
            isolate.set_slot(fork::ForkPool::new(2, permissions.clone()));
        }
        // Permissions slot: all-granted by default so existing code needs
        // no changes.  Restricted runtimes pass Some(PermissionsOptions{..}).
        isolate.set_slot(permissions);
        let context = {
            v8::scope!(let scope, &mut isolate);
            // Deserializes the snapshot's runtime context: bootstrap.js is
            // already evaluated in here — no JS parsing at startup. The
            // snapshot's DEFAULT context is a pristine one reserved for
            // `node:vm`, so this has to name its index rather than take the
            // default (see build.rs).
            let context =
                v8::Context::from_snapshot(scope, RUNTIME_CONTEXT_INDEX, Default::default())
                    .expect("snapshot carries the runtime context");
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

    /// Suppress 'beforeExit' emission for this runtime. The `oam test` path
    /// calls this before evaluating the test file: its module-eval pump
    /// shares the run path's drain seam, and emitting there would fire
    /// beforeExit after registration but BEFORE the tests execute
    /// (Node --test emits only after the tests).
    pub fn suppress_before_exit(&mut self) {
        self.isolate.set_slot(modules::SuppressBeforeExit);
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
        // The internal reference, not globalThis.process -- userland may have
        // replaced the latter (test-timers-process-tampering).
        let ref_key = v8::String::new(scope, "__oamProcessRef")?;
        let process = global
            .get(scope, ref_key.into())
            .filter(|v| v.is_object())
            .or_else(|| {
                let process_key = v8::String::new(scope, "process")?;
                global.get(scope, process_key.into())
            })?;
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
        // A throw from an 'exit' listener is fatal in Node: it reports the
        // error and exits 1, whatever the handler set exitCode to.
        // Inline the emit (no leaked global): touch process to build it if
        // lazy, guard on process._exiting so it fires once, emit 'exit' with the
        // current exitCode. Mirrors the JS-side emitProcessExit used by
        // process.exit(); process._exiting is the shared once-guard.
        // IIFE so the locals stay function-scoped: top-level `var` in a classic
        // script would leak onto globalThis (and trip Node common's leak check).
        if let Err(e) = self.execute_script(
            "<process-exit>",
            "(function () { \
               var p = globalThis.__oamProcessRef || globalThis.process; \
               if (p && !p._exiting) { \
                 p._exiting = true; \
                 var c = typeof p.exitCode === 'number' ? p.exitCode : 0; \
                 try { p.emit('exit', c); } catch (e) { \
                   p.exitCode = 1; \
                   try { console.error(globalThis.__oamFormatFatal(e)); } catch (_) {} \
                 } \
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

    // Node's exit path holds an internal reference to the real process
    // object, so `globalThis.process = {}` (which userland does -- see
    // test-timers-process-tampering) cannot break shutdown. Stash the same
    // reference here. Non-enumerable so Node's leaked-globals check ignores it.
    let process_key = v8::String::new(scope, "process").unwrap();
    if let Some(process) = context.global(scope).get(scope, process_key.into()) {
        let global = context.global(scope);
        let ref_key = v8::String::new(scope, "__oamProcessRef").unwrap();
        let mut desc = v8::PropertyDescriptor::new_from_value(process);
        desc.set_enumerable(false);
        desc.set_configurable(true);
        global.define_property(scope, ref_key.into(), &desc);
    }
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
    use std::sync::Mutex;

    // Env-var tests share a process-wide environment. Serialize them so
    // a parallel `cargo test` thread reading OAM_MAX_HEAP_MB doesn't see
    // a value another test just set. The lock is held for the duration of
    // the EnvGuard scope so the SET -- ASSERT -- DROP sequence is atomic
    // from any other test's perspective.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard for env-var mutations in tests. Sets `OAM_MAX_HEAP_MB` on
    /// construction and restores the prior value (or unsets it) on drop --
    /// including the panic-during-assertion case where a bare `remove_var`
    /// after the assert would be skipped by the unwinder. The guard holds
    /// `ENV_LOCK` for its lifetime so parallel tests can't observe a
    /// half-applied mutation.
    struct HeapCapEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prior: Option<String>,
    }

    impl HeapCapEnvGuard {
        fn set(value: &str) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prior = std::env::var("OAM_MAX_HEAP_MB").ok();
            // SAFETY: serialized by `_lock` above; no other thread reads this
            // env var until we drop.
            unsafe { std::env::set_var("OAM_MAX_HEAP_MB", value) };
            Self { _lock: lock, prior }
        }
        fn unset() -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prior = std::env::var("OAM_MAX_HEAP_MB").ok();
            // SAFETY: serialized by `_lock` above.
            unsafe { std::env::remove_var("OAM_MAX_HEAP_MB") };
            Self { _lock: lock, prior }
        }
    }

    impl Drop for HeapCapEnvGuard {
        fn drop(&mut self) {
            match self.prior.as_deref() {
                // SAFETY: serialized by `self._lock` (still held here), so no
                // other thread reads OAM_MAX_HEAP_MB concurrently.
                Some(v) => unsafe { std::env::set_var("OAM_MAX_HEAP_MB", v) },
                // SAFETY: as above -- `self._lock` is still held.
                None => unsafe { std::env::remove_var("OAM_MAX_HEAP_MB") },
            }
        }
    }

    #[test]
    fn heap_cap_defaults_to_4gib_when_unset() {
        let _g = HeapCapEnvGuard::unset();
        assert_eq!(
            resolve_heap_cap().mb,
            Some(4096),
            "unset OAM_MAX_HEAP_MB must default to 4 GiB; the pre-default \
             behavior (no cap) is what caused the 1.4 GiB mark-compact OOMs \
             in the crash log",
        );
        assert!(
            matches!(resolve_heap_cap().source, HeapCapSource::Default),
            "unset -> Default source",
        );
    }

    #[test]
    fn heap_cap_zero_opts_out() {
        let _g = HeapCapEnvGuard::set("0");
        assert_eq!(
            resolve_heap_cap().mb,
            None,
            "OAM_MAX_HEAP_MB=0 must mean no cap (V8 default); this is the \
             explicit opt-out for callers that genuinely want the heap unbounded",
        );
    }

    #[test]
    fn heap_cap_empty_string_falls_back_to_default() {
        let _g = HeapCapEnvGuard::set("");
        assert_eq!(
            resolve_heap_cap().mb,
            Some(4096),
            "an empty OAM_MAX_HEAP_MB is treated the same as unset",
        );
    }

    #[test]
    fn heap_cap_honors_explicit_value() {
        let _g = HeapCapEnvGuard::set("256");
        assert_eq!(resolve_heap_cap().mb, Some(256));
        assert!(
            matches!(resolve_heap_cap().source, HeapCapSource::User),
            "explicit positive value -> User source so the OOM banner attributes \
             the cap to OAM_MAX_HEAP_MB truthfully",
        );
    }

    #[test]
    fn heap_cap_garbage_value_opts_out() {
        let _g = HeapCapEnvGuard::set("not-a-number");
        assert_eq!(
            resolve_heap_cap().mb,
            None,
            "a non-numeric OAM_MAX_HEAP_MB opts out (typo-safe) rather than \
             silently pinning the heap to a tiny ceiling",
        );
    }

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

    // ---- soundness: fsReadSync bounds check must not wrap
    //
    // Regression for the overflow-bypassable bounds check in op_fs_read_sync.
    // `offset` comes from an unvalidated JS number; its saturating f64->usize
    // cast reaches usize::MAX, and the old `byte_offset + n <= len` check did
    // unchecked adds, so `usize::MAX + 1` wrapped to 0, passed the check, and
    // did an out-of-bounds copy_nonoverlapping (segfault in release, overflow
    // panic in debug). Reachable from ordinary user JS via `__oam.node`.

    #[test]
    fn fs_read_sync_rejects_wrapping_offset() {
        let path = temp_path("read-oob.bin");
        std::fs::write(&path, b"ABCDEFGH").unwrap();
        let js = js_path(&path);
        let mut rt = JsRuntime::new();
        // fd-based ops resolve their file registry through the CoreRuntime.
        rt.ensure_core_runtime();
        let result = rt
            .execute_script(
                "fs_read_oob.js",
                &format!(
                    r#"
                    const fd = __oam.node.fsOpenSync('{js}', 'r');
                    const buf = new Uint8Array(4);
                    // 1e30 saturates to usize::MAX in the cast; pre-fix this
                    // wrapped the bounds check into a wild OOB write.
                    const wrote = __oam.node.fsReadSync(fd, buf, 1e30, 1, 0);
                    const untouched = Array.from(buf).every((b) => b === 0);
                    // A normal read must still copy into the buffer.
                    const n = __oam.node.fsReadSync(fd, buf, 0, 4, 0);
                    __oam.node.fsCloseSync(fd);
                    `${{wrote}}:${{untouched}}:${{n}}:${{buf[0]}}`
                    "#
                ),
            )
            .unwrap();
        let _ = std::fs::remove_file(&path);
        // wrote=1 byte read; untouched=true (OOB copy suppressed); n=4 normal
        // read; buf[0]=65 ('A'). The zlibHandleWriteSync guard shares the same
        // checked_add fold and throws a RangeError on the wrapping case.
        assert_eq!(result, "1:true:4:65", "got: {result}");
    }

    // The read length is the SAME class of unvalidated JS number as the offset,
    // and feeds `vec![0u8; length]` a line before the guard -- pre-fix a huge
    // length aborted the process (capacity overflow) before any read happened.
    // The allocation is now clamped to the destination buffer's capacity.

    #[test]
    fn fs_read_sync_rejects_wrapping_length() {
        let path = temp_path("read-len-oob.bin");
        std::fs::write(&path, b"ABCDEFGH").unwrap();
        let js = js_path(&path);
        let mut rt = JsRuntime::new();
        rt.ensure_core_runtime();
        let result = rt
            .execute_script(
                "fs_read_len.js",
                &format!(
                    r#"
                    const fd = __oam.node.fsOpenSync('{js}', 'r');
                    const buf = new Uint8Array(4);
                    // 1e30 saturates to usize::MAX; pre-fix vec![0u8; length]
                    // aborted the process. Clamped to the buffer, this reads <=4.
                    const n = __oam.node.fsReadSync(fd, buf, 0, 1e30, 0);
                    __oam.node.fsCloseSync(fd);
                    `${{n}}:${{buf[0]}}`
                    "#
                ),
            )
            .unwrap();
        let _ = std::fs::remove_file(&path);
        // n=4 (length clamped to the 4-byte buffer), buf[0]=65 ('A').
        assert_eq!(result, "4:65", "got: {result}");
    }

    // zlibHandleWriteSync's out_len is an int32 (a negative value sign-extends
    // to ~usize::MAX) feeding `vec![0u8; out_len]` -- pre-fix that aborted the
    // process before the handle was used. The allocation is now clamped to the
    // output buffer's capacity, so the call proceeds normally.

    #[test]
    fn zlib_write_sync_rejects_negative_out_len() {
        let mut rt = JsRuntime::new();
        rt.ensure_core_runtime();
        let result = rt
            .execute_script(
                "zlib_out_len.js",
                r#"
                // mode 1 = deflate/compress, level -1 = default.
                const h = __oam.node.zlibHandleCreate(1, -1);
                const out = new Uint8Array(64);
                // outLen arg (-1) casts int32 -> usize::MAX; pre-fix the output
                // vec allocation aborted the process before the handle was used.
                const r = __oam.node.zlibHandleWriteSync(
                    h, 4, new Uint8Array([1, 2, 3, 4, 5]), out, 0, -1,
                );
                Array.isArray(r) ? ("ok:" + r.length) : ("bad:" + typeof r)
                "#,
            )
            .unwrap();
        // ok:2 -> the call returned [availOut, availIn] instead of aborting.
        assert_eq!(result, "ok:2", "got: {result}");
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
        assert_eq!(err, "ERR_ACCESS_DENIED", "got: {err}");
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
        assert_eq!(err, "ERR_ACCESS_DENIED", "got: {err}");
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
        assert_eq!(err, "ERR_ACCESS_DENIED", "got: {err}");
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
