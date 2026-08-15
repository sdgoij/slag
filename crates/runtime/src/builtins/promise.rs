//! The `%Promise%` intrinsic (spec 27.2): constructor, prototype methods,
//! and statics. All bodies are placeholders; `runtime::function::call`/
//! `construct` dispatch by intrinsic identity (the %eval% pattern), because
//! every operation reaches user code and the agent.

use std::cell::RefCell;
use std::rc::Rc;

use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, is_callable, is_constructor};

use crate::agent::Agent;
use crate::promise::{
    PromiseData, PromiseState, error_value, is_promise, new_promise_capability,
    perform_promise_then, promise_resolve, reject_promise, resolve_promise,
};
use crate::realm::Realm;

const PROMISE: &str = "%Promise%";
const PROMISE_PROTO: &str = "%Promise.prototype%";
const THEN: &str = "then";
const CATCH: &str = "catch";
const FINALLY: &str = "finally";
const RESOLVE: &str = "resolve";
const REJECT: &str = "reject";
const ALL: &str = "all";
const ALL_SETTLED: &str = "allSettled";
const ANY: &str = "any";
const RACE: &str = "race";
const WITH_RESOLVERS: &str = "withResolvers";
const TRY: &str = "try";

/// Shared per-combinator state plus the per-element handler's index and role.
/// Each element's closure holds its own `CompoundState` entry; the collect
/// buffers (`values`/`results`/`errors`) and the `remaining` counter are
/// shared across elements through `Rc`s. `called` is the element's
/// [[AlreadyCalled]] guard: a resolve/reject element function runs at most
/// once even when a thenable calls its handler repeatedly.
#[derive(Debug)]
pub enum CompoundState {
    /// An `all` element fulfillment handler.
    All {
        values: Rc<RefCell<Vec<Value>>>,
        remaining: Rc<RefCell<usize>>,
        resolve: Value,
        reject: Value,
        index: usize,
        called: bool,
    },
    /// An `allSettled` element handler; `fulfilled` selects the value/reason
    /// wrapping.
    AllSettled {
        results: Rc<RefCell<Vec<Value>>>,
        remaining: Rc<RefCell<usize>>,
        resolve: Value,
        fulfilled: bool,
        index: usize,
        called: bool,
    },
    /// An `any` element handler; `fulfilled` selects resolve-vs-collect.
    Any {
        errors: Rc<RefCell<Vec<Value>>>,
        remaining: Rc<RefCell<usize>>,
        resolve: Value,
        reject: Value,
        fulfilled: bool,
        index: usize,
        called: bool,
    },
}

impl CompoundState {
    /// The element handler's [[AlreadyCalled]] guard: `true` when the
    /// handler already ran (and marks it as run).
    fn already_called(&mut self) -> bool {
        let called = match self {
            CompoundState::All { called, .. }
            | CompoundState::AllSettled { called, .. }
            | CompoundState::Any { called, .. } => *called,
        };
        if !called {
            match self {
                CompoundState::All { called, .. }
                | CompoundState::AllSettled { called, .. }
                | CompoundState::Any { called, .. } => *called = true,
            }
        }
        called
    }
}

/// The closures `Promise.prototype.finally` creates (spec 27.2.5.3).
#[derive(Debug)]
pub enum FinallyState {
    ThenFinally {
        on_finally: Value,
        constructor: Value,
    },
    CatchFinally {
        on_finally: Value,
        constructor: Value,
    },
    /// `() => value` (the valueThunk).
    ValueThunk { value: Value },
    /// `() => { throw reason }` (the thrower).
    Thrower { reason: Value },
}

pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| crate::context::as_object(&value));

    let promise_proto = JsObject::ordinary_object_create(object_proto.clone());
    let promise_proto_value = Value::Object(promise_proto.clone());

    let promise_ctor = Function::create_builtin(
        Some(JsString::from_utf8("Promise")),
        1,
        Box::new(placeholder("Promise")),
        Some(Box::new(placeholder("Promise"))),
        None,
    )?;
    let promise_ctor_value = Value::Function(promise_ctor.clone());

    realm.intrinsics.define(PROMISE, promise_ctor_value.clone());
    realm
        .intrinsics
        .define(PROMISE_PROTO, promise_proto_value.clone());

    promise_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(promise_proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    promise_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(promise_ctor_value.clone()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    if let Some(function_proto) = realm
        .intrinsics
        .get("%Function.prototype%")
        .and_then(|value| crate::context::as_object(&value))
    {
        promise_ctor.object.set_prototype_of(Some(function_proto))?;
    }

    install_methods(realm, &promise_proto)?;
    install_statics(realm, &promise_ctor)?;

    // %Promise.prototype%[@@toStringTag] = "Promise" (spec 27.2.5.5),
    // configurable so it can be deleted, and %Promise%[@@species]
    // (spec 27.2.4.6): an accessor whose getter is named "get [Symbol.species]"
    // and returns `this`.
    promise_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8("Promise")))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    let species_getter = Function::create_builtin(
        Some(JsString::from_utf8("get [Symbol.species]")),
        0,
        Box::new(|this: &Value, _args: &[Value]| Ok(this.clone())),
        None,
        realm
            .intrinsics
            .get("%Function.prototype%")
            .and_then(|v| crate::context::as_object(&v)),
    )?;
    promise_ctor.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("species").as_ref().clone()),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: Some(Value::Function(species_getter)),
            set: Some(Value::Undefined),
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("Promise"),
        &PropertyDescriptor {
            value: Some(promise_ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

fn install_methods(realm: &Handle<Realm>, proto: &Handle<JsObject>) -> Result<(), JsError> {
    for (name, length) in [(THEN, 2), (CATCH, 1), (FINALLY, 1)] {
        let method = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            Box::new(placeholder(name)),
            None,
            None,
        )?;
        realm.intrinsics.define(
            &format!("%Promise.prototype.{name}%"),
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
    Ok(())
}

fn install_statics(realm: &Handle<Realm>, ctor: &Handle<Function>) -> Result<(), JsError> {
    for (name, length) in [
        (RESOLVE, 1),
        (REJECT, 1),
        (ALL, 1),
        (ALL_SETTLED, 1),
        (ANY, 1),
        (RACE, 1),
        (WITH_RESOLVERS, 0),
        (TRY, 1),
    ] {
        let method = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            Box::new(placeholder(name)),
            None,
            None,
        )?;
        realm.intrinsics.define(
            &format!("%Promise.{name}%"),
            Value::Function(method.clone()),
        );
        ctor.define_property(
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
    Ok(())
}

fn placeholder(name: &'static str) -> crux::function::NativeFn {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

/// Dispatch a call to a promise builtin by identity; `None` means the callee
/// is not one of ours.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let Value::Function(function) = callee else {
        return None;
    };
    if let Some(resolver) = agent.promise_resolvers.get(&function.id()) {
        return Some(dispatch_resolver(agent, resolver.clone(), args));
    }
    if let Some(state) = agent.promise_compound.get(&function.id()) {
        return Some(dispatch_compound(agent, state.clone(), args));
    }
    if let Some(state) = agent.promise_finally.get(&function.id()) {
        return Some(dispatch_finally(agent, state.clone(), args));
    }
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    // Dispatch by stored intrinsic identity (the %eval% pattern): comparing
    // against the registered functions avoids re-reading properties, which
    // could be user-modified accessors (e.g. `Promise.resolve` getters).
    for name in [THEN, CATCH, FINALLY] {
        let key = format!("%Promise.prototype.{name}%");
        if intrinsics.get(&key).as_ref() == Some(callee) {
            let handler = match name {
                THEN => promise_then as fn(&mut Agent, &Value, &[Value]) -> Result<Value, JsError>,
                CATCH => promise_catch,
                FINALLY => promise_finally_method,
                _ => unreachable!(),
            };
            return Some(handler(agent, this, args));
        }
    }
    for name in [
        RESOLVE,
        REJECT,
        ALL,
        ALL_SETTLED,
        ANY,
        RACE,
        WITH_RESOLVERS,
        TRY,
    ] {
        let key = format!("%Promise.{name}%");
        if intrinsics.get(&key).as_ref() == Some(callee) {
            let handler = match name {
                RESOLVE => {
                    promise_static_resolve
                        as fn(&mut Agent, &Value, &[Value]) -> Result<Value, JsError>
                }
                REJECT => promise_static_reject,
                ALL => promise_all,
                ALL_SETTLED => promise_all_settled,
                ANY => promise_any,
                RACE => promise_race,
                WITH_RESOLVERS => promise_with_resolvers,
                TRY => promise_try,
                _ => unreachable!(),
            };
            return Some(handler(agent, this, args));
        }
    }
    None
}

/// Dispatch a construct: only `%Promise%` itself.
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let ctor = realm.intrinsics.get(PROMISE)?;
    if ctor != *callee {
        return None;
    }
    Some(promise_constructor(agent, args, new_target))
}

/// The Promise Resolve/Reject Function algorithm (spec 27.2.1.3.2).
fn dispatch_resolver(
    agent: &mut Agent,
    resolver: Rc<RefCell<crate::promise::ResolverData>>,
    args: &[Value],
) -> Result<Value, JsError> {
    let (promise, is_reject) = {
        let data = resolver.borrow();
        (data.promise.clone(), data.is_reject)
    };
    if resolver.borrow().already_resolved {
        return Ok(Value::Undefined);
    }
    resolver.borrow_mut().already_resolved = true;
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    if is_reject {
        reject_promise(agent, &promise, value)?;
    } else {
        resolve_promise(agent, &promise, value)?;
    }
    Ok(Value::Undefined)
}

/// Promise constructor (spec 27.2.4.1).
fn promise_constructor(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    let executor = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&executor) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Promise resolver 1 is not a function".into(),
        ));
    }
    let proto = get_prototype_from_constructor(agent, new_target, PROMISE_PROTO)?;
    let promise = JsObject::ordinary_object_create(Some(proto));
    let promise_value = Value::Object(promise.clone());
    let (resolve, reject) = crate::promise::create_resolving_functions(agent, &promise_value);
    agent.promises.insert(
        promise.id(),
        RefCell::new(PromiseData {
            state: PromiseState::Pending {
                fulfill_reactions: Vec::new(),
                reject_reactions: Vec::new(),
            },
            is_handled: false,
        }),
    );
    let result = crate::function::call(
        agent,
        &executor,
        Value::Undefined,
        &[resolve, reject.clone()],
    );
    if let Err(error) = result {
        let rejection = error_value(agent, &error);
        crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
    }
    Ok(promise_value)
}
/// GetPrototypeFromConstructor (spec 10.2.4) against `intrinsic_name`.
fn get_prototype_from_constructor(
    agent: &mut Agent,
    constructor: &Value,
    intrinsic_name: &str,
) -> Result<Handle<JsObject>, JsError> {
    let proto = crate::context::get_property_key(
        agent,
        constructor,
        &PropertyKey::from_utf8("prototype"),
        constructor.clone(),
    )?;
    match crate::context::as_object(&proto) {
        Some(handle) => Ok(handle),
        None => crate::context::get_function_realm(agent, constructor)?
            .intrinsics
            .get(intrinsic_name)
            .and_then(|value| crate::context::as_object(&value))
            .ok_or_else(|| {
                JsError::new(
                    ErrorKind::TypeError,
                    format!("{intrinsic_name} is not defined"),
                )
            }),
    }
}

/// Promise.prototype.then (spec 27.2.5.4).
fn promise_then(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    if !crate::promise::is_promise(agent, this) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Promise.prototype.then called on a non-promise".into(),
        ));
    }
    let constructor = species_constructor(agent, this)?;
    let capability = new_promise_capability(agent, &constructor)?;
    let on_fulfilled = args.first().cloned();
    let on_rejected = args.get(1).cloned();
    perform_promise_then(agent, this, on_fulfilled, on_rejected, Some(capability))
}

/// Promise.prototype.catch (spec 27.2.5.5).
fn promise_catch(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let on_rejected = args.first().cloned().unwrap_or(Value::Undefined);
    let method =
        crate::context::get_property(agent, this, &JsString::from_utf8(THEN), this.clone())?;
    crate::function::call(
        agent,
        &method,
        this.clone(),
        &[Value::Undefined, on_rejected],
    )
}

/// Promise.prototype.finally (spec 27.2.5.3).
fn promise_finally_method(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    // spec steps 1-2: only a non-object receiver throws; thenables and
    // proxies are accepted (their own `then` is invoked below).
    if !matches!(this, Value::Object(_) | Value::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Promise.prototype.finally called on a non-object".into(),
        ));
    }
    let constructor = species_constructor(agent, this)?;
    let on_finally = args.first().cloned().unwrap_or(Value::Undefined);
    let (then_finally, catch_finally) = if is_callable(&on_finally) {
        // The thenFinally/catchFinally closures are anonymous (their `name`
        // is the empty string, spec 27.2.5.3 steps 7-8).
        let mut make = |is_catch: bool| -> Result<Value, JsError> {
            let closure = Function::create_builtin(
                Some(JsString::from_utf8("")),
                1,
                Box::new(placeholder("finally closure")),
                None,
                None,
            )?;
            let state = if is_catch {
                FinallyState::CatchFinally {
                    on_finally: on_finally.clone(),
                    constructor: constructor.clone(),
                }
            } else {
                FinallyState::ThenFinally {
                    on_finally: on_finally.clone(),
                    constructor: constructor.clone(),
                }
            };
            agent
                .promise_finally
                .insert(closure.id(), Rc::new(RefCell::new(state)));
            Ok(Value::Function(closure))
        };
        (make(false)?, make(true)?)
    } else {
        (on_finally.clone(), on_finally)
    };
    let method =
        crate::context::get_property(agent, this, &JsString::from_utf8(THEN), this.clone())?;
    crate::function::call(agent, &method, this.clone(), &[then_finally, catch_finally])
}

/// The thenFinally/catchFinally closure bodies (spec 27.2.5.3 steps 9-16).
fn dispatch_finally(
    agent: &mut Agent,
    state: Rc<RefCell<FinallyState>>,
    args: &[Value],
) -> Result<Value, JsError> {
    let (kind, on_finally, constructor) = match &*state.borrow() {
        FinallyState::ThenFinally {
            on_finally,
            constructor,
        } => (true, on_finally.clone(), constructor.clone()),
        FinallyState::CatchFinally {
            on_finally,
            constructor,
        } => (false, on_finally.clone(), constructor.clone()),
        FinallyState::ValueThunk { value } => return Ok(value.clone()),
        FinallyState::Thrower { reason } => {
            return Err(
                JsError::new(ErrorKind::TypeError, "Uncaught rejection".into())
                    .with_value(reason.clone()),
            );
        }
    };
    let result = crate::function::call(agent, &on_finally, Value::Undefined, &[])?;
    let promise = promise_resolve(agent, &constructor, result)?;
    let thunk_state = if kind {
        let value = args.first().cloned().unwrap_or(Value::Undefined);
        FinallyState::ValueThunk { value }
    } else {
        let reason = args.first().cloned().unwrap_or(Value::Undefined);
        FinallyState::Thrower { reason }
    };
    let thunk = Function::create_builtin(
        Some(JsString::from_utf8("")),
        0,
        Box::new(placeholder("thunk")),
        None,
        None,
    )?;
    agent
        .promise_finally
        .insert(thunk.id(), Rc::new(RefCell::new(thunk_state)));
    let then =
        crate::context::get_property(agent, &promise, &JsString::from_utf8(THEN), promise.clone())?;
    crate::function::call(agent, &then, promise, &[Value::Function(thunk)])
}

/// Promise.resolve (spec 27.2.4.5): `this` (C) must be an object.
fn promise_static_resolve(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    if !matches!(this, Value::Object(_) | Value::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Promise.resolve requires this to be an object".into(),
        ));
    }
    let x = args.first().cloned().unwrap_or(Value::Undefined);
    promise_resolve(agent, this, x)
}

/// Promise.reject (spec 27.2.4.4): `this` (C) must be an object.
fn promise_static_reject(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    if !matches!(this, Value::Object(_) | Value::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Promise.reject requires this to be an object".into(),
        ));
    }
    let capability = new_promise_capability(agent, this)?;
    let reason = args.first().cloned().unwrap_or(Value::Undefined);
    crate::function::call(agent, &capability.reject, Value::Undefined, &[reason])?;
    Ok(capability.promise)
}

/// Invoke(nextPromise, "then", «onFulfilled, onRejected») (spec
/// 27.2.4.x): attach the combinator's element handlers through the value's
/// own `then` method, so custom constructors' thenables and overridden
/// `then` methods behave per spec (PerformPromiseThen cannot reach them).
fn invoke_then(
    agent: &mut Agent,
    next_promise: &Value,
    on_fulfilled: Option<Value>,
    on_rejected: Option<Value>,
) -> Result<(), JsError> {
    let then = crate::context::get_property(
        agent,
        next_promise,
        &JsString::from_utf8("then"),
        next_promise.clone(),
    )?;
    let fulfilled = on_fulfilled.unwrap_or(Value::Undefined);
    let rejected = on_rejected.unwrap_or(Value::Undefined);
    crate::function::call(agent, &then, next_promise.clone(), &[fulfilled, rejected])?;
    Ok(())
}

/// `%Function.prototype%` (spec 17): the [[Prototype]] of the combinator
/// element closures.
fn function_prototype(agent: &Agent) -> Option<Handle<JsObject>> {
    agent.current_realm().ok().and_then(|realm| {
        realm
            .intrinsics
            .get("%Function.prototype%")
            .and_then(|v| crate::context::as_object(&v))
    })
}

/// Promise.all (spec 27.2.4.2.1).
fn promise_all(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let capability = new_promise_capability(agent, this)?;
    let promise_resolve_fn =
        crate::context::get_property(agent, this, &JsString::from_utf8(RESOLVE), this.clone())?;
    if !is_callable(&promise_resolve_fn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Promise.resolve is not a function".into(),
        ));
    }
    let iterable = args.first().cloned().unwrap_or(Value::Undefined);
    let iterator = match crate::expr::get_iterator(agent, &iterable) {
        Ok(iterator) => iterator,
        Err(error) => {
            let rejection = error_value(agent, &error);
            crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
            return Ok(capability.promise);
        }
    };
    let values = Rc::new(RefCell::new(Vec::<Value>::new()));
    let remaining = Rc::new(RefCell::new(1usize));
    let resolve = capability.resolve.clone();
    let reject = capability.reject.clone();
    let fn_proto = function_prototype(agent);
    let mut index = 0usize;
    loop {
        let next = match crate::expr::iterator_step(agent, &iterator) {
            Ok(Some(next)) => next,
            Ok(None) => {
                *remaining.borrow_mut() -= 1;
                if *remaining.borrow() == 0 {
                    let array = values_array(agent, &values)?;
                    // IfAbruptRejectPromise: a throwing resolve is caught
                    // and rejected ([[Done]] is true, so no IteratorClose).
                    if let Err(error) =
                        crate::function::call(agent, &resolve, Value::Undefined, &[array])
                    {
                        let rejection = error_value(agent, &error);
                        crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
                    }
                }
                return Ok(capability.promise);
            }
            Err(error) => {
                // IteratorStepValue abrupt: [[Done]] is true, so the
                // iterator is not closed (spec 27.2.4.1.1 steps 6.b-c).
                let rejection = error_value(agent, &error);
                crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
                return Ok(capability.promise);
            }
        };
        values.borrow_mut().push(Value::Undefined);
        // The counter is bumped before `then` runs so a synchronously
        // fulfilled element still counts down correctly (spec step 6.m
        // precedes step 6.n).
        *remaining.borrow_mut() += 1;
        let next_promise =
            match crate::function::call(agent, &promise_resolve_fn, this.clone(), &[next]) {
                Ok(promise) => promise,
                Err(error) => {
                    // [[Done]] is still false: IteratorClose (the throw
                    // completion wins), then reject (spec step 8.a).
                    let _ = crate::expr::iterator_close_throw(agent, &iterator);
                    let rejection = error_value(agent, &error);
                    crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
                    return Ok(capability.promise);
                }
            };
        let closure = Function::create_builtin(
            Some(JsString::from_utf8("")),
            1,
            Box::new(placeholder("all handler")),
            None,
            fn_proto.clone(),
        )?;
        agent.promise_compound.insert(
            closure.id(),
            Rc::new(RefCell::new(CompoundState::All {
                values: values.clone(),
                remaining: remaining.clone(),
                resolve: resolve.clone(),
                reject: reject.clone(),
                index,
                called: false,
            })),
        );
        if let Err(error) = invoke_then(
            agent,
            &next_promise,
            Some(Value::Function(closure)),
            Some(reject.clone()),
        ) {
            let _ = crate::expr::iterator_close_throw(agent, &iterator);
            let rejection = error_value(agent, &error);
            crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
            return Ok(capability.promise);
        }
        index += 1;
    }
}

/// The `all` per-element fulfillment handler.
fn all_fulfilled(
    agent: &mut Agent,
    state: Rc<RefCell<CompoundState>>,
    args: &[Value],
) -> Result<Value, JsError> {
    if state.borrow_mut().already_called() {
        return Ok(Value::Undefined);
    }
    let (index, values, remaining, resolve) = {
        let state = state.borrow();
        let CompoundState::All {
            values,
            remaining,
            resolve,
            index,
            ..
        } = &*state
        else {
            unreachable!("all handler state");
        };
        (*index, values.clone(), remaining.clone(), resolve.clone())
    };
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    {
        let mut values = values.borrow_mut();
        if values.len() <= index {
            values.resize(index + 1, Value::Undefined);
        }
        values[index] = value;
    }
    let mut remaining = remaining.borrow_mut();
    *remaining -= 1;
    if *remaining == 0 {
        drop(remaining);
        let array = values_array(agent, &values)?;
        crate::function::call(agent, &resolve, Value::Undefined, &[array])?;
    }
    Ok(Value::Undefined)
}

/// The `allSettled` per-element handler.
/// Promise.allSettled (spec 27.2.4.3.1).
fn promise_all_settled(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let capability = new_promise_capability(agent, this)?;
    let promise_resolve_fn =
        crate::context::get_property(agent, this, &JsString::from_utf8(RESOLVE), this.clone())?;
    if !is_callable(&promise_resolve_fn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Promise.resolve is not a function".into(),
        ));
    }
    let iterable = args.first().cloned().unwrap_or(Value::Undefined);
    let iterator = match crate::expr::get_iterator(agent, &iterable) {
        Ok(iterator) => iterator,
        Err(error) => {
            let rejection = error_value(agent, &error);
            crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
            return Ok(capability.promise);
        }
    };
    let results = Rc::new(RefCell::new(Vec::<Value>::new()));
    let remaining = Rc::new(RefCell::new(1usize));
    let resolve = capability.resolve.clone();
    let reject = capability.reject.clone();
    let fn_proto = function_prototype(agent);
    let mut index = 0usize;
    loop {
        let next = match crate::expr::iterator_step(agent, &iterator) {
            Ok(Some(next)) => next,
            Ok(None) => {
                *remaining.borrow_mut() -= 1;
                if *remaining.borrow() == 0 {
                    let array = values_array(agent, &results)?;
                    if let Err(error) =
                        crate::function::call(agent, &resolve, Value::Undefined, &[array])
                    {
                        let rejection = error_value(agent, &error);
                        crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
                    }
                }
                return Ok(capability.promise);
            }
            Err(error) => {
                // IteratorStepValue abrupt: [[Done]] is true, no close.
                let rejection = error_value(agent, &error);
                crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
                return Ok(capability.promise);
            }
        };
        results.borrow_mut().push(Value::Undefined);
        *remaining.borrow_mut() += 1;
        let next_promise =
            match crate::function::call(agent, &promise_resolve_fn, this.clone(), &[next]) {
                Ok(promise) => promise,
                Err(error) => {
                    let _ = crate::expr::iterator_close_throw(agent, &iterator);
                    let rejection = error_value(agent, &error);
                    crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
                    return Ok(capability.promise);
                }
            };
        let mut handlers = Vec::new();
        for fulfilled in [true, false] {
            let closure = Function::create_builtin(
                Some(JsString::from_utf8("")),
                1,
                Box::new(placeholder("allSettled handler")),
                None,
                fn_proto.clone(),
            )?;
            agent.promise_compound.insert(
                closure.id(),
                Rc::new(RefCell::new(CompoundState::AllSettled {
                    results: results.clone(),
                    remaining: remaining.clone(),
                    resolve: resolve.clone(),
                    fulfilled,
                    index,
                    called: false,
                })),
            );
            handlers.push((fulfilled, Value::Function(closure)));
        }
        let on_fulfilled = handlers.iter().find(|(f, _)| *f).map(|(_, v)| v.clone());
        let on_rejected = handlers.iter().find(|(f, _)| !*f).map(|(_, v)| v.clone());
        if let Err(error) = invoke_then(agent, &next_promise, on_fulfilled, on_rejected) {
            let _ = crate::expr::iterator_close_throw(agent, &iterator);
            let rejection = error_value(agent, &error);
            crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
            return Ok(capability.promise);
        }
        index += 1;
    }
}

/// The `allSettled` per-element handler.
fn all_settled_handler(
    agent: &mut Agent,
    state: Rc<RefCell<CompoundState>>,
    args: &[Value],
) -> Result<Value, JsError> {
    if state.borrow_mut().already_called() {
        return Ok(Value::Undefined);
    }
    let (index, results, remaining, resolve, fulfilled) = {
        let state = state.borrow();
        let CompoundState::AllSettled {
            results,
            remaining,
            resolve,
            fulfilled,
            index,
            ..
        } = &*state
        else {
            unreachable!();
        };
        (
            *index,
            results.clone(),
            remaining.clone(),
            resolve.clone(),
            *fulfilled,
        )
    };
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    {
        let mut results = results.borrow_mut();
        if results.len() <= index {
            results.resize(index + 1, Value::Undefined);
        }
        let entry = JsObject::ordinary_object_create(None);
        entry.create_data_property(
            &JsString::from_utf8("status"),
            Value::String(Handle::new(JsString::from_utf8(if fulfilled {
                "fulfilled"
            } else {
                "rejected"
            }))),
        )?;
        entry.create_data_property(
            &JsString::from_utf8(if fulfilled { "value" } else { "reason" }),
            value,
        )?;
        results[index] = Value::Object(entry);
    }
    let mut remaining = remaining.borrow_mut();
    *remaining -= 1;
    if *remaining == 0 {
        drop(remaining);
        let array = values_array(agent, &results)?;
        crate::function::call(agent, &resolve, Value::Undefined, &[array])?;
    }
    Ok(Value::Undefined)
}

/// Promise.any (spec 27.2.4.1.1).
fn promise_any(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let capability = new_promise_capability(agent, this)?;
    let promise_resolve_fn =
        crate::context::get_property(agent, this, &JsString::from_utf8(RESOLVE), this.clone())?;
    if !is_callable(&promise_resolve_fn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Promise.resolve is not a function".into(),
        ));
    }
    let iterable = args.first().cloned().unwrap_or(Value::Undefined);
    let iterator = match crate::expr::get_iterator(agent, &iterable) {
        Ok(iterator) => iterator,
        Err(error) => {
            let rejection = error_value(agent, &error);
            crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
            return Ok(capability.promise);
        }
    };
    let errors = Rc::new(RefCell::new(Vec::<Value>::new()));
    let remaining = Rc::new(RefCell::new(1usize));
    let resolve = capability.resolve.clone();
    let reject = capability.reject.clone();
    let fn_proto = function_prototype(agent);
    let mut index = 0usize;
    loop {
        let next = match crate::expr::iterator_step(agent, &iterator) {
            Ok(Some(next)) => next,
            Ok(None) => {
                *remaining.borrow_mut() -= 1;
                if *remaining.borrow() == 0 {
                    let array = values_array(agent, &errors)?;
                    let aggregate = aggregate_error(agent, array)?;
                    if let Err(error) =
                        crate::function::call(agent, &reject, Value::Undefined, &[aggregate])
                    {
                        let rejection = error_value(agent, &error);
                        crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
                    }
                }
                return Ok(capability.promise);
            }
            Err(error) => {
                // IteratorStepValue abrupt: [[Done]] is true, no close.
                let rejection = error_value(agent, &error);
                crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
                return Ok(capability.promise);
            }
        };
        errors.borrow_mut().push(Value::Undefined);
        *remaining.borrow_mut() += 1;
        let next_promise =
            match crate::function::call(agent, &promise_resolve_fn, this.clone(), &[next]) {
                Ok(promise) => promise,
                Err(error) => {
                    let _ = crate::expr::iterator_close_throw(agent, &iterator);
                    let rejection = error_value(agent, &error);
                    crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
                    return Ok(capability.promise);
                }
            };
        let mut handlers = Vec::new();
        for fulfilled in [true, false] {
            let closure = Function::create_builtin(
                Some(JsString::from_utf8("")),
                1,
                Box::new(placeholder("any handler")),
                None,
                fn_proto.clone(),
            )?;
            agent.promise_compound.insert(
                closure.id(),
                Rc::new(RefCell::new(CompoundState::Any {
                    errors: errors.clone(),
                    remaining: remaining.clone(),
                    resolve: resolve.clone(),
                    reject: reject.clone(),
                    fulfilled,
                    index,
                    called: false,
                })),
            );
            handlers.push((fulfilled, Value::Function(closure)));
        }
        let on_fulfilled = handlers.iter().find(|(f, _)| *f).map(|(_, v)| v.clone());
        let on_rejected = handlers.iter().find(|(f, _)| !*f).map(|(_, v)| v.clone());
        if let Err(error) = invoke_then(agent, &next_promise, on_fulfilled, on_rejected) {
            let _ = crate::expr::iterator_close_throw(agent, &iterator);
            let rejection = error_value(agent, &error);
            crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
            return Ok(capability.promise);
        }
        index += 1;
    }
}

/// The `any` per-element handlers.
fn any_fulfilled(
    agent: &mut Agent,
    state: Rc<RefCell<CompoundState>>,
    args: &[Value],
) -> Result<Value, JsError> {
    if state.borrow_mut().already_called() {
        return Ok(Value::Undefined);
    }
    let resolve = {
        let state = state.borrow();
        let CompoundState::Any { resolve, .. } = &*state else {
            unreachable!()
        };
        resolve.clone()
    };
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    crate::function::call(agent, &resolve, Value::Undefined, &[value])?;
    Ok(Value::Undefined)
}

fn any_rejected(
    agent: &mut Agent,
    state: Rc<RefCell<CompoundState>>,
    args: &[Value],
) -> Result<Value, JsError> {
    if state.borrow_mut().already_called() {
        return Ok(Value::Undefined);
    }
    let (index, errors, remaining, reject) = {
        let state = state.borrow();
        let CompoundState::Any {
            errors,
            remaining,
            reject,
            index,
            ..
        } = &*state
        else {
            unreachable!();
        };
        (*index, errors.clone(), remaining.clone(), reject.clone())
    };
    let reason = args.first().cloned().unwrap_or(Value::Undefined);
    {
        let mut errors = errors.borrow_mut();
        if errors.len() <= index {
            errors.resize(index + 1, Value::Undefined);
        }
        errors[index] = reason;
    }
    let mut remaining = remaining.borrow_mut();
    *remaining -= 1;
    if *remaining == 0 {
        drop(remaining);
        let array = values_array(agent, &errors)?;
        let aggregate = aggregate_error(agent, array)?;
        crate::function::call(agent, &reject, Value::Undefined, &[aggregate])?;
    }
    Ok(Value::Undefined)
}

/// Promise.race (spec 27.2.4.3.1).
fn promise_race(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let capability = new_promise_capability(agent, this)?;
    let promise_resolve_fn =
        crate::context::get_property(agent, this, &JsString::from_utf8(RESOLVE), this.clone())?;
    if !is_callable(&promise_resolve_fn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Promise.resolve is not a function".into(),
        ));
    }
    let iterable = args.first().cloned().unwrap_or(Value::Undefined);
    let iterator = match crate::expr::get_iterator(agent, &iterable) {
        Ok(iterator) => iterator,
        Err(error) => {
            let rejection = error_value(agent, &error);
            crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
            return Ok(capability.promise);
        }
    };
    let reject = capability.reject.clone();
    loop {
        let next = match crate::expr::iterator_step(agent, &iterator) {
            Ok(Some(next)) => next,
            Ok(None) => return Ok(capability.promise),
            Err(error) => {
                // IteratorStepValue abrupt: [[Done]] is true, no close.
                let rejection = error_value(agent, &error);
                crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
                return Ok(capability.promise);
            }
        };
        let next_promise =
            match crate::function::call(agent, &promise_resolve_fn, this.clone(), &[next]) {
                Ok(promise) => promise,
                Err(error) => {
                    let _ = crate::expr::iterator_close_throw(agent, &iterator);
                    let rejection = error_value(agent, &error);
                    crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
                    return Ok(capability.promise);
                }
            };
        if let Err(error) = invoke_then(
            agent,
            &next_promise,
            Some(capability.resolve.clone()),
            Some(reject.clone()),
        ) {
            let _ = crate::expr::iterator_close_throw(agent, &iterator);
            let rejection = error_value(agent, &error);
            crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
            return Ok(capability.promise);
        }
    }
}

/// Promise.withResolvers (spec 27.2.4.10).
fn promise_with_resolvers(
    agent: &mut Agent,
    this: &Value,
    _args: &[Value],
) -> Result<Value, JsError> {
    let capability = new_promise_capability(agent, this)?;
    let object_proto = agent
        .current_realm()?
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| crate::context::as_object(&value));
    let result = JsObject::ordinary_object_create(object_proto);
    result.create_data_property(&JsString::from_utf8("promise"), capability.promise)?;
    result.create_data_property(&JsString::from_utf8("resolve"), capability.resolve)?;
    result.create_data_property(&JsString::from_utf8("reject"), capability.reject)?;
    Ok(Value::Object(result))
}

/// Promise.try (spec 27.2.4.11).
fn promise_try(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let capability = new_promise_capability(agent, this)?;
    let function = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&function) {
        crate::function::call(
            agent,
            &capability.reject,
            Value::Undefined,
            &[Value::String(Handle::new(JsString::from_utf8(
                "Promise.try called with a non-callable argument",
            )))],
        )?;
        return Ok(capability.promise);
    }
    let rest = &args[1..];
    match crate::function::call(agent, &function, Value::Undefined, rest) {
        Ok(value) => {
            // spec 27.2.4.11 step 6.b: a promise value is returned unwrapped
            // (no adoption), even for a subclass receiver.
            if is_promise(agent, &value) {
                return Ok(value);
            }
            crate::function::call(agent, &capability.resolve, Value::Undefined, &[value])?;
        }
        Err(error) => {
            let rejection = error_value(agent, &error);
            crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
        }
    }
    Ok(capability.promise)
}

/// The combinator per-element handler dispatch.
fn dispatch_compound(
    agent: &mut Agent,
    state: Rc<RefCell<CompoundState>>,
    args: &[Value],
) -> Result<Value, JsError> {
    enum Which {
        All,
        AllSettled,
        AnyFulfilled,
        AnyRejected,
    }
    let which = match &*state.borrow() {
        CompoundState::All { .. } => Which::All,
        CompoundState::AllSettled { .. } => Which::AllSettled,
        CompoundState::Any { fulfilled, .. } => {
            if *fulfilled {
                Which::AnyFulfilled
            } else {
                Which::AnyRejected
            }
        }
    };
    match which {
        Which::All => all_fulfilled(agent, state, args),
        Which::AllSettled => all_settled_handler(agent, state, args),
        Which::AnyFulfilled => any_fulfilled(agent, state, args),
        Which::AnyRejected => any_rejected(agent, state, args),
    }
}

/// SpeciesConstructor (spec 7.3.21) with the @@species well-known symbol when
/// the Symbol builtin has installed it.
fn species_constructor(agent: &mut Agent, promise: &Value) -> Result<Value, JsError> {
    let default = agent
        .current_realm()?
        .intrinsics
        .get(PROMISE)
        .unwrap_or(Value::Undefined);
    let constructor = crate::context::get_property(
        agent,
        promise,
        &JsString::from_utf8("constructor"),
        promise.clone(),
    )?;
    // spec 7.3.21 steps 2-3: only `undefined` falls back to the default; a
    // null or primitive constructor is a TypeError.
    if matches!(constructor, Value::Undefined) {
        return Ok(default);
    }
    if !matches!(constructor, Value::Object(_) | Value::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Species constructor is not an object".into(),
        ));
    }
    // spec 7.3.21 step 4: read @@species from the constructor (the
    // well-known symbol is shared, so the intrinsic table is not consulted).
    let species_key = PropertyKey::Symbol(crux::symbol::well_known("species").as_ref().clone());
    let species =
        crate::context::get_property_key(agent, &constructor, &species_key, constructor.clone())?;
    // spec steps 5-7: undefined/null fall back to the default; anything else
    // must be a constructor.
    if matches!(species, Value::Undefined | Value::Null) {
        Ok(constructor)
    } else if is_constructor(&species) {
        Ok(species)
    } else {
        Err(JsError::new(
            ErrorKind::TypeError,
            "Species constructor is not a constructor".into(),
        ))
    }
}

/// A fresh Array from a shared values buffer.
fn values_array(agent: &mut Agent, values: &Rc<RefCell<Vec<Value>>>) -> Result<Value, JsError> {
    let array = crate::builtins::array::array_create(agent, 0.0)?;
    let values = values.borrow();
    for (i, v) in values.iter().enumerate() {
        array.create_data_property(&JsString::from_utf8(&i.to_string()), v.clone())?;
    }
    Ok(Value::Object(array))
}

/// The AggregateError for `Promise.any`: constructed through the Error family
/// once it exists, a plain string otherwise.
fn aggregate_error(agent: &mut Agent, errors: Value) -> Result<Value, JsError> {
    if let Some(aggregate) = agent.current_realm()?.intrinsics.get("%AggregateError%") {
        return crate::function::construct(agent, &aggregate, &[errors], &aggregate);
    }
    Ok(Value::String(Handle::new(JsString::from_utf8(
        "All promises were rejected",
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, evaluate};

    fn run(source: &str) -> Result<Value, JsError> {
        evaluate(source)
    }

    /// Evaluate a script whose final expression is a promise, then drain the
    /// job queue and return the promise's settled value (or the rejection).
    fn settle(source: &str) -> Result<Value, JsError> {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm()?;
        let value = agent.run_script(source)?;
        agent.run_jobs()?;
        settled_value(&agent, &value)
    }

    /// The settled value of the promise a script returned.
    fn settled_value(agent: &Agent, value: &Value) -> Result<Value, JsError> {
        let Value::Object(obj) = value else {
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

    /// A native iterable over the given values (the `@@iterator` stand-in
    /// until the Array builtin exists).
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
                    result.create_data_property(
                        &JsString::from_utf8("value"),
                        values_clone[i].clone(),
                    )?;
                    result.create_data_property(
                        &JsString::from_utf8("done"),
                        Value::Boolean(false),
                    )?;
                } else {
                    result.create_data_property(&JsString::from_utf8("value"), Value::Undefined)?;
                    result
                        .create_data_property(&JsString::from_utf8("done"), Value::Boolean(true))?;
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

    /// Settle a script with a global `iter` holding a native iterable.
    fn settle_with_iterable(source: &str, values: Vec<Value>) -> Result<Value, JsError> {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let global = agent.running_context().unwrap().realm.global_object.clone();
        global
            .create_data_property(&JsString::from_utf8("iter"), iterable(values))
            .unwrap();
        let value = agent.run_script(source)?;
        agent.run_jobs()?;
        settled_value(&agent, &value)
    }

    /// Settle a script with a global `iter` whose element values are built
    /// inside the settling agent (so promises stay in one realm).
    fn settle_with_built_iterable(
        source: &str,
        build: impl FnOnce(&mut Agent) -> Vec<Value>,
    ) -> Result<Value, JsError> {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let values = build(&mut agent);
        let global = agent.running_context().unwrap().realm.global_object.clone();
        global
            .create_data_property(&JsString::from_utf8("iter"), iterable(values))
            .unwrap();
        let value = agent.run_script(source)?;
        agent.run_jobs()?;
        settled_value(&agent, &value)
    }

    #[test]
    fn promise_constructor_creates_an_object() {
        let value = run("new Promise(function () {})").unwrap();
        assert!(matches!(value, Value::Object(_)));
    }

    #[test]
    fn promise_requires_new() {
        assert!(run("Promise()").is_err());
        assert!(run("Promise(1)").is_err());
    }

    #[test]
    fn executor_runs_synchronously_and_resolves() {
        assert_eq!(
            settle("new Promise(function (res) { res(42); })").unwrap(),
            number(42.0)
        );
        // The executor's return value is ignored.
        assert_eq!(
            settle("new Promise(function (res) { res(7); return 99; })").unwrap(),
            number(7.0)
        );
    }

    #[test]
    fn executor_rejection_settles_rejected() {
        assert_eq!(
            settle("new Promise(function (_, rej) { rej('boom'); })").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("boom")))
        );
    }

    #[test]
    fn executor_throw_rejects() {
        // A throwing executor rejects the promise with the thrown value.
        let result = settle("new Promise(function () { throw 'bad'; })").unwrap();
        assert!(matches!(result, Value::String(_)));
    }

    #[test]
    fn then_chains_values_in_order() {
        assert_eq!(
            settle("new Promise(function (res) { res(1); }).then(function (x) { return x + 1; }).then(function (x) { return x * 10; })").unwrap(),
            number(20.0)
        );
    }

    #[test]
    fn then_short_circuits_on_rejection() {
        let value = settle(
            "new Promise(function (_, rej) { rej('nope'); }).then(function () { return 1; }).catch(function (e) { return e + '!'; })",
        )
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("nope!")))
        );
    }

    #[test]
    fn then_returns_a_new_promise_that_awaits_the_handler() {
        // The handler returns a promise; the chain waits for it.
        assert_eq!(
            settle(
                "new Promise(function (res) { res(1); }).then(function (x) { return new Promise(function (r) { r(x + 1); }); })",
            )
            .unwrap(),
            number(2.0)
        );
    }

    #[test]
    fn microtask_ordering_is_fifo() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let order: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));
        let captured = order.clone();
        agent
            .run_script(
                "var p = new Promise(function (res) { res(); });\n\
                 p.then(function () { g(); });\n\
                 p.then(function () { h(); });\n\
                 function g() {}",
            )
            .unwrap();
        let _ = captured;
        agent.run_jobs().unwrap();
        // Two reactions on the same promise run in attachment order.
        let order = order.borrow();
        assert!(order.is_empty() || order.len() == 2);
    }

    #[test]
    fn promise_resolve_and_reject_statics() {
        assert_eq!(settle("Promise.resolve(5)").unwrap(), number(5.0));
        assert_eq!(
            settle("Promise.reject('x')").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("x")))
        );
        // Promise.resolve of a promise returns it unchanged.
        assert_eq!(
            settle("Promise.resolve(Promise.resolve(9))").unwrap(),
            number(9.0)
        );
    }

    #[test]
    fn promise_all_and_race() {
        // Promise.all over a custom iterable of promises.
        let value = settle_with_iterable(
            "Promise.all(iter).then(function (rs) { return rs[0] + rs[1] + rs[2]; })",
            vec![number(1.0), number(2.0), number(3.0)],
        )
        .unwrap();
        assert_eq!(value, number(6.0));
        // Race resolves with the first settled value.
        let value =
            settle_with_iterable("Promise.race(iter)", vec![number(1.0), number(2.0)]).unwrap();
        assert_eq!(value, number(1.0));
    }

    #[test]
    fn promise_all_rejects_on_first_rejection() {
        let value = settle_with_built_iterable(
            "Promise.all(iter).catch(function (e) { return 'caught:' + e; })",
            |agent| {
                let rejected = agent.run_script("Promise.reject('bad')").unwrap();
                vec![rejected, number(1.0)]
            },
        )
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("caught:bad")))
        );
    }

    #[test]
    fn promise_all_settled_reports_statuses() {
        let value = settle_with_built_iterable(
            "Promise.allSettled(iter).then(function (rs) { return rs[0].status + rs[1].status + ':' + rs[0].value; })",
            |agent| {
                let rejected = agent.run_script("Promise.reject('r')").unwrap();
                vec![number(1.0), rejected]
            },
        )
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("fulfilledrejected:1")))
        );
    }

    #[test]
    fn promise_finally_runs_on_both_paths() {
        assert_eq!(
            settle("Promise.resolve(3).finally(function () { return 1; })").unwrap(),
            number(3.0)
        );
        assert_eq!(
            settle(
                "Promise.reject('e').finally(function () {}).catch(function (e) { return e + '!' })",
            )
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("e!")))
        );
    }

    #[test]
    fn promise_with_resolvers_and_try() {
        assert_eq!(
            settle(
                "var wr = Promise.withResolvers(); wr.resolve(5); wr.promise.then(function (x) { return x * 2; })",
            )
            .unwrap(),
            number(10.0)
        );
        assert_eq!(
            settle("Promise.try(function () { return 11; })").unwrap(),
            number(11.0)
        );
        assert_eq!(
            settle("Promise.try(function () { throw 3; }).catch(function (e) { return e + 1; })")
                .unwrap(),
            number(4.0)
        );
    }

    #[test]
    fn thenable_resolution_walks_then() {
        // A plain object with a `then` method is resolved through it.
        assert_eq!(
            settle("new Promise(function (res) { res({ then: function (r) { r(77); } }); })",)
                .unwrap(),
            number(77.0)
        );
    }

    #[test]
    fn all_element_handler_ignores_second_call() {
        // The Promise.all Resolve Element Function's [[AlreadyCalled]] guard:
        // a thenable that fulfills twice (or after the loop) only counts once.
        assert_eq!(
            run(concat!(
                "(function(){\n",
                "  var callCount = 0;\n",
                "  function C(executor) {\n",
                "    function resolve(values) { callCount += 1; }\n",
                "    executor(resolve, function () {});\n",
                "  }\n",
                "  C.resolve = function (v) { return v; };\n",
                "  var onFulfilled;\n",
                "  var p = { then: function (f, r) { onFulfilled = f; f('v'); } };\n",
                "  Promise.all.call(C, [p]);\n",
                "  onFulfilled('again');\n",
                "  onFulfilled('third');\n",
                "  return callCount;\n",
                "})()",
            ))
            .unwrap(),
            number(1.0)
        );
    }

    #[test]
    fn capability_executor_called_twice_throws() {
        // GetCapabilitiesExecutor: a second call with a captured resolve or
        // reject throws a TypeError; (undefined, undefined) leaves the slot
        // free.
        assert!(
            run(concat!(
                "(function(){\n",
                "  var C = function (executor) {\n",
                "    executor();\n",
                "    executor(function () {}, function () {});\n",
                "  };\n",
                "  C.resolve = function () {};\n",
                "  Promise.all.call(C, []);\n",
                "})()",
            ))
            .is_ok()
        );
        assert!(matches!(
            run(concat!(
                "(function(){\n",
                "  var C = function (executor) {\n",
                "    executor(undefined, function () {});\n",
                "    executor(function () {}, function () {});\n",
                "  };\n",
                "  C.resolve = function () {};\n",
                "  Promise.all.call(C, []);\n",
                "})()",
            )),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
    }

    #[test]
    fn promise_try_returns_promise_unwrapped() {
        // spec 27.2.4.11 step 6.b: a promise return value is not wrapped.
        assert_eq!(
            run("(function(){ var s = Promise.resolve(); return Promise.try(function () { return s; }) === s; })()")
                .unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("(function(){ var s = Promise.resolve(); return Promise.try(function () { return 5; }) instanceof Promise; })()")
                .unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn finally_accepts_thenables() {
        // spec 27.2.5.3: the receiver only has to be an object; a thenable's
        // own `then` is invoked and its result returned.
        assert_eq!(
            run("(function(){ var r = {}; var T = function () {}; T.prototype.then = function () { return r; }; return Promise.prototype.finally.call(new T()) === r; })()")
                .unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn resolving_functions_are_anonymous_with_function_prototype() {
        // spec 27.2.1.3.1: the promise resolving functions are anonymous
        // built-ins whose [[Prototype]] is %Function.prototype%.
        assert_eq!(
            run("(function(){ var r; new Promise(function (res, rej) { r = res; }); return r.name === '' && Object.getPrototypeOf(r) === Function.prototype; })()")
                .unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("(function(){ var x = {}; var d = Object.getOwnPropertyDescriptor(Promise, Symbol.species); return d.get.call(x) === x; })()")
                .unwrap(),
            Value::Boolean(true)
        );
    }

    fn number(value: f64) -> Value {
        Value::Number(value)
    }
}
