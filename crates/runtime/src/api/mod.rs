//! The V8-shaped embedding API: isolates, contexts, handles, templates,
//! exceptions, and host functions.
//!
//! This is the Rust foundation the C/C++ drop-in surfaces (`crates/jsc`,
//! `crates/v8`) build on. It mirrors the shape of the V8 embedder API —
//! an [`Isolate`] owns the heap/execution state, a [`Context`] is a realm on
//! an isolate, [`Local`]/[`Global`] are handles, [`FunctionTemplate`]/
//! [`ObjectTemplate`] create host functions and objects, and
//! [`TryCatch`]/[`Exception`] model pending exceptions.
//!
//! Divergences from V8, all deliberate:
//! - Values are `Rc`-backed: handles are always valid, handle scopes are
//!   advisory markers, and `Global` is a strong reference either way.
//! - One context per isolate (the agent owns its realm).
//! - Microtasks only run when [`Context::run_microtasks`] is called; the
//!   auto-drain behaviour of the classic `embed` API is not inherited.

mod context;
mod external;
mod handle;
mod json;
mod object;
mod promise;
mod script;
mod template;
mod try_catch;

pub use context::{Context, ContextScope};
pub use external::External;
pub use handle::{EscapableHandleScope, Global, HandleScope, Local, MaybeLocal};
pub use json::Json;
pub use object::{Array, Object};
pub use promise::Promise;
pub use script::Script;
pub use template::{
    FunctionCallback, FunctionCallbackInfo, FunctionTemplate, ObjectTemplate, ReturnValue,
};
pub use try_catch::{Exception, TryCatch};

use std::cell::RefCell;
use std::collections::HashMap;

use crux::error::JsError;
use crux::value::Value;

use crate::agent::Agent;

/// The isolate: the heap/execution state (an [`Agent`]), the pending
/// exception slot, and host data slots (v8::Isolate::SetData/GetData).
///
/// `Isolate::new` returns the isolate **boxed**: contexts and templates hold
/// raw pointers to it, so the heap allocation must stay put even if the
/// `Box` is moved. `repr(C)` keeps the agent at offset 0 so the TLS agent
/// pointer recorded by `crux::function::with_agent` doubles as the isolate
/// pointer (see [`Isolate::get_current`]).
#[repr(C)]
pub struct Isolate {
    pub(crate) agent: Agent,
    pub(crate) pending_exception: RefCell<Option<Value>>,
    pub(crate) data: RefCell<HashMap<u32, usize>>,
}

impl Isolate {
    /// Create a fresh isolate: a bare agent with no realm. A [`Context`]
    /// must be created before any script can run (like V8: no context, no
    /// execution).
    ///
    /// The isolate is boxed so its address is stable: contexts, templates,
    /// and the TLS agent window hold raw pointers to it, and moving a `Box`
    /// moves the pointer, not the allocation.
    pub fn new() -> Box<Self> {
        Box::new(Self {
            agent: Agent::new(),
            pending_exception: RefCell::new(None),
            data: RefCell::new(HashMap::new()),
        })
    }

    /// The underlying agent (advanced use; the spec state lives here).
    pub fn agent(&mut self) -> &mut Agent {
        &mut self.agent
    }

    /// The current isolate on this thread, or `None` outside an eval/call
    /// window. The agent is the first field (`repr(C)`), so the TLS agent
    /// pointer set by `crux::function::with_agent` and the isolate pointer
    /// coincide. Valid only while that window is on the stack.
    pub fn get_current() -> Option<*mut Isolate> {
        let agent = crux::function::current_agent();
        if agent.is_null() {
            None
        } else {
            Some(agent as *mut Isolate)
        }
    }

    /// The agent pointer at offset 0 (FFI-facing; valid while the isolate
    /// is alive).
    pub fn agent_ptr(&self) -> *mut Agent {
        self as *const Isolate as *const Agent as *mut Agent
    }

    /// Drain the job queues: promise (microtask), timeout, then generic
    /// jobs (v8::Isolate::RunMicrotasks, minus the platform hook).
    pub fn run_microtasks(&mut self) -> Result<(), JsError> {
        self.agent.run_jobs()
    }

    /// v8::Isolate::SetData/GetData: host data slots keyed by slot number.
    pub fn set_data(&self, slot: u32, data: usize) {
        self.data.borrow_mut().insert(slot, data);
    }

    /// The value of a host data slot, if set.
    pub fn get_data(&self, slot: u32) -> Option<usize> {
        self.data.borrow().get(&slot).copied()
    }

    /// Throw a language value: set the pending exception
    /// (v8::Isolate::ThrowException).
    pub fn throw_exception(&self, value: Value) {
        self.set_pending_exception(value);
    }

    /// Whether a pending exception is set.
    pub fn has_pending_exception(&self) -> bool {
        self.pending_exception.borrow().is_some()
    }

    /// The pending exception value, if set.
    pub fn pending_exception(&self) -> Option<Value> {
        self.pending_exception.borrow().clone()
    }

    pub fn set_pending_exception(&self, value: Value) {
        *self.pending_exception.borrow_mut() = Some(value);
    }

    pub fn take_pending_exception(&self) -> Option<Value> {
        self.pending_exception.borrow_mut().take()
    }
}

impl Default for Box<Isolate> {
    fn default() -> Self {
        Isolate::new()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;

    fn isolate() -> Box<Isolate> {
        Isolate::new()
    }

    fn context(isolate: &mut Isolate) -> Context {
        Context::new(isolate).unwrap()
    }

    #[test]
    fn eval_returns_the_completion_value() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        assert_eq!(
            context.eval("1 + 2").to_local_checked().as_number(),
            Some(3.0)
        );
    }

    #[test]
    fn eval_failure_sets_the_pending_exception() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        let result = context.eval("throw new TypeError('boom')");
        assert!(result.is_empty());
        assert!(isolate.has_pending_exception());
        let exception = isolate.pending_exception().unwrap();
        assert_eq!(crux::value::type_of(&exception), "object");
        // The native error constructor was used, so the thrown value has a
        // `name` of TypeError.
        let exception_object = crate::context::as_object(&exception).unwrap();
        let name = exception_object
            .get(&crux::string::JsString::from_utf8("name"))
            .unwrap();
        assert_eq!(name.to_string(), "TypeError");
    }

    #[test]
    fn try_catch_observes_and_clears_the_pending_exception() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        {
            let try_catch = TryCatch::new(&mut isolate);
            let result = context.eval("throw new Error('boom')");
            assert!(result.is_empty());
            assert!(try_catch.has_caught());
            assert!(try_catch.exception().unwrap().is_object());
        }
        assert!(!isolate.has_pending_exception());
    }

    #[test]
    fn rethrow_keeps_the_exception_pending() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        {
            let try_catch = TryCatch::new(&mut isolate);
            let _ = context.eval("throw new Error('boom')");
            assert!(try_catch.has_caught());
            try_catch.rethrow();
        }
        assert!(isolate.has_pending_exception());
    }

    #[test]
    fn exception_throws_native_errors() {
        let mut isolate = isolate();
        let _context = context(&mut isolate);
        let error = Exception::throw_type_error(&mut isolate, "nope").unwrap();
        assert!(error.is_object());
        assert!(isolate.has_pending_exception());
    }

    #[test]
    fn host_function_is_callable_from_js() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        let template = FunctionTemplate::new(
            &mut isolate,
            Box::new(|info| {
                let sum: f64 = info.args().filter_map(|arg| arg.as_number()).sum();
                info.get_return_value().set_number(sum);
            }),
        );
        template.set_class_name("sum");
        let function = template.get_function(&context).unwrap();
        Object::set(&context, &context.global(), "sum", &function, true).unwrap();
        assert_eq!(
            context.eval("sum(1, 2, 3)").to_local_checked().as_number(),
            Some(6.0)
        );
        assert_eq!(
            context
                .eval("sum.name")
                .to_local_checked()
                .as_string()
                .as_deref(),
            Some("sum")
        );
    }

    #[test]
    fn host_callback_receives_this_and_args() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        let observed_this: Rc<RefCell<Option<Value>>> = Rc::new(RefCell::new(None));
        let observed = observed_this.clone();
        let template = FunctionTemplate::new(
            &mut isolate,
            Box::new(move |info| {
                *observed.borrow_mut() = Some(info.this().into_value());
                info.get_return_value()
                    .set(info.arg(0).unwrap_or(Local::undefined()));
            }),
        );
        let function = template.get_function(&context).unwrap();
        Object::set(&context, &context.global(), "echo", &function, true).unwrap();
        let result = context
            .try_eval("({ x: 'hi' }).x = echo.call({ y: 'obj' }, 'arg')")
            .unwrap_or_else(|error| panic!("eval failed: {error}"));
        assert_eq!(result.as_string().as_deref(), Some("arg"));
        // `this` inside the callback is the `call` receiver, an object.
        let this = observed_this.borrow().clone().unwrap();
        assert!(this.is_object());
        let this_object = crate::context::as_object(&this).unwrap();
        let y = this_object
            .get(&crux::string::JsString::from_utf8("y"))
            .unwrap();
        assert_eq!(y.to_string(), "obj");
    }

    #[test]
    fn host_callback_throw_propagates_to_js() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        let template = FunctionTemplate::new(
            &mut isolate,
            Box::new(|info| {
                unsafe { &*info.isolate() }
                    .throw_exception(Local::string("host threw").into_value());
            }),
        );
        let function = template.get_function(&context).unwrap();
        Object::set(&context, &context.global(), "boom", &function, true).unwrap();
        let result = context
            .eval("try { boom(); 'not reached' } catch (e) { e }")
            .to_local_checked();
        assert_eq!(result.as_string().as_deref(), Some("host threw"));
        assert!(!isolate.has_pending_exception());
    }

    #[test]
    fn host_constructor_creates_instances() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        let template = FunctionTemplate::new(
            &mut isolate,
            Box::new(|info| {
                if info.is_construct_call() {
                    info.get_return_value().set(info.this());
                } else {
                    info.get_return_value().set_number(0.0);
                }
            }),
        );
        template.set_class_name("Point");
        template.instance_template().set("x", Local::number(1.0));
        let constructor = template.get_function(&context).unwrap();
        Object::set(&context, &context.global(), "Point", &constructor, true).unwrap();

        let instance = context.eval("new Point()").to_local_checked();
        assert_eq!(
            Object::get(&context, &instance, "x").unwrap().as_number(),
            Some(1.0)
        );
        assert_eq!(
            context
                .eval("new Point() instanceof Point")
                .to_local_checked()
                .as_boolean(),
            Some(true)
        );
        assert_eq!(
            context.eval("Point()").to_local_checked().as_number(),
            Some(0.0)
        );
    }

    #[test]
    fn object_template_accessors_route_to_callbacks() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        let template = ObjectTemplate::new(&mut isolate);
        let stored: Rc<Cell<f64>> = Rc::new(Cell::new(7.0));
        let getter_stored = stored.clone();
        let setter_stored = stored.clone();
        template.set_accessor(
            "x",
            Box::new(move |info| {
                info.get_return_value().set_number(getter_stored.get());
            }),
            Some(Box::new(move |info| {
                setter_stored.set(info.arg(0).and_then(|arg| arg.as_number()).unwrap_or(0.0));
                info.get_return_value().set_undefined();
            })),
        );
        let object = template.new_instance(&context).unwrap();
        assert_eq!(
            Object::get(&context, &object, "x").unwrap().as_number(),
            Some(7.0)
        );
        Object::set(&context, &object, "x", &Local::number(9.0), true).unwrap();
        assert_eq!(stored.get(), 9.0);
        assert_eq!(
            Object::get(&context, &object, "x").unwrap().as_number(),
            Some(9.0)
        );
    }

    #[test]
    fn external_round_trips_host_pointers() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        let marker: *mut std::ffi::c_void = 0xDEAD as *mut std::ffi::c_void;
        let external = External::new(&mut isolate, marker).unwrap();
        assert_eq!(external.value(), marker);
        Object::set(
            &context,
            &context.global(),
            "ext",
            &external.as_value(),
            true,
        )
        .unwrap();
        let back = context.eval("ext").to_local_checked();
        assert_eq!(
            back.as_object().unwrap().id(),
            external.as_value().as_object().unwrap().id()
        );
    }

    #[test]
    fn object_and_array_helpers() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        let object = Object::new(&context).unwrap();
        Object::set(&context, &object, "a", &Local::number(1.0), true).unwrap();
        assert_eq!(
            Object::get(&context, &object, "a").unwrap().as_number(),
            Some(1.0)
        );
        assert!(Object::has(&context, &object, "a").unwrap());
        assert!(Object::delete(&context, &object, "a").unwrap());
        assert!(!Object::has(&context, &object, "a").unwrap());

        let array = Array::new(&context, &[Local::number(1.0), Local::number(2.0)]).unwrap();
        assert_eq!(Array::length(&context, &array).unwrap(), 2.0);
        assert_eq!(
            Array::get(&context, &array, 1).unwrap().as_number(),
            Some(2.0)
        );
        Object::set(&context, &context.global(), "a", &array, true).unwrap();
        assert_eq!(
            context
                .eval("Array.isArray(a)")
                .to_local_checked()
                .as_boolean(),
            Some(true)
        );
    }

    #[test]
    fn json_round_trip() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        let parsed = Json::parse(&context, r#"{"a": 1}"#).unwrap();
        assert_eq!(
            Object::get(&context, &parsed, "a").unwrap().as_number(),
            Some(1.0)
        );
        let text = Json::stringify(&context, &parsed).unwrap();
        assert_eq!(text.as_string().as_deref(), Some(r#"{"a":1}"#));
    }

    #[test]
    fn promise_helpers_read_state_and_run_microtasks() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        let promise = Promise::resolve(&context, &Local::number(42.0)).unwrap();
        assert_eq!(Promise::state(&context, &promise).unwrap(), "fulfilled");
        assert_eq!(
            Promise::result(&context, &promise).unwrap().as_number(),
            Some(42.0)
        );

        let chained = Promise::then(&context, &promise, None, None).unwrap();
        assert_eq!(Promise::state(&context, &chained).unwrap(), "pending");
        context.run_microtasks().unwrap();
        assert_eq!(
            Promise::result(&context, &chained).unwrap().as_number(),
            Some(42.0)
        );
    }

    #[test]
    fn script_compile_surfaces_syntax_errors() {
        let mut isolate = isolate();
        let context = context(&mut isolate);
        assert!(Script::compile(&context, "function (").is_err());
        let script = Script::compile(&context, "1 + 2").unwrap();
        assert_eq!(script.try_run(&context).unwrap().as_number(), Some(3.0));
    }

    #[test]
    fn handles_mirror_v8_shapes() {
        let local = Local::number(1.0);
        let global = Global::new(local);
        assert!(!global.is_empty());
        assert_eq!(global.get().as_number(), Some(1.0));
        let mut empty = Global::empty();
        assert!(empty.is_empty());
        empty.reset(Local::string("hi"));
        assert_eq!(empty.get().as_string().as_deref(), Some("hi"));
        empty.clear();
        assert!(empty.is_empty());

        let _scope = HandleScope::new();
        let maybe = MaybeLocal::Some(Local::number(1.0));
        assert!(!maybe.is_empty());
        assert_eq!(maybe.to_local().unwrap().as_number(), Some(1.0));
        assert!(MaybeLocal::Nothing.to_local().is_none());
    }

    #[test]
    fn isolate_data_slots() {
        let isolate = isolate();
        assert_eq!(isolate.get_data(1), None);
        isolate.set_data(1, 42);
        assert_eq!(isolate.get_data(1), Some(42));
    }
}
