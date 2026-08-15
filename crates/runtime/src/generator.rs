//! Generator objects (spec 27.4): GeneratorStart/Resume/Yield/Validate plus
//! the `%Generator.prototype%` methods, driving the resumable-function IR.

use std::cell::RefCell;
use std::rc::Rc;

use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::PropertyDescriptor;
use crux::string::JsString;
use crux::value::Value;

use crate::agent::Agent;
use crate::context::ExecutionContext;
use crate::env::EnvRef;
use crate::flow::Completion;
use crate::function::ThisMode;
use crate::ir::{CompiledBody, Resume, Suspension, Vm, VmOutcome};
use crate::realm::Realm;

const GENERATOR_PROTO: &str = "%Generator.prototype%";
const NEXT: &str = "next";
const RETURN: &str = "return";
const THROW: &str = "throw";

/// [[GeneratorState]] (spec 27.4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeneratorFlag {
    SuspendedStart,
    SuspendedYield,
    Executing,
    Completed,
}

/// The agent-side generator record: the [[GeneratorState]] and the resumable
/// VM plus its execution context.
#[derive(Debug)]
pub struct GeneratorState {
    pub flag: GeneratorFlag,
    pub vm: Option<Vm>,
    pub body: Option<CompiledBody>,
    pub context: Option<ExecutionContext>,
    pub function: Value,
    pub realm: Handle<Realm>,
    /// The post-instantiation environment: parameters are bound (and errors
    /// surface) when the generator is *called* (spec EvaluateGeneratorBody
    /// step 1), and the VM runs against this environment on the first
    /// `next()`.
    pub body_env: Option<EnvRef>,
}

pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| crate::context::as_object(&value));
    let proto = JsObject::ordinary_object_create(object_proto);
    let proto_value = Value::Object(proto.clone());
    realm.intrinsics.define(GENERATOR_PROTO, proto_value);
    for (name, length) in [(NEXT, 1), (RETURN, 1), (THROW, 1)] {
        let method = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            Box::new(|_, _| {
                Err(JsError::new(
                    ErrorKind::TypeError,
                    "generator prototype method must be called through the agent".into(),
                ))
            }),
            None,
            None,
        )?;
        // Registered as an intrinsic so a method of one realm called from
        // another dispatches with its own realm current.
        realm.intrinsics.define(
            &format!("%Generator.prototype.{name}%"),
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
    // %Generator.prototype%[@@iterator] = %Generator.prototype%.next's
    // iterator contract: the generator is its own iterator (spec 27.4.1).
    let function_proto = realm
        .intrinsics
        .get("%Function.prototype%")
        .and_then(|value| crate::context::as_object(&value))
        .and_then(|object| object.handle());
    let iterator_method = Function::create_builtin(
        Some(JsString::from_utf8("[Symbol.iterator]")),
        0,
        Box::new(|this, _| Ok(this.clone())),
        None,
        function_proto,
    )?;
    proto.define_property_key(
        &crux::property::PropertyKey::Symbol(crux::symbol::well_known("iterator").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::Function(iterator_method)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    // %Generator.prototype%[@@toStringTag] = "Generator" (spec 27.4.3.3).
    proto.define_property_key(
        &crux::property::PropertyKey::Symbol(
            crux::symbol::well_known("toStringTag").as_ref().clone(),
        ),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8("Generator")))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// Dispatch `%Generator.prototype%` methods by identity.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let proto_value = realm.intrinsics.get(GENERATOR_PROTO)?;
    let Value::Object(proto) = &proto_value else {
        return None;
    };
    for (name, handler) in [
        (
            NEXT,
            generator_next as fn(&mut Agent, &Value, &[Value]) -> Result<Value, JsError>,
        ),
        (RETURN, generator_return),
        (THROW, generator_throw),
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

/// GeneratorFunctionCall: calling a generator function returns a fresh
/// generator object (the body does not run until the first `next()`).
/// GeneratorFunctionCall (spec 15.5.2 EvaluateGeneratorBody): parameters are
/// bound at call time — errors surface synchronously — and the generator
/// object captures the instantiated environment for the first `next()`.
pub fn call_generator(
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
    // The VM drives the body's lexical environment (the one
    // function_declaration_instantiation installed on the running context),
    // so body-level let/const bindings are reachable.
    let instantiated_context = agent
        .execution_context_stack
        .last()
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no context to capture".into()))?;
    let body_env = instantiated_context.lexical_environment.clone();
    agent.execution_context_stack.pop();

    let proto_value = crate::context::get_property(
        agent,
        &Value::Function(function.clone()),
        &JsString::from_utf8("prototype"),
        Value::Function(function.clone()),
    )?;
    // GetPrototypeFromConstructor (spec 9.1.14): a non-object
    // `prototype` (e.g. `g.prototype = null`) falls back to the
    // function's realm's intrinsic generator prototype (the
    // cross-realm fixture asserts the creation realm's).
    let proto = match crate::context::as_object(&proto_value) {
        Some(proto) => proto,
        None => {
            let intrinsic = if data.is_async {
                "%AsyncGenerator.prototype%"
            } else {
                "%Generator.prototype%"
            };
            data.realm
                .intrinsics
                .get(intrinsic)
                .and_then(|value| crate::context::as_object(&value))
                .ok_or_else(|| {
                    JsError::new(ErrorKind::TypeError, format!("{intrinsic} is not defined"))
                })?
        }
    };
    let object = JsObject::ordinary_object_create(Some(proto));
    let object_value = Value::Object(object.clone());
    agent.generators.insert(
        object.id(),
        Rc::new(RefCell::new(GeneratorState {
            flag: GeneratorFlag::SuspendedStart,
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

/// GeneratorResume for `next(value)` (spec 27.4.3.2).
fn generator_next(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    generator_resume(agent, this, Resume::Normal(value))
}

/// GeneratorResumeAbrupt with a return completion (spec 27.4.3.3).
fn generator_return(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    generator_resume_abrupt(agent, this, Resume::Return(value))
}

/// GeneratorResumeAbrupt with a throw completion (spec 27.4.3.4).
fn generator_throw(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    generator_resume_abrupt(agent, this, Resume::Throw(value))
}

/// GeneratorResume (spec 27.4.3.2 steps 1-11) for a normal completion.
fn generator_resume(agent: &mut Agent, this: &Value, completion: Resume) -> Result<Value, JsError> {
    let Value::Object(obj) = this else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "GeneratorResume called on a non-object".into(),
        ));
    };
    let state = agent.generators.get(&obj.id()).cloned().ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "GeneratorResume called on a non-generator".into(),
        )
    })?;
    // A re-entrant `next()` during execution borrows the state mutably; the
    // spec's "already running" TypeError (27.4.3.2 step 4) must not panic.
    let flag = match state.try_borrow() {
        Ok(state) => state.flag,
        Err(_) => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Generator is already running".into(),
            ));
        }
    };
    match flag {
        GeneratorFlag::Completed => iterator_result(agent, Value::Undefined, true),
        GeneratorFlag::Executing => Err(JsError::new(
            ErrorKind::TypeError,
            "Generator is already running".into(),
        )),
        GeneratorFlag::SuspendedStart => {
            let mut state = match state.try_borrow_mut() {
                Ok(state) => state,
                Err(_) => {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Generator is already running".into(),
                    ));
                }
            };
            state.flag = GeneratorFlag::Executing;
            start_body(agent, &mut state, completion)
        }
        GeneratorFlag::SuspendedYield => {
            let mut state = match state.try_borrow_mut() {
                Ok(state) => state,
                Err(_) => {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Generator is already running".into(),
                    ));
                }
            };
            state.flag = GeneratorFlag::Executing;
            resume_body(agent, &mut state, completion)
        }
    }
}

/// GeneratorResumeAbrupt (spec 27.4.3.3/27.4.3.4) for throw/return.
fn generator_resume_abrupt(
    agent: &mut Agent,
    this: &Value,
    completion: Resume,
) -> Result<Value, JsError> {
    let Value::Object(obj) = this else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "GeneratorResumeAbrupt called on a non-object".into(),
        ));
    };
    let state = agent.generators.get(&obj.id()).cloned().ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "GeneratorResumeAbrupt called on a non-generator".into(),
        )
    })?;
    // A re-entrant `next` during execution borrows the state mutably; throw
    // the spec's "already running" TypeError instead of panicking.
    let flag = match state.try_borrow() {
        Ok(state) => state.flag,
        Err(_) => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Generator is already running".into(),
            ));
        }
    };
    match flag {
        GeneratorFlag::Executing => Err(JsError::new(
            ErrorKind::TypeError,
            "Generator is already running".into(),
        )),
        // spec 27.4.3.3/27.4.3.4 step: a completed generator propagates the
        // abrupt completion (throw throws; return yields its value and stays
        // done).
        GeneratorFlag::Completed => match completion {
            Resume::Return(value) => iterator_result(agent, value, true),
            Resume::Throw(value) => Err(JsError::new(
                ErrorKind::TypeError,
                "Uncaught generator throw".into(),
            )
            .with_value(value)),
            Resume::Normal(_) => unreachable!("abrupt resume"),
        },
        GeneratorFlag::SuspendedStart => {
            // The body never ran: complete immediately with the abrupt
            // completion (spec 27.4.3.4 step 5).
            let mut state = match state.try_borrow_mut() {
                Ok(state) => state,
                Err(_) => {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Generator is already running".into(),
                    ));
                }
            };
            state.flag = GeneratorFlag::Completed;
            match completion {
                Resume::Return(value) => iterator_result(agent, value, true),
                Resume::Throw(value) => Err(JsError::new(
                    ErrorKind::TypeError,
                    "Uncaught generator throw".into(),
                )
                .with_value(value)),
                Resume::Normal(_) => unreachable!("abrupt resume"),
            }
        }
        GeneratorFlag::SuspendedYield => {
            let mut state = match state.try_borrow_mut() {
                Ok(state) => state,
                Err(_) => {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Generator is already running".into(),
                    ));
                }
            };
            state.flag = GeneratorFlag::Executing;
            resume_body(agent, &mut state, completion)
        }
    }
}

/// GeneratorStart (spec 27.4.1): push the context instantiated at call time
/// and run the VM against its environment.
fn start_body(
    agent: &mut Agent,
    state: &mut GeneratorState,
    _completion: Resume,
) -> Result<Value, JsError> {
    let context = state
        .context
        .take()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no saved context".into()))?;
    let body_env = state
        .body_env
        .clone()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no instantiated environment".into()))?;
    let function_value = state.function.clone();
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
            "generator body was not compiled".into(),
        )
    })?;
    agent.execution_context_stack.push(context);
    let vm = Vm::new(body_env, data.strict);
    state.body = Some(body.clone());
    state.vm = Some(vm);
    let outcome = state.vm.as_mut().expect("vm set").start(agent, &body)?;
    finish_resume(agent, state, Ok(outcome))
}

/// Resume a suspended generator (spec 27.4.3.2 steps 5-11): re-push the
/// saved context and drive the VM. Abrupt resumes on a plain `yield` run the
/// throw/return machinery; only a `yield*` delegation receives them as
/// resumption values.
fn resume_body(
    agent: &mut Agent,
    state: &mut GeneratorState,
    completion: Resume,
) -> Result<Value, JsError> {
    let context = state
        .context
        .take()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no saved context".into()))?;
    let body = state
        .body
        .clone()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no saved body".into()))?;
    agent.execution_context_stack.push(context);
    // Was the suspension a `yield*` delegation? Its continuation reads the
    // abrupt completion; a plain `yield` propagates it through the machinery.
    let suspended_at_delegate = state
        .vm
        .as_ref()
        .and_then(|vm| vm.ip.checked_sub(1))
        .and_then(|ip| body.steps.get(ip))
        .is_some_and(|step| matches!(step, crate::ir::Step::Yield { delegate: true }));
    let outcome = {
        let vm = state
            .vm
            .as_mut()
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no suspended VM".into()))?;
        match completion {
            Resume::Throw(_) | Resume::Return(_) if !suspended_at_delegate => {
                vm.run_abrupt(agent, &body, completion)
            }
            _ => vm.run(agent, &body, completion),
        }
    };
    finish_resume(agent, state, outcome)
}

/// The tail of a resume: save the suspension or complete the generator.
fn finish_resume(
    agent: &mut Agent,
    state: &mut GeneratorState,
    outcome: Result<VmOutcome, JsError>,
) -> Result<Value, JsError> {
    match outcome? {
        VmOutcome::Suspended(Suspension::Yield { value, delegate }) => {
            let context = agent
                .execution_context_stack
                .pop()
                .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no context to pop".into()))?;
            state.context = Some(context);
            state.flag = GeneratorFlag::SuspendedYield;
            if delegate {
                // Spec 15.5.5: GeneratorYield(innerResult) yields the inner
                // iterator result object itself, so the outer consumer reads
                // its `value`/`done` lazily.
                Ok(value)
            } else {
                iterator_result(agent, value, false)
            }
        }
        VmOutcome::Suspended(Suspension::Await(_)) => {
            agent.execution_context_stack.pop();
            state.flag = GeneratorFlag::Completed;
            Err(JsError::new(
                ErrorKind::TypeError,
                "generator body awaited without being async".into(),
            ))
        }
        VmOutcome::Completed(completion) => {
            agent.execution_context_stack.pop();
            state.flag = GeneratorFlag::Completed;
            // spec 27.4.2.1 step 4.j: the generator body's `using` resources
            // are disposed when the body completes (implicit or explicit
            // return), even on an abrupt completion.
            let env = state.vm.as_ref().map(|vm| vm.lexical_env.clone());
            state.vm = None;
            let completion = match env {
                Some(env) => crate::eval::dispose_env_resources(agent, &env, Ok(completion))?,
                None => completion,
            };
            match completion {
                // Only a `return` completion carries a value; a normal
                // completion (the last statement's value, e.g. `[yield]` or
                // a `yield*` delegate's return) yields *undefined*.
                Completion::Return(value) => iterator_result(agent, value, true),
                Completion::Normal(_) | Completion::Empty => {
                    iterator_result(agent, Value::Undefined, true)
                }
                Completion::Throw(value) => Err(JsError::new(
                    ErrorKind::TypeError,
                    "Uncaught generator throw".into(),
                )
                .with_value(value)),
                Completion::Break { .. } | Completion::Continue { .. } => Err(JsError::new(
                    ErrorKind::SyntaxError,
                    "Illegal control flow in a generator body".into(),
                )),
            }
        }
    }
}

/// The iterator-result object `{ value, done }` (spec 27.4.3.2 step 11).
fn iterator_result(agent: &mut Agent, value: Value, done: bool) -> Result<Value, JsError> {
    // CreateIterResultObject (spec 7.3.17): an object inheriting
    // %Object.prototype% (result-prototype.js).
    let proto = agent
        .current_realm()?
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|v| crate::context::as_object(&v));
    let object = JsObject::ordinary_object_create(proto);
    object.create_data_property(&JsString::from_utf8("value"), value)?;
    object.create_data_property(&JsString::from_utf8("done"), Value::Boolean(done))?;
    Ok(Value::Object(object))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::evaluate;

    fn run(source: &str) -> Result<Value, JsError> {
        evaluate(source)
    }

    #[test]
    fn generator_returns_iteration_results() {
        let value = run(
            "function* g() { yield 1; yield 2; return 3; } \
             var it = g(); \
             var a = it.next(); var b = it.next(); var c = it.next(); var d = it.next(); \
             a.value + ',' + a.done + '|' + b.value + ',' + b.done + '|' + c.value + ',' + c.done + '|' + d.done",
        )
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8(
                "1,false|2,false|3,true|true"
            )))
        );
    }

    #[test]
    fn generator_next_passes_values_back() {
        let value = run(
            "function* g() { var a = yield 1; var b = yield a + 10; return a + b; } \
             var it = g(); \
             var first = it.next(); \
             var second = it.next(5); \
             var third = it.next(7); \
             first.value + ',' + second.value + ',' + third.value",
        )
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("1,15,12")))
        );
    }

    #[test]
    fn generator_return_completes_early() {
        let value = run("function* g() { yield 1; yield 2; } \
             var it = g(); \
             var a = it.next(); var b = it.return(9); var c = it.next(); \
             a.value + ',' + b.value + ',' + b.done + ',' + c.value + ',' + c.done")
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("1,9,true,undefined,true")))
        );
    }

    #[test]
    fn generator_throw_propagates() {
        let result = run(
            "function* g() { try { yield 1; } catch (e) { return 'caught:' + e; } } \
             var it = g(); it.next(); var r = it.throw('boom'); r.value",
        )
        .unwrap();
        assert_eq!(
            result,
            Value::String(Handle::new(JsString::from_utf8("caught:boom")))
        );
    }

    #[test]
    fn generator_yield_star_delegates() {
        let value = run("function* inner() { yield 1; yield 2; return 3; } \
             function* outer() { var r = yield* inner(); yield r; } \
             var it = outer(); \
             var a = it.next(); var b = it.next(); var c = it.next(); var d = it.next(); \
             a.value + ',' + b.value + ',' + c.value + ',' + d.value + ',' + d.done")
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("1,2,3,undefined,true")))
        );
    }

    #[test]
    fn generator_yield_star_defers_inner_value_read() {
        // Spec 15.5.5: GeneratorYield(innerResult) yields the inner iterator
        // result object itself, so the inner `.value` getter is not read until
        // the outer consumer reads the yielded result's `value`.
        let value = run(
            "var callCount = 0; \
             var spyValue = Object.defineProperty({ done: false }, 'value', { \
               get: function() { callCount += 1; return 42; } }); \
             var iterable = {}; \
             iterable[Symbol.iterator] = function() { return { next: function() { return spyValue; } }; }; \
             function* g() { yield* iterable; } \
             var it = g(); \
             var first = it.next(); \
             var afterNext = callCount; \
             var read = first.value; \
             afterNext + ',' + read + ',' + callCount",
        )
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("0,42,1")))
        );
    }

    #[test]
    fn generator_yield_star_return_completion_completes_generator() {
        // A done `return()` result completes the generator with ReturnCompletion
        // of its value: the statement after `yield*` never runs, but `finally`
        // does (spec 15.5.5 return case).
        let value = run(
            "var hitNext = false; var hitFinally = false; \
             var iterable = {}; \
             iterable[Symbol.iterator] = function() { \
               return { next: function() { return { done: false }; }, \
                        return: function() { return { done: true, value: 3333 }; } }; }; \
             function* g() { try { yield* iterable; hitNext = true; } finally { hitFinally = true; } } \
             var it = g(); it.next(); var r = it.return(2222); \
             r.value + ',' + r.done + ',' + hitNext + ',' + hitFinally",
        )
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("3333,true,false,true")))
        );
    }

    #[test]
    fn generator_yield_star_get_iterator_error_is_catchable() {
        // The TypeError from GetIterator propagates through the generator
        // body's try/catch (spec 15.5.5 step 4).
        let value = run("var badIter = {}; \
             badIter[Symbol.iterator] = function() { return 7; }; \
             function* g() { try { yield* badIter; } catch (e) { return 'caught'; } } \
             var it = g(); var r = it.next(); r.value + ',' + r.done")
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("caught,true")))
        );
    }

    #[test]
    fn generator_yield_star_no_return_method_propagates_return() {
        // Without a `return` method the delegation completes with the return
        // completion, running `finally` but not the statement after `yield*`.
        let value = run(
            "var hitNext = false; var hitFinally = false; \
             var iterable = {}; \
             iterable[Symbol.iterator] = function() { return { next: function() { return { done: false }; } }; }; \
             function* g() { try { yield* iterable; hitNext = true; } finally { hitFinally = true; } } \
             var it = g(); it.next(); it.return(9); \
             hitNext + ',' + hitFinally",
        )
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("false,true")))
        );
    }

    #[test]
    fn generator_yield_star_non_object_next_result_throws() {
        let value = run(
            "var iterable = {}; \
             iterable[Symbol.iterator] = function() { return { next: function() { return 8; } }; }; \
             function* g() { try { yield* iterable; } catch (e) { return e instanceof TypeError; } } \
             var it = g(); var r = it.next(); r.value + ',' + r.done",
        )
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("true,true")))
        );
    }

    #[test]
    fn generator_yield_star_for_of_loop() {
        let value = run("function* inner() { yield 1; yield 2; return 3; } \
             function* outer() { var r = yield* inner(); yield r; } \
             var it = outer(); var out = ''; var r; \
             while (!(r = it.next()).done) { out += r.value + ';'; } \
             out")
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("1;2;3;")))
        );
    }

    #[test]
    fn generator_for_of_loop() {
        let value = run(
            "function* g() { for (var i = 0; i < 3; i++) { yield i * 10; } } \
             var it = g(); var out = ''; var r; \
             while (!(r = it.next()).done) { out += r.value + ';'; } \
             out",
        )
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("0;10;20;")))
        );
    }

    #[test]
    fn generator_closure_per_iteration() {
        let value = run(
            "function* g() { for (let i = 0; i < 3; i++) { yield function () { return i; }; } } \
             var it = g(); var f0 = it.next().value; var f1 = it.next().value; var f2 = it.next().value; \
             f0() + ',' + f1() + ',' + f2()",
        )
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("0,1,2")))
        );
    }

    #[test]
    fn generator_try_finally_runs_on_return() {
        let value = run(
            "function* g() { try { yield 1; } finally { out += 'f;'; } } \
             var out = ''; var it = g(); it.next(); it.return(2); out",
        )
        .unwrap();
        assert_eq!(value, Value::String(Handle::new(JsString::from_utf8("f;"))));
    }

    #[test]
    fn generator_destructure_default_yield_suspends_when_iterator_done() {
        // The element still receives undefined after exhaustion, so the
        // default initializer runs (and suspends) instead of the pattern
        // finishing immediately.
        let value = run(
            "function* g() { var x; var vals = []; var result = [x = yield] = vals; return x; } \
             var it = g(); var a = it.next(); var b = it.next(86); \
             a.done + ',' + b.done + ',' + b.value",
        )
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("false,true,86")))
        );
    }

    #[test]
    fn generator_return_into_destructure_closes_iterator() {
        let value = run("var returnCount = 0; \
             var iterator = { next: function() { return {done: false, value: undefined}; }, \
                              return: function() { returnCount += 1; return {}; } }; \
             var iterable = {}; iterable[Symbol.iterator] = function() { return iterator; }; \
             function* g() { var vals = iterable; [{} = yield] = vals; } \
             var it = g(); it.next(); var r = it.return(777); \
             returnCount + ',' + r.value + ',' + r.done")
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("1,777,true")))
        );
    }

    #[test]
    fn generator_return_into_destructure_checks_return_result() {
        // IteratorClose with a return completion: a non-object `return`
        // result is a TypeError that replaces the return completion.
        let value = run(
            "var iterator = { next: function() { return {done: false, value: undefined}; }, \
             return: function() { return null; } }; \
             var iterable = {}; iterable[Symbol.iterator] = function() { return iterator; }; \
             function* g() { var vals = iterable; [{} = yield] = vals; } \
             var it = g(); it.next(); \
             var threw = false; try { it.return(); } catch (e) { threw = e instanceof TypeError; } \
             threw",
        )
        .unwrap();
        assert_eq!(value, Value::Boolean(true));
    }

    #[test]
    fn generator_throw_into_destructure_swallows_throwing_return() {
        // IteratorClose with a throw completion: the original error wins, so
        // a throwing `return` is swallowed.
        let value = run(
            "var returnCount = 0; \
             var iterator = { next: function() { return {done: false, value: undefined}; }, \
                              return: function() { returnCount += 1; throw new RangeError('x'); } }; \
             var iterable = {}; iterable[Symbol.iterator] = function() { return iterator; }; \
             function* g() { var vals = iterable; try { [{} = yield] = vals; } catch (e) { return 'caught:' + e; } } \
             var it = g(); it.next(); var r = it.throw('boom'); \
             returnCount + ',' + r.value + ',' + r.done",
        )
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("1,caught:boom,true")))
        );
    }

    #[test]
    fn generator_rest_member_target_evaluates_reference_before_collecting() {
        // The rest target's reference (with a `yield` in its computed key) is
        // evaluated before the remaining values are collected.
        let value = run("var x = {}; \
             function* g() { var vals = [33, 44, 55]; [...x[yield]] = vals; } \
             var it = g(); it.next(); it.next('prop'); \
             x.prop.length + ',' + x.prop[0] + ',' + x.prop[2]")
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("3,33,55")))
        );
    }

    #[test]
    fn generator_destructure_with_iterator_lacking_next_suspends() {
        // GetIterator does not require a callable `next`; the yield in the
        // member target suspends before any step, and `return` still closes.
        let value = run("var returnCount = 0; \
             var iterator = { return: function() { returnCount += 1; return {}; } }; \
             var iterable = {}; iterable[Symbol.iterator] = function() { return iterator; }; \
             function* g() { var vals = iterable; [{}[yield]] = vals; } \
             var it = g(); var a = it.next(); var r = it.return(5); \
             a.done + ',' + returnCount + ',' + r.done")
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("false,1,true")))
        );
    }

    #[test]
    fn generator_return_into_for_of_destructure_head_closes() {
        let value = run("var returnCount = 0; \
             var iterator = { next: function() { return {done: false, value: undefined}; }, \
                              return: function() { returnCount += 1; return {}; } }; \
             var iterable = {}; iterable[Symbol.iterator] = function() { return iterator; }; \
             function* g() { for ([{} = yield] of [iterable]) {} } \
             var it = g(); it.next(); var r = it.return(9); \
             returnCount + ',' + r.value + ',' + r.done")
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("1,9,true")))
        );
    }

    #[test]
    fn generator_class_computed_names_suspend_at_yield() {
        // A class definition inside a generator evaluates its computed names
        // as suspension points: each `[yield]` suspends and resumes with the
        // `.next()` argument as the property name (spec 15.7.14).
        let value = run("function* g() { \
               class C { \
                 [yield]() { return 'm'; } \
                 static [yield]() { return 's'; } \
               } \
               var c = new C(); \
               return c[1]() + ',' + C[2](); } \
             var it = g(); \
             var a = it.next(); it.next(1); var b = it.next(2); \
             a.done + ',' + b.value")
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("false,m,s")))
        );
    }

    #[test]
    fn generator_class_expression_computed_names_suspend_at_yield() {
        let value = run("function* g() { \
               var C = class { [yield]() { return 9; } }; \
               var c = new C(); \
               return c[yield](); } \
             var it = g(); it.next(); it.next('k'); var r = it.next('k'); \
             r.done + ',' + r.value")
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("true,9")))
        );
    }

    #[test]
    fn generator_class_accessor_computed_names_suspend_at_yield() {
        let value = run("var yieldSet; \
             function* g() { \
               class C { \
                 get [yield]() { return 'get'; } \
                 set [yield](v) { yieldSet = v; } \
               } \
               return C; } \
             var it = g(); it.next(); it.next('a'); var r = it.next('b'); \
             r.value.prototype.a + '|' + (r.value.prototype.b = 'set', yieldSet)")
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("get|set")))
        );
    }

    #[test]
    fn generator_using_is_disposed_on_completion() {
        // `using` resources in a generator body are disposed when the body
        // completes, not while suspended (spec 27.4.2.1 step 4.j).
        let value = run("var disposed = 0; \
             var resource = { [Symbol.dispose]: function() { disposed += 1; } }; \
             function* g() { using _ = resource; yield; } \
             var it = g(); \
             var beforeStart = disposed; \
             it.next(); \
             var whileSuspended = disposed; \
             it.next(); \
             var afterDone = disposed; \
             beforeStart + ',' + whileSuspended + ',' + afterDone")
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("0,0,1")))
        );
    }
}
