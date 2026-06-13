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
mod crypto_ops;
mod inspector;
mod modules;
pub mod napi;
mod node_ops;
mod ops;
mod timers;
pub use modules::ModuleHost;

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

impl JsRuntime {
    pub fn new() -> Self {
        init_platform();
        let params = v8::CreateParams::default().snapshot_blob(v8::StartupData::from(OAM_SNAPSHOT));
        let mut isolate = v8::Isolate::new(params);
        isolate.set_promise_reject_callback(modules::promise_reject_callback);
        isolate.add_message_listener(modules::message_listener);
        isolate.set_host_initialize_import_meta_object_callback(modules::import_meta_callback);
        isolate.set_host_import_module_dynamically_callback(modules::dynamic_import_callback);
        isolate.set_slot(timers::TimerQueue::default());
        isolate.set_slot(oam_core::CoreRuntime::new().expect("tokio runtime builds"));
        isolate.set_slot(ops::PendingOps::default());
        isolate.set_slot(crypto_ops::CryptoState::default());
        isolate.set_slot(napi::AddonRegistry::new());
        let context = {
            v8::scope!(let scope, &mut isolate);
            // Deserializes the snapshot's default context: bootstrap.js is
            // already evaluated in here — no JS parsing at startup.
            let context = v8::Context::new(scope, v8::ContextOptions::default());
            let global = v8::Global::new(scope, context);
            let scope = &mut v8::ContextScope::new(scope, context);
            install_console(scope, context);
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

    /// Read `process.exitCode` after a completed run — Node honors it at
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

    /// Compile and run `source` as a classic script. Returns the stringified
    /// completion value. Exceptions come back as `Err` with script:line info.
    pub fn execute_script(&mut self, name: &str, source: &str) -> Result<String> {
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
            tc.perform_microtask_checkpoint();
            if tc.has_caught() {
                return Err(exception_to_error(tc, name));
            }
            result_str
        };
        Ok(result)
    }
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

/// M0 console: log/info/debug -> stdout, warn/error -> stderr. Replaced at
/// JsRuntime::new time by node_compat's util.inspect console; kept as the
/// fallback surface during snapshot bring-up.
fn install_console(scope: &mut v8::PinScope<'_, '_>, context: v8::Local<v8::Context>) {
    let global = context.global(scope);
    let console = v8::Object::new(scope);

    let log = v8::Function::new(scope, console_log_stdout).unwrap();
    let err = v8::Function::new(scope, console_log_stderr).unwrap();

    for key in ["log", "info", "debug"] {
        let key_v8 = v8::String::new(scope, key).unwrap();
        console.set(scope, key_v8.into(), log.into());
    }
    for key in ["warn", "error"] {
        let key_v8 = v8::String::new(scope, key).unwrap();
        console.set(scope, key_v8.into(), err.into());
    }

    let console_key = v8::String::new(scope, "console").unwrap();
    global.set(scope, console_key.into(), console.into());
}

fn format_args(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments<'_>,
) -> String {
    let mut parts = Vec::with_capacity(args.length() as usize);
    for i in 0..args.length() {
        let part = args
            .get(i)
            .to_string(scope)
            .map(|s| s.to_rust_string_lossy(scope))
            .unwrap_or_default();
        parts.push(part);
    }
    parts.join(" ")
}

fn console_log_stdout(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    println!("{}", format_args(scope, &args));
}

fn console_log_stderr(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    eprintln!("{}", format_args(scope, &args));
}

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
}
