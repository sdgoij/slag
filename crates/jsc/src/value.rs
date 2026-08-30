//! Values (`JSValueRef`): type queries, constructors, and conversions.

use std::ptr;

use crux::string::JsString;
use crux::value::Value;

use crate::context::{JscContext, fill_exception, string_from_ref, string_ref};
use crate::refs;
use crate::{JSClassRef, JSContextRef, JSObjectRef, JSStringRef, JSType, JSValueRef};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueGetType(_ctx: JSContextRef, value: JSValueRef) -> JSType {
    crate::guard(|| match refs::ref_to_value(value) {
        None => JSType::Undefined,
        Some(v) if v.is_undefined() => JSType::Undefined,
        Some(v) if v.is_null() => JSType::Null,
        Some(v) if v.is_boolean() => JSType::Boolean,
        Some(v) if v.is_number() => JSType::Number,
        Some(v) if v.is_string() => JSType::String,
        Some(v) if v.is_symbol() => JSType::Symbol,
        Some(v) if v.is_bigint() => JSType::BigInt,
        Some(_) => JSType::Object,
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsUndefined(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| refs::ref_to_value(value).is_some_and(|v| v.is_undefined()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsNull(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| refs::ref_to_value(value).is_some_and(|v| v.is_null()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsBoolean(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| refs::ref_to_value(value).is_some_and(|v| v.is_boolean()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsNumber(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| refs::ref_to_value(value).is_some_and(|v| v.is_number()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsString(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| refs::ref_to_value(value).is_some_and(|v| v.is_string()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsSymbol(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| refs::ref_to_value(value).is_some_and(|v| v.is_symbol()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsObject(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| refs::ref_to_value(value).is_some_and(|v| v.is_object() || v.is_function()))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsArray(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| {
        let Some(value) = refs::ref_to_value(value) else {
            return false;
        };
        value
            .as_object()
            .is_some_and(|object| matches!(object.kind, crux::object::ObjectKind::Array))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsDate(ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return false;
        };
        let Some(value) = refs::ref_to_value(value) else {
            return false;
        };
        let Some(object) = value.as_object() else {
            return false;
        };
        ctx.api
            .with_agent(|agent| agent.date_data.contains_key(&object.id()))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsObjectOfClass(
    _ctx: JSContextRef,
    value: JSValueRef,
    js_class: JSClassRef,
) -> bool {
    crate::guard(|| {
        if js_class.is_null() {
            return false;
        }
        let Some(value) = refs::ref_to_value(value) else {
            return false;
        };
        let Some(object) = value.as_object() else {
            return false;
        };
        matches!(&object.kind, crux::object::ObjectKind::Host(ops) if std::rc::Rc::ptr_eq(ops, &crate::class::class_ops(js_class)))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsEqual(
    ctx: JSContextRef,
    a: JSValueRef,
    b: JSValueRef,
    exception: *mut JSValueRef,
) -> bool {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return false;
        };
        let Some(a) = refs::ref_to_value(a) else {
            return false;
        };
        let Some(b) = refs::ref_to_value(b) else {
            return false;
        };
        ctx.with_current(|| {
            let result = ctx
                .api
                .with_agent(|_agent| crux::ops::is_loosely_equal(&a, &b));
            match result {
                Ok(equal) => equal,
                Err(error) => {
                    fill_exception(ctx, &error, exception);
                    false
                }
            }
        })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsStrictEqual(
    _ctx: JSContextRef,
    a: JSValueRef,
    b: JSValueRef,
) -> bool {
    crate::guard(|| {
        let (Some(a), Some(b)) = (refs::ref_to_value(a), refs::ref_to_value(b)) else {
            return false;
        };
        crux::ops::is_strictly_equal(&a, &b)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsInstanceOfConstructor(
    ctx: JSContextRef,
    value: JSValueRef,
    constructor: JSObjectRef,
    exception: *mut JSValueRef,
) -> bool {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return false;
        };
        let Some(value) = refs::ref_to_value(value) else {
            return false;
        };
        let Some(constructor) = refs::ref_to_value(constructor) else {
            return false;
        };
        ctx.with_current(|| {
            let result = ctx.api.with_agent(|agent| {
                runtime::expr::ordinary_has_instance(agent, &constructor, &value)
                    .map(|value| value.as_boolean() == Some(true))
            });
            match result {
                Ok(is_instance) => is_instance,
                Err(error) => {
                    fill_exception(ctx, &error, exception);
                    false
                }
            }
        })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueMakeUndefined(_ctx: JSContextRef) -> JSValueRef {
    crate::guard(|| refs::value_to_ref(Value::Undefined))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueMakeNull(_ctx: JSContextRef) -> JSValueRef {
    crate::guard(|| refs::value_to_ref(Value::Null))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueMakeBoolean(_ctx: JSContextRef, boolean: bool) -> JSValueRef {
    crate::guard(|| refs::value_to_ref(Value::Boolean(boolean)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueMakeNumber(_ctx: JSContextRef, number: f64) -> JSValueRef {
    crate::guard(|| refs::value_to_ref(Value::Number(number)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueMakeString(_ctx: JSContextRef, string: JSStringRef) -> JSValueRef {
    crate::guard(|| match string_from_ref(string) {
        Some(string) => refs::value_to_ref(Value::String(crux::handle::Handle::new(string))),
        None => ptr::null_mut(),
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueMakeSymbol(
    _ctx: JSContextRef,
    description: JSStringRef,
) -> JSValueRef {
    crate::guard(|| {
        let description = string_from_ref(description).map(|string| string.to_string_lossy());
        let symbol = crux::symbol::Symbol::new(description.map(|d| JsString::from_utf8(&d)));
        refs::value_to_ref(Value::Symbol(crux::handle::Handle::new(symbol)))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueMakeFromJSONString(
    ctx: JSContextRef,
    string: JSStringRef,
) -> JSValueRef {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return ptr::null_mut();
        };
        let Some(text) = string_from_ref(string) else {
            return ptr::null_mut();
        };
        ctx.with_current(
            || match runtime::api::Json::parse(&ctx.api, &text.to_string_lossy()) {
                Ok(value) => refs::value_to_ref(value.into_value()),
                Err(_) => ptr::null_mut(),
            },
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueCreateJSONString(
    ctx: JSContextRef,
    value: JSValueRef,
    _indent: u32,
    exception: *mut JSValueRef,
) -> JSStringRef {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return ptr::null_mut();
        };
        let Some(value) = refs::ref_to_value(value) else {
            return ptr::null_mut();
        };
        let local = runtime::api::Local::from(value);
        ctx.with_current(|| match runtime::api::Json::stringify(&ctx.api, &local) {
            Ok(text) => match text.as_string() {
                Some(text) => string_ref(&JsString::from_utf8(&text)),
                None => ptr::null_mut(),
            },
            Err(error) => {
                fill_exception(ctx, &error, exception);
                ptr::null_mut()
            }
        })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueToBoolean(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| {
        let Some(value) = refs::ref_to_value(value) else {
            return false;
        };
        crux::convert::to_boolean(&value)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueToNumber(
    ctx: JSContextRef,
    value: JSValueRef,
    exception: *mut JSValueRef,
) -> f64 {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return 0.0;
        };
        let Some(value) = refs::ref_to_value(value) else {
            return 0.0;
        };
        ctx.with_current(|| {
            match ctx
                .api
                .with_agent(|agent| runtime::context::to_number(agent, &value))
            {
                Ok(number) => number,
                Err(error) => {
                    fill_exception(ctx, &error, exception);
                    0.0
                }
            }
        })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueToStringCopy(
    ctx: JSContextRef,
    value: JSValueRef,
    exception: *mut JSValueRef,
) -> JSStringRef {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return ptr::null_mut();
        };
        let Some(value) = refs::ref_to_value(value) else {
            return ptr::null_mut();
        };
        ctx.with_current(|| {
            match ctx
                .api
                .with_agent(|agent| runtime::context::to_string(agent, &value))
            {
                Ok(string) => string_ref(&string),
                Err(error) => {
                    fill_exception(ctx, &error, exception);
                    ptr::null_mut()
                }
            }
        })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueToObject(
    ctx: JSContextRef,
    value: JSValueRef,
    exception: *mut JSValueRef,
) -> JSObjectRef {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return ptr::null_mut();
        };
        let Some(value) = refs::ref_to_value(value) else {
            return ptr::null_mut();
        };
        ctx.with_current(|| {
            match ctx
                .api
                .with_agent(|agent| runtime::context::to_object(agent, &value))
            {
                Ok(object) => refs::value_to_ref(object),
                Err(error) => {
                    fill_exception(ctx, &error, exception);
                    ptr::null_mut()
                }
            }
        })
    })
}

/// JSValueProtect: values handed to the host are already strongly held, so
/// this is a no-op (divergence: refs never dangle).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueProtect(_ctx: JSContextRef, _value: JSValueRef) {
    crate::guard(|| {})
}

/// JSValueToStringWithContext: converts a JSValue to a string in the
/// given context. Divergence from real JSC: the `context` parameter is
/// currently ignored (slag uses a single context per isolate); the result
/// is identical to JSValueToStringCopy.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueToStringWithContext(
    ctx: JSContextRef,
    value: JSValueRef,
    _context: JSContextRef,
    exception: *mut JSValueRef,
) -> JSStringRef {
    // Delegate to JSValueToStringCopy — context is a no-op in slag.
    unsafe { JSValueToStringCopy(ctx, value, exception) }
}

/// JSValueUnprotect: release a primitive value ref early (objects stay
/// alive through the JS graph).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueUnprotect(_ctx: JSContextRef, value: JSValueRef) {
    crate::guard(|| refs::release_value_ref(value));
}

// ═══════════════════════════════════════════════════════════════════════════
// TypedArray stubs (Phase 3, Step 3.2 — medium priority)
// ═══════════════════════════════════════════════════════════════════════════
/// JSValueIsTypedArray: stub — returns false (slag has no TypedArray support yet).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsTypedArray(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| refs::ref_to_value(value).is_some_and(|v| v.is_undefined()))
}

/// JSValueIsArrayBuffer: stub — returns false.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsArrayBuffer(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| refs::ref_to_value(value).is_some_and(|v| v.is_undefined()))
}

/// JSValueIsDataView: stub — returns false.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsDataView(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| refs::ref_to_value(value).is_some_and(|v| v.is_undefined()))
}

/// JSValueIsInt8Array: stub — returns false.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsInt8Array(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| refs::ref_to_value(value).is_some_and(|v| v.is_undefined()))
}

/// JSValueIsUint8Array: stub — returns false.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsUint8Array(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| refs::ref_to_value(value).is_some_and(|v| v.is_undefined()))
}

/// JSValueIsUint8ClampedArray: stub — returns false.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsUint8ClampedArray(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| refs::ref_to_value(value).is_some_and(|v| v.is_undefined()))
}

/// JSValueIsInt16Array: stub — returns false.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsInt16Array(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| refs::ref_to_value(value).is_some_and(|v| v.is_undefined()))
}

/// JSValueIsUint16Array: stub — returns false.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsUint16Array(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| refs::ref_to_value(value).is_some_and(|v| v.is_undefined()))
}

/// JSValueIsInt32Array: stub — returns false.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsInt32Array(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| refs::ref_to_value(value).is_some_and(|v| v.is_undefined()))
}

/// JSValueIsUint32Array: stub — returns false.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsUint32Array(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| refs::ref_to_value(value).is_some_and(|v| v.is_undefined()))
}

/// JSValueIsFloat32Array: stub — returns false.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsFloat32Array(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| refs::ref_to_value(value).is_some_and(|v| v.is_undefined()))
}

/// JSValueIsFloat64Array: stub — returns false.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsFloat64Array(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| refs::ref_to_value(value).is_some_and(|v| v.is_undefined()))
}

/// JSValueIsBigInt64Array: stub — returns false.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsBigInt64Array(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| refs::ref_to_value(value).is_some_and(|v| v.is_undefined()))
}

/// JSValueIsBigUint64Array: stub — returns false.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsBigUint64Array(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| refs::ref_to_value(value).is_some_and(|v| v.is_undefined()))
}

/// JSValueGetUint8: stub — throws TypeError.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueGetUint8(
    _ctx: JSContextRef,
    value: JSValueRef,
    _index: usize,
) -> u8 {
    crate::guard(|| {
        refs::ref_to_value(value).is_some_and(|v| v.is_object() || v.is_function()) as u8
    })
}

/// JSValueSetInt8: stub — no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueSetInt8(
    _ctx: JSContextRef,
    _value: JSValueRef,
    _index: usize,
    _value_i8: i8,
) {
    crate::guard(|| {})
}

/// JSValueGetByteLength: stub — returns 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueGetByteLength(_ctx: JSContextRef, _value: JSValueRef) -> usize {
    0
}

/// JSValueGetByteOffset: stub — returns 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueGetByteOffset(_ctx: JSContextRef, _value: JSValueRef) -> usize {
    0
}

/// JSValueGetLength: stub — returns 0 for non-array objects.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueGetLength(_ctx: JSContextRef, value: JSValueRef) -> usize {
    crate::guard(|| {
        let Some(value) = refs::ref_to_value(value) else {
            return 0;
        };
        // Arrays have a .length property; objects don't.
        if value.is_object()
            && let Some(obj) = value.as_object()
            && matches!(obj.kind, crux::object::ObjectKind::Array)
        {
            return 0; // Stub: real length from JS would go here.
        }
        0
    })
}

/// JSValueSetLength: stub — no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueSetLength(_ctx: JSContextRef, _value: JSValueRef, _length: usize) {
    crate::guard(|| {})
}
