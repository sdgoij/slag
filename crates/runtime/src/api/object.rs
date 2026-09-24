//! Object and Array helpers (v8::Object, v8::Array).

use crux::error::{ErrorKind, JsError};
use crux::object::JsObject;
use crux::property::PropertyDescriptor;
use crux::property::PropertyKey;
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use super::context::Context;
use super::handle::Local;

/// The property key a host's handle names: a String or a Symbol, the two a
/// `Name` can be (spec 6.1.7).
///
/// The key-taking forms of the property operations are the `Name`-shaped ones,
/// so a handle of any other kind is refused rather than coerced — a `v8::Name`
/// cannot be built from one in the crate this stands in for either.
fn key_of(key: &Local) -> Result<PropertyKey, JsError> {
    match key.value().kind() {
        ValueKind::String(text) => Ok(PropertyKey::from_js_string(&text)),
        ValueKind::Symbol(symbol) => Ok(PropertyKey::Symbol(symbol)),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "property key is not a string or a symbol".into(),
        )),
    }
}

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
    ///
    /// Routed through the runtime's [[Get]] rather than the object's own
    /// property map: a module namespace's exports are read through its module
    /// environment, so the map read answers the placeholder instead.
    pub fn get(context: &Context, object: &Local, key: &str) -> Result<Local, JsError> {
        Self::get_key(context, object, &Local::string(key))
    }

    /// [[Get]] a property by the key a host built (spec 7.3.1): a String or a
    /// Symbol, which is what a `Name` is.
    pub fn get_key(context: &Context, object: &Local, key: &Local) -> Result<Local, JsError> {
        let base = Value::Object(Self::handle(object)?);
        let key = key_of(key)?;
        context.with_agent(|agent| {
            crate::context::get_property_key(agent, &base, &key, base).map(Local)
        })
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
        Self::set_key(context, object, &Local::string(key), value, throw)
    }

    /// [[Set]] a property by the key a host built (spec 7.3.3).
    pub fn set_key(
        context: &Context,
        object: &Local,
        key: &Local,
        value: &Local,
        throw: bool,
    ) -> Result<bool, JsError> {
        let object = Self::handle(object)?;
        let key = key_of(key)?;
        context.with_agent(|_| object.set_key(&key, value.into_value(), throw))
    }

    /// [[HasProperty]] (spec 7.3.10): walks the prototype chain.
    pub fn has(context: &Context, object: &Local, key: &str) -> Result<bool, JsError> {
        Self::has_key(context, object, &Local::string(key))
    }

    /// [[HasProperty]] by the key a host built (spec 7.3.10).
    pub fn has_key(context: &Context, object: &Local, key: &Local) -> Result<bool, JsError> {
        let object = Self::handle(object)?;
        let key = key_of(key)?;
        context.with_agent(|_| object.has_property_key(&key))
    }

    /// [[HasOwnProperty]] by the key a host built (spec 7.3.12): the prototype
    /// chain is not consulted.
    pub fn has_own_key(context: &Context, object: &Local, key: &Local) -> Result<bool, JsError> {
        let object = Self::handle(object)?;
        let key = key_of(key)?;
        context.with_agent(|_| object.has_own_property_key(&key))
    }

    /// [[Delete]] (spec 7.3.9).
    pub fn delete(context: &Context, object: &Local, key: &str) -> Result<bool, JsError> {
        Self::delete_key(context, object, &Local::string(key))
    }

    /// [[Delete]] by the key a host built (spec 7.3.9).
    pub fn delete_key(context: &Context, object: &Local, key: &Local) -> Result<bool, JsError> {
        let object = Self::handle(object)?;
        let key = key_of(key)?;
        context.with_agent(|_| object.delete_key(&key))
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
                    value: Some(value.into_value()),
                    writable: Some(writable),
                    enumerable: Some(enumerable),
                    configurable: Some(configurable),
                    get: None,
                    set: None,
                },
            )
        })
    }

    /// The built-in tag `Object.prototype.toString` reports for `value`
    /// (spec 20.1.3.6 steps 4-14), with the value ToObject'd first.
    ///
    /// The tag is decided by the engine's own brands — [[Call]], the
    /// [[ParameterMap]] slot, the boxed-primitive marker, the error, Date and
    /// RegExp slots, the object kind — none of which a host can reach by reading
    /// properties, and a host that has to describe a value without running any
    /// of its code needs the name for it. The string `@@toStringTag` override
    /// that `Object.prototype.toString` applies is deliberately not applied
    /// here, so the answer is the tag of the value's kind and nothing a script
    /// can change.
    pub fn builtin_tag(context: &Context, value: &Local) -> Result<String, JsError> {
        let object = context.with_agent(|agent| crate::context::to_object(agent, value.value()))?;
        context.with_agent(|agent| crate::builtins::object::builtin_tag(agent, &object))
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
            .map(|element| element.into_value())
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
