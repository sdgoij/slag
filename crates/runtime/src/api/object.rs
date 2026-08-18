//! Object and Array helpers (v8::Object, v8::Array).

use crux::error::{ErrorKind, JsError};
use crux::object::JsObject;
use crux::property::PropertyDescriptor;
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use super::context::Context;
use super::handle::Local;

/// Object helpers (v8::Object).
pub struct Object;

impl Object {
    /// Create an ordinary object in `context`'s realm.
    #[allow(clippy::new_ret_no_self)] // v8::Object::New returns a new object, not `Self`.
    pub fn new(context: &Context) -> Result<Local, JsError> {
        let prototype = context
            .realm()
            .intrinsics
            .get("%Object.prototype%")
            .and_then(|value| value.as_object());
        Ok(Local(Value::Object(JsObject::ordinary_object_create(
            prototype,
        ))))
    }

    /// The object half of a value (objects and functions).
    fn handle(value: &Local) -> Result<crux::handle::Handle<JsObject>, JsError> {
        crate::context::as_object(value.value())
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "value is not an object".into()))
    }

    /// [[Get]] a named property (spec 7.3.1).
    pub fn get(context: &Context, object: &Local, key: &str) -> Result<Local, JsError> {
        let object = Self::handle(object)?;
        context.with_agent(|_| Ok(Local(object.get(&JsString::from_utf8(key))?)))
    }

    /// [[Set]] a named property (spec 7.3.3); `throw` selects the silent /
    /// throwing failure mode.
    pub fn set(
        context: &Context,
        object: &Local,
        key: &str,
        value: &Local,
        throw: bool,
    ) -> Result<bool, JsError> {
        let object = Self::handle(object)?;
        context.with_agent(|_| {
            object.set(&JsString::from_utf8(key), value.clone().into_value(), throw)
        })
    }

    /// [[HasProperty]] (spec 7.3.10): walks the prototype chain.
    pub fn has(context: &Context, object: &Local, key: &str) -> Result<bool, JsError> {
        let object = Self::handle(object)?;
        context.with_agent(|_| object.has_property(&JsString::from_utf8(key)))
    }

    /// [[Delete]] (spec 7.3.9).
    pub fn delete(context: &Context, object: &Local, key: &str) -> Result<bool, JsError> {
        let object = Self::handle(object)?;
        context.with_agent(|_| object.delete(&JsString::from_utf8(key)))
    }

    /// Define an own data property with explicit attributes
    /// (v8::Object::DefineOwnProperty).
    pub fn define(
        context: &Context,
        object: &Local,
        key: &str,
        value: &Local,
        writable: bool,
        enumerable: bool,
        configurable: bool,
    ) -> Result<(), JsError> {
        let object = Self::handle(object)?;
        context.with_agent(|_| {
            object.define_property_or_throw(
                &JsString::from_utf8(key),
                &PropertyDescriptor {
                    value: Some(value.clone().into_value()),
                    writable: Some(writable),
                    enumerable: Some(enumerable),
                    configurable: Some(configurable),
                    get: None,
                    set: None,
                },
            )
        })
    }

    /// Get the object's prototype (v8::Object::GetPrototype).
    pub fn get_prototype(context: &Context, object: &Local) -> Result<Local, JsError> {
        let object = Self::handle(object)?;
        context.with_agent(|_| {
            Ok(object
                .get_prototype_of()?
                .map(Value::Object)
                .unwrap_or(Value::Null)
                .into())
        })
    }

    /// Set the object's prototype (v8::Object::SetPrototype).
    pub fn set_prototype(
        context: &Context,
        object: &Local,
        prototype: &Local,
    ) -> Result<bool, JsError> {
        let object = Self::handle(object)?;
        let prototype = match prototype.value().kind() {
            ValueKind::Object(_) => prototype.value().as_object(),
            ValueKind::Null => None,
            _ => {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "prototype must be an object or null".into(),
                ));
            }
        };
        context.with_agent(|_| object.set_prototype_of(prototype))
    }
}

/// Array helpers (v8::Array).
pub struct Array;

impl Array {
    /// Create an array from the given elements (spec 7.3.15 CreateArrayFromList).
    #[allow(clippy::new_ret_no_self)] // v8::Array::New returns a new array, not `Self`.
    pub fn new(context: &Context, elements: &[Local]) -> Result<Local, JsError> {
        let values: Vec<Value> = elements
            .iter()
            .map(|element| element.clone().into_value())
            .collect();
        context.with_agent(|agent| {
            crate::builtins::array::array_from_values(agent, &values).map(Local)
        })
    }

    /// The array's `length` property.
    pub fn length(context: &Context, array: &Local) -> Result<f64, JsError> {
        Object::get(context, array, "length")?
            .as_number()
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "value is not an array".into()))
    }

    /// The element at `index` (via `Object::get`).
    pub fn get(context: &Context, array: &Local, index: u32) -> Result<Local, JsError> {
        Object::get(context, array, &index.to_string())
    }

    /// Set the element at `index` (via `Object::set`, throwing).
    pub fn set(
        context: &Context,
        array: &Local,
        index: u32,
        value: &Local,
    ) -> Result<bool, JsError> {
        Object::set(context, array, &index.to_string(), value, true)
    }
}

/// Look up `object.method` on the global object (JSON.parse, Promise.resolve).
pub(crate) fn global_function(
    context: &Context,
    object: &str,
    method: &str,
) -> Result<Local, JsError> {
    let object = global_object(context, object)?;
    Object::get(context, &object, method)
}

/// Look up a named global object (JSON, Promise).
pub(crate) fn global_object(context: &Context, name: &str) -> Result<Local, JsError> {
    Object::get(context, &context.global(), name)
}
