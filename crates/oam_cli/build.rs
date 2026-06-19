//! Export the N-API symbol surface from the oam EXECUTABLE — .node addons
//! resolve `napi_*` from their host process (node.exe's model). Windows
//! needs an explicit .def; Linux needs --export-dynamic; macOS modern ld
//! strips unreferenced exports unless told otherwise (-export_dynamic).

const NAPI_EXPORTS: &[&str] = &[
    // alpha wave
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
    // gamma wave: full napi-sys-2.4.0 surface
    "napi_get_last_error_info",
    "napi_create_error",
    "napi_create_type_error",
    "napi_create_range_error",
    "napi_throw_range_error",
    "napi_is_error",
    "napi_fatal_exception",
    "napi_create_string_latin1",
    "napi_create_string_utf16",
    "napi_get_value_string_latin1",
    "napi_get_value_string_utf16",
    "napi_create_symbol",
    "napi_open_handle_scope",
    "napi_close_handle_scope",
    "napi_open_escapable_handle_scope",
    "napi_close_escapable_handle_scope",
    "napi_escape_handle",
    "napi_open_callback_scope",
    "napi_close_callback_scope",
    "napi_has_property",
    "napi_delete_property",
    "napi_has_own_property",
    "napi_has_element",
    "napi_delete_element",
    "napi_define_properties",
    "napi_get_property_names",
    "napi_get_prototype",
    "napi_get_new_target",
    "napi_coerce_to_bool",
    "napi_coerce_to_number",
    "napi_coerce_to_object",
    "napi_is_arraybuffer",
    "napi_create_arraybuffer",
    "napi_create_external_arraybuffer",
    "napi_get_arraybuffer_info",
    "napi_is_typedarray",
    "napi_create_typedarray",
    "napi_get_typedarray_info",
    "napi_create_dataview",
    "napi_is_dataview",
    "napi_get_dataview_info",
    "napi_create_promise",
    "napi_resolve_deferred",
    "napi_reject_deferred",
    "napi_is_promise",
    "napi_run_script",
    "napi_adjust_external_memory",
    "napi_create_date",
    "napi_is_date",
    "napi_get_date_value",
    "napi_get_node_version",
    "napi_get_uv_event_loop",
    "napi_module_register",
    "napi_add_env_cleanup_hook",
    "napi_remove_env_cleanup_hook",
    "napi_add_finalizer",
    "napi_async_init",
    "napi_async_destroy",
    "napi_make_callback",
    "napi_create_async_work",
    "napi_delete_async_work",
    "napi_queue_async_work",
    "napi_cancel_async_work",
    "napi_create_threadsafe_function",
    "napi_get_threadsafe_function_context",
    "napi_call_threadsafe_function",
    "napi_acquire_threadsafe_function",
    "napi_release_threadsafe_function",
    "napi_unref_threadsafe_function",
    "napi_ref_threadsafe_function",
    // data symbol -- exported via static, included here for .def completeness
    "uv_event_loop",
];

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        "windows" => {
            let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
            let def = out.join("napi.def");
            let mut text = String::from("EXPORTS\n");
            for symbol in NAPI_EXPORTS {
                if *symbol == "uv_event_loop" {
                    // uv_event_loop is a DATA symbol, not a function.
                    text.push_str("uv_event_loop DATA\n");
                } else {
                    text.push_str(symbol);
                    text.push('\n');
                }
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
