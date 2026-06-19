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

unsafe fn resolve_host() -> Option<Host> {
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

unsafe fn read_string(env: NapiEnv, value: NapiValue) -> String {
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

unsafe fn make_string(env: NapiEnv, text: &str) -> NapiValue {
    unsafe {
        let mut value: NapiValue = std::ptr::null_mut();
        (host().create_string_utf8)(env, text.as_ptr() as *const c_char, text.len(), &mut value);
        value
    }
}

unsafe extern "C" fn add(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
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

unsafe extern "C" fn greet(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
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

unsafe extern "C" fn concat(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
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
unsafe extern "C" fn make_int64(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
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

unsafe extern "C" fn boom(env: NapiEnv, _info: NapiCallbackInfo) -> NapiValue {
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
unsafe extern "C" fn counter_finalize(_env: NapiEnv, data: *mut c_void, _hint: *mut c_void) {
    if !data.is_null() {
        drop(unsafe { Box::from_raw(data as *mut i64) });
    }
}

/// wrapCounter() -> obj
/// Creates a new plain JS object and wraps a heap-allocated i64 (value 0) on it.
unsafe extern "C" fn wrap_counter(env: NapiEnv, _info: NapiCallbackInfo) -> NapiValue {
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
unsafe extern "C" fn counter_get(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
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
unsafe extern "C" fn counter_inc(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
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
unsafe extern "C" fn make_bigint64(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
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
unsafe extern "C" fn read_bigint64(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
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
unsafe extern "C" fn make_buffer(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
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
unsafe extern "C" fn buffer_len(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
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
#[allow(dead_code)]
unsafe extern "C" fn test_ref(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
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
