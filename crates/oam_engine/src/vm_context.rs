//! Real V8 contexts for `node:vm`.
//!
//! `vm.createContext(sandbox)` builds a genuine `v8::Context` whose global
//! object carries property interceptors aimed at the sandbox -- the same shape
//! as Node's `node_contextify.cc`. That is what buys the two properties a
//! `with (sandbox) { ... }` closure can never have: the script gets its own
//! intrinsics (its `Object` is not the host's `Object`, so prototype patching
//! inside the context cannot reach out), and a write to `globalThis` lands on
//! the sandbox instead of on the runtime's real global.
//!
//! ## Ownership
//!
//! Nothing here is rooted from Rust. A contextified sandbox holds its context's
//! global proxy under a private symbol, and the context holds the sandbox in
//! embedder data -- a cycle entirely inside the V8 heap, which the GC collects
//! once the sandbox goes unreachable. That matters because the sandbox is the
//! only handle JS ever has on a vm context (`createContext` returns it and
//! every `runInContext` takes it), so a Rust-side registry would pin every
//! context ever created for the life of the isolate.

/// Embedder-data slot on a contextified context, holding its sandbox.
///
/// rusty_v8 offsets every embedder slot past its own internal ones, so 0 is the
/// first slot we own -- it is not the debugger's index 0.
const SANDBOX_SLOT: i32 = 0;

/// Name of the private symbol linking a sandbox to its context's global proxy.
///
/// Private symbols are invisible to JS entirely: they do not show up in
/// `Object.getOwnPropertySymbols`, so a contextified sandbox still enumerates
/// exactly like the plain object the caller passed in.
const PROXY_PRIVATE: &str = "oam:vm:global_proxy";

fn proxy_private<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Private> {
    let name = v8::String::new(scope, PROXY_PRIVATE).unwrap();
    v8::Private::for_api(scope, Some(name))
}

/// The global proxy of `sandbox`'s context, or `None` if it was never
/// contextified.
fn proxy_of<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    sandbox: v8::Local<'s, v8::Object>,
) -> Option<v8::Local<'s, v8::Object>> {
    let key = proxy_private(scope);
    let value = sandbox.get_private(scope, key)?;
    v8::Local::<v8::Object>::try_from(value).ok()
}

/// The context a contextified sandbox owns, or `None` if it was never
/// contextified. Used by `vm.SourceTextModule` to compile and run a module
/// inside the context the caller named.
pub(crate) fn context_of_sandbox<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    sandbox: v8::Local<'s, v8::Object>,
) -> Option<v8::Local<'s, v8::Context>> {
    let proxy = proxy_of(scope, sandbox)?;
    proxy.get_creation_context(scope)
}

/// The sandbox behind the object an interceptor fired on.
///
/// Keyed off the holder's *creation* context rather than the isolate's current
/// one: the host reads properties off a vm global too (that is what
/// `runInContext` returns), and there the current context is the host's.
fn sandbox_of<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: &v8::PropertyCallbackArguments<'s>,
) -> Option<v8::Local<'s, v8::Object>> {
    let context = args.holder().get_creation_context(scope)?;
    let data = context.get_embedder_data(scope, SANDBOX_SLOT)?;
    v8::Local::<v8::Object>::try_from(data).ok()
}

fn is_read_only(attributes: v8::PropertyAttribute) -> bool {
    attributes.is_read_only()
}

// -------------------------------------------------------------- interceptors

/// Reads resolve against the sandbox first, then fall through to the context's
/// own global.
///
/// The fall-through is the whole reason `Intercepted::No` exists: declining
/// leaves V8 to run its normal lookup, which is what makes the context's
/// intrinsics (`Object`, `JSON`, `Math`) reachable without us enumerating them.
fn named_getter<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key: v8::Local<'s, v8::Name>,
    args: v8::PropertyCallbackArguments<'s>,
    mut rv: v8::ReturnValue<v8::Value>,
) -> v8::Intercepted {
    let Some(sandbox) = sandbox_of(scope, &args) else {
        // `Context::new` walks the global before we have attached the sandbox.
        return v8::Intercepted::kNo;
    };
    // A real-named lookup walks the prototype chain but skips interceptors, so
    // it cannot recurse back into this callback.
    let Some(value) = sandbox.get_real_named_property(scope, key) else {
        return v8::Intercepted::kNo;
    };
    // Inside the context the sandbox IS the global, so a self-reference has to
    // read back as the global proxy -- `sandbox.self = sandbox` then
    // `self === globalThis` is true in Node.
    if value == v8::Local::<v8::Value>::from(sandbox) {
        match args.holder().get_creation_context(scope) {
            Some(context) => {
                let proxy = context.global(scope);
                rv.set(proxy.into());
            }
            None => rv.set(value),
        }
    } else {
        rv.set(value);
    }
    v8::Intercepted::kYes
}

/// Writes land on the sandbox, which is what keeps the host's real global
/// clean: `vm.runInNewContext("globalThis.x = 1", s)` must set `s.x` and leave
/// the runtime's own `globalThis.x` undefined.
fn named_setter<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key: v8::Local<'s, v8::Name>,
    value: v8::Local<'s, v8::Value>,
    args: v8::PropertyCallbackArguments<'s>,
    _rv: v8::ReturnValue<()>,
) -> v8::Intercepted {
    let Some(sandbox) = sandbox_of(scope, &args) else {
        return v8::Intercepted::kNo;
    };
    // A read-only intrinsic stays read-only. Copying `undefined` or `NaN` onto
    // the sandbox would let the getter shadow the intrinsic with the assigned
    // value, so `undefined = 5; undefined` would answer 5.
    let holder = args.holder();
    if holder
        .get_real_named_property_attributes(scope, key)
        .is_some_and(is_read_only)
    {
        return v8::Intercepted::kNo;
    }
    if sandbox
        .get_real_named_property_attributes(scope, key)
        .is_some_and(is_read_only)
    {
        return v8::Intercepted::kNo;
    }
    sandbox.set(scope, key.into(), value);
    v8::Intercepted::kYes
}

/// Backs `in`, `hasOwnProperty` and `propertyIsEnumerable` on the global.
fn named_query<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key: v8::Local<'s, v8::Name>,
    args: v8::PropertyCallbackArguments<'s>,
    mut rv: v8::ReturnValue<v8::Integer>,
) -> v8::Intercepted {
    let Some(sandbox) = sandbox_of(scope, &args) else {
        return v8::Intercepted::kNo;
    };
    match sandbox.get_real_named_property_attributes(scope, key) {
        Some(attributes) => {
            rv.set_uint32(attributes.as_u32());
            v8::Intercepted::kYes
        }
        None => v8::Intercepted::kNo,
    }
}

fn named_deleter<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key: v8::Local<'s, v8::Name>,
    args: v8::PropertyCallbackArguments<'s>,
    mut rv: v8::ReturnValue<v8::Boolean>,
) -> v8::Intercepted {
    let Some(sandbox) = sandbox_of(scope, &args) else {
        return v8::Intercepted::kNo;
    };
    match sandbox.delete(scope, key.into()) {
        Some(deleted) => {
            rv.set_bool(deleted);
            v8::Intercepted::kYes
        }
        None => v8::Intercepted::kNo,
    }
}

/// Backs `Object.keys(globalThis)` and `for (const k in globalThis)`.
///
/// Index-like keys are deliberately left out: V8 asks the indexed handler for
/// those, and it requires that answer to be numeric.
fn named_enumerator<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::PropertyCallbackArguments<'s>,
    mut rv: v8::ReturnValue<v8::Array>,
) {
    let Some(sandbox) = sandbox_of(scope, &args) else {
        return;
    };
    let names = sandbox.get_property_names(
        scope,
        v8::GetPropertyNamesArgs {
            mode: v8::KeyCollectionMode::OwnOnly,
            property_filter: v8::PropertyFilter::ONLY_ENUMERABLE | v8::PropertyFilter::SKIP_SYMBOLS,
            index_filter: v8::IndexFilter::SkipIndices,
            key_conversion: v8::KeyConversionMode::ConvertToString,
        },
    );
    if let Some(names) = names {
        rv.set(names);
    }
}

/// The indexed half of enumeration.
///
/// V8 runs every element of this array through `Object::ToUint32` and aborts
/// the process if one does not convert, so it must contain array indices and
/// nothing else -- sharing the named enumerator here is a hard crash, not a
/// wrong answer.
fn indexed_enumerator<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::PropertyCallbackArguments<'s>,
    mut rv: v8::ReturnValue<v8::Array>,
) {
    let Some(sandbox) = sandbox_of(scope, &args) else {
        return;
    };
    let names = sandbox.get_property_names(
        scope,
        v8::GetPropertyNamesArgs {
            mode: v8::KeyCollectionMode::OwnOnly,
            property_filter: v8::PropertyFilter::ONLY_ENUMERABLE | v8::PropertyFilter::SKIP_SYMBOLS,
            index_filter: v8::IndexFilter::IncludeIndices,
            key_conversion: v8::KeyConversionMode::KeepNumbers,
        },
    );
    let Some(names) = names else {
        return;
    };
    let mut indices = Vec::new();
    for i in 0..names.length() {
        if let Some(name) = names.get_index(scope, i)
            && name.is_number()
        {
            indices.push(name);
        }
    }
    rv.set(v8::Array::new_with_elements(scope, &indices));
}

/// `Object.defineProperty(globalThis, ...)` -- and, less obviously, every
/// top-level `var` and function declaration, which V8 lowers to a define on the
/// global rather than a set.
fn named_definer<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key: v8::Local<'s, v8::Name>,
    descriptor: &v8::PropertyDescriptor,
    args: v8::PropertyCallbackArguments<'s>,
    _rv: v8::ReturnValue<()>,
) -> v8::Intercepted {
    let Some(sandbox) = sandbox_of(scope, &args) else {
        return v8::Intercepted::kNo;
    };
    if args
        .holder()
        .get_real_named_property_attributes(scope, key)
        .is_some_and(is_read_only)
    {
        return v8::Intercepted::kNo;
    }
    sandbox.define_property(scope, key, descriptor);
    v8::Intercepted::kYes
}

fn named_descriptor<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key: v8::Local<'s, v8::Name>,
    args: v8::PropertyCallbackArguments<'s>,
    mut rv: v8::ReturnValue<v8::Value>,
) -> v8::Intercepted {
    let Some(sandbox) = sandbox_of(scope, &args) else {
        return v8::Intercepted::kNo;
    };
    match sandbox.get_own_property_descriptor(scope, key) {
        Some(descriptor) if !descriptor.is_undefined() => {
            rv.set(descriptor);
            v8::Intercepted::kYes
        }
        _ => v8::Intercepted::kNo,
    }
}

// Indexed access is the named path with the index spelled as a string: a
// sandbox is a plain object, where `s[0]` and `s["0"]` are one property.

fn index_key<'s>(scope: &mut v8::PinScope<'s, '_>, index: u32) -> Option<v8::Local<'s, v8::Name>> {
    let text = v8::String::new(scope, &index.to_string())?;
    Some(text.into())
}

fn indexed_getter<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    index: u32,
    args: v8::PropertyCallbackArguments<'s>,
    rv: v8::ReturnValue<v8::Value>,
) -> v8::Intercepted {
    match index_key(scope, index) {
        Some(key) => named_getter(scope, key, args, rv),
        None => v8::Intercepted::kNo,
    }
}

fn indexed_setter<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    index: u32,
    value: v8::Local<'s, v8::Value>,
    args: v8::PropertyCallbackArguments<'s>,
    rv: v8::ReturnValue<()>,
) -> v8::Intercepted {
    match index_key(scope, index) {
        Some(key) => named_setter(scope, key, value, args, rv),
        None => v8::Intercepted::kNo,
    }
}

fn indexed_query<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    index: u32,
    args: v8::PropertyCallbackArguments<'s>,
    rv: v8::ReturnValue<v8::Integer>,
) -> v8::Intercepted {
    match index_key(scope, index) {
        Some(key) => named_query(scope, key, args, rv),
        None => v8::Intercepted::kNo,
    }
}

fn indexed_deleter<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    index: u32,
    args: v8::PropertyCallbackArguments<'s>,
    rv: v8::ReturnValue<v8::Boolean>,
) -> v8::Intercepted {
    match index_key(scope, index) {
        Some(key) => named_deleter(scope, key, args, rv),
        None => v8::Intercepted::kNo,
    }
}

fn indexed_definer<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    index: u32,
    descriptor: &v8::PropertyDescriptor,
    args: v8::PropertyCallbackArguments<'s>,
    rv: v8::ReturnValue<()>,
) -> v8::Intercepted {
    match index_key(scope, index) {
        Some(key) => named_definer(scope, key, descriptor, args, rv),
        None => v8::Intercepted::kNo,
    }
}

fn indexed_descriptor<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    index: u32,
    args: v8::PropertyCallbackArguments<'s>,
    rv: v8::ReturnValue<v8::Value>,
) -> v8::Intercepted {
    match index_key(scope, index) {
        Some(key) => named_descriptor(scope, key, args, rv),
        None => v8::Intercepted::kNo,
    }
}

// ---------------------------------------------------------------------- ops

/// `op_vm_create_context(sandbox) -> globalProxy`
///
/// Idempotent: a sandbox that already carries a context hands back the same
/// global proxy, so `vm.createContext(s)` twice yields one context.
pub(crate) fn op_vm_create_context<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Ok(sandbox) = v8::Local::<v8::Object>::try_from(args.get(0)) else {
        let message = v8::String::new(scope, "sandbox must be an object").unwrap();
        let error = v8::Exception::type_error(scope, message);
        scope.throw_exception(error);
        return;
    };

    if let Some(proxy) = proxy_of(scope, sandbox) {
        rv.set(proxy.into());
        return;
    }

    let template = v8::ObjectTemplate::new(scope);
    template.set_named_property_handler(
        v8::NamedPropertyHandlerConfiguration::new()
            .getter(named_getter)
            .setter(named_setter)
            .query(named_query)
            .deleter(named_deleter)
            .enumerator(named_enumerator)
            .definer(named_definer)
            .descriptor(named_descriptor),
    );
    template.set_indexed_property_handler(
        v8::IndexedPropertyHandlerConfiguration::new()
            .getter(indexed_getter)
            .setter(indexed_setter)
            .query(indexed_query)
            .deleter(indexed_deleter)
            .enumerator(indexed_enumerator)
            .definer(indexed_definer)
            .descriptor(indexed_descriptor),
    );

    let host = scope.get_current_context();
    let token = host.get_security_token(scope);
    let context = v8::Context::new(
        scope,
        v8::ContextOptions {
            global_template: Some(template),
            ..Default::default()
        },
    );
    // Share the host's security token so the embedder and the script can pass
    // objects across the boundary. A vm context is a fresh set of intrinsics,
    // not a security sandbox -- Node says the same in its own docs, and code
    // that needs isolation wants a separate process.
    context.set_security_token(token);
    context.set_embedder_data(SANDBOX_SLOT, sandbox.into());

    let proxy = context.global(scope);
    let key = proxy_private(scope);
    sandbox.set_private(scope, key, proxy.into());
    rv.set(proxy.into());
}

/// `op_vm_is_context(value) -> boolean`
pub(crate) fn op_vm_is_context<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let contextified = v8::Local::<v8::Object>::try_from(args.get(0))
        .ok()
        .and_then(|sandbox| proxy_of(scope, sandbox))
        .is_some();
    rv.set_bool(contextified);
}

/// Compiles and runs `code` in `context`, re-throwing into the caller.
///
/// The result and any exception cross the inner handle scope as globals: a
/// `Local` minted inside would dangle the moment the scope closes.
fn run(
    scope: &mut v8::PinScope<'_, '_>,
    context: v8::Local<'_, v8::Context>,
    code: v8::Local<'_, v8::String>,
    origin_name: v8::Local<'_, v8::Value>,
    line_offset: i32,
    column_offset: i32,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let outcome = {
        v8::scope_with_context!(let inner, scope, context);
        v8::tc_scope!(let tc, inner);
        let code = v8::Local::new(tc, code);
        let origin_name = v8::Local::new(tc, origin_name);
        let origin = v8::ScriptOrigin::new(
            tc,
            origin_name,
            line_offset,
            column_offset,
            false,
            0,
            None,
            false,
            false,
            false,
            None,
        );
        let value = v8::Script::compile(tc, code, Some(&origin)).and_then(|script| script.run(tc));
        match value {
            Some(value) => Ok(v8::Global::new(tc, value)),
            None => {
                // A compile error and a thrown exception arrive the same way.
                let error = tc.exception().unwrap_or_else(|| v8::undefined(tc).into());
                Err(v8::Global::new(tc, error))
            }
        }
    };
    match outcome {
        Ok(value) => rv.set(v8::Local::new(scope, &value)),
        Err(error) => {
            let error = v8::Local::new(scope, &error);
            scope.throw_exception(error);
        }
    }
}

/// `op_vm_run_in_context(sandbox, code, filename, lineOffset, columnOffset)`
pub(crate) fn op_vm_run_in_context<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    rv: v8::ReturnValue<'_, v8::Value>,
) {
    let sandbox = match v8::Local::<v8::Object>::try_from(args.get(0)) {
        Ok(sandbox) => sandbox,
        Err(_) => {
            let message = v8::String::new(scope, "contextifiedObject must be an object").unwrap();
            let error = v8::Exception::type_error(scope, message);
            scope.throw_exception(error);
            return;
        }
    };
    let Some(proxy) = proxy_of(scope, sandbox) else {
        let message = v8::String::new(
            scope,
            "The \"contextifiedObject\" argument must be an vm.Context",
        )
        .unwrap();
        let error = v8::Exception::type_error(scope, message);
        scope.throw_exception(error);
        return;
    };
    let Some(context) = proxy.get_creation_context(scope) else {
        let message = v8::String::new(scope, "vm context is no longer available").unwrap();
        let error = v8::Exception::error(scope, message);
        scope.throw_exception(error);
        return;
    };
    let Ok(code) = v8::Local::<v8::String>::try_from(args.get(1)) else {
        let message = v8::String::new(scope, "code must be a string").unwrap();
        let error = v8::Exception::type_error(scope, message);
        scope.throw_exception(error);
        return;
    };
    let origin_name = args.get(2);
    let line_offset = args.get(3).int32_value(scope).unwrap_or(0);
    let column_offset = args.get(4).int32_value(scope).unwrap_or(0);
    run(
        scope,
        context,
        code,
        origin_name,
        line_offset,
        column_offset,
        rv,
    );
}

/// `op_vm_run_in_this_context(code, filename, lineOffset, columnOffset)`
///
/// Compiles against the live global rather than wrapping the source in a
/// function, so a top-level `var` really does create a global binding and stack
/// frames carry the caller's filename.
pub(crate) fn op_vm_run_in_this_context<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Ok(code) = v8::Local::<v8::String>::try_from(args.get(0)) else {
        let message = v8::String::new(scope, "code must be a string").unwrap();
        let error = v8::Exception::type_error(scope, message);
        scope.throw_exception(error);
        return;
    };
    let origin_name = args.get(1);
    let line_offset = args.get(2).int32_value(scope).unwrap_or(0);
    let column_offset = args.get(3).int32_value(scope).unwrap_or(0);
    let context = scope.get_current_context();
    run(
        scope,
        context,
        code,
        origin_name,
        line_offset,
        column_offset,
        rv,
    );
}

/// `op_vm_compile(code, filename, lineOffset, columnOffset)`
///
/// Compiles and discards, purely so `new vm.Script(badSource)` throws a
/// SyntaxError from the constructor the way Node's does instead of deferring it
/// to the first run.
pub(crate) fn op_vm_compile<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Ok(code) = v8::Local::<v8::String>::try_from(args.get(0)) else {
        let message = v8::String::new(scope, "code must be a string").unwrap();
        let error = v8::Exception::type_error(scope, message);
        scope.throw_exception(error);
        return;
    };
    let origin_name = args.get(1);
    let line_offset = args.get(2).int32_value(scope).unwrap_or(0);
    let column_offset = args.get(3).int32_value(scope).unwrap_or(0);
    let origin = v8::ScriptOrigin::new(
        scope,
        origin_name,
        line_offset,
        column_offset,
        false,
        0,
        None,
        false,
        false,
        false,
        None,
    );
    // No TryCatch: a compile failure leaves the SyntaxError pending, which is
    // exactly what should reach the caller.
    v8::Script::compile(scope, code, Some(&origin));
}
