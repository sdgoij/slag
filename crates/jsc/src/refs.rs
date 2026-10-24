//! Ref encoding: how opaque C refs map onto engine values.
//!
//! - `JSObjectRef` (and any `JSValueRef` that is an object or function) is
//!   the object's own stable id, tagged `01` — so pointer equality on refs is
//!   object identity, like JSC. The id resolves through a strongly-owned
//!   registry that pins the full value (a function's `function_self` link is
//!   weak, so the registry must hold the strong `Value::Function` or the
//!   function dies when the boundary value drops).
//! - primitive `JSValueRef`s are `00`-tagged ids into the `ffi` value table.
//! - `JSStringRef`s are `10`-tagged ids into the `ffi` string table.
//!
//! The tag is what keeps the three tables apart. JSC's C types are distinct
//! pointers, but at the ABI level they are all `void *`, so a host that passes
//! a `JSStringRef` where a `JSValueRef` is expected compiles; here it decodes
//! to nothing instead of to whatever occupies that id in another table.

use std::cell::RefCell;
use std::collections::HashMap;

use crux::handle::Handle;
use crux::object::JsObject;
use crux::value::Value;
use ffi::{release_value, retain_value, retain_value_ref, value};

use crate::{JSObjectRef, JSStringRef, JSValueRef};

thread_local! {
    /// Object-id -> value, so tagged refs resolve while the host holds
    /// them. Strong: a ref handed to the host pins its value (like the
    /// value table pins primitives), so a freshly parsed object or a bare
    /// host function survives until the host releases it.
    static OBJECTS: RefCell<HashMap<u64, Value>> = RefCell::new(HashMap::new());
}

/// The ref-kind tag in the low two bits (the id occupies the rest).
const TAG_MASK: u64 = 0b11;
const TAG_VALUE: u64 = 0b00;
const TAG_OBJECT: u64 = 0b01;
const TAG_STRING: u64 = 0b10;

fn encode(id: u64, tag: u64) -> usize {
    ((id << 2) | tag) as usize
}

/// The (id, tag) behind a non-null ref.
fn decode(ptr: usize) -> (u64, u64) {
    let bits = ptr as u64;
    (bits >> 2, bits & TAG_MASK)
}

fn tagged(id: u64) -> JSValueRef {
    encode(id, TAG_OBJECT) as JSValueRef
}

fn untagged(id: u64) -> JSValueRef {
    encode(id, TAG_VALUE) as JSValueRef
}

/// The ref for a string-table id.
pub(crate) fn string_ref_from_id(id: u64) -> JSStringRef {
    encode(id, TAG_STRING) as JSStringRef
}

/// The string-table id behind a `JSStringRef`, or `None` for a null ref or
/// one of another kind.
pub(crate) fn string_ref_id(r: JSStringRef) -> Option<u64> {
    if r.is_null() {
        return None;
    }
    let (id, tag) = decode(r as usize);
    (tag == TAG_STRING).then_some(id)
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
        OBJECTS.with(|objects| objects.borrow_mut().insert(object.id(), *value));
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
    let (id, tag) = decode(r as usize);
    match tag {
        TAG_OBJECT => OBJECTS.with(|objects| objects.borrow().get(&id).cloned()),
        TAG_VALUE => value(id),
        // A string ref is not a value ref.
        _ => None,
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

/// The object id behind a tagged ref, even after the object has been swept.
/// A finalize callback receives such a ref and may read the private-data
/// slot, which is keyed by id rather than by the (now dead) value.
pub fn object_id(r: JSValueRef) -> Option<u64> {
    if r.is_null() {
        return None;
    }
    // Object values are always handed out as `TAG_OBJECT`, so a value ref
    // never names one (and a string ref never names anything here).
    let (id, tag) = decode(r as usize);
    (tag == TAG_OBJECT).then_some(id)
}

/// Add a reference to a primitive value ref; objects are already pinned by
/// the ref itself.
pub fn protect_value_ref(r: JSValueRef) {
    if r.is_null() {
        return;
    }
    let (id, tag) = decode(r as usize);
    if tag == TAG_VALUE {
        retain_value_ref(id);
    }
}

/// Release a value ref. A primitive gives up its table entry; an object
/// gives up its pin (the ref must not be used afterwards, per JSC's
/// `JSValueUnprotect` contract).
pub fn release_value_ref(r: JSValueRef) {
    if r.is_null() {
        return;
    }
    let (id, tag) = decode(r as usize);
    match tag {
        TAG_OBJECT => {
            OBJECTS.with(|objects| {
                objects.borrow_mut().remove(&id);
            });
        }
        TAG_VALUE => release_value(id),
        // A string ref's entry belongs to `JSStringRelease`.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crux::value::ValueKind;

    #[test]
    fn a_string_ref_never_decodes_as_a_value() {
        let id = ffi::retain_string(crux::string::JsString::from_utf8("x"));
        let string = string_ref_from_id(id);
        assert_eq!(string_ref_id(string), Some(id));
        // A host that passes a string ref where a value ref is expected
        // compiles in C; here it decodes to nothing.
        let as_value: JSValueRef = string.cast();
        assert!(ref_to_value(as_value).is_none());
        assert!(object_id(as_value).is_none());
        ffi::release_string(id);
    }

    #[test]
    fn a_value_ref_never_decodes_as_a_string() {
        let r = value_to_ref(Value::Number(7.5));
        assert!(string_ref_id(r.cast()).is_none());
        assert!(matches!(
            ref_to_value(r).map(|value| value.kind()),
            Some(ValueKind::Number(n)) if n == 7.5
        ));
        release_value_ref(r);
        assert!(ref_to_value(r).is_none());
    }

    #[test]
    fn the_low_two_bits_name_the_ref_kind() {
        let value = value_to_ref(Value::Number(1.0));
        assert_eq!((value as usize) & (TAG_MASK as usize), TAG_VALUE as usize);
        release_value_ref(value);
        let string = string_ref_from_id(1);
        assert_eq!((string as usize) & (TAG_MASK as usize), TAG_STRING as usize);
        assert_eq!(string_ref_id(string), Some(1));
    }
}
