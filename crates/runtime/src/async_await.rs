//! Async function machinery (spec 27.7): the async-function driver that runs
//! the resumable-function IR (AsyncFunctionStart / Await), and the
//! AsyncFromSyncIterator helper for `for await`.

use std::cell::RefCell;
use std::rc::Rc;

use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::object::JsObject;
use crux::string::JsString;
use crux::value::{Value, is_callable};

use crate::agent::Agent;
use crate::context::ExecutionContext;
use crate::expr::IteratorRecord;
use crate::flow::Completion;
use crate::ir::{CompiledBody, Resume, Suspension, Vm, VmOutcome};

/// The resumable state of a running async function body: the VM, the saved
/// execution context (re-pushed on each resume), and the promise to settle.
#[derive(Debug)]
pub struct AsyncFunctionState {
    pub vm: Vm,
    pub body: CompiledBody,
    pub context: ExecutionContext,
    pub promise: Value,
    pub resolve: Value,
    pub reject: Value,
}

/// The method of the AsyncFromSyncIterator (spec 27.1.4.3-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsyncFromSyncMethod {
    Next,
    Return,
    Throw,
}

/// An AsyncFromSyncIterator method closure's state.
#[derive(Debug, Clone)]
pub struct AsyncFromSyncEntry {
    pub sync: IteratorRecord,
    pub method: AsyncFromSyncMethod,
}

/// An async-function await-resume handler's state.
#[derive(Debug, Clone)]
pub struct ResumeHandler {
    pub state: Rc<RefCell<AsyncFunctionState>>,
    pub is_reject: bool,
}

impl AsyncFromSyncMethod {
    fn name(self) -> &'static str {
        match self {
            AsyncFromSyncMethod::Next => "next",
            AsyncFromSyncMethod::Return => "return",
            AsyncFromSyncMethod::Throw => "throw",
        }
    }
}

/// AsyncFunctionStart (spec 27.7.4.1): run the body synchronously until the
/// first await, then return the promise.
pub fn call_async_function(
    agent: &mut Agent,
    function: &Handle<Function>,
    this: Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let data = agent
        .ecma_functions
        .get(&function.id())
        .cloned()
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "Function body is not registered".into(),
            )
        })?;
    let function_value = function.self_value();
    let old_env = data.environment.clone();
    let function_env = crate::env::new_function_environment(
        Some(old_env),
        function_value.clone(),
        Value::Undefined,
        data.this_mode == crate::function::ThisMode::Lexical,
    );
    let context = ExecutionContext {
        function: Some(function_value.clone()),
        realm: data.realm.clone(),
        script_or_module: None,
        lexical_environment: function_env.clone(),
        variable_environment: function_env.clone(),
        private_environment: data.private_environment.clone(),
        source: agent
            .running_context()
            .ok()
            .and_then(|context| context.source.clone()),
    };
    agent.execution_context_stack.push(context.clone());
    let result = (|| -> Result<Value, JsError> {
        if data.this_mode != crate::function::ThisMode::Lexical {
            let this = if data.this_mode == crate::function::ThisMode::Sloppy
                && matches!(this, Value::Undefined | Value::Null)
            {
                let global = agent.running_context()?.realm.global_object.clone();
                Value::Object(global)
            } else {
                this
            };
            function_env.bind_this_value(this)?;
        }
        crate::function::function_declaration_instantiation(
            agent,
            &function_value,
            &data,
            args,
            &function_env,
        )?;
        // The VM drives the body's lexical environment (the one
        // function_declaration_instantiation installed on the running
        // context), not the outer function env, so body-level let/const
        // bindings are reachable.
        let body_env = agent.running_context()?.lexical_environment.clone();
        let body = data.ir.clone().ok_or_else(|| {
            JsError::new(ErrorKind::TypeError, "async body was not compiled".into())
        })?;
        let promise_ctor = agent
            .current_realm()?
            .intrinsics
            .get("%Promise%")
            .unwrap_or(Value::Undefined);
        let capability = crate::promise::new_promise_capability(agent, &promise_ctor)?;
        let state = Rc::new(RefCell::new(AsyncFunctionState {
            vm: Vm::new(body_env, data.strict),
            body,
            context,
            promise: capability.promise.clone(),
            resolve: capability.resolve.clone(),
            reject: capability.reject.clone(),
        }));
        let mut state_ref = state.borrow_mut();
        let body = state_ref.body.clone();
        let outcome = state_ref.vm.start(agent, &body)?;
        drop(state_ref);
        match outcome {
            VmOutcome::Completed(completion) => {
                agent.execution_context_stack.pop();
                settle_async(agent, &state, completion)?;
            }
            VmOutcome::Suspended(Suspension::Await(value)) => {
                agent.execution_context_stack.pop();
                attach_await(agent, &state, value)?;
            }
            VmOutcome::Suspended(_) => {
                agent.execution_context_stack.pop();
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "async function suspended on a non-await point".into(),
                ));
            }
        }
        Ok(capability.promise)
    })();
    if result.is_err() {
        agent.execution_context_stack.pop();
    }
    result
}

/// Attach the Await reactions (spec 27.6.3.1): resume the VM on fulfillment
/// or rejection of the awaited value.
pub(crate) fn attach_await(
    agent: &mut Agent,
    state: &Rc<RefCell<AsyncFunctionState>>,
    value: Value,
) -> Result<(), JsError> {
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let promise = crate::promise::promise_resolve(agent, &promise_ctor, value)?;
    let on_fulfilled = make_resume_handler(agent, state.clone(), false)?;
    let on_rejected = make_resume_handler(agent, state.clone(), true)?;
    crate::promise::perform_promise_then(
        agent,
        &promise,
        Some(on_fulfilled),
        Some(on_rejected),
        None,
    )?;
    Ok(())
}

fn make_resume_handler(
    agent: &mut Agent,
    state: Rc<RefCell<AsyncFunctionState>>,
    is_reject: bool,
) -> Result<Value, JsError> {
    let closure = Function::create_builtin(
        Some(JsString::from_utf8("")),
        1,
        Box::new(|_, _| {
            Err(JsError::new(
                ErrorKind::TypeError,
                "async resume handler must be called through the agent".into(),
            ))
        }),
        None,
        None,
    )?;
    agent
        .async_resume
        .insert(closure.id(), Rc::new(ResumeHandler { state, is_reject }));
    Ok(Value::Function(closure))
}

/// Dispatch a resume handler by identity.
pub fn dispatch_resume(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let Value::Function(function) = callee else {
        return None;
    };
    let entry = agent.async_resume.get(&function.id()).cloned()?;
    Some(resume_async(agent, entry, args))
}

fn resume_async(
    agent: &mut Agent,
    entry: Rc<ResumeHandler>,
    args: &[Value],
) -> Result<Value, JsError> {
    let state = entry.state.clone();
    let is_reject = entry.is_reject;
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    let resume = if is_reject {
        Resume::Throw(value)
    } else {
        Resume::Normal(value)
    };
    let context = state.borrow().context.clone();
    agent.execution_context_stack.push(context);
    let body = state.borrow().body.clone();
    let outcome = {
        let mut state = state.borrow_mut();
        state.vm.run_abrupt(agent, &body, resume)
    };
    agent.execution_context_stack.pop();
    match outcome? {
        VmOutcome::Suspended(Suspension::Await(value)) => {
            attach_await(agent, &state, value)?;
        }
        VmOutcome::Suspended(_) => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "async function suspended on a non-await point".into(),
            ));
        }
        VmOutcome::Completed(completion) => {
            settle_async(agent, &state, completion)?;
        }
    }
    Ok(Value::Undefined)
}

/// Resolve or reject the async function's promise with the body completion.
fn settle_async(
    agent: &mut Agent,
    state: &Rc<RefCell<AsyncFunctionState>>,
    completion: Completion,
) -> Result<Value, JsError> {
    let (resolve, reject) = {
        let state = state.borrow();
        (state.resolve.clone(), state.reject.clone())
    };
    match completion {
        Completion::Return(value) | Completion::Normal(value) => {
            crate::function::call(agent, &resolve, Value::Undefined, &[value])?;
        }
        Completion::Empty => {
            crate::function::call(agent, &resolve, Value::Undefined, &[Value::Undefined])?;
        }
        Completion::Throw(value) => {
            crate::function::call(agent, &reject, Value::Undefined, &[value])?;
        }
        Completion::Break { .. } | Completion::Continue { .. } => {
            crate::function::call(
                agent,
                &reject,
                Value::Undefined,
                &[Value::String(Handle::new(JsString::from_utf8(
                    "Illegal control flow in an async body",
                )))],
            )?;
        }
    }
    Ok(Value::Undefined)
}

/// Create the AsyncFromSyncIterator object for a sync iterator (spec
/// 27.1.4.1): `next`/`return`/`throw` wrap the sync methods and return
/// resolved promises.
pub fn async_from_sync_iterator(
    agent: &mut Agent,
    sync: &IteratorRecord,
) -> Result<Handle<JsObject>, JsError> {
    // AsyncFromSyncIterator inherits %AsyncIterator.prototype% (spec
    // 27.1.4.1) so the async iterator helpers are reachable on it.
    let proto = agent
        .current_realm()
        .ok()
        .and_then(|realm| realm.intrinsics.get("%AsyncIterator.prototype%"))
        .and_then(|value| crate::context::as_object(&value));
    let object = JsObject::ordinary_object_create(proto);
    for method in [
        AsyncFromSyncMethod::Next,
        AsyncFromSyncMethod::Return,
        AsyncFromSyncMethod::Throw,
    ] {
        let closure = Function::create_builtin(
            Some(JsString::from_utf8(method.name())),
            1,
            Box::new(|_, _| {
                Err(JsError::new(
                    ErrorKind::TypeError,
                    "AsyncFromSyncIterator method must be called through the agent".into(),
                ))
            }),
            None,
            None,
        )?;
        agent.async_from_sync.insert(
            closure.id(),
            Rc::new(AsyncFromSyncEntry {
                sync: sync.clone(),
                method,
            }),
        );
        object.create_data_property(
            &JsString::from_utf8(method.name()),
            Value::Function(closure),
        )?;
    }
    Ok(object)
}

/// Dispatch an AsyncFromSyncIterator method by identity.
pub fn dispatch_async_from_sync(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let Value::Function(function) = callee else {
        return None;
    };
    let entry = agent.async_from_sync.get(&function.id()).cloned()?;
    Some(run_async_from_sync(agent, entry, args))
}

fn run_async_from_sync(
    agent: &mut Agent,
    entry: Rc<AsyncFromSyncEntry>,
    args: &[Value],
) -> Result<Value, JsError> {
    let sync = entry.sync.clone();
    let method = entry.method;
    let result = match method {
        AsyncFromSyncMethod::Next => {
            crate::function::call(agent, &sync.next, sync.iterator.clone(), args)?
        }
        AsyncFromSyncMethod::Return => {
            let return_method = crate::context::get_property(
                agent,
                &sync.iterator,
                &JsString::from_utf8("return"),
                sync.iterator.clone(),
            )?;
            if is_callable(&return_method) {
                crate::function::call(agent, &return_method, sync.iterator.clone(), args)?
            } else {
                let result = JsObject::ordinary_object_create(None);
                result.create_data_property(
                    &JsString::from_utf8("value"),
                    args.first().cloned().unwrap_or(Value::Undefined),
                )?;
                result.create_data_property(&JsString::from_utf8("done"), Value::Boolean(true))?;
                Value::Object(result)
            }
        }
        AsyncFromSyncMethod::Throw => {
            let throw_method = crate::context::get_property(
                agent,
                &sync.iterator,
                &JsString::from_utf8("throw"),
                sync.iterator.clone(),
            )?;
            if is_callable(&throw_method) {
                crate::function::call(agent, &throw_method, sync.iterator.clone(), args)?
            } else {
                let reason = args.first().cloned().unwrap_or(Value::Undefined);
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "iterator has no throw method".into(),
                )
                .with_value(reason));
            }
        }
    };
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    crate::promise::promise_resolve(agent, &promise_ctor, result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::promise::PromiseState;

    fn number(value: f64) -> Value {
        Value::Number(value)
    }

    /// Run a script whose final expression is a promise, drain the jobs, and
    /// return the settled value.
    fn settle(source: &str) -> Result<Value, JsError> {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let value = agent.run_script(source)?;
        agent.run_jobs()?;
        let Value::Object(obj) = &value else {
            return Ok(value.clone());
        };
        let Some(data) = agent.promises.get(&obj.id()) else {
            return Ok(value.clone());
        };
        match &data.borrow().state {
            PromiseState::Fulfilled(v) => Ok(v.clone()),
            PromiseState::Rejected(v) => Ok(v.clone()),
            PromiseState::Pending { .. } => Err(JsError::new(
                ErrorKind::TypeError,
                "promise still pending".into(),
            )),
        }
    }

    #[test]
    fn async_function_returns_a_promise() {
        let value = settle("async function f() { return 42; } f()").unwrap();
        assert_eq!(value, number(42.0));
    }

    #[test]
    fn async_function_with_await() {
        let value = settle("async function f() { var x = await 10; return x + 5; } f()").unwrap();
        assert_eq!(value, number(15.0));
    }

    #[test]
    fn async_function_body_completion_is_the_resolve_value() {
        // The body's completion value resolves the promise (spec 27.7.4.1).
        assert_eq!(
            settle("async function f() { await 1; } f()").unwrap(),
            number(1.0)
        );
        assert_eq!(
            settle("async function f() { 5; } f()").unwrap(),
            number(5.0)
        );
    }

    #[test]
    fn async_rejection_propagates() {
        let value = settle(
            "async function f() { throw 'bad'; } f().catch(function (e) { return 'caught:' + e; })",
        )
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("caught:bad")))
        );
    }

    #[test]
    fn await_rejection_throws_in_the_body() {
        let value = settle(
            "async function f() { try { await Promise.reject('boom'); } catch (e) { return e + '!'; } } f()",
        )
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("boom!")))
        );
    }

    #[test]
    fn async_awaits_thenables_and_promises() {
        assert_eq!(
            settle("async function f() { return await { then: function (r) { r(77); } }; } f()",)
                .unwrap(),
            number(77.0)
        );
        assert_eq!(
            settle("async function f() { return await Promise.resolve(5); } f()",).unwrap(),
            number(5.0)
        );
    }

    #[test]
    fn async_arrow_functions() {
        assert_eq!(settle("var f = async () => 7; f()").unwrap(), number(7.0));
        assert_eq!(
            settle("var f = async () => await 8; f()").unwrap(),
            number(8.0)
        );
    }

    #[test]
    fn async_microtask_ordering() {
        // Await always yields to the microtask queue.
        let value = settle(
            "var order = ''; \
             async function f() { order += 'a'; await 1; order += 'c'; } \
             var p = f(); order += 'b'; \
             p.then(function () { return order; })",
        )
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("abc")))
        );
    }

    #[test]
    fn async_methods_on_objects() {
        assert_eq!(
            settle("var o = { async m() { return this.x; }, x: 9 }; o.m()",).unwrap(),
            number(9.0)
        );
    }

    #[test]
    fn for_await_of_over_an_async_iterator() {
        // A hand-rolled async iterator over two values, exposed as a global
        // with `@@asyncIterator` (the Symbol builtin does not exist yet).
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let global = agent.running_context().unwrap().realm.global_object.clone();
        global
            .create_data_property(&JsString::from_utf8("aiter"), async_iterable())
            .unwrap();
        let value = agent
            .run_script(
                "var out = ''; \
                 (async function () { \
                   for await (var v of aiter) { out += v + ';'; } \
                   return out; \
                 })()",
            )
            .unwrap();
        agent.run_jobs().unwrap();
        let Value::Object(obj) = &value else {
            panic!("not a promise");
        };
        let data = agent.promises.get(&obj.id()).unwrap();
        let state = data.borrow();
        match &state.state {
            PromiseState::Fulfilled(v) => assert_eq!(
                v.clone(),
                Value::String(Handle::new(JsString::from_utf8("10;20;")))
            ),
            other => panic!("unexpected state {other:?}"),
        }
    }

    /// An object whose `@@asyncIterator` yields two resolved promises.
    fn async_iterable() -> Value {
        let count = std::cell::Cell::new(0u32);
        let next = crux::Function::create_builtin(
            Some(JsString::from_utf8("next")),
            0,
            Box::new(move |_, _| {
                let i = count.get() + 1;
                count.set(i);
                let result = crux::object::JsObject::ordinary_object_create(None);
                if i <= 2 {
                    result
                        .create_data_property(
                            &JsString::from_utf8("value"),
                            Value::Number((i * 10) as f64),
                        )
                        .unwrap();
                    result
                        .create_data_property(&JsString::from_utf8("done"), Value::Boolean(false))
                        .unwrap();
                } else {
                    result
                        .create_data_property(&JsString::from_utf8("value"), Value::Undefined)
                        .unwrap();
                    result
                        .create_data_property(&JsString::from_utf8("done"), Value::Boolean(true))
                        .unwrap();
                }
                // Resolved through the Await machinery (PromiseResolve wraps
                // plain values); the closure cannot reach the agent.
                Ok(Value::Object(result))
            }),
            None,
            None,
        )
        .unwrap();
        let iterator = crux::object::JsObject::ordinary_object_create(None);
        iterator
            .create_data_property(&JsString::from_utf8("next"), Value::Function(next))
            .unwrap();
        let iterable = crux::object::JsObject::ordinary_object_create(None);
        let iterator_for_method = iterator.clone();
        iterable
            .define_property_key(
                &crux::property::PropertyKey::Symbol(
                    crux::symbol::well_known("asyncIterator").as_ref().clone(),
                ),
                &crux::property::PropertyDescriptor::data(Value::Function(
                    crux::Function::create_builtin(
                        Some(JsString::from_utf8("[Symbol.asyncIterator]")),
                        0,
                        Box::new(move |_, _| Ok(Value::Object(iterator_for_method.clone()))),
                        None,
                        None,
                    )
                    .unwrap(),
                )),
            )
            .unwrap();
        Value::Object(iterable)
    }

    #[test]
    fn for_await_of_wraps_sync_iterators() {
        // A sync iterable is wrapped in AsyncFromSyncIterator.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let global = agent.running_context().unwrap().realm.global_object.clone();
        global
            .create_data_property(
                &JsString::from_utf8("iter"),
                iterable(vec![number(1.0), number(2.0)]),
            )
            .unwrap();
        let value = agent
            .run_script(
                "var out = ''; \
                 (async function () { \
                   for await (var v of iter) { out += v + ';'; } \
                   return out; \
                 })()",
            )
            .unwrap();
        agent.run_jobs().unwrap();
        let Value::Object(obj) = &value else {
            panic!("not a promise");
        };
        let data = agent.promises.get(&obj.id()).unwrap();
        let state = data.borrow();
        match &state.state {
            PromiseState::Fulfilled(v) => {
                assert_eq!(
                    v.clone(),
                    Value::String(Handle::new(JsString::from_utf8("1;2;")))
                );
            }
            other => panic!("unexpected state {other:?}"),
        }
    }

    /// A native iterable over the given values (the `@@iterator` stand-in).
    fn iterable(values: Vec<Value>) -> Value {
        let values_clone = values.clone();
        let index = std::cell::Cell::new(0usize);
        let next = crux::Function::create_builtin(
            Some(JsString::from_utf8("next")),
            0,
            Box::new(move |_, _| {
                let i = index.get();
                let result = crux::object::JsObject::ordinary_object_create(None);
                if i < values_clone.len() {
                    index.set(i + 1);
                    result
                        .create_data_property(
                            &JsString::from_utf8("value"),
                            values_clone[i].clone(),
                        )
                        .unwrap();
                    result
                        .create_data_property(&JsString::from_utf8("done"), Value::Boolean(false))
                        .unwrap();
                } else {
                    result
                        .create_data_property(&JsString::from_utf8("value"), Value::Undefined)
                        .unwrap();
                    result
                        .create_data_property(&JsString::from_utf8("done"), Value::Boolean(true))
                        .unwrap();
                }
                Ok(Value::Object(result))
            }),
            None,
            None,
        )
        .unwrap();
        let iterator = crux::object::JsObject::ordinary_object_create(None);
        iterator
            .create_data_property(&JsString::from_utf8("next"), Value::Function(next))
            .unwrap();
        let iterable = crux::object::JsObject::ordinary_object_create(None);
        let iterator_for_method = iterator.clone();
        iterable
            .define_property_key(
                &crux::property::PropertyKey::Symbol(
                    crux::symbol::well_known("iterator").as_ref().clone(),
                ),
                &crux::property::PropertyDescriptor::data(Value::Function(
                    crux::Function::create_builtin(
                        Some(JsString::from_utf8("[Symbol.iterator]")),
                        0,
                        Box::new(move |_, _| Ok(Value::Object(iterator_for_method.clone()))),
                        None,
                        None,
                    )
                    .unwrap(),
                )),
            )
            .unwrap();
        Value::Object(iterable)
    }
}
