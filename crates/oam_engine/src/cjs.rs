//! CommonJS interop: the bridge that makes the installed npm ecosystem run.
//!
//! Execution model: CJS modules are compiled with V8's CompileFunction
//! (real line numbers, no textual wrapper) against the Node parameter list
//! plus `global` — (exports, require, module, __filename, __dirname,
//! global) — and executed EAGERLY: `require()` is synchronous by
//! definition, and when the ESM graph loader hits a CJS file it executes
//! it at load time, then reflects the ACTUAL runtime keys of
//! `module.exports` as named exports through a generated ESM facade. ES2022
//! arbitrary module-namespace names (`export { v as "any-string" }`) make
//! the facade fully general — no cjs-module-lexer heuristics, no missed
//! named exports.
//!
//! Documented divergences from Node:
//! - `import` of a CJS module executes it during graph LOAD, not at
//!   evaluation order position. Observable only via side-effect ordering
//!   between sibling imports; require-time semantics inside the CJS world
//!   are unchanged.
//! - `__esModule` is honored (Bun-compatible): a transpiled-ESM CJS module
//!   with `exports.__esModule = true` gets `exports.default` as its ESM
//!   default import instead of the exports object. This is what the
//!   TypeScript/Babel ecosystem expects; Node's lexer only special-cases a
//!   subset.
//!
//! Cycle semantics are Node's: the module object is cached BEFORE its body
//! runs, so a cycle sees partial exports. A module whose body throws is
//! evicted from the cache (Node retries on the next require).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::modules::module_key;

#[derive(Default)]
pub(crate) struct CjsCache {
    /// module-key -> the CJS `module` object ({exports, filename, id,
    /// loaded}). The exports property is re-read on every hit so
    /// `module.exports =` reassignment and mid-cycle partials both behave.
    modules: HashMap<PathBuf, v8::Global<v8::Object>>,
}

/// Add the internal facade hook to the existing `__oam` object (created by
/// ops::install — call order in JsRuntime::new matters).
pub(crate) fn install(scope: &mut v8::PinScope<'_, '_>, context: v8::Local<v8::Context>) {
    let global = context.global(scope);
    let internal_key = v8::String::new(scope, "__oam").unwrap();
    let internal = global
        .get(scope, internal_key.into())
        .expect("__oam installed by ops::install");
    let internal = v8::Local::<v8::Object>::try_from(internal).expect("__oam is an object");
    let key = v8::String::new(scope, "requireCjs").unwrap();
    let func = v8::Function::new(scope, op_require_cjs).unwrap();
    internal.set(scope, key.into(), func.into());
}

fn throw_error(scope: &mut v8::PinScope<'_, '_>, message: &str) {
    let message = v8::String::new(scope, message)
        .unwrap_or_else(|| v8::String::new(scope, "oam: error message too long").unwrap());
    let exception = v8::Exception::error(scope, message);
    scope.throw_exception(exception);
}

/// Throw an Error carrying Node's `.code` — the ecosystem branches on
/// MODULE_NOT_FOUND / ERR_REQUIRE_ESM, not on message text.
fn throw_error_with_code(scope: &mut v8::PinScope<'_, '_>, code: &str, message: &str) {
    let message =
        v8::String::new(scope, message).unwrap_or_else(|| v8::String::new(scope, code).unwrap());
    let exception = v8::Exception::error(scope, message);
    if let Ok(obj) = v8::Local::<v8::Object>::try_from(exception) {
        let key = v8::String::new(scope, "code").unwrap();
        if let Some(value) = v8::String::new(scope, code) {
            obj.set(scope, key.into(), value.into());
        }
    }
    scope.throw_exception(exception);
}

/// CJS wrapper parameters, in order. Shared between the require() loader and
/// the `oam compile` bytecode producer so their compiled functions -- and thus
/// their V8 code-cache blobs -- are interchangeable.
pub(crate) const CJS_PARAMS: [&str; 6] = [
    "exports",
    "require",
    "module",
    "__filename",
    "__dirname",
    "global",
];

/// Replace a leading `#!` shebang with a `//` comment, byte-for-byte so every
/// source offset is preserved. Shared so the loader, the bytecode producer, and
/// the embedded-seed key all transform the source identically -- a mismatch
/// would silently miss the cache, never miscompile.
pub(crate) fn strip_shebang(source: String) -> String {
    if let Some(rest) = source.strip_prefix("#!") {
        format!("//{rest}")
    } else {
        source
    }
}

/// Compile `source` as a CJS wrapper function and return its V8 bytecode blob,
/// WITHOUT executing it. Used by `oam compile` to embed bytecode so a compiled
/// binary's first run skips parse+compile (even with no writable cache dir).
/// The compile mirrors the require() loader -- same `CJS_PARAMS`, same origin
/// shape, same shebang transform -- so the runtime consume path accepts the
/// blob; a rejected blob would only trigger a recompile, never a wrong result.
/// Returns None on a syntax error or if V8 declines to produce a cache.
pub(crate) fn produce_cjs_code_cache(
    scope: &mut v8::PinScope<'_, '_>,
    source: &str,
) -> Option<Vec<u8>> {
    let source = strip_shebang(source.to_string());
    let source_v8 = v8::String::new(scope, &source)?;
    let name: v8::Local<v8::Value> = v8::String::new(scope, "<oam-compile>")?.into();
    let origin = v8::ScriptOrigin::new(
        scope, name, 0, 0, false, 0, None, false, false, /* is_module */ false, None,
    );
    let mut compiler_source = v8::script_compiler::Source::new(source_v8, Some(&origin));
    let params: Vec<v8::Local<v8::String>> = CJS_PARAMS
        .iter()
        .map(|p| v8::String::new(scope, p).unwrap())
        .collect();
    let wrapper = v8::script_compiler::compile_function(
        scope,
        &mut compiler_source,
        &params,
        &[],
        v8::script_compiler::CompileOptions::NoCompileOptions,
        v8::script_compiler::NoCacheReason::NoReason,
    )?;
    wrapper.create_code_cache().map(|cd| cd.to_vec())
}

/// `__oam.requireCjs(path)`: pure cache lookup used by generated ESM
/// facades. The module was executed during graph load; a miss here is an
/// internal invariant violation, not a user error.
fn op_require_cjs(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = args.get(0).to_string(scope) else {
        throw_error(scope, "oam internal: requireCjs needs a path");
        return;
    };
    let key = PathBuf::from(path.to_rust_string_lossy(scope));
    let cached = scope
        .get_slot::<CjsCache>()
        .and_then(|cache| cache.modules.get(&key).cloned());
    match cached {
        Some(module) => {
            let module = v8::Local::new(scope, &module);
            if let Some(exports) = module_exports(scope, module) {
                rv.set(exports);
            }
        }
        None => throw_error(
            scope,
            &format!(
                "oam internal: CJS module not preloaded for facade: {}",
                key.display()
            ),
        ),
    }
}

fn module_exports<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    module: v8::Local<v8::Object>,
) -> Option<v8::Local<'s, v8::Value>> {
    let key = v8::String::new(scope, "exports")?;
    module.get(scope, key.into())
}

/// The `require` implementation. Each CJS module gets its own Function
/// instance whose `data` is the requiring file's path (zero-capture
/// callbacks: per-module state must travel through V8, not Rust closures).
fn require_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(specifier) = args.get(0).to_string(scope) else {
        let message = v8::String::new(scope, "require expects a specifier string").unwrap();
        let exception = v8::Exception::type_error(scope, message);
        scope.throw_exception(exception);
        return;
    };
    let specifier = specifier.to_rust_string_lossy(scope);
    let referrer = PathBuf::from(
        args.data()
            .to_string(scope)
            .map(|s| s.to_rust_string_lossy(scope))
            .unwrap_or_default(),
    );

    let path = match oam_loader::resolve_require(&specifier, &referrer) {
        Ok(path) => path,
        Err(failure) => {
            // Node's MODULE_NOT_FOUND (a runtime Error, not ODIF) — the
            // diagnostic message carries the precise reason, the code
            // carries what optional-dependency try/catch blocks test.
            throw_error_with_code(scope, "MODULE_NOT_FOUND", &failure.message);
            return;
        }
    };
    // Virtual modules (node:NAME stripped, oam:NAME full) come from the
    // snapshot registry; the registry object is the require() result —
    // same instance the ESM facade exposes.
    let registry_key = path.to_str().and_then(|s| {
        s.strip_prefix("node:")
            .map(str::to_string)
            .or_else(|| s.starts_with("oam:").then(|| s.to_string()))
    });
    if let Some(name) = registry_key {
        if let Some(builtin) = get_builtin(scope, &name) {
            rv.set(builtin);
        }
        return;
    }
    let is_json = path.extension().and_then(|e| e.to_str()) == Some("json");
    if !is_json && oam_loader::module_kind(&path) == oam_loader::ModuleKind::Esm {
        throw_error_with_code(
            scope,
            "ERR_REQUIRE_ESM",
            &format!(
                "[ERR_REQUIRE_ESM] require() of ES module {} from {} is not supported yet — \
                 import it instead (synchronous require(esm) is on the M2 roadmap)",
                path.display(),
                referrer.display()
            ),
        );
        return;
    }
    if let Some(exports) = load_cjs(scope, &path) {
        rv.set(exports);
    }
}

/// Load (or return cached) the CJS module at `path` and return its
/// `module.exports`. `None` means a JS exception is pending — callers in
/// callback position just return; the graph loader converts it via its
/// TryCatch. Re-entrant: module bodies call require, which lands back here
/// on the same stack.
pub(crate) fn load_cjs<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    path: &Path,
) -> Option<v8::Local<'s, v8::Value>> {
    let key = match module_key(path) {
        Ok(key) => key,
        Err(e) => {
            throw_error(scope, &format!("bad module path {}: {e}", path.display()));
            return None;
        }
    };
    let file = key.to_string_lossy().into_owned();

    let cached = scope
        .get_slot::<CjsCache>()
        .and_then(|cache| cache.modules.get(&key).cloned());
    if let Some(module) = cached {
        let module = v8::Local::new(scope, &module);
        return module_exports(scope, module);
    }

    // N-API addons: dlopen + register, cached like any CJS module.
    if key.extension().and_then(|e| e.to_str()) == Some("node") {
        // Native .node loading is OFF by default. A Node-built addon (compiled
        // against node.exe) can DEADLOCK the OS loader when dlopen'd into oam
        // (its load-time static initializers run under the Windows loader lock
        // and call back into the host) -- an unkillable hang BEFORE any oam code
        // runs. That hang defeats the universal `try { require(native) } catch {
        // pure-JS fallback }` pattern (ssh2/cpu-features, bufferutil, bcrypt,
        // ...). Throwing a clean, catchable error instead lets every such
        // fallback work. oam's own N-API alpha (compatible addons) is available
        // via OAM_ENABLE_NATIVE_ADDONS=1.
        if std::env::var_os("OAM_ENABLE_NATIVE_ADDONS").is_none() {
            throw_error_with_code(
                scope,
                "OAM-NATIVE0001",
                &format!(
                    "cannot load native addon {file}: native .node addons are disabled by default \
                     (a Node-built addon can deadlock the loader). Set OAM_ENABLE_NATIVE_ADDONS=1 \
                     to enable oam's N-API support (alpha)."
                ),
            );
            return None;
        }
        let exports_value = crate::napi::load_addon(scope, &key)?;
        let module = v8::Object::new(scope);
        let exports_key = v8::String::new(scope, "exports")?;
        module.set(scope, exports_key.into(), exports_value);
        let global = v8::Global::new(scope, module);
        scope
            .get_slot_mut::<CjsCache>()
            .expect("cjs cache installed")
            .modules
            .insert(key, global);
        return Some(exports_value);
    }

    // TypeScript reached via require() (.ts/.cts/.mts): read the source, then
    // consult the install-time pre-compilation cache (written by
    // `oam install --precompile`, keyed by source hash). A cache hit skips the
    // oxc transpile; a miss strips types inline below, mirroring the ESM host.
    let ext = key.extension().and_then(|e| e.to_str()).unwrap_or("");
    let is_ts = matches!(ext, "ts" | "cts" | "mts");

    let raw = match std::fs::read_to_string(&key) {
        Ok(source) => source,
        Err(e) => {
            throw_error(scope, &format!("cannot read {file}: {e}"));
            return None;
        }
    };

    let source = if is_ts {
        if let Some(cached) = oam_loader::precompile::try_precompile_cache(&key, &raw) {
            // Already plain JS from `oam install --precompile`; do NOT re-transpile.
            cached
        } else {
            // oxc strips the TS types, then the CJS wrapper below compiles+runs
            // the resulting JS. A transpile failure carries the loader's ODIF
            // code/message rather than a raw oxc panic; any ESM syntax that
            // survives stripping (e.g. a bare `export`) fails later at
            // compile_function and surfaces as the V8 SyntaxError.
            match oam_loader::transpile_typescript(&key, &raw) {
                Ok(js) => js,
                Err(e) => {
                    let (code, message) = e
                        .diagnostics
                        .first()
                        .map(|d| (d.code.clone(), d.message.clone()))
                        .unwrap_or_else(|| {
                            (
                                "OAM-PARSE0002".to_string(),
                                format!("{file}: transpile failed"),
                            )
                        });
                    throw_error_with_code(scope, &code, &message);
                    return None;
                }
            }
        }
    } else {
        raw
    };

    // The module object, cached BEFORE execution so cycles see partials.
    let module = v8::Object::new(scope);
    let filename_v8 = v8::String::new(scope, &file)?;
    for prop in ["filename", "id"] {
        let prop_key = v8::String::new(scope, prop)?;
        module.set(scope, prop_key.into(), filename_v8.into());
    }
    let loaded_key = v8::String::new(scope, "loaded")?;
    let false_v8 = v8::Boolean::new(scope, false);
    module.set(scope, loaded_key.into(), false_v8.into());

    if key.extension().and_then(|e| e.to_str()) == Some("json") {
        // BOM strip, like Node's JSON loader — PowerShell's Set-Content
        // writes BOMs and JSON.parse rejects them.
        let source = source.trim_start_matches('\u{feff}');
        let text = v8::String::new(scope, source)?;
        let parsed = v8::json::parse(scope, text)?; // None: SyntaxError pending
        let exports_key = v8::String::new(scope, "exports")?;
        module.set(scope, exports_key.into(), parsed);
        let global = v8::Global::new(scope, module);
        scope
            .get_slot_mut::<CjsCache>()
            .expect("cjs cache installed")
            .modules
            .insert(key, global);
        return Some(parsed);
    }

    let exports = v8::Object::new(scope);
    let exports_key = v8::String::new(scope, "exports")?;
    module.set(scope, exports_key.into(), exports.into());
    {
        let global = v8::Global::new(scope, module);
        scope
            .get_slot_mut::<CjsCache>()
            .expect("cjs cache installed")
            .modules
            .insert(key.clone(), global);
    }

    // Shebang: byte-for-byte replacement keeps every offset intact. Shared
    // with produce_cjs_code_cache so the cache key matches at compile time.
    let source = strip_shebang(source);

    let source_v8 = match v8::String::new(scope, &source) {
        Some(source_v8) => source_v8,
        None => {
            evict(scope, &key);
            throw_error(scope, &format!("{file}: source too long for V8 string"));
            return None;
        }
    };
    let name: v8::Local<v8::Value> = filename_v8.into();
    let origin = v8::ScriptOrigin::new(
        scope, name, 0, 0, false, 0, None, false, false, /* is_module */ false, None,
    );
    // V8 bytecode cache: consume a stored blob for this exact source if one
    // exists (skips the parse+compile) else compile fresh. The blob is keyed by
    // source + V8 version so a foreign blob can't be served here; V8 also flags
    // a bad blob via `rejected()` below. `cached_blob` is held for the whole
    // compile because `CachedData` borrows it (BufferNotOwned).
    let cached_blob = crate::code_cache::load(&source, crate::code_cache::Kind::Function);
    let consuming = cached_blob.is_some();
    let mut compiler_source = match cached_blob {
        Some(ref blob) => v8::script_compiler::Source::new_with_cached_data(
            source_v8,
            Some(&origin),
            v8::script_compiler::CachedData::new(blob),
        ),
        None => v8::script_compiler::Source::new(source_v8, Some(&origin)),
    };
    let params: Vec<v8::Local<v8::String>> = CJS_PARAMS
        .iter()
        .map(|p| v8::String::new(scope, p).unwrap())
        .collect();
    let compile_options = if consuming {
        v8::script_compiler::CompileOptions::ConsumeCodeCache
    } else {
        v8::script_compiler::CompileOptions::NoCompileOptions
    };
    let Some(wrapper) = v8::script_compiler::compile_function(
        scope,
        &mut compiler_source,
        &params,
        &[],
        compile_options,
        v8::script_compiler::NoCacheReason::NoReason,
    ) else {
        // Syntax error is pending; a broken module must not stay cached.
        evict(scope, &key);
        return None;
    };

    // (Re)write the cache after the body runs (below) on a miss, or if V8
    // rejected the consumed blob (stale/foreign) and recompiled. Producing
    // AFTER the call captures the body bytecode, not just the outer wrapper.
    let refresh_cache = !consuming
        || compiler_source
            .get_cached_data()
            .map(|cd| cd.rejected())
            .unwrap_or(true);

    let require = make_require(scope, &file)?;
    let dirname = key
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dirname_v8 = v8::String::new(scope, &dirname)?;
    let context = scope.get_current_context();
    let global_obj = context.global(scope);

    let recv: v8::Local<v8::Value> = exports.into();
    let call_args: [v8::Local<v8::Value>; 6] = [
        exports.into(),
        require.into(),
        module.into(),
        filename_v8.into(),
        dirname_v8.into(),
        global_obj.into(),
    ];
    if wrapper.call(scope, recv, &call_args).is_none() {
        // The body threw: exception stays pending, cache entry goes (Node
        // re-executes on the next require).
        evict(scope, &key);
        return None;
    }

    // Persist the bytecode now that the body has executed, so the cached blob
    // includes the body and not just the outer wrapper. Best-effort: a failed
    // produce or write just means the next run pays the compile again. The
    // enabled() guard skips the serialize work entirely when the cache is off.
    if refresh_cache
        && crate::code_cache::enabled()
        && let Some(cd) = wrapper.create_code_cache()
    {
        crate::code_cache::store(&source, crate::code_cache::Kind::Function, &cd);
    }

    let loaded_key = v8::String::new(scope, "loaded")?;
    let true_v8 = v8::Boolean::new(scope, true);
    module.set(scope, loaded_key.into(), true_v8.into());

    // Re-read: `module.exports = ...` reassignment is the common idiom.
    module_exports(scope, module)
}

fn evict(scope: &mut v8::PinScope<'_, '_>, key: &Path) {
    if let Some(cache) = scope.get_slot_mut::<CjsCache>() {
        cache.modules.remove(key);
    }
}

/// Build a `require` function bound to `filename` (each CJS module gets
/// one; node:module's createRequire hands them to ESM callers too).
pub(crate) fn make_require<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    filename: &str,
) -> Option<v8::Local<'s, v8::Function>> {
    let data = v8::String::new(scope, filename)?;
    v8::Function::builder(require_callback)
        .data(data.into())
        .build(scope)
}

/// Instantiate (or fetch cached) the node builtin `name` via the snapshot
/// registry. None means a JS exception is pending.
pub(crate) fn get_builtin<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    name: &str,
) -> Option<v8::Local<'s, v8::Value>> {
    let context = scope.get_current_context();
    let global = context.global(scope);
    let registry_key = v8::String::new(scope, "__oamNode")?;
    let registry = global.get(scope, registry_key.into())?;
    let Ok(registry) = v8::Local::<v8::Object>::try_from(registry) else {
        throw_error(scope, "oam internal: __oamNode registry missing");
        return None;
    };
    let get_key = v8::String::new(scope, "get")?;
    let get = registry.get(scope, get_key.into())?;
    let Ok(get) = v8::Local::<v8::Function>::try_from(get) else {
        throw_error(scope, "oam internal: __oamNode.get missing");
        return None;
    };
    let name_v8 = v8::String::new(scope, name)?;
    let recv: v8::Local<v8::Value> = registry.into();
    get.call(scope, recv, &[name_v8.into()])
}

/// Generate the ESM facade for an already-executed CJS module. Named
/// exports are the exports object's own enumerable string keys — exact,
/// because the module already ran. `default` follows __esModule (see
/// module docs).
pub(crate) fn facade_source(
    scope: &mut v8::PinScope<'_, '_>,
    key: &Path,
    exports: v8::Local<v8::Value>,
) -> String {
    let path_literal = serde_json::to_string(&key.to_string_lossy()).expect("path serializes");
    let prelude = format!(
        "const m = __oam.requireCjs({path_literal});\n\
         export default (m !== null && (typeof m === \"object\" || typeof m === \"function\") \
         && m.__esModule) ? m[\"default\"] : m;\n"
    );
    facade_with_prelude(scope, prelude, exports)
}

/// Generate the ESM facade for a node builtin. Default is the module
/// object itself (Node parity: `import fs from 'node:fs'` is the
/// namespace), named exports are its runtime keys.
pub(crate) fn builtin_facade_source(
    scope: &mut v8::PinScope<'_, '_>,
    name: &str,
    exports: v8::Local<v8::Value>,
) -> String {
    let name_literal = serde_json::to_string(name).expect("name serializes");
    let prelude = format!("const m = __oamNode.get({name_literal});\nexport default m;\n");
    facade_with_prelude(scope, prelude, exports)
}

fn facade_with_prelude(
    scope: &mut v8::PinScope<'_, '_>,
    prelude: String,
    exports: v8::Local<v8::Value>,
) -> String {
    let mut out = prelude;
    let Ok(exports_obj) = v8::Local::<v8::Object>::try_from(exports) else {
        return out; // primitive module.exports: default-only facade
    };
    let names = exports_obj.get_own_property_names(
        scope,
        v8::GetPropertyNamesArgsBuilder::new()
            .mode(v8::KeyCollectionMode::OwnOnly)
            .property_filter(v8::PropertyFilter::ONLY_ENUMERABLE | v8::PropertyFilter::SKIP_SYMBOLS)
            .key_conversion(v8::KeyConversionMode::ConvertToString)
            .build(),
    );
    let Some(names) = names else { return out };

    for i in 0..names.length() {
        let Some(name) = names.get_index(scope, i) else {
            continue;
        };
        let Some(name) = name.to_string(scope) else {
            continue;
        };
        let name = name.to_rust_string_lossy(scope);
        if name == "default" || name == "__esModule" {
            continue;
        }
        let literal = serde_json::to_string(&name).expect("key serializes");
        // ES2022 arbitrary module namespace names: any key string is a
        // legal export name when spelled as a string literal.
        out.push_str(&format!(
            "const $cjs{i} = m[{literal}]; export {{ $cjs{i} as {literal} }};\n"
        ));
    }
    out
}

/// Run a CJS entry file as the program: execute it, then pump the event
/// loop (timers/ops keep the process alive, Node semantics) and fail on
/// unhandled rejections, exactly like the ESM path.
impl crate::JsRuntime {
    pub fn execute_cjs(&mut self, entry: &Path) -> Result<(), Vec<oam_diagnostics::Diagnostic>> {
        self.reset_run_slots()?;

        // --inspect-brk on a CJS entry: cloned out before the &mut isolate
        // borrow. (The ESM path lives in execute_module.)
        let wait = self
            .inspector
            .as_ref()
            .filter(|state| state.break_on_start())
            .map(|state| state.shared());

        v8::scope_with_context!(let scope, &mut self.isolate, &self.context);
        v8::tc_scope!(let tc, scope);

        // load_cjs runs the entry body directly (no separate graph/evaluate),
        // so attach and arm break-on-start right before it: the pause lands on
        // the entry's first statement.
        if let Some(shared) = &wait {
            shared.wait_for_attach();
            shared.arm_break();
        }

        if load_cjs(tc, entry).is_none() {
            return Err(vec![crate::modules::catch_to_diagnostic(
                tc,
                &entry.to_string_lossy(),
            )]);
        }
        tc.perform_microtask_checkpoint();
        if let Some(failure) = crate::modules::drain_uncaught(tc) {
            return Err(failure);
        }
        crate::modules::pump_event_loop(tc, None)?;
        match crate::modules::unhandled_rejection_failures(tc) {
            Some(failures) => Err(failures),
            None => Ok(()),
        }
    }
}
