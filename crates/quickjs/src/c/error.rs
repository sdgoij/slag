//! Error constructors and throwers. The variadic `JS_New*Error` / `JS_Throw*Error`
//! functions live in `c/shim/variadic.c` and call [`qjs_new_error`] /
//! [`qjs_throw_error`]; the non-variadic ones are here.

use std::ffi::{CStr, c_char};

use crux::error::{ErrorKind, JsError};

use crate::c::abi;
#[cfg(feature = "ng")]
use crate::c::runtime::current;
use crate::c::runtime::{JSContext, context_api};

fn kind_from_c(kind: i32) -> ErrorKind {
    match kind {
        2 => ErrorKind::RangeError,
        3 => ErrorKind::ReferenceError,
        4 => ErrorKind::SyntaxError,
        5 => ErrorKind::TypeError,
        _ => ErrorKind::Error,
    }
}

fn make_error(
    api_ctx: runtime::api::Context,
    kind: ErrorKind,
    message: String,
) -> Option<crux::value::Value> {
    let error = JsError::new(kind, message);
    api_ctx
        .with_agent(|agent| runtime::builtins::error::to_throwable(agent, &error))
        .ok()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn qjs_new_error(
    ctx: *mut JSContext,
    kind: i32,
    message: *const c_char,
) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    // SAFETY: the shim passes a NUL-terminated string.
    let message = unsafe { CStr::from_ptr(message) }
        .to_string_lossy()
        .into_owned();
    match make_error(api_ctx, kind_from_c(kind), message) {
        Some(value) => abi::value_to_jsvalue(value),
        None => abi::exception(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn qjs_throw_error(ctx: *mut JSContext, kind: i32, message: *const c_char) {
    let Some(api_ctx) = context_api(ctx) else {
        return;
    };
    // SAFETY: the shim passes a NUL-terminated string.
    let message = unsafe { CStr::from_ptr(message) }
        .to_string_lossy()
        .into_owned();
    if let Some(value) = make_error(api_ctx, kind_from_c(kind), message) {
        unsafe { &*api_ctx.isolate() }.set_pending_exception(value);
    }
}

fn is_error_value(value: abi::JSValue, api_ctx: runtime::api::Context) -> bool {
    let Some(value) = abi::jsvalue_to_value(value) else {
        return false;
    };
    // QuickJS reads the object header; this engine has no per-object error
    // slot, so the agent's error slot set is the brand.
    api_ctx.with_agent(|agent| runtime::builtins::error::is_error(agent, value))
}

// NG declares `JS_IsError(JSValueConst)`; Bellard declares
// `JS_IsError(JSContext *, JSValueConst)` and returns `JS_BOOL`.
#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_IsError(value: abi::JSValue) -> bool {
    let Some(api_ctx) = context_api(current()) else {
        return false;
    };
    is_error_value(value, api_ctx)
}

#[cfg(feature = "bellard")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_IsError(ctx: *mut JSContext, value: abi::JSValue) -> abi::JS_BOOL {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::js_bool(false);
    };
    abi::js_bool(is_error_value(value, api_ctx))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewError(ctx: *mut JSContext) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    match make_error(api_ctx, ErrorKind::Error, String::new()) {
        Some(value) => abi::value_to_jsvalue(value),
        None => abi::exception(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_ThrowOutOfMemory(ctx: *mut JSContext) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    if let Some(value) = make_error(api_ctx, ErrorKind::Error, "out of memory".to_string()) {
        unsafe { &*api_ctx.isolate() }.set_pending_exception(value);
    }
    abi::exception()
}
