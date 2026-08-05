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

/// Modules created by `new vm.SourceTextModule`, by the id handed to JS.
#[derive(Default)]
pub(crate) struct VmModules {
    by_id: HashMap<u32, v8::Global<v8::Module>>,
    /// Identity hash -> ids. A hash can collide, so the resolve callback
    /// narrows the candidates by comparing module identity.
    ids_by_hash: HashMap<i32, Vec<u32>>,
    /// (referrer id, specifier) -> resolved id, filled by `link()` before
    /// instantiation and read by the resolve callback.
    links: HashMap<(u32, String), u32>,
    next_id: u32,
}

impl VmModules {
    fn insert(&mut self, hash: i32, module: v8::Global<v8::Module>) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.by_id.insert(id, module);
        self.ids_by_hash.entry(hash).or_default().push(id);
        id
    }
}

fn with_modules<'s, T>(
    scope: &mut v8::PinScope<'s, '_>,
    id: u32,
    f: impl FnOnce(&mut v8::PinScope<'s, '_>, v8::Local<'s, v8::Module>) -> T,
) -> Option<T> {
    // The global is cloned out before any Local is made, so the slot borrow
    // never overlaps use of the scope.
    let global = scope.get_slot::<VmModules>()?.by_id.get(&id)?.clone();
    let module = v8::Local::new(scope, &global);
    Some(f(scope, module))
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
    let identifier = args.get(1);
    let origin = v8::ScriptOrigin::new(
        scope, identifier, 0, 0, false, 0, None, false, false,
        // is_module -- without it V8 compiles a classic script and every
        // import/export is a syntax error.
        true, None,
    );
    let mut source = v8::script_compiler::Source::new(source_text, Some(&origin));
    // No TryCatch: a syntax error is left pending, which is what should reach
    // `new SourceTextModule(...)`.
    let Some(module) = v8::script_compiler::compile_module(scope, &mut source) else {
        return;
    };
    let hash = module.get_identity_hash().get();
    let global = v8::Global::new(scope, module);
    let id = match scope.get_slot_mut::<VmModules>() {
        Some(modules) => modules.insert(hash, global),
        None => {
            let mut modules = VmModules::default();
            let id = modules.insert(hash, global);
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
        v8::Array::new_with_elements(scope, &out)
    });
    match specifiers {
        Some(array) => rv.set(array.into()),
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
            .filter_map(|id| modules.by_id.get(id).map(|m| (*id, m.clone())))
            .collect()
    };
    let referrer_id = candidates.into_iter().find_map(|(id, global)| {
        let module = v8::Local::new(scope, &global);
        (module == referrer).then_some(id)
    })?;

    let target = {
        let modules = scope.get_slot::<VmModules>()?;
        let target_id = modules.links.get(&(referrer_id, specifier))?;
        modules.by_id.get(target_id)?.clone()
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
        module.instantiate_module(scope, resolve_vm_module)
    });
    match done {
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
    match with_modules(scope, id, |scope, module| module.evaluate(scope)) {
        Some(Some(value)) => rv.set(value),
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
    match with_modules(scope, id, |_scope, module| module.get_module_namespace()) {
        Some(namespace) => rv.set(namespace),
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
    let status = with_modules(scope, id, |_scope, module| match module.get_status() {
        v8::ModuleStatus::Uninstantiated => "unlinked",
        v8::ModuleStatus::Instantiating => "linking",
        v8::ModuleStatus::Instantiated => "linked",
        v8::ModuleStatus::Evaluating => "evaluating",
        v8::ModuleStatus::Evaluated => "evaluated",
        v8::ModuleStatus::Errored => "errored",
    });
    match status.and_then(|s| v8::String::new(scope, s)) {
        Some(text) => rv.set(text.into()),
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
    let error = with_modules(scope, id, |_scope, module| {
        (module.get_status() == v8::ModuleStatus::Errored).then(|| module.get_exception())
    });
    match error {
        Some(Some(value)) => rv.set(value),
        _ => {
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
