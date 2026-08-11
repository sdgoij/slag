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
    /// The call arguments, applied by GeneratorStart on the first `next()`.
    pub start_this: Value,
    pub start_args: Vec<Value>,
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
    let iterator_method = Function::create_builtin(
        Some(JsString::from_utf8("[Symbol.iterator]")),
        0,
        Box::new(|this, _| Ok(this.clone())),
        None,
        None,
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
    let proto = data
        .realm
        .intrinsics
        .get(GENERATOR_PROTO)
        .and_then(|value| crate::context::as_object(&value))
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                format!("{GENERATOR_PROTO} is not defined"),
            )
        })?;
    let object = JsObject::ordinary_object_create(Some(proto));
    let object_value = Value::Object(object.clone());
    agent.generators.insert(
        object.id(),
        Rc::new(RefCell::new(GeneratorState {
            flag: GeneratorFlag::SuspendedStart,
            vm: None,
            body: None,
            context: None,
            function: Value::Function(function.clone()),
            realm: data.realm.clone(),
            start_this: this,
            start_args: args.to_vec(),
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
    if state.borrow().flag == GeneratorFlag::Executing {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Generator is already running".into(),
        ));
    }
    let flag = state.borrow().flag;
    match flag {
        GeneratorFlag::Completed => iterator_result(agent, Value::Undefined, true),
        GeneratorFlag::Executing => Err(JsError::new(
            ErrorKind::TypeError,
            "Generator is already running".into(),
        )),
        GeneratorFlag::SuspendedStart => {
            let mut state = state.borrow_mut();
            state.flag = GeneratorFlag::Executing;
            start_body(agent, &mut state, completion)
        }
        GeneratorFlag::SuspendedYield => {
            let mut state = state.borrow_mut();
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
    let flag = state.borrow().flag;
    match flag {
        GeneratorFlag::Executing => Err(JsError::new(
            ErrorKind::TypeError,
            "Generator is already running".into(),
        )),
        GeneratorFlag::Completed => iterator_result(agent, Value::Undefined, true),
        GeneratorFlag::SuspendedStart => {
            // The body never ran: complete immediately with the abrupt
            // completion (spec 27.4.3.4 step 5).
            let mut state = state.borrow_mut();
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
            let mut state = state.borrow_mut();
            state.flag = GeneratorFlag::Executing;
            resume_body(agent, &mut state, completion)
        }
    }
}

/// GeneratorStart (spec 27.4.1): push the context, instantiate, run the VM.
fn start_body(
    agent: &mut Agent,
    state: &mut GeneratorState,
    _completion: Resume,
) -> Result<Value, JsError> {
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
    };
    agent.execution_context_stack.push(context.clone());
    if data.this_mode != ThisMode::Lexical {
        let this = if data.this_mode == ThisMode::Sloppy
            && matches!(state.start_this, Value::Undefined | Value::Null)
        {
            let global = agent.running_context()?.realm.global_object.clone();
            Value::Object(global)
        } else {
            state.start_this.clone()
        };
        function_env.bind_this_value(this)?;
    }
    crate::function::function_declaration_instantiation(
        agent,
        &function_value,
        &data,
        &state.start_args,
        &function_env,
    )?;
    let vm = Vm::new(function_env, data.strict);
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
            let _ = delegate;
            iterator_result(agent, value, false)
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
            state.vm = None;
            match completion {
                Completion::Return(value) | Completion::Normal(value) => {
                    iterator_result(agent, value, true)
                }
                Completion::Empty => iterator_result(agent, Value::Undefined, true),
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
fn iterator_result(_agent: &mut Agent, value: Value, done: bool) -> Result<Value, JsError> {
    let object = JsObject::ordinary_object_create(None);
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
}
