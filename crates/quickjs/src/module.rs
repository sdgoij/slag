//! The `Module` facade (rquickjs-shaped): a source-text module record.
//!
//! Static imports resolve through the engine's own module registry; a host
//! that needs to satisfy `import` from outside the registry sets its resolver
//! through the C loader hook (`HostHooks::resolve_imported_module`). Dynamic
//! import and `import.meta` are tracked as embedding gaps.

pub use runtime::api::ModuleStatus;

use runtime::api::Module as ApiModule;

use crate::context::Context;
use crate::exception::Error;
use crate::object::Object;
use crate::promise::Promise;

/// A JavaScript module (rquickjs's `Module`).
#[derive(Clone, Copy)]
pub struct Module(ApiModule);

impl Module {
    /// Parse `source` as a module named `name` (rquickjs's `Module::new`).
    pub fn new(context: &Context, name: &str, source: &str) -> Result<Self, Error> {
        ApiModule::compile(&context.api(), name, source)
            .map(Self)
            .map_err(Error::from)
    }

    /// Evaluate the module; the value is the evaluation promise (rquickjs's
    /// `Module::eval`).
    pub fn evaluate(&self, context: &Context) -> Result<Promise, Error> {
        ApiModule::evaluate(&self.0, &context.api())
            .map(Promise::from_local)
            .map_err(Error::from)
    }

    /// The module's namespace object (spec 16.2.1.10).
    pub fn namespace(&self, context: &Context) -> Result<Object, Error> {
        ApiModule::namespace(&self.0, &context.api())
            .map(Object::from_local)
            .map_err(Error::from)
    }

    /// The module's status.
    pub fn status(&self) -> ModuleStatus {
        self.0.status()
    }

    /// The name the module was compiled under, when it has one.
    pub fn name(&self) -> Option<String> {
        self.0.name()
    }
}
