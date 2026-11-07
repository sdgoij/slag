//! Drive the Bellard C compat tests (compiled by `build.rs` into
//! `libqjs_compat_tests.a`): the drop-in proof for the Bellard skin's ABI
//! deltas (`JS_NewClassID` one-arg, `JS_NewArrayBuffer` free callback, and
//! the `JS_BOOL` predicates returning `int` with a context).

#![cfg(feature = "bellard")]

#[link(name = "qjs_compat_tests", kind = "static")]
unsafe extern "C" {
    fn qjs_test_api_delta();
    fn qjs_test_predicates();
    fn qjs_test_bellard_only();
    fn qjs_test_print_value();
    fn qjs_test_abi_layout();
}

#[test]
fn crate_is_linked() {
    // Reference the crate so rustc keeps it on the linker line: the C archive
    // calls back into the exported JS* symbols, which only exist once the
    // quickjs rlib is linked.
    let _: *mut quickjs::Runtime = std::ptr::null_mut();
}

#[test]
fn c_api_delta() {
    unsafe {
        qjs_test_api_delta();
    }
}

#[test]
fn c_predicates() {
    unsafe {
        qjs_test_predicates();
    }
}

#[test]
fn c_bellard_only() {
    unsafe {
        qjs_test_bellard_only();
    }
}

#[test]
fn c_print_value() {
    unsafe {
        qjs_test_print_value();
    }
}

#[test]
fn c_abi_layout() {
    unsafe {
        qjs_test_abi_layout();
    }
}
