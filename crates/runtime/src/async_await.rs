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
use crux::value::{Value, ValueKind, is_callable};

use crate::agent::Agent;
use crate::context::ExecutionContext;
use crate::expr::IteratorRecord;
use crate::flow::Completion;
use crate::ir::{CompiledBody, Resume, Suspension, Vm, VmOutcome};
use crate::promise::{
    PromiseCapability, new_promise_capability, perform_promise_then, promise_resolve,
};

/// The resumable state of a running async function body: the VM, the saved
/// execution context (re-pushed on each resume), and the promise to settle.
/// A module body also records its SourceTextModule so completion settles the
/// module record (status, evaluation error, async-parent propagation).
#[derive(Debug)]
pub struct AsyncFunctionState {
    pub vm: Vm,
    pub body: CompiledBody,
    pub context: ExecutionContext,
    pub promise: Value,
    pub resolve: Value,
    pub reject: Value,
    pub module: Option<crux::handle::Handle<crate::module::SourceTextModule>>,
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
        annex_b_hoistable: Default::default(),
    };
    agent.execution_context_stack.push(context.clone());
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let capability = crate::promise::new_promise_capability(agent, &promise_ctor)?;
    (|| -> Result<Value, JsError> {
        // Any failure — parameter binding (defaults may eval), the VM's
        // initial run, or a settle/attach hook — rejects the promise: an
        // async function never throws synchronously (spec 27.7.4.1).
        let run = || -> Result<Value, JsError> {
            if data.this_mode != crate::function::ThisMode::Lexical {
                // OrdinaryCallBindThis (spec 10.2.1): sloppy functions coerce
                // undefined/null to the global object and box primitives.
                let this = if data.this_mode == crate::function::ThisMode::Sloppy {
                    match this.kind() {
                        ValueKind::Undefined | ValueKind::Null => {
                            let global = agent.running_context()?.realm.global_object.clone();
                            Value::Object(global)
                        }
                        ValueKind::Object(_) | ValueKind::Function(_) => this,
                        _ => crate::context::to_object(agent, &this)?,
                    }
                } else {
                    this
                };
                function_env.bind_this_value(this)?;
            }
            crate::function::function_declaration_instantiation(
                agent,
                &function_value,
                &data.params,
                &data.body,
                data.this_mode,
                data.strict,
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
            let state = Rc::new(RefCell::new(AsyncFunctionState {
                vm: Vm::new(body_env, data.strict),
                body,
                context,
                promise: capability.promise.clone(),
                resolve: capability.resolve.clone(),
                reject: capability.reject.clone(),
                module: None,
            }));
            let mut state_ref = state.borrow_mut();
            let body = state_ref.body.clone();
            let outcome = state_ref.vm.start(agent, &body)?;
            drop(state_ref);
            match outcome {
                VmOutcome::Completed(completion) => {
                    agent.execution_context_stack.pop();
                    settle_async_completion(agent, &state, completion)?;
                }
                VmOutcome::Suspended(Suspension::Await(value)) => {
                    agent.execution_context_stack.pop();
                    attach_await(agent, &state, value)?;
                }
                VmOutcome::Suspended(_) => {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "async function suspended on a non-await point".into(),
                    ));
                }
            }
            Ok(capability.promise.clone())
        };
        match run() {
            Ok(value) => Ok(value),
            Err(error) => {
                agent.execution_context_stack.pop();
                let rejection = crate::promise::error_value(agent, &error);
                crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
                Ok(capability.promise)
            }
        }
    })()
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
    let ValueKind::Function(function) = callee.kind() else {
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
    let outcome = match outcome {
        Ok(outcome) => outcome,
        // A step error in a resumed body rejects the function's promise — the
        // await reaction must not surface a synchronous throw.
        Err(error) => {
            let (reject, promise) = {
                let state = state.borrow();
                (state.reject.clone(), state.promise.clone())
            };
            let rejection = crate::promise::error_value(agent, &error);
            crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
            return Ok(promise);
        }
    };
    match outcome {
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
            settle_async_completion(agent, &state, completion)?;
        }
    }
    Ok(Value::Undefined)
}

/// Settle the async function's promise with the body completion, disposing
/// the body's `using` resources first (spec 15.8.5.2 steps 9-11). Async
/// resources suspend through the job queue: the driver folds each disposal
/// into the completion and settles the promise when the stack drains.
fn settle_async_completion(
    agent: &mut Agent,
    state: &Rc<RefCell<AsyncFunctionState>>,
    completion: Completion,
) -> Result<(), JsError> {
    let resources = {
        let state = state.borrow();
        state.vm.lexical_env.drain_disposable_resources()
    };
    if resources.is_empty() {
        settle_async(agent, state, completion)?;
        return Ok(());
    }
    let (resolve, reject) = {
        let state = state.borrow();
        (state.resolve.clone(), state.reject.clone())
    };
    crate::builtins::disposable::dispose_async_body_resources(
        agent,
        resources,
        completion,
        crate::builtins::disposable::AsyncBodySettlement::Function { resolve, reject },
    )
}

/// Resolve or reject the async function's promise with the body completion.
fn settle_async(
    agent: &mut Agent,
    state: &Rc<RefCell<AsyncFunctionState>>,
    completion: Completion,
) -> Result<Value, JsError> {
    // A module body's completion settles the module record too (status,
    // evaluation error, and async-parent propagation, spec 16.2.2.5).
    if let Some(module) = state.borrow().module.clone() {
        crate::module::finish_module_evaluation(agent, &module, state, completion)?;
        return Ok(Value::Undefined);
    }
    let (resolve, reject) = {
        let state = state.borrow();
        (state.resolve.clone(), state.reject.clone())
    };
    match completion {
        // Only a `return` completion carries a value; a normal completion
        // resolves the promise with *undefined*.
        Completion::Return(value) => {
            crate::function::call(agent, &resolve, Value::Undefined, &[value])?;
        }
        Completion::Normal(_) | Completion::Empty => {
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
    let ValueKind::Function(function) = callee.kind() else {
        return None;
    };
    let entry = agent.async_from_sync.get(&function.id()).cloned()?;
    Some(run_async_from_sync(agent, entry, args))
}

/// The continuation state of an AsyncFromSyncIterator value-unwrap (spec
/// 27.1.5.4): `done` was already read from the sync result, and the closure
/// settles the wrapper's capability once the value's promise settles.
#[derive(Debug, Clone)]
pub struct AsyncFromSyncContinuationEntry {
    pub capability: PromiseCapability,
    pub sync: IteratorRecord,
    pub done: bool,
    pub is_reject: bool,
    pub close_on_rejection: bool,
}

/// Run one AsyncFromSyncIterator method (spec 27.1.5.2): call the sync
/// method, then AsyncFromSyncIteratorContinuation — the wrapper's promise
/// resolves with `{ value, done }` where `value` is promise-unwrapped (and,
/// on rejection, the sync iterator is closed when the result was not done).
fn run_async_from_sync(
    agent: &mut Agent,
    entry: Rc<AsyncFromSyncEntry>,
    args: &[Value],
) -> Result<Value, JsError> {
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let capability = new_promise_capability(agent, &promise_ctor)?;
    let promise = capability.promise.clone();
    let sync = entry.sync.clone();
    let method = entry.method;
    let result = match method {
        AsyncFromSyncMethod::Next => {
            let next = crate::expr::iterator_next_method(agent, &sync)?;
            crate::function::call(agent, &next, sync.iterator.clone(), args)
        }
        AsyncFromSyncMethod::Return => {
            let return_method = crate::context::get_property(
                agent,
                &sync.iterator,
                &JsString::from_utf8("return"),
                sync.iterator.clone(),
            )?;
            if is_callable(&return_method) {
                crate::function::call(agent, &return_method, sync.iterator.clone(), args)
            } else {
                // spec 27.1.5.2.2 steps 8-9: no return method resolves with
                // `{ value: (the argument), done: true }` directly.
                let result = JsObject::ordinary_object_create(None);
                result.create_data_property(
                    &JsString::from_utf8("value"),
                    args.first().cloned().unwrap_or(Value::Undefined),
                )?;
                result.create_data_property(&JsString::from_utf8("done"), Value::Boolean(true))?;
                crate::function::call(
                    agent,
                    &capability.resolve,
                    Value::Undefined,
                    &[Value::Object(result)],
                )?;
                return Ok(promise);
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
                crate::function::call(agent, &throw_method, sync.iterator.clone(), args)
            } else {
                // spec 27.1.5.2.3 steps 8-9: no throw method closes the
                // iterator (for finally blocks) and rejects with a TypeError.
                if let Err(error) = crate::expr::iterator_close(agent, &sync) {
                    let rejection = crate::promise::error_value(agent, &error);
                    crate::function::call(
                        agent,
                        &capability.reject,
                        Value::Undefined,
                        &[rejection],
                    )?;
                    return Ok(promise);
                }
                let error =
                    JsError::new(ErrorKind::TypeError, "iterator has no throw method".into());
                let rejection = crate::promise::error_value(agent, &error);
                crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
                return Ok(promise);
            }
        }
    };
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            // IfAbruptRejectPromise (spec 27.1.5.2.1 step 6): the wrapper's
            // promise rejects instead of throwing.
            let rejection = crate::promise::error_value(agent, &error);
            crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
            return Ok(promise);
        }
    };
    if !matches!(result.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
        // spec 27.1.5.2.2/3: a non-object result rejects with a fresh
        // TypeError.
        let error = JsError::new(
            ErrorKind::TypeError,
            "iterator result is not an object".into(),
        );
        let rejection = crate::promise::error_value(agent, &error);
        crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
        return Ok(promise);
    }
    // AsyncFromSyncIteratorContinuation (spec 27.1.5.4) steps 2-5.
    let done = match crate::context::get_property(
        agent,
        &result,
        &JsString::from_utf8("done"),
        result.clone(),
    ) {
        Ok(done) => crux::convert::to_boolean(&done),
        Err(error) => {
            let rejection = crate::promise::error_value(agent, &error);
            crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
            return Ok(promise);
        }
    };
    let value = match crate::context::get_property(
        agent,
        &result,
        &JsString::from_utf8("value"),
        result.clone(),
    ) {
        Ok(value) => value,
        Err(error) => {
            let rejection = crate::promise::error_value(agent, &error);
            crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
            return Ok(promise);
        }
    };
    // Steps 6-7: a throwing PromiseResolve (e.g. a broken promise) closes
    // the sync iterator when the result was not done. AsyncIteratorClose
    // with the throw completion: the original error wins, so a throwing
    // `return` (or a non-object `return` result) is swallowed (spec
    // 27.1.5.4 steps 5-6).
    let close_on_rejection = method != AsyncFromSyncMethod::Return;
    let value_wrapper = match promise_resolve(agent, &promise_ctor, value.clone()) {
        Ok(promise) => promise,
        Err(error) => {
            if !done && close_on_rejection {
                let _ = crate::expr::iterator_close_throw(agent, &sync);
            }
            let rejection = crate::promise::error_value(agent, &error);
            crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
            return Ok(promise);
        }
    };
    // Steps 8-11: unwrap the value; on rejection, close the sync iterator
    // (spec 27.1.5.4 steps 10-11) when the result was not done.
    for is_reject in [false, true] {
        if is_reject && (done || !close_on_rejection) {
            continue;
        }
        let closure = Function::create_builtin(
            Some(JsString::from_utf8("")),
            1,
            Box::new(|_, _| {
                Err(JsError::new(
                    ErrorKind::TypeError,
                    "async-from-sync continuation must be called through the agent".into(),
                ))
            }),
            None,
            None,
        )?;
        agent.async_from_sync_continuations.insert(
            closure.id(),
            Rc::new(AsyncFromSyncContinuationEntry {
                capability: capability.clone(),
                sync: sync.clone(),
                done,
                is_reject,
                close_on_rejection,
            }),
        );
        let handler = Value::Function(closure);
        let (on_fulfilled, on_rejected) = if is_reject {
            (None, Some(handler))
        } else {
            (Some(handler), None)
        };
        perform_promise_then(agent, &value_wrapper, on_fulfilled, on_rejected, None)?;
    }
    Ok(promise)
}

/// Dispatch an AsyncFromSyncIterator value-unwrap continuation by identity.
pub fn dispatch_async_from_sync_continuation(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let ValueKind::Function(function) = callee.kind() else {
        return None;
    };
    let entry = agent
        .async_from_sync_continuations
        .get(&function.id())
        .cloned()?;
    Some(resume_async_from_sync_continuation(agent, entry, args))
}

fn resume_async_from_sync_continuation(
    agent: &mut Agent,
    entry: Rc<AsyncFromSyncContinuationEntry>,
    args: &[Value],
) -> Result<Value, JsError> {
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    if entry.is_reject {
        // The yielded value rejected: close the sync iterator when the
        // result was not done, then reject the wrapper's promise.
        // AsyncIteratorClose with a throw completion (spec 27.1.5.4 steps
        // 13-14): the original rejection wins, so a throwing or non-object
        // `return` result is swallowed.
        if !entry.done && entry.close_on_rejection {
            let _ = crate::expr::iterator_close_throw(agent, &entry.sync);
        }
        crate::function::call(agent, &entry.capability.reject, Value::Undefined, &[value])?;
        return Ok(Value::Undefined);
    }
    let result = JsObject::ordinary_object_create(None);
    result.create_data_property(&JsString::from_utf8("value"), value)?;
    result.create_data_property(&JsString::from_utf8("done"), Value::Boolean(entry.done))?;
    crate::function::call(
        agent,
        &entry.capability.resolve,
        Value::Undefined,
        &[Value::Object(result)],
    )?;
    Ok(Value::Undefined)
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
        let ValueKind::Object(obj) = value.kind() else {
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
        // The promise resolves with the body's completion (spec 27.7.4.1
        // AsyncFunctionStart): only a `return` completion carries a value;
        // any other normal completion resolves with *undefined*.
        assert_eq!(
            settle("async function f() { await 1; } f()").unwrap(),
            Value::Undefined
        );
        assert_eq!(
            settle("async function f() { 5; } f()").unwrap(),
            Value::Undefined
        );
        assert_eq!(
            settle("async function f() { return 5; } f()").unwrap(),
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
        let ValueKind::Object(obj) = value.kind() else {
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
        let ValueKind::Object(obj) = value.kind() else {
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
