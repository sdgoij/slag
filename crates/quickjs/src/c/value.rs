//! Value constructors, conversions, and string marshaling.

use std::ffi::{CStr, c_char};

use crux::handle::Handle;
use crux::string::JsString;
use crux::value::Value as CruxValue;

use crate::c::abi;
use crate::c::cstring::{release_cstring, retain_cstring};
use crate::c::runtime::{JS_MarkFunc, JSContext, JSRuntime, context_api};

/// ToString (spec 7.1.12) of a decoded value, in the context's realm.
fn to_string_value(api_ctx: runtime::api::Context, value: CruxValue) -> Option<JsString> {
    api_ctx
        .with_agent(|agent| runtime::context::to_string(agent, &value))
        .ok()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewInt32(_ctx: *mut JSContext, value: i32) -> abi::JSValue {
    abi::int32(value)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewFloat64(_ctx: *mut JSContext, value: f64) -> abi::JSValue {
    abi::float64(value)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewBool(_ctx: *mut JSContext, value: bool) -> abi::JSValue {
    abi::boolean(value)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewStringLen(
    _ctx: *mut JSContext,
    str: *const c_char,
    len: usize,
) -> abi::JSValue {
    // SAFETY: the header requires `len` bytes readable from `str`.
    let bytes = unsafe { std::slice::from_raw_parts(str as *const u8, len) };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return abi::exception();
    };
    abi::value_to_jsvalue(CruxValue::String(Handle::new(JsString::from_utf8(text))))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewAtomString(ctx: *mut JSContext, str: *const c_char) -> abi::JSValue {
    // SAFETY: the header requires a NUL-terminated string.
    let len = unsafe { CStr::from_ptr(str) }.to_bytes().len();
    // SAFETY: forwarding the C contract to `JS_NewStringLen`.
    unsafe { JS_NewStringLen(ctx, str, len) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_FreeValue(_ctx: *mut JSContext, value: abi::JSValue) {
    abi::free_jsvalue(value);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_FreeValueRT(_rt: *mut JSRuntime, value: abi::JSValue) {
    abi::free_jsvalue(value);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_DupValue(_ctx: *mut JSContext, value: abi::JSValue) -> abi::JSValue {
    abi::dup_jsvalue(value)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_DupValueRT(_rt: *mut JSRuntime, value: abi::JSValue) -> abi::JSValue {
    abi::dup_jsvalue(value)
}

/// The `gc_mark` half of the class contract. A host's `JSClassGCMark` runs
/// inside the engine's mark phase to add the references a class instance
/// owns; the facade never runs that phase for host values — the box registry
/// roots every host-held value, so there is nothing to add. Exported as a
/// no-op so a host's `gc_mark` that calls it still links.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_MarkValue(
    _rt: *mut JSRuntime,
    _val: abi::JSValue,
    _mark_func: *const JS_MarkFunc,
) {
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_ToString(ctx: *mut JSContext, value: abi::JSValue) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    let Some(value) = abi::jsvalue_to_value(value) else {
        return abi::exception();
    };
    match to_string_value(api_ctx, value) {
        Some(string) => abi::value_to_jsvalue(CruxValue::String(Handle::new(string))),
        None => abi::exception(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_ToCStringLen2(
    ctx: *mut JSContext,
    plen: *mut usize,
    value: abi::JSValue,
    _cesu8: bool,
) -> *const c_char {
    let Some(api_ctx) = context_api(ctx) else {
        return std::ptr::null();
    };
    let Some(value) = abi::jsvalue_to_value(value) else {
        return std::ptr::null();
    };
    let Some(string) = to_string_value(api_ctx, value) else {
        return std::ptr::null();
    };
    let text = string.to_string_lossy();
    if !plen.is_null() {
        // SAFETY: the caller passes a writable length slot or null.
        unsafe { *plen = text.len() };
    }
    retain_cstring(text.as_bytes())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_FreeCString(_ctx: *mut JSContext, ptr: *const c_char) {
    release_cstring(ptr);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_ToInt32(
    _ctx: *mut JSContext,
    pres: *mut i32,
    value: abi::JSValue,
) -> i32 {
    let Some(value) = abi::jsvalue_to_value(value) else {
        return -1;
    };
    match crux::convert::to_number(&value) {
        Ok(number) => {
            if !pres.is_null() {
                // SAFETY: the caller passes a writable result slot or null.
                unsafe { *pres = crux::convert::to_int32(number) };
            }
            0
        }
        Err(_) => -1,
    }
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_ToNumber(_ctx: *mut JSContext, value: abi::JSValue) -> abi::JSValue {
    let Some(value) = abi::jsvalue_to_value(value) else {
        return abi::exception();
    };
    match crux::convert::to_number(&value) {
        Ok(number) => abi::float64(number),
        Err(_) => abi::exception(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_ToBool(_ctx: *mut JSContext, value: abi::JSValue) -> i32 {
    let Some(value) = abi::jsvalue_to_value(value) else {
        return -1;
    };
    i32::from(crux::convert::to_boolean(&value))
}
