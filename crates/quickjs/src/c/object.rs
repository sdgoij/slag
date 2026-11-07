//! Object creation and property access (string- and atom-keyed).

use std::ffi::{CStr, c_char};

use runtime::api::{self, Local};

use crate::c::abi;
use crate::c::atom::atom_text;
use crate::c::runtime::{JSContext, context_api};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewObject(ctx: *mut JSContext) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    match api::Object::new(&api_ctx) {
        Ok(local) => abi::value_to_jsvalue(local.into_value()),
        Err(_) => abi::exception(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_GetGlobalObject(ctx: *mut JSContext) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    abi::value_to_jsvalue(api_ctx.global().into_value())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_GetPropertyStr(
    ctx: *mut JSContext,
    this_obj: abi::JSValue,
    prop: *const c_char,
) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    let Some(object) = abi::jsvalue_to_value(this_obj) else {
        return abi::exception();
    };
    let object = Local::from_value(object);
    // SAFETY: the header requires `prop` to be a NUL-terminated C string.
    let prop = unsafe { CStr::from_ptr(prop) };
    let Ok(prop) = prop.to_str() else {
        return abi::exception();
    };
    match api::Object::get(&api_ctx, &object, prop) {
        Ok(local) => abi::value_to_jsvalue(local.into_value()),
        Err(_) => abi::exception(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_SetPropertyStr(
    ctx: *mut JSContext,
    this_obj: abi::JSValue,
    prop: *const c_char,
    value: abi::JSValue,
) -> i32 {
    let Some(api_ctx) = context_api(ctx) else {
        return -1;
    };
    let Some(object) = abi::jsvalue_to_value(this_obj) else {
        return -1;
    };
    let object = Local::from_value(object);
    let Some(value) = abi::jsvalue_to_value(value) else {
        return -1;
    };
    let value = Local::from_value(value);
    // SAFETY: the header requires `prop` to be a NUL-terminated C string.
    let prop = unsafe { CStr::from_ptr(prop) };
    let Ok(prop) = prop.to_str() else {
        return -1;
    };
    match api::Object::set(&api_ctx, &object, prop, &value, false) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_GetProperty(
    ctx: *mut JSContext,
    this_obj: abi::JSValue,
    prop: u32,
) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    let Some(text) = atom_text(prop) else {
        return abi::exception();
    };
    let Some(object) = abi::jsvalue_to_value(this_obj) else {
        return abi::exception();
    };
    let object = Local::from_value(object);
    let key = text.to_string_lossy();
    match api::Object::get(&api_ctx, &object, &key) {
        Ok(local) => abi::value_to_jsvalue(local.into_value()),
        Err(_) => abi::exception(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_SetProperty(
    ctx: *mut JSContext,
    this_obj: abi::JSValue,
    prop: u32,
    value: abi::JSValue,
) -> i32 {
    let Some(api_ctx) = context_api(ctx) else {
        return -1;
    };
    let Some(text) = atom_text(prop) else {
        return -1;
    };
    let Some(object) = abi::jsvalue_to_value(this_obj) else {
        return -1;
    };
    let object = Local::from_value(object);
    let Some(value) = abi::jsvalue_to_value(value) else {
        return -1;
    };
    let value = Local::from_value(value);
    let key = text.to_string_lossy();
    match api::Object::set(&api_ctx, &object, &key, &value, false) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_GetPropertyUint32(
    ctx: *mut JSContext,
    this_obj: abi::JSValue,
    idx: u32,
) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    let Some(object) = abi::jsvalue_to_value(this_obj) else {
        return abi::exception();
    };
    let object = Local::from_value(object);
    let key = idx.to_string();
    match api::Object::get(&api_ctx, &object, &key) {
        Ok(local) => abi::value_to_jsvalue(local.into_value()),
        Err(_) => abi::exception(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_SetPropertyUint32(
    ctx: *mut JSContext,
    this_obj: abi::JSValue,
    idx: u32,
    value: abi::JSValue,
) -> i32 {
    let Some(api_ctx) = context_api(ctx) else {
        return -1;
    };
    let Some(object) = abi::jsvalue_to_value(this_obj) else {
        return -1;
    };
    let object = Local::from_value(object);
    let Some(value) = abi::jsvalue_to_value(value) else {
        return -1;
    };
    let value = Local::from_value(value);
    let key = idx.to_string();
    match api::Object::set(&api_ctx, &object, &key, &value, false) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}
