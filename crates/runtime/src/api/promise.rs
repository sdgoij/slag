//! Promise helpers (v8::Promise).

use crux::error::{ErrorKind, JsError};
use crux::value::Value;

use super::context::Context;
use super::handle::Local;
use super::object::{Object, global_object};

/// Promise helpers (v8::Promise). Resolution/rejection/then go through the
/// global `Promise` built-ins; state and result read the agent's promise
/// table (spec 27.2.1 [[PromiseState]]/[[PromiseResult]]).
pub struct Promise;

impl Promise {
    /// A promise resolved with `value` (v8::Promise::Resolve).
    pub fn resolve(context: &Context, value: &Local) -> Result<Local, JsError> {
        let constructor = global_object(context, "Promise")?;
        let resolve = Object::get(context, &constructor, "resolve")?;
        context.try_call(&resolve, &constructor, std::slice::from_ref(value))
    }

    /// A promise rejected with `value` (v8::Promise::Reject).
    pub fn reject(context: &Context, value: &Local) -> Result<Local, JsError> {
        let constructor = global_object(context, "Promise")?;
        let reject = Object::get(context, &constructor, "reject")?;
        context.try_call(&reject, &constructor, std::slice::from_ref(value))
    }

    /// `promise.then(on_fulfilled, on_rejected)`, returning the derived
    /// promise.
    pub fn then(
        context: &Context,
        promise: &Local,
        on_fulfilled: Option<&Local>,
        on_rejected: Option<&Local>,
    ) -> Result<Local, JsError> {
        let then = Object::get(context, promise, "then")?;
        let fulfilled = on_fulfilled.cloned().unwrap_or_else(Local::undefined);
        let rejected = on_rejected.cloned().unwrap_or_else(Local::undefined);
        context.try_call(&then, promise, &[fulfilled, rejected])
    }

    /// The promise's state: `"pending"`, `"fulfilled"`, or `"rejected"`
    /// (the spec 27.2.1 [[PromiseState]]).
    pub fn state(context: &Context, promise: &Local) -> Result<&'static str, JsError> {
        let object = promise_object(promise)?;
        context.with_agent(|agent| {
            let Some(data) = agent.promises.get(&object.id()) else {
                return Err(not_a_promise());
            };
            Ok(match &data.borrow().state {
                crate::promise::PromiseState::Pending { .. } => "pending",
                crate::promise::PromiseState::Fulfilled(_) => "fulfilled",
                crate::promise::PromiseState::Rejected(_) => "rejected",
            })
        })
    }

    /// The promise's result value (spec 27.2.1 [[PromiseResult]]), or
    /// *undefined* while pending.
    pub fn result(context: &Context, promise: &Local) -> Result<Local, JsError> {
        let object = promise_object(promise)?;
        context.with_agent(|agent| {
            let Some(data) = agent.promises.get(&object.id()) else {
                return Err(not_a_promise());
            };
            let value = match &data.borrow().state {
                crate::promise::PromiseState::Fulfilled(value)
                | crate::promise::PromiseState::Rejected(value) => *value,
                crate::promise::PromiseState::Pending { .. } => Value::Undefined,
            };
            Ok(Local(value))
        })
    }
}

fn promise_object(
    promise: &Local,
) -> Result<crux::handle::Handle<crux::object::JsObject>, JsError> {
    promise.as_object().ok_or_else(not_a_promise)
}

fn not_a_promise() -> JsError {
    JsError::new(ErrorKind::TypeError, "value is not a promise".into())
}
