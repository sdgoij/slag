//! Pending exceptions (`JS_Throw`, `JS_GetException`, `JS_HasException`).

use crate::c::abi;
use crate::c::runtime::{JSContext, context_api};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_Throw(ctx: *mut JSContext, obj: abi::JSValue) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    let Some(value) = abi::jsvalue_to_value(obj) else {
        return abi::exception();
    };
    unsafe { &*api_ctx.isolate() }.set_pending_exception(value);
    abi::exception()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_GetException(ctx: *mut JSContext) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::null();
    };
    match unsafe { &*api_ctx.isolate() }.take_pending_exception() {
        Some(value) => abi::value_to_jsvalue(value),
        None => abi::null(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_HasException(ctx: *mut JSContext) -> abi::JS_BOOL {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::js_bool(false);
    };
    abi::js_bool(unsafe { &*api_ctx.isolate() }.has_pending_exception())
}
