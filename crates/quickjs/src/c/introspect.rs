//! Value/object introspection (`JS_IsFunction`, `JS_IsConstructor`,
//! `JS_IsArray`, `JS_IsProxy`, `JS_GetVersion`, and the builtin-instance
//! predicates). The header's inline `JS_IsString`/`JS_IsNumber`/`JS_IsObject`/
//! etc. read the `JSValue` tag directly and need no exported symbol.

#[cfg(feature = "ng")]
use std::ffi::c_char;

use crux::object::ObjectKind;
use crux::value::{is_callable, is_constructor};

use crate::c::abi;
use crate::c::runtime::JSContext;
#[cfg(feature = "ng")]
use crate::c::runtime::{context_api, current};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_IsFunction(_ctx: *mut JSContext, val: abi::JSValue) -> abi::JS_BOOL {
    abi::js_bool(abi::jsvalue_to_value(val).is_some_and(|value| is_callable(&value)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_IsConstructor(_ctx: *mut JSContext, val: abi::JSValue) -> abi::JS_BOOL {
    abi::js_bool(abi::jsvalue_to_value(val).is_some_and(|value| is_constructor(&value)))
}

fn is_array_value(val: abi::JSValue) -> bool {
    abi::jsvalue_to_value(val)
        .and_then(|value| value.as_object())
        .is_some_and(|object| matches!(object.kind, ObjectKind::Array(_)))
}

// NG declares `JS_IsArray(JSValueConst)`; Bellard declares
// `JS_IsArray(JSContext *, JSValueConst)` and returns `int`.
#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_IsArray(val: abi::JSValue) -> bool {
    is_array_value(val)
}

#[cfg(feature = "bellard")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_IsArray(_ctx: *mut JSContext, val: abi::JSValue) -> abi::JS_BOOL {
    abi::js_bool(is_array_value(val))
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_IsProxy(val: abi::JSValue) -> bool {
    abi::jsvalue_to_value(val)
        .and_then(|value| value.as_object())
        .is_some_and(|object| matches!(object.kind, ObjectKind::Proxy(_)))
}

/// The object a value names, when it is one; the builtin-instance predicates
/// all brand-check by object id through the agent's per-type side tables.
fn object_of(val: abi::JSValue) -> Option<crux::handle::Handle<crux::object::JsObject>> {
    abi::jsvalue_to_value(val).and_then(|value| value.as_object())
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_IsDate(v: abi::JSValue) -> bool {
    let Some(object) = object_of(v) else {
        return false;
    };
    let Some(api_ctx) = context_api(current()) else {
        return false;
    };
    api_ctx.with_agent(|agent| agent.date_data.contains_key(&object.id()))
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_IsRegExp(val: abi::JSValue) -> bool {
    let Some(object) = object_of(val) else {
        return false;
    };
    let Some(api_ctx) = context_api(current()) else {
        return false;
    };
    api_ctx.with_agent(|agent| agent.regexp_state(object.id()).is_some())
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_IsMap(val: abi::JSValue) -> bool {
    let Some(object) = object_of(val) else {
        return false;
    };
    let Some(api_ctx) = context_api(current()) else {
        return false;
    };
    api_ctx.with_agent(|agent| agent.has_map(object.id()))
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_IsSet(val: abi::JSValue) -> bool {
    let Some(object) = object_of(val) else {
        return false;
    };
    let Some(api_ctx) = context_api(current()) else {
        return false;
    };
    api_ctx.with_agent(|agent| agent.has_set(object.id()))
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_IsDataView(val: abi::JSValue) -> bool {
    let Some(object) = object_of(val) else {
        return false;
    };
    let Some(api_ctx) = context_api(current()) else {
        return false;
    };
    api_ctx.with_agent(|agent| agent.dataview_state(object.id()).is_some())
}

#[cfg(feature = "ng")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn JS_GetVersion() -> *const c_char {
    c"0.18.0".as_ptr()
}
