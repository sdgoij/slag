//! Host jobs (`JS_EnqueueJob`).

use crux::error::{ErrorKind, JsError};

use crate::c::abi;
use crate::c::runtime::{JSContext, context_api};

/// The C `JSJobFunc` (quickjs.h): `(ctx, argc, argv) -> JSValue`.
pub type JSJobFunc = unsafe extern "C" fn(*mut JSContext, i32, *const abi::JSValue) -> abi::JSValue;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_EnqueueJob(
    ctx: *mut JSContext,
    job_func: Option<JSJobFunc>,
    argc: i32,
    argv: *const abi::JSValue,
) -> i32 {
    let Some(api_ctx) = context_api(ctx) else {
        return -1;
    };
    let Some(job_func) = job_func else {
        return -1;
    };
    if argc < 0 || (argc > 0 && argv.is_null()) {
        return -1;
    }
    let argc = argc as usize;
    api_ctx.with_agent(|agent| {
        // Dup the arguments: the host owns `argv` and may free it once this
        // returns; the job releases them when it runs.
        let args: Vec<abi::JSValue> = if argc == 0 {
            Vec::new()
        } else {
            // SAFETY: the header requires `argc` readable JSValues from `argv`.
            unsafe { std::slice::from_raw_parts(argv, argc) }
                .iter()
                .map(|arg| abi::dup_jsvalue(*arg))
                .collect()
        };
        agent.enqueue_generic_job(None, move |_agent| {
            // SAFETY: `ctx` names a live context; the args are valid for the
            // callback and freed after it returns.
            let result = unsafe { job_func(ctx, args.len() as i32, args.as_ptr()) };
            let converted = abi::jsvalue_to_value(result);
            abi::free_jsvalue(result);
            for arg in &args {
                abi::free_jsvalue(*arg);
            }
            match converted {
                Some(value) => Ok(value),
                None => Err(JsError::new(ErrorKind::Error, "job threw".into())),
            }
        });
    });
    0
}
