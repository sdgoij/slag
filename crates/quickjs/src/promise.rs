//! The `Promise` facade (rquickjs-shaped): a handle over a promise value.
//! Resolution/rejection/then go through the global `Promise` built-ins; state
//! and result read the agent's promise table.

use runtime::api::{self, Local};

use crate::context::Context;
use crate::exception::Error;
use crate::value::Value;

/// A JavaScript promise (rquickjs's `Promise`).
#[derive(Debug, Clone, Copy)]
pub struct Promise(Local);

impl Promise {
    pub(crate) fn from_local(local: Local) -> Self {
        Self(local)
    }

    /// A promise resolved with `value` (rquickjs's `Promise::resolve`).
    pub fn resolve(context: &Context, value: Value) -> Result<Self, Error> {
        api::Promise::resolve(&context.api(), value.as_local())
            .map(Self)
            .map_err(Error::from)
    }

    /// A promise rejected with `value` (rquickjs's `Promise::reject`).
    pub fn reject(context: &Context, value: Value) -> Result<Self, Error> {
        api::Promise::reject(&context.api(), value.as_local())
            .map(Self)
            .map_err(Error::from)
    }

    /// `promise.then(on_fulfilled, on_rejected)`, returning the derived
    /// promise (rquickjs's `Promise::then`).
    pub fn then(
        &self,
        context: &Context,
        on_fulfilled: Option<&Value>,
        on_rejected: Option<&Value>,
    ) -> Result<Self, Error> {
        let fulfilled = on_fulfilled.map(|value| value.as_local());
        let rejected = on_rejected.map(|value| value.as_local());
        api::Promise::then(&context.api(), &self.0, fulfilled, rejected)
            .map(Self)
            .map_err(Error::from)
    }

    /// The promise's state: `"pending"`, `"fulfilled"`, or `"rejected"`.
    pub fn state(&self, context: &Context) -> Result<&'static str, Error> {
        api::Promise::state(&context.api(), &self.0).map_err(Error::from)
    }

    /// The promise's result value, or *undefined* while pending.
    pub fn result(&self, context: &Context) -> Result<Value, Error> {
        api::Promise::result(&context.api(), &self.0)
            .map(Value::from_local)
            .map_err(Error::from)
    }

    /// The value this promise is.
    pub fn as_value(&self) -> Value {
        Value::from_local(self.0)
    }
}
