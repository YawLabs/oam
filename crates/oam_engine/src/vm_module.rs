//! `vm.SourceTextModule` -- real V8 modules for `node:vm`.
//!
//! A `v8::Module` is `Data`, not `Value`, so unlike a vm context it cannot be
//! stashed on a JS object and left to the GC. It lives in an isolate slot
//! keyed by an id, and the JS wrapper releases its entry through a
//! FinalizationRegistry (see the `vm` factory in js/node_compat.js) -- without
//! that, every module ever compiled would be pinned for the life of the
//! isolate.
//!
//! ## Why linking is split in two
//!
//! V8 instantiates synchronously: its resolve callback must hand back a
//! `v8::Module` immediately, with no chance to await. Node's linker may return
//! a promise. So `link()` resolves the whole graph in JS first, records the
//! answers here, and only then instantiates -- at which point the callback is
//! a lookup that cannot fail for a reason the caller could have fixed. Same
//! shape node uses.

use std::collections::HashMap;

/// A module plus the context it belongs to.
///
/// The context is not decoration: a module compiles, instantiates and
/// evaluates in ONE context, and it must be the one the caller asked for.
/// Accepting `{ context }` and then working in the host context would let a
/// module that believes it is sandboxed write to the real global.
struct ModuleEntry {
    module: v8::Global<v8::Module>,
    /// `None` means the host context.
    context: Option<v8::Global<v8::Context>>,
}

/// Modules created by `new vm.SourceTextModule`, by the id handed to JS.
#[derive(Default)]
pub(crate) struct VmModules {
    by_id: HashMap<u32, ModuleEntry>,
    /// Identity hash -> ids. A hash can collide, so the resolve callback
    /// narrows the candidates by comparing module identity.
    ids_by_hash: HashMap<i32, Vec<u32>>,
    /// (referrer id, specifier) -> resolved id, filled by `link()` before
    /// instantiation and read by the resolve callback.
    links: HashMap<(u32, String), u32>,
    next_id: u32,
}

impl VmModules {
    fn insert(
        &mut self,
        hash: i32,
        module: v8::Global<v8::Module>,
        context: Option<v8::Global<v8::Context>>,
    ) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.by_id.insert(id, ModuleEntry { module, context });
        self.ids_by_hash.entry(hash).or_default().push(id);
        id
    }
}

/// Runs `f` with the module, in the module's OWN context.
///
/// `f` returns a Global because anything minted inside the entered context
/// would dangle once the inner scope closes.
fn with_modules<T: 'static>(
    scope: &mut v8::PinScope<'_, '_>,
    id: u32,
    f: impl for<'i> FnOnce(
        &mut v8::PinScope<'i, '_>,
        v8::Local<'i, v8::Module>,
    ) -> Option<v8::Global<T>>,
) -> Option<Option<v8::Global<T>>> {
    // Globals are cloned out before any Local is made, so the slot borrow
    // never overlaps use of the scope.
    let (module, context) = {
        let entry = scope.get_slot::<VmModules>()?.by_id.get(&id)?;
        (entry.module.clone(), entry.context.clone())
    };
    match context {
        Some(context) => {
            let context = v8::Local::new(scope, &context);
            v8::scope_with_context!(let inner, scope, context);
            let module = v8::Local::new(inner, &module);
            Some(f(inner, module))
        }
        None => {
            let module = v8::Local::new(scope, &module);
            Some(f(scope, module))
        }
    }
}

fn arg_u32(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments<'_>,
    i: i32,
) -> u32 {
    args.get(i).uint32_value(scope).unwrap_or(0)
}

fn throw_unknown_module(scope: &mut v8::PinScope<'_, '_>) {
    let message = v8::String::new(scope, "vm module handle is no longer valid").unwrap();
    let error = v8::Exception::error(scope, message);
    scope.throw_exception(error);
}

/// `op_vm_module_compile(source, identifier) -> id`
pub(crate) fn op_vm_module_compile<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Ok(source_text) = v8::Local::<v8::String>::try_from(args.get(0)) else {
        let message = v8::String::new(scope, "code must be a string").unwrap();
        let error = v8::Exception::type_error(scope, message);
        scope.throw_exception(error);
        return;
    };
    // The context the caller named, if any. Compiling elsewhere and then
    // calling the module part of that context is the isolation bug this
    // whole option exists to avoid.
    let requested_context = v8::Local::<v8::Object>::try_from(args.get(2))
        .ok()
        .and_then(|sandbox| crate::vm_context::context_of_sandbox(scope, sandbox));
    let identifier = args.get(1);
    let origin = v8::ScriptOrigin::new(
        scope, identifier, 0, 0, false, 0, None, false, false,
        // is_module -- without it V8 compiles a classic script and every
        // import/export is a syntax error.
        true, None,
    );
    let context_global = requested_context.map(|context| v8::Global::new(scope, context));
    // No TryCatch either way: a syntax error is left pending, which is what
    // should reach `new SourceTextModule(...)`.
    let compiled = match &context_global {
        Some(context) => {
            let context = v8::Local::new(scope, context);
            v8::scope_with_context!(let inner, scope, context);
            let source_text = v8::Local::new(inner, source_text);
            let identifier = v8::Local::new(inner, identifier);
            let origin = v8::ScriptOrigin::new(
                inner, identifier, 0, 0, false, 0, None, false, false, true, None,
            );
            let mut source = v8::script_compiler::Source::new(source_text, Some(&origin));
            v8::script_compiler::compile_module(inner, &mut source).map(|module| {
                (
                    module.get_identity_hash().get(),
                    v8::Global::new(inner, module),
                )
            })
        }
        None => {
            let mut source = v8::script_compiler::Source::new(source_text, Some(&origin));
            v8::script_compiler::compile_module(scope, &mut source).map(|module| {
                (
                    module.get_identity_hash().get(),
                    v8::Global::new(scope, module),
                )
            })
        }
    };
    let Some((hash, global)) = compiled else {
        return;
    };
    let id = match scope.get_slot_mut::<VmModules>() {
        Some(modules) => modules.insert(hash, global, context_global),
        None => {
            let mut modules = VmModules::default();
            let id = modules.insert(hash, global, context_global);
            scope.set_slot(modules);
            id
        }
    };
    rv.set_uint32(id);
}

/// `op_vm_module_requests(id) -> string[]`
pub(crate) fn op_vm_module_requests<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = arg_u32(scope, &args, 0);
    let specifiers = with_modules(scope, id, |scope, module| {
        let requests = module.get_module_requests();
        let mut out = Vec::with_capacity(requests.length());
        for i in 0..requests.length() {
            let Some(request) = requests.get(scope, i) else {
                continue;
            };
            let Ok(request) = v8::Local::<v8::ModuleRequest>::try_from(request) else {
                continue;
            };
            out.push(v8::Local::<v8::Value>::from(request.get_specifier()));
        }
        let array = v8::Array::new_with_elements(scope, &out);
        Some(v8::Global::new(scope, array))
    });
    match specifiers.flatten() {
        Some(array) => {
            let array = v8::Local::new(scope, &array);
            rv.set(array.into());
        }
        None => throw_unknown_module(scope),
    }
}

/// `op_vm_module_link(id, specifiers, resolvedIds)`
///
/// Records what each specifier resolved to. Kept separate from instantiation
/// because the JS linker may await.
pub(crate) fn op_vm_module_link<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = arg_u32(scope, &args, 0);
    let (Ok(specifiers), Ok(resolved)) = (
        v8::Local::<v8::Array>::try_from(args.get(1)),
        v8::Local::<v8::Array>::try_from(args.get(2)),
    ) else {
        let message = v8::String::new(scope, "link expects two arrays").unwrap();
        let error = v8::Exception::type_error(scope, message);
        scope.throw_exception(error);
        return;
    };
    let mut pairs = Vec::with_capacity(specifiers.length() as usize);
    for i in 0..specifiers.length() {
        let (Some(specifier), Some(target)) =
            (specifiers.get_index(scope, i), resolved.get_index(scope, i))
        else {
            continue;
        };
        let specifier = specifier.to_rust_string_lossy(scope);
        let Some(target) = target.uint32_value(scope) else {
            continue;
        };
        pairs.push((specifier, target));
    }
    if let Some(modules) = scope.get_slot_mut::<VmModules>() {
        for (specifier, target) in pairs {
            modules.links.insert((id, specifier), target);
        }
    }
}

/// V8 asks for each import synchronously. Everything it can ask for was
/// resolved by `link()` already, so this is a lookup.
fn resolve_vm_module<'s>(
    context: v8::Local<'s, v8::Context>,
    specifier: v8::Local<'s, v8::String>,
    _import_attributes: v8::Local<'s, v8::FixedArray>,
    referrer: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Module>> {
    v8::callback_scope!(unsafe scope, context);
    let specifier = specifier.to_rust_string_lossy(scope);

    // Identity hashes collide, so narrow by hash and then compare identity --
    // resolving an import to the wrong module would be silent and very hard to
    // trace back.
    let candidates: Vec<(u32, v8::Global<v8::Module>)> = {
        let modules = scope.get_slot::<VmModules>()?;
        let ids = modules
            .ids_by_hash
            .get(&referrer.get_identity_hash().get())?;
        ids.iter()
            .filter_map(|id| modules.by_id.get(id).map(|e| (*id, e.module.clone())))
            .collect()
    };
    let referrer_id = candidates.into_iter().find_map(|(id, global)| {
        let module = v8::Local::new(scope, &global);
        (module == referrer).then_some(id)
    })?;

    let target = {
        let modules = scope.get_slot::<VmModules>()?;
        let target_id = modules.links.get(&(referrer_id, specifier))?;
        modules.by_id.get(target_id)?.module.clone()
    };
    Some(v8::Local::new(scope, &target))
}

/// `op_vm_module_instantiate(id)`
pub(crate) fn op_vm_module_instantiate<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = arg_u32(scope, &args, 0);
    // Instantiation failure leaves an exception pending, which is the answer
    // the caller wants; `Some(false)` without one should not look like success.
    let done = with_modules(scope, id, |scope, module| {
        let linked = module.instantiate_module(scope, resolve_vm_module);
        let flag = v8::Boolean::new(scope, linked.unwrap_or(true));
        Some(v8::Global::new(scope, flag))
    });
    let outcome = done
        .as_ref()
        .map(|inner| inner.as_ref().map(|f| v8::Local::new(scope, f).is_true()));
    match outcome {
        Some(Some(true)) | Some(None) => {}
        Some(Some(false)) => {
            if !scope.is_execution_terminating() {
                let message = v8::String::new(scope, "vm module could not be linked").unwrap();
                let error = v8::Exception::error(scope, message);
                scope.throw_exception(error);
            }
        }
        None => throw_unknown_module(scope),
    }
}

/// `op_vm_module_evaluate(id) -> Promise`
pub(crate) fn op_vm_module_evaluate<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = arg_u32(scope, &args, 0);
    let evaluated = with_modules(scope, id, |scope, module| {
        module
            .evaluate(scope)
            .map(|value| v8::Global::new(scope, value))
    });
    match evaluated {
        Some(Some(value)) => {
            let value = v8::Local::new(scope, &value);
            rv.set(value);
        }
        // Evaluation threw; the exception is already pending.
        Some(None) => {}
        None => throw_unknown_module(scope),
    }
}

/// `op_vm_module_namespace(id) -> object`
pub(crate) fn op_vm_module_namespace<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = arg_u32(scope, &args, 0);
    let namespace = with_modules(scope, id, |scope, module| {
        Some(v8::Global::new(scope, module.get_module_namespace()))
    });
    match namespace.flatten() {
        Some(namespace) => {
            let namespace = v8::Local::new(scope, &namespace);
            rv.set(namespace);
        }
        None => throw_unknown_module(scope),
    }
}

/// `op_vm_module_status(id) -> string`
pub(crate) fn op_vm_module_status<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = arg_u32(scope, &args, 0);
    // node's vocabulary, not V8's: a caller reads `module.status`.
    let status = with_modules(scope, id, |scope, module| {
        let name = match module.get_status() {
            v8::ModuleStatus::Uninstantiated => "unlinked",
            v8::ModuleStatus::Instantiating => "linking",
            v8::ModuleStatus::Instantiated => "linked",
            v8::ModuleStatus::Evaluating => "evaluating",
            v8::ModuleStatus::Evaluated => "evaluated",
            v8::ModuleStatus::Errored => "errored",
        };
        v8::String::new(scope, name).map(|text| v8::Global::new(scope, text))
    });
    match status.flatten() {
        Some(text) => {
            let text = v8::Local::new(scope, &text);
            rv.set(text.into());
        }
        None => throw_unknown_module(scope),
    }
}

/// `op_vm_module_error(id) -> value` -- only meaningful when errored.
pub(crate) fn op_vm_module_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = arg_u32(scope, &args, 0);
    let error = with_modules(scope, id, |scope, module| {
        (module.get_status() == v8::ModuleStatus::Errored)
            .then(|| v8::Global::new(scope, module.get_exception()))
    });
    match error.flatten() {
        Some(value) => {
            let value = v8::Local::new(scope, &value);
            rv.set(value);
        }
        None => {
            let undefined = v8::undefined(scope);
            rv.set(undefined.into());
        }
    }
}

/// `op_vm_module_release(id)` -- called from the JS wrapper's
/// FinalizationRegistry once the wrapper is collected.
pub(crate) fn op_vm_module_release<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = arg_u32(scope, &args, 0);
    if let Some(modules) = scope.get_slot_mut::<VmModules>() {
        modules.by_id.remove(&id);
        for ids in modules.ids_by_hash.values_mut() {
            ids.retain(|candidate| *candidate != id);
        }
        modules.ids_by_hash.retain(|_, ids| !ids.is_empty());
        // Edges from the dead module go too; edges INTO it are dropped with
        // their own referrer.
        modules.links.retain(|(referrer, _), _| *referrer != id);
    }
}
