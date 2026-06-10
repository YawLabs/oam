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
mod modules;
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
    isolate: v8::OwnedIsolate,
    context: v8::Global<v8::Context>,
}

impl JsRuntime {
    pub fn new() -> Self {
        init_platform();
        let params = v8::CreateParams::default().snapshot_blob(v8::StartupData::from(OAM_SNAPSHOT));
        let mut isolate = v8::Isolate::new(params);
        isolate.set_promise_reject_callback(modules::promise_reject_callback);
        isolate.add_message_listener(modules::message_listener);
        isolate.set_slot(timers::TimerQueue::default());
        isolate.set_slot(oam_core::CoreRuntime::new().expect("tokio runtime builds"));
        isolate.set_slot(ops::PendingOps::default());
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
            cjs::install(scope, context);
            global
        };
        Self { isolate, context }
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
            value
                .to_string(tc)
                .map(|s| s.to_rust_string_lossy(tc))
                .unwrap_or_default()
        };
        self.isolate.perform_microtask_checkpoint();
        Ok(result)
    }
}

impl Default for JsRuntime {
    fn default() -> Self {
        Self::new()
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

/// M0 console: log/info/debug -> stdout, warn/error -> stderr.
/// Replaced in M1 by the ODIF-aware console in snapshot JS.
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
}
