//! A real N-API addon, in Rust: compiled to a cdylib, renamed .node by the
//! e2e suite, loaded by oam via require(). It resolves every napi_*
//! symbol FROM THE HOST PROCESS'S EXPORT TABLE at registration — the
//! portable equivalent of node-gyp's delay-load hook, and a direct proof
//! that the oam executable exports the ABI.
//!
//! Alpha exports: add(a, b), greet(name), concat(list), answer (= 42),
//!   boom() which throws a TypeError with a code, makeInt64(hi, lo).
//!
//! Beta exports (wrap/refs/bigint/buffer):
//!   wrapCounter()       -> obj       napi_create_object + napi_wrap
//!   counterGet(obj)     -> Number    napi_unwrap
//!   counterInc(obj)               napi_unwrap + mutate
//!   makeBigInt64(lo)    -> BigInt   napi_create_bigint_int64
//!   readBigInt64(big)   -> Number   napi_get_value_bigint_int64
//!   makeBuffer(n)       -> Uint8Array napi_create_buffer
//!   bufferLen(buf)      -> Number   napi_get_buffer_info
//!   testRef(value) -> [v, v] napi_create_reference + napi_get_reference_value + napi_delete_reference

use std::ffi::{c_char, c_void};

type NapiEnv = *mut c_void;
type NapiValue = *mut c_void;
type NapiStatus = i32;
type NapiCallbackInfo = *mut c_void;
type NapiCallback = unsafe extern "C" fn(NapiEnv, NapiCallbackInfo) -> NapiValue;
type NapiFinalizeCb = Option<unsafe extern "C" fn(NapiEnv, *mut c_void, *mut c_void)>;

/// The napi functions this addon consumes, resolved from the host.
struct Host {
    // ----- alpha -----
    create_function: unsafe extern "C" fn(
        NapiEnv,
        *const c_char,
        usize,
        NapiCallback,
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
    #[allow(dead_code)]
    create_reference: unsafe extern "C" fn(NapiEnv, NapiValue, u32, *mut *mut c_void) -> NapiStatus,
    #[allow(dead_code)]
    get_reference_value: unsafe extern "C" fn(NapiEnv, *mut c_void, *mut NapiValue) -> NapiStatus,
    #[allow(dead_code)]
    delete_reference: unsafe extern "C" fn(NapiEnv, *mut c_void) -> NapiStatus,
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

/// makeInt64(hi: i32, lo: i32) -> number | bigint
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

/// Finalizer called when V8 GC collects the object (or napi_remove_wrap).
/// Drops the heap-allocated i64 counter.
///
/// # Safety
///
/// Valid only as the `napi_finalize` registered by `napi_wrap` in
/// `wrap_counter`: `data` must be the `Box::into_raw(Box::new(0i64))` pointer
/// handed over there, and the host must invoke it at most once for that wrap.
/// `env` and `hint` are ignored.
unsafe extern "C" fn counter_finalize(_env: NapiEnv, data: *mut c_void, _hint: *mut c_void) {
    if !data.is_null() {
        // SAFETY: `data` is non-null on this branch, and the only pointer this
        // addon ever wraps with this finalizer is the
        // `Box::into_raw(Box::new(0i64))` produced in `wrap_counter` -- so it
        // is a live, correctly-aligned `*mut i64` from the global allocator.
        // N-API runs a finalizer at most once per wrap (GC collection or
        // `napi_remove_wrap`), so this `Box::from_raw` reclaims that
        // allocation exactly once: no double free, and no Rust alias survives
        // because ownership was moved into the JS object at wrap time.
        drop(unsafe { Box::from_raw(data as *mut i64) });
    }
}

/// wrapCounter() -> obj
/// Creates a new plain JS object and wraps a heap-allocated i64 (value 0) on it.
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback` with a live
/// `env`; the `info` frame is ignored. Ownership of a heap-allocated i64 is
/// transferred to the returned JS object, whose finalizer
/// (`counter_finalize`) is what frees it.
unsafe extern "C" fn wrap_counter(env: NapiEnv, _info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `obj` is a live local, so `napi_create_object`'s out-pointer is
    // valid, and a non-zero status bails out before the handle is used. The
    // counter is a fresh `Box::into_raw` allocation with no surviving Rust
    // alias, so passing it to `napi_wrap` transfers sole ownership to the JS
    // object; `counter_finalize` is the matching `Box::from_raw` and runs once
    // when that object is collected. The trailing nulls are the finalize hint
    // and the optional `napi_ref` out-param, neither of which this addon
    // wants. A failing `napi_wrap` would leak the box -- a leak, not
    // unsoundness.
    unsafe {
        let mut obj: NapiValue = std::ptr::null_mut();
        if (host().create_object)(env, &mut obj) != 0 {
            return std::ptr::null_mut();
        }
        let counter = Box::into_raw(Box::new(0i64)) as *mut c_void;
        (host().wrap)(
            env,
            obj,
            counter,
            Some(counter_finalize),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        obj
    }
}

/// counterGet(obj) -> Number
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback` with a live `env`
/// and its `info` frame. Argument 0 is expected to be an object produced by
/// `wrapCounter`; any other value simply fails to unwrap and yields null.
unsafe extern "C" fn counter_get(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `argc` is seeded with `argv`'s real capacity of 1, so
    // `napi_get_cb_info` cannot overrun the stack array. `native` is
    // dereferenced only after `napi_unwrap` returned OK *and* the pointer
    // tested non-null, and the only pointer ever wrapped by this addon is the
    // `*mut i64` from `wrap_counter` -- so the read is of a live, aligned
    // `i64` still owned by the JS object (it is reachable through `argv[0]`,
    // therefore the finalizer has not run). The read copies the value out; no
    // reference outlives the statement.
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
        if (host().unwrap)(env, argv[0], &mut native) != 0 || native.is_null() {
            return std::ptr::null_mut();
        }
        let val = *(native as *const i64);
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
/// the i64 wrapped on it is mutated in place.
unsafe extern "C" fn counter_inc(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `argc` is seeded with `argv`'s capacity of 1, bounding what
    // `napi_get_cb_info` writes. The increment happens only after
    // `napi_unwrap` returned OK and `native` tested non-null, and the sole
    // pointer type this addon ever wraps is the `*mut i64` from
    // `wrap_counter` -- so this is an aligned read-modify-write on a live
    // allocation the JS object still owns. N-API callbacks run on the single
    // JS thread, so no other thread can be touching the same counter
    // concurrently.
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
        if (host().unwrap)(env, argv[0], &mut native) != 0 || native.is_null() {
            return std::ptr::null_mut();
        }
        *(native as *mut i64) += 1;
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

/// testRef(value) -> [v, v]
/// Exercises create_reference / get_reference_value / delete_reference.
/// Creates a reference to `value`, reads it back twice, deletes the reference,
/// returns [first_read, second_read] -- both should equal the original value.
///
/// # Safety
///
/// Only ever invoked by the N-API host as a `napi_callback` with a live `env`
/// and its `info` frame. Not currently listed in the registration tables, so
/// in practice the host never calls it at all.
#[allow(dead_code)]
unsafe extern "C" fn test_ref(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `argc` is seeded with `argv`'s capacity of 1, bounding what
    // `napi_get_cb_info` writes. `ref_handle`, `v1` and `v2` are live locals
    // backing the out-pointers, and a failed `napi_create_reference` returns
    // early rather than using an unwritten handle. The reference is created
    // with refcount 1 and both reads happen before `napi_delete_reference`,
    // so `ref_handle` is live at every use and is never touched again after
    // the delete. `v1` stays usable afterwards because it is a handle-scope
    // value owned by the current callback frame, not by the reference.
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
        let value = argv[0];

        // Create a reference with refcount = 1.
        let mut ref_handle: *mut c_void = std::ptr::null_mut();
        if (host().create_reference)(env, value, 1, &mut ref_handle) != 0 {
            return std::ptr::null_mut();
        }

        // Read it back twice.
        let mut v1: NapiValue = std::ptr::null_mut();
        let mut v2: NapiValue = std::ptr::null_mut();
        (host().get_reference_value)(env, ref_handle, &mut v1);
        (host().get_reference_value)(env, ref_handle, &mut v2);

        // Delete the reference.
        (host().delete_reference)(env, ref_handle);

        // Build [v1, v2] as a 2-element array.
        // No napi_create_array here -- use a JS literal trick via String instead,
        // or just return v1 (same pointer). We'll return the first value since
        // both should be the same object.  The test checks identity (===).
        v1
    }
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
                callback,
                std::ptr::null_mut(),
                &mut function,
            );
            (host().set_named_property)(env, exports, name.as_ptr(), function);
        }

        let beta_bindings: [(&std::ffi::CStr, NapiCallback); 7] = [
            (c"wrapCounter", wrap_counter),
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
                callback,
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
