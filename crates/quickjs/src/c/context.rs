//! Evaluation (`JS_Eval`).

use std::ffi::{CStr, c_char};

use crate::c::abi;
use crate::c::module;
use crate::c::runtime::{JSContext, context_api};

// `JS_Eval()` flags (quickjs.h).
const JS_EVAL_TYPE_MASK: i32 = 3;
const JS_EVAL_TYPE_MODULE: i32 = 1;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_Eval(
    ctx: *mut JSContext,
    input: *const c_char,
    input_len: usize,
    filename: *const c_char,
    eval_flags: i32,
) -> abi::JSValue {
    // A panic from a host callback (or anywhere in the engine) must not unwind
    // across the C boundary; `guard` contains it and poisons the isolate.
    ffi::guard(|| unsafe { eval_impl(ctx, input, input_len, filename, eval_flags) })
}

unsafe fn eval_impl(
    ctx: *mut JSContext,
    input: *const c_char,
    input_len: usize,
    filename: *const c_char,
    eval_flags: i32,
) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    // SAFETY: the header requires input[input_len] == '\0' and input_len bytes
    // readable.
    let bytes = unsafe { std::slice::from_raw_parts(input as *const u8, input_len) };
    let Ok(source) = std::str::from_utf8(bytes) else {
        return abi::exception();
    };
    let filename = if filename.is_null() {
        None
    } else {
        // SAFETY: the header requires a NUL-terminated filename when non-null.
        match unsafe { CStr::from_ptr(filename) }.to_str() {
            Ok(filename) => Some(filename),
            Err(_) => return abi::exception(),
        }
    };
    if (eval_flags & JS_EVAL_TYPE_MASK) == JS_EVAL_TYPE_MODULE {
        return module::eval_module(&api_ctx, ctx, filename, source, eval_flags);
    }
    match api_ctx.try_eval(source) {
        Ok(local) => abi::value_to_jsvalue(local.into_value()),
        Err(error) => {
            let thrown = api_ctx
                .with_agent(|agent| runtime::builtins::error::to_throwable(agent, &error))
                .unwrap_or(crux::value::Value::Undefined);
            unsafe { &*api_ctx.isolate() }.set_pending_exception(thrown);
            abi::exception()
        }
    }
}
