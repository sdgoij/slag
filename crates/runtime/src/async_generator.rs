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
use crux::value::{Value, ValueKind};

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
    pub body: Option<Rc<CompiledBody>>,
    pub context: Option<ExecutionContext>,
    pub function: Value,
    pub realm: Handle<Realm>,
    /// The post-instantiation environment: parameters are bound (and errors
    /// surface) when the async generator is *called* (spec
    /// EvaluateAsyncGeneratorBody step 1), and the VM runs against this
    /// environment on the first request.
    pub body_env: Option<EnvRef>,
}

/// What an await-resume closure of an async generator body does when the
/// promise settles (spec 27.9.3): resume the body from its own `await`
/// (`Body`), run the AsyncGeneratorYield continuation after a `yield`
/// (`Yield`), deliver a `return()` value into a suspended yield
/// (`ReturnResume`), or complete the request of a `return()` on a
/// suspended-start/completed generator (`AwaitReturn`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwaitKind {
    Body,
    Yield,
    ReturnResume,
    AwaitReturn,
}

/// The state of an await-resume closure of an async generator body.
#[derive(Debug, Clone)]
pub struct AsyncGeneratorAwaitEntry {
    pub object_id: u64,
    pub is_reject: bool,
    pub kind: AwaitKind,
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
        // Registered as an intrinsic so a method of one realm called from
        // another dispatches with its own realm current.
        realm.intrinsics.define(
            &format!("%AsyncGenerator.prototype.{name}%"),
            Value::Function(method.clone()),
        );
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
    let ValueKind::Object(proto) = proto_value.kind() else {
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
        annex_b_hoistable: Default::default(),
    };
    // EvaluateAsyncGeneratorBody runs FunctionDeclarationInstantiation at
    // call time, so parameter binding errors (e.g. a throwing @@iterator in a
    // destructuring pattern) throw synchronously (spec 15.6.2).
    agent.execution_context_stack.push(context);
    let instantiate = (|| -> Result<(), JsError> {
        if data.this_mode != ThisMode::Lexical {
            // OrdinaryCallBindThis (spec 10.2.1): sloppy functions coerce
            // undefined/null to the global object and box primitives.
            let this = if data.this_mode == ThisMode::Sloppy {
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
    let ValueKind::Object(obj) = this.kind() else {
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

/// NewPromiseCapability(%Promise%) of the current realm (spec 27.9.1.2
/// step 2): every request promise comes from the realm that called the
/// method.
fn new_capability(agent: &mut Agent) -> Result<PromiseCapability, JsError> {
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    new_promise_capability(agent, &promise_ctor)
}

fn state(agent: &Agent, object_id: u64) -> Result<AsyncGeneratorFlag, JsError> {
    let state = agent
        .async_generators
        .get(&object_id)
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not an async generator".into()))?;
    Ok(state.borrow().flag)
}

fn set_flag(agent: &Agent, object_id: u64, flag: AsyncGeneratorFlag) -> Result<(), JsError> {
    let state = agent
        .async_generators
        .get(&object_id)
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not an async generator".into()))?;
    state.borrow_mut().flag = flag;
    Ok(())
}

fn set_current(
    agent: &Agent,
    object_id: u64,
    request: AsyncGeneratorRequest,
) -> Result<(), JsError> {
    let state = agent
        .async_generators
        .get(&object_id)
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not an async generator".into()))?;
    state.borrow_mut().current = Some(request);
    Ok(())
}

fn take_current(agent: &Agent, object_id: u64) -> Result<Option<AsyncGeneratorRequest>, JsError> {
    let state = agent
        .async_generators
        .get(&object_id)
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not an async generator".into()))?;
    Ok(state.borrow_mut().current.take())
}

fn save_context(agent: &Agent, object_id: u64, context: ExecutionContext) -> Result<(), JsError> {
    let state = agent
        .async_generators
        .get(&object_id)
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not an async generator".into()))?;
    state.borrow_mut().context = Some(context);
    Ok(())
}

/// AsyncGeneratorEnqueue (spec 27.9.3.4): append the request. The prototype
/// methods decide whether the generator is resumed.
fn push_request(
    agent: &Agent,
    object_id: u64,
    request: AsyncGeneratorRequest,
) -> Result<(), JsError> {
    let state = agent
        .async_generators
        .get(&object_id)
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not an async generator".into()))?;
    state.borrow_mut().queue.push_back(request);
    Ok(())
}

fn pop_front(
    agent: &Agent,
    object_id: u64,
) -> Result<Option<(AsyncGeneratorRequest, Resume)>, JsError> {
    let state = agent
        .async_generators
        .get(&object_id)
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not an async generator".into()))?;
    let mut state = state.borrow_mut();
    let Some(request) = state.queue.pop_front() else {
        return Ok(None);
    };
    let completion = request.completion.clone();
    Ok(Some((request, completion)))
}

/// %AsyncGeneratorPrototype%.next (spec 27.9.1.2): the capability is created
/// before the `this` check, so a bad generator rejects the promise instead of
/// throwing synchronously.
fn async_generator_next(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let capability = new_capability(agent)?;
    let promise = capability.promise.clone();
    let object_id = match validate(agent, this) {
        Ok(id) => id,
        Err(error) => {
            // spec 27.9.1.2 step 4: IfAbruptRejectPromise.
            let rejection = crate::promise::error_value(agent, &error);
            crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
            return Ok(promise);
        }
    };
    let flag = state(agent, object_id)?;
    if flag == AsyncGeneratorFlag::Completed {
        // spec 27.9.1.2 steps 6-7: a completed generator resolves with
        // { value: undefined, done: true } without enqueueing.
        let result = iterator_result(agent, Value::Undefined, true)?;
        crate::function::call(agent, &capability.resolve, Value::Undefined, &[result])?;
        return Ok(promise);
    }
    push_request(
        agent,
        object_id,
        AsyncGeneratorRequest {
            completion: Resume::Normal(args.first().cloned().unwrap_or(Value::Undefined)),
            capability,
        },
    )?;
    if matches!(
        flag,
        AsyncGeneratorFlag::SuspendedStart | AsyncGeneratorFlag::SuspendedYield
    ) {
        async_generator_resume_next(agent, object_id)?;
    }
    Ok(promise)
}

/// %AsyncGeneratorPrototype%.return (spec 27.9.1.3).
fn async_generator_return(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let capability = new_capability(agent)?;
    let promise = capability.promise.clone();
    let object_id = match validate(agent, this) {
        Ok(id) => id,
        Err(error) => {
            let rejection = crate::promise::error_value(agent, &error);
            crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
            return Ok(promise);
        }
    };
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    let flag = state(agent, object_id)?;
    push_request(
        agent,
        object_id,
        AsyncGeneratorRequest {
            completion: Resume::Return(value.clone()),
            capability,
        },
    )?;
    if matches!(
        flag,
        AsyncGeneratorFlag::SuspendedStart | AsyncGeneratorFlag::Completed
    ) {
        // spec 27.9.1.3 steps 8-9: the generator never resumes; the return
        // value is awaited and the request completed from the continuation.
        let request = pop_front(agent, object_id)?.map(|(request, _)| request);
        if let Some(request) = request {
            set_current(agent, object_id, request)?;
        }
        set_flag(agent, object_id, AsyncGeneratorFlag::AwaitingReturn)?;
        await_return(agent, object_id, value)?;
    } else if flag == AsyncGeneratorFlag::SuspendedYield {
        async_generator_resume_next(agent, object_id)?;
    }
    Ok(promise)
}

/// %AsyncGeneratorPrototype%.throw (spec 27.9.1.4): throw values are never
/// unwrapped (the fixtures reject with the exception itself).
fn async_generator_throw(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let capability = new_capability(agent)?;
    let promise = capability.promise.clone();
    let object_id = match validate(agent, this) {
        Ok(id) => id,
        Err(error) => {
            let rejection = crate::promise::error_value(agent, &error);
            crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
            return Ok(promise);
        }
    };
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    let flag = state(agent, object_id)?;
    if flag == AsyncGeneratorFlag::SuspendedStart {
        set_flag(agent, object_id, AsyncGeneratorFlag::Completed)?;
    }
    if state(agent, object_id)? == AsyncGeneratorFlag::Completed {
        // spec 27.9.1.4 steps 7-8: completed generators reject directly.
        crate::function::call(agent, &capability.reject, Value::Undefined, &[value])?;
        return Ok(promise);
    }
    push_request(
        agent,
        object_id,
        AsyncGeneratorRequest {
            completion: Resume::Throw(value),
            capability,
        },
    )?;
    if flag == AsyncGeneratorFlag::SuspendedYield {
        async_generator_resume_next(agent, object_id)?;
    }
    Ok(promise)
}

/// AsyncGeneratorResumeNext (spec 27.9.3.6): while the generator is idle,
/// run the next queued request; once the body has completed, resolve the
/// remaining requests without resuming (spec 27.9.3.10 drain).
fn async_generator_resume_next(agent: &mut Agent, object_id: u64) -> Result<(), JsError> {
    loop {
        let flag = state(agent, object_id)?;
        match flag {
            AsyncGeneratorFlag::Executing | AsyncGeneratorFlag::AwaitingReturn => {
                return Ok(());
            }
            AsyncGeneratorFlag::Completed => {
                let Some((request, completion)) = pop_front(agent, object_id)? else {
                    return Ok(());
                };
                match completion {
                    Resume::Return(value) => {
                        // spec 27.9.3.10: a queued return sets awaiting-return
                        // and awaits the value (AsyncGeneratorAwaitReturn).
                        set_flag(agent, object_id, AsyncGeneratorFlag::AwaitingReturn)?;
                        set_current(agent, object_id, request)?;
                        await_return(agent, object_id, value)?;
                        return Ok(());
                    }
                    Resume::Throw(value) => {
                        complete_step(agent, &request, Completion::Throw(value), true)?;
                    }
                    Resume::Normal(_) => {
                        complete_step(agent, &request, Completion::Normal(Value::Undefined), true)?;
                    }
                }
            }
            AsyncGeneratorFlag::SuspendedStart | AsyncGeneratorFlag::SuspendedYield => {
                let was_start = flag == AsyncGeneratorFlag::SuspendedStart;
                let Some((request, completion)) = pop_front(agent, object_id)? else {
                    return Ok(());
                };
                set_current(agent, object_id, request.clone())?;
                set_flag(agent, object_id, AsyncGeneratorFlag::Executing)?;
                match completion {
                    // A suspended-start generator closes without resuming
                    // (spec 27.9.1.4 steps 6-7 handle this before enqueueing,
                    // so these branches are defensive).
                    Resume::Return(value) if was_start => {
                        set_flag(agent, object_id, AsyncGeneratorFlag::Completed)?;
                        complete_step(agent, &request, Completion::Normal(value), true)?;
                    }
                    Resume::Throw(value) if was_start => {
                        set_flag(agent, object_id, AsyncGeneratorFlag::Completed)?;
                        complete_step(agent, &request, Completion::Throw(value), true)?;
                    }
                    Resume::Normal(value) => {
                        let outcome = if was_start {
                            start_body(agent, object_id)
                        } else {
                            resume_body(agent, object_id, Resume::Normal(value))
                        };
                        drive(agent, object_id, outcome)?;
                    }
                    Resume::Return(value) => {
                        // AsyncGeneratorUnwrapYieldResumption (spec
                        // 27.9.3.7): a return resumption awaits its value
                        // before the body sees it.
                        resume_with_return(agent, object_id, value)?;
                    }
                    Resume::Throw(value) => {
                        let outcome = resume_body(agent, object_id, Resume::Throw(value));
                        drive(agent, object_id, outcome)?;
                    }
                }
            }
        }
    }
}

/// The tail of a drive: save the suspension (awaiting the yield value or a
/// body await) or settle the current request when the body completes, then
/// keep processing the queue.
fn drive(
    agent: &mut Agent,
    object_id: u64,
    outcome: Result<VmOutcome, JsError>,
) -> Result<(), JsError> {
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            agent.execution_context_stack.pop();
            let request = take_current(agent, object_id)?;
            set_flag(agent, object_id, AsyncGeneratorFlag::Completed)?;
            if let Some(request) = request {
                let rejection = crate::promise::error_value(agent, &error);
                complete_step(agent, &request, Completion::Throw(rejection), true)?;
            }
            return async_generator_resume_next(agent, object_id);
        }
    };
    match outcome {
        // `run_inner`'s driver consumes tail calls internally; an escaped one
        // is an internal invariant violation.
        VmOutcome::TailCall(_) => Err(JsError::new(
            ErrorKind::TypeError,
            "tail call escaped the async-generator driver".into(),
        )),
        VmOutcome::Suspended(Suspension::Yield {
            value,
            delegate: true,
        }) => {
            // A delegated `yield`: the value came from the inner iterator's
            // already-awaited result, so AsyncGeneratorYield completes the
            // current request with it directly — it is not awaited again
            // (spec 15.5.5 normal case: AsyncGeneratorYield(IteratorValue)).
            let context = agent
                .execution_context_stack
                .pop()
                .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no context to pop".into()))?;
            save_context(agent, object_id, context)?;
            resume_from_yield(agent, object_id, value)?;
            Ok(())
        }
        VmOutcome::Suspended(Suspension::Yield { value, .. }) => {
            let context = agent
                .execution_context_stack
                .pop()
                .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no context to pop".into()))?;
            save_context(agent, object_id, context)?;
            // spec 27.8.3.7: `yield arg` awaits arg first, so the value
            // reaches AsyncGeneratorYield unwrapped. The state stays
            // executing while that await is pending, so queued requests wait.
            attach_await(agent, object_id, value, AwaitKind::Yield)?;
            Ok(())
        }
        VmOutcome::Suspended(Suspension::Await(value)) => {
            let context = agent
                .execution_context_stack
                .pop()
                .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no context to pop".into()))?;
            save_context(agent, object_id, context)?;
            attach_await(agent, object_id, value, AwaitKind::Body)?;
            Ok(())
        }
        VmOutcome::Suspended(Suspension::AwaitReturn(value)) => {
            // The `yield*` delegation has no `return` method: the received
            // value is awaited and the body is resumed with a return
            // completion of it (spec 15.5.5 return case step b).
            let context = agent
                .execution_context_stack
                .pop()
                .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no context to pop".into()))?;
            save_context(agent, object_id, context)?;
            attach_await(agent, object_id, value, AwaitKind::ReturnResume)?;
            Ok(())
        }
        VmOutcome::Completed(completion) => {
            agent.execution_context_stack.pop();
            // spec 27.6.3.2 steps 6-7: normal and empty completions become
            // undefined; a return completion keeps its value.
            let completion = match completion {
                Completion::Normal(_) | Completion::Empty => Completion::Normal(Value::Undefined),
                other => other,
            };
            // spec 9.4.3: the body's `using` resources are disposed when the
            // body completes; async-dispose hints suspend through the job
            // queue, so `complete_current_request` runs from the driver once
            // the disposals settle.
            let body_env = agent
                .async_generators
                .get(&object_id)
                .cloned()
                .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not an async generator".into()))?
                .borrow()
                .body_env
                .clone();
            let resources = match body_env {
                Some(env) => env.drain_disposable_resources(),
                None => Vec::new(),
            };
            if resources
                .iter()
                .any(|resource| !resource.method.is_undefined())
            {
                crate::builtins::disposable::dispose_async_body_resources(
                    agent,
                    resources,
                    completion,
                    crate::builtins::disposable::AsyncBodySettlement::Generator { object_id },
                )?;
            } else {
                complete_current_request(agent, object_id, completion)?;
            }
            Ok(())
        }
    }
}

/// Complete the current request with a body completion (spec 27.6.3.2 steps
/// 6-7): mark the generator completed, settle the current request, then drain
/// the queue. Called directly on completion or by the async-body disposal
/// driver after the awaited disposals settle.
pub fn complete_current_request(
    agent: &mut Agent,
    object_id: u64,
    completion: Completion,
) -> Result<(), JsError> {
    let request = take_current(agent, object_id)?;
    set_flag(agent, object_id, AsyncGeneratorFlag::Completed)?;
    if let Some(request) = request {
        complete_step(agent, &request, completion, true)?;
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
    let ValueKind::Function(function_handle) = function_value.kind() else {
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

/// Attach the Await reactions for a body `await`, a `yield` value, or a
/// `return()` value (spec 27.6.3.6 / 27.9.3.7 / 27.9.3.9): the continuation
/// resumes the VM or completes the queued request.
fn attach_await(
    agent: &mut Agent,
    object_id: u64,
    value: Value,
    kind: AwaitKind,
) -> Result<(), JsError> {
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
                kind,
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
    let ValueKind::Function(function) = callee.kind() else {
        return None;
    };
    let entry = agent.async_generator_awaits.get(&function.id()).cloned()?;
    Some(resume_from_await(agent, entry, args))
}

/// The await continuation: dispatch by what the await was for.
fn resume_from_await(
    agent: &mut Agent,
    entry: Rc<AsyncGeneratorAwaitEntry>,
    args: &[Value],
) -> Result<Value, JsError> {
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    match entry.kind {
        AwaitKind::Body => {
            let completion = if entry.is_reject {
                Resume::Throw(value)
            } else {
                Resume::Normal(value)
            };
            let outcome = resume_body(agent, entry.object_id, completion);
            drive(agent, entry.object_id, outcome)?;
        }
        AwaitKind::Yield => {
            if entry.is_reject {
                // The awaited yield value rejected (spec 27.8.3.7: `yield`
                // awaits its value first); resume the body with the
                // rejection so a try/catch around the yield can observe it.
                let outcome = resume_body(agent, entry.object_id, Resume::Throw(value));
                drive(agent, entry.object_id, outcome)?;
            } else {
                resume_from_yield(agent, entry.object_id, value)?;
            }
        }
        AwaitKind::ReturnResume => {
            let completion = if entry.is_reject {
                Resume::Throw(value)
            } else {
                Resume::Return(value)
            };
            let outcome = resume_body(agent, entry.object_id, completion);
            drive(agent, entry.object_id, outcome)?;
        }
        AwaitKind::AwaitReturn => {
            let request = take_current(agent, entry.object_id)?;
            set_flag(agent, entry.object_id, AsyncGeneratorFlag::Completed)?;
            if let Some(request) = request {
                let completion = if entry.is_reject {
                    Completion::Throw(value)
                } else {
                    Completion::Normal(value)
                };
                complete_step(agent, &request, completion, true)?;
            }
            async_generator_resume_next(agent, entry.object_id)?;
        }
    }
    Ok(Value::Undefined)
}

/// AsyncGeneratorYield (spec 27.9.3.8), reached from the `yield`-await
/// continuation: complete the current request with the awaited value, then
/// either keep executing the body with the next queued request's completion
/// or suspend at the yield.
fn resume_from_yield(agent: &mut Agent, object_id: u64, value: Value) -> Result<(), JsError> {
    let request = take_current(agent, object_id)?;
    if let Some(request) = request {
        complete_step(agent, &request, Completion::Normal(value), false)?;
    }
    let Some((next, completion)) = pop_front(agent, object_id)? else {
        set_flag(agent, object_id, AsyncGeneratorFlag::SuspendedYield)?;
        return Ok(());
    };
    set_current(agent, object_id, next)?;
    match completion {
        Resume::Normal(value) => {
            let outcome = resume_body(agent, object_id, Resume::Normal(value));
            drive(agent, object_id, outcome)?;
        }
        Resume::Throw(value) => {
            let outcome = resume_body(agent, object_id, Resume::Throw(value));
            drive(agent, object_id, outcome)?;
        }
        Resume::Return(value) => {
            resume_with_return(agent, object_id, value)?;
        }
    }
    Ok(())
}

/// AsyncGeneratorUnwrapYieldResumption (spec 27.9.3.7): a `return()`
/// delivered into the suspended body first awaits its value, so a throwing
/// PromiseResolve (e.g. a broken promise) surfaces inside the body's
/// try/catch — including before a `yield*` delegation sees it (the
/// delegation then awaits it again when it has no `return` method).
fn resume_with_return(agent: &mut Agent, object_id: u64, value: Value) -> Result<(), JsError> {
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let promise = match promise_resolve(agent, &promise_ctor, value.clone()) {
        Ok(promise) => promise,
        Err(error) => {
            let rejection = crate::promise::error_value(agent, &error);
            let outcome = resume_body(agent, object_id, Resume::Throw(rejection));
            return drive(agent, object_id, outcome);
        }
    };
    attach_await(agent, object_id, promise, AwaitKind::ReturnResume)?;
    Ok(())
}

/// AsyncGeneratorAwaitReturn (spec 27.9.3.9): a `return()` on a
/// suspended-start or completed generator awaits the return value; a
/// throwing PromiseResolve rejects the request immediately.
fn await_return(agent: &mut Agent, object_id: u64, value: Value) -> Result<(), JsError> {
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let promise = match promise_resolve(agent, &promise_ctor, value.clone()) {
        Ok(promise) => promise,
        Err(error) => {
            let request = take_current(agent, object_id)?;
            set_flag(agent, object_id, AsyncGeneratorFlag::Completed)?;
            if let Some(request) = request {
                let rejection = crate::promise::error_value(agent, &error);
                complete_step(agent, &request, Completion::Throw(rejection), true)?;
            }
            return async_generator_resume_next(agent, object_id);
        }
    };
    attach_await(agent, object_id, promise, AwaitKind::AwaitReturn)?;
    Ok(())
}

/// AsyncGeneratorCompleteStep (spec 27.9.3.5): resolve the request's
/// capability with `{ value, done }` or reject it with a throw completion's
/// value. Values are not promise-unwrapped here: a `yield`'s value was
/// already awaited before AsyncGeneratorYield ran.
fn complete_step(
    agent: &mut Agent,
    request: &AsyncGeneratorRequest,
    completion: Completion,
    done: bool,
) -> Result<(), JsError> {
    match completion {
        Completion::Throw(value) => {
            crate::function::call(
                agent,
                &request.capability.reject,
                Value::Undefined,
                &[value],
            )?;
        }
        Completion::Normal(value) | Completion::Return(value) => {
            let result = iterator_result(agent, value, done)?;
            crate::function::call(
                agent,
                &request.capability.resolve,
                Value::Undefined,
                &[result],
            )?;
        }
        Completion::Empty => {
            let result = iterator_result(agent, Value::Undefined, done)?;
            crate::function::call(
                agent,
                &request.capability.resolve,
                Value::Undefined,
                &[result],
            )?;
        }
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
    Ok(())
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
        let ValueKind::Object(obj) = value.kind() else {
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
