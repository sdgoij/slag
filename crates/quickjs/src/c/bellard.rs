//! Bellard-only exports (absent from QuickJS-NG): the strict/same-value
//! comparisons, `JS_ParseJSON2`, the intrinsic-add no-ops, the strip-info
//! flag, and the `JS_PrintValue*` debug-dump family.

use std::cell::Cell;
use std::ffi::{c_char, c_void};

use runtime::api::{self, Local};

use crate::c::abi;
use crate::c::module::set_exception;
use crate::c::runtime::{JSContext, JSRuntime, context_api, current};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_StrictEq(
    _ctx: *mut JSContext,
    op1: abi::JSValue,
    op2: abi::JSValue,
) -> abi::JS_BOOL {
    let (Some(a), Some(b)) = (abi::jsvalue_to_value(op1), abi::jsvalue_to_value(op2)) else {
        return abi::js_bool(false);
    };
    abi::js_bool(crux::ops::is_strictly_equal(&a, &b))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_SameValue(
    _ctx: *mut JSContext,
    op1: abi::JSValue,
    op2: abi::JSValue,
) -> abi::JS_BOOL {
    let (Some(a), Some(b)) = (abi::jsvalue_to_value(op1), abi::jsvalue_to_value(op2)) else {
        return abi::js_bool(false);
    };
    abi::js_bool(crux::ops::same_value(&a, &b))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_SameValueZero(
    _ctx: *mut JSContext,
    op1: abi::JSValue,
    op2: abi::JSValue,
) -> abi::JS_BOOL {
    let (Some(a), Some(b)) = (abi::jsvalue_to_value(op1), abi::jsvalue_to_value(op2)) else {
        return abi::js_bool(false);
    };
    abi::js_bool(crux::ops::same_value_zero(&a, &b))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_ParseJSON2(
    ctx: *mut JSContext,
    buf: *const c_char,
    buf_len: usize,
    _filename: *const c_char,
    _flags: i32,
) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    if buf.is_null() {
        return abi::exception();
    }
    // SAFETY: the header requires `buf_len` readable bytes from `buf`.
    let bytes = unsafe { std::slice::from_raw_parts(buf as *const u8, buf_len) };
    let Ok(source) = std::str::from_utf8(bytes) else {
        return abi::exception();
    };
    match api::Json::parse(&api_ctx, source) {
        Ok(local) => abi::value_to_jsvalue(local.into_value()),
        Err(error) => set_exception(&api_ctx, &error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_AddIntrinsicStringNormalize(_ctx: *mut JSContext) {
    // The engine ships a full realm; String.prototype.normalize is already
    // installed, so this matches the "intrinsic additions are no-ops" policy.
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_SetUncatchableException(_ctx: *mut JSContext, _flag: abi::JS_BOOL) {
    // Uncatchable exceptions are not modeled; every thrown value is catchable.
}

thread_local! {
    static STRIP_INFO: Cell<i32> = const { Cell::new(0) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_SetStripInfo(_rt: *mut JSRuntime, flags: i32) {
    STRIP_INFO.with(|strip| strip.set(flags));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_GetStripInfo(_rt: *mut JSRuntime) -> i32 {
    STRIP_INFO.with(|strip| strip.get())
}

/// The C `JSPrintValueWrite` (quickjs.h): the dump's text sink.
pub type JSPrintValueWrite = unsafe extern "C" fn(*mut c_void, *const c_char, usize);

/// The C `JSPrintValueOptions` (quickjs.h): two 8-bit flags packed into the
/// first 32-bit unit, then three `uint32_t` limits. Matches the header's
/// bitfield layout on both MSVC and GNU (16 bytes, 4-byte aligned).
#[repr(C)]
pub struct JSPrintValueOptions {
    pub show_hidden: u8,
    pub raw_dump: u8,
    pub max_depth: u32,
    pub max_string_length: u32,
    pub max_item_count: u32,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_PrintValueSetDefaultOptions(options: *mut JSPrintValueOptions) {
    if options.is_null() {
        return;
    }
    // SAFETY: the caller passes a writable options struct or null.
    unsafe {
        *options = JSPrintValueOptions {
            show_hidden: 0,
            raw_dump: 0,
            max_depth: 0,
            max_string_length: 0,
            max_item_count: 0,
        };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_PrintValueRT(
    _rt: *mut JSRuntime,
    write_func: Option<JSPrintValueWrite>,
    write_opaque: *mut c_void,
    val: abi::JSValue,
    options: *const JSPrintValueOptions,
) {
    print_value(current(), write_func, write_opaque, val, options);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_PrintValue(
    ctx: *mut JSContext,
    write_func: Option<JSPrintValueWrite>,
    write_opaque: *mut c_void,
    val: abi::JSValue,
    options: *const JSPrintValueOptions,
) {
    print_value(ctx, write_func, write_opaque, val, options);
}

fn print_value(
    ctx: *mut JSContext,
    write_func: Option<JSPrintValueWrite>,
    write_opaque: *mut c_void,
    val: abi::JSValue,
    options: *const JSPrintValueOptions,
) {
    let Some(write_func) = write_func else {
        return;
    };
    let max_string_length = if options.is_null() {
        0
    } else {
        // SAFETY: the caller passes a readable options struct or null.
        unsafe { (*options).max_string_length }
    };
    let bytes = dump_string(ctx, val, max_string_length);
    // SAFETY: `write_func` is the host's writer; `bytes` is the dump text.
    unsafe {
        write_func(write_opaque, bytes.as_ptr() as *const c_char, bytes.len());
    }
}

fn dump_string(ctx: *mut JSContext, val: abi::JSValue, max_string_length: u32) -> Vec<u8> {
    let mut text = match abi::jsvalue_to_value(val) {
        Some(value) => {
            let local = Local::from_value(value);
            context_api(ctx)
                .and_then(|api_ctx| api::Json::stringify(&api_ctx, &local).ok())
                .and_then(|string| string.as_string())
                .or_else(|| local.as_string())
                .unwrap_or_else(|| local.type_of().to_string())
        }
        None => "<invalid>".to_string(),
    };
    if max_string_length > 0 {
        text = text.chars().take(max_string_length as usize).collect();
    }
    text.into_bytes()
}
