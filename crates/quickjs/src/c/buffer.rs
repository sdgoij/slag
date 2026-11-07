//! ArrayBuffer creation, access, and detachment (`JS_NewArrayBufferCopy`,
//! `JS_NewArrayBuffer`, `JS_GetArrayBuffer`, `JS_IsArrayBuffer`,
//! `JS_DetachArrayBuffer`).

use std::ffi::c_void;

use crux::typed_array::SharedBuffer;
use crux::value::Value;
use runtime::builtins::array_buffer::{
    array_buffer_from_block, detach_array_buffer_with_key, shared_array_buffer_from_block,
};

use crate::c::abi;
#[cfg(feature = "ng")]
use crate::c::runtime::current;
use crate::c::runtime::{JSContext, JSRuntime, context_api};

/// The C `JSReallocArrayBufferDataFunc` (quickjs.h): resizable-buffer realloc.
pub type JSReallocArrayBufferDataFunc =
    unsafe extern "C" fn(*mut JSRuntime, *mut c_void, *mut c_void, usize) -> *mut c_void;

/// The C `JSFreeArrayBufferDataFunc` (Bellard quickjs.h): frees a host buffer.
#[cfg(feature = "bellard")]
pub type JSFreeArrayBufferDataFunc = unsafe extern "C" fn(*mut JSRuntime, *mut c_void, *mut c_void);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewArrayBufferCopy(
    ctx: *mut JSContext,
    buf: *const u8,
    len: usize,
) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    // SAFETY: the header requires `len` readable bytes from `buf`.
    let bytes = unsafe { std::slice::from_raw_parts(buf, len) };
    let shared = SharedBuffer::new(len);
    if shared.write(0, bytes).is_err() {
        return abi::exception();
    }
    match api_ctx.with_agent(|agent| array_buffer_from_block(agent, shared, len)) {
        Ok(value) => abi::value_to_jsvalue(value),
        Err(_) => abi::exception(),
    }
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewArrayBuffer(
    ctx: *mut JSContext,
    buf: *mut u8,
    len: usize,
    max_len: usize,
    realloc_func: *const JSReallocArrayBufferDataFunc,
    _opaque: *mut c_void,
    is_shared: bool,
) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    if buf.is_null() {
        return abi::exception();
    }
    if max_len != 0 || !realloc_func.is_null() {
        // Resizable external buffers are unsupported (gap G10); the fixed-size
        // borrow is the only path.
        return abi::exception();
    }
    // SAFETY: `buf` is `len` bytes the host owns and keeps valid for the
    // buffer's lifetime (QuickJS's external-buffer contract); no deleter
    // because the host frees it.
    let shared = unsafe { SharedBuffer::borrowed(buf, len, None) };
    let result = api_ctx.with_agent(|agent| {
        if is_shared {
            shared_array_buffer_from_block(agent, shared, len)
        } else {
            array_buffer_from_block(agent, shared, len)
        }
    });
    match result {
        Ok(value) => abi::value_to_jsvalue(value),
        Err(_) => abi::exception(),
    }
}

#[cfg(feature = "bellard")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_NewArrayBuffer(
    ctx: *mut JSContext,
    buf: *mut u8,
    len: usize,
    free_func: Option<JSFreeArrayBufferDataFunc>,
    opaque: *mut c_void,
    is_shared: i32,
) -> abi::JSValue {
    let Some(api_ctx) = context_api(ctx) else {
        return abi::exception();
    };
    if buf.is_null() {
        return abi::exception();
    }
    let rt = unsafe { crate::c::runtime::JS_GetRuntime(ctx) };
    let deleter = free_func.map(move |free_func| {
        Box::new(move || {
            // SAFETY: the free callback is the host's, called with the runtime
            // and the buffer it was registered with.
            unsafe { free_func(rt, opaque, buf as *mut c_void) };
        }) as Box<dyn FnOnce()>
    });
    // SAFETY: `buf` is `len` bytes the host owns until the deleter runs.
    let shared = unsafe { SharedBuffer::borrowed(buf, len, deleter) };
    let result = api_ctx.with_agent(|agent| {
        if is_shared != 0 {
            shared_array_buffer_from_block(agent, shared, len)
        } else {
            array_buffer_from_block(agent, shared, len)
        }
    });
    match result {
        Ok(value) => abi::value_to_jsvalue(value),
        Err(_) => abi::exception(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_GetArrayBuffer(
    ctx: *mut JSContext,
    psize: *mut usize,
    obj: abi::JSValue,
) -> *mut u8 {
    let Some(api_ctx) = context_api(ctx) else {
        return std::ptr::null_mut();
    };
    let Some(object) = abi::jsvalue_to_value(obj).and_then(|value| value.as_object()) else {
        return std::ptr::null_mut();
    };
    let id = object.id();
    let info = api_ctx.with_agent(|agent| {
        agent.buffer_state(id).and_then(|state| {
            if state.detached {
                None
            } else {
                Some((state.shared.data_ptr(), state.byte_length))
            }
        })
    });
    match info {
        Some((ptr, len)) => {
            if !psize.is_null() {
                // SAFETY: the caller passes a writable size slot or null.
                unsafe { *psize = len };
            }
            ptr
        }
        None => std::ptr::null_mut(),
    }
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_IsArrayBuffer(obj: abi::JSValue) -> bool {
    let Some(object) = abi::jsvalue_to_value(obj).and_then(|value| value.as_object()) else {
        return false;
    };
    let Some(api_ctx) = context_api(current()) else {
        return false;
    };
    api_ctx.with_agent(|agent| {
        agent
            .buffer_state(object.id())
            .is_some_and(|state| !state.is_shared)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_DetachArrayBuffer(ctx: *mut JSContext, obj: abi::JSValue) {
    let Some(api_ctx) = context_api(ctx) else {
        return;
    };
    let Some(object) = abi::jsvalue_to_value(obj).and_then(|value| value.as_object()) else {
        return;
    };
    let id = object.id();
    api_ctx.with_agent(|agent| {
        detach_array_buffer_with_key(agent, id, &Value::Undefined);
    });
}
