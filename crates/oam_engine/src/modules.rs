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

use oam_diagnostics::{Diagnostic, Origin, Severity};
use std::collections::HashMap;
use std::num::NonZeroI32;
use std::path::{Path, PathBuf};

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
    paths_by_hash: HashMap<NonZeroI32, PathBuf>,
    /// (referrer path, specifier as written) -> resolved path, recorded at
    /// preload so the V8 resolve callback is a pure lookup.
    edges: HashMap<(PathBuf, String), PathBuf>,
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
        let entry = std::path::absolute(entry).map_err(|e| {
            rt_diag(
                "OAM-RT0002",
                format!("bad entry path {}: {e}", entry.display()),
            )
        })?;
        self.isolate.set_slot(ModuleMap::default());

        v8::scope_with_context!(let scope, &mut self.isolate, &self.context);
        v8::tc_scope!(let tc, scope);

        load_module_tree(tc, host, &entry)?;

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

        let promise = v8::Local::<v8::Promise>::try_from(value)
            .map_err(|_| rt_diag("OAM-RT0001", "module evaluation did not return a promise"))?;
        match promise.state() {
            v8::PromiseState::Fulfilled => Ok(()),
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
                "top-level await did not settle: pending timers/IO require the event loop (M1, oam_core)",
            )),
        }
    }
}

/// Recursively compile the module graph into the isolate's ModuleMap.
fn load_module_tree(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    host: &dyn ModuleHost,
    path: &PathBuf,
) -> Result<(), Vec<Diagnostic>> {
    if tc
        .get_slot::<ModuleMap>()
        .expect("module map installed")
        .by_path
        .contains_key(path)
    {
        return Ok(());
    }

    let code = host.load(path)?;
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
        map.paths_by_hash.insert(hash, path.clone());
    }

    let requests = module.get_module_requests();
    let mut children = Vec::new();
    for i in 0..requests.length() {
        let request = requests.get(tc, i).expect("module request index in range");
        let request = v8::Local::<v8::ModuleRequest>::try_from(request)
            .expect("module request entry is a ModuleRequest");
        let specifier = request.get_specifier().to_rust_string_lossy(tc);
        let resolved = host.resolve(&specifier, path)?;
        let resolved = std::path::absolute(&resolved).unwrap_or(resolved);
        tc.get_slot_mut::<ModuleMap>()
            .expect("module map installed")
            .edges
            .insert((path.clone(), specifier), resolved.clone());
        children.push(resolved);
    }
    for child in children {
        load_module_tree(tc, host, &child)?;
    }
    Ok(())
}

/// V8 resolve callback: pure lookup of preload-recorded edges. Zero capture.
fn resolve_module_callback<'s>(
    context: v8::Local<'s, v8::Context>,
    specifier: v8::Local<'s, v8::String>,
    _import_attributes: v8::Local<'s, v8::FixedArray>,
    referrer: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Module>> {
    v8::callback_scope!(unsafe scope, context);
    let spec = specifier.to_rust_string_lossy(scope);
    let hash = referrer.get_identity_hash();
    let module = {
        let map = scope.get_slot::<ModuleMap>()?;
        let referrer_path = map.paths_by_hash.get(&hash)?;
        let resolved = map.edges.get(&(referrer_path.clone(), spec))?;
        map.by_path.get(resolved)?.clone()
    };
    Some(v8::Local::new(scope, &module))
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
