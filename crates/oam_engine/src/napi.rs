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
    env_ref
        .refs
        .retain(|r| r.as_ref() as *const NapiRefEntry != target);
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
            .unwrap_or(std::ptr::null_mut()) as *mut c_void;
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
            unsafe { *result_data = dst.as_ptr() as *mut c_void };
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
                .map(|p| p.as_ptr() as *mut c_void)
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
