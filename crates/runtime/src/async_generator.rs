//! Async generator objects (spec 27.6): the queue of AsyncGeneratorRequests,
//! the `%AsyncGenerator.prototype%` methods, and the driver that runs the
//! resumable-function IR to each yield/await suspension.
//!
//! The state lives in `agent.async_generators` as `Rc<RefCell<…>>`. Every
//! driver borrow is short-lived: the VM is taken out of the state before it
//! runs, so re-entrant calls (a `next()` inside the body, or the await
//! reactions firing mid-run) can always `borrow_mut` the state again.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::PropertyDescriptor;
use crux::string::JsString;
use crux::value::{Value, is_callable};

use crate::agent::Agent;
use crate::context::ExecutionContext;
use crate::env::EnvRef;
use crate::flow::Completion;
use crate::function::ThisMode;
use crate::ir::{CompiledBody, Resume, Suspension, Vm, VmOutcome};
use crate::promise::{
    PromiseCapability, new_promise_capability, perform_promise_then, promise_resolve,
};
use crate::realm::Realm;

const ASYNC_GENERATOR_PROTO: &str = "%AsyncGenerator.prototype%";
const NEXT: &str = "next";
const RETURN: &str = "return";
const THROW: &str = "throw";

/// [[AsyncGeneratorState]] (spec 27.6.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsyncGeneratorFlag {
    SuspendedStart,
    SuspendedYield,
    Executing,
    AwaitingReturn,
    Completed,
}

/// An AsyncGeneratorRequest (spec 27.6.1.2): the queued completion and the
/// capability of the promise `next`/`return`/`throw` returned.
#[derive(Debug, Clone)]
pub struct AsyncGeneratorRequest {
    pub completion: Resume,
    pub capability: PromiseCapability,
}

/// The agent-side async generator record: the [[AsyncGeneratorState]], the
/// request queue, and the resumable VM plus its execution context. The
/// request currently being driven is kept in `current` while the body awaits.
#[derive(Debug)]
pub struct AsyncGeneratorState {
    pub object: u64,
    pub flag: AsyncGeneratorFlag,
    pub queue: VecDeque<AsyncGeneratorRequest>,
    pub current: Option<AsyncGeneratorRequest>,
    pub vm: Option<Vm>,
    pub body: Option<CompiledBody>,
    pub context: Option<ExecutionContext>,
    pub function: Value,
    pub realm: Handle<Realm>,
    /// The post-instantiation environment: parameters are bound (and errors
    /// surface) when the async generator is *called* (spec
    /// EvaluateAsyncGeneratorBody step 1), and the VM runs against this
    /// environment on the first request.
    pub body_env: Option<EnvRef>,
}

/// The state of an await-resume closure of an async generator body.
#[derive(Debug, Clone)]
pub struct AsyncGeneratorAwaitEntry {
    pub object_id: u64,
    pub is_reject: bool,
}

pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let async_iterator_proto = realm
        .intrinsics
        .get("%AsyncIterator.prototype%")
        .and_then(|value| crate::context::as_object(&value));
    let proto = JsObject::ordinary_object_create(async_iterator_proto);
    let proto_value = Value::Object(proto.clone());
    realm.intrinsics.define(ASYNC_GENERATOR_PROTO, proto_value);
    for (name, length) in [(NEXT, 1), (RETURN, 1), (THROW, 1)] {
        let method = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            Box::new(|_, _| {
                Err(JsError::new(
                    ErrorKind::TypeError,
                    "async generator prototype method must be called through the agent".into(),
                ))
            }),
            None,
            None,
        )?;
        proto.define_property(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: Some(Value::Function(method)),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
    }
    let async_iterator_method = Function::create_builtin(
        Some(JsString::from_utf8("[Symbol.asyncIterator]")),
        0,
        Box::new(|this, _| Ok(this.clone())),
        None,
        realm
            .intrinsics
            .get("%Function.prototype%")
            .and_then(|v| crate::context::as_object(&v)),
    )?;
    proto.define_property_key(
        &crux::property::PropertyKey::Symbol(
            crux::symbol::well_known("asyncIterator").as_ref().clone(),
        ),
        &PropertyDescriptor {
            value: Some(Value::Function(async_iterator_method)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    // spec 27.6.3.3: the tag is an own property of %AsyncGenerator.prototype%
    // with { writable: false, enumerable: false, configurable: true }
    // (AsyncGeneratorPrototype/Symbol.toStringTag.js reads the
    // double-getPrototypeOf chain).
    proto.define_property_key(
        &crux::property::PropertyKey::Symbol(
            crux::symbol::well_known("toStringTag").as_ref().clone(),
        ),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8(
                "AsyncGenerator",
            )))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// Dispatch `%AsyncGenerator.prototype%` methods by identity.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let proto_value = realm.intrinsics.get(ASYNC_GENERATOR_PROTO)?;
    let Value::Object(proto) = &proto_value else {
        return None;
    };
    for (name, handler) in [
        (
            NEXT,
            async_generator_next as fn(&mut Agent, &Value, &[Value]) -> Result<Value, JsError>,
        ),
        (RETURN, async_generator_return),
        (THROW, async_generator_throw),
    ] {
        let Ok(method) = proto.get(&JsString::from_utf8(name)) else {
            continue;
        };
        if method == *callee {
            return Some(handler(agent, this, args));
        }
    }
    None
}

/// AsyncGeneratorFunctionCall: calling an async generator function returns a
/// fresh async generator object (the body does not run until the first
/// request).
pub fn call_async_generator(
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
    let function_env = crate::env::new_function_environment(
        Some(data.environment.clone()),
        function_value.clone(),
        Value::Undefined,
        data.this_mode == ThisMode::Lexical,
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
    // EvaluateAsyncGeneratorBody runs FunctionDeclarationInstantiation at
    // call time, so parameter binding errors (e.g. a throwing @@iterator in a
    // destructuring pattern) throw synchronously (spec 15.6.2).
    agent.execution_context_stack.push(context);
    let instantiate = (|| -> Result<(), JsError> {
        if data.this_mode != ThisMode::Lexical {
            let this = if data.this_mode == ThisMode::Sloppy
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
        Ok(())
    })();
    if let Err(error) = instantiate {
        agent.execution_context_stack.pop();
        return Err(error);
    }
    let instantiated_context = agent
        .execution_context_stack
        .last()
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no context to capture".into()))?;
    let body_env = instantiated_context.lexical_environment.clone();
    agent.execution_context_stack.pop();

    // OrdinaryCreateFromConstructor (spec 10.2.4): the instance inherits the
    // function's own `prototype` property (a per-function object whose
    // prototype is %AsyncGenerator.prototype%), falling back to the intrinsic
    // when that property is not an object. This keeps the
    // double-getPrototypeOf chain of Symbol.toStringTag.js intact.
    let function_value = function_value.clone();
    let prototype = crate::context::get_property(
        agent,
        &function_value,
        &JsString::from_utf8("prototype"),
        function_value.clone(),
    )?;
    let proto = match crate::context::as_object(&prototype) {
        Some(object) => object,
        None => data
            .realm
            .intrinsics
            .get(ASYNC_GENERATOR_PROTO)
            .and_then(|value| crate::context::as_object(&value))
            .ok_or_else(|| {
                JsError::new(
                    ErrorKind::TypeError,
                    format!("{ASYNC_GENERATOR_PROTO} is not defined"),
                )
            })?,
    };
    let object = JsObject::ordinary_object_create(Some(proto));
    let object_value = Value::Object(object.clone());
    agent.async_generators.insert(
        object.id(),
        Rc::new(RefCell::new(AsyncGeneratorState {
            object: object.id(),
            flag: AsyncGeneratorFlag::SuspendedStart,
            queue: VecDeque::new(),
            current: None,
            vm: None,
            body: None,
            context: Some(instantiated_context),
            function: Value::Function(function.clone()),
            realm: data.realm.clone(),
            body_env: Some(body_env),
        })),
    );
    Ok(object_value)
}

/// AsyncGeneratorValidate (spec 27.6.3.3): the `this` value must be an async
/// generator object of this realm's agent.
fn validate(agent: &Agent, this: &Value) -> Result<u64, JsError> {
    let Value::Object(obj) = this else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "AsyncGeneratorResume called on a non-object".into(),
        ));
    };
    if !agent.async_generators.contains_key(&obj.id()) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "AsyncGeneratorResume called on a non-async-generator".into(),
        ));
    }
    Ok(obj.id())
}

fn async_generator_next(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object_id = validate(agent, this)?;
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    enqueue(agent, object_id, Resume::Normal(value))
}

fn async_generator_return(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let object_id = validate(agent, this)?;
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    // spec 27.6.3.5: while the body is executing, `return` marks the state so
    // the current await settles into the queued return (AsyncGeneratorAwaitReturn).
    let state = agent
        .async_generators
        .get(&object_id)
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not an async generator".into()))?;
    if state.borrow().flag == AsyncGeneratorFlag::Executing {
        state.borrow_mut().flag = AsyncGeneratorFlag::AwaitingReturn;
    }
    enqueue(agent, object_id, Resume::Return(value))
}

fn async_generator_throw(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let object_id = validate(agent, this)?;
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    enqueue(agent, object_id, Resume::Throw(value))
}

/// AsyncGeneratorEnqueue (spec 27.6.1.3): append the request and, when the
/// generator is idle, start processing the queue.
fn enqueue(agent: &mut Agent, object_id: u64, completion: Resume) -> Result<Value, JsError> {
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let capability = new_promise_capability(agent, &promise_ctor)?;
    let promise = capability.promise.clone();
    let request = AsyncGeneratorRequest {
        completion,
        capability,
    };
    let flag = {
        let state = agent
            .async_generators
            .get(&object_id)
            .cloned()
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not an async generator".into()))?;
        let mut state = state.borrow_mut();
        state.queue.push_back(request);
        state.flag
    };
    if !matches!(
        flag,
        AsyncGeneratorFlag::Executing | AsyncGeneratorFlag::AwaitingReturn
    ) {
        async_generator_resume_next(agent, object_id)?;
    }
    Ok(promise)
}

/// AsyncGeneratorResumeNext (spec 27.6.3.1): process the queue while the
/// generator is idle.
fn async_generator_resume_next(agent: &mut Agent, object_id: u64) -> Result<(), JsError> {
    loop {
        let flag = agent
            .async_generators
            .get(&object_id)
            .map(|state| state.borrow().flag);
        match flag {
            Some(AsyncGeneratorFlag::Executing)
            | Some(AsyncGeneratorFlag::AwaitingReturn)
            | None => {
                return Ok(());
            }
            Some(AsyncGeneratorFlag::Completed) => {
                let request = {
                    let state =
                        agent
                            .async_generators
                            .get(&object_id)
                            .cloned()
                            .ok_or_else(|| {
                                JsError::new(ErrorKind::TypeError, "not an async generator".into())
                            })?;
                    state.borrow_mut().queue.pop_front()
                };
                let Some(request) = request else {
                    return Ok(());
                };
                resolve_request(agent, &request, Value::Undefined, true)?;
            }
            Some(AsyncGeneratorFlag::SuspendedStart) | Some(AsyncGeneratorFlag::SuspendedYield) => {
                let (request, completion, was_start) = {
                    let state =
                        agent
                            .async_generators
                            .get(&object_id)
                            .cloned()
                            .ok_or_else(|| {
                                JsError::new(ErrorKind::TypeError, "not an async generator".into())
                            })?;
                    let mut state = state.borrow_mut();
                    let Some(request) = state.queue.pop_front() else {
                        return Ok(());
                    };
                    let was_start = state.flag == AsyncGeneratorFlag::SuspendedStart;
                    let completion = request.completion.clone();
                    state.flag = AsyncGeneratorFlag::Executing;
                    state.current = Some(request.clone());
                    (request, completion, was_start)
                };
                match completion {
                    Resume::Return(value) if was_start => {
                        // A suspendedStart generator closes without resuming
                        // (spec 27.6.3.1 step 11: AsyncGeneratorResolve).
                        let state =
                            agent
                                .async_generators
                                .get(&object_id)
                                .cloned()
                                .ok_or_else(|| {
                                    JsError::new(
                                        ErrorKind::TypeError,
                                        "not an async generator".into(),
                                    )
                                })?;
                        let mut state = state.borrow_mut();
                        state.flag = AsyncGeneratorFlag::Completed;
                        state.current = None;
                        drop(state);
                        resolve_request(agent, &request, value, true)?;
                    }
                    Resume::Throw(value) if was_start => {
                        // A suspendedStart generator rejects without resuming.
                        let state =
                            agent
                                .async_generators
                                .get(&object_id)
                                .cloned()
                                .ok_or_else(|| {
                                    JsError::new(
                                        ErrorKind::TypeError,
                                        "not an async generator".into(),
                                    )
                                })?;
                        {
                            let mut state = state.borrow_mut();
                            state.flag = AsyncGeneratorFlag::Completed;
                            state.current = None;
                        }
                        reject_request(agent, &request, value)?;
                    }
                    Resume::Return(value) => {
                        let outcome = resume_body(agent, object_id, Resume::Return(value));
                        drive(agent, object_id, outcome)?;
                    }
                    Resume::Throw(value) => {
                        let outcome = resume_body(agent, object_id, Resume::Throw(value));
                        drive(agent, object_id, outcome)?;
                    }
                    Resume::Normal(value) => {
                        let outcome = if was_start {
                            start_body(agent, object_id)
                        } else {
                            resume_body(agent, object_id, Resume::Normal(value))
                        };
                        drive(agent, object_id, outcome)?;
                    }
                }
            }
        }
    }
}

/// The tail of a drive: save the suspension or settle the current request,
/// then keep processing the queue.
fn drive(
    agent: &mut Agent,
    object_id: u64,
    outcome: Result<VmOutcome, JsError>,
) -> Result<(), JsError> {
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            agent.execution_context_stack.pop();
            let state = agent
                .async_generators
                .get(&object_id)
                .cloned()
                .ok_or_else(|| {
                    JsError::new(ErrorKind::TypeError, "not an async generator".into())
                })?;
            let request = {
                let mut state = state.borrow_mut();
                state.flag = AsyncGeneratorFlag::Completed;
                state.vm = None;
                state.current.take()
            };
            if let Some(request) = request {
                let rejection = crate::promise::error_value(agent, &error);
                crate::function::call(
                    agent,
                    &request.capability.reject,
                    Value::Undefined,
                    &[rejection],
                )?;
            }
            return async_generator_resume_next(agent, object_id);
        }
    };
    match outcome {
        VmOutcome::Suspended(Suspension::Yield { value, .. }) => {
            let context = agent
                .execution_context_stack
                .pop()
                .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no context to pop".into()))?;
            let request = {
                let state = agent
                    .async_generators
                    .get(&object_id)
                    .cloned()
                    .ok_or_else(|| {
                        JsError::new(ErrorKind::TypeError, "not an async generator".into())
                    })?;
                let mut state = state.borrow_mut();
                state.context = Some(context);
                state.flag = AsyncGeneratorFlag::SuspendedYield;
                state.current.take().ok_or_else(|| {
                    JsError::new(ErrorKind::TypeError, "no current request".into())
                })?
            };
            resolve_request(agent, &request, value, false)?;
        }
        VmOutcome::Suspended(Suspension::Await(value)) => {
            let context = agent
                .execution_context_stack
                .pop()
                .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no context to pop".into()))?;
            let state = agent
                .async_generators
                .get(&object_id)
                .cloned()
                .ok_or_else(|| {
                    JsError::new(ErrorKind::TypeError, "not an async generator".into())
                })?;
            state.borrow_mut().context = Some(context);
            attach_await(agent, object_id, value)?;
            return Ok(());
        }
        VmOutcome::Completed(completion) => {
            agent.execution_context_stack.pop();
            let request = {
                let state = agent
                    .async_generators
                    .get(&object_id)
                    .cloned()
                    .ok_or_else(|| {
                        JsError::new(ErrorKind::TypeError, "not an async generator".into())
                    })?;
                let mut state = state.borrow_mut();
                state.flag = AsyncGeneratorFlag::Completed;
                state.vm = None;
                state.current.take().ok_or_else(|| {
                    JsError::new(ErrorKind::TypeError, "no current request".into())
                })?
            };
            match completion {
                Completion::Return(value) | Completion::Normal(value) => {
                    resolve_request(agent, &request, value, true)?;
                }
                Completion::Empty => resolve_request(agent, &request, Value::Undefined, true)?,
                Completion::Throw(value) => reject_request(agent, &request, value)?,
                Completion::Break { .. } | Completion::Continue { .. } => {
                    let error = JsError::new(
                        ErrorKind::SyntaxError,
                        "Illegal control flow in an async generator body".into(),
                    );
                    let rejection = crate::promise::error_value(agent, &error);
                    crate::function::call(
                        agent,
                        &request.capability.reject,
                        Value::Undefined,
                        &[rejection],
                    )?;
                }
            }
        }
    }
    async_generator_resume_next(agent, object_id)
}

/// AsyncGeneratorStart (spec 27.6.1.4): push the context instantiated at
/// call time and run the VM against its environment. Only normal completions
/// reach here (throw/return on suspendedStart close the generator without
/// resuming).
fn start_body(agent: &mut Agent, object_id: u64) -> Result<VmOutcome, JsError> {
    let (context, body_env) = {
        let state = agent
            .async_generators
            .get(&object_id)
            .cloned()
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not an async generator".into()))?;
        let mut state = state.borrow_mut();
        (
            state
                .context
                .take()
                .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no saved context".into()))?,
            state.body_env.clone().ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "no instantiated environment".into())
            })?,
        )
    };
    let function_value = context
        .function
        .clone()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no function in context".into()))?;
    let Value::Function(function_handle) = &function_value else {
        return Err(JsError::new(ErrorKind::TypeError, "not a function".into()));
    };
    let function_id = function_handle.id();
    let data = agent
        .ecma_functions
        .get(&function_id)
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "body not registered".into()))?;
    let body = data.ir.clone().ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "async generator body was not compiled".into(),
        )
    })?;
    agent.execution_context_stack.push(context);
    let mut vm = Vm::new(body_env, data.strict);
    let outcome = vm.start(agent, &body)?;
    let state = agent
        .async_generators
        .get(&object_id)
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not an async generator".into()))?;
    let mut state = state.borrow_mut();
    state.vm = Some(vm);
    state.body = Some(body);
    Ok(outcome)
}

/// Resume a suspended async generator (spec 27.6.3.2): re-push the saved
/// context and drive the VM. The VM is taken out of the state while it runs
/// so re-entrant requests can still reach the queue.
fn resume_body(
    agent: &mut Agent,
    object_id: u64,
    completion: Resume,
) -> Result<VmOutcome, JsError> {
    let (context, body, suspended_at_delegate, mut vm) = {
        let state = agent
            .async_generators
            .get(&object_id)
            .cloned()
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not an async generator".into()))?;
        let mut state = state.borrow_mut();
        let vm = state
            .vm
            .take()
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no suspended VM".into()))?;
        let suspended_at_delegate = vm
            .ip
            .checked_sub(1)
            .and_then(|ip| state.body.as_ref().and_then(|body| body.steps.get(ip)))
            .is_some_and(|step| matches!(step, crate::ir::Step::Yield { delegate: true }));
        (
            state
                .context
                .take()
                .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no saved context".into()))?,
            state
                .body
                .clone()
                .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no saved body".into()))?,
            suspended_at_delegate,
            vm,
        )
    };
    agent.execution_context_stack.push(context);
    let outcome = match completion {
        Resume::Throw(_) | Resume::Return(_) if !suspended_at_delegate => {
            vm.run_abrupt(agent, &body, completion)
        }
        _ => vm.run(agent, &body, completion),
    };
    let state = agent
        .async_generators
        .get(&object_id)
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not an async generator".into()))?;
    state.borrow_mut().vm = Some(vm);
    outcome
}

/// Attach the Await reactions (spec 27.6.3.6): resume the VM on fulfillment
/// or rejection of the awaited value.
fn attach_await(agent: &mut Agent, object_id: u64, value: Value) -> Result<(), JsError> {
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let promise = promise_resolve(agent, &promise_ctor, value)?;
    for is_reject in [false, true] {
        let closure = Function::create_builtin(
            Some(JsString::from_utf8("")),
            1,
            Box::new(|_, _| {
                Err(JsError::new(
                    ErrorKind::TypeError,
                    "async generator await handler must be called through the agent".into(),
                ))
            }),
            None,
            None,
        )?;
        agent.async_generator_awaits.insert(
            closure.id(),
            Rc::new(AsyncGeneratorAwaitEntry {
                object_id,
                is_reject,
            }),
        );
        let handler = Value::Function(closure);
        let (on_fulfilled, on_rejected) = if is_reject {
            (None, Some(handler))
        } else {
            (Some(handler), None)
        };
        perform_promise_then(agent, &promise, on_fulfilled, on_rejected, None)?;
    }
    Ok(())
}

/// Dispatch an await-resume closure by identity.
pub fn dispatch_await(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let Value::Function(function) = callee else {
        return None;
    };
    let entry = agent.async_generator_awaits.get(&function.id()).cloned()?;
    Some(resume_from_await(agent, entry, args))
}

/// The await continuation: resume the body with the awaited value (or
/// rejection), or — when `return()` was called while awaiting — with the
/// queued return completion.
fn resume_from_await(
    agent: &mut Agent,
    entry: Rc<AsyncGeneratorAwaitEntry>,
    args: &[Value],
) -> Result<Value, JsError> {
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    let completion = {
        let state = agent
            .async_generators
            .get(&entry.object_id)
            .cloned()
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not an async generator".into()))?;
        let mut state = state.borrow_mut();
        if state.flag == AsyncGeneratorFlag::AwaitingReturn {
            let request = state.queue.pop_front().ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "no queued return request".into())
            })?;
            state.current = Some(request.clone());
            let completion = request.completion;
            state.flag = AsyncGeneratorFlag::Executing;
            completion
        } else {
            state.flag = AsyncGeneratorFlag::Executing;
            if entry.is_reject {
                Resume::Throw(value)
            } else {
                Resume::Normal(value)
            }
        }
    };
    let outcome = resume_body(agent, entry.object_id, completion);
    drive(agent, entry.object_id, outcome)?;
    Ok(Value::Undefined)
}

/// Reject the request's capability with `value`.
fn reject_request(
    agent: &mut Agent,
    request: &AsyncGeneratorRequest,
    value: Value,
) -> Result<(), JsError> {
    let error = JsError::new(
        ErrorKind::TypeError,
        "Uncaught async generator throw".into(),
    )
    .with_value(value);
    let rejection = crate::promise::error_value(agent, &error);
    crate::function::call(
        agent,
        &request.capability.reject,
        Value::Undefined,
        &[rejection],
    )?;
    Ok(())
}

/// AsyncGeneratorResolve (spec 27.6.3.7): resolve the request's capability
/// with `{ value, done }`, promise-unwrapping thenable values.
fn resolve_request(
    agent: &mut Agent,
    request: &AsyncGeneratorRequest,
    value: Value,
    done: bool,
) -> Result<(), JsError> {
    let thenable = matches!(value, Value::Object(_) | Value::Function(_))
        && crate::context::get_property(agent, &value, &JsString::from_utf8("then"), value.clone())
            .is_ok_and(|then| is_callable(&then));
    if !thenable {
        let result = iterator_result(agent, value, done)?;
        crate::function::call(
            agent,
            &request.capability.resolve,
            Value::Undefined,
            &[result],
        )?;
        return Ok(());
    }
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let promise = promise_resolve(agent, &promise_ctor, value)?;
    let capability = request.capability.clone();
    let resolve_closure = Function::create_builtin(
        Some(JsString::from_utf8("")),
        1,
        Box::new(|_, _| {
            Err(JsError::new(
                ErrorKind::TypeError,
                "async generator resolve handler must be called through the agent".into(),
            ))
        }),
        None,
        None,
    )?;
    agent
        .async_generator_resolvers
        .insert(resolve_closure.id(), (capability, done));
    perform_promise_then(
        agent,
        &promise,
        Some(Value::Function(resolve_closure)),
        Some(request.capability.reject.clone()),
        None,
    )?;
    Ok(())
}

/// The thenable-unwrap continuation: resolve the capability with the settled
/// value.
pub fn dispatch_resolver(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let Value::Function(function) = callee else {
        return None;
    };
    let (capability, done) = agent
        .async_generator_resolvers
        .get(&function.id())
        .cloned()?;
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    let result = match iterator_result(agent, value, done) {
        Ok(result) => result,
        Err(error) => return Some(Err(error)),
    };
    Some(crate::function::call(
        agent,
        &capability.resolve,
        Value::Undefined,
        &[result],
    ))
}

/// CreateIterResultObject (spec 8.4.11): `{ value, done }` with
/// %Object.prototype% as the prototype.
fn iterator_result(agent: &Agent, value: Value, done: bool) -> Result<Value, JsError> {
    let object_proto = agent
        .current_realm()
        .ok()
        .and_then(|realm| realm.intrinsics.get("%Object.prototype%"))
        .and_then(|value| crate::context::as_object(&value));
    let object = JsObject::ordinary_object_create(object_proto);
    object.create_data_property(&JsString::from_utf8("value"), value)?;
    object.create_data_property(&JsString::from_utf8("done"), Value::Boolean(done))?;
    Ok(Value::Object(object))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;

    /// Evaluate a script whose final expression is a promise, drain the job
    /// queue, and return the promise's settled value (or the rejection).
    fn settle(source: &str) -> Result<Value, JsError> {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm()?;
        let value = agent.run_script(source)?;
        agent.run_jobs()?;
        let Value::Object(obj) = &value else {
            return Ok(value.clone());
        };
        let Some(data) = agent.promises.get(&obj.id()) else {
            return Ok(value.clone());
        };
        match &data.borrow().state {
            crate::promise::PromiseState::Fulfilled(v) => Ok(v.clone()),
            crate::promise::PromiseState::Rejected(v) => Ok(v.clone()),
            _ => Err(JsError::new(
                ErrorKind::TypeError,
                "promise not settled".into(),
            )),
        }
    }

    fn str(value: &str) -> Value {
        Value::String(Handle::new(JsString::from_utf8(value)))
    }

    #[test]
    fn async_generator_yields_iteration_results() {
        assert_eq!(
            settle(concat!(
                "(async function () {",
                "  async function* g() { yield 1; yield 2; }",
                "  const it = g();",
                "  const a = await it.next();",
                "  const b = await it.next();",
                "  const c = await it.next();",
                "  return JSON.stringify([a.value, a.done, b.value, b.done, c.value, c.done]);",
                "})()"
            ))
            .unwrap(),
            str("[1,false,2,false,null,true]")
        );
    }

    #[test]
    fn async_generator_awaits_inside_the_body() {
        assert_eq!(
            settle(concat!(
                "(async function () {",
                "  async function* g() { const x = await Promise.resolve(10); yield x; }",
                "  const it = g();",
                "  const a = await it.next();",
                "  return JSON.stringify([a.value, a.done]);",
                "})()"
            ))
            .unwrap(),
            str("[10,false]")
        );
    }

    #[test]
    fn async_generator_return_completes_and_closes() {
        assert_eq!(
            settle(concat!(
                "(async function () {",
                "  async function* g() { yield 1; yield 2; }",
                "  const it = g();",
                "  await it.next();",
                "  const r = await it.return(99);",
                "  const n = await it.next();",
                "  return JSON.stringify([r.value, r.done, n.done]);",
                "})()"
            ))
            .unwrap(),
            str("[99,true,true]")
        );
    }

    #[test]
    fn async_generator_throw_reaches_the_body() {
        assert_eq!(
            settle(concat!(
                "(async function () {",
                "  async function* g() { try { yield 1; } catch (e) { return 'caught:' + e; } }",
                "  const it = g();",
                "  await it.next();",
                "  const r = await it.throw('boom');",
                "  return JSON.stringify([r.value, r.done]);",
                "})()"
            ))
            .unwrap(),
            str("[\"caught:boom\",true]")
        );
    }

    #[test]
    fn async_generator_finally_runs_on_return() {
        assert_eq!(
            settle(concat!(
                "(async function () {",
                "  var marker = '';",
                "  async function* g() { try { yield 1; } finally { marker = 'cleaned'; } }",
                "  const it = g();",
                "  await it.next();",
                "  await it.return(7);",
                "  return marker;",
                "})()"
            ))
            .unwrap(),
            str("cleaned")
        );
    }

    #[test]
    fn async_generator_unwraps_thenable_yields() {
        assert_eq!(
            settle(concat!(
                "(async function () {",
                "  async function* g() { yield Promise.resolve(5); }",
                "  const it = g();",
                "  const a = await it.next();",
                "  return a.value;",
                "})()"
            ))
            .unwrap(),
            Value::Number(5.0)
        );
    }

    #[test]
    fn async_generator_function_constructor_creates_working_generators() {
        assert_eq!(
            settle(concat!(
                "(async function () {",
                "  const AsyncGeneratorFunction = Object.getPrototypeOf(async function* () {}).constructor;",
                "  const g = AsyncGeneratorFunction('n', 'for (let i = 0; i < n; i++) yield i * 2');",
                "  const it = g(3);",
                "  const a = await it.next();",
                "  const b = await it.next();",
                "  return a.value + b.value;",
                "})()"
            ))
            .unwrap(),
            Value::Number(2.0)
        );
    }

    #[test]
    fn async_generator_yield_star_delegates() {
        assert_eq!(
            settle(concat!(
                "(async function () {",
                "  async function* inner() { yield 1; yield 2; }",
                "  async function* outer() { yield* inner(); }",
                "  const it = outer();",
                "  const a = await it.next();",
                "  const b = await it.next();",
                "  return a.value + b.value;",
                "})()"
            ))
            .unwrap(),
            Value::Number(3.0)
        );
    }
}
