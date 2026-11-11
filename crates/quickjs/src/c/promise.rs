//! Promise helpers (`JS_NewSettledPromise`, `JS_IsPromise`, `JS_PromiseState`,
//! `JS_PromiseResult`). Resolution/rejection go through the global `Promise`
//! built-ins; state and result read the agent's promise table.

use runtime::api::{self, Local};

use crate::c::abi;
use crate::c::module::set_exception;
#[cfg(feature = "ng")]
use crate::c::runtime::current;
use crate::c::runtime::{JSContext, context_api};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewPromiseCapability(
    ctx: *mut JSContext,
    resolving_funcs: *mut abi::JSValue,
) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    let global = api_ctx.global();
    let constructor = match api::Object::get(&api_ctx, &global, "Promise") {
        Ok(constructor) => constructor.into_value(),
        Err(error) => return set_exception(&api_ctx, &error),
    };
    let capability =
        api_ctx.with_agent(|agent| runtime::promise::new_promise_capability(agent, &constructor));
    let capability = match capability {
        Ok(capability) => capability,
        Err(error) => return set_exception(&api_ctx, &error),
    };
    if !resolving_funcs.is_null() {
        // SAFETY: the header requires a two-slot writable array here.
        unsafe {
            *resolving_funcs = abi::value_to_jsvalue(capability.resolve);
            *resolving_funcs.add(1) = abi::value_to_jsvalue(capability.reject);
        }
    }
    abi::value_to_jsvalue(capability.promise)
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewSettledPromise(
    ctx: *mut JSContext,
    is_reject: bool,
    value: abi::JSValue,
) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    let Some(value) = abi::jsvalue_to_value(value) else {
        return abi::exception();
    };
    let value = Local::from_value(value);
    let result = if is_reject {
        api::Promise::reject(&api_ctx, &value)
    } else {
        api::Promise::resolve(&api_ctx, &value)
    };
    match result {
        Ok(local) => abi::value_to_jsvalue(local.into_value()),
        Err(_) => abi::exception(),
    }
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_IsPromise(val: abi::JSValue) -> bool {
    let Some(object) = abi::jsvalue_to_value(val).and_then(|value| value.as_object()) else {
        return false;
    };
    let Some(api_ctx) = context_api(current()) else {
        return false;
    };
    api_ctx.with_agent(|agent| agent.promise_data(&object).is_some())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_PromiseState(ctx: *mut JSContext, promise: abi::JSValue) -> i32 {
    let Some(api_ctx) = context_api(ctx) else {
        return -1;
    };
    let Some(value) = abi::jsvalue_to_value(promise) else {
        return -1;
    };
    let promise = Local::from_value(value);
    match api::Promise::state(&api_ctx, &promise) {
        Ok("pending") => 0,
        Ok("fulfilled") => 1,
        Ok("rejected") => 2,
        _ => -1,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_PromiseResult(
    ctx: *mut JSContext,
    promise: abi::JSValue,
) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    let Some(value) = abi::jsvalue_to_value(promise) else {
        return abi::exception();
    };
    let promise = Local::from_value(value);
    match api::Promise::result(&api_ctx, &promise) {
        Ok(local) => abi::value_to_jsvalue(local.into_value()),
        Err(_) => abi::exception(),
    }
}
