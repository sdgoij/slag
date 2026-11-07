//! NUL-terminated buffers handed to the host as `JS_ToCString*` /
//! `JS_AtomToCString*` pointers. The entry keeps the allocation alive and its
//! address stable; the host frees it with `JS_FreeCString`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_char;

thread_local! {
    static CSTRINGS: RefCell<HashMap<usize, Box<[u8]>>> = RefCell::new(HashMap::new());
}

pub(crate) fn retain_cstring(bytes: &[u8]) -> *const c_char {
    let mut buffer = Vec::with_capacity(bytes.len() + 1);
    buffer.extend_from_slice(bytes);
    buffer.push(0);
    let boxed: Box<[u8]> = buffer.into_boxed_slice();
    let ptr = boxed.as_ptr();
    CSTRINGS.with(|strings| strings.borrow_mut().insert(ptr as usize, boxed));
    ptr as *const c_char
}

pub(crate) fn release_cstring(ptr: *const c_char) {
    if ptr.is_null() {
        return;
    }
    CSTRINGS.with(|strings| {
        strings.borrow_mut().remove(&(ptr as usize));
    });
}
