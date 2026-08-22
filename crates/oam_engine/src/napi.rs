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

/// A persistent reference to a V8 value. Stored in `NapiEnv::refs`.
struct NapiRefEntry {
    value: v8::Global<v8::Value>,
    refcount: u32,
}

/// A native-object wrapping: maps a JS object to a raw Rust pointer.
/// Stored in `NapiEnv::wraps` for `napi_wrap` / `napi_unwrap`.
struct NapiWrapEntry {
    object: v8::Global<v8::Value>,
    native: *mut c_void,
    finalize: Option<unsafe extern "C" fn(Env, *mut c_void, *mut c_void)>,
    hint: *mut c_void,
}

/// One env per loaded addon (Node's model).
///
/// Previously leaked for the process lifetime; now owned by an
/// `AddonRegistry` stored on `JsRuntime`.  The env drops when the
/// runtime drops, which also drops every `FnData` it owns.
pub struct NapiEnv {
    /// `*mut v8::PinScope<'_, '_>` while inside a native entry; null
    /// outside. Saved/restored re-entrantly by the trampolines.
    scope: *mut c_void,
    /// Deferred exception, thrown into V8 when the native frame returns.
    pending: Option<v8::Global<v8::Value>>,
    /// Every `FnData` created via `napi_create_function` / `napi_define_class`.
    /// `Box` required for pointer stability: addons hold raw `*mut FnData`.
    #[allow(clippy::vec_box)]
    fn_data: Vec<Box<FnData>>,
    /// Persistent references (napi_create_reference).
    #[allow(clippy::vec_box)]
    refs: Vec<Box<NapiRefEntry>>,
    /// Active napi_wrap entries for this env.
    #[allow(clippy::vec_box)]
    wraps: Vec<Box<NapiWrapEntry>>,
}

impl NapiEnv {
    fn new() -> Box<NapiEnv> {
        Box::new(NapiEnv {
            scope: std::ptr::null_mut(),
            pending: None,
            fn_data: Vec::new(),
            refs: Vec::new(),
            wraps: Vec::new(),
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
#[derive(Default)]
pub struct AddonRegistry {
    /// Owned `NapiEnv` allocations, one per successfully loaded addon.
    /// `Box` is required: the C ABI dispenses `*mut NapiEnv` and a flat
    /// `Vec<NapiEnv>` would move elements on realloc, invalidating those
    /// pointers (every addon would crash on the next napi callback).
    #[allow(clippy::vec_box)]
    envs: Vec<Box<NapiEnv>>,
    /// Loaded addon libraries.  Dropping a `Library` unloads the `.node`
    /// DLL; this must happen AFTER the envs are dropped so any destructor
    /// in the library can still reach the env.
    libraries: Vec<libloading::Library>,
}

impl AddonRegistry {
    pub fn new() -> Self {
        Self::default()
    }
}

// Thread-local drop counter: each `NapiEnv::drop` increments it.
// Used only in tests to verify that every load is matched by a drop.
// Not a doc comment -- rustdoc can't document items produced by a macro.
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

/// Callback registration payload, owned by `NapiEnv::fn_data`.
/// Drops when the env drops (i.e. when the owning JsRuntime drops).
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

// ================================================================= externals

/// `v8::External` wrapping a raw pointer.  The optional finalizer is
/// accepted but ignored for now (no GC finalizer hook yet).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_external(
    env: Env,
    data: *mut c_void,
    _finalize_cb: *mut c_void,
    _finalize_hint: *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let ext = v8::External::new(scope, data);
    unsafe { out(result, from_local(ext.into())) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_external(
    env: Env,
    value: NapiValue,
    result: *mut *mut c_void,
) -> NapiStatus {
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(ext) = v8::Local::<v8::External>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    if result.is_null() {
        return NAPI_INVALID_ARG;
    }
    unsafe { *result = ext.value() };
    NAPI_OK
}

// ================================================================ references
//
// NapiRef handle: an opaque *mut c_void that points to a heap-stable
// NapiRefEntry box owned by NapiEnv::refs.

type NapiRefHandle = *mut c_void;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_reference(
    env: Env,
    value: NapiValue,
    initial_refcount: u32,
    result: *mut NapiRefHandle,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let global = v8::Global::new(scope, local);
    let env_ref = unsafe { &mut *env };
    env_ref.refs.push(Box::new(NapiRefEntry {
        value: global,
        refcount: initial_refcount,
    }));
    let ptr = &mut **env_ref.refs.last_mut().unwrap() as *mut NapiRefEntry as *mut c_void;
    unsafe { out(result, ptr) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_delete_reference(env: Env, ref_: NapiRefHandle) -> NapiStatus {
    if env.is_null() || ref_.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env_ref = unsafe { &mut *env };
    let target = ref_ as *const NapiRefEntry;
    env_ref.refs.retain(|r| !std::ptr::eq(r.as_ref(), target));
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_reference_value(
    env: Env,
    ref_: NapiRefHandle,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    if ref_.is_null() {
        return NAPI_INVALID_ARG;
    }
    let entry = unsafe { &*(ref_ as *const NapiRefEntry) };
    let local = v8::Local::new(scope, &entry.value);
    unsafe { out(result, from_local(local)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_reference_ref(
    env: Env,
    ref_: NapiRefHandle,
    result: *mut u32,
) -> NapiStatus {
    if env.is_null() || ref_.is_null() {
        return NAPI_INVALID_ARG;
    }
    let entry = unsafe { &mut *(ref_ as *mut NapiRefEntry) };
    entry.refcount = entry.refcount.saturating_add(1);
    if !result.is_null() {
        unsafe { *result = entry.refcount };
    }
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_reference_unref(
    env: Env,
    ref_: NapiRefHandle,
    result: *mut u32,
) -> NapiStatus {
    if env.is_null() || ref_.is_null() {
        return NAPI_INVALID_ARG;
    }
    let entry = unsafe { &mut *(ref_ as *mut NapiRefEntry) };
    entry.refcount = entry.refcount.saturating_sub(1);
    if !result.is_null() {
        unsafe { *result = entry.refcount };
    }
    NAPI_OK
}

// =========================================================== wrap / classes

/// Property descriptor mirroring `napi_property_descriptor` from js_native_api.h.
/// Must match the C layout exactly (repr(C)).
#[repr(C)]
pub struct NapiPropertyDescriptor {
    pub utf8name: *const c_char,
    pub name: NapiValue,
    pub method: Option<unsafe extern "C" fn(Env, *mut CbInfo) -> NapiValue>,
    pub getter: Option<unsafe extern "C" fn(Env, *mut CbInfo) -> NapiValue>,
    pub setter: Option<unsafe extern "C" fn(Env, *mut CbInfo) -> NapiValue>,
    pub value: NapiValue,
    pub attributes: u32,
    pub data: *mut c_void,
}

const NAPI_ATTR_WRITABLE: u32 = 0x1;
const NAPI_ATTR_ENUMERABLE: u32 = 0x2;
const NAPI_ATTR_CONFIGURABLE: u32 = 0x4;
const NAPI_ATTR_STATIC: u32 = 0x400;

fn napi_attrs_to_v8(attrs: u32) -> v8::PropertyAttribute {
    let mut a = v8::PropertyAttribute::NONE;
    if attrs & NAPI_ATTR_WRITABLE == 0 {
        a = a | v8::PropertyAttribute::READ_ONLY;
    }
    if attrs & NAPI_ATTR_ENUMERABLE == 0 {
        a = a | v8::PropertyAttribute::DONT_ENUM;
    }
    if attrs & NAPI_ATTR_CONFIGURABLE == 0 {
        a = a | v8::PropertyAttribute::DONT_DELETE;
    }
    a
}

/// Create a native-backed JS class (constructor function + prototype methods).
///
/// - Properties with `napi_static` go on the constructor itself.
/// - Otherwise they go on `constructor.prototype`.
/// - `method` descriptors create bound functions via the existing trampoline.
/// - `value` descriptors set named values directly.
/// - `getter`/`setter` descriptors are accepted but silently skipped (beta
///   limitation: V8 accessor setup requires a separate AccessorCallback path
///   not yet wired to the napi trampoline).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_define_class(
    env: Env,
    utf8name: *const c_char,
    _length: usize,
    constructor: unsafe extern "C" fn(Env, *mut CbInfo) -> NapiValue,
    data: *mut c_void,
    property_count: usize,
    properties: *const NapiPropertyDescriptor,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // Create the constructor function via the shared trampoline.
    let mut ctor_out: NapiValue = std::ptr::null_mut();
    let status = unsafe {
        napi_create_function(env, utf8name, usize::MAX, constructor, data, &mut ctor_out)
    };
    if status != NAPI_OK {
        return status;
    }
    let Some(ctor_local) = (unsafe { to_local(ctor_out) }) else {
        return NAPI_GENERIC_FAILURE;
    };
    let Ok(ctor_fn) = v8::Local::<v8::Function>::try_from(ctor_local) else {
        return NAPI_GENERIC_FAILURE;
    };

    // Get `constructor.prototype` for instance methods.
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_val = ctor_fn.get(scope, proto_key.into());
    let proto_obj = proto_val
        .and_then(|v| v8::Local::<v8::Object>::try_from(v).ok())
        .unwrap_or_else(|| v8::Object::new(scope));

    let props = if property_count > 0 && !properties.is_null() {
        unsafe { std::slice::from_raw_parts(properties, property_count) }
    } else {
        &[]
    };

    for prop in props {
        // Resolve property name.
        let key: v8::Local<v8::Value> = if !prop.utf8name.is_null() {
            let name = unsafe { std::ffi::CStr::from_ptr(prop.utf8name) }.to_string_lossy();
            match v8::String::new(scope, &name) {
                Some(s) => s.into(),
                None => continue,
            }
        } else if let Some(name_val) = unsafe { to_local(prop.name) } {
            name_val
        } else {
            continue;
        };

        let target: v8::Local<v8::Object> = if prop.attributes & NAPI_ATTR_STATIC != 0 {
            ctor_fn.into()
        } else {
            proto_obj
        };

        // define_own_property needs Local<Name>; convert via try_from.
        // Both String and Symbol implement Name, so this should always succeed
        // for the UTF-8-name and napi_value-as-name paths.
        let name_key: v8::Local<v8::Name> = match v8::Local::<v8::Name>::try_from(key) {
            Ok(n) => n,
            Err(_) => continue,
        };

        if let Some(method_cb) = prop.method {
            // Method descriptor: create a function via the trampoline.
            let mut fn_out: NapiValue = std::ptr::null_mut();
            let status = unsafe {
                napi_create_function(
                    env,
                    prop.utf8name,
                    usize::MAX,
                    method_cb,
                    prop.data,
                    &mut fn_out,
                )
            };
            if status != NAPI_OK {
                continue;
            }
            if let Some(fn_val) = unsafe { to_local(fn_out) } {
                let attr = napi_attrs_to_v8(prop.attributes);
                target.define_own_property(scope, name_key, fn_val, attr);
            }
        } else if let Some(prop_val) = unsafe { to_local(prop.value) } {
            // Value descriptor.
            let attr = napi_attrs_to_v8(prop.attributes);
            target.define_own_property(scope, name_key, prop_val, attr);
        }
        // getter/setter: deferred -- require AccessorCallback wiring
    }

    unsafe { out(result, from_local(ctor_fn.into())) }
}

/// Store a raw Rust pointer on a JS object for later retrieval via `napi_unwrap`.
///
/// Any previous wrap on the same object is replaced. The optional finalizer
/// is stored but not yet called automatically (pending GC hook support).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_wrap(
    env: Env,
    js_object: NapiValue,
    native_object: *mut c_void,
    finalize_cb: Option<unsafe extern "C" fn(Env, *mut c_void, *mut c_void)>,
    finalize_hint: *mut c_void,
    result: *mut NapiRefHandle,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(js_object) }) else {
        return NAPI_INVALID_ARG;
    };
    let global = v8::Global::new(scope, local);
    let env_ref = unsafe { &mut *env };

    // Replace any existing wrap for this object.
    for existing in &mut env_ref.wraps {
        let existing_local = v8::Local::new(scope, &existing.object);
        if local.strict_equals(existing_local) {
            existing.native = native_object;
            existing.finalize = finalize_cb;
            existing.hint = finalize_hint;
            if !result.is_null() {
                unsafe { *result = std::ptr::null_mut() };
            }
            return NAPI_OK;
        }
    }

    env_ref.wraps.push(Box::new(NapiWrapEntry {
        object: global,
        native: native_object,
        finalize: finalize_cb,
        hint: finalize_hint,
    }));

    if !result.is_null() {
        unsafe { *result = std::ptr::null_mut() };
    }
    NAPI_OK
}

/// Retrieve the native pointer stored by `napi_wrap`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_unwrap(
    env: Env,
    js_object: NapiValue,
    result: *mut *mut c_void,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(js_object) }) else {
        return NAPI_INVALID_ARG;
    };
    let env_ref = unsafe { &*env };
    for wrap in &env_ref.wraps {
        let wrap_local = v8::Local::new(scope, &wrap.object);
        if local.strict_equals(wrap_local) {
            if !result.is_null() {
                unsafe { *result = wrap.native };
            }
            return NAPI_OK;
        }
    }
    NAPI_INVALID_ARG
}

/// Remove the wrap stored by `napi_wrap` and retrieve the native pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_remove_wrap(
    env: Env,
    js_object: NapiValue,
    result: *mut *mut c_void,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(js_object) }) else {
        return NAPI_INVALID_ARG;
    };
    let env_ref = unsafe { &mut *env };
    let mut found = None;
    env_ref.wraps.retain(|w| {
        let w_local = v8::Local::new(scope, &w.object);
        if local.strict_equals(w_local) {
            found = Some(w.native);
            false
        } else {
            true
        }
    });
    match found {
        Some(ptr) => {
            if !result.is_null() {
                unsafe { *result = ptr };
            }
            NAPI_OK
        }
        None => NAPI_INVALID_ARG,
    }
}

/// Call a constructor function with `new` to produce a new instance.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_new_instance(
    env: Env,
    constructor: NapiValue,
    argc: usize,
    argv: *const NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(ctor_local) = (unsafe { to_local(constructor) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(ctor_fn) = v8::Local::<v8::Function>::try_from(ctor_local) else {
        return NAPI_FUNCTION_EXPECTED;
    };
    let args: Vec<v8::Local<v8::Value>> = (0..argc)
        .filter_map(|i| unsafe { to_local(*argv.add(i)) })
        .collect();
    match ctor_fn.new_instance(scope, &args) {
        Some(obj) => unsafe { out(result, from_local(obj.into())) },
        None => NAPI_PENDING_EXCEPTION,
    }
}

/// Check `value instanceof constructor`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_instanceof(
    env: Env,
    object: NapiValue,
    constructor: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(obj_local) = (unsafe { to_local(object) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(ctor_local) = (unsafe { to_local(constructor) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(ctor_obj) = v8::Local::<v8::Object>::try_from(ctor_local) else {
        return NAPI_FUNCTION_EXPECTED;
    };
    match obj_local.instance_of(scope, ctor_obj) {
        Some(is) => unsafe { out(result, is) },
        None => NAPI_PENDING_EXCEPTION,
    }
}

// =================================================================== bigint

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_bigint_int64(
    env: Env,
    value: i64,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    unsafe {
        out(
            result,
            from_local(v8::BigInt::new_from_i64(scope, value).into()),
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_bigint_uint64(
    env: Env,
    value: u64,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    unsafe {
        out(
            result,
            from_local(v8::BigInt::new_from_u64(scope, value).into()),
        )
    }
}

/// Returns `(value, lossless)` where `lossless` is false if the BigInt
/// is too large to represent exactly as i64.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_bigint_int64(
    env: Env,
    value: NapiValue,
    result: *mut i64,
    lossless: *mut bool,
) -> NapiStatus {
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(bigint) = v8::Local::<v8::BigInt>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    let (val, ok) = bigint.i64_value();
    if !result.is_null() {
        unsafe { *result = val };
    }
    if !lossless.is_null() {
        unsafe { *lossless = ok };
    }
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_bigint_uint64(
    env: Env,
    value: NapiValue,
    result: *mut u64,
    lossless: *mut bool,
) -> NapiStatus {
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(bigint) = v8::Local::<v8::BigInt>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    let (val, ok) = bigint.u64_value();
    if !result.is_null() {
        unsafe { *result = val };
    }
    if !lossless.is_null() {
        unsafe { *lossless = ok };
    }
    NAPI_OK
}

// =================================================================== buffers

/// Create a zero-initialized Buffer of `size` bytes. Returns a Uint8Array
/// view backed by an ArrayBuffer, plus a pointer to the raw data.
///
/// The returned pointer is valid as long as the V8 heap owns the backing store
/// (i.e. while the ArrayBuffer returned in `result` is alive).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_buffer(
    env: Env,
    size: usize,
    data: *mut *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let ab = v8::ArrayBuffer::new(scope, size);
    if !data.is_null() {
        let store = ab.get_backing_store();
        let ptr = store
            .data()
            .map(|p| p.as_ptr())
            .unwrap_or(std::ptr::null_mut());
        unsafe { *data = ptr };
    }
    let Some(view) = v8::Uint8Array::new(scope, ab, 0, size) else {
        return NAPI_GENERIC_FAILURE;
    };
    unsafe { out(result, from_local(view.into())) }
}

/// Create a Buffer, copy `size` bytes from `data` into it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_buffer_copy(
    env: Env,
    size: usize,
    data: *const c_void,
    result_data: *mut *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let ab = v8::ArrayBuffer::new(scope, size);
    let store = ab.get_backing_store();
    if let Some(dst) = store.data() {
        let src_bytes = unsafe { std::slice::from_raw_parts(data as *const u8, size) };
        let dst_bytes = unsafe { std::slice::from_raw_parts_mut(dst.as_ptr() as *mut u8, size) };
        dst_bytes.copy_from_slice(src_bytes);
        if !result_data.is_null() {
            unsafe { *result_data = dst.as_ptr() };
        }
    }
    let Some(view) = v8::Uint8Array::new(scope, ab, 0, size) else {
        return NAPI_GENERIC_FAILURE;
    };
    unsafe { out(result, from_local(view.into())) }
}

/// Create an external buffer: an ArrayBuffer that wraps existing memory.
/// The finalizer is called when V8 GC collects the ArrayBuffer.
///
/// Note: the finalizer is stored for completeness but is not called until GC
/// hook support is added. Callers that MUST run cleanup on collection should
/// use napi_create_buffer_copy instead (which copies into V8-managed memory).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_external_buffer(
    env: Env,
    size: usize,
    data: *mut c_void,
    _finalize_cb: *mut c_void,
    _finalize_hint: *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, size) };
    let store = v8::ArrayBuffer::new_backing_store_from_bytes(bytes.to_vec().into_boxed_slice());
    let ab = v8::ArrayBuffer::with_backing_store(scope, &store.make_shared());
    let Some(view) = v8::Uint8Array::new(scope, ab, 0, size) else {
        return NAPI_GENERIC_FAILURE;
    };
    unsafe { out(result, from_local(view.into())) }
}

/// Returns true for any TypedArray (Uint8Array, Buffer, etc.).
/// Node's napi_is_buffer is Buffer-specific, but for oam we treat any
/// byte-array view as a buffer — addons that pass Uint8Array/Buffer both
/// work correctly, which is the relevant use case.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_buffer(
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
    unsafe { out(result, local.is_uint8_array()) }
}

/// Get a pointer to the underlying bytes of a TypedArray or ArrayBuffer.
/// For a TypedArray, accounts for byte_offset so `*data` points to element 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_buffer_info(
    env: Env,
    value: NapiValue,
    data: *mut *mut c_void,
    length: *mut usize,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };

    if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(local) {
        let ab = match view.buffer(scope) {
            Some(ab) => ab,
            None => return NAPI_GENERIC_FAILURE,
        };
        let store = ab.get_backing_store();
        let byte_off = view.byte_offset();
        let byte_len = view.byte_length();
        if !data.is_null() {
            let base = store
                .data()
                .map(|p| p.as_ptr() as *mut u8)
                .unwrap_or(std::ptr::null_mut());
            unsafe { *data = base.add(byte_off) as *mut c_void };
        }
        if !length.is_null() {
            unsafe { *length = byte_len };
        }
        NAPI_OK
    } else if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(local) {
        let store = ab.get_backing_store();
        if !data.is_null() {
            let ptr = store
                .data()
                .map(|p| p.as_ptr())
                .unwrap_or(std::ptr::null_mut());
            unsafe { *data = ptr };
        }
        if !length.is_null() {
            unsafe { *length = ab.byte_length() };
        }
        NAPI_OK
    } else {
        NAPI_INVALID_ARG
    }
}

// ======== gamma wave: full napi-sys-2.4.0 surface ========

/// Extended error info returned by napi_get_last_error_info.
#[repr(C)]
pub struct NapiExtendedErrorInfo {
    pub error_message: *const c_char,
    pub engine_reserved: *mut c_void,
    pub engine_error_code: u32,
    pub error_code: NapiStatus,
}
// SAFETY: only ever read from C side; never mutated after initialisation.
unsafe impl Sync for NapiExtendedErrorInfo {}

/// Node.js version info returned by napi_get_node_version.
#[repr(C)]
pub struct NapiNodeVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub release: *const c_char,
}
// SAFETY: only ever read from C side; never mutated after initialisation.
unsafe impl Sync for NapiNodeVersion {}

static NODE_VERSION: NapiNodeVersion = NapiNodeVersion {
    major: 22,
    minor: 0,
    patch: 0,
    release: c"node".as_ptr(),
};

// uv_event_loop: a data symbol addons may look up via GetProcAddress.
// Exported as a null pointer (oam has no libuv event loop).
#[unsafe(no_mangle)]
pub static mut uv_event_loop: *mut c_void = std::ptr::null_mut();

// ------------------------------------------------------------------ errors

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_last_error_info(
    _env: Env,
    result: *mut *const NapiExtendedErrorInfo,
) -> NapiStatus {
    // Return a static zeroed struct -- callers inspect it after a failed call.
    static EMPTY: NapiExtendedErrorInfo = NapiExtendedErrorInfo {
        error_message: std::ptr::null(),
        engine_reserved: std::ptr::null_mut(),
        engine_error_code: 0,
        error_code: 0,
    };
    if !result.is_null() {
        unsafe { *result = &EMPTY as *const NapiExtendedErrorInfo };
    }
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_error(
    env: Env,
    _code: NapiValue,
    msg: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(msg_local) = (unsafe { to_local(msg) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(msg_str) = v8::Local::<v8::String>::try_from(msg_local) else {
        return NAPI_STRING_EXPECTED;
    };
    let error = v8::Exception::error(scope, msg_str);
    unsafe { out(result, from_local(error)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_type_error(
    env: Env,
    _code: NapiValue,
    msg: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(msg_local) = (unsafe { to_local(msg) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(msg_str) = v8::Local::<v8::String>::try_from(msg_local) else {
        return NAPI_STRING_EXPECTED;
    };
    let error = v8::Exception::type_error(scope, msg_str);
    unsafe { out(result, from_local(error)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_range_error(
    env: Env,
    _code: NapiValue,
    msg: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(msg_local) = (unsafe { to_local(msg) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(msg_str) = v8::Local::<v8::String>::try_from(msg_local) else {
        return NAPI_STRING_EXPECTED;
    };
    let error = v8::Exception::range_error(scope, msg_str);
    unsafe { out(result, from_local(error)) }
}

fn range_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    message: v8::Local<v8::String>,
) -> v8::Local<'static, v8::Value> {
    let error = v8::Exception::range_error(scope, message);
    unsafe { std::mem::transmute::<v8::Local<v8::Value>, v8::Local<'static, v8::Value>>(error) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_throw_range_error(
    env: Env,
    code: *const c_char,
    msg: *const c_char,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    match unsafe { build_error(scope, code, msg, range_error) } {
        Some(error) => {
            unsafe { (*env).pending = Some(error) };
            NAPI_OK
        }
        None => NAPI_GENERIC_FAILURE,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_error(
    env: Env,
    value: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    // An Error is an object; check by seeing if it has a 'message' property
    // and is an instance of Error via the global Error constructor.
    let is_err = if let Ok(obj) = v8::Local::<v8::Object>::try_from(local) {
        let ctx = scope.get_current_context();
        let global = ctx.global(scope);
        let error_key = v8::String::new(scope, "Error").unwrap();
        if let Some(error_ctor) = global.get(scope, error_key.into()) {
            if let Ok(ctor_obj) = v8::Local::<v8::Object>::try_from(error_ctor) {
                obj.instance_of(scope, ctor_obj).unwrap_or(false)
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };
    unsafe { out(result, is_err) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_fatal_error(
    location: *const c_char,
    _loc_len: usize,
    message: *const c_char,
    _msg_len: usize,
) -> ! {
    let loc = if location.is_null() {
        "<unknown>".to_string()
    } else {
        unsafe { std::ffi::CStr::from_ptr(location) }
            .to_string_lossy()
            .into_owned()
    };
    let msg = if message.is_null() {
        "<no message>".to_string()
    } else {
        unsafe { std::ffi::CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    };
    eprintln!("FATAL ERROR: {loc}: {msg}");
    std::process::abort()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_fatal_exception(env: Env, _err: NapiValue) -> NapiStatus {
    let _ = env;
    NAPI_GENERIC_FAILURE
}

// ------------------------------------------------------------------ strings

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_string_latin1(
    env: Env,
    str_ptr: *const c_char,
    length: usize,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    if str_ptr.is_null() {
        return NAPI_INVALID_ARG;
    }
    let bytes: &[u8] = unsafe {
        if length == usize::MAX {
            std::ffi::CStr::from_ptr(str_ptr).to_bytes()
        } else {
            std::slice::from_raw_parts(str_ptr as *const u8, length)
        }
    };
    let Some(s) = v8::String::new_from_one_byte(scope, bytes, v8::NewStringType::Normal) else {
        return NAPI_GENERIC_FAILURE;
    };
    unsafe { out(result, from_local(s.into())) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_string_utf16(
    env: Env,
    str_ptr: *const u16,
    length: usize,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    if str_ptr.is_null() {
        return NAPI_INVALID_ARG;
    }
    let units: &[u16] = unsafe {
        if length == usize::MAX {
            // NUL-terminated UTF-16: find the terminator
            let mut len = 0usize;
            while *str_ptr.add(len) != 0 {
                len += 1;
            }
            std::slice::from_raw_parts(str_ptr, len)
        } else {
            std::slice::from_raw_parts(str_ptr, length)
        }
    };
    let Some(s) = v8::String::new_from_two_byte(scope, units, v8::NewStringType::Normal) else {
        return NAPI_GENERIC_FAILURE;
    };
    unsafe { out(result, from_local(s.into())) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_string_latin1(
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
    // Latin-1 length = number of UTF-16 code units (chars) for BMP strings
    let char_len = string.length();
    if buf.is_null() {
        return unsafe { out(result, char_len) };
    }
    if bufsize == 0 {
        return unsafe { out(result, 0) };
    }
    // Write latin-1 bytes via write_one_byte_v2.
    let copy_len = char_len.min(bufsize - 1);
    let buf_slice = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, copy_len) };
    string.write_one_byte_v2(scope, 0, buf_slice, v8::WriteFlags::empty());
    unsafe { *(buf.add(copy_len)) = 0 };
    if !result.is_null() {
        unsafe { *result = copy_len };
    }
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_string_utf16(
    env: Env,
    value: NapiValue,
    buf: *mut u16,
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
    let char_len = string.length();
    if buf.is_null() {
        return unsafe { out(result, char_len) };
    }
    if bufsize == 0 {
        return unsafe { out(result, 0) };
    }
    let copy_len = char_len.min(bufsize - 1);
    let buf_slice = unsafe { std::slice::from_raw_parts_mut(buf, copy_len) };
    string.write_v2(scope, 0, buf_slice, v8::WriteFlags::empty());
    unsafe { *(buf.add(copy_len)) = 0u16 };
    if !result.is_null() {
        unsafe { *result = copy_len };
    }
    NAPI_OK
}

// ------------------------------------------------------------------ symbols

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_symbol(
    env: Env,
    description: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let desc = if description.is_null() {
        None
    } else {
        unsafe { to_local(description) }.and_then(|v| v8::Local::<v8::String>::try_from(v).ok())
    };
    let sym = v8::Symbol::new(scope, desc);
    unsafe { out(result, from_local(sym.into())) }
}

// ------------------------------------------------------------------ handle scopes (stubs)

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_open_handle_scope(_env: Env, result: *mut *mut c_void) -> NapiStatus {
    // We don't implement real handle scopes -- the V8 PinScope on the stack
    // serves the same purpose. Return a sentinel so callers don't null-deref.
    if !result.is_null() {
        unsafe { *result = std::ptr::dangling_mut::<c_void>() };
    }
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_close_handle_scope(_env: Env, _scope: *mut c_void) -> NapiStatus {
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_open_escapable_handle_scope(
    _env: Env,
    result: *mut *mut c_void,
) -> NapiStatus {
    if !result.is_null() {
        unsafe { *result = std::ptr::dangling_mut::<c_void>() };
    }
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_close_escapable_handle_scope(
    _env: Env,
    _scope: *mut c_void,
) -> NapiStatus {
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_escape_handle(
    _env: Env,
    _scope: *mut c_void,
    escapee: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // No real escapable scope needed; just forward the value.
    if result.is_null() {
        return NAPI_INVALID_ARG;
    }
    unsafe { *result = escapee };
    NAPI_OK
}

// ------------------------------------------------------------------ callback scope stubs

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_open_callback_scope(
    _env: Env,
    _resource_object: NapiValue,
    _context: *mut c_void,
    result: *mut *mut c_void,
) -> NapiStatus {
    if !result.is_null() {
        unsafe { *result = std::ptr::dangling_mut::<c_void>() };
    }
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_close_callback_scope(_env: Env, _scope: *mut c_void) -> NapiStatus {
    NAPI_OK
}

// ------------------------------------------------------------------ properties

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_has_property(
    env: Env,
    object: NapiValue,
    key: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    let Some(key_local) = (unsafe { to_local(key) }) else {
        return NAPI_INVALID_ARG;
    };
    let has = target.has(scope, key_local).unwrap_or(false);
    unsafe { out(result, has) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_delete_property(
    env: Env,
    object: NapiValue,
    key: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    let Some(key_local) = (unsafe { to_local(key) }) else {
        return NAPI_INVALID_ARG;
    };
    let deleted = target.delete(scope, key_local).unwrap_or(false);
    if !result.is_null() {
        unsafe { *result = deleted };
    }
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_has_own_property(
    env: Env,
    object: NapiValue,
    key: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    let Some(key_local) = (unsafe { to_local(key) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(key_name) = v8::Local::<v8::Name>::try_from(key_local) else {
        return NAPI_INVALID_ARG;
    };
    let has = target.has_own_property(scope, key_name).unwrap_or(false);
    unsafe { out(result, has) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_has_element(
    env: Env,
    object: NapiValue,
    index: u32,
    result: *mut bool,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    let has = target.has_index(scope, index).unwrap_or(false);
    unsafe { out(result, has) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_delete_element(
    env: Env,
    object: NapiValue,
    index: u32,
    result: *mut bool,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    let deleted = target.delete_index(scope, index).unwrap_or(false);
    if !result.is_null() {
        unsafe { *result = deleted };
    }
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_define_properties(
    env: Env,
    object: NapiValue,
    count: usize,
    props: *const NapiPropertyDescriptor,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    if count == 0 || props.is_null() {
        return NAPI_OK;
    }
    let descriptors = unsafe { std::slice::from_raw_parts(props, count) };
    for prop in descriptors {
        let key: v8::Local<v8::Value> = if !prop.utf8name.is_null() {
            let name = unsafe { std::ffi::CStr::from_ptr(prop.utf8name) }.to_string_lossy();
            match v8::String::new(scope, &name) {
                Some(s) => s.into(),
                None => continue,
            }
        } else if let Some(name_val) = unsafe { to_local(prop.name) } {
            name_val
        } else {
            continue;
        };
        let name_key: v8::Local<v8::Name> = match v8::Local::<v8::Name>::try_from(key) {
            Ok(n) => n,
            Err(_) => continue,
        };
        if let Some(method_cb) = prop.method {
            let mut fn_out: NapiValue = std::ptr::null_mut();
            let st = unsafe {
                napi_create_function(
                    env,
                    prop.utf8name,
                    usize::MAX,
                    method_cb,
                    prop.data,
                    &mut fn_out,
                )
            };
            if st != NAPI_OK {
                continue;
            }
            if let Some(fn_val) = unsafe { to_local(fn_out) } {
                let attr = napi_attrs_to_v8(prop.attributes);
                target.define_own_property(scope, name_key, fn_val, attr);
            }
        } else if let Some(prop_val) = unsafe { to_local(prop.value) } {
            let attr = napi_attrs_to_v8(prop.attributes);
            target.define_own_property(scope, name_key, prop_val, attr);
        }
    }
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_property_names(
    env: Env,
    object: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    match target.get_property_names(scope, v8::GetPropertyNamesArgs::default()) {
        Some(names) => unsafe { out(result, from_local(names.into())) },
        None => NAPI_PENDING_EXCEPTION,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_prototype(
    env: Env,
    object: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    let proto = target.get_prototype(scope);
    match proto {
        Some(p) => unsafe { out(result, from_local(p)) },
        None => unsafe { out(result, from_local(v8::null(scope).into())) },
    }
}

// ------------------------------------------------------------------ callbacks

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_new_target(
    _env: Env,
    cbinfo: *mut CbInfo,
    result: *mut NapiValue,
) -> NapiStatus {
    // CbInfo doesn't carry new.target yet; return undefined.
    let _ = cbinfo;
    if result.is_null() {
        return NAPI_INVALID_ARG;
    }
    // Signal "not called as constructor" by writing null.
    unsafe { *result = std::ptr::null_mut() };
    NAPI_OK
}

// ------------------------------------------------------------------ coercions

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_coerce_to_bool(
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
    let b = local.to_boolean(scope);
    unsafe { out(result, from_local(b.into())) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_coerce_to_number(
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
    match local.to_number(scope) {
        Some(n) => unsafe { out(result, from_local(n.into())) },
        None => NAPI_PENDING_EXCEPTION,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_coerce_to_object(
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
    match local.to_object(scope) {
        Some(obj) => unsafe { out(result, from_local(obj.into())) },
        None => NAPI_PENDING_EXCEPTION,
    }
}

// ------------------------------------------------------------------ arraybuffers

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_arraybuffer(
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
    unsafe { out(result, local.is_array_buffer()) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_arraybuffer(
    env: Env,
    size: usize,
    data: *mut *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let ab = v8::ArrayBuffer::new(scope, size);
    if !data.is_null() {
        let store = ab.get_backing_store();
        let ptr = store
            .data()
            .map(|p| p.as_ptr())
            .unwrap_or(std::ptr::null_mut());
        unsafe { *data = ptr };
    }
    unsafe { out(result, from_local(ab.into())) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_external_arraybuffer(
    env: Env,
    external_data: *mut c_void,
    byte_length: usize,
    _finalize_cb: *mut c_void,
    _finalize_hint: *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let bytes = unsafe { std::slice::from_raw_parts(external_data as *const u8, byte_length) };
    let store = v8::ArrayBuffer::new_backing_store_from_bytes(bytes.to_vec().into_boxed_slice());
    let ab = v8::ArrayBuffer::with_backing_store(scope, &store.make_shared());
    unsafe { out(result, from_local(ab.into())) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_arraybuffer_info(
    env: Env,
    ab_value: NapiValue,
    data: *mut *mut c_void,
    length: *mut usize,
) -> NapiStatus {
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(ab_value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    let store = ab.get_backing_store();
    if !data.is_null() {
        unsafe {
            *data = store
                .data()
                .map(|p| p.as_ptr())
                .unwrap_or(std::ptr::null_mut())
        };
    }
    if !length.is_null() {
        unsafe { *length = ab.byte_length() };
    }
    NAPI_OK
}

// ------------------------------------------------------------------ typedarrays

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_typedarray(
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
    unsafe { out(result, local.is_typed_array()) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_typedarray(
    env: Env,
    type_: u32,
    count: usize,
    ab_value: NapiValue,
    offset: usize,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(ab_value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    let view: Option<v8::Local<v8::Value>> = match type_ {
        0 => v8::Int8Array::new(scope, ab, offset, count).map(|v| v.into()),
        1 => v8::Uint8Array::new(scope, ab, offset, count).map(|v| v.into()),
        2 => v8::Uint8ClampedArray::new(scope, ab, offset, count).map(|v| v.into()),
        3 => v8::Int16Array::new(scope, ab, offset, count).map(|v| v.into()),
        4 => v8::Uint16Array::new(scope, ab, offset, count).map(|v| v.into()),
        5 => v8::Int32Array::new(scope, ab, offset, count).map(|v| v.into()),
        6 => v8::Uint32Array::new(scope, ab, offset, count).map(|v| v.into()),
        7 => v8::Float32Array::new(scope, ab, offset, count).map(|v| v.into()),
        8 => v8::Float64Array::new(scope, ab, offset, count).map(|v| v.into()),
        9 => v8::BigInt64Array::new(scope, ab, offset, count).map(|v| v.into()),
        10 => v8::BigUint64Array::new(scope, ab, offset, count).map(|v| v.into()),
        _ => return NAPI_INVALID_ARG,
    };
    match view {
        Some(v) => unsafe { out(result, from_local(v)) },
        None => NAPI_GENERIC_FAILURE,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_typedarray_info(
    env: Env,
    ta_value: NapiValue,
    type_out: *mut u32,
    count_out: *mut usize,
    data_out: *mut *mut c_void,
    ab_out: *mut NapiValue,
    offset_out: *mut usize,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(ta_value) }) else {
        return NAPI_INVALID_ARG;
    };
    // Determine the element type by checking each concrete typed array type.
    let type_id: u32;
    let element_size: usize;
    if let Ok(v) = v8::Local::<v8::Int8Array>::try_from(local) {
        type_id = 0;
        element_size = 1;
        let _ = v;
    } else if let Ok(v) = v8::Local::<v8::Uint8Array>::try_from(local) {
        type_id = 1;
        element_size = 1;
        let _ = v;
    } else if let Ok(v) = v8::Local::<v8::Uint8ClampedArray>::try_from(local) {
        type_id = 2;
        element_size = 1;
        let _ = v;
    } else if let Ok(v) = v8::Local::<v8::Int16Array>::try_from(local) {
        type_id = 3;
        element_size = 2;
        let _ = v;
    } else if let Ok(v) = v8::Local::<v8::Uint16Array>::try_from(local) {
        type_id = 4;
        element_size = 2;
        let _ = v;
    } else if let Ok(v) = v8::Local::<v8::Int32Array>::try_from(local) {
        type_id = 5;
        element_size = 4;
        let _ = v;
    } else if let Ok(v) = v8::Local::<v8::Uint32Array>::try_from(local) {
        type_id = 6;
        element_size = 4;
        let _ = v;
    } else if let Ok(v) = v8::Local::<v8::Float32Array>::try_from(local) {
        type_id = 7;
        element_size = 4;
        let _ = v;
    } else if let Ok(v) = v8::Local::<v8::Float64Array>::try_from(local) {
        type_id = 8;
        element_size = 8;
        let _ = v;
    } else if let Ok(v) = v8::Local::<v8::BigInt64Array>::try_from(local) {
        type_id = 9;
        element_size = 8;
        let _ = v;
    } else if let Ok(v) = v8::Local::<v8::BigUint64Array>::try_from(local) {
        type_id = 10;
        element_size = 8;
        let _ = v;
    } else {
        return NAPI_INVALID_ARG;
    }
    let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    let byte_off = view.byte_offset();
    let byte_len = view.byte_length();
    let elem_count = byte_len.checked_div(element_size).unwrap_or(0);
    if !type_out.is_null() {
        unsafe { *type_out = type_id };
    }
    if !count_out.is_null() {
        unsafe { *count_out = elem_count };
    }
    if !offset_out.is_null() {
        unsafe { *offset_out = byte_off };
    }
    if let Some(ab) = view.buffer(scope) {
        if !ab_out.is_null() {
            unsafe { *ab_out = from_local(ab.into()) };
        }
        if !data_out.is_null() {
            let store = ab.get_backing_store();
            let base = store
                .data()
                .map(|p| p.as_ptr() as *mut u8)
                .unwrap_or(std::ptr::null_mut());
            unsafe { *data_out = base.add(byte_off) as *mut c_void };
        }
    }
    NAPI_OK
}

// ------------------------------------------------------------------ dataview

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_dataview(
    env: Env,
    size: usize,
    ab_value: NapiValue,
    offset: usize,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(ab_value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    let dv = v8::DataView::new(scope, ab, offset, size);
    unsafe { out(result, from_local(dv.into())) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_dataview(
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
    unsafe { out(result, local.is_data_view()) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_dataview_info(
    env: Env,
    dv_value: NapiValue,
    byte_length: *mut usize,
    data: *mut *mut c_void,
    ab_out: *mut NapiValue,
    offset: *mut usize,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(dv_value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    let byte_off = view.byte_offset();
    let byte_len = view.byte_length();
    if !byte_length.is_null() {
        unsafe { *byte_length = byte_len };
    }
    if !offset.is_null() {
        unsafe { *offset = byte_off };
    }
    if let Some(ab) = view.buffer(scope) {
        if !ab_out.is_null() {
            unsafe { *ab_out = from_local(ab.into()) };
        }
        if !data.is_null() {
            let store = ab.get_backing_store();
            let base = store
                .data()
                .map(|p| p.as_ptr() as *mut u8)
                .unwrap_or(std::ptr::null_mut());
            unsafe { *data = base.add(byte_off) as *mut c_void };
        }
    }
    NAPI_OK
}

// ------------------------------------------------------------------ promises

/// Heap-allocated resolver, dispensed as an opaque deferred handle.
struct DeferredEntry {
    resolver: v8::Global<v8::PromiseResolver>,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_promise(
    env: Env,
    deferred: *mut *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        return NAPI_GENERIC_FAILURE;
    };
    let promise = resolver.get_promise(scope);
    let global_resolver = v8::Global::new(scope, resolver);
    let entry = Box::new(DeferredEntry {
        resolver: global_resolver,
    });
    let ptr = Box::into_raw(entry) as *mut c_void;
    if !deferred.is_null() {
        unsafe { *deferred = ptr };
    }
    unsafe { out(result, from_local(promise.into())) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_resolve_deferred(
    env: Env,
    deferred: *mut c_void,
    resolution: NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    if deferred.is_null() {
        return NAPI_INVALID_ARG;
    }
    // Validate the resolution BEFORE taking ownership of the deferred. N-API
    // frees the deferred only on success; if Box::from_raw ran first, this
    // invalid-arg return would drop (free) the DeferredEntry while the caller
    // still owns the handle, turning a contract-conforming retry into a
    // use-after-free / double-free.
    let Some(value) = (unsafe { to_local(resolution) }) else {
        return NAPI_INVALID_ARG;
    };
    let entry = unsafe { Box::from_raw(deferred as *mut DeferredEntry) };
    let resolver = v8::Local::new(scope, &entry.resolver);
    resolver.resolve(scope, value);
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_reject_deferred(
    env: Env,
    deferred: *mut c_void,
    rejection: NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    if deferred.is_null() {
        return NAPI_INVALID_ARG;
    }
    // Validate the rejection BEFORE taking ownership of the deferred (see
    // napi_resolve_deferred): freeing the DeferredEntry on this invalid-arg
    // path would dangle the caller's handle and make a retry a use-after-free.
    let Some(value) = (unsafe { to_local(rejection) }) else {
        return NAPI_INVALID_ARG;
    };
    let entry = unsafe { Box::from_raw(deferred as *mut DeferredEntry) };
    let resolver = v8::Local::new(scope, &entry.resolver);
    resolver.reject(scope, value);
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_promise(
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
    unsafe { out(result, local.is_promise()) }
}

// ------------------------------------------------------------------ script

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_run_script(
    env: Env,
    script: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(script) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(script_str) = v8::Local::<v8::String>::try_from(local) else {
        return NAPI_STRING_EXPECTED;
    };
    let source = v8::Script::compile(scope, script_str, None);
    let Some(compiled) = source else {
        return NAPI_PENDING_EXCEPTION;
    };
    match compiled.run(scope) {
        Some(val) => {
            if result.is_null() {
                NAPI_OK
            } else {
                unsafe { out(result, from_local(val)) }
            }
        }
        None => NAPI_PENDING_EXCEPTION,
    }
}

// ------------------------------------------------------------------ memory

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_adjust_external_memory(
    _env: Env,
    _change: i64,
    adjusted: *mut i64,
) -> NapiStatus {
    if !adjusted.is_null() {
        unsafe { *adjusted = 0 };
    }
    NAPI_OK
}

// ------------------------------------------------------------------ dates

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_date(
    env: Env,
    time: f64,
    result: *mut NapiValue,
) -> NapiStatus {
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    match v8::Date::new(scope, time) {
        Some(date) => unsafe { out(result, from_local(date.into())) },
        None => NAPI_GENERIC_FAILURE,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_date(env: Env, value: NapiValue, result: *mut bool) -> NapiStatus {
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    unsafe { out(result, local.is_date()) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_date_value(
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
    let Ok(date) = v8::Local::<v8::Date>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    unsafe { out(result, date.number_value(scope).unwrap_or(f64::NAN)) }
}

// ------------------------------------------------------------------ version / event loop

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_node_version(
    _env: Env,
    version: *mut *const NapiNodeVersion,
) -> NapiStatus {
    if !version.is_null() {
        unsafe { *version = &NODE_VERSION as *const NapiNodeVersion };
    }
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_uv_event_loop(_env: Env, loop_: *mut *mut c_void) -> NapiStatus {
    if !loop_.is_null() {
        unsafe { *loop_ = std::ptr::null_mut() };
    }
    NAPI_GENERIC_FAILURE
}

// ------------------------------------------------------------------ module registration

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_module_register(_mod: *mut c_void) -> NapiStatus {
    // Old-style napi_module registration -- not used by napi-sys 2.x.
    NAPI_OK
}

// ------------------------------------------------------------------ cleanup hooks

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_add_env_cleanup_hook(
    _env: Env,
    _fun: *mut c_void,
    _arg: *mut c_void,
) -> NapiStatus {
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_remove_env_cleanup_hook(
    _env: Env,
    _fun: *mut c_void,
    _arg: *mut c_void,
) -> NapiStatus {
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_add_finalizer(
    _env: Env,
    _js_object: NapiValue,
    _native_object: *mut c_void,
    _finalize_cb: *mut c_void,
    _finalize_hint: *mut c_void,
    _result: *mut *mut c_void,
) -> NapiStatus {
    NAPI_OK
}

// ------------------------------------------------------------------ async (stubs -- not supported)

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_async_init(
    _env: Env,
    _resource: NapiValue,
    _name: NapiValue,
    _result: *mut *mut c_void,
) -> NapiStatus {
    NAPI_GENERIC_FAILURE
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_async_destroy(_env: Env, _context: *mut c_void) -> NapiStatus {
    NAPI_GENERIC_FAILURE
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_make_callback(
    _env: Env,
    _context: *mut c_void,
    _recv: NapiValue,
    _func: NapiValue,
    _argc: usize,
    _argv: *const NapiValue,
    _result: *mut NapiValue,
) -> NapiStatus {
    NAPI_GENERIC_FAILURE
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_async_work(
    _env: Env,
    _resource: NapiValue,
    _name: NapiValue,
    _execute: *mut c_void,
    _complete: *mut c_void,
    _data: *mut c_void,
    _result: *mut *mut c_void,
) -> NapiStatus {
    NAPI_GENERIC_FAILURE
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_delete_async_work(_env: Env, _work: *mut c_void) -> NapiStatus {
    NAPI_GENERIC_FAILURE
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_queue_async_work(_env: Env, _work: *mut c_void) -> NapiStatus {
    NAPI_GENERIC_FAILURE
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_cancel_async_work(_env: Env, _work: *mut c_void) -> NapiStatus {
    NAPI_GENERIC_FAILURE
}

// ------------------------------------------------------------------ threadsafe functions
//
// oam has no libuv event loop, so true cross-thread dispatch is not supported.
// napi-rs 2.x creates a "GC ThreadsafeFunction" during module init to schedule
// finalizer calls back onto the JS thread.  If napi_create_threadsafe_function
// returns failure, napi-rs panics immediately.  We return NAPI_OK with a dummy
// opaque handle so the creation succeeds; napi_call_threadsafe_function is a
// no-op (finalizers are silently dropped -- acceptable for a sync-only host).

struct TsfnStub {
    context: *mut c_void,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_threadsafe_function(
    _env: Env,
    _func: NapiValue,
    _async_resource: NapiValue,
    _async_resource_name: NapiValue,
    _max_queue_size: usize,
    _initial_thread_count: usize,
    _thread_finalize_data: *mut c_void,
    _thread_finalize_cb: *mut c_void,
    context: *mut c_void,
    _call_js_cb: *mut c_void,
    result: *mut *mut c_void,
) -> NapiStatus {
    if result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let stub = Box::new(TsfnStub { context });
    unsafe { *result = Box::into_raw(stub) as *mut c_void };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_threadsafe_function_context(
    func: *mut c_void,
    result: *mut *mut c_void,
) -> NapiStatus {
    if func.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let stub = unsafe { &*(func as *const TsfnStub) };
    unsafe { *result = stub.context };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_call_threadsafe_function(
    _func: *mut c_void,
    _data: *mut c_void,
    _is_blocking: u32,
) -> NapiStatus {
    // No event loop -- silently drop. Finalizers will not run.
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_acquire_threadsafe_function(_func: *mut c_void) -> NapiStatus {
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_release_threadsafe_function(
    _func: *mut c_void,
    _mode: u32,
) -> NapiStatus {
    // Leaks the TsfnStub -- acceptable for a sync host where addons live
    // until the runtime shuts down.
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_unref_threadsafe_function(
    _env: Env,
    _func: *mut c_void,
) -> NapiStatus {
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_ref_threadsafe_function(_env: Env, _func: *mut c_void) -> NapiStatus {
    NAPI_OK
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
    let trace = std::env::var_os("OAM_NAPI_TRACE").is_some();
    if trace {
        eprintln!("[napi] dlopen start {}", path.display());
    }
    let library = match unsafe { libloading::Library::new(path) } {
        Ok(library) => library,
        Err(e) => {
            throw(scope, &format!("cannot load addon {}: {e}", path.display()));
            return None;
        }
    };
    if trace {
        eprintln!("[napi] dlopen ok; looking up napi_register_module_v1");
    }
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

    // Tie the library's lifetime to the JsRuntime BEFORE running register().
    // register() can create napi functions -- whose code pointers live in this
    // DLL -- and attach them to persistent JS objects (globalThis, exports, or
    // even the error it throws), so once it has run the DLL can never be safely
    // unloaded. Pushing here rather than only on the success path keeps the
    // pending-exception return below from dropping `library` and unmapping code
    // the surviving env still points into.
    scope
        .get_slot_mut::<AddonRegistry>()
        .expect("AddonRegistry slot installed")
        .libraries
        .push(library);

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
    if trace {
        eprintln!("[napi] symbol found; calling register()");
    }
    let result = unsafe { register(env, from_local(exports_value)) };
    if trace {
        eprintln!("[napi] register() returned");
    }
    unsafe {
        (*env).scope = std::ptr::null_mut();
    }

    if let Some(pending) = unsafe { (*env).pending.take() } {
        let exception = v8::Local::new(scope, &pending);
        scope.throw_exception(exception);
        return None;
    }

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
