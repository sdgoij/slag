//! Objects, functions, and property-name arrays (JSObjectRef.h,
//! JSPropertyNameArrayRef.h).

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr;

use crux::handle::Handle;
use crux::object::JsObject;
use crux::string::JsString;
use crux::value::Value;
use runtime::api::{self, Local};

use crate::class;
use crate::context::{JscContext, fill_exception, string_from_ref, string_ref};
use crate::refs;
use crate::{
    JSClassRef, JSContextRef, JSObjectCallAsFunctionCallback, JSObjectRef,
    JSPropertyNameAccumulatorRef, JSPropertyNameArrayRef, JSStringRef, JSValueRef,
};

thread_local! {
    /// Retained property-name arrays (JSPropertyNameArrayRef).
    static NAME_ARRAYS: RefCell<HashMap<usize, Vec<JsString>>> = RefCell::new(HashMap::new());
}

/// The object half of a ref, if it is an object value.
fn object_of(ctx: &JscContext, value: JSValueRef) -> Option<Handle<JsObject>> {
    let _ = ctx;
    refs::ref_to_value(value).and_then(|value| runtime::context::as_object(&value))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSObjectMake(
    ctx: JSContextRef,
    js_class: JSClassRef,
    data: *mut c_void,
) -> JSObjectRef {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return ptr::null_mut();
        };
        let object = ctx.with_current(|| class::make_object(ctx, js_class, data));
        refs::value_to_ref(Value::Object(object))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSObjectMakeFunctionWithCallback(
    ctx: JSContextRef,
    name: JSStringRef,
    call_as_function: Option<JSObjectCallAsFunctionCallback>,
) -> JSObjectRef {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return ptr::null_mut();
        };
        let Some(callback) = call_as_function else {
            return ptr::null_mut();
        };
        let name = string_from_ref(name).map(|name| name.to_string_lossy());
        let function = ctx.with_current(|| {
            class::make_function_with_callback(ctx, name.as_deref().map(String::from), callback)
        });
        refs::value_to_ref(Value::Function(function))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSObjectMakeArray(
    ctx: JSContextRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSObjectRef {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return ptr::null_mut();
        };
        let args = argv_to_locals(argument_count, arguments);
        ctx.with_current(|| match api::Array::new(&ctx.api, &args) {
            Ok(array) => refs::value_to_ref(array.into_value()),
            Err(error) => {
                fill_exception(ctx, &error, exception);
                ptr::null_mut()
            }
        })
    })
}

/// Construct an instance of a global constructor (Date, RegExp, Error).
fn make_via_global(
    ctx: &JscContext,
    global_name: &str,
    argument_count: usize,
    arguments: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSObjectRef {
    let constructor = match api::Object::get(&ctx.api, &ctx.api.global(), global_name) {
        Ok(constructor) => constructor,
        Err(error) => {
            fill_exception(ctx, &error, exception);
            return ptr::null_mut();
        }
    };
    let args = argv_to_locals(argument_count, arguments);
    match ctx.api.try_construct(&constructor, &args) {
        Ok(value) => refs::value_to_ref(value.into_value()),
        Err(error) => {
            fill_exception(ctx, &error, exception);
            ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSObjectMakeDate(
    ctx: JSContextRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSObjectRef {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return ptr::null_mut();
        };
        ctx.with_current(|| make_via_global(ctx, "Date", argument_count, arguments, exception))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSObjectMakeError(
    ctx: JSContextRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSObjectRef {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return ptr::null_mut();
        };
        ctx.with_current(|| make_via_global(ctx, "Error", argument_count, arguments, exception))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSObjectMakeRegExp(
    ctx: JSContextRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSObjectRef {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return ptr::null_mut();
        };
        ctx.with_current(|| make_via_global(ctx, "RegExp", argument_count, arguments, exception))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSObjectMakeFunction(
    ctx: JSContextRef,
    _name: JSStringRef,
    parameter_count: u32,
    parameter_names: *const JSStringRef,
    body: JSStringRef,
    _source_url: JSStringRef,
    _starting_line_number: i32,
    exception: *mut JSValueRef,
) -> JSObjectRef {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return ptr::null_mut();
        };
        let Some(body) = string_from_ref(body) else {
            return ptr::null_mut();
        };
        let params: Vec<String> = if parameter_names.is_null() {
            Vec::new()
        } else {
            (0..parameter_count)
                .filter_map(|index| {
                    string_from_ref(unsafe { *parameter_names.add(index as usize) })
                        .map(|name| name.to_string_lossy())
                })
                .collect()
        };
        let constructor = match api::Object::get(&ctx.api, &ctx.api.global(), "Function") {
            Ok(constructor) => constructor,
            Err(error) => {
                fill_exception(ctx, &error, exception);
                return ptr::null_mut();
            }
        };
        let params_text = params.join(", ");
        let args = [
            Local::string(params_text),
            Local::string(body.to_string_lossy()),
        ];
        ctx.with_current(|| match ctx.api.try_construct(&constructor, &args) {
            Ok(value) => refs::value_to_ref(value.into_value()),
            Err(error) => {
                fill_exception(ctx, &error, exception);
                ptr::null_mut()
            }
        })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSObjectGetPrototype(
    ctx: JSContextRef,
    object: JSObjectRef,
) -> JSValueRef {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return ptr::null_mut();
        };
        let Some(object) = object_of(ctx, object) else {
            return ptr::null_mut();
        };
        match ctx.api.with_agent(|_| object.get_prototype_of()) {
            Ok(Some(prototype)) => refs::value_to_ref(Value::Object(prototype)),
            Ok(None) => refs::value_to_ref(Value::Null),
            Err(_) => ptr::null_mut(),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSObjectSetPrototype(
    ctx: JSContextRef,
    object: JSObjectRef,
    value: JSValueRef,
) {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return;
        };
        let Some(object) = object_of(ctx, object) else {
            return;
        };
        let prototype = match refs::ref_to_value(value) {
            Some(Value::Object(object)) => Some(object),
            Some(Value::Null) => None,
            _ => return,
        };
        let _ = ctx.api.with_agent(|_| object.set_prototype_of(prototype));
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSObjectHasProperty(
    ctx: JSContextRef,
    object: JSObjectRef,
    property_name: JSStringRef,
) -> bool {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return false;
        };
        let Some(object) = object_of(ctx, object) else {
            return false;
        };
        let Some(name) = string_from_ref(property_name) else {
            return false;
        };
        ctx.with_current(|| {
            ctx.api
                .with_agent(|_| object.has_property(&name))
                .unwrap_or(false)
        })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSObjectGetProperty(
    ctx: JSContextRef,
    object: JSObjectRef,
    property_name: JSStringRef,
    exception: *mut JSValueRef,
) -> JSValueRef {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return ptr::null_mut();
        };
        let Some(object) = object_of(ctx, object) else {
            return ptr::null_mut();
        };
        let Some(name) = string_from_ref(property_name) else {
            return ptr::null_mut();
        };
        ctx.with_current(|| match ctx.api.with_agent(|_| object.get(&name)) {
            Ok(value) => refs::value_to_ref(value),
            Err(error) => {
                fill_exception(ctx, &error, exception);
                ptr::null_mut()
            }
        })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSObjectSetProperty(
    ctx: JSContextRef,
    object: JSObjectRef,
    property_name: JSStringRef,
    value: JSValueRef,
    _attributes: u32,
    exception: *mut JSValueRef,
) {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return;
        };
        let Some(object) = object_of(ctx, object) else {
            return;
        };
        let Some(name) = string_from_ref(property_name) else {
            return;
        };
        let Some(value) = refs::ref_to_value(value) else {
            return;
        };
        ctx.with_current(|| {
            if let Err(error) = ctx.api.with_agent(|_| object.set(&name, value, true)) {
                fill_exception(ctx, &error, exception);
            }
        })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSObjectDeleteProperty(
    ctx: JSContextRef,
    object: JSObjectRef,
    property_name: JSStringRef,
    exception: *mut JSValueRef,
) -> bool {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return false;
        };
        let Some(object) = object_of(ctx, object) else {
            return false;
        };
        let Some(name) = string_from_ref(property_name) else {
            return false;
        };
        ctx.with_current(|| match ctx.api.with_agent(|_| object.delete(&name)) {
            Ok(deleted) => deleted,
            Err(error) => {
                fill_exception(ctx, &error, exception);
                false
            }
        })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSObjectGetPropertyAtIndex(
    ctx: JSContextRef,
    object: JSObjectRef,
    property_index: u32,
    exception: *mut JSValueRef,
) -> JSValueRef {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return ptr::null_mut();
        };
        let Some(object) = object_of(ctx, object) else {
            return ptr::null_mut();
        };
        let key = JsString::from_utf8(&property_index.to_string());
        ctx.with_current(|| match ctx.api.with_agent(|_| object.get(&key)) {
            Ok(value) => refs::value_to_ref(value),
            Err(error) => {
                fill_exception(ctx, &error, exception);
                ptr::null_mut()
            }
        })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSObjectSetPropertyAtIndex(
    ctx: JSContextRef,
    object: JSObjectRef,
    property_index: u32,
    value: JSValueRef,
    exception: *mut JSValueRef,
) {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return;
        };
        let Some(object) = object_of(ctx, object) else {
            return;
        };
        let Some(value) = refs::ref_to_value(value) else {
            return;
        };
        let key = JsString::from_utf8(&property_index.to_string());
        ctx.with_current(|| {
            if let Err(error) = ctx.api.with_agent(|_| object.set(&key, value, true)) {
                fill_exception(ctx, &error, exception);
            }
        })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSObjectGetPrivate(object: JSObjectRef) -> *mut c_void {
    crate::guard(|| {
        let Some(object) = refs::ref_to_value(object).and_then(|value| value.as_object()) else {
            return ptr::null_mut();
        };
        class::get_private(object.id()).unwrap_or(0) as *mut c_void
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSObjectSetPrivate(object: JSObjectRef, data: *mut c_void) -> bool {
    crate::guard(|| {
        let Some(object) = refs::ref_to_value(object).and_then(|value| value.as_object()) else {
            return false;
        };
        class::set_private(object.id(), data as usize);
        true
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSObjectIsFunction(ctx: JSContextRef, object: JSObjectRef) -> bool {
    crate::guard(|| {
        let _ = ctx;
        refs::ref_to_value(object)
            .map(|value| crux::value::is_callable(&value))
            .unwrap_or(false)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSObjectIsConstructor(ctx: JSContextRef, object: JSObjectRef) -> bool {
    crate::guard(|| {
        let _ = ctx;
        refs::ref_to_value(object)
            .map(|value| crux::value::is_constructor(&value))
            .unwrap_or(false)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSObjectCallAsFunction(
    ctx: JSContextRef,
    object: JSObjectRef,
    this_object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSValueRef {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return ptr::null_mut();
        };
        let Some(function) = refs::ref_to_value(object) else {
            return ptr::null_mut();
        };
        let this = refs::ref_to_value(this_object).unwrap_or(Value::Undefined);
        let args = argv_to_locals(argument_count, arguments);
        ctx.with_current(|| {
            match ctx
                .api
                .try_call(&Local::from(function), &Local::from(this), &args)
            {
                Ok(value) => refs::value_to_ref(value.into_value()),
                Err(error) => {
                    fill_exception(ctx, &error, exception);
                    ptr::null_mut()
                }
            }
        })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSObjectCallAsConstructor(
    ctx: JSContextRef,
    object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSObjectRef {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return ptr::null_mut();
        };
        let Some(constructor) = refs::ref_to_value(object) else {
            return ptr::null_mut();
        };
        let args = argv_to_locals(argument_count, arguments);
        ctx.with_current(
            || match ctx.api.try_construct(&Local::from(constructor), &args) {
                Ok(value) => refs::value_to_ref(value.into_value()),
                Err(error) => {
                    fill_exception(ctx, &error, exception);
                    ptr::null_mut()
                }
            },
        )
    })
}

/// Materialize a `JSValueRef` argument array as locals (null refs become
/// *undefined*).
fn argv_to_locals(argument_count: usize, arguments: *const JSValueRef) -> Vec<Local> {
    if arguments.is_null() {
        return Vec::new();
    }
    (0..argument_count)
        .map(|index| {
            let arg = unsafe { *arguments.add(index) };
            Local::from(refs::ref_to_value(arg).unwrap_or(Value::Undefined))
        })
        .collect()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSObjectCopyPropertyNames(
    ctx: JSContextRef,
    object: JSObjectRef,
) -> JSPropertyNameArrayRef {
    crate::guard(|| {
        let Some(ctx) = JscContext::get(ctx as *mut JscContext) else {
            return ptr::null_mut();
        };
        let Some(object) = object_of(ctx, object) else {
            return ptr::null_mut();
        };
        let keys = ctx
            .with_current(|| ctx.api.with_agent(|_| object.own_property_keys()))
            .unwrap_or_default();
        let names: Vec<JsString> = keys
            .into_iter()
            .filter_map(|key| match key {
                crux::property::PropertyKey::String(id) => Some(crux::string::lookup(id)),
                crux::property::PropertyKey::Symbol(_) => None,
            })
            .collect();
        let id = retain_name_array(names);
        id as JSPropertyNameArrayRef
    })
}

fn retain_name_array(names: Vec<JsString>) -> usize {
    let id = NAMES.with(|next| {
        let id = next.get();
        next.set(id + 1);
        id
    });
    NAME_ARRAYS.with(|arrays| arrays.borrow_mut().insert(id, names));
    id
}

thread_local! {
    static NAMES: std::cell::Cell<usize> = const { std::cell::Cell::new(1) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSPropertyNameArrayRetain(
    array: JSPropertyNameArrayRef,
) -> JSPropertyNameArrayRef {
    crate::guard(|| {
        if NAME_ARRAYS.with(|arrays| arrays.borrow().contains_key(&(array as usize))) {
            array
        } else {
            ptr::null_mut()
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSPropertyNameArrayRelease(array: JSPropertyNameArrayRef) {
    crate::guard(|| {
        NAME_ARRAYS.with(|arrays| {
            arrays.borrow_mut().remove(&(array as usize));
        });
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSPropertyNameArrayGetCount(array: JSPropertyNameArrayRef) -> usize {
    crate::guard(|| {
        NAME_ARRAYS
            .with(|arrays| {
                arrays
                    .borrow()
                    .get(&(array as usize))
                    .map(|names| names.len())
            })
            .unwrap_or(0)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSPropertyNameArrayGetNameAtIndex(
    array: JSPropertyNameArrayRef,
    index: usize,
) -> JSStringRef {
    crate::guard(|| {
        let name = NAME_ARRAYS.with(|arrays| {
            arrays
                .borrow()
                .get(&(array as usize))
                .and_then(|names| names.get(index).cloned())
        });
        match name {
            Some(name) => string_ref(&name),
            None => ptr::null_mut(),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn JSPropertyNameAccumulatorAddName(
    accumulator: JSPropertyNameAccumulatorRef,
    property_name: JSStringRef,
) {
    crate::guard(|| {
        let Some(name) = string_from_ref(property_name) else {
            return;
        };
        class::property_names_of(accumulator).push(name);
    })
}
