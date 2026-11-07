//! The `js_malloc` family: a size-tracking allocator. Rust's allocator API
//! needs the layout to deallocate, so every live allocation records its size
//! in a thread-local table. C hosts use these exactly as QuickJS's — allocate
//! with `js_malloc`/`js_calloc`/`js_realloc`, free with `js_free`.

use std::alloc::{Layout, alloc, alloc_zeroed, dealloc, realloc};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};

use crate::c::runtime::{JSContext, JSRuntime};

#[derive(Clone, Copy)]
struct AllocEntry {
    layout: Layout,
    size: usize,
}

thread_local! {
    static ALLOCS: RefCell<HashMap<usize, AllocEntry>> = RefCell::new(HashMap::new());
}

fn layout(size: usize) -> Layout {
    // `max_align_t` is 16 on x86-64; `malloc` promises that much.
    Layout::from_size_align(size, 16).unwrap()
}

fn malloc_impl(size: usize) -> *mut c_void {
    if size == 0 {
        return std::ptr::null_mut();
    }
    let layout = layout(size);
    let ptr = unsafe { alloc(layout) } as *mut c_void;
    if ptr.is_null() {
        return ptr;
    }
    ALLOCS.with(|allocs| {
        allocs
            .borrow_mut()
            .insert(ptr as usize, AllocEntry { layout, size });
    });
    ptr
}

fn calloc_impl(count: usize, size: usize) -> *mut c_void {
    let total = count.saturating_mul(size);
    if total == 0 {
        return std::ptr::null_mut();
    }
    let layout = layout(total);
    let ptr = unsafe { alloc_zeroed(layout) } as *mut c_void;
    if ptr.is_null() {
        return ptr;
    }
    ALLOCS.with(|allocs| {
        allocs.borrow_mut().insert(
            ptr as usize,
            AllocEntry {
                layout,
                size: total,
            },
        );
    });
    ptr
}

fn free_impl(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let entry = ALLOCS.with(|allocs| allocs.borrow_mut().remove(&(ptr as usize)));
    if let Some(entry) = entry {
        unsafe { dealloc(ptr as *mut u8, entry.layout) };
    }
}

fn realloc_impl(ptr: *mut c_void, size: usize) -> *mut c_void {
    if ptr.is_null() {
        return malloc_impl(size);
    }
    if size == 0 {
        free_impl(ptr);
        return std::ptr::null_mut();
    }
    let Some(old) = ALLOCS.with(|allocs| allocs.borrow().get(&(ptr as usize)).copied()) else {
        return std::ptr::null_mut();
    };
    let new_layout = layout(size);
    let new_ptr = unsafe { realloc(ptr as *mut u8, old.layout, size) } as *mut c_void;
    if new_ptr.is_null() {
        return new_ptr;
    }
    ALLOCS.with(|allocs| {
        let mut allocs = allocs.borrow_mut();
        allocs.remove(&(ptr as usize));
        allocs.insert(
            new_ptr as usize,
            AllocEntry {
                layout: new_layout,
                size,
            },
        );
    });
    new_ptr
}

fn usable_impl(ptr: *const c_void) -> usize {
    ALLOCS.with(|allocs| {
        allocs
            .borrow()
            .get(&(ptr as usize))
            .map(|entry| entry.size)
            .unwrap_or(0)
    })
}

fn strdup_impl(str: *const c_char) -> *mut c_char {
    if str.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the header requires a NUL-terminated string.
    let text = unsafe { CStr::from_ptr(str) }.to_bytes();
    let ptr = malloc_impl(text.len() + 1) as *mut u8;
    if ptr.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `ptr` is a fresh allocation of `text.len() + 1` bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), ptr, text.len());
        *ptr.add(text.len()) = 0;
    }
    ptr as *mut c_char
}

fn strndup_impl(str: *const c_char, len: usize) -> *mut c_char {
    if str.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the header requires `len` bytes readable from `str`.
    let bytes = unsafe { std::slice::from_raw_parts(str as *const u8, len) };
    let copied = bytes.iter().position(|&byte| byte == 0).unwrap_or(len);
    let ptr = malloc_impl(copied + 1) as *mut u8;
    if ptr.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `ptr` is a fresh allocation of `copied + 1` bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, copied);
        *ptr.add(copied) = 0;
    }
    ptr as *mut c_char
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn js_malloc(_ctx: *mut JSContext, size: usize) -> *mut c_void {
    malloc_impl(size)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn js_mallocz(_ctx: *mut JSContext, size: usize) -> *mut c_void {
    calloc_impl(1, size)
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn js_calloc(_ctx: *mut JSContext, count: usize, size: usize) -> *mut c_void {
    calloc_impl(count, size)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn js_free(_ctx: *mut JSContext, ptr: *mut c_void) {
    free_impl(ptr);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn js_realloc(
    _ctx: *mut JSContext,
    ptr: *mut c_void,
    size: usize,
) -> *mut c_void {
    realloc_impl(ptr, size)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn js_malloc_usable_size(_ctx: *mut JSContext, ptr: *const c_void) -> usize {
    usable_impl(ptr)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn js_strdup(_ctx: *mut JSContext, str: *const c_char) -> *mut c_char {
    strdup_impl(str)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn js_strndup(
    _ctx: *mut JSContext,
    str: *const c_char,
    len: usize,
) -> *mut c_char {
    strndup_impl(str, len)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn js_malloc_rt(_rt: *mut JSRuntime, size: usize) -> *mut c_void {
    malloc_impl(size)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn js_mallocz_rt(_rt: *mut JSRuntime, size: usize) -> *mut c_void {
    calloc_impl(1, size)
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn js_calloc_rt(
    _rt: *mut JSRuntime,
    count: usize,
    size: usize,
) -> *mut c_void {
    calloc_impl(count, size)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn js_free_rt(_rt: *mut JSRuntime, ptr: *mut c_void) {
    free_impl(ptr);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn js_realloc_rt(
    _rt: *mut JSRuntime,
    ptr: *mut c_void,
    size: usize,
) -> *mut c_void {
    realloc_impl(ptr, size)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn js_malloc_usable_size_rt(
    _rt: *mut JSRuntime,
    ptr: *const c_void,
) -> usize {
    usable_impl(ptr)
}
