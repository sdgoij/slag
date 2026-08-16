//! The `%AsyncIterator.prototype%` intrinsic (spec 27.1.4): `@@asyncIterator`,
//! `@@asyncDispose`, and the async iterator helpers. The helpers run over the
//! underlying async iterator through promise continuations: a lazy helper
//! (`map`/`filter`/`take`/`drop`/`flatMap`) returns an async-iterator-helper
//! object whose `next()` returns a promise; an eager helper (`reduce`/
//! `toArray`/`forEach`/`some`/`every`/`find`) returns a promise directly.
//! `%AsyncGenerator.prototype%` inherits this prototype.

use std::cell::RefCell;
use std::rc::Rc;

use crux::convert::to_boolean;
use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, is_callable};

use crate::agent::Agent;
use crate::context::as_object;
use crate::expr::get_method;
use crate::promise::{
    PromiseCapability, new_promise_capability, perform_promise_then, promise_resolve,
};
use crate::realm::Realm;

const ASYNC_ITERATOR_PROTO: &str = "%AsyncIterator.prototype%";
const ASYNC_ITERATOR_HELPER_PROTO: &str = "%AsyncIteratorHelper.prototype%";

const MAP: &str = "map";
const FILTER: &str = "filter";
const TAKE: &str = "take";
const DROP: &str = "drop";
const FLAT_MAP: &str = "flatMap";
const REDUCE: &str = "reduce";
const TO_ARRAY: &str = "toArray";
const FOR_EACH: &str = "forEach";
const SOME: &str = "some";
const EVERY: &str = "every";
const FIND: &str = "find";

/// An async Iterator Record: the iterator and its callable `next` method.
#[derive(Debug, Clone)]
pub struct AsyncIteratorRecord {
    pub iterator: Value,
    pub next: Value,
}

/// The mode of an async-iterator helper.
#[derive(Debug)]
pub enum HelperMode {
    Map {
        mapper: Value,
    },
    Filter {
        filterer: Value,
    },
    Take {
        remaining: f64,
    },
    Drop {
        remaining: f64,
    },
    FlatMap {
        mapper: Value,
        inner: Option<AsyncIteratorRecord>,
    },
}

/// The state of an async-iterator-helper object, keyed by object identity.
#[derive(Debug)]
pub struct HelperState {
    pub iterator: Option<AsyncIteratorRecord>,
    pub done: bool,
    pub mode: HelperMode,
}

/// The driver of an eager async helper, keyed by driver-object identity.
#[derive(Debug)]
pub struct EagerState {
    pub record: AsyncIteratorRecord,
    pub mode: EagerMode,
    pub capability: PromiseCapability,
}

/// The eager helper modes.
#[derive(Debug)]
pub enum EagerMode {
    Reduce {
        reducer: Value,
        accumulator: Option<Value>,
        started: bool,
    },
    ToArray {
        values: Vec<Value>,
    },
    ForEach {
        f: Value,
    },
    Some {
        f: Value,
    },
    Every {
        f: Value,
    },
    Find {
        f: Value,
    },
}

/// The continuation of an await in an async-iterator helper driver.
#[derive(Debug, Clone)]
pub enum AwaitEntry {
    Lazy {
        object_id: u64,
        is_reject: bool,
    },
    Eager {
        driver_id: u64,
        is_reject: bool,
    },
    /// The awaited mapped value of a `map` helper.
    Mapped {
        object_id: u64,
        is_reject: bool,
    },
    /// The awaited predicate result of a `filter` helper.
    FilterKeep {
        object_id: u64,
        value: Value,
        is_reject: bool,
    },
    /// A step of the current `flatMap` inner iterator.
    FlatInner {
        object_id: u64,
        is_reject: bool,
    },
}

pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let proto = JsObject::ordinary_object_create(object_proto);
    let proto_value = Value::Object(proto.clone());
    realm.intrinsics.define(ASYNC_ITERATOR_PROTO, proto_value);

    // @@asyncIterator returns `this` (spec 27.1.4.2).
    let function_proto = realm
        .intrinsics
        .get("%Function.prototype%")
        .and_then(|v| as_object(&v));
    let async_iterator = Function::create_builtin(
        Some(JsString::from_utf8("[Symbol.asyncIterator]")),
        0,
        Box::new(|this, _| Ok(this.clone())),
        None,
        function_proto.clone(),
    )?;
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("asyncIterator").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::Function(async_iterator)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // @@asyncDispose closes the iterator through `return` (spec 27.1.4.6).
    let async_dispose = Function::create_builtin(
        Some(JsString::from_utf8("[Symbol.asyncDispose]")),
        0,
        Box::new(placeholder("[Symbol.asyncDispose]")),
        None,
        function_proto,
    )?;
    realm.intrinsics.define(
        "%AsyncIterator.prototype.@@asyncDispose%",
        Value::Function(async_dispose.clone()),
    );
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("asyncDispose").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::Function(async_dispose)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // @@toStringTag = "Async Iterator" with { writable: false, enumerable:
    // false, configurable: true } (spec 27.1.4.3).
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8(
                "Async Iterator",
            )))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // The helper surface.
    for (name, length) in [
        (MAP, 1),
        (FILTER, 1),
        (TAKE, 1),
        (DROP, 1),
        (FLAT_MAP, 1),
        (REDUCE, 1),
        (TO_ARRAY, 0),
        (FOR_EACH, 1),
        (SOME, 1),
        (EVERY, 1),
        (FIND, 1),
    ] {
        let method = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            Box::new(placeholder(name)),
            None,
            None,
        )?;
        realm.intrinsics.define(
            &format!("%AsyncIterator.prototype.{name}%"),
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

    install_helper_prototype(realm)?;
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

/// `%AsyncIteratorHelper.prototype%`: `next`, `return`, `@@asyncIterator`.
fn install_helper_prototype(realm: &Handle<Realm>) -> Result<(), JsError> {
    let async_iterator_proto = realm
        .intrinsics
        .get(ASYNC_ITERATOR_PROTO)
        .and_then(|value| as_object(&value));
    let proto = JsObject::ordinary_object_create(async_iterator_proto);
    let proto_value = Value::Object(proto.clone());
    realm
        .intrinsics
        .define(ASYNC_ITERATOR_HELPER_PROTO, proto_value);
    for name in ["next", "return"] {
        let method = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            0,
            Box::new(placeholder(name)),
            None,
            None,
        )?;
        realm.intrinsics.define(
            &format!("%AsyncIteratorHelper.prototype.{name}%"),
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
    let async_iterator = Function::create_builtin(
        Some(JsString::from_utf8("[Symbol.asyncIterator]")),
        0,
        Box::new(|this, _| Ok(this.clone())),
        None,
        None,
    )?;
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("asyncIterator").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::Function(async_iterator)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor::none(Value::String(Handle::new(JsString::from_utf8(
            "Async Iterator Helper",
        )))),
    )?;
    Ok(())
}

/// A prototype-method handler, dispatched by intrinsic identity.
type MethodHandler = fn(&mut Agent, &Value, &[Value]) -> Result<Value, JsError>;

/// Route a call to an AsyncIterator builtin by identity.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let Value::Function(function) = callee else {
        return None;
    };
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    for name in ["next", "return"] {
        let key = format!("%AsyncIteratorHelper.prototype.{name}%");
        if intrinsics.get(&key).as_ref() == Some(callee) {
            return Some(async_helper_method(agent, name == "return", this, args));
        }
    }
    if intrinsics
        .get("%AsyncIterator.prototype.@@asyncDispose%")
        .as_ref()
        == Some(callee)
    {
        return Some(async_dispose(agent, this, args));
    }
    let proto_value = intrinsics.get(ASYNC_ITERATOR_PROTO)?;
    let Value::Object(_proto_obj) = &proto_value else {
        return None;
    };
    let lazy: &[(&str, MethodHandler)] = &[
        (MAP, map_method),
        (FILTER, filter_method),
        (TAKE, take_method),
        (DROP, drop_method),
        (FLAT_MAP, flat_map_method),
    ];
    for (name, handler) in lazy {
        let key = format!("%AsyncIterator.prototype.{name}%");
        if intrinsics.get(&key).as_ref() == Some(callee) {
            return Some(handler(agent, this, args));
        }
    }
    let eager: &[(&str, MethodHandler)] = &[
        (REDUCE, reduce_method),
        (TO_ARRAY, to_array_method),
        (FOR_EACH, for_each_method),
        (SOME, some_method),
        (EVERY, every_method),
        (FIND, find_method),
    ];
    for (name, handler) in eager {
        let key = format!("%AsyncIterator.prototype.{name}%");
        if intrinsics.get(&key).as_ref() == Some(callee) {
            return Some(handler(agent, this, args));
        }
    }
    // The await continuations of the drivers.
    if let Some(entry) = agent.async_iterator_awaits.get(&function.id()).cloned() {
        return Some(dispatch_await(agent, entry, args));
    }
    None
}

/// `%AsyncIterator.prototype%[@@asyncDispose]` (spec 27.1.4.6).
fn async_dispose(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let capability = new_promise_capability(agent, &promise_ctor)?;
    let promise = capability.promise.clone();
    // spec 27.1.4.6 step 3: GetMethod(return) — a throwing `return` getter
    // rejects the promise (IfAbruptRejectPromise), never throws synchronously.
    let return_method = match crate::context::get_property(
        agent,
        this,
        &JsString::from_utf8("return"),
        this.clone(),
    ) {
        Ok(method) => method,
        Err(error) => {
            let rejection = crate::promise::error_value(agent, &error);
            crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
            return Ok(promise);
        }
    };
    let return_method = match return_method {
        Value::Undefined | Value::Null => None,
        value if is_callable(&value) => Some(value),
        _ => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "iterator return method is not callable".into(),
            ));
        }
    };
    match return_method {
        None => {
            crate::function::call(
                agent,
                &capability.resolve,
                Value::Undefined,
                &[Value::Undefined],
            )?;
        }
        Some(return_method) => {
            let result = crate::function::call(agent, &return_method, this.clone(), &[]);
            match result {
                Ok(result) => {
                    let promise = promise_resolve(agent, &promise_ctor, result)?;
                    perform_promise_then(
                        agent,
                        &promise,
                        Some(capability.resolve.clone()),
                        Some(capability.reject.clone()),
                        Some(capability),
                    )?;
                }
                Err(error) => {
                    let rejection = crate::promise::error_value(agent, &error);
                    crate::function::call(
                        agent,
                        &capability.reject,
                        Value::Undefined,
                        &[rejection],
                    )?;
                }
            }
        }
    }
    Ok(promise)
}

// ---- async-iterator record helpers ----

/// GetIteratorDirect on an async iterator: `this` must be an object with a
/// callable `next`.
fn get_async_iterator_direct(
    agent: &mut Agent,
    this: &Value,
) -> Result<AsyncIteratorRecord, JsError> {
    if !matches!(this, Value::Object(_) | Value::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "AsyncIterator method called on a non-object".into(),
        ));
    }
    let next =
        crate::context::get_property(agent, this, &JsString::from_utf8("next"), this.clone())?;
    if !is_callable(&next) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "AsyncIterator's next method is not callable".into(),
        ));
    }
    Ok(AsyncIteratorRecord {
        iterator: this.clone(),
        next,
    })
}

fn make_await_closure(agent: &mut Agent, entry: AwaitEntry) -> Result<Value, JsError> {
    let closure = Function::create_builtin(
        Some(JsString::from_utf8("")),
        1,
        Box::new(|_, _| {
            Err(JsError::new(
                ErrorKind::TypeError,
                "async iterator await handler must be called through the agent".into(),
            ))
        }),
        None,
        None,
    )?;
    agent
        .async_iterator_awaits
        .insert(closure.id(), Rc::new(entry));
    Ok(Value::Function(closure))
}

/// The continuation of a lazy helper's `next()`.
fn dispatch_await(
    agent: &mut Agent,
    entry: Rc<AwaitEntry>,
    args: &[Value],
) -> Result<Value, JsError> {
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    match &*entry {
        AwaitEntry::Lazy {
            object_id,
            is_reject,
        } => {
            if *is_reject {
                reject_pending(agent, *object_id, value)?;
                return Ok(Value::Undefined);
            }
            continue_lazy(agent, *object_id, value)
        }
        AwaitEntry::Eager {
            driver_id,
            is_reject,
        } => {
            if *is_reject {
                reject_eager(agent, *driver_id, value)?;
                return Ok(Value::Undefined);
            }
            continue_eager(agent, *driver_id, value)
        }
        AwaitEntry::Mapped {
            object_id,
            is_reject,
        } => {
            if *is_reject {
                reject_pending(agent, *object_id, value)?;
                return Ok(Value::Undefined);
            }
            continue_mapped(agent, *object_id, value)
        }
        AwaitEntry::FilterKeep {
            object_id,
            value: keep,
            is_reject,
        } => {
            if *is_reject {
                reject_pending(agent, *object_id, value)?;
                return Ok(Value::Undefined);
            }
            continue_filter(agent, *object_id, keep.clone(), value)
        }
        AwaitEntry::FlatInner {
            object_id,
            is_reject,
        } => {
            if *is_reject {
                reject_pending(agent, *object_id, value)?;
                return Ok(Value::Undefined);
            }
            continue_flat_inner(agent, *object_id, value)
        }
    }
}

/// Reject the pending `next()` promise of an async-iterator helper.
fn reject_pending(agent: &mut Agent, object_id: u64, value: Value) -> Result<(), JsError> {
    let capability = agent
        .async_iterator_pending
        .get(&object_id)
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no pending capability".into()))?;
    let error = JsError::new(ErrorKind::TypeError, "Uncaught rejection".into()).with_value(value);
    let rejection = crate::promise::error_value(agent, &error);
    crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
    Ok(())
}

/// Reject the result promise of an eager helper driver.
fn reject_eager(agent: &mut Agent, driver_id: u64, value: Value) -> Result<(), JsError> {
    let driver = agent
        .async_iterator_eager
        .get(&driver_id)
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no eager driver".into()))?;
    let capability = driver.borrow().capability.clone();
    let error = JsError::new(ErrorKind::TypeError, "Uncaught rejection".into()).with_value(value);
    let rejection = crate::promise::error_value(agent, &error);
    crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
    Ok(())
}

/// The continuation of a `map` helper's awaited mapper result: resolve the
/// pending `next()` promise with `{ mapped, done: false }`.
fn continue_mapped(agent: &mut Agent, object_id: u64, mapped: Value) -> Result<Value, JsError> {
    let capability = agent
        .async_iterator_pending
        .get(&object_id)
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no pending capability".into()))?;
    crate::function::call(
        agent,
        &capability.resolve,
        Value::Undefined,
        &[iterator_result(agent, mapped, false)?],
    )?;
    Ok(Value::Undefined)
}

/// The continuation of a `filter` helper's awaited predicate result.
fn continue_filter(
    agent: &mut Agent,
    object_id: u64,
    value: Value,
    keep: Value,
) -> Result<Value, JsError> {
    let capability = agent
        .async_iterator_pending
        .get(&object_id)
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no pending capability".into()))?;
    if to_boolean(&keep) {
        crate::function::call(
            agent,
            &capability.resolve,
            Value::Undefined,
            &[iterator_result(agent, value, false)?],
        )?;
        return Ok(Value::Undefined);
    }
    // The predicate was false: pull the next underlying value.
    let record =
        {
            let state = agent
                .async_iterator_helpers
                .get(&object_id)
                .cloned()
                .ok_or_else(|| {
                    JsError::new(ErrorKind::TypeError, "not an async iterator helper".into())
                })?;
            state.borrow().iterator.clone().ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "no underlying iterator".into())
            })?
        };
    let result = crate::function::call(agent, &record.next, record.iterator.clone(), &[])?;
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let promise = promise_resolve(agent, &promise_ctor, result)?;
    let on_fulfilled = make_await_closure(
        agent,
        AwaitEntry::Lazy {
            object_id,
            is_reject: false,
        },
    )?;
    let on_rejected = make_await_closure(
        agent,
        AwaitEntry::Lazy {
            object_id,
            is_reject: true,
        },
    )?;
    perform_promise_then(agent, &promise, Some(on_fulfilled), Some(on_rejected), None)?;
    Ok(Value::Undefined)
}

/// The continuation of a `flatMap` helper's inner-iterator step.
fn continue_flat_inner(agent: &mut Agent, object_id: u64, result: Value) -> Result<Value, JsError> {
    if !matches!(result, Value::Object(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator result is not an object".into(),
        ));
    }
    let done =
        crate::context::get_property(agent, &result, &JsString::from_utf8("done"), result.clone())?;
    let value = crate::context::get_property(
        agent,
        &result,
        &JsString::from_utf8("value"),
        result.clone(),
    )?;
    let capability = agent
        .async_iterator_pending
        .get(&object_id)
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no pending capability".into()))?;
    if to_boolean(&done) {
        // The inner iterator finished: clear it and continue the outer.
        let record = {
            let state = agent
                .async_iterator_helpers
                .get(&object_id)
                .cloned()
                .ok_or_else(|| {
                    JsError::new(ErrorKind::TypeError, "not an async iterator helper".into())
                })?;
            let mut state = state.borrow_mut();
            if let HelperMode::FlatMap { inner, .. } = &mut state.mode {
                *inner = None;
            }
            state.iterator.clone().ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "no underlying iterator".into())
            })?
        };
        let result = crate::function::call(agent, &record.next, record.iterator.clone(), &[])?;
        let promise_ctor = agent
            .current_realm()?
            .intrinsics
            .get("%Promise%")
            .unwrap_or(Value::Undefined);
        let promise = promise_resolve(agent, &promise_ctor, result)?;
        let on_fulfilled = make_await_closure(
            agent,
            AwaitEntry::Lazy {
                object_id,
                is_reject: false,
            },
        )?;
        let on_rejected = make_await_closure(
            agent,
            AwaitEntry::Lazy {
                object_id,
                is_reject: true,
            },
        )?;
        perform_promise_then(agent, &promise, Some(on_fulfilled), Some(on_rejected), None)?;
        return Ok(Value::Undefined);
    }
    crate::function::call(
        agent,
        &capability.resolve,
        Value::Undefined,
        &[iterator_result(agent, value, false)?],
    )?;
    Ok(Value::Undefined)
}

/// The `next`/`return` dispatch of an async-iterator-helper object.
fn async_helper_method(
    agent: &mut Agent,
    is_return: bool,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let Value::Object(obj) = this else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "AsyncIterator helper method called on a non-object".into(),
        ));
    };
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let capability = new_promise_capability(agent, &promise_ctor)?;
    let result_promise = capability.promise.clone();
    let state = agent
        .async_iterator_helpers
        .get(&obj.id())
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not an async iterator helper".into()))?;
    if is_return {
        // Close the underlying async iterator through its `return`.
        let record = {
            let mut state = state.borrow_mut();
            state.done = true;
            state.iterator.take()
        };
        let value = args.first().cloned().unwrap_or(Value::Undefined);
        match record {
            None => {
                crate::function::call(
                    agent,
                    &capability.resolve,
                    Value::Undefined,
                    &[iterator_result(agent, Value::Undefined, true)?],
                )?;
            }
            Some(record) => {
                let return_method = crate::context::get_property(
                    agent,
                    &record.iterator,
                    &JsString::from_utf8("return"),
                    record.iterator.clone(),
                )?;
                if is_callable(&return_method) {
                    let result =
                        crate::function::call(agent, &return_method, record.iterator.clone(), &[]);
                    match result {
                        Ok(result) => {
                            let promise = promise_resolve(agent, &promise_ctor, result)?;
                            perform_promise_then(
                                agent,
                                &promise,
                                Some(capability.resolve.clone()),
                                Some(capability.reject.clone()),
                                Some(capability),
                            )?;
                        }
                        Err(error) => {
                            let rejection = crate::promise::error_value(agent, &error);
                            crate::function::call(
                                agent,
                                &capability.reject,
                                Value::Undefined,
                                &[rejection],
                            )?;
                        }
                    }
                } else {
                    crate::function::call(
                        agent,
                        &capability.resolve,
                        Value::Undefined,
                        &[iterator_result(agent, value, true)?],
                    )?;
                }
            }
        }
        return Ok(result_promise);
    }
    // next()
    if state.borrow().done {
        crate::function::call(
            agent,
            &capability.resolve,
            Value::Undefined,
            &[iterator_result(agent, Value::Undefined, true)?],
        )?;
        return Ok(result_promise);
    }
    // An exhausted `take` helper yields done without pulling the underlying
    // iterator again.
    if matches!(
        &state.borrow().mode,
        HelperMode::Take { remaining } if *remaining <= 0.0
    ) {
        crate::function::call(
            agent,
            &capability.resolve,
            Value::Undefined,
            &[iterator_result(agent, Value::Undefined, true)?],
        )?;
        return Ok(result_promise);
    }
    // A `flatMap` helper with an active inner iterator steps it instead of
    // the outer one.
    let flat_inner = {
        let state = state.borrow();
        match &state.mode {
            HelperMode::FlatMap {
                inner: Some(inner), ..
            } => Some(inner.clone()),
            _ => None,
        }
    };
    if let Some(inner) = flat_inner {
        let result = crate::function::call(agent, &inner.next, inner.iterator.clone(), &[])?;
        let promise = promise_resolve(agent, &promise_ctor, result)?;
        let on_fulfilled = make_await_closure(
            agent,
            AwaitEntry::FlatInner {
                object_id: obj.id(),
                is_reject: false,
            },
        )?;
        let on_rejected = make_await_closure(
            agent,
            AwaitEntry::FlatInner {
                object_id: obj.id(),
                is_reject: true,
            },
        )?;
        agent
            .async_iterator_pending
            .insert(obj.id(), capability.clone());
        perform_promise_then(agent, &promise, Some(on_fulfilled), Some(on_rejected), None)?;
        return Ok(result_promise);
    }
    let record = state
        .borrow()
        .iterator
        .clone()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no underlying iterator".into()))?;
    // chain the underlying next through the driver
    let result = crate::function::call(agent, &record.next, record.iterator.clone(), &[]);
    let value = match result {
        Ok(value) => value,
        Err(error) => {
            let rejection = crate::promise::error_value(agent, &error);
            crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
            return Ok(capability.promise);
        }
    };
    let promise = promise_resolve(agent, &promise_ctor, value)?;
    let on_fulfilled = make_await_closure(
        agent,
        AwaitEntry::Lazy {
            object_id: obj.id(),
            is_reject: false,
        },
    )?;
    let on_rejected = make_await_closure(
        agent,
        AwaitEntry::Lazy {
            object_id: obj.id(),
            is_reject: true,
        },
    )?;
    // The capability travels in a per-helper slot so the continuation can
    // resolve the right promise even across re-drives.
    agent
        .async_iterator_pending
        .insert(obj.id(), capability.clone());
    perform_promise_then(agent, &promise, Some(on_fulfilled), Some(on_rejected), None)?;
    Ok(result_promise)
}

/// Continue a lazy helper after the underlying result arrived.
fn continue_lazy(agent: &mut Agent, object_id: u64, result: Value) -> Result<Value, JsError> {
    if !matches!(result, Value::Object(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator result is not an object".into(),
        ));
    }
    let done =
        crate::context::get_property(agent, &result, &JsString::from_utf8("done"), result.clone())?;
    let value = crate::context::get_property(
        agent,
        &result,
        &JsString::from_utf8("value"),
        result.clone(),
    )?;
    let capability = agent
        .async_iterator_pending
        .get(&object_id)
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no pending capability".into()))?;
    if to_boolean(&done) {
        let state = agent
            .async_iterator_helpers
            .get(&object_id)
            .cloned()
            .ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "not an async iterator helper".into())
            })?;
        state.borrow_mut().done = true;
        crate::function::call(
            agent,
            &capability.resolve,
            Value::Undefined,
            &[iterator_result(agent, Value::Undefined, true)?],
        )?;
        return Ok(Value::Undefined);
    }
    let (mode, record) = {
        let state = agent
            .async_iterator_helpers
            .get(&object_id)
            .cloned()
            .ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "not an async iterator helper".into())
            })?;
        let state = state.borrow();
        let mode = match &state.mode {
            HelperMode::Map { mapper } => HelperMode::Map {
                mapper: mapper.clone(),
            },
            HelperMode::Filter { filterer } => HelperMode::Filter {
                filterer: filterer.clone(),
            },
            HelperMode::Take { remaining } => HelperMode::Take {
                remaining: *remaining,
            },
            HelperMode::Drop { remaining } => HelperMode::Drop {
                remaining: *remaining,
            },
            HelperMode::FlatMap { mapper, inner } => HelperMode::FlatMap {
                mapper: mapper.clone(),
                inner: inner.clone(),
            },
        };
        (Some(mode), state.iterator.clone())
    };
    let mut mode =
        mode.ok_or_else(|| JsError::new(ErrorKind::TypeError, "no helper mode".into()))?;
    let record = record
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no underlying iterator".into()))?;
    match &mut mode {
        HelperMode::Map { mapper } => {
            // The mapper result must be awaited (spec 27.1.4.3).
            let mapped = crate::function::call(agent, mapper, Value::Undefined, &[value])?;
            let promise_ctor = agent
                .current_realm()?
                .intrinsics
                .get("%Promise%")
                .unwrap_or(Value::Undefined);
            let promise = promise_resolve(agent, &promise_ctor, mapped)?;
            let on_fulfilled = make_await_closure(
                agent,
                AwaitEntry::Mapped {
                    object_id,
                    is_reject: false,
                },
            )?;
            let on_rejected = make_await_closure(
                agent,
                AwaitEntry::Mapped {
                    object_id,
                    is_reject: true,
                },
            )?;
            perform_promise_then(agent, &promise, Some(on_fulfilled), Some(on_rejected), None)?;
        }
        HelperMode::Filter { filterer } => {
            // The predicate result must be awaited (spec 27.1.4.4).
            let keep = crate::function::call(
                agent,
                filterer,
                Value::Undefined,
                std::slice::from_ref(&value),
            )?;
            let promise_ctor = agent
                .current_realm()?
                .intrinsics
                .get("%Promise%")
                .unwrap_or(Value::Undefined);
            let promise = promise_resolve(agent, &promise_ctor, keep)?;
            let on_fulfilled = make_await_closure(
                agent,
                AwaitEntry::FilterKeep {
                    object_id,
                    value: value.clone(),
                    is_reject: false,
                },
            )?;
            let on_rejected = make_await_closure(
                agent,
                AwaitEntry::FilterKeep {
                    object_id,
                    value: value.clone(),
                    is_reject: true,
                },
            )?;
            perform_promise_then(agent, &promise, Some(on_fulfilled), Some(on_rejected), None)?;
        }
        HelperMode::Take { remaining } => {
            if *remaining <= 0.0 {
                let state = agent
                    .async_iterator_helpers
                    .get(&object_id)
                    .cloned()
                    .ok_or_else(|| {
                        JsError::new(ErrorKind::TypeError, "not an async iterator helper".into())
                    })?;
                state.borrow_mut().done = true;
                crate::function::call(
                    agent,
                    &capability.resolve,
                    Value::Undefined,
                    &[iterator_result(agent, Value::Undefined, true)?],
                )?;
            } else {
                *remaining -= 1.0;
                crate::function::call(
                    agent,
                    &capability.resolve,
                    Value::Undefined,
                    &[iterator_result(agent, value, false)?],
                )?;
            }
        }
        HelperMode::Drop { remaining } => {
            if *remaining > 0.0 {
                *remaining -= 1.0;
                let result =
                    crate::function::call(agent, &record.next, record.iterator.clone(), &[])?;
                let promise_ctor = agent
                    .current_realm()?
                    .intrinsics
                    .get("%Promise%")
                    .unwrap_or(Value::Undefined);
                let promise = promise_resolve(agent, &promise_ctor, result)?;
                let on_fulfilled = make_await_closure(
                    agent,
                    AwaitEntry::Lazy {
                        object_id,
                        is_reject: false,
                    },
                )?;
                let on_rejected = make_await_closure(
                    agent,
                    AwaitEntry::Lazy {
                        object_id,
                        is_reject: true,
                    },
                )?;
                perform_promise_then(agent, &promise, Some(on_fulfilled), Some(on_rejected), None)?;
            } else {
                crate::function::call(
                    agent,
                    &capability.resolve,
                    Value::Undefined,
                    &[iterator_result(agent, value, false)?],
                )?;
            }
        }
        HelperMode::FlatMap { mapper, inner } => match inner {
            // An inner iterator is already active; step it.
            Some(inner) => step_flat_inner(agent, object_id, inner)?,
            // No inner yet: map the outer value and start its iterator.
            None => {
                let mapped = crate::function::call(agent, mapper, Value::Undefined, &[value])?;
                let iter_method = get_method(agent, &mapped, "@@asyncIterator")?;
                match iter_method {
                    Some(method) => {
                        let inner_value =
                            crate::function::call(agent, &method, mapped.clone(), &[])?;
                        if !matches!(inner_value, Value::Object(_)) {
                            return Err(JsError::new(
                                ErrorKind::TypeError,
                                "flatMap inner async iterator must be an object".into(),
                            ));
                        }
                        let next = crate::context::get_property(
                            agent,
                            &inner_value,
                            &JsString::from_utf8("next"),
                            inner_value.clone(),
                        )?;
                        if !is_callable(&next) {
                            return Err(JsError::new(
                                ErrorKind::TypeError,
                                "flatMap inner async iterator has no callable next".into(),
                            ));
                        }
                        *inner = Some(AsyncIteratorRecord {
                            iterator: inner_value,
                            next,
                        });
                        step_flat_inner(agent, object_id, inner.as_mut().expect("set"))?;
                    }
                    None => {
                        // Fall back to a sync iterator wrapped as async-from-sync.
                        let sync = crate::expr::get_iterator(agent, &mapped)?;
                        let object = crate::async_await::async_from_sync_iterator(agent, &sync)?;
                        let next = crate::context::get_property(
                            agent,
                            &Value::Object(object.clone()),
                            &JsString::from_utf8("next"),
                            Value::Object(object.clone()),
                        )?;
                        *inner = Some(AsyncIteratorRecord {
                            iterator: Value::Object(object),
                            next,
                        });
                        step_flat_inner(agent, object_id, inner.as_mut().expect("set"))?;
                    }
                }
            }
        },
    }
    // Persist mode mutations (take/drop counts, the flatMap inner iterator)
    // so later continuations see them.
    let state = agent
        .async_iterator_helpers
        .get(&object_id)
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not an async iterator helper".into()))?;
    state.borrow_mut().mode = mode;
    Ok(Value::Undefined)
}

/// Step the current flatMap inner iterator; when it is done, re-drive the
/// outer iterator.
fn step_flat_inner(
    agent: &mut Agent,
    object_id: u64,
    inner: &mut AsyncIteratorRecord,
) -> Result<(), JsError> {
    let result = crate::function::call(agent, &inner.next, inner.iterator.clone(), &[])?;
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let promise = promise_resolve(agent, &promise_ctor, result)?;
    let on_fulfilled = make_await_closure(
        agent,
        AwaitEntry::FlatInner {
            object_id,
            is_reject: false,
        },
    )?;
    let on_rejected = make_await_closure(
        agent,
        AwaitEntry::FlatInner {
            object_id,
            is_reject: true,
        },
    )?;
    perform_promise_then(agent, &promise, Some(on_fulfilled), Some(on_rejected), None)?;
    Ok(())
}

// ---- the lazy helper entry points ----

fn create_helper(agent: &mut Agent, state: HelperState) -> Result<Value, JsError> {
    let proto = agent
        .current_realm()?
        .intrinsics
        .get(ASYNC_ITERATOR_HELPER_PROTO)
        .and_then(|value| as_object(&value));
    let object = JsObject::ordinary_object_create(proto);
    agent
        .async_iterator_helpers
        .insert(object.id(), Rc::new(RefCell::new(state)));
    Ok(Value::Object(object))
}

fn map_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let mapper = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&mapper) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "AsyncIterator.prototype.map requires a callable mapper".into(),
        ));
    }
    let record = get_async_iterator_direct(agent, this)?;
    create_helper(
        agent,
        HelperState {
            iterator: Some(record),
            done: false,
            mode: HelperMode::Map { mapper },
        },
    )
}

fn filter_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let filterer = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&filterer) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "AsyncIterator.prototype.filter requires a callable filterer".into(),
        ));
    }
    let record = get_async_iterator_direct(agent, this)?;
    create_helper(
        agent,
        HelperState {
            iterator: Some(record),
            done: false,
            mode: HelperMode::Filter { filterer },
        },
    )
}

fn take_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let limit_arg = args.first().cloned().unwrap_or(Value::Undefined);
    let limit = crux::convert::to_integer_or_infinity(crux::convert::to_number(&limit_arg)?);
    if limit < 0.0 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "AsyncIterator.prototype.take requires a non-negative limit".into(),
        ));
    }
    let record = get_async_iterator_direct(agent, this)?;
    create_helper(
        agent,
        HelperState {
            iterator: Some(record),
            done: false,
            mode: HelperMode::Take { remaining: limit },
        },
    )
}

fn drop_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let limit_arg = args.first().cloned().unwrap_or(Value::Undefined);
    let limit = crux::convert::to_integer_or_infinity(crux::convert::to_number(&limit_arg)?);
    if limit < 0.0 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "AsyncIterator.prototype.drop requires a non-negative limit".into(),
        ));
    }
    let record = get_async_iterator_direct(agent, this)?;
    create_helper(
        agent,
        HelperState {
            iterator: Some(record),
            done: false,
            mode: HelperMode::Drop { remaining: limit },
        },
    )
}

fn flat_map_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let mapper = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&mapper) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "AsyncIterator.prototype.flatMap requires a callable mapper".into(),
        ));
    }
    let record = get_async_iterator_direct(agent, this)?;
    create_helper(
        agent,
        HelperState {
            iterator: Some(record),
            done: false,
            mode: HelperMode::FlatMap {
                mapper,
                inner: None,
            },
        },
    )
}

// ---- the eager helpers ----

fn start_eager(agent: &mut Agent, this: &Value, mode: EagerMode) -> Result<Value, JsError> {
    let record = get_async_iterator_direct(agent, this)?;
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let capability = new_promise_capability(agent, &promise_ctor)?;
    let promise = capability.promise.clone();
    let driver = JsObject::ordinary_object_create(None);
    agent.async_iterator_eager.insert(
        driver.id(),
        Rc::new(RefCell::new(EagerState {
            record,
            mode,
            capability: capability.clone(),
        })),
    );
    eager_step(agent, driver.id())?;
    Ok(promise)
}

/// Drive one step of an eager helper.
fn eager_step(agent: &mut Agent, driver_id: u64) -> Result<Value, JsError> {
    let (record, capability) = {
        let driver = agent
            .async_iterator_eager
            .get(&driver_id)
            .cloned()
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no eager driver".into()))?;
        let driver = driver.borrow();
        (driver.record.clone(), driver.capability.clone())
    };
    let result = crate::function::call(agent, &record.next, record.iterator.clone(), &[]);
    let value = match result {
        Ok(value) => value,
        Err(error) => {
            let rejection = crate::promise::error_value(agent, &error);
            crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
            return Ok(Value::Undefined);
        }
    };
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let promise = promise_resolve(agent, &promise_ctor, value)?;
    let on_fulfilled = make_await_closure(
        agent,
        AwaitEntry::Eager {
            driver_id,
            is_reject: false,
        },
    )?;
    let on_rejected = make_await_closure(
        agent,
        AwaitEntry::Eager {
            driver_id,
            is_reject: true,
        },
    )?;
    perform_promise_then(agent, &promise, Some(on_fulfilled), Some(on_rejected), None)?;
    Ok(Value::Undefined)
}

/// Continue an eager helper after the underlying result arrived.
fn continue_eager(agent: &mut Agent, driver_id: u64, result: Value) -> Result<Value, JsError> {
    if !matches!(result, Value::Object(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator result is not an object".into(),
        ));
    }
    let done =
        crate::context::get_property(agent, &result, &JsString::from_utf8("done"), result.clone())?;
    let value = crate::context::get_property(
        agent,
        &result,
        &JsString::from_utf8("value"),
        result.clone(),
    )?;
    let (mut mode, capability) = {
        let driver = agent
            .async_iterator_eager
            .get(&driver_id)
            .cloned()
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no eager driver".into()))?;
        let driver = driver.borrow();
        let mode = match &driver.mode {
            EagerMode::Reduce {
                reducer,
                accumulator,
                started,
            } => EagerMode::Reduce {
                reducer: reducer.clone(),
                accumulator: accumulator.clone(),
                started: *started,
            },
            EagerMode::ToArray { values } => EagerMode::ToArray {
                values: values.clone(),
            },
            EagerMode::ForEach { f } => EagerMode::ForEach { f: f.clone() },
            EagerMode::Some { f } => EagerMode::Some { f: f.clone() },
            EagerMode::Every { f } => EagerMode::Every { f: f.clone() },
            EagerMode::Find { f } => EagerMode::Find { f: f.clone() },
        };
        (mode, driver.capability.clone())
    };
    let finish = |agent: &mut Agent, value: Value| -> Result<(), JsError> {
        crate::function::call(agent, &capability.resolve, Value::Undefined, &[value])?;
        Ok(())
    };
    let done_flag = to_boolean(&done);
    match &mut mode {
        EagerMode::Reduce {
            reducer,
            accumulator,
            started,
        } => {
            if done_flag {
                let result = accumulator.clone().unwrap_or(Value::Undefined);
                finish(agent, result)?;
            } else if !*started {
                *started = true;
                *accumulator = Some(value);
                store_eager(
                    agent,
                    driver_id,
                    EagerMode::Reduce {
                        reducer: reducer.clone(),
                        accumulator: accumulator.clone(),
                        started: *started,
                    },
                )?;
                eager_step(agent, driver_id)?;
            } else {
                let acc = accumulator.clone().unwrap_or(Value::Undefined);
                let next = crate::function::call(agent, reducer, Value::Undefined, &[acc, value])?;
                *accumulator = Some(next);
                store_eager(
                    agent,
                    driver_id,
                    EagerMode::Reduce {
                        reducer: reducer.clone(),
                        accumulator: accumulator.clone(),
                        started: *started,
                    },
                )?;
                eager_step(agent, driver_id)?;
            }
        }
        EagerMode::ToArray { values } => {
            if done_flag {
                let array = crate::builtins::array::array_from_values(agent, values)?;
                finish(agent, array)?;
            } else {
                values.push(value);
                store_eager(
                    agent,
                    driver_id,
                    EagerMode::ToArray {
                        values: values.clone(),
                    },
                )?;
                eager_step(agent, driver_id)?;
            }
        }
        EagerMode::ForEach { f } => {
            if done_flag {
                finish(agent, Value::Undefined)?;
            } else {
                crate::function::call(agent, f, Value::Undefined, &[value])?;
                eager_step(agent, driver_id)?;
            }
        }
        EagerMode::Some { f } => {
            if done_flag {
                finish(agent, Value::Boolean(false))?;
            } else if to_boolean(&crate::function::call(
                agent,
                f,
                Value::Undefined,
                &[value],
            )?) {
                finish(agent, Value::Boolean(true))?;
            } else {
                eager_step(agent, driver_id)?;
            }
        }
        EagerMode::Every { f } => {
            if done_flag {
                finish(agent, Value::Boolean(true))?;
            } else if !to_boolean(&crate::function::call(
                agent,
                f,
                Value::Undefined,
                &[value],
            )?) {
                finish(agent, Value::Boolean(false))?;
            } else {
                eager_step(agent, driver_id)?;
            }
        }
        EagerMode::Find { f } => {
            if done_flag {
                finish(agent, Value::Undefined)?;
            } else if to_boolean(&crate::function::call(
                agent,
                f,
                Value::Undefined,
                std::slice::from_ref(&value),
            )?) {
                finish(agent, value)?;
            } else {
                eager_step(agent, driver_id)?;
            }
        }
    }
    Ok(Value::Undefined)
}

fn store_eager(agent: &mut Agent, driver_id: u64, mode: EagerMode) -> Result<(), JsError> {
    let driver = agent
        .async_iterator_eager
        .get(&driver_id)
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no eager driver".into()))?;
    driver.borrow_mut().mode = mode;
    Ok(())
}

fn reduce_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let reducer = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&reducer) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "AsyncIterator.prototype.reduce requires a callable reducer".into(),
        ));
    }
    start_eager(
        agent,
        this,
        EagerMode::Reduce {
            reducer,
            accumulator: args.get(1).cloned(),
            started: args.get(1).is_some(),
        },
    )
}

fn to_array_method(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    start_eager(agent, this, EagerMode::ToArray { values: Vec::new() })
}

fn for_each_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let f = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&f) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "AsyncIterator.prototype.forEach requires a callable function".into(),
        ));
    }
    start_eager(agent, this, EagerMode::ForEach { f })
}

fn some_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let f = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&f) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "AsyncIterator.prototype.some requires a callable predicate".into(),
        ));
    }
    start_eager(agent, this, EagerMode::Some { f })
}

fn every_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let f = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&f) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "AsyncIterator.prototype.every requires a callable predicate".into(),
        ));
    }
    start_eager(agent, this, EagerMode::Every { f })
}

fn find_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let f = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&f) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "AsyncIterator.prototype.find requires a callable predicate".into(),
        ));
    }
    start_eager(agent, this, EagerMode::Find { f })
}

fn iterator_result(agent: &Agent, value: Value, done: bool) -> Result<Value, JsError> {
    let object_proto = agent
        .current_realm()
        .ok()
        .and_then(|realm| realm.intrinsics.get("%Object.prototype%"))
        .and_then(|value| as_object(&value));
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

    /// An async generator yielding the values 1..n.
    fn ag(n: u32) -> &'static str {
        match n {
            3 => "(async function* () { yield 1; yield 2; yield 3; })()",
            _ => "(async function* () { yield 1; yield 2; })()",
        }
    }

    #[test]
    fn map_projects_each_value() {
        assert_eq!(
            settle(&format!(
                "(async function () {{ const it = {0}.map(x => x * 10); return JSON.stringify(await it.toArray()); }})()",
                ag(3)
            ))
            .unwrap(),
            str("[10,20,30]")
        );
    }

    #[test]
    fn map_awaits_mapper_results() {
        assert_eq!(
            settle(&format!(
                "(async function () {{ const it = {0}.map(async x => x * 2); return JSON.stringify(await it.toArray()); }})()",
                ag(2)
            ))
            .unwrap(),
            str("[2,4]")
        );
    }

    #[test]
    fn filter_keeps_matching_values() {
        assert_eq!(
            settle(&format!(
                "(async function () {{ const it = {0}.filter(x => x % 2 === 1); return JSON.stringify(await it.toArray()); }})()",
                ag(3)
            ))
            .unwrap(),
            str("[1,3]")
        );
    }

    #[test]
    fn take_limits_values() {
        assert_eq!(
            settle(&format!(
                "(async function () {{ const it = {0}.take(2); return JSON.stringify(await it.toArray()); }})()",
                ag(3)
            ))
            .unwrap(),
            str("[1,2]")
        );
    }

    #[test]
    fn drop_skips_values() {
        assert_eq!(
            settle(&format!(
                "(async function () {{ const it = {0}.drop(1); return JSON.stringify(await it.toArray()); }})()",
                ag(3)
            ))
            .unwrap(),
            str("[2,3]")
        );
    }

    #[test]
    fn flat_map_flattens_inner_async_iterables() {
        assert_eq!(
            settle(&format!(
                "(async function () {{ const it = {0}.flatMap(x => (async function* () {{ yield x; yield x * 2; }})()); return JSON.stringify(await it.toArray()); }})()",
                ag(2)
            ))
            .unwrap(),
            str("[1,2,2,4]")
        );
    }

    #[test]
    fn reduce_accumulates_with_initial_value() {
        assert_eq!(
            settle(&format!(
                "(async function () {{ const it = {0}; return await it.reduce((acc, x) => acc + x, 0); }})()",
                ag(3)
            ))
            .unwrap(),
            Value::Number(6.0)
        );
    }

    #[test]
    fn for_each_runs_for_every_value() {
        assert_eq!(
            settle(&format!(
                "(async function () {{ const seen = []; const it = {0}; await it.forEach(x => seen.push(x)); return JSON.stringify(seen); }})()",
                ag(2)
            ))
            .unwrap(),
            str("[1,2]")
        );
    }

    #[test]
    fn some_and_every_and_find() {
        assert_eq!(
            settle(&format!(
                "(async function () {{ const a = await {0}.some(x => x > 2); const b = await {0}.every(x => x > 0); const c = await {0}.find(x => x === 2); return JSON.stringify([a, b, c]); }})()",
                ag(3)
            ))
            .unwrap(),
            str("[true,true,2]")
        );
    }

    #[test]
    fn helper_is_an_async_iterator() {
        assert_eq!(
            settle(&format!(
                "(async function () {{ const it = {0}.map(x => x); const first = await it.next(); const second = await it.next(); return JSON.stringify([first.value, first.done, second.value, second.done]); }})()",
                ag(2)
            ))
            .unwrap(),
            str("[1,false,2,false]")
        );
    }

    #[test]
    fn to_array_on_plain_async_iterable() {
        assert_eq!(
            settle(concat!(
                "(async function () {",
                "  const it = { *[Symbol.iterator]() { yield 'a'; yield 'b'; } };",
                "  return JSON.stringify(await it[Symbol.iterator]().toAsync().toArray());",
                "})()"
            ))
            .unwrap(),
            str("[\"a\",\"b\"]")
        );
    }

    #[test]
    fn lazy_helper_is_lazy() {
        assert_eq!(
            settle(&format!(
                "(async function () {{ let calls = 0; const it = {0}.map(x => {{ calls++; return x; }}); const a = await it.next(); const b = await it.next(); return JSON.stringify([calls, a.value, b.value]); }})()",
                ag(3)
            ))
            .unwrap(),
            str("[2,1,2]")
        );
    }

    #[test]
    fn async_dispose_closes_the_iterator_and_runs_finally() {
        assert_eq!(
            settle(
                "(async function () { const it = (async function* () { try { yield 1; yield 2; } finally { globalThis.closed = true; } })(); await it.next(); await it[Symbol.asyncDispose](); const r = await it.next(); return JSON.stringify([globalThis.closed, r.done]); })()"
            )
            .unwrap(),
            str("[true,true]")
        );
    }

    #[test]
    fn async_dispose_resolves_with_the_return_result() {
        assert_eq!(
            settle(
                "(async function () { const it = (async function* () { yield 1; })(); const result = await it[Symbol.asyncDispose](); return result.done === true && 'value' in result; })()"
            )
            .unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn async_dispose_without_return_method_resolves_undefined() {
        assert_eq!(
            settle(
                "(async function () { const proto = Object.getPrototypeOf(Object.getPrototypeOf((async function* () {}).prototype)); const it = Object.create(proto); return await it[Symbol.asyncDispose](); })()"
            )
            .unwrap(),
            Value::Undefined
        );
    }

    #[test]
    fn async_dispose_rejects_when_return_rejects() {
        assert_eq!(
            settle(
                "(async function () { const it = (async function* () { try { yield 1; } finally { throw new Error('boom'); } })(); await it.next(); try { await it[Symbol.asyncDispose](); return 'no-throw'; } catch (e) { return 'threw'; } })()"
            )
            .unwrap(),
            str("threw")
        );
    }
}
