//! The `Function` facade (rquickjs-shaped): host functions and calls.

use runtime::api::{self, Local};

use crate::context::Context;
use crate::exception::Error;
use crate::value::Value;

/// The call site a host callback receives (rquickjs's `Rest`/`CallArgs`).
pub struct CallArgs<'a> {
    info: &'a api::FunctionCallbackInfo<'a>,
}

impl<'a> CallArgs<'a> {
    pub(crate) fn new(info: &'a api::FunctionCallbackInfo<'a>) -> Self {
        Self { info }
    }

    /// The `this` value of the call.
    pub fn this(&self) -> Value {
        Value::from_local(self.info.this())
    }

    /// The number of arguments.
    pub fn len(&self) -> usize {
        self.info.length()
    }

    pub fn is_empty(&self) -> bool {
        self.info.length() == 0
    }

    /// The argument at `index`, if present.
    pub fn get(&self, index: usize) -> Option<Value> {
        self.info.arg(index).map(Value::from_local)
    }

    /// Set the callback's return value.
    pub fn set_return(&self, value: Value) {
        self.info.get_return_value().set(*value.as_local());
    }
}

/// A JavaScript function (rquickjs's `Function`).
#[derive(Debug, Clone, Copy)]
pub struct Function(Local);

impl Function {
    pub(crate) fn from_local(local: Local) -> Self {
        Self(local)
    }

    /// Call the function with `this` and `args`.
    pub fn call(
        &self,
        context: &Context,
        this: Value,
        args: &[Value],
    ) -> std::result::Result<Value, Error> {
        let locals: Vec<Local> = args.iter().map(|arg| *arg.as_local()).collect();
        context
            .api()
            .try_call(&self.0, this.as_local(), &locals)
            .map(Value::from_local)
            .map_err(Error::from_js_error)
    }

    /// The value this function is.
    pub fn as_value(&self) -> Value {
        Value::from_local(self.0)
    }

    pub(crate) fn as_local(&self) -> &Local {
        &self.0
    }
}
