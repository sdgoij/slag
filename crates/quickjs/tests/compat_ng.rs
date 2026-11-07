//! Drive the C compat tests (compiled by `build.rs` into `libqjs_compat_tests.a`):
//! the drop-in proof — C programs written against `quickjs.h`, compiled with the
//! vendored header, linked against the `qjs` crate, running unmodified.

#![cfg(feature = "ng")]

#[link(name = "qjs_compat_tests", kind = "static")]
unsafe extern "C" {
    fn qjs_test_eval_and_values();
    fn qjs_test_objects();
    fn qjs_test_exceptions();
    fn qjs_test_atoms_and_conversions();
    fn qjs_test_errors();
    fn qjs_test_allocator();
    fn qjs_test_classes();
    fn qjs_test_class_callbacks();
    fn qjs_test_exotic_methods();
    fn qjs_test_exotic_define_and_names();
    fn qjs_test_cfunctions();
    fn qjs_test_function_lists();
    fn qjs_test_function_list_accessors();
    fn qjs_test_boxed_string_gc();
    fn qjs_test_introspection();
    fn qjs_test_builtin_introspection();
    fn qjs_test_array_buffer();
    fn qjs_test_typed_array();
    fn qjs_test_host_buffer();
    fn qjs_test_host_uint8_array();
    fn qjs_test_new_typed_array();
    fn qjs_test_promises();
    fn qjs_test_promise_capability();
    fn qjs_test_jobs();
    fn qjs_test_source_module();
    fn qjs_test_c_module();
    fn qjs_test_module_loader();
    fn qjs_test_mark_value();
    fn qjs_test_module_loader_attrs();
    fn qjs_test_dynamic_import();
    fn qjs_test_host_value_survives_gc();
    fn qjs_test_box_canonicalization();
    fn qjs_test_cstring_table_growth();
    fn qjs_test_atom_lifetime_across_contexts();
    fn qjs_test_gc_lifetime();
    fn qjs_test_context_released_in_callback();
    fn qjs_test_reentrant_callback_throws();
    fn qjs_test_exception_nesting();
    fn qjs_test_throw_out_of_memory();
    fn qjs_test_abi_layout();
    fn qjs_test_by_value_jsvalue_abi();
}

#[test]
fn crate_is_linked() {
    // Reference the crate so rustc keeps it on the linker line: the C archive
    // calls back into the exported JS* symbols, which only exist once the
    // quickjs rlib is linked.
    let _: *mut quickjs::Runtime = std::ptr::null_mut();
}

#[test]
fn c_eval_and_values() {
    unsafe {
        qjs_test_eval_and_values();
    }
}

#[test]
fn c_objects() {
    unsafe {
        qjs_test_objects();
    }
}

#[test]
fn c_exceptions() {
    unsafe {
        qjs_test_exceptions();
    }
}

#[test]
fn c_atoms_and_conversions() {
    unsafe {
        qjs_test_atoms_and_conversions();
    }
}

#[test]
fn c_errors() {
    unsafe {
        qjs_test_errors();
    }
}

#[test]
fn c_allocator() {
    unsafe {
        qjs_test_allocator();
    }
}

#[test]
fn c_classes() {
    unsafe {
        qjs_test_classes();
    }
}

#[test]
fn c_class_callbacks() {
    unsafe {
        qjs_test_class_callbacks();
    }
}

#[test]
fn c_exotic_methods() {
    unsafe {
        qjs_test_exotic_methods();
    }
}

#[test]
fn c_exotic_define_and_names() {
    unsafe {
        qjs_test_exotic_define_and_names();
    }
}

#[test]
fn c_cfunctions() {
    unsafe {
        qjs_test_cfunctions();
    }
}

#[test]
fn c_function_lists() {
    unsafe {
        qjs_test_function_lists();
    }
}

#[test]
fn c_function_list_accessors() {
    unsafe {
        qjs_test_function_list_accessors();
    }
}

#[test]
fn c_boxed_string_gc() {
    unsafe {
        qjs_test_boxed_string_gc();
    }
}

#[test]
fn c_introspection() {
    unsafe {
        qjs_test_introspection();
    }
}

#[test]
fn c_builtin_introspection() {
    unsafe {
        qjs_test_builtin_introspection();
    }
}

#[test]
fn c_array_buffer() {
    unsafe {
        qjs_test_array_buffer();
    }
}

#[test]
fn c_typed_array() {
    unsafe {
        qjs_test_typed_array();
    }
}

#[test]
fn c_host_buffer() {
    unsafe {
        qjs_test_host_buffer();
    }
}

#[test]
fn c_host_uint8_array() {
    unsafe {
        qjs_test_host_uint8_array();
    }
}

#[test]
fn c_new_typed_array() {
    unsafe {
        qjs_test_new_typed_array();
    }
}

#[test]
fn c_promises() {
    unsafe {
        qjs_test_promises();
    }
}

#[test]
fn c_promise_capability() {
    unsafe {
        qjs_test_promise_capability();
    }
}

#[test]
fn c_jobs() {
    unsafe {
        qjs_test_jobs();
    }
}

#[test]
fn c_source_module() {
    unsafe {
        qjs_test_source_module();
    }
}

#[test]
fn c_c_module() {
    unsafe {
        qjs_test_c_module();
    }
}

#[test]
fn c_module_loader() {
    unsafe {
        qjs_test_module_loader();
    }
}

#[test]
fn c_mark_value() {
    unsafe {
        qjs_test_mark_value();
    }
}

#[test]
fn c_module_loader_attrs() {
    unsafe {
        qjs_test_module_loader_attrs();
    }
}

#[test]
fn c_dynamic_import() {
    unsafe {
        qjs_test_dynamic_import();
    }
}

#[test]
fn c_host_value_survives_gc() {
    unsafe {
        qjs_test_host_value_survives_gc();
    }
}

#[test]
fn c_box_canonicalization() {
    unsafe {
        qjs_test_box_canonicalization();
    }
}

#[test]
fn c_cstring_table_growth() {
    unsafe {
        qjs_test_cstring_table_growth();
    }
}

#[test]
fn c_atom_lifetime_across_contexts() {
    unsafe {
        qjs_test_atom_lifetime_across_contexts();
    }
}

#[test]
fn c_gc_lifetime() {
    unsafe {
        qjs_test_gc_lifetime();
    }
}

#[test]
fn c_context_released_in_callback() {
    unsafe {
        qjs_test_context_released_in_callback();
    }
}

#[test]
fn c_reentrant_callback_throws() {
    unsafe {
        qjs_test_reentrant_callback_throws();
    }
}

#[test]
fn c_exception_nesting() {
    unsafe {
        qjs_test_exception_nesting();
    }
}

#[test]
fn c_throw_out_of_memory() {
    unsafe {
        qjs_test_throw_out_of_memory();
    }
}

#[test]
fn c_abi_layout() {
    unsafe {
        qjs_test_abi_layout();
    }
}

#[test]
fn c_by_value_jsvalue_abi() {
    unsafe {
        qjs_test_by_value_jsvalue_abi();
    }
}
