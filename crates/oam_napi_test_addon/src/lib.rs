//! A real N-API addon, in Rust: compiled to a cdylib, renamed .node by the
//! e2e suite, loaded by oam via require(). It resolves every napi_*
//! symbol FROM THE HOST PROCESS'S EXPORT TABLE at registration — the
//! portable equivalent of node-gyp's delay-load hook, and a direct proof
//! that the oam executable exports the ABI.
//!
//! Exports: add(a, b), greet(name), concat(list), answer (= 42), and
//! boom() which throws a TypeError with a code.

use std::ffi::{c_char, c_void};

type NapiEnv = *mut c_void;
type NapiValue = *mut c_void;
type NapiStatus = i32;
type NapiCallbackInfo = *mut c_void;
type NapiCallback = unsafe extern "C" fn(NapiEnv, NapiCallbackInfo) -> NapiValue;

/// The napi functions this addon consumes, resolved from the host.
struct Host {
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
        *mut NapiValue,
        *mut *mut c_void,
    ) -> NapiStatus,
    set_named_property:
        unsafe extern "C" fn(NapiEnv, NapiValue, *const c_char, NapiValue) -> NapiStatus,
    create_int32: unsafe extern "C" fn(NapiEnv, i32, *mut NapiValue) -> NapiStatus,
    get_value_int32: unsafe extern "C" fn(NapiEnv, NapiValue, *mut i32) -> NapiStatus,
    create_string_utf8:
        unsafe extern "C" fn(NapiEnv, *const c_char, usize, *mut NapiValue) -> NapiStatus,
    get_value_string_utf8:
        unsafe extern "C" fn(NapiEnv, NapiValue, *mut c_char, usize, *mut usize) -> NapiStatus,
    throw_type_error: unsafe extern "C" fn(NapiEnv, *const c_char, *const c_char) -> NapiStatus,
    get_array_length: unsafe extern "C" fn(NapiEnv, NapiValue, *mut u32) -> NapiStatus,
    get_element: unsafe extern "C" fn(NapiEnv, NapiValue, u32, *mut NapiValue) -> NapiStatus,
    get_undefined: unsafe extern "C" fn(NapiEnv, *mut NapiValue) -> NapiStatus,
}

static HOST: std::sync::OnceLock<Host> = std::sync::OnceLock::new();

fn host() -> &'static Host {
    HOST.get().expect("host napi symbols resolved")
}

unsafe fn resolve_host() -> Option<Host> {
    // The host EXECUTABLE's export table (GetModuleHandle(NULL) /
    // dlopen(NULL)) — exactly where node addons find the ABI.
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
            create_function: sym!(b"napi_create_function"),
            get_cb_info: sym!(b"napi_get_cb_info"),
            set_named_property: sym!(b"napi_set_named_property"),
            create_int32: sym!(b"napi_create_int32"),
            get_value_int32: sym!(b"napi_get_value_int32"),
            create_string_utf8: sym!(b"napi_create_string_utf8"),
            get_value_string_utf8: sym!(b"napi_get_value_string_utf8"),
            throw_type_error: sym!(b"napi_throw_type_error"),
            get_array_length: sym!(b"napi_get_array_length"),
            get_element: sym!(b"napi_get_element"),
            get_undefined: sym!(b"napi_get_undefined"),
        })
    }
}

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

unsafe extern "C" fn boom(env: NapiEnv, _info: NapiCallbackInfo) -> NapiValue {
    unsafe {
        (host().throw_type_error)(env, c"ERR_NATIVE_BOOM".as_ptr(), c"native says no".as_ptr());
        let mut undefined: NapiValue = std::ptr::null_mut();
        (host().get_undefined)(env, &mut undefined);
        undefined
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_register_module_v1(env: NapiEnv, exports: NapiValue) -> NapiValue {
    unsafe {
        let Some(resolved) = resolve_host() else {
            return exports; // host exports missing: leave exports empty
        };
        let _ = HOST.set(resolved);

        let bindings: [(&std::ffi::CStr, NapiCallback); 4] = [
            (c"add", add),
            (c"greet", greet),
            (c"concat", concat),
            (c"boom", boom),
        ];
        for (name, callback) in bindings {
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
