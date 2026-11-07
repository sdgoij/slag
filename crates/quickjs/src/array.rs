//! The `Array` facade (rquickjs-shaped).

use runtime::api::{self, Local};

use crate::context::Context;
use crate::exception::Error;
use crate::value::Value;

/// A JavaScript array (rquickjs's `Array`).
#[derive(Debug, Clone, Copy)]
pub struct Array(Local);

impl Array {
    /// Create an array from the given elements (spec 7.3.15 CreateArrayFromList).
    pub fn new(context: &Context, elements: &[Value]) -> std::result::Result<Self, Error> {
        let locals: Vec<Local> = elements.iter().map(|element| *element.as_local()).collect();
        api::Array::new(&context.api(), &locals)
            .map(Self)
            .map_err(Error::from_js_error)
    }

    /// The array's `length`.
    pub fn length(&self, context: &Context) -> std::result::Result<f64, Error> {
        api::Array::length(&context.api(), &self.0).map_err(Error::from_js_error)
    }

    /// The element at `index`.
    pub fn get(&self, context: &Context, index: u32) -> std::result::Result<Value, Error> {
        api::Array::get(&context.api(), &self.0, index)
            .map(Value::from_local)
            .map_err(Error::from_js_error)
    }

    /// Set the element at `index`.
    pub fn set(
        &self,
        context: &Context,
        index: u32,
        value: Value,
    ) -> std::result::Result<(), Error> {
        api::Array::set(&context.api(), &self.0, index, value.as_local())
            .map(|_| ())
            .map_err(Error::from_js_error)
    }

    /// The value this array is.
    pub fn as_value(&self) -> Value {
        Value::from_local(self.0)
    }
}
