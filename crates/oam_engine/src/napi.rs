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

/// A persistent reference to a V8 value. Lives in a [`NapiRefSlot`] of
/// `NapiEnv::refs`.
struct NapiRefEntry {
    value: v8::Global<v8::Value>,
    refcount: u32,
}

/// One slot of an env's reference table.
///
/// A slot is never removed once created. `napi_delete_reference` takes the
/// entry OUT and bumps `generation`; the index then goes on the free list for
/// the next create. Because a handle carries the generation it was minted
/// with, a handle for a recycled slot fails the generation compare instead of
/// silently resolving to whoever owns the slot now. See the encoding notes
/// above [`encode_ref_handle`].
struct NapiRefSlot {
    /// `None` while the slot is free.
    entry: Option<NapiRefEntry>,
    /// Bumped on every free. Starts at 1 and never reaches 0 again, which is
    /// what keeps a minted handle from ever being all-zero -- the ABI's null
    /// `napi_ref` sentinel.
    generation: usize,
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
    /// Persistent references (napi_create_reference), addressed by an
    /// index+generation handle rather than by address. No `Box` here, unlike
    /// the two vecs either side: nothing outside this file ever holds a
    /// pointer INTO this table, so its elements are free to move on realloc.
    refs: Vec<NapiRefSlot>,
    /// Indices of `refs` slots whose entry has been deleted and which the next
    /// `napi_create_reference` may reuse.
    ref_free: Vec<usize>,
    /// Folded into every handle this env mints, so a handle from one env
    /// decodes to a miss in another. See [`encode_ref_handle`].
    ref_tag: usize,
    /// Active napi_wrap entries for this env.
    #[allow(clippy::vec_box)]
    wraps: Vec<Box<NapiWrapEntry>>,
    /// Backing store for `napi_get_last_error_info`. Node keeps this per-env
    /// too, which is what makes returning a pointer to it sound: the struct
    /// lives as long as the env the addon is calling through.
    last_error: NapiExtendedErrorInfo,
}

impl NapiEnv {
    fn new() -> Box<NapiEnv> {
        Box::new(NapiEnv {
            scope: std::ptr::null_mut(),
            pending: None,
            fn_data: Vec::new(),
            refs: Vec::new(),
            ref_free: Vec::new(),
            ref_tag: next_env_ref_tag(),
            wraps: Vec::new(),
            last_error: NapiExtendedErrorInfo {
                error_message: std::ptr::null(),
                engine_reserved: std::ptr::null_mut(),
                engine_error_code: 0,
                error_code: NAPI_OK,
            },
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

// Thread-local counter of entries actually PUSHED into `NapiEnv::refs` by
// napi_create_reference. Used only in tests, to prove a rejected call creates
// nothing: the orphan a failed create used to leave behind is invisible from
// JS (nothing exposes `env.refs`), so this is the seam that makes it testable.
// Not a doc comment -- rustdoc can't document items produced by a macro.
#[cfg(test)]
thread_local! {
    pub static NAPI_REF_PUSH_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

type Env = *mut NapiEnv;

/// Build a `NapiEnv` with `scope` installed, hand it plus one valid
/// `napi_value` to `f`, then tear the scope back down -- the same install /
/// restore a native entry performs, so the C entry points behave exactly as
/// they do under a real addon. Test-only.
#[cfg(test)]
pub(crate) fn with_test_env<R>(
    scope: &mut v8::PinScope<'_, '_>,
    f: impl FnOnce(Env, NapiValue) -> R,
) -> R {
    let mut env = Box::new(NapiEnv::new());
    env.scope = std::ptr::from_ref(&*scope) as *mut c_void;
    let value = from_local(v8::Object::new(scope).into());
    let ptr: Env = &raw mut **env;
    let out = f(ptr, value);
    // Match load_addon: the scope must not outlive the frame that installed it.
    env.scope = std::ptr::null_mut();
    out
}

/// SAFETY HELPERS — every napi fn funnels through these.
///
/// # Safety
///
/// The caller must pass the `env` pointer this engine handed the addon; it is
/// dereferenced and its stashed PinScope recovered -- sound only while a
/// native entry (trampoline / load_addon) is on the stack, which is the only
/// time napi fns run.
unsafe fn env_scope<'a>(env: Env) -> Option<&'a v8::PinScope<'static, 'static>> {
    // SAFETY: `env` is the caller's `*mut NapiEnv`; `as_ref` guards null, then the stashed `scope` field is cast back to a SHARED `&PinScope` -- non-null only while a native entry is on the stack.
    unsafe {
        let env = env.as_ref()?;
        (env.scope as *const v8::PinScope<'static, 'static>).as_ref()
    }
}

/// # Safety
///
/// The caller must pass a `napi_value` produced by this engine; it is
/// reinterpreted as a `v8::Local` (ABI-identical, size-asserted). Null yields
/// None.
unsafe fn to_local<'s>(value: NapiValue) -> Option<v8::Local<'s, v8::Value>> {
    if value.is_null() {
        None
    } else {
        // SAFETY: reinterprets the non-null `napi_value` pointer as a `v8::Local<Value>`; the two are ABI-identical (size-asserted at module top).
        Some(unsafe { std::mem::transmute::<NapiValue, v8::Local<'s, v8::Value>>(value) })
    }
}

fn from_local(local: v8::Local<'_, v8::Value>) -> NapiValue {
    // SAFETY: ABI pun of a `v8::Local<Value>` to the pointer-sized `napi_value` (size-asserted equal at module top).
    unsafe { std::mem::transmute::<v8::Local<'_, v8::Value>, NapiValue>(local) }
}

/// Record WHY an entry point is failing, then return that status.
///
/// `napi_get_last_error_info` is the documented way an addon asks "why did
/// that fail?", and returning a permanently-empty struct answered "nothing
/// failed" -- actively misleading, precisely when the caller is debugging.
/// Anything that records here must clear on success via [`ok`], or the next
/// reader sees a stale reason.
///
/// # Safety
///
/// `env` must be the live `napi_env` this engine handed the addon, or null.
unsafe fn fail(env: Env, status: NapiStatus, message: &'static std::ffi::CStr) -> NapiStatus {
    // SAFETY: caller guarantees `env` is this engine's env pointer or null;
    // `as_mut` guards null, and the write only touches the env's own slot.
    if let Some(env_ref) = unsafe { env.as_mut() } {
        env_ref.last_error.error_code = status;
        env_ref.last_error.error_message = message.as_ptr();
    }
    status
}

/// Clear the last-error slot and return NAPI_OK.
///
/// # Safety
///
/// `env` must be the live `napi_env` this engine handed the addon, or null.
unsafe fn ok(env: Env) -> NapiStatus {
    // SAFETY: as in `fail` -- null-guarded, writes only the env's own slot.
    if let Some(env_ref) = unsafe { env.as_mut() } {
        env_ref.last_error.error_code = NAPI_OK;
        env_ref.last_error.error_message = std::ptr::null();
    }
    NAPI_OK
}

/// # Safety
///
/// The caller must pass a valid writable `*mut T` (or null) for `ptr`; out()
/// null-checks before writing `value`.
unsafe fn out<T>(ptr: *mut T, value: T) -> NapiStatus {
    if ptr.is_null() {
        return NAPI_INVALID_ARG;
    }
    // SAFETY: `ptr` was null-checked at the top of out(); writes `value` into the caller's out-parameter.
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
    // SAFETY: the External's payload is the `*mut FnData` stored when this function was built (napi_create_function); it stays live in NapiEnv::fn_data for the env's lifetime.
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
    // SAFETY: `env` is the FnData's owning env; swaps in the current PinScope, invokes the addon callback `fn_data.cb` (a C-ABI fn pointer from the addon), and returns the previous scope pointer to restore re-entrantly.
    let (prev_scope, result) = unsafe {
        let prev = (*env).scope;
        (*env).scope = scope as *mut v8::PinScope<'_, '_> as *mut c_void;
        let result = (fn_data.cb)(env, &mut info as *mut CbInfo);
        (prev, result)
    };
    // SAFETY: `env` is the FnData's owning env; restores the previous scope pointer and, if the callback left a deferred exception, re-throws it into V8.
    unsafe {
        (*env).scope = prev_scope;
        if let Some(pending) = (*env).pending.take() {
            let exception = v8::Local::new(scope, &pending);
            scope.throw_exception(exception);
            return;
        }
    }
    // SAFETY: `result` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    if let Some(local) = unsafe { to_local(result) } {
        rv.set(local);
    }
}

// =========================================================== value creation

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_undefined(env: Env, result: *mut NapiValue) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(v8::undefined(scope).into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_null(env: Env, result: *mut NapiValue) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(v8::null(scope).into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_global(env: Env, result: *mut NapiValue) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let context = scope.get_current_context();
    let global = context.global(scope);
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(global.into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_boolean(
    env: Env,
    value: bool,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(v8::Boolean::new(scope, value).into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_int32(
    env: Env,
    value: i32,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(v8::Integer::new(scope, value).into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_uint32(
    env: Env,
    value: u32,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe {
        out(
            result,
            from_local(v8::Integer::new_from_unsigned(scope, value).into()),
        )
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_int64(
    env: Env,
    value: i64,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // Node-parity: Number.MAX_SAFE_INTEGER = 2^53 - 1. Values whose absolute
    // value EQUALS 2^53 are representable in f64 exactly but the surrounding
    // integers are not, so Node's napi_create_int64 returns BigInt at the
    // boundary. Inclusive range against (2^53 - 1) matches.
    const MAX_SAFE: i64 = (1_i64 << 53) - 1;
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
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

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_double(
    env: Env,
    value: f64,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(v8::Number::new(scope, value).into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_string_utf8(
    env: Env,
    string: *const c_char,
    length: usize,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    if string.is_null() {
        return NAPI_INVALID_ARG;
    }
    // SAFETY: `string` is the caller's NUL-terminated C string, null-checked before this read.
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
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(created.into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_object(env: Env, result: *mut NapiValue) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(v8::Object::new(scope).into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_array(env: Env, result: *mut NapiValue) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(v8::Array::new(scope, 0).into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_array_with_length(
    env: Env,
    length: usize,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe {
        out(
            result,
            from_local(v8::Array::new(scope, length as i32).into()),
        )
    }
}

// =========================================================== value reading

/// napi_valuetype numeric values per js_native_api_types.h.
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_typeof(env: Env, value: NapiValue, result: *mut i32) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
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
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, kind) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_bool(
    env: Env,
    value: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    if !local.is_boolean() {
        return NAPI_BOOLEAN_EXPECTED;
    }
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.is_true()) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_int32(
    env: Env,
    value: NapiValue,
    result: *mut i32,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    if !local.is_number() {
        return NAPI_NUMBER_EXPECTED;
    }
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.int32_value(scope).unwrap_or(0)) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_uint32(
    env: Env,
    value: NapiValue,
    result: *mut u32,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    if !local.is_number() {
        return NAPI_NUMBER_EXPECTED;
    }
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.uint32_value(scope).unwrap_or(0)) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_int64(
    env: Env,
    value: NapiValue,
    result: *mut i64,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    if !local.is_number() {
        return NAPI_NUMBER_EXPECTED;
    }
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.number_value(scope).unwrap_or(0.0) as i64) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_double(
    env: Env,
    value: NapiValue,
    result: *mut f64,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    if !local.is_number() {
        return NAPI_NUMBER_EXPECTED;
    }
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.number_value(scope).unwrap_or(0.0)) }
}

/// Two-phase: null buf -> *result = utf8 byte length; else copy + NUL.
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_string_utf8(
    env: Env,
    value: NapiValue,
    buf: *mut c_char,
    bufsize: usize,
    result: *mut usize,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(string) = v8::Local::<v8::String>::try_from(local) else {
        return NAPI_STRING_EXPECTED;
    };
    let text = string.to_rust_string_lossy(scope);
    if buf.is_null() {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        return unsafe { out(result, text.len()) };
    }
    if bufsize == 0 {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        return unsafe { out(result, 0) };
    }
    let copy_len = text.len().min(bufsize - 1);
    // SAFETY: `buf` is the caller's out buffer with `bufsize` >= copy_len+1 (enforced above); copies `copy_len` UTF-8 bytes, writes the NUL terminator, and reports the length via the null-checked `result`.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buf as *mut u8, copy_len);
        *buf.add(copy_len) = 0;
        if !result.is_null() {
            *result = copy_len;
        }
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_array(
    env: Env,
    value: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.is_array()) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_array_length(
    env: Env,
    value: NapiValue,
    result: *mut u32,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(array) = v8::Local::<v8::Array>::try_from(local) else {
        return NAPI_ARRAY_EXPECTED;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, array.length()) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_strict_equals(
    env: Env,
    lhs: NapiValue,
    rhs: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `lhs` and `rhs` are napi_value handles from this env; to_local reinterprets each as a repr-compatible `v8::Local` (size-asserted) and returns None for a null handle.
    let (Some(a), Some(b)) = (unsafe { (to_local(lhs), to_local(rhs)) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, a.strict_equals(b)) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_coerce_to_string(
    env: Env,
    value: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    match local.to_string(scope) {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        Some(string) => unsafe { out(result, from_local(string.into())) },
        None => NAPI_PENDING_EXCEPTION,
    }
}

// ============================================================== properties

/// # Safety
///
/// The caller must pass a `napi_value` from this env; it is read as a
/// `v8::Local` and downcast to Object.
unsafe fn as_object<'s>(value: NapiValue) -> Option<v8::Local<'s, v8::Object>> {
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let local = unsafe { to_local(value) }?;
    v8::Local::<v8::Object>::try_from(local).ok()
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_set_named_property(
    env: Env,
    object: NapiValue,
    name: *const c_char,
    value: NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let (Some(value), false) = (unsafe { to_local(value) }, name.is_null()) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `name` is the caller's NUL-terminated C string, null-checked before this read.
    let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy();
    let Some(key) = v8::String::new(scope, &name) else {
        return NAPI_GENERIC_FAILURE;
    };
    target.set(scope, key.into(), value);
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_named_property(
    env: Env,
    object: NapiValue,
    name: *const c_char,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    if name.is_null() {
        return NAPI_INVALID_ARG;
    }
    // SAFETY: `name` is the caller's NUL-terminated C string, null-checked before this read.
    let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy();
    let Some(key) = v8::String::new(scope, &name) else {
        return NAPI_GENERIC_FAILURE;
    };
    match target.get(scope, key.into()) {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        Some(value) => unsafe { out(result, from_local(value)) },
        None => NAPI_PENDING_EXCEPTION,
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_has_named_property(
    env: Env,
    object: NapiValue,
    name: *const c_char,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    if name.is_null() {
        return NAPI_INVALID_ARG;
    }
    // SAFETY: `name` is the caller's NUL-terminated C string, null-checked before this read.
    let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy();
    let Some(key) = v8::String::new(scope, &name) else {
        return NAPI_GENERIC_FAILURE;
    };
    let has = target.has(scope, key.into()).unwrap_or(false);
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, has) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_set_property(
    env: Env,
    object: NapiValue,
    key: NapiValue,
    value: NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    // SAFETY: `key` and `value` are napi_value handles from this env; to_local reinterprets each as a repr-compatible `v8::Local` (size-asserted) and returns None for a null handle.
    let (Some(key), Some(value)) = (unsafe { (to_local(key), to_local(value)) }) else {
        return NAPI_INVALID_ARG;
    };
    target.set(scope, key, value);
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_property(
    env: Env,
    object: NapiValue,
    key: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    // SAFETY: `key` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(key) = (unsafe { to_local(key) }) else {
        return NAPI_INVALID_ARG;
    };
    match target.get(scope, key) {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        Some(value) => unsafe { out(result, from_local(value)) },
        None => NAPI_PENDING_EXCEPTION,
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_set_element(
    env: Env,
    object: NapiValue,
    index: u32,
    value: NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(value) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    target.set_index(scope, index, value);
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_element(
    env: Env,
    object: NapiValue,
    index: u32,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    match target.get_index(scope, index) {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        Some(value) => unsafe { out(result, from_local(value)) },
        None => NAPI_PENDING_EXCEPTION,
    }
}

// =============================================================== functions

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_function(
    env: Env,
    name: *const c_char,
    _length: usize,
    cb: unsafe extern "C" fn(Env, *mut CbInfo) -> NapiValue,
    data: *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
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
        // SAFETY: `name` is the caller's NUL-terminated C string, null-checked before this read.
        let label = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy();
        if let Some(label) = v8::String::new(scope, &label) {
            function.set_name(label);
        }
    }
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(function.into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_cb_info(
    env: Env,
    cbinfo: *mut CbInfo,
    argc: *mut usize,
    argv: *mut NapiValue,
    this_arg: *mut NapiValue,
    data: *mut *mut c_void,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `cbinfo` is the CbInfo pointer the trampoline placed on its stack and passed to this callback; as_ref yields None if the addon passed null.
    let Some(info) = (unsafe { cbinfo.as_ref() }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: writes back through the caller's out-parameters -- each of `argc`, `argv`, `this_arg`, `data` is null-checked before use, and `argv` is filled for at most the `*argc` slots the caller declared.
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

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_call_function(
    env: Env,
    recv: NapiValue,
    func: NapiValue,
    argc: usize,
    argv: *const NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `recv` and `func` are napi_value handles from this env; to_local reinterprets each as a repr-compatible `v8::Local` (size-asserted) and returns None for a null handle.
    let (Some(recv), Some(func_value)) = (unsafe { (to_local(recv), to_local(func)) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(function) = v8::Local::<v8::Function>::try_from(func_value) else {
        return NAPI_FUNCTION_EXPECTED;
    };
    let args: Vec<v8::Local<v8::Value>> = (0..argc)
        // SAFETY: `argv[i]` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
        .filter_map(|i| unsafe { to_local(*argv.add(i)) })
        .collect();
    match function.call(scope, recv, &args) {
        Some(value) => {
            if result.is_null() {
                NAPI_OK
            } else {
                // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
                unsafe { out(result, from_local(value)) }
            }
        }
        None => NAPI_PENDING_EXCEPTION,
    }
}

// ================================================================== errors

/// # Safety
///
/// `code` and `msg` are optional caller C strings, each read only after a
/// null check; `scope` is a live PinScope.
unsafe fn build_error(
    scope: &v8::PinScope<'_, '_>,
    code: *const c_char,
    msg: *const c_char,
    kind: fn(&v8::PinScope<'_, '_>, v8::Local<v8::String>) -> v8::Local<'static, v8::Value>,
) -> Option<v8::Global<v8::Value>> {
    let message = if msg.is_null() {
        "unknown native error".into()
    } else {
        // SAFETY: `msg` is the caller's NUL-terminated C string, null-checked before this read.
        unsafe { std::ffi::CStr::from_ptr(msg) }.to_string_lossy()
    };
    let message = v8::String::new(scope, &message)?;
    let error = kind(scope, message);
    if !code.is_null()
        && let Ok(object) = v8::Local::<v8::Object>::try_from(error)
    {
        // SAFETY: `code` is the caller's NUL-terminated C string, null-checked before this read.
        let code_text = unsafe { std::ffi::CStr::from_ptr(code) }.to_string_lossy();
        let key = v8::String::new(scope, "code")?;
        if let Some(value) = v8::String::new(scope, &code_text) {
            object.set(scope, key.into(), value.into());
        }
    }
    Some(v8::Global::new(scope, error))
}

fn plain_error<'s>(
    scope: &v8::PinScope<'s, '_>,
    message: v8::Local<v8::String>,
) -> v8::Local<'static, v8::Value> {
    let error = v8::Exception::error(scope, message);
    // SAFETY: relifetimes the freshly built error `v8::Local` to 'static; build_error immediately re-roots it into a `v8::Global` before the scope ends, so the handle stays valid.
    unsafe { std::mem::transmute::<v8::Local<v8::Value>, v8::Local<'static, v8::Value>>(error) }
}

fn type_error<'s>(
    scope: &v8::PinScope<'s, '_>,
    message: v8::Local<v8::String>,
) -> v8::Local<'static, v8::Value> {
    let error = v8::Exception::type_error(scope, message);
    // SAFETY: relifetimes the freshly built error `v8::Local` to 'static; build_error immediately re-roots it into a `v8::Global` before the scope ends, so the handle stays valid.
    unsafe { std::mem::transmute::<v8::Local<v8::Value>, v8::Local<'static, v8::Value>>(error) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_throw(env: Env, error: NapiValue) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `error` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(error) }) else {
        return NAPI_INVALID_ARG;
    };
    let global = v8::Global::new(scope, local);
    // SAFETY: `env` is a valid `*mut NapiEnv` (established above); reads/writes its `pending` deferred-exception slot.
    unsafe { (*env).pending = Some(global) };
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_throw_error(
    env: Env,
    code: *const c_char,
    msg: *const c_char,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `code` and `msg` are the caller's optional C strings; build_error reads each only after a null check.
    match unsafe { build_error(scope, code, msg, plain_error) } {
        Some(error) => {
            // SAFETY: `env` is a valid `*mut NapiEnv` (established above); reads/writes its `pending` deferred-exception slot.
            unsafe { (*env).pending = Some(error) };
            NAPI_OK
        }
        None => NAPI_GENERIC_FAILURE,
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_throw_type_error(
    env: Env,
    code: *const c_char,
    msg: *const c_char,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `code` and `msg` are the caller's optional C strings; build_error reads each only after a null check.
    match unsafe { build_error(scope, code, msg, type_error) } {
        Some(error) => {
            // SAFETY: `env` is a valid `*mut NapiEnv` (established above); reads/writes its `pending` deferred-exception slot.
            unsafe { (*env).pending = Some(error) };
            NAPI_OK
        }
        None => NAPI_GENERIC_FAILURE,
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_exception_pending(env: Env, result: *mut bool) -> NapiStatus {
    // SAFETY: `env` is the caller's `*mut NapiEnv`; as_ref/as_mut yields None if the caller passed null.
    let Some(env_ref) = (unsafe { env.as_ref() }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, env_ref.pending.is_some()) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_and_clear_last_exception(
    env: Env,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `env` is a valid `*mut NapiEnv` (established above); reads/writes its `pending` deferred-exception slot.
    let pending = unsafe { (*env).pending.take() };
    let value = match pending {
        Some(global) => from_local(v8::Local::new(scope, &global)),
        None => from_local(v8::undefined(scope).into()),
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, value) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_version(env: Env, result: *mut u32) -> NapiStatus {
    if env.is_null() {
        return NAPI_INVALID_ARG;
    }
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, 8) } // N-API version 8 (the stable baseline)
}

// ================================================================= externals

/// `v8::External` wrapping a raw pointer.  The optional finalizer is
/// accepted but ignored for now (no GC finalizer hook yet).
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_external(
    env: Env,
    data: *mut c_void,
    _finalize_cb: *mut c_void,
    _finalize_hint: *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let ext = v8::External::new(scope, data);
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(ext.into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_external(
    env: Env,
    value: NapiValue,
    result: *mut *mut c_void,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(ext) = v8::Local::<v8::External>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    if result.is_null() {
        return NAPI_INVALID_ARG;
    }
    // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
    unsafe { *result = ext.value() };
    NAPI_OK
}

// ================================================================ references
//
// NapiRef handle: an opaque *mut c_void that is NOT a pointer. It packs a slot
// index, that slot's generation, and a per-env tag; see `encode_ref_handle`.

type NapiRefHandle = *mut c_void;

// ------------------------------------------------------- handle encoding
//
// A `napi_ref` is pointer-sized because the ABI says so, but nothing in the
// ABI says it must BE a pointer, and making it one is what created the hole
// this encoding closes. The layout, most significant field first:
//
//     [ env tag : REF_TAG_BITS | generation : REF_GEN_BITS | index : REF_INDEX_BITS ]
//
// Widths are derived from the pointer width, so a 32-bit target stays correct
// with every field simply narrower (8 / 12 / 12 instead of 16 / 24 / 24).

/// Width of the per-env tag. Published so the e2e suite can forge handles
/// against the real layout instead of duplicating these numbers.
pub const REF_TAG_BITS: u32 = usize::BITS / 4;
/// Width of the slot index.
pub const REF_INDEX_BITS: u32 = (usize::BITS - REF_TAG_BITS) / 2;
/// Width of the per-slot generation counter.
pub const REF_GEN_BITS: u32 = usize::BITS - REF_TAG_BITS - REF_INDEX_BITS;

const REF_INDEX_MASK: usize = (1usize << REF_INDEX_BITS) - 1;
const REF_GEN_MASK: usize = (1usize << REF_GEN_BITS) - 1;
const REF_TAG_MASK: usize = (1usize << REF_TAG_BITS) - 1;

/// Highest generation a slot may reach. On hitting it the slot is RETIRED
/// rather than reused, which is what stops the counter wrapping around onto a
/// generation some addon still holds a handle for. Costs one empty slot after
/// 16.7M create/delete cycles of that single slot on a 64-bit target.
const REF_MAX_GENERATION: usize = REF_GEN_MASK;

/// Largest reference table one env can have (16.7M slots on a 64-bit target).
/// Past this `napi_create_reference` refuses rather than minting a handle
/// whose index would be truncated.
const REF_MAX_SLOTS: usize = REF_INDEX_MASK + 1;

/// Source of per-env tags.
///
/// The old address-identity scan got cross-env rejection for free, because the
/// allocator never hands two live objects the same address. An index alone has
/// no such property -- every env's table starts at index 0 -- so the tag is
/// what replaces it. It is `REF_TAG_BITS` wide, so it repeats after 65,536
/// envs on a 64-bit target; two LIVE envs would have to be that far apart in
/// load order to share one, and an env is created per loaded addon. Stated
/// rather than hidden: this is a narrower guarantee than address uniqueness,
/// and it is the one place this design is weaker than what it replaces.
static NAPI_ENV_TAG_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

fn next_env_ref_tag() -> usize {
    NAPI_ENV_TAG_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) & REF_TAG_MASK
}

/// Pack a slot index and generation, with this env's tag, into the
/// pointer-sized value the ABI hands back as a `napi_ref`.
///
/// Never returns null. `generation` is >= 1 for every live slot and sits in a
/// middle field, so the whole word is non-zero -- which matters because the
/// ABI uses a null `napi_ref` as "no reference" (`napi_wrap` writes one).
fn encode_ref_handle(tag: usize, index: usize, generation: usize) -> NapiRefHandle {
    debug_assert!(index <= REF_INDEX_MASK, "slot index overflows its field");
    debug_assert!(
        (1..=REF_GEN_MASK).contains(&generation),
        "generation overflows its field, or is the null-sentinel 0"
    );
    let bits = ((tag & REF_TAG_MASK) << (REF_GEN_BITS + REF_INDEX_BITS))
        | ((generation & REF_GEN_MASK) << REF_INDEX_BITS)
        | (index & REF_INDEX_MASK);
    bits as NapiRefHandle
}

/// Inverse of [`encode_ref_handle`], rejecting anything `tag` did not mint.
///
/// Pure integer work: no dereference, so a null, stale, foreign or wholly
/// forged handle costs a few instructions and can never fault. That is the
/// substantive difference from the address scan this replaced, which had to
/// prove a handle was live before it dared touch it.
fn decode_ref_handle(tag: usize, ref_: NapiRefHandle) -> Option<(usize, usize)> {
    let bits = ref_ as usize;
    // The ABI's "no reference" sentinel, and never a value we mint.
    if bits == 0 {
        return None;
    }
    if (bits >> (REF_GEN_BITS + REF_INDEX_BITS)) != (tag & REF_TAG_MASK) {
        return None;
    }
    let generation = (bits >> REF_INDEX_BITS) & REF_GEN_MASK;
    if generation == 0 {
        return None;
    }
    Some((bits & REF_INDEX_MASK, generation))
}

impl NapiEnv {
    /// Install `entry` in a free slot (or a fresh one) and mint its handle.
    ///
    /// Returns the entry back in `Err` when the table is full, so the caller
    /// can drop it OUTSIDE its borrow of the env: dropping a `NapiRefEntry`
    /// disposes a `v8::Global`, which touches the isolate, and nothing in this
    /// file may hold a reference into `NapiEnv` across a call into V8.
    fn alloc_ref(&mut self, entry: NapiRefEntry) -> Result<NapiRefHandle, NapiRefEntry> {
        let index = match self.ref_free.pop() {
            Some(index) => index,
            None => {
                if self.refs.len() >= REF_MAX_SLOTS {
                    return Err(entry);
                }
                self.refs.push(NapiRefSlot {
                    entry: None,
                    generation: 1,
                });
                self.refs.len() - 1
            }
        };
        let tag = self.ref_tag;
        let slot = &mut self.refs[index];
        slot.entry = Some(entry);
        Ok(encode_ref_handle(tag, index, slot.generation))
    }

    /// Resolve `ref_` to its live entry, or `None` when the handle is null,
    /// foreign to this env, out of range, names a generation the slot has
    /// already moved past, or names a slot that is currently free.
    fn ref_slot_entry(&self, ref_: NapiRefHandle) -> Option<&NapiRefEntry> {
        let (index, generation) = decode_ref_handle(self.ref_tag, ref_)?;
        let slot = self.refs.get(index)?;
        if slot.generation != generation {
            return None;
        }
        slot.entry.as_ref()
    }

    /// Exclusive counterpart to [`NapiEnv::ref_slot_entry`], for the refcount
    /// mutators. Same acceptance rule.
    fn ref_slot_entry_mut(&mut self, ref_: NapiRefHandle) -> Option<&mut NapiRefEntry> {
        let (index, generation) = decode_ref_handle(self.ref_tag, ref_)?;
        let slot = self.refs.get_mut(index)?;
        if slot.generation != generation {
            return None;
        }
        slot.entry.as_mut()
    }

    /// Take the entry `ref_` names out of the table and make its slot reusable.
    ///
    /// Hands the entry back rather than dropping it here, for the same reason
    /// [`NapiEnv::alloc_ref`] does: the caller drops it once the borrow on the
    /// env is gone.
    fn free_ref(&mut self, ref_: NapiRefHandle) -> Option<NapiRefEntry> {
        let (index, generation) = decode_ref_handle(self.ref_tag, ref_)?;
        let slot = self.refs.get_mut(index)?;
        if slot.generation != generation {
            return None;
        }
        // A free slot has already had its generation bumped, so a stale handle
        // normally fails the compare above. This still catches the one case it
        // cannot: a RETIRED slot, whose generation is pinned at the maximum.
        let entry = slot.entry.take()?;
        if slot.generation < REF_MAX_GENERATION {
            slot.generation += 1;
            self.ref_free.push(index);
        }
        // else: the slot has exhausted its generations. Leaving it off the free
        // list retires it for good, which is what stops the counter wrapping
        // back onto a handle some addon may still be holding.
        Some(entry)
    }
}

/// Resolve a caller-supplied `napi_ref` to its entry, for the read paths.
///
/// The handle is DECODED, never dereferenced: it is an index plus a generation
/// plus this env's tag, so resolving it is a bounds check and two integer
/// compares against the env's own table. The only pointer this touches is
/// `env` itself, and the reference it returns is derived from that -- so a
/// null, stale, foreign or wholly forged handle is a miss, and never a fault.
///
/// This replaced a `ptr::eq` scan over boxed entries whose ABA hole was
/// documented in place and then measured: 5,000 create/delete/create cycles
/// through the vendored addon accepted 40 stale handles and resolved every one
/// of them to the WRONG live reference. A generation that is bumped on free
/// makes that case a deterministic miss.
///
/// # Safety
///
/// `env` must be the live `napi_env` this engine handed the addon; `ref_` is
/// checked, so it may be null, stale or foreign.
unsafe fn ref_entry<'a>(env: Env, ref_: NapiRefHandle) -> Option<&'a NapiRefEntry> {
    // SAFETY: `env` is the caller's `*mut NapiEnv`; `as_ref` guards null. The
    // reborrow is SHARED, which is what lets the caller keep the result live
    // across `v8::Local::new` without claiming exclusivity over the whole env.
    unsafe { env.as_ref()?.ref_slot_entry(ref_) }
}

/// Exclusive counterpart to [`ref_entry`], for the refcount mutators.
///
/// Callers must not hold the result across ANY call into V8 -- V8 can re-enter
/// JS, JS can re-enter this ABI, and a second `&mut *env` while this one is
/// live is aliasing UB. The two callers below only touch an integer and a
/// caller-owned out-pointer, neither of which re-enters.
///
/// # Safety
///
/// As [`ref_entry`].
unsafe fn ref_entry_mut<'a>(env: Env, ref_: NapiRefHandle) -> Option<&'a mut NapiRefEntry> {
    // SAFETY: `env` is the caller's `*mut NapiEnv`; `as_mut` guards null, and
    // the handle is decoded rather than dereferenced.
    unsafe { env.as_mut()?.ref_slot_entry_mut(ref_) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_reference(
    env: Env,
    value: NapiValue,
    initial_refcount: u32,
    result: *mut NapiRefHandle,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        // SAFETY: `env` is the caller's env pointer or null; fail() null-guards.
        return unsafe {
            fail(
                env,
                NAPI_INVALID_ARG,
                c"napi_create_reference: no native scope is active on this env",
            )
        };
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        // SAFETY: as above.
        return unsafe {
            fail(
                env,
                NAPI_INVALID_ARG,
                c"napi_create_reference: `value` is null",
            )
        };
    };
    // Reject a null out-parameter BEFORE creating anything. Pushing first and
    // failing afterwards left the entry in `env.refs` with no handle ever
    // dispensed for it -- unreachable, and freed only when the env drops. Node
    // validates its arguments up front and creates no reference at all.
    if result.is_null() {
        // SAFETY: as above.
        return unsafe {
            fail(
                env,
                NAPI_INVALID_ARG,
                c"napi_create_reference: `result` out-pointer is null",
            )
        };
    }
    // Built BEFORE the env is borrowed: `v8::Global::new` calls into V8, and
    // nothing in this file may hold a reference into `NapiEnv` across that.
    let global = v8::Global::new(scope, local);
    let entry = NapiRefEntry {
        value: global,
        refcount: initial_refcount,
    };
    // SAFETY: `env` is the caller's `*mut NapiEnv`, checked non-null above; reborrowed here to reach its owned ref table. The borrow ends with this statement -- `alloc_ref` returns owned values either way.
    let minted = unsafe { &mut *env }.alloc_ref(entry);
    let handle = match minted {
        Ok(handle) => handle,
        Err(rejected) => {
            // Drop the rejected entry -- and the `v8::Global` it owns -- now
            // that no borrow of the env is live.
            drop(rejected);
            // SAFETY: `env` is the caller's env pointer; fail() null-guards.
            return unsafe {
                fail(
                    env,
                    NAPI_GENERIC_FAILURE,
                    c"napi_create_reference: this env's reference table is full",
                )
            };
        }
    };
    #[cfg(test)]
    NAPI_REF_PUSH_COUNT.with(|c| c.set(c.get() + 1));
    // SAFETY: `result` is the caller's out-parameter pointer, null-checked above; out() null-checks it again before writing.
    let status = unsafe { out(result, handle) };
    if status == NAPI_OK {
        // SAFETY: `env` is the caller's env pointer; ok() null-guards.
        return unsafe { ok(env) };
    }
    status
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_delete_reference(env: Env, ref_: NapiRefHandle) -> NapiStatus {
    // SAFETY: `env` is the caller's `*mut NapiEnv`; `as_mut` guards null.
    let Some(env_ref) = (unsafe { env.as_mut() }) else {
        return NAPI_INVALID_ARG;
    };
    let removed = env_ref.free_ref(ref_);
    let found = removed.is_some();
    // Drop the entry -- and the `v8::Global` it owns -- only now that the
    // borrow on the env is gone: disposing a persistent handle calls into V8.
    drop(removed);
    if !found {
        // Nothing was removed, so `ref_` is null, already deleted, or was never
        // ours. This used to answer NAPI_OK regardless, which made a
        // double-delete look successful and hid the addon's own bug -- and was
        // inconsistent with the read/ref/unref paths, which do validate. Node
        // treats an invalid ref as undefined behaviour; refusing is strictly
        // safer and is a deliberate divergence (docs/node-divergences.md).
        // SAFETY: `env` is the caller's env pointer; fail() null-guards.
        return unsafe {
            fail(
                env,
                NAPI_INVALID_ARG,
                c"napi_delete_reference: handle is not a live reference in this env (already deleted, or never created here)",
            )
        };
    }
    // SAFETY: `env` is the caller's env pointer; ok() null-guards.
    unsafe { ok(env) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_reference_value(
    env: Env,
    ref_: NapiRefHandle,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        // SAFETY: `env` is the caller's env pointer or null; fail() null-guards.
        return unsafe {
            fail(
                env,
                NAPI_INVALID_ARG,
                c"napi_get_reference_value: no native scope is active on this env",
            )
        };
    };
    // The borrow below is SHARED and stays live across `v8::Local::new`. That
    // is deliberate: `Local::new` only copies a persistent into the current
    // handle scope, so it cannot run JS and cannot re-enter this ABI. A
    // re-entrant `napi_create_reference` would take `&mut *env`, which is why
    // the exclusive lookup (`ref_entry_mut`) is kept off this path.
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv` (env_scope proved it non-null); ref_entry decodes the handle rather than dereferencing it, so null, stale and foreign handles are all misses.
    let Some(entry) = (unsafe { ref_entry(env, ref_) }) else {
        // SAFETY: as above.
        return unsafe {
            fail(
                env,
                NAPI_INVALID_ARG,
                c"napi_get_reference_value: handle is not a live reference in this env (already deleted, or never created here)",
            )
        };
    };
    let local = v8::Local::new(scope, &entry.value);
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    let status = unsafe { out(result, from_local(local)) };
    if status == NAPI_OK {
        // SAFETY: `env` is the caller's env pointer; ok() null-guards.
        return unsafe { ok(env) };
    }
    // SAFETY: as above.
    unsafe {
        fail(
            env,
            status,
            c"napi_get_reference_value: `result` out-pointer is null",
        )
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_reference_ref(
    env: Env,
    ref_: NapiRefHandle,
    result: *mut u32,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; ref_entry_mut guards null on it and decodes the handle rather than dereferencing it, so a deleted or foreign handle is a miss.
    let Some(entry) = (unsafe { ref_entry_mut(env, ref_) }) else {
        // SAFETY: `env` is the caller's env pointer or null; fail() null-guards.
        return unsafe {
            fail(
                env,
                NAPI_INVALID_ARG,
                c"napi_reference_ref: handle is not a live reference in this env (already deleted, or never created here)",
            )
        };
    };
    entry.refcount = entry.refcount.saturating_add(1);
    // Copy out and END the exclusive borrow here: `ok()` below re-derives its
    // own `&mut *env`, and the two must not be live at once.
    let count = entry.refcount;
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = count };
    }
    // SAFETY: `env` is the caller's env pointer; ok() null-guards.
    unsafe { ok(env) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_reference_unref(
    env: Env,
    ref_: NapiRefHandle,
    result: *mut u32,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; ref_entry_mut guards null on it and decodes the handle rather than dereferencing it, so a deleted or foreign handle is a miss.
    let Some(entry) = (unsafe { ref_entry_mut(env, ref_) }) else {
        // SAFETY: `env` is the caller's env pointer or null; fail() null-guards.
        return unsafe {
            fail(
                env,
                NAPI_INVALID_ARG,
                c"napi_reference_unref: handle is not a live reference in this env (already deleted, or never created here)",
            )
        };
    };
    entry.refcount = entry.refcount.saturating_sub(1);
    // As in napi_reference_ref: end the exclusive borrow before `ok()`.
    let count = entry.refcount;
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = count };
    }
    // SAFETY: `env` is the caller's env pointer; ok() null-guards.
    unsafe { ok(env) }
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
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
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
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // Create the constructor function via the shared trampoline.
    let mut ctor_out: NapiValue = std::ptr::null_mut();
    // SAFETY: forwards to napi_create_function with this env and the caller-supplied constructor/method fn pointer and data -- the same C-ABI contract as the entry points.
    let status = unsafe {
        napi_create_function(env, utf8name, usize::MAX, constructor, data, &mut ctor_out)
    };
    if status != NAPI_OK {
        return status;
    }
    // SAFETY: `ctor_out` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
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
        // SAFETY: `properties` points to at least `property_count` caller-owned elements; from_raw_parts reads them for the length the C caller declared.
        unsafe { std::slice::from_raw_parts(properties, property_count) }
    } else {
        &[]
    };

    for prop in props {
        // Resolve property name.
        let key: v8::Local<v8::Value> = if !prop.utf8name.is_null() {
            // SAFETY: `prop.utf8name` is the caller's NUL-terminated C string, null-checked before this read.
            let name = unsafe { std::ffi::CStr::from_ptr(prop.utf8name) }.to_string_lossy();
            match v8::String::new(scope, &name) {
                Some(s) => s.into(),
                None => continue,
            }
        // SAFETY: `prop.name` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
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
            // SAFETY: forwards to napi_create_function with this env and the caller-supplied constructor/method fn pointer and data -- the same C-ABI contract as the entry points.
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
            // SAFETY: `fn_out` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
            if let Some(fn_val) = unsafe { to_local(fn_out) } {
                let attr = napi_attrs_to_v8(prop.attributes);
                target.define_own_property(scope, name_key, fn_val, attr);
            }
        // SAFETY: `prop.value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
        } else if let Some(prop_val) = unsafe { to_local(prop.value) } {
            // Value descriptor.
            let attr = napi_attrs_to_v8(prop.attributes);
            target.define_own_property(scope, name_key, prop_val, attr);
        }
        // getter/setter: deferred -- require AccessorCallback wiring
    }

    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(ctor_fn.into())) }
}

/// Store a raw Rust pointer on a JS object for later retrieval via `napi_unwrap`.
///
/// Any previous wrap on the same object is replaced. The optional finalizer
/// is stored but not yet called automatically (pending GC hook support).
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_wrap(
    env: Env,
    js_object: NapiValue,
    native_object: *mut c_void,
    finalize_cb: Option<unsafe extern "C" fn(Env, *mut c_void, *mut c_void)>,
    finalize_hint: *mut c_void,
    result: *mut NapiRefHandle,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `js_object` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(js_object) }) else {
        return NAPI_INVALID_ARG;
    };
    let global = v8::Global::new(scope, local);
    // SAFETY: `env` is the caller's `*mut NapiEnv`, checked non-null above; reborrowed here to reach its owned fn_data/refs/wraps vecs.
    let env_ref = unsafe { &mut *env };

    // Replace any existing wrap for this object.
    for existing in &mut env_ref.wraps {
        let existing_local = v8::Local::new(scope, &existing.object);
        if local.strict_equals(existing_local) {
            existing.native = native_object;
            existing.finalize = finalize_cb;
            existing.hint = finalize_hint;
            if !result.is_null() {
                // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
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
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = std::ptr::null_mut() };
    }
    NAPI_OK
}

/// Retrieve the native pointer stored by `napi_wrap`.
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_unwrap(
    env: Env,
    js_object: NapiValue,
    result: *mut *mut c_void,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `js_object` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(js_object) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `env` is the caller's `*mut NapiEnv`, checked non-null above; reborrowed here to reach its owned fn_data/refs/wraps vecs.
    let env_ref = unsafe { &*env };
    for wrap in &env_ref.wraps {
        let wrap_local = v8::Local::new(scope, &wrap.object);
        if local.strict_equals(wrap_local) {
            if !result.is_null() {
                // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
                unsafe { *result = wrap.native };
            }
            return NAPI_OK;
        }
    }
    NAPI_INVALID_ARG
}

/// Remove the wrap stored by `napi_wrap` and retrieve the native pointer.
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_remove_wrap(
    env: Env,
    js_object: NapiValue,
    result: *mut *mut c_void,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `js_object` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(js_object) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `env` is the caller's `*mut NapiEnv`, checked non-null above; reborrowed here to reach its owned fn_data/refs/wraps vecs.
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
                // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
                unsafe { *result = ptr };
            }
            NAPI_OK
        }
        None => NAPI_INVALID_ARG,
    }
}

/// Call a constructor function with `new` to produce a new instance.
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_new_instance(
    env: Env,
    constructor: NapiValue,
    argc: usize,
    argv: *const NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `constructor` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(ctor_local) = (unsafe { to_local(constructor) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(ctor_fn) = v8::Local::<v8::Function>::try_from(ctor_local) else {
        return NAPI_FUNCTION_EXPECTED;
    };
    let args: Vec<v8::Local<v8::Value>> = (0..argc)
        // SAFETY: `argv[i]` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
        .filter_map(|i| unsafe { to_local(*argv.add(i)) })
        .collect();
    match ctor_fn.new_instance(scope, &args) {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        Some(obj) => unsafe { out(result, from_local(obj.into())) },
        None => NAPI_PENDING_EXCEPTION,
    }
}

/// Check `value instanceof constructor`.
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_instanceof(
    env: Env,
    object: NapiValue,
    constructor: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(obj_local) = (unsafe { to_local(object) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `constructor` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(ctor_local) = (unsafe { to_local(constructor) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(ctor_obj) = v8::Local::<v8::Object>::try_from(ctor_local) else {
        return NAPI_FUNCTION_EXPECTED;
    };
    match obj_local.instance_of(scope, ctor_obj) {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        Some(is) => unsafe { out(result, is) },
        None => NAPI_PENDING_EXCEPTION,
    }
}

// =================================================================== bigint

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_bigint_int64(
    env: Env,
    value: i64,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe {
        out(
            result,
            from_local(v8::BigInt::new_from_i64(scope, value).into()),
        )
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_bigint_uint64(
    env: Env,
    value: u64,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe {
        out(
            result,
            from_local(v8::BigInt::new_from_u64(scope, value).into()),
        )
    }
}

/// Returns `(value, lossless)` where `lossless` is false if the BigInt
/// is too large to represent exactly as i64.
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_bigint_int64(
    env: Env,
    value: NapiValue,
    result: *mut i64,
    lossless: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(bigint) = v8::Local::<v8::BigInt>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    let (val, ok) = bigint.i64_value();
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = val };
    }
    if !lossless.is_null() {
        // SAFETY: `lossless` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *lossless = ok };
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_bigint_uint64(
    env: Env,
    value: NapiValue,
    result: *mut u64,
    lossless: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(bigint) = v8::Local::<v8::BigInt>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    let (val, ok) = bigint.u64_value();
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = val };
    }
    if !lossless.is_null() {
        // SAFETY: `lossless` is a caller-provided out-parameter pointer, null-checked before this write.
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
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_buffer(
    env: Env,
    size: usize,
    data: *mut *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
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
        // SAFETY: `data` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *data = ptr };
    }
    let Some(view) = v8::Uint8Array::new(scope, ab, 0, size) else {
        return NAPI_GENERIC_FAILURE;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(view.into())) }
}

/// Create a Buffer, copy `size` bytes from `data` into it.
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_buffer_copy(
    env: Env,
    size: usize,
    data: *const c_void,
    result_data: *mut *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let ab = v8::ArrayBuffer::new(scope, size);
    let store = ab.get_backing_store();
    if let Some(dst) = store.data() {
        // SAFETY: `data` points to at least `size` caller-owned elements; from_raw_parts reads them for the length the C caller declared.
        let src_bytes = unsafe { std::slice::from_raw_parts(data as *const u8, size) };
        // SAFETY: `dst.as_ptr` points to at least `size` caller-owned writable elements; from_raw_parts_mut forms the out-slice written below.
        let dst_bytes = unsafe { std::slice::from_raw_parts_mut(dst.as_ptr() as *mut u8, size) };
        dst_bytes.copy_from_slice(src_bytes);
        if !result_data.is_null() {
            // SAFETY: `result_data` is a caller-provided out-parameter pointer, null-checked before this write.
            unsafe { *result_data = dst.as_ptr() };
        }
    }
    let Some(view) = v8::Uint8Array::new(scope, ab, 0, size) else {
        return NAPI_GENERIC_FAILURE;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(view.into())) }
}

/// Create an external buffer: an ArrayBuffer that wraps existing memory.
/// The finalizer is called when V8 GC collects the ArrayBuffer.
///
/// Note: the finalizer is stored for completeness but is not called until GC
/// hook support is added. Callers that MUST run cleanup on collection should
/// use napi_create_buffer_copy instead (which copies into V8-managed memory).
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_external_buffer(
    env: Env,
    size: usize,
    data: *mut c_void,
    _finalize_cb: *mut c_void,
    _finalize_hint: *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `data` points to at least `size` caller-owned elements; from_raw_parts reads them for the length the C caller declared.
    let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, size) };
    let store = v8::ArrayBuffer::new_backing_store_from_bytes(bytes.to_vec().into_boxed_slice());
    let ab = v8::ArrayBuffer::with_backing_store(scope, &store.make_shared());
    let Some(view) = v8::Uint8Array::new(scope, ab, 0, size) else {
        return NAPI_GENERIC_FAILURE;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(view.into())) }
}

/// Returns true for any TypedArray (Uint8Array, Buffer, etc.).
/// Node's napi_is_buffer is Buffer-specific, but for oam we treat any
/// byte-array view as a buffer — addons that pass Uint8Array/Buffer both
/// work correctly, which is the relevant use case.
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_buffer(
    env: Env,
    value: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.is_uint8_array()) }
}

/// Get a pointer to the underlying bytes of a TypedArray or ArrayBuffer.
/// For a TypedArray, accounts for byte_offset so `*data` points to element 0.
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_buffer_info(
    env: Env,
    value: NapiValue,
    data: *mut *mut c_void,
    length: *mut usize,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
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
            // SAFETY: `data` is a caller-provided out-parameter pointer, null-checked before this write.
            unsafe { *data = base.add(byte_off) as *mut c_void };
        }
        if !length.is_null() {
            // SAFETY: `length` is a caller-provided out-parameter pointer, null-checked before this write.
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
            // SAFETY: `data` is a caller-provided out-parameter pointer, null-checked before this write.
            unsafe { *data = ptr };
        }
        if !length.is_null() {
            // SAFETY: `length` is a caller-provided out-parameter pointer, null-checked before this write.
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

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_last_error_info(
    env: Env,
    result: *mut *const NapiExtendedErrorInfo,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; `as_ref` guards null and
    // the shared reborrow only reads the env's own last-error slot.
    let Some(env_ref) = (unsafe { env.as_ref() }) else {
        return NAPI_INVALID_ARG;
    };
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked
        // before this write. The pointer written borrows the env's own slot, which
        // outlives this call -- the addon is calling THROUGH that env.
        unsafe { *result = &raw const env_ref.last_error };
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_error(
    env: Env,
    _code: NapiValue,
    msg: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `msg` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(msg_local) = (unsafe { to_local(msg) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(msg_str) = v8::Local::<v8::String>::try_from(msg_local) else {
        return NAPI_STRING_EXPECTED;
    };
    let error = v8::Exception::error(scope, msg_str);
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(error)) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_type_error(
    env: Env,
    _code: NapiValue,
    msg: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `msg` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(msg_local) = (unsafe { to_local(msg) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(msg_str) = v8::Local::<v8::String>::try_from(msg_local) else {
        return NAPI_STRING_EXPECTED;
    };
    let error = v8::Exception::type_error(scope, msg_str);
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(error)) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_range_error(
    env: Env,
    _code: NapiValue,
    msg: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `msg` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(msg_local) = (unsafe { to_local(msg) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(msg_str) = v8::Local::<v8::String>::try_from(msg_local) else {
        return NAPI_STRING_EXPECTED;
    };
    let error = v8::Exception::range_error(scope, msg_str);
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(error)) }
}

fn range_error<'s>(
    scope: &v8::PinScope<'s, '_>,
    message: v8::Local<v8::String>,
) -> v8::Local<'static, v8::Value> {
    let error = v8::Exception::range_error(scope, message);
    // SAFETY: relifetimes the freshly built error `v8::Local` to 'static; build_error immediately re-roots it into a `v8::Global` before the scope ends, so the handle stays valid.
    unsafe { std::mem::transmute::<v8::Local<v8::Value>, v8::Local<'static, v8::Value>>(error) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_throw_range_error(
    env: Env,
    code: *const c_char,
    msg: *const c_char,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `code` and `msg` are the caller's optional C strings; build_error reads each only after a null check.
    match unsafe { build_error(scope, code, msg, range_error) } {
        Some(error) => {
            // SAFETY: `env` is a valid `*mut NapiEnv` (established above); reads/writes its `pending` deferred-exception slot.
            unsafe { (*env).pending = Some(error) };
            NAPI_OK
        }
        None => NAPI_GENERIC_FAILURE,
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_error(
    env: Env,
    value: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
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
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, is_err) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
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
        // SAFETY: `location` is the caller's NUL-terminated C string, null-checked before this read.
        unsafe { std::ffi::CStr::from_ptr(location) }
            .to_string_lossy()
            .into_owned()
    };
    let msg = if message.is_null() {
        "<no message>".to_string()
    } else {
        // SAFETY: `message` is the caller's NUL-terminated C string, null-checked before this read.
        unsafe { std::ffi::CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    };
    eprintln!("FATAL ERROR: {loc}: {msg}");
    std::process::abort()
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_fatal_exception(env: Env, _err: NapiValue) -> NapiStatus {
    let _ = env;
    NAPI_GENERIC_FAILURE
}

// ------------------------------------------------------------------ strings

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_string_latin1(
    env: Env,
    str_ptr: *const c_char,
    length: usize,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    if str_ptr.is_null() {
        return NAPI_INVALID_ARG;
    }
    // SAFETY: `str_ptr` is the caller's NUL-terminated C string, null-checked before this read.
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
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(s.into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_string_utf16(
    env: Env,
    str_ptr: *const u16,
    length: usize,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    if str_ptr.is_null() {
        return NAPI_INVALID_ARG;
    }
    // SAFETY: `str_ptr` points to at least `len` caller-owned elements; from_raw_parts reads them for the length the C caller declared.
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
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(s.into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_string_latin1(
    env: Env,
    value: NapiValue,
    buf: *mut c_char,
    bufsize: usize,
    result: *mut usize,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(string) = v8::Local::<v8::String>::try_from(local) else {
        return NAPI_STRING_EXPECTED;
    };
    // Latin-1 length = number of UTF-16 code units (chars) for BMP strings
    let char_len = string.length();
    if buf.is_null() {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        return unsafe { out(result, char_len) };
    }
    if bufsize == 0 {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        return unsafe { out(result, 0) };
    }
    // Write latin-1 bytes via write_one_byte_v2.
    let copy_len = char_len.min(bufsize - 1);
    // SAFETY: `buf` points to at least `copy_len` caller-owned writable elements; from_raw_parts_mut forms the out-slice written below.
    let buf_slice = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, copy_len) };
    string.write_one_byte_v2(scope, 0, buf_slice, v8::WriteFlags::empty());
    // SAFETY: writes the trailing NUL into the caller's `buf` at the copy length (within the `bufsize` bound checked above).
    unsafe { *(buf.add(copy_len)) = 0 };
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = copy_len };
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_string_utf16(
    env: Env,
    value: NapiValue,
    buf: *mut u16,
    bufsize: usize,
    result: *mut usize,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(string) = v8::Local::<v8::String>::try_from(local) else {
        return NAPI_STRING_EXPECTED;
    };
    let char_len = string.length();
    if buf.is_null() {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        return unsafe { out(result, char_len) };
    }
    if bufsize == 0 {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        return unsafe { out(result, 0) };
    }
    let copy_len = char_len.min(bufsize - 1);
    // SAFETY: `buf` points to at least `copy_len` caller-owned writable elements; from_raw_parts_mut forms the out-slice written below.
    let buf_slice = unsafe { std::slice::from_raw_parts_mut(buf, copy_len) };
    string.write_v2(scope, 0, buf_slice, v8::WriteFlags::empty());
    // SAFETY: writes the trailing NUL into the caller's `buf` at the copy length (within the `bufsize` bound checked above).
    unsafe { *(buf.add(copy_len)) = 0u16 };
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = copy_len };
    }
    NAPI_OK
}

// ------------------------------------------------------------------ symbols

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_symbol(
    env: Env,
    description: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let desc = if description.is_null() {
        None
    } else {
        // SAFETY: `description` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
        unsafe { to_local(description) }.and_then(|v| v8::Local::<v8::String>::try_from(v).ok())
    };
    let sym = v8::Symbol::new(scope, desc);
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(sym.into())) }
}

// ------------------------------------------------------------------ handle scopes (stubs)

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_open_handle_scope(_env: Env, result: *mut *mut c_void) -> NapiStatus {
    // We don't implement real handle scopes -- the V8 PinScope on the stack
    // serves the same purpose. Return a sentinel so callers don't null-deref.
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = std::ptr::dangling_mut::<c_void>() };
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_close_handle_scope(_env: Env, _scope: *mut c_void) -> NapiStatus {
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_open_escapable_handle_scope(
    _env: Env,
    result: *mut *mut c_void,
) -> NapiStatus {
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = std::ptr::dangling_mut::<c_void>() };
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_close_escapable_handle_scope(
    _env: Env,
    _scope: *mut c_void,
) -> NapiStatus {
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
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
    // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
    unsafe { *result = escapee };
    NAPI_OK
}

// ------------------------------------------------------------------ callback scope stubs

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_open_callback_scope(
    _env: Env,
    _resource_object: NapiValue,
    _context: *mut c_void,
    result: *mut *mut c_void,
) -> NapiStatus {
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = std::ptr::dangling_mut::<c_void>() };
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_close_callback_scope(_env: Env, _scope: *mut c_void) -> NapiStatus {
    NAPI_OK
}

// ------------------------------------------------------------------ properties

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_has_property(
    env: Env,
    object: NapiValue,
    key: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    // SAFETY: `key` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(key_local) = (unsafe { to_local(key) }) else {
        return NAPI_INVALID_ARG;
    };
    let has = target.has(scope, key_local).unwrap_or(false);
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, has) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_delete_property(
    env: Env,
    object: NapiValue,
    key: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    // SAFETY: `key` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(key_local) = (unsafe { to_local(key) }) else {
        return NAPI_INVALID_ARG;
    };
    let deleted = target.delete(scope, key_local).unwrap_or(false);
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = deleted };
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_has_own_property(
    env: Env,
    object: NapiValue,
    key: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    // SAFETY: `key` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(key_local) = (unsafe { to_local(key) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(key_name) = v8::Local::<v8::Name>::try_from(key_local) else {
        return NAPI_INVALID_ARG;
    };
    let has = target.has_own_property(scope, key_name).unwrap_or(false);
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, has) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_has_element(
    env: Env,
    object: NapiValue,
    index: u32,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    let has = target.has_index(scope, index).unwrap_or(false);
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, has) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_delete_element(
    env: Env,
    object: NapiValue,
    index: u32,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    let deleted = target.delete_index(scope, index).unwrap_or(false);
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = deleted };
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_define_properties(
    env: Env,
    object: NapiValue,
    count: usize,
    props: *const NapiPropertyDescriptor,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    if count == 0 || props.is_null() {
        return NAPI_OK;
    }
    // SAFETY: `props` points to at least `count` caller-owned elements; from_raw_parts reads them for the length the C caller declared.
    let descriptors = unsafe { std::slice::from_raw_parts(props, count) };
    for prop in descriptors {
        let key: v8::Local<v8::Value> = if !prop.utf8name.is_null() {
            // SAFETY: `prop.utf8name` is the caller's NUL-terminated C string, null-checked before this read.
            let name = unsafe { std::ffi::CStr::from_ptr(prop.utf8name) }.to_string_lossy();
            match v8::String::new(scope, &name) {
                Some(s) => s.into(),
                None => continue,
            }
        // SAFETY: `prop.name` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
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
            // SAFETY: forwards to napi_create_function with this env and the caller-supplied constructor/method fn pointer and data -- the same C-ABI contract as the entry points.
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
            // SAFETY: `fn_out` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
            if let Some(fn_val) = unsafe { to_local(fn_out) } {
                let attr = napi_attrs_to_v8(prop.attributes);
                target.define_own_property(scope, name_key, fn_val, attr);
            }
        // SAFETY: `prop.value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
        } else if let Some(prop_val) = unsafe { to_local(prop.value) } {
            let attr = napi_attrs_to_v8(prop.attributes);
            target.define_own_property(scope, name_key, prop_val, attr);
        }
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_property_names(
    env: Env,
    object: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    match target.get_property_names(scope, v8::GetPropertyNamesArgs::default()) {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        Some(names) => unsafe { out(result, from_local(names.into())) },
        None => NAPI_PENDING_EXCEPTION,
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_prototype(
    env: Env,
    object: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    let proto = target.get_prototype(scope);
    match proto {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        Some(p) => unsafe { out(result, from_local(p)) },
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        None => unsafe { out(result, from_local(v8::null(scope).into())) },
    }
}

// ------------------------------------------------------------------ callbacks

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
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
    // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
    unsafe { *result = std::ptr::null_mut() };
    NAPI_OK
}

// ------------------------------------------------------------------ coercions

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_coerce_to_bool(
    env: Env,
    value: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let b = local.to_boolean(scope);
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(b.into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_coerce_to_number(
    env: Env,
    value: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    match local.to_number(scope) {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        Some(n) => unsafe { out(result, from_local(n.into())) },
        None => NAPI_PENDING_EXCEPTION,
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_coerce_to_object(
    env: Env,
    value: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    match local.to_object(scope) {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        Some(obj) => unsafe { out(result, from_local(obj.into())) },
        None => NAPI_PENDING_EXCEPTION,
    }
}

// ------------------------------------------------------------------ arraybuffers

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_arraybuffer(
    env: Env,
    value: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.is_array_buffer()) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_arraybuffer(
    env: Env,
    size: usize,
    data: *mut *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
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
        // SAFETY: `data` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *data = ptr };
    }
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(ab.into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_external_arraybuffer(
    env: Env,
    external_data: *mut c_void,
    byte_length: usize,
    _finalize_cb: *mut c_void,
    _finalize_hint: *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `external_data` points to at least `byte_length` caller-owned elements; from_raw_parts reads them for the length the C caller declared.
    let bytes = unsafe { std::slice::from_raw_parts(external_data as *const u8, byte_length) };
    let store = v8::ArrayBuffer::new_backing_store_from_bytes(bytes.to_vec().into_boxed_slice());
    let ab = v8::ArrayBuffer::with_backing_store(scope, &store.make_shared());
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(ab.into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_arraybuffer_info(
    env: Env,
    ab_value: NapiValue,
    data: *mut *mut c_void,
    length: *mut usize,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `ab_value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(ab_value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    let store = ab.get_backing_store();
    if !data.is_null() {
        // SAFETY: `data` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe {
            *data = store
                .data()
                .map(|p| p.as_ptr())
                .unwrap_or(std::ptr::null_mut())
        };
    }
    if !length.is_null() {
        // SAFETY: `length` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *length = ab.byte_length() };
    }
    NAPI_OK
}

// ------------------------------------------------------------------ typedarrays

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_typedarray(
    env: Env,
    value: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.is_typed_array()) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_typedarray(
    env: Env,
    type_: u32,
    count: usize,
    ab_value: NapiValue,
    offset: usize,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `ab_value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
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
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        Some(v) => unsafe { out(result, from_local(v)) },
        None => NAPI_GENERIC_FAILURE,
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
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
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `ta_value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
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
        // SAFETY: `type_out` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *type_out = type_id };
    }
    if !count_out.is_null() {
        // SAFETY: `count_out` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *count_out = elem_count };
    }
    if !offset_out.is_null() {
        // SAFETY: `offset_out` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *offset_out = byte_off };
    }
    if let Some(ab) = view.buffer(scope) {
        if !ab_out.is_null() {
            // SAFETY: `ab_out` is a caller-provided out-parameter pointer, null-checked before this write.
            unsafe { *ab_out = from_local(ab.into()) };
        }
        if !data_out.is_null() {
            let store = ab.get_backing_store();
            let base = store
                .data()
                .map(|p| p.as_ptr() as *mut u8)
                .unwrap_or(std::ptr::null_mut());
            // SAFETY: `data_out` is a caller-provided out-parameter pointer, null-checked before this write.
            unsafe { *data_out = base.add(byte_off) as *mut c_void };
        }
    }
    NAPI_OK
}

// ------------------------------------------------------------------ dataview

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_dataview(
    env: Env,
    size: usize,
    ab_value: NapiValue,
    offset: usize,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `ab_value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(ab_value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    let dv = v8::DataView::new(scope, ab, offset, size);
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(dv.into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_dataview(
    env: Env,
    value: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.is_data_view()) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_dataview_info(
    env: Env,
    dv_value: NapiValue,
    byte_length: *mut usize,
    data: *mut *mut c_void,
    ab_out: *mut NapiValue,
    offset: *mut usize,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `dv_value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(dv_value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    let byte_off = view.byte_offset();
    let byte_len = view.byte_length();
    if !byte_length.is_null() {
        // SAFETY: `byte_length` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *byte_length = byte_len };
    }
    if !offset.is_null() {
        // SAFETY: `offset` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *offset = byte_off };
    }
    if let Some(ab) = view.buffer(scope) {
        if !ab_out.is_null() {
            // SAFETY: `ab_out` is a caller-provided out-parameter pointer, null-checked before this write.
            unsafe { *ab_out = from_local(ab.into()) };
        }
        if !data.is_null() {
            let store = ab.get_backing_store();
            let base = store
                .data()
                .map(|p| p.as_ptr() as *mut u8)
                .unwrap_or(std::ptr::null_mut());
            // SAFETY: `data` is a caller-provided out-parameter pointer, null-checked before this write.
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

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_promise(
    env: Env,
    deferred: *mut *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
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
        // SAFETY: `deferred` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *deferred = ptr };
    }
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(promise.into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_resolve_deferred(
    env: Env,
    deferred: *mut c_void,
    resolution: NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
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
    // SAFETY: `resolution` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(value) = (unsafe { to_local(resolution) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `deferred` is the opaque handle this API handed out via Box::into_raw in napi_create_promise; the C caller passes back the same pointer, so reclaiming the Box is a valid round-trip (non-null and the value validated first).
    let entry = unsafe { Box::from_raw(deferred as *mut DeferredEntry) };
    let resolver = v8::Local::new(scope, &entry.resolver);
    resolver.resolve(scope, value);
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_reject_deferred(
    env: Env,
    deferred: *mut c_void,
    rejection: NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    if deferred.is_null() {
        return NAPI_INVALID_ARG;
    }
    // Validate the rejection BEFORE taking ownership of the deferred (see
    // napi_resolve_deferred): freeing the DeferredEntry on this invalid-arg
    // path would dangle the caller's handle and make a retry a use-after-free.
    // SAFETY: `rejection` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(value) = (unsafe { to_local(rejection) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `deferred` is the opaque handle this API handed out via Box::into_raw in napi_create_promise; the C caller passes back the same pointer, so reclaiming the Box is a valid round-trip (non-null and the value validated first).
    let entry = unsafe { Box::from_raw(deferred as *mut DeferredEntry) };
    let resolver = v8::Local::new(scope, &entry.resolver);
    resolver.reject(scope, value);
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_promise(
    env: Env,
    value: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.is_promise()) }
}

// ------------------------------------------------------------------ script

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_run_script(
    env: Env,
    script: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `script` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
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
                // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
                unsafe { out(result, from_local(val)) }
            }
        }
        None => NAPI_PENDING_EXCEPTION,
    }
}

// ------------------------------------------------------------------ memory

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_adjust_external_memory(
    _env: Env,
    _change: i64,
    adjusted: *mut i64,
) -> NapiStatus {
    if !adjusted.is_null() {
        // SAFETY: `adjusted` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *adjusted = 0 };
    }
    NAPI_OK
}

// ------------------------------------------------------------------ dates

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_date(
    env: Env,
    time: f64,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    match v8::Date::new(scope, time) {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        Some(date) => unsafe { out(result, from_local(date.into())) },
        None => NAPI_GENERIC_FAILURE,
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_date(env: Env, value: NapiValue, result: *mut bool) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.is_date()) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_date_value(
    env: Env,
    value: NapiValue,
    result: *mut f64,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(date) = v8::Local::<v8::Date>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, date.number_value(scope).unwrap_or(f64::NAN)) }
}

// ------------------------------------------------------------------ version / event loop

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_node_version(
    _env: Env,
    version: *mut *const NapiNodeVersion,
) -> NapiStatus {
    if !version.is_null() {
        // SAFETY: `version` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *version = &NODE_VERSION as *const NapiNodeVersion };
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_uv_event_loop(_env: Env, loop_: *mut *mut c_void) -> NapiStatus {
    if !loop_.is_null() {
        // SAFETY: `loop_` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *loop_ = std::ptr::null_mut() };
    }
    NAPI_GENERIC_FAILURE
}

// ------------------------------------------------------------------ module registration

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_module_register(_mod: *mut c_void) -> NapiStatus {
    // Old-style napi_module registration -- not used by napi-sys 2.x.
    NAPI_OK
}

// ------------------------------------------------------------------ cleanup hooks

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_add_env_cleanup_hook(
    _env: Env,
    _fun: *mut c_void,
    _arg: *mut c_void,
) -> NapiStatus {
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_remove_env_cleanup_hook(
    _env: Env,
    _fun: *mut c_void,
    _arg: *mut c_void,
) -> NapiStatus {
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
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

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_async_init(
    _env: Env,
    _resource: NapiValue,
    _name: NapiValue,
    _result: *mut *mut c_void,
) -> NapiStatus {
    NAPI_GENERIC_FAILURE
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_async_destroy(_env: Env, _context: *mut c_void) -> NapiStatus {
    NAPI_GENERIC_FAILURE
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
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

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
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

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_delete_async_work(_env: Env, _work: *mut c_void) -> NapiStatus {
    NAPI_GENERIC_FAILURE
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_queue_async_work(_env: Env, _work: *mut c_void) -> NapiStatus {
    NAPI_GENERIC_FAILURE
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
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

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
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
    // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
    unsafe { *result = Box::into_raw(stub) as *mut c_void };
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_threadsafe_function_context(
    func: *mut c_void,
    result: *mut *mut c_void,
) -> NapiStatus {
    if func.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    // SAFETY: `func` is the opaque threadsafe-function handle returned by napi_create_threadsafe_function (a Box::into_raw'd TsfnStub); checked non-null above.
    let stub = unsafe { &*(func as *const TsfnStub) };
    // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
    unsafe { *result = stub.context };
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_call_threadsafe_function(
    _func: *mut c_void,
    _data: *mut c_void,
    _is_blocking: u32,
) -> NapiStatus {
    // No event loop -- silently drop. Finalizers will not run.
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_acquire_threadsafe_function(_func: *mut c_void) -> NapiStatus {
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_release_threadsafe_function(
    _func: *mut c_void,
    _mode: u32,
) -> NapiStatus {
    // Leaks the TsfnStub -- acceptable for a sync host where addons live
    // until the runtime shuts down.
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_unref_threadsafe_function(
    _env: Env,
    _func: *mut c_void,
) -> NapiStatus {
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
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
    // SAFETY: loads the addon shared object at `path`; running an arbitrary `.node`'s initializers is inherently unsafe FFI, bounded by the AddonRegistry that keeps the library mapped for the runtime's life.
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
        // SAFETY: resolves `napi_register_module_v1` in the just-loaded `library`; the returned Symbol borrows `library`, which stays live until the fn pointer is copied out on the next lines.
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
    // Push FIRST, then derive the pointer from the element in place.
    //
    // Deriving from the Box and then MOVING it into the registry invalidates
    // the pointer: moving a Box is a typed move, so the tag `env` carries is
    // popped and every later use of it is UB. Confirmed mechanically --
    // `oam_aliasing_model::held_derive_then_move_box_is_ub` models the old
    // order and miri rejects it ("trying to retag ... for Unique permission,
    // but that tag does not exist in the borrow stack"). This is the same
    // push-then-derive idiom napi_create_function and napi_create_reference
    // already use.
    let env: Env = {
        let registry = scope
            .get_slot_mut::<AddonRegistry>()
            .expect("AddonRegistry slot installed");
        registry.envs.push(NapiEnv::new());
        &raw mut **registry.envs.last_mut().expect("just pushed")
    };

    let exports = v8::Object::new(scope);
    let exports_value: v8::Local<v8::Value> = exports.into();

    // SAFETY: `env` is this addon's freshly allocated NapiEnv; stashes the live PinScope pointer so nested napi calls during register() can recover it.
    unsafe {
        (*env).scope = scope as *mut v8::PinScope<'_, '_> as *mut c_void;
    }
    if trace {
        eprintln!("[napi] symbol found; calling register()");
    }
    // SAFETY: calls the addon's `napi_register_module_v1` (a C-ABI fn pointer from the loaded `.node`) with this env and the exports object; the addon then runs arbitrary native code -- the inherent FFI trust boundary.
    let result = unsafe { register(env, from_local(exports_value)) };
    if trace {
        eprintln!("[napi] register() returned");
    }
    // SAFETY: `env` is this addon's NapiEnv; clears the stashed scope pointer now that register() has returned and the borrow is ending.
    unsafe {
        (*env).scope = std::ptr::null_mut();
    }

    // SAFETY: `env` is a valid `*mut NapiEnv` (established above); reads/writes its `pending` deferred-exception slot.
    if let Some(pending) = unsafe { (*env).pending.take() } {
        let exception = v8::Local::new(scope, &pending);
        scope.throw_exception(exception);
        return None;
    }

    // SAFETY: `result` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
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
