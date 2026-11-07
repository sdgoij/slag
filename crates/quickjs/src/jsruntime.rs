//! The `Runtime` facade (rquickjs-shaped): the isolate and its heap.

use crux::error::JsError;
use runtime::api::{self, Isolate};

use crate::class::Class;
use crate::context::Context;
use crate::exception::Error;
use crate::function::{CallArgs, Function};

/// A JavaScript runtime (rquickjs's `Runtime`): one isolate.
pub struct Runtime {
    isolate: Box<Isolate>,
}

impl Runtime {
    pub fn new() -> Self {
        Self {
            isolate: Isolate::new(),
        }
    }

    /// Create a realm (context) on this runtime.
    pub fn create_context(&mut self) -> std::result::Result<Context, Error> {
        Context::new(&mut self.isolate).map_err(Error::from_js_error)
    }

    /// Create a host function value (not yet attached to any object).
    pub fn create_function<F>(
        &mut self,
        context: &Context,
        callback: F,
    ) -> std::result::Result<Function, Error>
    where
        F: for<'a> Fn(&CallArgs<'a>) + 'static,
    {
        let template = api::FunctionTemplate::new(
            &mut self.isolate,
            Box::new(move |info: &api::FunctionCallbackInfo| {
                callback(&CallArgs::new(info));
            }),
        );
        template
            .get_function(&context.api())
            .map(Function::from_local)
            .map_err(Error::from_js_error)
    }

    /// Create a host function and set it as a global named `name`.
    pub fn add_function<F>(
        &mut self,
        context: &Context,
        name: &str,
        callback: F,
    ) -> std::result::Result<(), Error>
    where
        F: for<'a> Fn(&CallArgs<'a>) + 'static,
    {
        let function = self.create_function(context, callback)?;
        context.set_global(name, function.as_value())
    }

    /// Create a host-defined class (a constructible function whose prototype
    /// carries the methods registered with [`Class::method`]).
    pub fn create_class<F>(&mut self, name: &str, constructor: F) -> Class
    where
        F: for<'a> Fn(&CallArgs<'a>) + 'static,
    {
        let callback: api::FunctionCallback = Box::new(move |info: &api::FunctionCallbackInfo| {
            constructor(&CallArgs::new(info));
        });
        Class::new(&mut self.isolate, name, callback)
    }

    /// Drain the job queues (microtasks, timers, generic jobs).
    pub fn run_microtasks(&mut self) -> std::result::Result<(), JsError> {
        self.isolate.run_microtasks()
    }

    /// A host's heap size limit (`v8::CreateParams::heap_limits`). A maximum
    /// of 0 is "no limit".
    pub fn set_heap_limits(&mut self, initial: usize, maximum: usize) {
        self.isolate.set_heap_limits(initial, maximum);
    }

    /// Ask a running execution to stop (v8::Isolate::TerminateExecution).
    pub fn terminate_execution(&self) {
        self.isolate.terminate_execution();
    }

    /// Enable the engine's per-allocation `--gc-stress` mode (a Slag GC knob,
    /// not a QuickJS API): collect at every safe point with the barrier audit
    /// on. Exercises the facade under the rooting/barrier checks the CLI's
    /// `--gc-stress` turns on.
    pub fn set_gc_stress(&mut self, enabled: bool) {
        self.isolate.agent().set_gc_stress(enabled);
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}
