//! Export the N-API symbol surface from the oam EXECUTABLE — .node addons
//! resolve `napi_*` from their host process (node.exe's model). Windows
//! needs an explicit .def; Linux needs --export-dynamic; macOS modern ld
//! strips unreferenced exports unless told otherwise (-export_dynamic).

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
    // Externals (beta)
    "napi_create_external",
    "napi_get_value_external",
    // References (beta)
    "napi_create_reference",
    "napi_delete_reference",
    "napi_get_reference_value",
    "napi_reference_ref",
    "napi_reference_unref",
    // Wrap / classes (beta)
    "napi_define_class",
    "napi_wrap",
    "napi_unwrap",
    "napi_remove_wrap",
    "napi_new_instance",
    "napi_instanceof",
    // BigInt (beta)
    "napi_create_bigint_int64",
    "napi_create_bigint_uint64",
    "napi_get_value_bigint_int64",
    "napi_get_value_bigint_uint64",
    // Buffers (beta)
    "napi_create_buffer",
    "napi_create_buffer_copy",
    "napi_create_external_buffer",
    "napi_is_buffer",
    "napi_get_buffer_info",
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
        "macos" => {
            // Apple's modern ld DOES strip unreferenced exports from the
            // dynamic symbol table; addons dlopen-resolving napi_* against
            // the host process need -export_dynamic to keep them visible.
            // (The cdylib side defers unresolved symbols to load time via
            // rustc's default `-undefined dynamic_lookup`.)
            println!("cargo:rustc-link-arg-bins=-Wl,-export_dynamic");
        }
        _ => {}
    }
}
