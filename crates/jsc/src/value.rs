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
        Some(Value::Undefined) | None => JSType::Undefined,
        Some(Value::Null) => JSType::Null,
        Some(Value::Boolean(_)) => JSType::Boolean,
        Some(Value::Number(_)) => JSType::Number,
        Some(Value::String(_)) => JSType::String,
        Some(Value::Symbol(_)) => JSType::Symbol,
        Some(Value::BigInt(_)) => JSType::BigInt,
        Some(Value::Object(_)) | Some(Value::Function(_)) => JSType::Object,
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsUndefined(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| matches!(refs::ref_to_value(value), Some(Value::Undefined)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsNull(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| matches!(refs::ref_to_value(value), Some(Value::Null)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsBoolean(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| matches!(refs::ref_to_value(value), Some(Value::Boolean(_))))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsNumber(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| matches!(refs::ref_to_value(value), Some(Value::Number(_))))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsString(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| matches!(refs::ref_to_value(value), Some(Value::String(_))))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsSymbol(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| matches!(refs::ref_to_value(value), Some(Value::Symbol(_))))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsObject(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| {
        matches!(
            refs::ref_to_value(value),
            Some(Value::Object(_)) | Some(Value::Function(_))
        )
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueIsArray(_ctx: JSContextRef, value: JSValueRef) -> bool {
    crate::guard(|| {
        let Some(value) = refs::ref_to_value(value) else {
            return false;
        };
        match &value {
            Value::Object(object) => matches!(object.kind, crux::object::ObjectKind::Array),
            _ => false,
        }
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
                    .map(|value| matches!(value, Value::Boolean(true)))
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

/// JSValueUnprotect: release a primitive value ref early (objects stay
/// alive through the JS graph).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSValueUnprotect(_ctx: JSContextRef, value: JSValueRef) {
    crate::guard(|| refs::release_value_ref(value));
}
