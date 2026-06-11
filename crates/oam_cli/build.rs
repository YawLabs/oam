//! Export the N-API symbol surface from the oam EXECUTABLE — .node addons
//! resolve `napi_*` from their host process (node.exe's model). Windows
//! needs an explicit .def; Linux needs --export-dynamic; macOS executables
//! export by default.

const NAPI_EXPORTS: &[&str] = &[
    "napi_get_undefined",
    "napi_get_null",
    "napi_get_global",
    "napi_get_boolean",
    "napi_create_int32",
    "napi_create_uint32",
    "napi_create_int64",
    "napi_create_double",
    "napi_create_string_utf8",
    "napi_create_object",
    "napi_create_array",
    "napi_create_array_with_length",
    "napi_typeof",
    "napi_get_value_bool",
    "napi_get_value_int32",
    "napi_get_value_uint32",
    "napi_get_value_int64",
    "napi_get_value_double",
    "napi_get_value_string_utf8",
    "napi_is_array",
    "napi_get_array_length",
    "napi_strict_equals",
    "napi_coerce_to_string",
    "napi_set_named_property",
    "napi_get_named_property",
    "napi_has_named_property",
    "napi_set_property",
    "napi_get_property",
    "napi_set_element",
    "napi_get_element",
    "napi_create_function",
    "napi_get_cb_info",
    "napi_call_function",
    "napi_throw",
    "napi_throw_error",
    "napi_throw_type_error",
    "napi_is_exception_pending",
    "napi_get_and_clear_last_exception",
    "napi_get_version",
];

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        "windows" => {
            let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
            let def = out.join("napi.def");
            let mut text = String::from("EXPORTS\n");
            for symbol in NAPI_EXPORTS {
                text.push_str(symbol);
                text.push('\n');
            }
            std::fs::write(&def, text).expect("write napi.def");
            println!("cargo:rustc-link-arg-bins=/DEF:{}", def.display());
        }
        "linux" => {
            println!("cargo:rustc-link-arg-bins=-Wl,--export-dynamic");
        }
        _ => {
            // macOS executables export their symbols by default; addons
            // link with -undefined dynamic_lookup.
        }
    }
}
