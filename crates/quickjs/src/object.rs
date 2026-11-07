//! The `Object` facade (rquickjs-shaped): property access on an object value.

use runtime::api::{self, Local};

use crate::context::Context;
use crate::exception::Error;
use crate::value::Value;

/// A JavaScript object (rquickjs's `Object`).
#[derive(Debug, Clone, Copy)]
pub struct Object(Local);

impl Object {
    pub(crate) fn from_local(local: Local) -> Self {
        Self(local)
    }

    /// Create an ordinary object in `context`'s realm.
    pub fn new(context: &Context) -> std::result::Result<Self, Error> {
        api::Object::new(&context.api())
            .map(Self)
            .map_err(Error::from_js_error)
    }

    /// [[Get]] a named property (spec 7.3.1).
    pub fn get(&self, context: &Context, key: &str) -> std::result::Result<Value, Error> {
        api::Object::get(&context.api(), &self.0, key)
            .map(Value::from_local)
            .map_err(Error::from_js_error)
    }

    /// [[Set]] a named property, throwing on failure (spec 7.3.3).
    pub fn set(
        &self,
        context: &Context,
        key: &str,
        value: Value,
    ) -> std::result::Result<(), Error> {
        api::Object::set(&context.api(), &self.0, key, value.as_local(), true)
            .map(|_| ())
            .map_err(Error::from_js_error)
    }

    /// [[HasProperty]] (spec 7.3.10): walks the prototype chain.
    pub fn has(&self, context: &Context, key: &str) -> std::result::Result<bool, Error> {
        api::Object::has(&context.api(), &self.0, key).map_err(Error::from_js_error)
    }

    /// [[Delete]] a named property (spec 7.3.9).
    pub fn delete(&self, context: &Context, key: &str) -> std::result::Result<bool, Error> {
        api::Object::delete(&context.api(), &self.0, key).map_err(Error::from_js_error)
    }

    /// The value this object is.
    pub fn as_value(&self) -> Value {
        Value::from_local(self.0)
    }
}
