//! The `Context`/`Ctx` facade (rquickjs-shaped): a realm on a runtime.

use crux::error::JsError;
use runtime::api::{self, Isolate};

use crate::exception::Error;
use crate::object::Object;
use crate::value::Value;

/// A realm on a runtime (rquickjs's `Context`).
#[derive(Clone, Copy)]
pub struct Context {
    api: api::Context,
}

impl Context {
    pub(crate) fn new(isolate: &mut Isolate) -> std::result::Result<Self, JsError> {
        Ok(Self {
            api: api::Context::new(isolate)?,
        })
    }

    pub(crate) fn api(&self) -> api::Context {
        self.api
    }

    /// Evaluate a Script and return its completion value.
    pub fn eval(&self, source: &str) -> std::result::Result<Value, Error> {
        self.api
            .try_eval(source)
            .map(Value::from_local)
            .map_err(Error::from_js_error)
    }

    /// The realm's global object.
    pub fn global(&self) -> Object {
        Object::from_local(self.api.global())
    }

    /// Set a property on the global object.
    pub fn set_global(&self, name: &str, value: Value) -> std::result::Result<(), Error> {
        let global = self.api.global();
        api::Object::set(&self.api, &global, name, value.as_local(), true)
            .map(|_| ())
            .map_err(Error::from_js_error)
    }

    /// Drain the job queues (microtasks, timers, generic jobs).
    pub fn run_microtasks(&self) -> std::result::Result<(), JsError> {
        self.api.run_microtasks()
    }
}
