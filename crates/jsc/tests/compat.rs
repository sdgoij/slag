//! Drive the C compat tests (compiled by build.rs into
//! `libjsc_compat_tests.a`) — the drop-in proof: C programs written against
//! the documented JavaScriptCore C API, compiled with our headers, linked
//! against the jsc crate, running unmodified.

#[link(name = "jsc_compat_tests", kind = "static")]
unsafe extern "C" {
    fn test_strings();
    fn test_eval_numbers();
    fn test_exceptions();
    fn test_object_properties();
    fn test_host_function();
    fn test_json();
    fn test_class_callbacks();
    fn test_arrays_and_indices();
}

/// Reference the crate so rustc keeps it on the linker line: the C archive
/// calls back into the exported JS* symbols, which only exist once the jsc
/// rlib is linked.
#[test]
fn crate_is_linked() {
    let _: *mut jsc::OpaqueJSContext = std::ptr::null_mut();
}

#[test]
fn c_strings_and_eval() {
    unsafe {
        test_strings();
        test_eval_numbers();
    }
}

#[test]
fn c_objects_and_functions() {
    unsafe {
        test_object_properties();
        test_host_function();
    }
}

#[test]
fn c_exceptions_json_and_arrays() {
    unsafe {
        test_exceptions();
        test_json();
        test_arrays_and_indices();
    }
}

#[test]
fn c_class_callbacks() {
    unsafe {
        test_class_callbacks();
    }
}
