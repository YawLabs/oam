//! A real N-API addon, in Rust: compiled to a cdylib, renamed .node by the
//! e2e suite, loaded by oam via require(). It resolves every napi_*
//! symbol FROM THE HOST PROCESS'S EXPORT TABLE at registration — the
//! portable equivalent of node-gyp's delay-load hook, and a direct proof
//! that the oam executable exports the ABI.
//!
//! Alpha exports: add(a, b), greet(name), concat(list), answer (= 42),
//!   boom() which throws a TypeError with a code, makeInt64(hi, lo).
//!
//! Beta exports -- the `beta_bindings` table in `napi_register_module_v1` is
//! the authoritative list; each callback's own doc comment has the detail.
//!
//!   wrap:
//!     wrapCounter()       -> obj        napi_create_object + napi_wrap
//!     wrapForeign()       -> obj        napi_wrap around a pointer that is
//!                                       not a counter
//!     counterGet(obj)     -> Number     napi_unwrap
//!     counterInc(obj)                   napi_unwrap + mutate
//!   bigint / buffer:
//!     makeBigInt64(lo)    -> BigInt     napi_create_bigint_int64
//!     readBigInt64(big)   -> Number     napi_get_value_bigint_int64
//!     makeBuffer(n)       -> Uint8Array napi_create_buffer
//!     bufferLen(buf)      -> Number     napi_get_buffer_info
//!   references (each returns the host's status, or a count / handle):
//!     refRefCount(v), refUnrefCount(v)      napi_reference_ref / _unref
//!     refAfterDelete(v), refRefAfterDelete(v), refUnrefAfterDelete(v),
//!     deleteRefTwice(v), createRefNullOut(v), foreignHandleStatus()
//!                                           refusals of an invalid handle
//!     makeRefHandle(v) -> BigInt, readRefHandle(h), readRefInt(h),
//!     deleteRefHandle(h)                    a raw handle driven from JS
//!     abaStaleReadStatus(), abaHandleRecycled()
//!                                           slot reuse after a delete
//!     lastErrorMessage(v) -> string         napi_get_last_error_info
//!   sized names:
//!     sizedNames()        -> obj        napi_create_function /
//!                                       napi_define_class with an explicit
//!                                       name length; see [`sized_names`]
//!     stringRules()       -> obj        statuses of the string constructors
//!                                       and napi_define_class at the edges
//!                                       of their text; see [`string_rules`]
//!     descriptorNames()   -> obj        functions made from property
//!                                       descriptors; see [`descriptor_names`]
//!
//! Teardown probe -- no export; armed at registration when the environment
//! names a log file in `OAM_NAPI_TEARDOWN_LOG`. See [`teardown_arm`].

use std::ffi::{c_char, c_void};

type NapiEnv = *mut c_void;
type NapiValue = *mut c_void;
type NapiStatus = i32;
type NapiCallbackInfo = *mut c_void;
type NapiCallback = unsafe extern "C" fn(NapiEnv, NapiCallbackInfo) -> NapiValue;
type NapiFinalizeCb = Option<unsafe extern "C" fn(NapiEnv, *mut c_void, *mut c_void)>;
/// The `void (*fun)(void* arg)` of `napi_add_env_cleanup_hook`.
type NapiCleanupHook = extern "C" fn(*mut c_void);

/// The napi functions this addon consumes, resolved from the host.
struct Host {
    // ----- alpha -----
    create_function: unsafe extern "C" fn(
        NapiEnv,
        *const c_char,
        usize,
        Option<NapiCallback>,
        *mut c_void,
        *mut NapiValue,
    ) -> NapiStatus,
    get_cb_info: unsafe extern "C" fn(
        NapiEnv,
        NapiCallbackInfo,
        *mut usize,
        *mut NapiValue,
        *mut NapiValue, // this
        *mut *mut c_void,
    ) -> NapiStatus,
    set_named_property:
        unsafe extern "C" fn(NapiEnv, NapiValue, *const c_char, NapiValue) -> NapiStatus,
    create_int32: unsafe extern "C" fn(NapiEnv, i32, *mut NapiValue) -> NapiStatus,
    create_int64: unsafe extern "C" fn(NapiEnv, i64, *mut NapiValue) -> NapiStatus,
    get_value_int32: unsafe extern "C" fn(NapiEnv, NapiValue, *mut i32) -> NapiStatus,
    create_string_utf8:
        unsafe extern "C" fn(NapiEnv, *const c_char, usize, *mut NapiValue) -> NapiStatus,
    get_value_string_utf8:
        unsafe extern "C" fn(NapiEnv, NapiValue, *mut c_char, usize, *mut usize) -> NapiStatus,
    throw_type_error: unsafe extern "C" fn(NapiEnv, *const c_char, *const c_char) -> NapiStatus,
    get_array_length: unsafe extern "C" fn(NapiEnv, NapiValue, *mut u32) -> NapiStatus,
    get_element: unsafe extern "C" fn(NapiEnv, NapiValue, u32, *mut NapiValue) -> NapiStatus,
    get_undefined: unsafe extern "C" fn(NapiEnv, *mut NapiValue) -> NapiStatus,

    // ----- beta -----
    create_object: unsafe extern "C" fn(NapiEnv, *mut NapiValue) -> NapiStatus,
    wrap: unsafe extern "C" fn(
        NapiEnv,
        NapiValue,
        *mut c_void,
        NapiFinalizeCb,
        *mut c_void,
        *mut *mut c_void,
    ) -> NapiStatus,
    unwrap: unsafe extern "C" fn(NapiEnv, NapiValue, *mut *mut c_void) -> NapiStatus,
    create_bigint_int64: unsafe extern "C" fn(NapiEnv, i64, *mut NapiValue) -> NapiStatus,
    get_value_bigint_int64:
        unsafe extern "C" fn(NapiEnv, NapiValue, *mut i64, *mut bool) -> NapiStatus,
    create_buffer:
        unsafe extern "C" fn(NapiEnv, usize, *mut *mut c_void, *mut NapiValue) -> NapiStatus,
    get_buffer_info:
        unsafe extern "C" fn(NapiEnv, NapiValue, *mut *mut c_void, *mut usize) -> NapiStatus,
    create_reference: unsafe extern "C" fn(NapiEnv, NapiValue, u32, *mut *mut c_void) -> NapiStatus,
    get_reference_value: unsafe extern "C" fn(NapiEnv, *mut c_void, *mut NapiValue) -> NapiStatus,
    delete_reference: unsafe extern "C" fn(NapiEnv, *mut c_void) -> NapiStatus,
    reference_ref: unsafe extern "C" fn(NapiEnv, *mut c_void, *mut u32) -> NapiStatus,
    reference_unref: unsafe extern "C" fn(NapiEnv, *mut c_void, *mut u32) -> NapiStatus,
    get_last_error_info:
        unsafe extern "C" fn(NapiEnv, *mut *const NapiExtendedErrorInfo) -> NapiStatus,
    add_env_cleanup_hook: unsafe extern "C" fn(NapiEnv, NapiCleanupHook, *mut c_void) -> NapiStatus,
    remove_env_cleanup_hook:
        unsafe extern "C" fn(NapiEnv, NapiCleanupHook, *mut c_void) -> NapiStatus,
    get_global: unsafe extern "C" fn(NapiEnv, *mut NapiValue) -> NapiStatus,
    get_named_property:
        unsafe extern "C" fn(NapiEnv, NapiValue, *const c_char, *mut NapiValue) -> NapiStatus,
    call_function: unsafe extern "C" fn(
        NapiEnv,
        NapiValue,
        NapiValue,
        usize,
        *const NapiValue,
        *mut NapiValue,
    ) -> NapiStatus,
    define_class: unsafe extern "C" fn(
        NapiEnv,
        *const c_char,
        usize,
        Option<NapiCallback>,
        *mut c_void,
        usize,
        *const NapiPropertyDescriptor,
        *mut NapiValue,
    ) -> NapiStatus,
    define_properties: unsafe extern "C" fn(
        NapiEnv,
        NapiValue,
        usize,
        *const NapiPropertyDescriptor,
    ) -> NapiStatus,
    create_string_latin1:
        unsafe extern "C" fn(NapiEnv, *const c_char, usize, *mut NapiValue) -> NapiStatus,
    create_string_utf16:
        unsafe extern "C" fn(NapiEnv, *const u16, usize, *mut NapiValue) -> NapiStatus,
    create_symbol: unsafe extern "C" fn(NapiEnv, NapiValue, *mut NapiValue) -> NapiStatus,
    get_and_clear_last_exception: unsafe extern "C" fn(NapiEnv, *mut NapiValue) -> NapiStatus,
}

/// Mirrors `napi_property_descriptor` from js_native_api_types.h.
#[repr(C)]
struct NapiPropertyDescriptor {
    utf8name: *const c_char,
    name: NapiValue,
    method: Option<NapiCallback>,
    getter: Option<NapiCallback>,
    setter: Option<NapiCallback>,
    value: NapiValue,
    attributes: u32,
    data: *mut c_void,
}

/// Mirrors `napi_extended_error_info` from js_native_api_types.h.
#[repr(C)]
struct NapiExtendedErrorInfo {
    error_message: *const c_char,
    engine_reserved: *mut c_void,
    engine_error_code: u32,
    error_code: NapiStatus,
}

static HOST: std::sync::OnceLock<Host> = std::sync::OnceLock::new();

fn host() -> &'static Host {
    HOST.get().expect("host napi symbols resolved")
}

/// # Safety
///
/// Must be called only from the addon's N-API registration entry point, on
/// the thread the host used to load this cdylib, and never concurrently: it
/// pulls raw addresses out of the host executable's export table and
/// reinterprets each one as the N-API function-pointer type declared in
/// `Host`. A caller running it inside a process that does not actually export
/// the `napi_*` ABI would install wrong-signature pointers in `HOST`. The
/// returned `Host` is valid only while the host process image stays loaded,
/// i.e. for the rest of the process lifetime.
unsafe fn resolve_host() -> Option<Host> {
    // SAFETY: `Library::this()` borrows the already-loaded host executable
    // image -- it opens nothing new and there is no handle to outlive, so the
    // symbols stay valid for the life of the process. Every `sym!` lookup
    // names a `napi_*` symbol that oam_cli/build.rs re-exports from that
    // image, and each resolved address is coerced to the exact declared
    // N-API signature in `Host` above, which mirrors the ABI Node publishes
    // for those entry points. A symbol that is absent makes `get()` return
    // `Err`, and `?` bails out with `None` rather than storing a bogus
    // pointer.
    unsafe {
        #[cfg(windows)]
        let this = libloading::os::windows::Library::this().ok()?;
        #[cfg(not(windows))]
        let this = libloading::os::unix::Library::this();

        macro_rules! sym {
            ($name:literal) => {
                *this.get($name).ok()?
            };
        }
        Some(Host {
            // alpha
            create_function: sym!(b"napi_create_function"),
            get_cb_info: sym!(b"napi_get_cb_info"),
            set_named_property: sym!(b"napi_set_named_property"),
            create_int32: sym!(b"napi_create_int32"),
            create_int64: sym!(b"napi_create_int64"),
            get_value_int32: sym!(b"napi_get_value_int32"),
            create_string_utf8: sym!(b"napi_create_string_utf8"),
            get_value_string_utf8: sym!(b"napi_get_value_string_utf8"),
            throw_type_error: sym!(b"napi_throw_type_error"),
            get_array_length: sym!(b"napi_get_array_length"),
            get_element: sym!(b"napi_get_element"),
            get_undefined: sym!(b"napi_get_undefined"),
            // beta
            create_object: sym!(b"napi_create_object"),
            wrap: sym!(b"napi_wrap"),
            unwrap: sym!(b"napi_unwrap"),
            create_bigint_int64: sym!(b"napi_create_bigint_int64"),
            get_value_bigint_int64: sym!(b"napi_get_value_bigint_int64"),
            create_buffer: sym!(b"napi_create_buffer"),
            get_buffer_info: sym!(b"napi_get_buffer_info"),
            create_reference: sym!(b"napi_create_reference"),
            get_reference_value: sym!(b"napi_get_reference_value"),
            delete_reference: sym!(b"napi_delete_reference"),
            reference_ref: sym!(b"napi_reference_ref"),
            reference_unref: sym!(b"napi_reference_unref"),
            get_last_error_info: sym!(b"napi_get_last_error_info"),
            add_env_cleanup_hook: sym!(b"napi_add_env_cleanup_hook"),
            remove_env_cleanup_hook: sym!(b"napi_remove_env_cleanup_hook"),
            get_global: sym!(b"napi_get_global"),
            get_named_property: sym!(b"napi_get_named_property"),
            call_function: sym!(b"napi_call_function"),
            define_class: sym!(b"napi_define_class"),
            define_properties: sym!(b"napi_define_properties"),
            create_string_latin1: sym!(b"napi_create_string_latin1"),
            create_string_utf16: sym!(b"napi_create_string_utf16"),
            create_symbol: sym!(b"napi_create_symbol"),
            get_and_clear_last_exception: sym!(b"napi_get_and_clear_last_exception"),
        })
    }
}

// ================================================================ alpha impls

/// # Safety
///
/// `env` must be the live `napi_env` handed to the callback currently on the
/// stack, and `value` a `napi_value` valid in that env; `HOST` must already
/// have been populated by `resolve_host`. A non-string `value` is not UB --
/// the sizing call fails and an empty `String` is returned.
unsafe fn read_string(env: NapiEnv, value: NapiValue) -> String {
    // SAFETY: This is the two-pass shape `napi_get_value_string_utf8`
    // documents. The first call passes a null buffer with capacity 0 purely
    // to learn `len`, the UTF-8 byte length *excluding* the terminator; a
    // non-zero status means `len` was never written, so that path returns
    // early instead of reading it. The buffer is then allocated `len + 1`
    // bytes so the ABI has room to append its NUL, and the capacity given to
    // the second call is `buf.len()` -- the allocation's true size -- so the
    // host physically cannot write past the end. `written` comes back as the
    // byte count excluding the NUL, so truncating to it drops the terminator
    // and leaves only host-initialized bytes for `from_utf8_lossy`.
    unsafe {
        let mut len = 0usize;
        if (host().get_value_string_utf8)(env, value, std::ptr::null_mut(), 0, &mut len) != 0 {
            return String::new();
        }
        let mut buf = vec![0u8; len + 1];
        let mut written = 0usize;
        (host().get_value_string_utf8)(
            env,
            value,
            buf.as_mut_ptr() as *mut c_char,
            buf.len(),
            &mut written,
        );
        buf.truncate(written);
        String::from_utf8_lossy(&buf).into_owned()
    }
}

/// # Safety
///
/// `env` must be the live `napi_env` of the callback in progress and `HOST`
/// must already be resolved. The returned `napi_value` belongs to the current
/// handle scope, so the caller must not let it escape that scope.
unsafe fn make_string(env: NapiEnv, text: &str) -> NapiValue {
    // SAFETY: `text.as_ptr()` / `text.len()` describe a UTF-8 byte range that
    // stays live for the whole call, and `napi_create_string_utf8` copies out
    // of it rather than retaining it -- so no NUL terminator is required, the
    // explicit length is what the ABI reads. `value` is a live local, making
    // the out-pointer the host writes through valid and correctly aligned.
    unsafe {
        let mut value: NapiValue = std::ptr::null_mut();
        (host().create_string_utf8)(env, text.as_ptr() as *const c_char, text.len(), &mut value);
        value
    }
}

/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback`: `env` is the
/// live `napi_env` on the callback stack and `info` the matching
/// `napi_callback_info`, both valid for the duration of the call. `add`
/// reads two int32 arguments out of that frame, so calling it with a
/// hand-built `info` is undefined behaviour.
unsafe extern "C" fn add(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `argc` is initialized to 2, the true capacity of the `argv`
    // stack array, and that in/out count is how `napi_get_cb_info` learns how
    // many slots it may fill -- so the host cannot overrun the array no
    // matter how many arguments JS actually passed. Any slot the host does
    // not fill is either an `undefined` handle or the null it was initialized
    // to; both make `napi_get_value_int32` return a non-zero status, which
    // routes to the TypeError path instead of reading a garbage handle. The
    // `this` and `data` out-params are null because neither is wanted.
    unsafe {
        let mut argc = 2usize;
        let mut argv: [NapiValue; 2] = [std::ptr::null_mut(); 2];
        (host().get_cb_info)(
            env,
            info,
            &mut argc,
            argv.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        let (mut a, mut b) = (0i32, 0i32);
        if (host().get_value_int32)(env, argv[0], &mut a) != 0
            || (host().get_value_int32)(env, argv[1], &mut b) != 0
        {
            (host().throw_type_error)(
                env,
                c"ERR_NATIVE_ARGS".as_ptr(),
                c"add(a, b) wants two numbers".as_ptr(),
            );
            return std::ptr::null_mut();
        }
        let mut result: NapiValue = std::ptr::null_mut();
        (host().create_int32)(env, a.wrapping_add(b), &mut result);
        result
    }
}

/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback`, with a live
/// `env` and the `info` frame belonging to this call. `greet` reads argument
/// 0 as a string, so it must not be called with a fabricated `info`.
unsafe extern "C" fn greet(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `argc` starts at 1, `argv`'s real capacity, so
    // `napi_get_cb_info` fills at most the one slot that exists. `read_string`
    // and `make_string` receive the same live `env` from this callback frame,
    // and `read_string` tolerates a non-string or unfilled `argv[0]` by
    // returning an empty String rather than dereferencing a bad handle.
    unsafe {
        let mut argc = 1usize;
        let mut argv: [NapiValue; 1] = [std::ptr::null_mut(); 1];
        (host().get_cb_info)(
            env,
            info,
            &mut argc,
            argv.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        let name = read_string(env, argv[0]);
        make_string(env, &format!("hello from native, {name}"))
    }
}

/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback`, with a live
/// `env` and its matching `info`. `concat` treats argument 0 as a JS array
/// and reads each element as a string.
unsafe extern "C" fn concat(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `argc` is seeded with `argv`'s real capacity of 1, bounding what
    // `napi_get_cb_info` may write into the stack array. The element loop is
    // bounded by the `length` the host itself reported for `argv[0]`, so every
    // `napi_get_element` index is in range by construction; if the value is
    // not an array, `napi_get_array_length` leaves `length` at its initialized
    // 0 and the loop body never runs. Each element goes through `read_string`,
    // which does its own two-pass sizing.
    unsafe {
        let mut argc = 1usize;
        let mut argv: [NapiValue; 1] = [std::ptr::null_mut(); 1];
        (host().get_cb_info)(
            env,
            info,
            &mut argc,
            argv.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        let mut length = 0u32;
        (host().get_array_length)(env, argv[0], &mut length);
        let mut joined = String::new();
        for i in 0..length {
            let mut element: NapiValue = std::ptr::null_mut();
            (host().get_element)(env, argv[0], i, &mut element);
            if i > 0 {
                joined.push('+');
            }
            joined.push_str(&read_string(env, element));
        }
        make_string(env, &joined)
    }
}

/// makeInt64(hi: i32, lo: i32) -> number (napi_create_int64 is always a Number)
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback`, with a live
/// `env` and the `info` frame for this call. Reads two int32 arguments from
/// that frame and recombines them into an i64.
unsafe extern "C" fn make_int64(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `argc` is initialized to 2, `argv`'s true capacity, so
    // `napi_get_cb_info` can fill at most the two slots that exist regardless
    // of how many arguments JS passed. Either conversion failing -- a missing
    // or non-numeric argument -- yields a non-zero status and takes the throw
    // path, so `hi` and `lo` are combined only once both reads demonstrably
    // succeeded. The shift and widening afterwards are pure integer
    // arithmetic on values already owned by Rust.
    unsafe {
        let mut argc = 2usize;
        let mut argv: [NapiValue; 2] = [std::ptr::null_mut(); 2];
        (host().get_cb_info)(
            env,
            info,
            &mut argc,
            argv.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        let (mut hi, mut lo) = (0i32, 0i32);
        if (host().get_value_int32)(env, argv[0], &mut hi) != 0
            || (host().get_value_int32)(env, argv[1], &mut lo) != 0
        {
            (host().throw_type_error)(
                env,
                c"ERR_NATIVE_ARGS".as_ptr(),
                c"makeInt64(hi, lo) wants two i32s".as_ptr(),
            );
            return std::ptr::null_mut();
        }
        let value: i64 = ((hi as i64) << 32) | (lo as u32 as i64);
        let mut result: NapiValue = std::ptr::null_mut();
        (host().create_int64)(env, value, &mut result);
        result
    }
}

/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback` with a live
/// `env`; the `info` frame is ignored. Returns with a pending TypeError on
/// `env`, so the host must treat the returned value as a throw rather than a
/// result.
unsafe extern "C" fn boom(env: NapiEnv, _info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: Both pointers are `c"..."` literals -- static storage, already
    // NUL terminated -- which is exactly what `napi_throw_type_error` expects,
    // and it copies the strings rather than retaining them, so nothing needs
    // to outlive this call. `undefined` is a live local, so its address is a
    // valid out-param for `napi_get_undefined`; the resulting handle is only
    // returned, never dereferenced here.
    unsafe {
        (host().throw_type_error)(env, c"ERR_NATIVE_BOOM".as_ptr(), c"native says no".as_ptr());
        let mut undefined: NapiValue = std::ptr::null_mut();
        (host().get_undefined)(env, &mut undefined);
        undefined
    }
}

// ================================================================ beta impls

/// Every counter `wrap_counter` has handed to the host and whose finalizer has
/// not yet run, keyed by the address of its heap allocation -- the exact value
/// passed to `napi_wrap` as the native pointer.
///
/// The addon never dereferences the pointer `napi_unwrap` returns. That
/// pointer comes back from the host as an untyped `void*`, and nothing in it
/// proves it still points at a live `i64` this addon allocated: the object may
/// have been wrapped by a different addon (or by this one with a different
/// type), or its finalizer may already have reclaimed the allocation. Casting
/// it to `*mut i64` and reading through it would be a type confusion or a
/// use-after-free in either case. Instead the address is used only as a key
/// into this table, which owns the allocation; a key outside the table's live
/// addresses is absent. The key cannot prove provenance: a foreign object
/// holding a stale address that coincides with a live counter's would match,
/// so lookup is an identity check, not an ownership check -- harmless here
/// only because nothing is ever dereferenced through it.
///
/// A `BTreeMap` because `BTreeMap::new` is `const`, so the table needs no lazy
/// initializer.
static COUNTERS: std::sync::Mutex<std::collections::BTreeMap<usize, Box<i64>>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Locks `COUNTERS`, recovering the table from a poisoned lock rather than
/// panicking: every caller is an `extern "C"` callback, where an unwind would
/// abort the host. The table holds only plain integers, so a panic part-way
/// through an update cannot leave it in a state later readers would misuse.
fn counters() -> std::sync::MutexGuard<'static, std::collections::BTreeMap<usize, Box<i64>>> {
    COUNTERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Registers a new zeroed counter and returns the native pointer to hand to
/// `napi_wrap`. The table keeps ownership of the allocation; the pointer is
/// only an identity token for it.
fn counter_register() -> *mut c_void {
    let counter = Box::new(0i64);
    let native = std::ptr::from_ref::<i64>(&*counter)
        .cast_mut()
        .cast::<c_void>();
    counters().insert(native.addr(), counter);
    native
}

/// Drops the counter registered under `native`, if any. Returns whether one
/// was registered, so a finalizer invoked twice, or with a foreign pointer,
/// is a no-op rather than a double free.
fn counter_release(native: *mut c_void) -> bool {
    counters().remove(&native.addr()).is_some()
}

/// The current value of the counter registered under `native`, or `None` when
/// `native` is not a live counter of this addon.
fn counter_value(native: *mut c_void) -> Option<i64> {
    counters().get(&native.addr()).map(|counter| **counter)
}

/// Increments the counter registered under `native`. Returns `false`, leaving
/// every counter untouched, when `native` is not a live counter of this addon.
fn counter_increment(native: *mut c_void) -> bool {
    match counters().get_mut(&native.addr()) {
        Some(counter) => {
            **counter += 1;
            true
        }
        None => false,
    }
}

/// The `napi_finalize` registered with each wrap. The host calls it once the
/// wrapped object has been collected -- not from inside the collector, but
/// the next time this addon is called into -- and for every wrap still in
/// place when the runtime is torn down. `napi_remove_wrap` cancels it, so it
/// is NOT called for a removed wrap (nothing here removes one).
///
/// Releases the counter registered for the wrapped pointer. Safe to call with
/// any pointer, any number of times: only a registered counter is dropped.
///
/// No e2e script forces a collection, so the collected-object path is not
/// exercised from this addon. The teardown path is: the e2e suite's
/// `napi_teardown_*` tests read the `finalize` line this writes when the
/// teardown probe is armed, and a process that exits without dropping its
/// runtime (`process.exit()`) never reaches it. The host-side contract
/// (finalize once at release, never after `napi_remove_wrap`) is unit-tested
/// in `oam_engine::napi` (`wrap_roundtrip_reference_and_finalizer`).
extern "C" fn counter_finalize(_env: NapiEnv, data: *mut c_void, _hint: *mut c_void) {
    counter_release(data);
    teardown_note("finalize");
}

/// wrapCounter() -> obj
/// Creates a new plain JS object and wraps the address of a registry-owned
/// counter (value 0) as an identity token.
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback` with a live
/// `env`; the `info` frame is ignored. The returned JS object's finalizer
/// (`counter_finalize`) releases the counter registered for it.
unsafe extern "C" fn wrap_counter(env: NapiEnv, _info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `obj` is a live local, so `napi_create_object`'s out-pointer is
    // valid, and a non-zero status bails out before the handle is used. The
    // native pointer given to `napi_wrap` is an identity token the host only
    // stores and hands back; the allocation behind it is owned by `COUNTERS`,
    // never by the host, so nothing the host does with it can free or alias
    // it. The trailing nulls are the finalize hint and the optional
    // `napi_ref` out-param, neither of which this addon wants. If `napi_wrap`
    // fails the counter is released here, since no finalizer will ever run
    // for it.
    unsafe {
        let mut obj: NapiValue = std::ptr::null_mut();
        if (host().create_object)(env, &mut obj) != 0 {
            return std::ptr::null_mut();
        }
        let native = counter_register();
        let finalize: unsafe extern "C" fn(NapiEnv, *mut c_void, *mut c_void) = counter_finalize;
        if (host().wrap)(
            env,
            obj,
            native,
            Some(finalize),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ) != 0
        {
            counter_release(native);
            return std::ptr::null_mut();
        }
        obj
    }
}

/// wrapForeign() -> obj
/// Creates a plain JS object wrapped around a pointer `counter_register`
/// never produced -- the shape an object wrapped by a different addon
/// presents to this one's counter callbacks.
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback` with a live
/// `env`; the `info` frame is ignored. The wrapped pointer is fabricated but
/// never dereferenced by anyone: the host stores it, and `counter_finalize`
/// only removes registry keys, so the finalizer drops nothing either way.
unsafe extern "C" fn wrap_foreign(env: NapiEnv, _info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `obj` is a live local, so `napi_create_object`'s out-pointer is
    // valid, and a non-zero status bails out before the handle is used. The
    // fabricated pointer is an identity token the host only stores and hands
    // back for registry lookup -- never dereferenced -- and `counter_finalize`
    // removes only registered keys, so finalizing it is a no-op rather than a
    // bad free. The trailing nulls are the finalize hint and the optional
    // `napi_ref` out-param, neither of which this fixture wants.
    unsafe {
        let mut obj: NapiValue = std::ptr::null_mut();
        if (host().create_object)(env, &mut obj) != 0 {
            return std::ptr::null_mut();
        }
        let foreign = std::ptr::without_provenance_mut::<c_void>(0xdead_beef_usize);
        let finalize: unsafe extern "C" fn(NapiEnv, *mut c_void, *mut c_void) = counter_finalize;
        if (host().wrap)(
            env,
            obj,
            foreign,
            Some(finalize),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ) != 0
        {
            return std::ptr::null_mut();
        }
        obj
    }
}

/// Reads argument 0 of the current call and returns the native pointer
/// wrapped on it, or `None` when it carries no wrap.
///
/// # Safety
///
/// `env` must be live and `info` must be the callback-info frame of the
/// N-API call currently executing on it.
unsafe fn unwrap_arg0(env: NapiEnv, info: NapiCallbackInfo) -> Option<*mut c_void> {
    // SAFETY: `argc` is seeded with `argv`'s real capacity of 1, so
    // `napi_get_cb_info` cannot overrun the stack array; `env` and `info` are
    // live per this function's contract. `native` is a live local, so its
    // address is a valid out-param for `napi_unwrap`. The returned pointer is
    // never dereferenced -- callers only use it as a `COUNTERS` key.
    unsafe {
        let mut argc = 1usize;
        let mut argv: [NapiValue; 1] = [std::ptr::null_mut(); 1];
        (host().get_cb_info)(
            env,
            info,
            &mut argc,
            argv.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        let mut native: *mut c_void = std::ptr::null_mut();
        if (host().unwrap)(env, argv[0], &mut native) != 0 {
            return None;
        }
        Some(native)
    }
}

/// counterGet(obj) -> Number
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback` with a live `env`
/// and its `info` frame. Argument 0 is expected to be an object produced by
/// `wrapCounter`; any other value -- unwrapped, or wrapped with a pointer this
/// addon did not register -- yields null.
unsafe extern "C" fn counter_get(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `env` and `info` come straight from the host for this call,
    // which is `unwrap_arg0`'s contract. The value is read out of `COUNTERS`
    // by key, with no raw-pointer dereference, and `result` is a live local
    // whose address is a valid out-param for `napi_create_int64`.
    unsafe {
        let Some(val) = unwrap_arg0(env, info).and_then(counter_value) else {
            return std::ptr::null_mut();
        };
        let mut result: NapiValue = std::ptr::null_mut();
        (host().create_int64)(env, val, &mut result);
        result
    }
}

/// counterInc(obj) -- increments the wrapped counter
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback` with a live `env`
/// and its `info` frame. Argument 0 is expected to be a `wrapCounter` object;
/// the counter registered for it is incremented. Any other value yields null
/// and changes nothing.
unsafe extern "C" fn counter_inc(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `env` and `info` come straight from the host for this call,
    // which is `unwrap_arg0`'s contract. The increment goes through
    // `COUNTERS` by key, with no raw-pointer dereference, and `undef` is a
    // live local whose address is a valid out-param for `napi_get_undefined`.
    unsafe {
        if !unwrap_arg0(env, info).is_some_and(counter_increment) {
            return std::ptr::null_mut();
        }
        let mut undef: NapiValue = std::ptr::null_mut();
        (host().get_undefined)(env, &mut undef);
        undef
    }
}

/// makeBigInt64(lo: number) -> BigInt
/// Creates a BigInt from an i64 value. `lo` is read as an i32 from JS.
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback` with a live `env`
/// and its `info` frame. Reads argument 0 as an int32 and widens it to i64.
unsafe extern "C" fn make_bigint64(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `argc` starts at `argv`'s true capacity of 1, so
    // `napi_get_cb_info` fills at most the slot that exists. A missing or
    // non-numeric argument leaves `n` at its initialized 0 -- the failed
    // status is deliberately ignored here, the fixture yields BigInt(0)
    // instead of throwing -- so no uninitialized value is ever read. `result`
    // is a live local backing the out-pointer.
    unsafe {
        let mut argc = 1usize;
        let mut argv: [NapiValue; 1] = [std::ptr::null_mut(); 1];
        (host().get_cb_info)(
            env,
            info,
            &mut argc,
            argv.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        let mut n = 0i32;
        (host().get_value_int32)(env, argv[0], &mut n);
        let mut result: NapiValue = std::ptr::null_mut();
        (host().create_bigint_int64)(env, n as i64, &mut result);
        result
    }
}

/// readBigInt64(bigint) -> Number -- reads the BigInt back as an i64, returns as Number
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback` with a live `env`
/// and its `info` frame. Argument 0 must be a BigInt for the read to
/// succeed; anything else returns null.
unsafe extern "C" fn read_bigint64(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `argc` is seeded with `argv`'s capacity of 1, bounding what
    // `napi_get_cb_info` writes. `val` and `lossless` are live locals, so both
    // out-pointers are valid and correctly sized for the `i64` and `bool` the
    // ABI writes through them, and `val` is forwarded to `napi_create_int64`
    // only after a zero status confirms the host actually wrote it.
    unsafe {
        let mut argc = 1usize;
        let mut argv: [NapiValue; 1] = [std::ptr::null_mut(); 1];
        (host().get_cb_info)(
            env,
            info,
            &mut argc,
            argv.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        let mut val = 0i64;
        let mut lossless = true;
        if (host().get_value_bigint_int64)(env, argv[0], &mut val, &mut lossless) != 0 {
            return std::ptr::null_mut();
        }
        let mut result: NapiValue = std::ptr::null_mut();
        (host().create_int64)(env, val, &mut result);
        result
    }
}

/// makeBuffer(n: number) -> Uint8Array -- creates an n-byte buffer (zeroed)
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback` with a live `env`
/// and its `info` frame. Reads argument 0 as a byte count and asks the host
/// to allocate a buffer of that size.
unsafe extern "C" fn make_buffer(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `argc` starts at `argv`'s capacity of 1, bounding
    // `napi_get_cb_info`. `n` is clamped with `max(0)` before the `as usize`
    // cast, so a negative JS number cannot wrap into a huge allocation
    // request, and a missing argument leaves it at 0. `data` is a live local
    // that the host writes the buffer address into; it is never dereferenced
    // here, since the bytes are owned by V8 and reached only through
    // `result`.
    unsafe {
        let mut argc = 1usize;
        let mut argv: [NapiValue; 1] = [std::ptr::null_mut(); 1];
        (host().get_cb_info)(
            env,
            info,
            &mut argc,
            argv.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        let mut n = 0i32;
        (host().get_value_int32)(env, argv[0], &mut n);
        let size = n.max(0) as usize;
        let mut data: *mut c_void = std::ptr::null_mut();
        let mut result: NapiValue = std::ptr::null_mut();
        (host().create_buffer)(env, size, &mut data, &mut result);
        result
    }
}

/// bufferLen(buf) -> Number -- returns the byte length of a buffer/TypedArray
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback` with a live `env`
/// and its `info` frame. Argument 0 must be a Buffer or TypedArray for the
/// info read to succeed.
unsafe extern "C" fn buffer_len(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `argc` is seeded with `argv`'s capacity of 1, bounding what
    // `napi_get_cb_info` writes. `data` and `length` are live locals backing
    // the two out-pointers; `data` is never dereferenced, and `length` is read
    // only after a zero status proves the host wrote it -- so a non-buffer
    // argument returns null instead of reporting a garbage length.
    unsafe {
        let mut argc = 1usize;
        let mut argv: [NapiValue; 1] = [std::ptr::null_mut(); 1];
        (host().get_cb_info)(
            env,
            info,
            &mut argc,
            argv.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        let mut data: *mut c_void = std::ptr::null_mut();
        let mut length = 0usize;
        if (host().get_buffer_info)(env, argv[0], &mut data, &mut length) != 0 {
            return std::ptr::null_mut();
        }
        let mut result: NapiValue = std::ptr::null_mut();
        (host().create_int64)(env, length as i64, &mut result);
        result
    }
}

/// refAfterDelete(value) -> number
/// Regression probe for the use-after-free on a deleted reference. Creates a
/// reference to `value`, deletes it, then asks the host to read the now-stale
/// handle and returns the host's STATUS code as a number.
///
/// A host that dereferences the freed entry answers 0 (napi_ok) and hands back
/// memory it already released; a host that validates the handle against its
/// own ref table answers 1 (napi_invalid_arg). The JS side asserts the latter,
/// so this test fails against the unfixed host rather than merely not crashing.
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback` with a live `env`
/// and its `info` frame.
unsafe extern "C" fn ref_after_delete(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `argc` is seeded with `argv`'s capacity of 1, bounding what
    // `napi_get_cb_info` writes. `ref_handle`, `stale` and `out` are live
    // locals backing the out-pointers, and a failed `napi_create_reference`
    // returns early rather than using an unwritten handle. `ref_handle` is
    // deliberately passed to `get_reference_value` AFTER `delete_reference`
    // -- that is the behaviour under test -- and the host is required to
    // reject it; the addon never dereferences it itself.
    unsafe {
        let mut argc = 1usize;
        let mut argv: [NapiValue; 1] = [std::ptr::null_mut(); 1];
        (host().get_cb_info)(
            env,
            info,
            &mut argc,
            argv.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );

        let mut out: NapiValue = std::ptr::null_mut();
        let mut ref_handle: *mut c_void = std::ptr::null_mut();
        if (host().create_reference)(env, argv[0], 1, &mut ref_handle) != 0 {
            (host().create_int32)(env, -1, &mut out);
            return out;
        }
        (host().delete_reference)(env, ref_handle);

        // `ref_handle` now names a freed entry. Ask the host to read it.
        let mut stale: NapiValue = std::ptr::null_mut();
        let status = (host().get_reference_value)(env, ref_handle, &mut stale);

        (host().create_int32)(env, status, &mut out);
        out
    }
}

// ------------------------------------------------- reference-lifecycle probes
//
// These exist to pin the HOST's napi_ref handling, so each returns a raw status
// or refcount as a number rather than throwing: the JS side asserts the exact
// value, which makes a host regression visible instead of merely making the
// test not crash.

/// Read one argument out of the callback frame.
///
/// # Safety
///
/// Only called from a `napi_callback` with a live `env` and its `info` frame.
unsafe fn one_arg(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `argc` is seeded with `argv`'s capacity of 1, bounding what
    // `napi_get_cb_info` writes into the local array.
    unsafe {
        let mut argc = 1usize;
        let mut argv: [NapiValue; 1] = [std::ptr::null_mut(); 1];
        (host().get_cb_info)(
            env,
            info,
            &mut argc,
            argv.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        argv[0]
    }
}

/// Wrap an i32 as a napi number, for the probes that report a status or count.
///
/// # Safety
///
/// Only called from a `napi_callback` with a live `env`.
unsafe fn num(env: NapiEnv, value: i32) -> NapiValue {
    // SAFETY: `out` is a live local backing the out-pointer.
    unsafe {
        let mut out: NapiValue = std::ptr::null_mut();
        (host().create_int32)(env, value, &mut out);
        out
    }
}

/// refRefCount(value) -> number: refcount reported by napi_reference_ref.
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback`.
unsafe extern "C" fn ref_ref_count(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `handle` and `count` are live locals backing the out-pointers,
    // and a failed create returns before the handle is used.
    unsafe {
        let value = one_arg(env, info);
        let mut handle: *mut c_void = std::ptr::null_mut();
        if (host().create_reference)(env, value, 1, &mut handle) != 0 {
            return num(env, -1);
        }
        let mut count = 0u32;
        let status = (host().reference_ref)(env, handle, &mut count);
        (host().delete_reference)(env, handle);
        if status != 0 {
            return num(env, -2);
        }
        num(env, count as i32)
    }
}

/// refUnrefCount(value) -> number: refcount reported by napi_reference_unref.
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback`.
unsafe extern "C" fn ref_unref_count(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `handle` and `count` are live locals backing the out-pointers,
    // and a failed create returns before the handle is used.
    unsafe {
        let value = one_arg(env, info);
        let mut handle: *mut c_void = std::ptr::null_mut();
        if (host().create_reference)(env, value, 2, &mut handle) != 0 {
            return num(env, -1);
        }
        let mut count = 0u32;
        let status = (host().reference_unref)(env, handle, &mut count);
        (host().delete_reference)(env, handle);
        if status != 0 {
            return num(env, -2);
        }
        num(env, count as i32)
    }
}

/// refRefAfterDelete(value) -> number: host status for napi_reference_ref on a
/// handle the addon already deleted.
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback`.
unsafe extern "C" fn ref_ref_after_delete(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `handle` is a live local; it is deliberately passed to
    // `reference_ref` AFTER `delete_reference` -- that is the behaviour under
    // test -- and the addon never dereferences it itself.
    unsafe {
        let value = one_arg(env, info);
        let mut handle: *mut c_void = std::ptr::null_mut();
        if (host().create_reference)(env, value, 1, &mut handle) != 0 {
            return num(env, -1);
        }
        (host().delete_reference)(env, handle);
        let mut count = 0u32;
        num(env, (host().reference_ref)(env, handle, &mut count))
    }
}

/// refUnrefAfterDelete(value) -> number: host status for napi_reference_unref
/// on a handle the addon already deleted.
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback`.
unsafe extern "C" fn ref_unref_after_delete(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: as in ref_ref_after_delete -- the stale handle is handed to the
    // host on purpose and never dereferenced by the addon.
    unsafe {
        let value = one_arg(env, info);
        let mut handle: *mut c_void = std::ptr::null_mut();
        if (host().create_reference)(env, value, 1, &mut handle) != 0 {
            return num(env, -1);
        }
        (host().delete_reference)(env, handle);
        let mut count = 0u32;
        num(env, (host().reference_unref)(env, handle, &mut count))
    }
}

/// foreignHandleStatus() -> number: host status for napi_get_reference_value on
/// a non-null pointer this env never dispensed.
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback`.
unsafe extern "C" fn foreign_handle_status(env: NapiEnv, _info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `bogus` is the address of a live stack local -- a valid readable
    // address that is nonetheless NOT a napi_ref this env handed out. The host
    // must reject it on identity and never dereference it as an entry.
    unsafe {
        let mut bogus = 0u64;
        let handle = (&raw mut bogus).cast::<c_void>();
        let mut out_value: NapiValue = std::ptr::null_mut();
        num(
            env,
            (host().get_reference_value)(env, handle, &mut out_value),
        )
    }
}

/// createRefNullOut(value) -> number: host status when the out-pointer is null.
/// The host must reject the call without leaving a reference behind.
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback`.
unsafe extern "C" fn create_ref_null_out(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: the null out-pointer IS the input under test; the host is
    // required to null-check it rather than write through it.
    unsafe {
        let value = one_arg(env, info);
        num(
            env,
            (host().create_reference)(env, value, 1, std::ptr::null_mut()),
        )
    }
}

/// makeRefHandle(value) -> BigInt: create a reference and hand its raw handle
/// back to JS as an exact 64-bit integer, so a test can feed one env's handle
/// to a DIFFERENT env.
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback`.
unsafe extern "C" fn make_ref_handle(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `handle` and `out` are live locals backing the out-pointers. The
    // reference is deliberately NOT deleted -- the test reads it afterwards.
    unsafe {
        let value = one_arg(env, info);
        let mut handle: *mut c_void = std::ptr::null_mut();
        if (host().create_reference)(env, value, 1, &mut handle) != 0 {
            return num(env, -1);
        }
        let mut out: NapiValue = std::ptr::null_mut();
        (host().create_bigint_int64)(env, handle as i64, &mut out);
        out
    }
}

/// readRefHandle(bigint) -> number: host status for napi_get_reference_value on
/// a handle supplied by JS. The owning env answers napi_ok; another env must
/// refuse, because the handle is live but belongs to a different ref table.
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback`.
unsafe extern "C" fn read_ref_handle(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `bits` and `lossless` are live locals backing the out-pointers.
    // The reconstructed pointer goes straight to the host, which validates it
    // against its own ref table; the addon never dereferences it.
    unsafe {
        let value = one_arg(env, info);
        let mut bits = 0i64;
        let mut lossless = false;
        if (host().get_value_bigint_int64)(env, value, &mut bits, &mut lossless) != 0 {
            return num(env, -1);
        }
        let handle = bits as *mut c_void;
        let mut out_value: NapiValue = std::ptr::null_mut();
        num(
            env,
            (host().get_reference_value)(env, handle, &mut out_value),
        )
    }
}

/// readRefInt(bigint) -> number: resolve a JS-supplied handle and report the
/// int32 the referenced value holds, rather than just the host's status.
///
/// The status-only `readRefHandle` cannot tell one live reference from
/// another: a host that hands two `napi_create_reference` calls the SAME slot
/// still answers napi_ok for both. Reading the VALUE back is what makes a
/// clobbered slot observable.
///
/// * `>= 0`  -- the referenced int32.
/// * `-1`    -- the argument was not a readable BigInt.
/// * `-2`    -- the host refused the handle (any non-ok status).
/// * `-3`    -- the handle resolved, but the value was not an int32.
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback`.
unsafe extern "C" fn read_ref_int(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `bits`, `lossless`, `out_value` and `answer` are live locals
    // backing the out-pointers. The reconstructed handle goes straight to the
    // host, which validates it against its own ref table; the addon never
    // dereferences it, and `out_value` is only read once the host reported
    // napi_ok, i.e. once it wrote through that pointer.
    unsafe {
        let value = one_arg(env, info);
        let mut bits = 0i64;
        let mut lossless = false;
        if (host().get_value_bigint_int64)(env, value, &mut bits, &mut lossless) != 0 {
            return num(env, -1);
        }
        let mut out_value: NapiValue = std::ptr::null_mut();
        if (host().get_reference_value)(env, bits as *mut c_void, &mut out_value) != 0 {
            return num(env, -2);
        }
        let mut answer = 0i32;
        if (host().get_value_int32)(env, out_value, &mut answer) != 0 {
            return num(env, -3);
        }
        num(env, answer)
    }
}

/// deleteRefHandle(bigint) -> number: host status for napi_delete_reference on
/// a handle supplied by JS. The companion to `makeRefHandle`, so a test can
/// drive create/delete pairs from JS and watch what the host does with the
/// slots in between.
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback`.
unsafe extern "C" fn delete_ref_handle(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `bits` and `lossless` are live locals backing the out-pointers.
    // The reconstructed handle goes straight to the host, which validates it
    // against its own ref table; the addon never dereferences it.
    unsafe {
        let value = one_arg(env, info);
        let mut bits = 0i64;
        let mut lossless = false;
        if (host().get_value_bigint_int64)(env, value, &mut bits, &mut lossless) != 0 {
            return num(env, -1);
        }
        num(env, (host().delete_reference)(env, bits as *mut c_void))
    }
}

/// deleteRefTwice(value) -> number: host status for the SECOND
/// napi_delete_reference on the same handle. A host that does not validate
/// answers 0 and hides the addon's double-delete.
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback`.
unsafe extern "C" fn delete_ref_twice(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `handle` is a live local backing the out-pointer. The second
    // delete is the behaviour under test; the addon never dereferences it.
    unsafe {
        let value = one_arg(env, info);
        let mut handle: *mut c_void = std::ptr::null_mut();
        if (host().create_reference)(env, value, 1, &mut handle) != 0 {
            return num(env, -1);
        }
        if (host().delete_reference)(env, handle) != 0 {
            return num(env, -2);
        }
        num(env, (host().delete_reference)(env, handle))
    }
}

/// abaStaleReadStatus() -> number: the ABA probe.
///
/// Creates a reference to the number 111, DELETES it, then creates a second
/// reference to the number 222. That second create is what recycles whatever
/// storage the delete released. It then hands the host the FIRST (stale)
/// handle and reports what happened:
///
/// * `1`   -- the host refused it (`napi_invalid_arg`). Correct.
/// * `222` -- the host accepted it and resolved it to the SECOND reference.
///   That is the ABA bug: a live, correctly-typed entry, but not the one the
///   handle named.
/// * `111` -- the host accepted it and answered with the original value, i.e.
///   it read through storage it had already released.
/// * negative -- a setup step failed; see the arm that produced it.
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback`.
unsafe extern "C" fn aba_stale_read_status(env: NapiEnv, _info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: every out-pointer below backs a live local. `first` is handed to
    // `get_reference_value` AFTER its `delete_reference` on purpose -- that is
    // the behaviour under test -- and the addon never dereferences it itself.
    unsafe {
        let mut v1: NapiValue = std::ptr::null_mut();
        let mut v2: NapiValue = std::ptr::null_mut();
        (host().create_int32)(env, 111, &mut v1);
        (host().create_int32)(env, 222, &mut v2);

        let mut first: *mut c_void = std::ptr::null_mut();
        if (host().create_reference)(env, v1, 1, &mut first) != 0 {
            return num(env, -1);
        }
        if (host().delete_reference)(env, first) != 0 {
            return num(env, -2);
        }
        // Recycles the slot -- and, under a pointer-identity scheme, very
        // likely the exact heap block -- that the delete above released.
        let mut second: *mut c_void = std::ptr::null_mut();
        if (host().create_reference)(env, v2, 1, &mut second) != 0 {
            return num(env, -3);
        }

        let mut stale: NapiValue = std::ptr::null_mut();
        if (host().get_reference_value)(env, first, &mut stale) != 0 {
            return num(env, 1);
        }
        let mut observed = 0i32;
        if (host().get_value_int32)(env, stale, &mut observed) != 0 {
            return num(env, -4);
        }
        num(env, observed)
    }
}

/// abaHandleRecycled() -> number: 1 when the handle a create hands back right
/// after a delete is bit-identical to the deleted one, 0 otherwise.
///
/// Diagnostic, and the reason `abaStaleReadStatus` is load-bearing rather than
/// merely green: under a pointer-identity scheme a recycled ADDRESS is exactly
/// what lets a stale handle resolve to the wrong entry, so this reports whether
/// the allocator really did recycle on this run.
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback`.
unsafe extern "C" fn aba_handle_recycled(env: NapiEnv, _info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: every out-pointer below backs a live local; the two handles are
    // only ever COMPARED here, never dereferenced by the addon.
    unsafe {
        let mut value: NapiValue = std::ptr::null_mut();
        (host().create_int32)(env, 7, &mut value);

        let mut first: *mut c_void = std::ptr::null_mut();
        if (host().create_reference)(env, value, 1, &mut first) != 0 {
            return num(env, -1);
        }
        if (host().delete_reference)(env, first) != 0 {
            return num(env, -2);
        }
        let mut second: *mut c_void = std::ptr::null_mut();
        if (host().create_reference)(env, value, 1, &mut second) != 0 {
            return num(env, -3);
        }
        num(env, i32::from(first == second))
    }
}

/// lastErrorMessage(value) -> string: provoke a known failure (reading a
/// deleted reference), then report what napi_get_last_error_info says about it.
/// Returns "" when the host reports no message at all.
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback`.
unsafe extern "C" fn last_error_message(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `handle` and `info_ptr` are live locals backing the out-pointers.
    // `info_ptr` is only dereferenced after the host reported success AND it
    // came back non-null; `error_message` is read only when non-null, and the
    // host owns that string for at least the life of this env.
    unsafe {
        let value = one_arg(env, info);
        let mut handle: *mut c_void = std::ptr::null_mut();
        if (host().create_reference)(env, value, 1, &mut handle) != 0 {
            return string(env, "create failed");
        }
        (host().delete_reference)(env, handle);

        // Known-bad call: the host must refuse it.
        let mut out_value: NapiValue = std::ptr::null_mut();
        if (host().get_reference_value)(env, handle, &mut out_value) == 0 {
            return string(env, "expected the stale read to fail");
        }

        // The documented way to ask WHY.
        let mut info_ptr: *const NapiExtendedErrorInfo = std::ptr::null();
        if (host().get_last_error_info)(env, &mut info_ptr) != 0 || info_ptr.is_null() {
            return string(env, "no info");
        }
        let code = (*info_ptr).error_code;
        let msg = (*info_ptr).error_message;
        if msg.is_null() {
            return string(env, &format!("code={code} message=<null>"));
        }
        let text = std::ffi::CStr::from_ptr(msg).to_string_lossy().into_owned();
        string(env, &format!("code={code} message={text}"))
    }
}

/// Wrap a Rust string as a napi string.
///
/// # Safety
///
/// Only called from a `napi_callback` with a live `env`.
unsafe fn string(env: NapiEnv, value: &str) -> NapiValue {
    // SAFETY: `out` is a live local backing the out-pointer; the text pointer
    // and length describe a buffer that outlives the call.
    unsafe {
        let mut out: NapiValue = std::ptr::null_mut();
        (host().create_string_utf8)(env, value.as_ptr().cast::<c_char>(), value.len(), &mut out);
        out
    }
}

// =============================================================== sized names

/// The do-nothing constructor of the classes `sizedNames` defines: only
/// their `name` is read.
extern "C" fn sized_names_noop(_env: NapiEnv, _info: NapiCallbackInfo) -> NapiValue {
    std::ptr::null_mut()
}

/// Lengths past `i32::MAX` (other than NAPI_AUTO_LENGTH) are refused.
const OVERLONG_NAME: usize = i32::MAX as usize + 1;

/// `sizedNames()` -> an object whose members exercise the name length of
/// `napi_create_function` and `napi_define_class`. Every buffer carries text
/// past the given length and a NUL after that, so a host that ignored the
/// length and read to the terminator would produce the longer name:
///
/// * `fn`       -- `"sizedFnTAIL"`, length 7: named `sizedFn`
/// * `embedded` -- `"ab\0cd"`, length 5: the NUL is part of the name
/// * `invalid`  -- `"x\xFFy"`, length 3: the stray byte becomes U+FFFD
/// * `empty`    -- `"zero"`, length 0: named the empty string
/// * `auto`     -- `"autoName"`, NAPI_AUTO_LENGTH: read to the NUL
/// * `cls`      -- class `"SizedClassTAIL"`, length 10: named `SizedClass`
/// * `fnOverlong`, `clsOverlong` -- the status for a length past
///   `i32::MAX` (node answers `napi_invalid_arg`, 1)
/// * `nullNameOverlong` -- the status of `napi_create_function` with a null
///   name and that same length (node never reads the length then: `napi_ok`)
///
/// The host calls it as a `napi_callback`, with a live `env`.
extern "C" fn sized_names(env: NapiEnv, _info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `HOST` is resolved before any export can be called and `env`
    // is the live env of this call. Every name pointer is a static byte
    // string valid for at least the length passed with it (and
    // NUL-terminated, so an AUTO read stays inside it too); the overlong
    // lengths are refused by the host before it reads anything, and the
    // pointer passed with them is still a valid NUL-terminated string (or
    // null, which the host never reads).
    // `result`, `function` and `value` are live locals backing the
    // out-pointers, and every `napi_value` stays within this call.
    unsafe {
        let host = host();
        let mut result: NapiValue = std::ptr::null_mut();
        (host.create_object)(env, &mut result);
        let functions: [(&std::ffi::CStr, &[u8], usize); 5] = [
            (c"fn", b"sizedFnTAIL\0", 7),
            (c"embedded", b"ab\0cd\0", 5),
            (c"invalid", b"x\xFFy\0", 3),
            (c"empty", b"zero\0", 0),
            (c"auto", b"autoName\0", usize::MAX),
        ];
        for (key, name, len) in functions {
            let mut function: NapiValue = std::ptr::null_mut();
            (host.create_function)(
                env,
                name.as_ptr().cast::<c_char>(),
                len,
                Some(sized_names_noop),
                std::ptr::null_mut(),
                &mut function,
            );
            (host.set_named_property)(env, result, key.as_ptr(), function);
        }
        let mut class: NapiValue = std::ptr::null_mut();
        (host.define_class)(
            env,
            c"SizedClassTAIL".as_ptr(),
            10,
            Some(sized_names_noop),
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            &mut class,
        );
        (host.set_named_property)(env, result, c"cls".as_ptr(), class);

        let mut function: NapiValue = std::ptr::null_mut();
        let fn_status = (host.create_function)(
            env,
            c"overlong".as_ptr(),
            OVERLONG_NAME,
            Some(sized_names_noop),
            std::ptr::null_mut(),
            &mut function,
        );
        let cls_status = (host.define_class)(
            env,
            c"Overlong".as_ptr(),
            OVERLONG_NAME,
            Some(sized_names_noop),
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            &mut class,
        );
        // A null name is never read, so its length is not checked either.
        let null_name_status = (host.create_function)(
            env,
            std::ptr::null(),
            OVERLONG_NAME,
            Some(sized_names_noop),
            std::ptr::null_mut(),
            &mut function,
        );
        for (key, status) in [
            (c"fnOverlong", fn_status),
            (c"clsOverlong", cls_status),
            (c"nullNameOverlong", null_name_status),
        ] {
            let mut value: NapiValue = std::ptr::null_mut();
            (host.create_int32)(env, status, &mut value);
            (host.set_named_property)(env, result, key.as_ptr(), value);
        }
        result
    }
}

/// Set `object[key]` to an int32.
///
/// # Safety
///
/// Only called from a `napi_callback` with a live `env`; `object` is a
/// `napi_value` of that call.
unsafe fn set_int(env: NapiEnv, object: NapiValue, key: &std::ffi::CStr, number: i32) {
    // SAFETY: `value` is a live local backing the out-pointer; `key` is a
    // NUL-terminated C string; `env` and `object` are live per the contract.
    unsafe {
        let mut value: NapiValue = std::ptr::null_mut();
        (host().create_int32)(env, number, &mut value);
        (host().set_named_property)(env, object, key.as_ptr(), value);
    }
}

/// One of the three string constructors, with the pointee type of its text
/// pointer erased so `string_rules` can drive all three through one table.
/// The three host signatures differ only in that pointee (`c_char` or
/// `u16`), and raw pointers to sized types are ABI-compatible, so calling
/// any of them through this type is calling it through its own ABI.
type StringCtor = unsafe extern "C" fn(NapiEnv, *const c_void, usize, *mut NapiValue) -> NapiStatus;

/// `stringRules()` -> `{ utf8, latin1, utf16, cls }`: the statuses the
/// string constructors and `napi_define_class` answer at the edges of their
/// text arguments. For each encoding:
///
/// * `nullZero`  -- a null pointer with length 0 (plus `nullZeroValue`,
///   the string made, when the call succeeds)
/// * `nullTwo`   -- a null pointer with length 2
/// * `nullAuto`  -- a null pointer with NAPI_AUTO_LENGTH
/// * `overlong`  -- a valid pointer with a length past `i32::MAX`; refused
///   before the text is read
/// * `nullOut`   -- a valid text and a null result pointer
/// * `two`       -- `"abTAIL"` with length 2 (plus `twoValue`)
///
/// `cls` holds `napi_define_class` with a null name (`nullAuto`,
/// `nullZero`) and with a property count of 1 but no descriptor array
/// (`nullProps`), and `napi_define_properties` given that same count and
/// null array (`definePropertiesNullProps`). It also holds the statuses of
/// `napi_define_class` with a null constructor (`nullCtor`) and of
/// `napi_create_function` with a null callback (`createFunctionNullCb`),
/// and of `napi_define_properties` with a property count and no descriptor
/// array on the number 7 (`numberNullProps`) and on undefined
/// (`undefinedNullProps`), each with `<name>Error`, the exception the call
/// left pending, when there was one.
///
/// The host calls it as a `napi_callback`, with a live `env`.
extern "C" fn string_rules(env: NapiEnv, _info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `HOST` is resolved before any export can be called and `env`
    // is the live env of this call. Each text pointer is either null or a
    // static NUL-terminated buffer of its encoding that is longer than the
    // explicit lengths passed with it; the overlong length and the null
    // pointers with a non-zero length are refused before anything is read
    // (the behaviour under test -- node and oam both check first). Each
    // string constructor is called through `StringCtor`, the same ABI as its
    // own signature (see that type), with a text of its own encoding. `result`,
    // `entry`, `value` and `class` are live locals backing the out-pointers.
    unsafe {
        let host = host();
        let mut result: NapiValue = std::ptr::null_mut();
        (host.create_object)(env, &mut result);
        static UTF16_TEXT: [u16; 7] = [
            b'a' as u16,
            b'b' as u16,
            b'T' as u16,
            b'A' as u16,
            b'I' as u16,
            b'L' as u16,
            0,
        ];
        // See `StringCtor` for why the erased signature is the same ABI.
        let erase = |f: *const ()| std::mem::transmute::<*const (), StringCtor>(f);
        let encodings: [(&std::ffi::CStr, StringCtor, *const c_void); 3] = [
            (
                c"utf8",
                erase(host.create_string_utf8 as *const ()),
                c"abTAIL".as_ptr().cast(),
            ),
            (
                c"latin1",
                erase(host.create_string_latin1 as *const ()),
                c"abTAIL".as_ptr().cast(),
            ),
            (
                c"utf16",
                erase(host.create_string_utf16 as *const ()),
                UTF16_TEXT.as_ptr().cast(),
            ),
        ];
        for (key, ctor, text) in encodings {
            let mut entry: NapiValue = std::ptr::null_mut();
            (host.create_object)(env, &mut entry);
            let cases: [(&std::ffi::CStr, *const c_void, usize, bool); 6] = [
                (c"nullZero", std::ptr::null(), 0, true),
                (c"nullTwo", std::ptr::null(), 2, true),
                (c"nullAuto", std::ptr::null(), usize::MAX, true),
                (c"overlong", text, OVERLONG_NAME, true),
                (c"nullOut", text, 2, false),
                (c"two", text, 2, true),
            ];
            for (name, ptr, len, with_out) in cases {
                let mut value: NapiValue = std::ptr::null_mut();
                let out: *mut NapiValue = if with_out {
                    &mut value
                } else {
                    std::ptr::null_mut()
                };
                let status = ctor(env, ptr, len, out);
                set_int(env, entry, name, status);
                if status == 0 && !value.is_null() {
                    let label = format!("{}Value\0", name.to_str().unwrap_or("?"));
                    (host.set_named_property)(env, entry, label.as_ptr().cast(), value);
                }
            }
            (host.set_named_property)(env, result, key.as_ptr(), entry);
        }

        let mut cls: NapiValue = std::ptr::null_mut();
        (host.create_object)(env, &mut cls);
        for (name, len) in [(c"nullAuto", usize::MAX), (c"nullZero", 0)] {
            let mut class: NapiValue = std::ptr::null_mut();
            let status = (host.define_class)(
                env,
                std::ptr::null(),
                len,
                Some(sized_names_noop),
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
                &mut class,
            );
            set_int(env, cls, name, status);
        }
        // A property count with no descriptor array, from both definers.
        let mut class: NapiValue = std::ptr::null_mut();
        let status = (host.define_class)(
            env,
            c"NoProps".as_ptr(),
            usize::MAX,
            Some(sized_names_noop),
            std::ptr::null_mut(),
            1,
            std::ptr::null(),
            &mut class,
        );
        set_int(env, cls, c"nullProps", status);
        let status = (host.define_properties)(env, cls, 1, std::ptr::null());
        set_int(env, cls, c"definePropertiesNullProps", status);
        // A null callback, from both function makers.
        let status = (host.define_class)(
            env,
            c"NullCtor".as_ptr(),
            usize::MAX,
            None,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            &mut class,
        );
        set_int(env, cls, c"nullCtor", status);
        let mut function: NapiValue = std::ptr::null_mut();
        let status = (host.create_function)(
            env,
            c"nullCb".as_ptr(),
            usize::MAX,
            None,
            std::ptr::null_mut(),
            &mut function,
        );
        set_int(env, cls, c"createFunctionNullCb", status);
        // napi_define_properties with a property count and no descriptor
        // array, on targets that are not objects: the array is checked
        // first.
        let mut number: NapiValue = std::ptr::null_mut();
        (host.create_int32)(env, 7, &mut number);
        let mut undefined: NapiValue = std::ptr::null_mut();
        (host.get_undefined)(env, &mut undefined);
        for (name, target) in [
            (c"numberNullProps", number),
            (c"undefinedNullProps", undefined),
        ] {
            let status = (host.define_properties)(env, target, 1, std::ptr::null());
            // Clear any exception the call left pending, so the calls below
            // run clean, and keep it as `<name>Error` (undefined when there
            // was none).
            let mut error: NapiValue = std::ptr::null_mut();
            (host.get_and_clear_last_exception)(env, &mut error);
            set_int(env, cls, name, status);
            if !error.is_null() {
                let label = format!("{}Error\0", name.to_str().unwrap_or("?"));
                (host.set_named_property)(env, cls, label.as_ptr().cast(), error);
            }
        }
        (host.set_named_property)(env, result, c"cls".as_ptr(), cls);
        result
    }
}

/// `descriptorNames()` -> `{ obj, cls, symMethod, symAcc }`: functions
/// defined from property descriptors, so the caller can read the `name` of
/// each. `obj` gets them through `napi_define_properties`, `cls` (class
/// `Named`) through `napi_define_class`; both carry:
///
/// * `method`     -- a method named by `utf8name`
/// * `valueNamed` -- a method named by a string `name` value
/// * `[symMethod]` -- a method named by a symbol (description `"symMethod"`)
/// * `acc`        -- a getter and setter named by `utf8name`
/// * `[symAcc]`   -- a getter named by a symbol (description `"symAcc"`)
///
/// On `cls` they are on the prototype, plus a static method `sm`.
///
/// The host calls it as a `napi_callback`, with a live `env`.
extern "C" fn descriptor_names(env: NapiEnv, _info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `HOST` is resolved before any export can be called and `env`
    // is the live env of this call. Every descriptor name is a static
    // NUL-terminated literal or a `napi_value` of this call, every callback
    // has the `napi_callback` signature, and the descriptor arrays outlive
    // the calls that read them. All out-pointers are live locals.
    unsafe {
        let host = host();
        let symbol = |description: &str| {
            let mut symbol: NapiValue = std::ptr::null_mut();
            (host.create_symbol)(env, string(env, description), &mut symbol);
            symbol
        };
        let sym_method = symbol("symMethod");
        let sym_acc = symbol("symAcc");
        let value_named = string(env, "valueNamed");
        let noop: NapiCallback = sized_names_noop;
        let descriptor = |utf8name: *const c_char, name: NapiValue| NapiPropertyDescriptor {
            utf8name,
            name,
            method: None,
            getter: None,
            setter: None,
            value: std::ptr::null_mut(),
            attributes: 0,
            data: std::ptr::null_mut(),
        };
        let method = |utf8name, name| NapiPropertyDescriptor {
            method: Some(noop),
            ..descriptor(utf8name, name)
        };
        let accessor = |utf8name, name, with_setter: bool| NapiPropertyDescriptor {
            getter: Some(noop),
            setter: with_setter.then_some(noop),
            ..descriptor(utf8name, name)
        };
        let null = std::ptr::null_mut();
        let common = || {
            [
                method(c"method".as_ptr(), null),
                method(std::ptr::null(), value_named),
                method(std::ptr::null(), sym_method),
                accessor(c"acc".as_ptr(), null, true),
                accessor(std::ptr::null(), sym_acc, false),
            ]
        };

        let mut result: NapiValue = std::ptr::null_mut();
        (host.create_object)(env, &mut result);

        let mut obj: NapiValue = std::ptr::null_mut();
        (host.create_object)(env, &mut obj);
        let props = common();
        (host.define_properties)(env, obj, props.len(), props.as_ptr());
        (host.set_named_property)(env, result, c"obj".as_ptr(), obj);

        let [a, b, c, d, e] = common();
        let class_props = [
            a,
            b,
            c,
            d,
            e,
            NapiPropertyDescriptor {
                attributes: 0x400, // napi_static
                ..method(c"sm".as_ptr(), null)
            },
        ];
        let mut class: NapiValue = std::ptr::null_mut();
        (host.define_class)(
            env,
            c"Named".as_ptr(),
            usize::MAX,
            Some(sized_names_noop),
            std::ptr::null_mut(),
            class_props.len(),
            class_props.as_ptr(),
            &mut class,
        );
        (host.set_named_property)(env, result, c"cls".as_ptr(), class);
        (host.set_named_property)(env, result, c"symMethod".as_ptr(), sym_method);
        (host.set_named_property)(env, result, c"symAcc".as_ptr(), sym_acc);
        result
    }
}

// ============================================================ teardown probe

/// Where the teardown probe writes, once armed. Unset in every run that does
/// not name a log file, and then nothing below does anything.
static TEARDOWN_LOG: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// The `napi_env` of the most recent armed registration: what a library
/// destructor in a real addon would have saved in a static.
static TEARDOWN_ENV: std::sync::atomic::AtomicPtr<c_void> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// Set once this image has registered [`teardown_at_unload`], so an image
/// that stays mapped across two loads registers it once.
static TEARDOWN_AT_UNLOAD: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

unsafe extern "C" {
    /// C's `atexit`. Called from a shared library it registers with THAT
    /// library: the handler runs when the library is unloaded, or at process
    /// exit when it is still loaded then -- the mechanism behind a C++
    /// static destructor. Measured with this addon on Windows (MSVC, static
    /// CRT) and on Linux (glibc 2.39): the handler runs inside the unload of
    /// a worker's last handle, and at process exit otherwise. Not measured
    /// on macOS, where the e2e tests only require that it runs once.
    safe fn atexit(handler: extern "C" fn()) -> std::ffi::c_int;
}

/// Append one line to the teardown log. A no-op unless the probe is armed;
/// a write that fails is dropped (the e2e then fails on the missing line).
fn teardown_note(line: &str) {
    use std::io::Write;
    let Some(path) = TEARDOWN_LOG.get() else {
        return;
    };
    if let Ok(mut log) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
    {
        let _ = writeln!(log, "{line}");
    }
}

/// The three cleanup hooks the probe registers; `arg` only names them.
/// With `OAM_NAPI_TEARDOWN_CALL` set as well, hook 3 then calls
/// `globalThis.__oamTeardownCall()` -- addon code that runs JS from a
/// cleanup hook -- and logs `call:status=N`.
extern "C" fn teardown_hook(arg: *mut c_void) {
    teardown_note(&format!("hook:{}", arg.addr()));
    if arg.addr() == 3 && std::env::var_os("OAM_NAPI_TEARDOWN_CALL").is_some() {
        let env = TEARDOWN_ENV.load(std::sync::atomic::Ordering::Relaxed);
        let mut global: NapiValue = std::ptr::null_mut();
        let mut function: NapiValue = std::ptr::null_mut();
        let mut result: NapiValue = std::ptr::null_mut();
        // SAFETY: `HOST` was resolved before any hook was registered. `env`
        // is the env this image registered its hooks on, which the host runs
        // them with, so it is the env of this call. The name is a static,
        // NUL-terminated literal; `global`, `function` and `result` are live
        // locals backing the out-pointers, and a `napi_value` the host wrote
        // is passed back only within this call. No arguments are passed (a
        // null argv with argc 0).
        let status = unsafe {
            let host = host();
            let mut status = (host.get_global)(env, &mut global);
            if status == 0 {
                status = (host.get_named_property)(
                    env,
                    global,
                    c"__oamTeardownCall".as_ptr(),
                    &mut function,
                );
            }
            if status == 0 {
                status =
                    (host.call_function)(env, global, function, 0, std::ptr::null(), &mut result);
            }
            status
        };
        teardown_note(&format!("call:status={status}"));
    }
}

/// The library destructor: runs when this image is unloaded, or at process
/// exit. It does what a careless static destructor does -- calls N-API with
/// the env it saved -- and records the status the host answered with. A host
/// that had already freed that env would be reading freed memory here.
extern "C" fn teardown_at_unload() {
    let env = TEARDOWN_ENV.load(std::sync::atomic::Ordering::Relaxed);
    let mut object: NapiValue = std::ptr::null_mut();
    // SAFETY: `HOST` was resolved before this handler was registered, and the
    // host image outlives this one. `env` is the pointer the host handed the
    // registration; whether it may still be USED this late is the host
    // behaviour under test -- the host must keep it valid to pass for as
    // long as this library can run. `object` is a live local out-pointer.
    let status = unsafe { (host().create_object)(env, &mut object) };
    teardown_note(&format!("unload:status={status}"));
}

/// Arm the teardown probe if `OAM_NAPI_TEARDOWN_LOG` names a file: save
/// `env` and register [`teardown_at_unload`] with the C runtime. Returns
/// whether the probe is armed; `napi_register_module_v1` then registers
/// cleanup hooks 1, 2 and 3 in that order and removes 2. The log shows the
/// order the host tears an addon down in: the hooks (3 then 1 -- reverse
/// registration, 2 never), `finalize` for each wrap still alive (see
/// `counter_finalize`), and `unload:status=N`.
fn teardown_arm(env: NapiEnv) -> bool {
    let Some(path) = std::env::var_os("OAM_NAPI_TEARDOWN_LOG") else {
        return false;
    };
    let _ = TEARDOWN_LOG.set(path.into());
    TEARDOWN_ENV.store(env, std::sync::atomic::Ordering::Relaxed);
    if !TEARDOWN_AT_UNLOAD.swap(true, std::sync::atomic::Ordering::Relaxed) {
        atexit(teardown_at_unload);
    }
    true
}

// ======================================================= module registration

/// # Safety
///
/// N-API entry point: the host runtime must pass a valid `env` and `exports`
/// per the N-API ABI (Node 18+ shape). Called exactly once per addon load
/// by `dlopen`/`LoadLibrary` resolution; concurrent calls are a host bug.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_register_module_v1(env: NapiEnv, exports: NapiValue) -> NapiValue {
    // SAFETY: The host calls this exactly once per addon load with a live
    // `env` and the module's `exports` object, both valid for this call.
    // `resolve_host` runs first and short-circuits by returning the untouched
    // `exports` if any `napi_*` symbol is missing, so `host()` is only reached
    // once the whole table is installed. Every `name.as_ptr()` is a `c"..."`
    // literal -- static and NUL terminated -- paired with `usize::MAX`, the
    // NAPI_AUTO_LENGTH sentinel, and every callback in the two binding tables
    // is an `unsafe extern "C" fn` with the exact `napi_callback` signature
    // the ABI expects. `function` and `answer` are live locals backing their
    // out-pointers.
    unsafe {
        let Some(resolved) = resolve_host() else {
            return exports;
        };
        let _ = HOST.set(resolved);

        // The teardown probe's cleanup hooks: `teardown_hook` never
        // dereferences its argument, which only names the hook.
        if teardown_arm(env) {
            for name in 1..=3usize {
                (host().add_env_cleanup_hook)(
                    env,
                    teardown_hook,
                    std::ptr::without_provenance_mut(name),
                );
            }
            (host().remove_env_cleanup_hook)(
                env,
                teardown_hook,
                std::ptr::without_provenance_mut(2),
            );
        }

        let alpha_bindings: [(&std::ffi::CStr, NapiCallback); 5] = [
            (c"add", add),
            (c"greet", greet),
            (c"concat", concat),
            (c"boom", boom),
            (c"makeInt64", make_int64),
        ];
        for (name, callback) in alpha_bindings {
            let mut function: NapiValue = std::ptr::null_mut();
            (host().create_function)(
                env,
                name.as_ptr(),
                usize::MAX,
                Some(callback),
                std::ptr::null_mut(),
                &mut function,
            );
            (host().set_named_property)(env, exports, name.as_ptr(), function);
        }

        let beta_bindings: [(&std::ffi::CStr, NapiCallback); 26] = [
            (c"sizedNames", sized_names),
            (c"stringRules", string_rules),
            (c"descriptorNames", descriptor_names),
            (c"abaStaleReadStatus", aba_stale_read_status),
            (c"abaHandleRecycled", aba_handle_recycled),
            (c"deleteRefHandle", delete_ref_handle),
            (c"deleteRefTwice", delete_ref_twice),
            (c"lastErrorMessage", last_error_message),
            (c"refAfterDelete", ref_after_delete),
            (c"refRefCount", ref_ref_count),
            (c"refUnrefCount", ref_unref_count),
            (c"refRefAfterDelete", ref_ref_after_delete),
            (c"refUnrefAfterDelete", ref_unref_after_delete),
            (c"foreignHandleStatus", foreign_handle_status),
            (c"createRefNullOut", create_ref_null_out),
            (c"makeRefHandle", make_ref_handle),
            (c"readRefHandle", read_ref_handle),
            (c"readRefInt", read_ref_int),
            (c"wrapCounter", wrap_counter),
            (c"wrapForeign", wrap_foreign),
            (c"counterGet", counter_get),
            (c"counterInc", counter_inc),
            (c"makeBigInt64", make_bigint64),
            (c"readBigInt64", read_bigint64),
            (c"makeBuffer", make_buffer),
            (c"bufferLen", buffer_len),
        ];
        for (name, callback) in beta_bindings {
            let mut function: NapiValue = std::ptr::null_mut();
            (host().create_function)(
                env,
                name.as_ptr(),
                usize::MAX,
                Some(callback),
                std::ptr::null_mut(),
                &mut function,
            );
            (host().set_named_property)(env, exports, name.as_ptr(), function);
        }

        let mut answer: NapiValue = std::ptr::null_mut();
        (host().create_int32)(env, 42, &mut answer);
        (host().set_named_property)(env, exports, c"answer".as_ptr(), answer);

        exports
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four tests below share the process-global `COUNTERS` table, whose
    /// keys are heap addresses. Run concurrently, one test's `counter_release`
    /// frees an address the allocator hands straight to a sibling's
    /// `counter_register`, and a later lookup finds a foreign counter
    /// (observed on the mac release leg as `counter_value` returning
    /// `Some(1)` after release). Holding this lock keeps each
    /// register/increment/release/assert sequence exclusive. Serializing these
    /// four tests is the deliberate trade against cargo's default parallel
    /// test threads: the exclusivity matters more than the wall-clock time.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Recover a poisoned `TEST_LOCK` rather than unwind: a sibling that
    /// panicked already failed the run, and masking its real assertion
    /// behind a lock-poison message helps nobody.
    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn counter_registry_roundtrip() {
        let _guard = test_guard();
        let native = counter_register();
        assert_eq!(counter_value(native), Some(0));
        assert!(counter_increment(native));
        assert!(counter_increment(native));
        assert_eq!(counter_value(native), Some(2));
        assert!(counter_release(native));
        // Finalized: no read, no write, and a second finalize is a no-op.
        assert_eq!(counter_value(native), None);
        assert!(!counter_increment(native));
        assert!(!counter_release(native));
    }

    #[test]
    fn foreign_pointer_is_never_treated_as_a_counter() {
        let _guard = test_guard();
        // A pointer this addon never registered -- e.g. another addon's wrap --
        // must not be read or written as an i64.
        let mut foreign = [0u8; 3];
        let native = foreign.as_mut_ptr().cast::<c_void>();
        assert_eq!(counter_value(native), None);
        assert!(!counter_increment(native));
        assert!(!counter_release(native));
        assert_eq!(foreign, [0u8; 3]);
        assert_eq!(counter_value(std::ptr::null_mut()), None);
    }

    #[test]
    fn counters_are_independent() {
        let _guard = test_guard();
        let a = counter_register();
        let b = counter_register();
        assert_ne!(a, b);
        assert!(counter_increment(a));
        assert_eq!(counter_value(a), Some(1));
        assert_eq!(counter_value(b), Some(0));
        counter_finalize(std::ptr::null_mut(), a, std::ptr::null_mut());
        assert_eq!(counter_value(a), None);
        assert_eq!(counter_value(b), Some(0));
        assert!(counter_release(b));
    }

    #[test]
    fn poisoned_lock_still_serves_every_caller() {
        let _guard = test_guard();
        // `counters()` must recover a poisoned lock instead of unwinding: its
        // callers are `extern "C"` callbacks where a panic aborts the host.
        // Poison the lock once, then prove the registry keeps working.
        let poison = std::panic::catch_unwind(|| {
            let _guard = COUNTERS.lock().unwrap();
            panic!("poison the registry lock");
        });
        assert!(poison.is_err(), "the probe panic must be caught");
        // Paranoia: if the panic never reached the guard the lock, this test
        // would pass while covering nothing -- confirm the lock IS poisoned.
        assert!(COUNTERS.is_poisoned(), "the lock must be poisoned");
        let native = counter_register();
        assert_eq!(counter_value(native), Some(0));
        assert!(counter_increment(native));
        assert_eq!(counter_value(native), Some(1));
        assert!(counter_release(native));
        assert_eq!(counter_value(native), None);
    }
}
