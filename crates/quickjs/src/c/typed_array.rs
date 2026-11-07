//! Typed arrays (`JS_NewUint8ArrayCopy`, `JS_GetTypedArrayBuffer`,
//! `JS_GetTypedArrayType`). A typed array is the engine's `ObjectKind::IntegerIndexed`
//! object over an ArrayBuffer; creation reuses the engine's own
//! `typed_array_buffer_path` constructor.

#[cfg(feature = "ng")]
use std::ffi::c_void;

#[cfg(feature = "ng")]
use crux::error::{ErrorKind, JsError};
use crux::object::ObjectKind;
#[cfg(feature = "ng")]
use crux::typed_array::{ElementType, SharedBuffer};
use runtime::api::{self, Local};
#[cfg(feature = "ng")]
use runtime::builtins::array_buffer::array_buffer_from_block;
#[cfg(feature = "ng")]
use runtime::builtins::typed_array::typed_array_buffer_path;

use crate::c::abi;
#[cfg(feature = "ng")]
use crate::c::buffer::JSReallocArrayBufferDataFunc;
use crate::c::module::set_exception;
use crate::c::runtime::{JSContext, context_api};

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewUint8ArrayCopy(
    ctx: *mut JSContext,
    buf: *const u8,
    len: usize,
) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    // SAFETY: the header requires `len` readable bytes from `buf`.
    let bytes = unsafe { std::slice::from_raw_parts(buf, len) };
    let result = api_ctx.with_agent(|agent| {
        let shared = SharedBuffer::new(len);
        shared.write(0, bytes)?;
        let buffer = array_buffer_from_block(agent, shared, len)?;
        let proto = agent
            .current_realm()?
            .intrinsics
            .get("%Uint8Array.prototype%")
            .and_then(|value| value.as_object())
            .ok_or_else(|| {
                JsError::new(
                    ErrorKind::TypeError,
                    "%Uint8Array.prototype% missing".into(),
                )
            })?;
        typed_array_buffer_path(agent, proto, ElementType::Uint8, &buffer, &[])
    });
    match result {
        Ok(value) => abi::value_to_jsvalue(value),
        Err(_) => abi::exception(),
    }
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewUint8Array(
    ctx: *mut JSContext,
    buf: *mut u8,
    len: usize,
    realloc_func: *const JSReallocArrayBufferDataFunc,
    _opaque: *mut c_void,
    _is_shared: bool,
) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    if buf.is_null() {
        return abi::exception();
    }
    if !realloc_func.is_null() {
        // Resizable external buffers are unsupported (gap G10).
        return abi::exception();
    }
    let result = api_ctx.with_agent(|agent| {
        // SAFETY: `buf` is `len` bytes the host owns and keeps valid for the
        // view's lifetime; no deleter because the host frees it.
        let shared = unsafe { SharedBuffer::borrowed(buf, len, None) };
        let buffer = array_buffer_from_block(agent, shared, len)?;
        let proto = agent
            .current_realm()?
            .intrinsics
            .get("%Uint8Array.prototype%")
            .and_then(|value| value.as_object())
            .ok_or_else(|| {
                JsError::new(
                    ErrorKind::TypeError,
                    "%Uint8Array.prototype% missing".into(),
                )
            })?;
        typed_array_buffer_path(agent, proto, ElementType::Uint8, &buffer, &[])
    });
    match result {
        Ok(value) => abi::value_to_jsvalue(value),
        Err(_) => abi::exception(),
    }
}

/// The global constructor name for a `JSTypedArrayEnum` value.
fn typed_array_ctor_name(array_type: i32) -> Option<&'static str> {
    match array_type {
        0 => Some("Uint8ClampedArray"),
        1 => Some("Int8Array"),
        2 => Some("Uint8Array"),
        3 => Some("Int16Array"),
        4 => Some("Uint16Array"),
        5 => Some("Int32Array"),
        6 => Some("Uint32Array"),
        7 => Some("BigInt64Array"),
        8 => Some("BigUint64Array"),
        9 => Some("Float16Array"),
        10 => Some("Float32Array"),
        11 => Some("Float64Array"),
        _ => None,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewTypedArray(
    ctx: *mut JSContext,
    argc: i32,
    argv: *const abi::JSValue,
    array_type: i32,
) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    if argc < 0 {
        return abi::exception();
    }
    let Some(name) = typed_array_ctor_name(array_type) else {
        return abi::exception();
    };
    let global = api_ctx.global();
    let constructor = match api::Object::get(&api_ctx, &global, name) {
        Ok(constructor) => constructor,
        Err(error) => return set_exception(&api_ctx, &error),
    };
    let mut args = Vec::with_capacity(argc as usize);
    if !argv.is_null() && argc > 0 {
        // SAFETY: the header requires `argc` readable entries from `argv`.
        let raw = unsafe { std::slice::from_raw_parts(argv, argc as usize) };
        for value in raw {
            let Some(value) = abi::jsvalue_to_value(*value) else {
                return abi::exception();
            };
            args.push(Local::from_value(value));
        }
    }
    match api_ctx.try_construct(&constructor, &args) {
        Ok(local) => abi::value_to_jsvalue(local.into_value()),
        Err(error) => set_exception(&api_ctx, &error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_GetTypedArrayBuffer(
    _ctx: *mut JSContext,
    obj: abi::JSValue,
    pbyte_offset: *mut usize,
    pbyte_length: *mut usize,
    pbytes_per_element: *mut usize,
) -> abi::JSValue {
    let Some(value) = abi::jsvalue_to_value(obj) else {
        return abi::exception();
    };
    let Some(object) = value.as_object() else {
        return abi::exception();
    };
    let ObjectKind::IntegerIndexed(slots) = &object.kind else {
        return abi::exception();
    };
    if !pbyte_offset.is_null() {
        // SAFETY: the caller passes a writable slot or null.
        unsafe { *pbyte_offset = slots.byte_offset };
    }
    if !pbyte_length.is_null() {
        unsafe { *pbyte_length = slots.byte_length };
    }
    if !pbytes_per_element.is_null() {
        unsafe { *pbytes_per_element = slots.element_type.size() };
    }
    abi::value_to_jsvalue(slots.buffer_object)
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_GetTypedArrayType(obj: abi::JSValue) -> i32 {
    let Some(value) = abi::jsvalue_to_value(obj) else {
        return -1;
    };
    let Some(object) = value.as_object() else {
        return -1;
    };
    let ObjectKind::IntegerIndexed(slots) = &object.kind else {
        return -1;
    };
    element_type_enum(slots.element_type)
}

#[cfg(feature = "ng")]
fn element_type_enum(element_type: ElementType) -> i32 {
    match element_type {
        ElementType::Uint8Clamped => 0,
        ElementType::Int8 => 1,
        ElementType::Uint8 => 2,
        ElementType::Int16 => 3,
        ElementType::Uint16 => 4,
        ElementType::Int32 => 5,
        ElementType::Uint32 => 6,
        ElementType::BigInt64 => 7,
        ElementType::BigUint64 => 8,
        ElementType::Float16 => 9,
        ElementType::Float32 => 10,
        ElementType::Float64 => 11,
    }
}
