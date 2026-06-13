//! N-API (Node-API) alpha: the C-ABI layer compiled .node addons call.
//!
//! Scope of the alpha — the value/property/function/error core (~45
//! functions), enough for procedural addons: create/read numbers,
//! strings, booleans, objects, arrays; named/indexed properties;
//! native-backed functions with callbacks; throw/catch. NOT yet:
//! napi_wrap/class machinery, references, async_work, threadsafe
//! functions, buffers/arraybuffers, bigint — tracked on the punch list.
//!
//! ABI model (the Deno approach): `napi_value` IS a `v8::Local<Value>`
//! transmuted to a pointer — `Local` is repr(C) over `NonNull`, asserted
//! below. Every napi call happens while the engine is on the stack
//! (module init or a callback trampoline), so a live `PinScope` exists;
//! the env carries a raw pointer to it, saved/restored re-entrantly at
//! every native entry. Exceptions are DEFERRED: napi_throw_* stores the
//! value in the env, and the trampoline re-throws into V8 on exit.
//!
//! Symbol export: oam_cli/build.rs exports every `napi_*` below from the
//! oam EXECUTABLE (.def on Windows, --export-dynamic on Linux; macOS
//! exes export by default), which is how addons resolve the ABI at load.

#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_char, c_void};

// napi_value == Local<Value> (pointer-sized, non-null). Compile-time proof.
const _: () = assert!(
    std::mem::size_of::<v8::Local<'static, v8::Value>>() == std::mem::size_of::<*mut c_void>()
);

pub type NapiValue = *mut c_void;
pub type NapiStatus = i32;

pub const NAPI_OK: NapiStatus = 0;
pub const NAPI_INVALID_ARG: NapiStatus = 1;
pub const NAPI_OBJECT_EXPECTED: NapiStatus = 2;
pub const NAPI_STRING_EXPECTED: NapiStatus = 3;
pub const NAPI_FUNCTION_EXPECTED: NapiStatus = 5;
pub const NAPI_NUMBER_EXPECTED: NapiStatus = 6;
pub const NAPI_BOOLEAN_EXPECTED: NapiStatus = 7;
pub const NAPI_ARRAY_EXPECTED: NapiStatus = 8;
pub const NAPI_GENERIC_FAILURE: NapiStatus = 9;
pub const NAPI_PENDING_EXCEPTION: NapiStatus = 10;

/// One env per loaded addon (Node's model).
///
/// Previously leaked for the process lifetime; now owned by an
/// `AddonRegistry` stored on `JsRuntime`.  The env drops when the
/// runtime drops, which also drops every `FnData` it owns.
#[repr(C)]
pub struct NapiEnv {
    /// `*mut v8::PinScope<'_, '_>` while inside a native entry; null
    /// outside. Saved/restored re-entrantly by the trampolines.
    scope: *mut c_void,
    /// Deferred exception, thrown into V8 when the native frame returns.
    pending: Option<v8::Global<v8::Value>>,
    /// Every `FnData` created via `napi_create_function` for this env.
    /// Stored here so they live exactly as long as the env, and the env
    /// lives exactly as long as the owning `JsRuntime`.
    fn_data: Vec<Box<FnData>>,
}

impl NapiEnv {
    fn new() -> Box<NapiEnv> {
        Box::new(NapiEnv {
            scope: std::ptr::null_mut(),
            pending: None,
            fn_data: Vec::new(),
        })
    }
}

/// Per-runtime owner of all heap allocations made on behalf of N-API
/// addons: the `NapiEnv` boxes (one per loaded addon) and the
/// `libloading::Library` handles for the `.node` files.
///
/// Drop order relative to `JsRuntime` fields matters:
///
/// * `AddonRegistry` is declared AFTER `isolate` in `JsRuntime`, so in
///   Rust's field-drop order (declaration order, top to bottom) the
///   registry drops AFTER the isolate.  Once the isolate is gone the V8
///   heap is gone; any V8 `External` that pointed at a `FnData` has
///   already been collected, so dropping `FnData` at this point is safe.
/// * The `Library` handles live at least as long as the `NapiEnv` boxes
///   because they are stored together in `AddonRegistry`; the vecs
///   drop in declaration order, so `envs` drops before `libraries`.
///   Even if a destructor inside the `.node` library calls back through
///   the N-API ABI, the `NapiEnv` outlives the library unload.
pub struct AddonRegistry {
    /// Owned `NapiEnv` allocations, one per successfully loaded addon.
    envs: Vec<Box<NapiEnv>>,
    /// Loaded addon libraries.  Dropping a `Library` unloads the `.node`
    /// DLL; this must happen AFTER the envs are dropped so any destructor
    /// in the library can still reach the env.
    libraries: Vec<libloading::Library>,
}

impl AddonRegistry {
    pub fn new() -> Self {
        AddonRegistry {
            envs: Vec::new(),
            libraries: Vec::new(),
        }
    }
}

/// Thread-local drop counter: each `NapiEnv::drop` increments it.
/// Used only in tests to verify that every load is matched by a drop.
#[cfg(test)]
thread_local! {
    pub static NAPI_ENV_DROP_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
impl Drop for NapiEnv {
    fn drop(&mut self) {
        NAPI_ENV_DROP_COUNT.with(|c| c.set(c.get() + 1));
    }
}

type Env = *mut NapiEnv;

/// SAFETY HELPERS — every napi fn funnels through these.
unsafe fn env_scope<'a>(env: Env) -> Option<&'a mut v8::PinScope<'static, 'static>> {
    unsafe {
        let env = env.as_mut()?;
        (env.scope as *mut v8::PinScope<'static, 'static>).as_mut()
    }
}

unsafe fn to_local<'s>(value: NapiValue) -> Option<v8::Local<'s, v8::Value>> {
    if value.is_null() {
        None
    } else {
        Some(unsafe { std::mem::transmute::<NapiValue, v8::Local<'s, v8::Value>>(value) })
    }
}

fn from_local(local: v8::Local<'_, v8::Value>) -> NapiValue {
    unsafe { std::mem::transmute::<v8::Local<'_, v8::Value>, NapiValue>(local) }
}

unsafe fn out<T>(ptr: *mut T, value: T) -> NapiStatus {
    if ptr.is_null() {
        return NAPI_INVALID_ARG;
    }
    unsafe { ptr.write(value) };
    NAPI_OK
}

/// Callback registration payload (leaked per napi_create_function).
struct FnData {
    cb: unsafe extern "C" fn(Env, *mut CbInfo) -> NapiValue,
    data: *mut c_void,
    env: Env,
}

/// What napi_get_cb_info reads. Lives on the trampoline's stack.
#[repr(C)]
pub struct CbInfo {
    args: Vec<NapiValue>,
    this: NapiValue,
    data: *mut c_void,
}

/// The zero-capture trampoline behind every napi_create_function.
fn napi_trampoline(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Ok(external) = v8::Local::<v8::External>::try_from(args.data()) else {
        return;
    };
    let fn_data = unsafe { &*(external.value() as *const FnData) };
    let env = fn_data.env;

    let mut collected = Vec::with_capacity(args.length() as usize);
    for i in 0..args.length() {
        collected.push(from_local(args.get(i)));
    }
    let mut info = CbInfo {
        args: collected,
        this: from_local(args.this().into()),
        data: fn_data.data,
    };

    // Re-entrant scope swap: nested napi_call_function arrives back here.
    let (prev_scope, result) = unsafe {
        let prev = (*env).scope;
        (*env).scope = scope as *mut v8::PinScope<'_, '_> as *mut c_void;
        let result = (fn_data.cb)(env, &mut info as *mut CbInfo);
        (prev, result)
    };
    unsafe {
        (*env).scope = prev_scope;
        if let Some(pending) = (*env).pending.take() {
            let exception = v8::Local::new(scope, &pending);
            scope.throw_exception(exception);
            return;
        }
    }
    if let Some(local) = unsafe { to_local(result) } {
        rv.set(local);
    }
}

// =========================================================== value creation

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_undefined(env: Env, result: *mut NapiValue) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    unsafe { out(result, from_local(v8::undefined(scope).into())) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_null(env: Env, result: *mut NapiValue) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    unsafe { out(result, from_local(v8::null(scope).into())) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_global(env: Env, result: *mut NapiValue) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let context = scope.get_current_context();
    let global = context.global(scope);
    unsafe { out(result, from_local(global.into())) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_boolean(
    env: Env,
    value: bool,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    unsafe { out(result, from_local(v8::Boolean::new(scope, value).into())) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_int32(
    env: Env,
    value: i32,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    unsafe { out(result, from_local(v8::Integer::new(scope, value).into())) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_uint32(
    env: Env,
    value: u32,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    unsafe {
        out(
            result,
            from_local(v8::Integer::new_from_unsigned(scope, value).into()),
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_int64(
    env: Env,
    value: i64,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // Node-parity: Number.MAX_SAFE_INTEGER = 2^53 - 1. Values whose absolute
    // value EQUALS 2^53 are representable in f64 exactly but the surrounding
    // integers are not, so Node's napi_create_int64 returns BigInt at the
    // boundary. Inclusive range against (2^53 - 1) matches.
    const MAX_SAFE: i64 = (1_i64 << 53) - 1;
    unsafe {
        if (-MAX_SAFE..=MAX_SAFE).contains(&value) {
            out(
                result,
                from_local(v8::Number::new(scope, value as f64).into()),
            )
        } else {
            out(
                result,
                from_local(v8::BigInt::new_from_i64(scope, value).into()),
            )
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_double(
    env: Env,
    value: f64,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    unsafe { out(result, from_local(v8::Number::new(scope, value).into())) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_string_utf8(
    env: Env,
    string: *const c_char,
    length: usize,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    if string.is_null() {
        return NAPI_INVALID_ARG;
    }
    let bytes = unsafe {
        if length == usize::MAX {
            std::ffi::CStr::from_ptr(string).to_bytes()
        } else {
            std::slice::from_raw_parts(string as *const u8, length)
        }
    };
    let text = String::from_utf8_lossy(bytes);
    let Some(created) = v8::String::new(scope, &text) else {
        return NAPI_GENERIC_FAILURE;
    };
    unsafe { out(result, from_local(created.into())) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_object(env: Env, result: *mut NapiValue) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    unsafe { out(result, from_local(v8::Object::new(scope).into())) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_array(env: Env, result: *mut NapiValue) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    unsafe { out(result, from_local(v8::Array::new(scope, 0).into())) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_array_with_length(
    env: Env,
    length: usize,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    unsafe {
        out(
            result,
            from_local(v8::Array::new(scope, length as i32).into()),
        )
    }
}

// =========================================================== value reading

/// napi_valuetype numeric values per js_native_api_types.h.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_typeof(env: Env, value: NapiValue, result: *mut i32) -> NapiStatus {
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let kind = if local.is_undefined() {
        0
    } else if local.is_null() {
        1
    } else if local.is_boolean() {
        2
    } else if local.is_number() {
        3
    } else if local.is_string() {
        4
    } else if local.is_symbol() {
        5
    } else if local.is_function() {
        7
    } else if local.is_external() {
        8
    } else if local.is_big_int() {
        9
    } else {
        6 // object
    };
    unsafe { out(result, kind) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_bool(
    env: Env,
    value: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    if !local.is_boolean() {
        return NAPI_BOOLEAN_EXPECTED;
    }
    unsafe { out(result, local.is_true()) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_int32(
    env: Env,
    value: NapiValue,
    result: *mut i32,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    if !local.is_number() {
        return NAPI_NUMBER_EXPECTED;
    }
    unsafe { out(result, local.int32_value(scope).unwrap_or(0)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_uint32(
    env: Env,
    value: NapiValue,
    result: *mut u32,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    if !local.is_number() {
        return NAPI_NUMBER_EXPECTED;
    }
    unsafe { out(result, local.uint32_value(scope).unwrap_or(0)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_int64(
    env: Env,
    value: NapiValue,
    result: *mut i64,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    if !local.is_number() {
        return NAPI_NUMBER_EXPECTED;
    }
    unsafe { out(result, local.number_value(scope).unwrap_or(0.0) as i64) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_double(
    env: Env,
    value: NapiValue,
    result: *mut f64,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    if !local.is_number() {
        return NAPI_NUMBER_EXPECTED;
    }
    unsafe { out(result, local.number_value(scope).unwrap_or(0.0)) }
}

/// Two-phase: null buf -> *result = utf8 byte length; else copy + NUL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_string_utf8(
    env: Env,
    value: NapiValue,
    buf: *mut c_char,
    bufsize: usize,
    result: *mut usize,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(string) = v8::Local::<v8::String>::try_from(local) else {
        return NAPI_STRING_EXPECTED;
    };
    let text = string.to_rust_string_lossy(scope);
    if buf.is_null() {
        return unsafe { out(result, text.len()) };
    }
    if bufsize == 0 {
        return unsafe { out(result, 0) };
    }
    let copy_len = text.len().min(bufsize - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buf as *mut u8, copy_len);
        *buf.add(copy_len) = 0;
        if !result.is_null() {
            *result = copy_len;
        }
    }
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_array(
    env: Env,
    value: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    unsafe { out(result, local.is_array()) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_array_length(
    env: Env,
    value: NapiValue,
    result: *mut u32,
) -> NapiStatus {
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(array) = v8::Local::<v8::Array>::try_from(local) else {
        return NAPI_ARRAY_EXPECTED;
    };
    unsafe { out(result, array.length()) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_strict_equals(
    env: Env,
    lhs: NapiValue,
    rhs: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let (Some(a), Some(b)) = (unsafe { (to_local(lhs), to_local(rhs)) }) else {
        return NAPI_INVALID_ARG;
    };
    unsafe { out(result, a.strict_equals(b)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_coerce_to_string(
    env: Env,
    value: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    match local.to_string(scope) {
        Some(string) => unsafe { out(result, from_local(string.into())) },
        None => NAPI_PENDING_EXCEPTION,
    }
}

// ============================================================== properties

unsafe fn as_object<'s>(value: NapiValue) -> Option<v8::Local<'s, v8::Object>> {
    let local = unsafe { to_local(value) }?;
    v8::Local::<v8::Object>::try_from(local).ok()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_set_named_property(
    env: Env,
    object: NapiValue,
    name: *const c_char,
    value: NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    let (Some(value), false) = (unsafe { to_local(value) }, name.is_null()) else {
        return NAPI_INVALID_ARG;
    };
    let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy();
    let Some(key) = v8::String::new(scope, &name) else {
        return NAPI_GENERIC_FAILURE;
    };
    target.set(scope, key.into(), value);
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_named_property(
    env: Env,
    object: NapiValue,
    name: *const c_char,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    if name.is_null() {
        return NAPI_INVALID_ARG;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy();
    let Some(key) = v8::String::new(scope, &name) else {
        return NAPI_GENERIC_FAILURE;
    };
    match target.get(scope, key.into()) {
        Some(value) => unsafe { out(result, from_local(value)) },
        None => NAPI_PENDING_EXCEPTION,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_has_named_property(
    env: Env,
    object: NapiValue,
    name: *const c_char,
    result: *mut bool,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    if name.is_null() {
        return NAPI_INVALID_ARG;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy();
    let Some(key) = v8::String::new(scope, &name) else {
        return NAPI_GENERIC_FAILURE;
    };
    let has = target.has(scope, key.into()).unwrap_or(false);
    unsafe { out(result, has) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_set_property(
    env: Env,
    object: NapiValue,
    key: NapiValue,
    value: NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    let (Some(key), Some(value)) = (unsafe { (to_local(key), to_local(value)) }) else {
        return NAPI_INVALID_ARG;
    };
    target.set(scope, key, value);
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_property(
    env: Env,
    object: NapiValue,
    key: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    let Some(key) = (unsafe { to_local(key) }) else {
        return NAPI_INVALID_ARG;
    };
    match target.get(scope, key) {
        Some(value) => unsafe { out(result, from_local(value)) },
        None => NAPI_PENDING_EXCEPTION,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_set_element(
    env: Env,
    object: NapiValue,
    index: u32,
    value: NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    let Some(value) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    target.set_index(scope, index, value);
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_element(
    env: Env,
    object: NapiValue,
    index: u32,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    match target.get_index(scope, index) {
        Some(value) => unsafe { out(result, from_local(value)) },
        None => NAPI_PENDING_EXCEPTION,
    }
}

// =============================================================== functions

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_function(
    env: Env,
    name: *const c_char,
    _length: usize,
    cb: unsafe extern "C" fn(Env, *mut CbInfo) -> NapiValue,
    data: *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // Store FnData in the env's owned vec so it drops with the env.
    // SAFETY: env is valid (env_scope checked it above); we push before
    // the External is created, so the pointer is stable for V8's lifetime.
    let env_ref = unsafe { &mut *env };
    env_ref.fn_data.push(Box::new(FnData { cb, data, env }));
    let fn_data: *mut FnData = &mut **env_ref.fn_data.last_mut().unwrap();
    let external = v8::External::new(scope, fn_data as *mut c_void);
    let Some(function) = v8::Function::builder(napi_trampoline)
        .data(external.into())
        .build(scope)
    else {
        return NAPI_GENERIC_FAILURE;
    };
    if !name.is_null() {
        let label = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy();
        if let Some(label) = v8::String::new(scope, &label) {
            function.set_name(label);
        }
    }
    unsafe { out(result, from_local(function.into())) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_cb_info(
    env: Env,
    cbinfo: *mut CbInfo,
    argc: *mut usize,
    argv: *mut NapiValue,
    this_arg: *mut NapiValue,
    data: *mut *mut c_void,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(info) = (unsafe { cbinfo.as_ref() }) else {
        return NAPI_INVALID_ARG;
    };
    unsafe {
        if !argc.is_null() {
            let wanted = *argc;
            if !argv.is_null() {
                for i in 0..wanted {
                    let value = info
                        .args
                        .get(i)
                        .copied()
                        .unwrap_or_else(|| from_local(v8::undefined(scope).into()));
                    argv.add(i).write(value);
                }
            }
            *argc = info.args.len();
        }
        if !this_arg.is_null() {
            *this_arg = info.this;
        }
        if !data.is_null() {
            *data = info.data;
        }
    }
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_call_function(
    env: Env,
    recv: NapiValue,
    func: NapiValue,
    argc: usize,
    argv: *const NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let (Some(recv), Some(func_value)) = (unsafe { (to_local(recv), to_local(func)) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(function) = v8::Local::<v8::Function>::try_from(func_value) else {
        return NAPI_FUNCTION_EXPECTED;
    };
    let args: Vec<v8::Local<v8::Value>> = (0..argc)
        .filter_map(|i| unsafe { to_local(*argv.add(i)) })
        .collect();
    match function.call(scope, recv, &args) {
        Some(value) => {
            if result.is_null() {
                NAPI_OK
            } else {
                unsafe { out(result, from_local(value)) }
            }
        }
        None => NAPI_PENDING_EXCEPTION,
    }
}

// ================================================================== errors

unsafe fn build_error(
    scope: &mut v8::PinScope<'_, '_>,
    code: *const c_char,
    msg: *const c_char,
    kind: fn(&mut v8::PinScope<'_, '_>, v8::Local<v8::String>) -> v8::Local<'static, v8::Value>,
) -> Option<v8::Global<v8::Value>> {
    let message = if msg.is_null() {
        "unknown native error".into()
    } else {
        unsafe { std::ffi::CStr::from_ptr(msg) }.to_string_lossy()
    };
    let message = v8::String::new(scope, &message)?;
    let error = kind(scope, message);
    if !code.is_null()
        && let Ok(object) = v8::Local::<v8::Object>::try_from(error)
    {
        let code_text = unsafe { std::ffi::CStr::from_ptr(code) }.to_string_lossy();
        let key = v8::String::new(scope, "code")?;
        if let Some(value) = v8::String::new(scope, &code_text) {
            object.set(scope, key.into(), value.into());
        }
    }
    Some(v8::Global::new(scope, error))
}

fn plain_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    message: v8::Local<v8::String>,
) -> v8::Local<'static, v8::Value> {
    let error = v8::Exception::error(scope, message);
    unsafe { std::mem::transmute::<v8::Local<v8::Value>, v8::Local<'static, v8::Value>>(error) }
}

fn type_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    message: v8::Local<v8::String>,
) -> v8::Local<'static, v8::Value> {
    let error = v8::Exception::type_error(scope, message);
    unsafe { std::mem::transmute::<v8::Local<v8::Value>, v8::Local<'static, v8::Value>>(error) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_throw(env: Env, error: NapiValue) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(error) }) else {
        return NAPI_INVALID_ARG;
    };
    let global = v8::Global::new(scope, local);
    unsafe { (*env).pending = Some(global) };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_throw_error(
    env: Env,
    code: *const c_char,
    msg: *const c_char,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    match unsafe { build_error(scope, code, msg, plain_error) } {
        Some(error) => {
            unsafe { (*env).pending = Some(error) };
            NAPI_OK
        }
        None => NAPI_GENERIC_FAILURE,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_throw_type_error(
    env: Env,
    code: *const c_char,
    msg: *const c_char,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    match unsafe { build_error(scope, code, msg, type_error) } {
        Some(error) => {
            unsafe { (*env).pending = Some(error) };
            NAPI_OK
        }
        None => NAPI_GENERIC_FAILURE,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_exception_pending(env: Env, result: *mut bool) -> NapiStatus {
    let Some(env_ref) = (unsafe { env.as_ref() }) else {
        return NAPI_INVALID_ARG;
    };
    unsafe { out(result, env_ref.pending.is_some()) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_and_clear_last_exception(
    env: Env,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let pending = unsafe { (*env).pending.take() };
    let value = match pending {
        Some(global) => from_local(v8::Local::new(scope, &global)),
        None => from_local(v8::undefined(scope).into()),
    };
    unsafe { out(result, value) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_version(env: Env, result: *mut u32) -> NapiStatus {
    if env.is_null() {
        return NAPI_INVALID_ARG;
    }
    unsafe { out(result, 8) } // N-API version 8 (the stable baseline)
}

// =========================================================== addon loading

type RegisterFn = unsafe extern "C" fn(Env, NapiValue) -> NapiValue;

/// Load a .node addon and run its napi_register_module_v1. Returns the
/// module's exports value, or None with a pending JS exception.
///
/// The `AddonRegistry` is retrieved from the isolate slot; both the
/// `NapiEnv` allocation and the `libloading::Library` handle are pushed
/// into it so they live until the owning `JsRuntime` drops.
pub(crate) fn load_addon<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    path: &std::path::Path,
) -> Option<v8::Local<'s, v8::Value>> {
    let library = match unsafe { libloading::Library::new(path) } {
        Ok(library) => library,
        Err(e) => {
            throw(scope, &format!("cannot load addon {}: {e}", path.display()));
            return None;
        }
    };
    let register: libloading::Symbol<RegisterFn> =
        match unsafe { library.get(b"napi_register_module_v1") } {
            Ok(symbol) => symbol,
            Err(e) => {
                throw(
                    scope,
                    &format!(
                        "{} is not an N-API addon (napi_register_module_v1 missing): {e}",
                        path.display()
                    ),
                );
                return None;
            }
        };
    let register: RegisterFn = *register;

    // Allocate a fresh NapiEnv.  Push it into the AddonRegistry slot so
    // its lifetime is tied to the JsRuntime.  We hold a raw pointer for
    // use during registration (the Box heap address is stable even as the
    // registry's Vec reallocates).
    let mut env_box = NapiEnv::new();
    let env: Env = &mut *env_box as *mut NapiEnv;
    scope
        .get_slot_mut::<AddonRegistry>()
        .expect("AddonRegistry slot installed")
        .envs
        .push(env_box);

    let exports = v8::Object::new(scope);
    let exports_value: v8::Local<v8::Value> = exports.into();

    unsafe {
        (*env).scope = scope as *mut v8::PinScope<'_, '_> as *mut c_void;
    }
    let result = unsafe { register(env, from_local(exports_value)) };
    unsafe {
        (*env).scope = std::ptr::null_mut();
    }

    if let Some(pending) = unsafe { (*env).pending.take() } {
        let exception = v8::Local::new(scope, &pending);
        scope.throw_exception(exception);
        return None;
    }
    scope
        .get_slot_mut::<AddonRegistry>()
        .expect("AddonRegistry slot installed")
        .libraries
        .push(library);

    match unsafe { to_local(result) } {
        Some(returned) => Some(returned),
        None => Some(exports_value),
    }
}

fn throw(scope: &mut v8::PinScope<'_, '_>, message: &str) {
    let message = v8::String::new(scope, message)
        .unwrap_or_else(|| v8::String::new(scope, "napi load error").unwrap());
    let exception = v8::Exception::error(scope, message);
    scope.throw_exception(exception);
}
