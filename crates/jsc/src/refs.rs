//! Ref encoding: how opaque C refs map onto engine values.
//!
//! - `JSObjectRef` (and any `JSValueRef` that is an object or function) is
//!   the object's own stable id, tagged with the low bit — so pointer
//!   equality on refs is object identity, like JSC. The id resolves through
//!   a strongly-owned registry that pins the full value (a function's
//!   `function_self` link is weak, so the registry must hold the strong
//!   `Value::Function` or the function dies when the boundary value drops).
//! - primitive `JSValueRef`s are ids (tag 0) into the `ffi` value table.
//! - `JSStringRef`s are ids into the `ffi` string table.

use std::cell::RefCell;
use std::collections::HashMap;

use crux::handle::Handle;
use crux::object::JsObject;
use crux::value::Value;
use ffi::{release_value, retain_value, value};

use crate::{JSObjectRef, JSValueRef};

thread_local! {
    /// Object-id -> value, so tagged refs resolve while the host holds
    /// them. Strong: a ref handed to the host pins its value (like the
    /// value table pins primitives), so a freshly parsed object or a bare
    /// host function survives until the host releases it.
    static OBJECTS: RefCell<HashMap<u64, Value>> = RefCell::new(HashMap::new());
}

fn tagged(id: u64) -> JSValueRef {
    (id << 1 | 1) as usize as JSValueRef
}

fn untagged(id: u64) -> JSValueRef {
    (id << 1) as usize as JSValueRef
}

fn is_tagged(r: JSValueRef) -> bool {
    (r as usize) & 1 == 1
}

fn untag(r: JSValueRef) -> u64 {
    (r as usize >> 1) as u64
}

/// The object half of a value (objects and functions), for ref encoding.
fn object_handle(value: &Value) -> Option<Handle<JsObject>> {
    if let Some(object) = value.as_object() {
        return Some(object);
    }
    if let Some(function) = value.as_function() {
        return function.object.handle();
    }
    None
}

/// Pin `value` under its object's id so its tagged ref resolves later.
fn pin(value: &Value) {
    if let Some(object) = object_handle(value) {
        OBJECTS.with(|objects| objects.borrow_mut().insert(object.id(), value.clone()));
    }
}

/// Encode a value as a `JSValueRef`/`JSObjectRef`.
pub fn value_to_ref(value: Value) -> JSValueRef {
    match object_handle(&value) {
        Some(object) => {
            pin(&value);
            tagged(object.id())
        }
        None => untagged(retain_value(value)),
    }
}

/// Decode a `JSValueRef`/`JSObjectRef` into a value; `None` for a null ref
/// or one whose object has been collected.
pub fn ref_to_value(r: JSValueRef) -> Option<Value> {
    if r.is_null() {
        return None;
    }
    if is_tagged(r) {
        OBJECTS.with(|objects| objects.borrow().get(&untag(r)).cloned())
    } else {
        value(untag(r))
    }
}

/// The ref for an object handle (pins it as an object value).
pub fn object_ref(object: &Handle<JsObject>) -> JSObjectRef {
    let value = Value::Object(*object);
    pin(&value);
    tagged(object.id())
}

/// The ref for an already-registered object id (class callbacks, where the
/// object crossed the boundary at creation).
pub fn object_id_ref(id: u64) -> JSObjectRef {
    tagged(id)
}

/// The ref for the object half of a value (pins the full value, so a
/// function `this` stays callable), or null for primitives.
pub fn value_object_ref(value: &Value) -> JSObjectRef {
    match object_handle(value) {
        Some(object) => {
            pin(value);
            tagged(object.id())
        }
        None => std::ptr::null_mut(),
    }
}

/// Release a primitive value ref (a no-op for objects).
pub fn release_value_ref(r: JSValueRef) {
    if !r.is_null() && !is_tagged(r) {
        release_value(untag(r));
    }
}
