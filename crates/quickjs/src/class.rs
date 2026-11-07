//! The `Class` facade (rquickjs-shaped): a host-defined constructor and its
//! prototype. A class is a [`runtime::api::FunctionTemplate`] whose prototype
//! template carries the methods the host registers.
//!
//! Register every method *before* materializing the constructor in a realm —
//! the prototype template is applied when the constructor is first asked for,
//! so a method added afterwards is missing from the already-made prototype.
//!
//! Finalizers (QuickJS's `JSClassFinalizer`) are available through
//! `crux::host::HostOps` via [`runtime::api::ObjectTemplate`]'s host state,
//! and are not yet wired into this thin facade.

use std::rc::Rc;

use crux::error::{ErrorKind, JsError};
use runtime::api::{self, FunctionCallback, Isolate};

use crate::context::Context;
use crate::exception::Error;
use crate::function::Function;
use crate::object::Object;

/// A host-defined class (rquickjs's `Class`).
pub struct Class {
    template: Rc<api::FunctionTemplate>,
}

impl Class {
    pub(crate) fn new(isolate: &mut Isolate, name: &str, constructor: FunctionCallback) -> Self {
        let template = api::FunctionTemplate::new(isolate, constructor);
        template.set_class_name(name);
        Self { template }
    }

    /// The constructor function, materialized in `context`'s realm.
    pub fn constructor(&self, context: &Context) -> Result<Function, Error> {
        self.template
            .get_function(&context.api())
            .map(Function::from_local)
            .map_err(Error::from)
    }

    /// The constructor's `.prototype` object.
    pub fn prototype(&self, context: &Context) -> Result<Object, Error> {
        let constructor = self.constructor(context)?;
        let object = Object::from_local(*constructor.as_local());
        object
            .get(context, "prototype")?
            .as_object()
            .ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "prototype is not an object".into()).into()
            })
    }

    /// Register a method on the prototype.
    pub fn method(&self, name: &str, method: &Function) {
        self.template
            .prototype_template()
            .set(name, *method.as_local());
    }
}
