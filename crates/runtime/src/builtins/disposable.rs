//! The `%DisposableStack%` and `%AsyncDisposableStack%` intrinsics (spec
//! 27.4.2/27.4.3): explicit resource management. Both hold a stack of
//! disposable resources with a dispose hint; `dispose`/`disposeAsync` run
//! them in reverse order, wrapping multiple failures in `SuppressedError`
//! chains.

use std::cell::RefCell;
use std::rc::Rc;

use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, is_callable};

use crate::agent::Agent;
use crate::context::as_object;
use crate::realm::Realm;

const DISPOSABLE_STACK: &str = "%DisposableStack%";
const DISPOSABLE_STACK_PROTO: &str = "%DisposableStack.prototype%";
const ASYNC_DISPOSABLE_STACK: &str = "%AsyncDisposableStack%";
const ASYNC_DISPOSABLE_STACK_PROTO: &str = "%AsyncDisposableStack.prototype%";

/// A disposable resource: the value the method is called on, the dispose
/// method, and whether the hint is ~async-dispose~.
#[derive(Debug, Clone)]
pub struct DisposableResource {
    pub value: Value,
    pub method: Value,
    pub hint: bool,
}

/// [[DisposableState]] and [[DisposeCapability]] of a stack instance.
#[derive(Debug, Default)]
pub struct DisposableStackData {
    pub disposed: bool,
    pub resources: Vec<DisposableResource>,
}

/// The driver of an in-flight `disposeAsync`.
#[derive(Debug)]
pub struct AsyncDisposalDriver {
    pub stack_id: u64,
    pub index: usize,
    /// The accumulated completion: `None` while clean, `Some(error)` once a
    /// disposal threw.
    pub completion: Option<Value>,
}

pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    install_stack(realm, "DisposableStack", false)?;
    install_stack(realm, "AsyncDisposableStack", true)?;
    Ok(())
}

/// Install one of the two stack built-ins.
fn install_stack(realm: &Handle<Realm>, name: &str, is_async: bool) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let ctor_key = if is_async {
        ASYNC_DISPOSABLE_STACK
    } else {
        DISPOSABLE_STACK
    };
    let proto_key = if is_async {
        ASYNC_DISPOSABLE_STACK_PROTO
    } else {
        DISPOSABLE_STACK_PROTO
    };
    let dispose_name = if is_async { "disposeAsync" } else { "dispose" };
    let dispose_symbol = if is_async { "asyncDispose" } else { "dispose" };
    let tag = name;

    let proto = JsObject::ordinary_object_create(object_proto.clone());
    let proto_value = Value::Object(proto.clone());
    realm.intrinsics.define(proto_key, proto_value.clone());

    let ctor = Function::create_builtin(
        Some(JsString::from_utf8(name)),
        0,
        Box::new(placeholder(name.to_string())),
        Some(Box::new(placeholder(name.to_string()))),
        None,
    )?;
    let ctor_value = Value::Function(ctor.clone());
    realm.intrinsics.define(ctor_key, ctor_value.clone());
    if let Some(function_proto) = realm
        .intrinsics
        .get("%Function.prototype%")
        .and_then(|value| as_object(&value))
    {
        ctor.object.set_prototype_of(Some(function_proto))?;
    }
    ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(ctor_value.clone()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // Methods: adopt, defer, dispose/disposeAsync, move, use, and the
    // Symbol.dispose/Symbol.asyncDispose method.
    for (method_name, length) in [
        ("adopt", 2),
        ("defer", 1),
        (dispose_name, 0),
        ("move", 0),
        ("use", 1),
    ] {
        let method = Function::create_builtin(
            Some(JsString::from_utf8(method_name)),
            length,
            Box::new(placeholder(method_name.to_string())),
            None,
            None,
        )?;
        realm.intrinsics.define(
            &format!("%{name}.prototype.{method_name}%"),
            Value::Function(method.clone()),
        );
        proto.define_property(
            &JsString::from_utf8(method_name),
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
    // The Symbol.dispose / Symbol.asyncDispose method.
    let symbol_method = Function::create_builtin(
        Some(JsString::from_utf8(&format!("[{dispose_symbol}]"))),
        0,
        Box::new(placeholder(dispose_symbol.to_string())),
        None,
        None,
    )?;
    realm.intrinsics.define(
        &format!("%{name}.prototype.@@{dispose_symbol}%"),
        Value::Function(symbol_method.clone()),
    );
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known(dispose_symbol).as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::Function(symbol_method)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // The `disposed` accessor (only on %DisposableStack.prototype% per the
    // current spec shape; AsyncDisposableStack uses `disposed` too).
    let get = Function::create_builtin(
        Some(JsString::from_utf8("get disposed")),
        0,
        Box::new(placeholder("get disposed".to_string())),
        None,
        None,
    )?;
    realm.intrinsics.define(
        &format!("%{name}.prototype.disposed-get%"),
        Value::Function(get.clone()),
    );
    proto.define_property(
        &JsString::from_utf8("disposed"),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: Some(Value::Function(get)),
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // @@toStringTag.
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor::none(Value::String(Handle::new(JsString::from_utf8(tag)))),
    )?;

    realm.global_object.define_property_or_throw(
        &JsString::from_utf8(name),
        &PropertyDescriptor {
            value: Some(ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

fn placeholder(name: String) -> crux::function::NativeFn {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

/// Install one of the two stack built-ins.
/// Route a call to a stack builtin by identity.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    for (name, is_async) in [("DisposableStack", false), ("AsyncDisposableStack", true)] {
        let (ctor_key, proto_key) = if is_async {
            (ASYNC_DISPOSABLE_STACK, ASYNC_DISPOSABLE_STACK_PROTO)
        } else {
            (DISPOSABLE_STACK, DISPOSABLE_STACK_PROTO)
        };
        if intrinsics.get(ctor_key).as_ref() == Some(callee) {
            return Some(Err(JsError::new(
                ErrorKind::TypeError,
                format!("{name} must be called with new"),
            )));
        }
        let dispose_name = if is_async { "disposeAsync" } else { "dispose" };
        let dispose_symbol = if is_async { "asyncDispose" } else { "dispose" };
        let method_key = |m: &str| format!("%{name}.prototype.{m}%");
        let proto_value = intrinsics.get(proto_key)?;
        let Value::Object(proto_obj) = &proto_value else {
            continue;
        };
        for (method_name, handler) in [
            (
                "adopt",
                adopt as fn(&mut Agent, &Value, &[Value], bool) -> Result<Value, JsError>,
            ),
            ("defer", defer),
            ("move", move_stack),
            ("use", use_value),
        ] {
            if intrinsics.get(&method_key(method_name)).as_ref() == Some(callee) {
                return Some(handler(agent, this, args, is_async));
            }
        }
        if intrinsics.get(&method_key(dispose_name)).as_ref() == Some(callee) {
            return Some(if is_async {
                dispose_async_method(agent, this)
            } else {
                dispose_method(agent, this)
            });
        }
        if intrinsics
            .get(&format!("%{name}.prototype.@@{dispose_symbol}%"))
            .as_ref()
            == Some(callee)
        {
            return Some(if is_async {
                dispose_async_method(agent, this)
            } else {
                dispose_method(agent, this)
            });
        }
        if intrinsics.get(&method_key("disposed-get")).as_ref() == Some(callee) {
            return Some(disposed_getter(agent, this));
        }
        let _ = proto_obj;
    }
    None
}

/// Route a construct: `%DisposableStack%`/`%AsyncDisposableStack%` only.
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    _args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    for (key, proto_key) in [
        (DISPOSABLE_STACK, DISPOSABLE_STACK_PROTO),
        (ASYNC_DISPOSABLE_STACK, ASYNC_DISPOSABLE_STACK_PROTO),
    ] {
        if realm.intrinsics.get(key).as_ref() == Some(callee) {
            return Some(stack_construct(agent, new_target, proto_key));
        }
    }
    None
}

fn stack_construct(
    agent: &mut Agent,
    new_target: &Value,
    proto_key: &str,
) -> Result<Value, JsError> {
    let proto = crate::context::get_property_key(
        agent,
        new_target,
        &PropertyKey::from_utf8("prototype"),
        new_target.clone(),
    )?;
    let proto = match as_object(&proto) {
        Some(handle) => handle,
        None => agent
            .current_realm()?
            .intrinsics
            .get(proto_key)
            .and_then(|value| as_object(&value))
            .ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, format!("{proto_key} is not defined"))
            })?,
    };
    let object = JsObject::ordinary_object_create(Some(proto));
    agent
        .disposable_stacks
        .insert(object.id(), RefCell::new(DisposableStackData::default()));
    Ok(Value::Object(object))
}

/// RequireInternalSlot on the stack.
fn stack_data(agent: &Agent, this: &Value) -> Result<u64, JsError> {
    let Value::Object(obj) = this else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Method called on a non-object".into(),
        ));
    };
    if !agent.disposable_stacks.contains_key(&obj.id()) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Method called on a non-stack object".into(),
        ));
    }
    Ok(obj.id())
}

fn disposed_getter(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let id = stack_data(agent, this)?;
    let disposed = agent
        .disposable_stacks
        .get(&id)
        .map(|data| data.borrow().disposed)
        .unwrap_or(false);
    Ok(Value::Boolean(disposed))
}

/// GetDisposeMethod (spec 27.4.1.1): the value's `Symbol.dispose`/`
/// Symbol.asyncDispose` method; `undefined` when absent.
fn get_dispose_method(agent: &mut Agent, value: &Value, is_async: bool) -> Result<Value, JsError> {
    let symbol = if is_async { "asyncDispose" } else { "dispose" };
    let method = crate::context::get_property_key(
        agent,
        value,
        &PropertyKey::Symbol(crux::symbol::well_known(symbol).as_ref().clone()),
        value.clone(),
    )?;
    match method {
        Value::Undefined | Value::Null => Ok(Value::Undefined),
        value if is_callable(&value) => Ok(value),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "dispose method is not callable".into(),
        )),
    }
}

fn adopt(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    is_async: bool,
) -> Result<Value, JsError> {
    let id = stack_data(agent, this)?;
    let on_dispose = args.get(1).cloned().unwrap_or(Value::Undefined);
    if !is_callable(&on_dispose) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "DisposableStack.prototype.adopt requires a callable onDispose".into(),
        ));
    }
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    add_resource(agent, id, value.clone(), on_dispose, is_async)?;
    Ok(value)
}

fn defer(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    is_async: bool,
) -> Result<Value, JsError> {
    let id = stack_data(agent, this)?;
    let on_dispose = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&on_dispose) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "DisposableStack.prototype.defer requires a callable onDispose".into(),
        ));
    }
    add_resource(agent, id, Value::Undefined, on_dispose, is_async)?;
    Ok(Value::Undefined)
}

fn use_value(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    is_async: bool,
) -> Result<Value, JsError> {
    let id = stack_data(agent, this)?;
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    if matches!(value, Value::Undefined | Value::Null) {
        return Ok(value);
    }
    if !matches!(value, Value::Object(_) | Value::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "DisposableStack.prototype.use requires an object value".into(),
        ));
    }
    let method = get_dispose_method(agent, &value, is_async)?;
    if matches!(method, Value::Undefined) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "value is not disposable".into(),
        ));
    }
    add_resource(agent, id, value.clone(), method, is_async)?;
    Ok(value)
}

/// AddDisposableResource (spec 27.4.1.2): append `{ value, method, hint }`
/// to the stack, throwing when the stack is disposed.
fn add_resource(
    agent: &mut Agent,
    id: u64,
    value: Value,
    method: Value,
    is_async: bool,
) -> Result<(), JsError> {
    let data = agent
        .disposable_stacks
        .get(&id)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not a stack".into()))?;
    let mut data = data.borrow_mut();
    if data.disposed {
        return Err(JsError::new(
            ErrorKind::ReferenceError,
            "DisposableStack is already disposed".into(),
        ));
    }
    data.resources.push(DisposableResource {
        value,
        method,
        hint: is_async,
    });
    Ok(())
}

fn move_stack(
    agent: &mut Agent,
    this: &Value,
    _args: &[Value],
    is_async: bool,
) -> Result<Value, JsError> {
    let id = stack_data(agent, this)?;
    let (proto_key, name) = if is_async {
        (ASYNC_DISPOSABLE_STACK_PROTO, "AsyncDisposableStack")
    } else {
        (DISPOSABLE_STACK_PROTO, "DisposableStack")
    };
    let resources = {
        let data = agent
            .disposable_stacks
            .get(&id)
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not a stack".into()))?;
        let mut data = data.borrow_mut();
        if data.disposed {
            return Err(JsError::new(
                ErrorKind::ReferenceError,
                format!("{name} is already disposed"),
            ));
        }
        data.disposed = true;
        std::mem::take(&mut data.resources)
    };
    let proto = agent
        .current_realm()?
        .intrinsics
        .get(proto_key)
        .and_then(|value| as_object(&value))
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, format!("{proto_key} missing")))?;
    let object = JsObject::ordinary_object_create(Some(proto));
    agent.disposable_stacks.insert(
        object.id(),
        RefCell::new(DisposableStackData {
            disposed: false,
            resources,
        }),
    );
    Ok(Value::Object(object))
}

/// DisposeResources (spec 27.4.1.3): run the resources in reverse order,
/// chaining multiple failures into SuppressedError objects.
fn dispose_resources(
    agent: &mut Agent,
    id: u64,
    sync_only: bool,
) -> Result<Option<Value>, JsError> {
    let resources = {
        let data = agent
            .disposable_stacks
            .get(&id)
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not a stack".into()))?;
        data.borrow().resources.clone()
    };
    let mut completion: Option<Value> = None;
    for resource in resources.iter().rev() {
        if resource.method == Value::Undefined {
            continue;
        }
        let result = crate::function::call(agent, &resource.method, resource.value.clone(), &[]);
        match result {
            Ok(_) => {}
            Err(error) => {
                let new_error = crate::promise::error_value(agent, &error);
                match completion.take() {
                    None => completion = Some(new_error),
                    Some(suppressed) => {
                        completion = Some(make_suppressed_error(agent, new_error, suppressed)?);
                    }
                }
            }
        }
        if sync_only && resource.hint {
            // Async-dispose hint on the sync stack is unreachable (the sync
            // stack only ever pushes sync resources).
        }
    }
    Ok(completion)
}

fn dispose_method(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let id = stack_data(agent, this)?;
    let data = agent
        .disposable_stacks
        .get(&id)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not a stack".into()))?;
    {
        let mut data = data.borrow_mut();
        if data.disposed {
            return Ok(Value::Undefined);
        }
        data.disposed = true;
    }
    let completion = dispose_resources(agent, id, true)?;
    match completion {
        Some(error) => Err(
            JsError::new(ErrorKind::TypeError, "Uncaught disposal error".into()).with_value(error),
        ),
        None => Ok(Value::Undefined),
    }
}

fn dispose_async_method(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let id = stack_data(agent, this)?;
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let capability = crate::promise::new_promise_capability(agent, &promise_ctor)?;
    let result_promise = capability.promise.clone();
    let already_disposed = {
        let data = agent
            .disposable_stacks
            .get(&id)
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not a stack".into()))?;
        let mut data = data.borrow_mut();
        let disposed = data.disposed;
        if !disposed {
            data.disposed = true;
        }
        disposed
    };
    if already_disposed {
        crate::function::call(
            agent,
            &capability.resolve,
            Value::Undefined,
            &[Value::Undefined],
        )?;
        return Ok(result_promise);
    }
    let driver = JsObject::ordinary_object_create(None);
    agent.disposable_async_drivers.insert(
        driver.id(),
        Rc::new(RefCell::new(AsyncDisposalDriver {
            stack_id: id,
            index: 0,
            completion: None,
        })),
    );
    agent
        .disposable_async_caps
        .insert(driver.id(), capability.clone());
    drive_async_disposal(agent, driver.id())
}

/// Drive one step of `disposeAsync`: dispose the next resource, awaiting
/// async results through promise continuations.
fn drive_async_disposal(agent: &mut Agent, driver_id: u64) -> Result<Value, JsError> {
    let (resources, index, completion) = {
        let driver = agent
            .disposable_async_drivers
            .get(&driver_id)
            .cloned()
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no disposal driver".into()))?;
        let driver = driver.borrow();
        let resources = agent
            .disposable_stacks
            .get(&driver.stack_id)
            .map(|data| data.borrow().resources.clone())
            .unwrap_or_default();
        (resources, driver.index, driver.completion.clone())
    };
    if index >= resources.len() {
        let capability = agent
            .disposable_async_caps
            .get(&driver_id)
            .cloned()
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no disposal capability".into()))?;
        match completion {
            Some(error) => {
                let rejection = error;
                crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
            }
            None => {
                crate::function::call(
                    agent,
                    &capability.resolve,
                    Value::Undefined,
                    &[Value::Undefined],
                )?;
            }
        }
        return Ok(Value::Undefined);
    }
    let resource = resources[index].clone();
    let result = crate::function::call(agent, &resource.method, resource.value.clone(), &[]);
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            let new_error = crate::promise::error_value(agent, &error);
            let driver = agent
                .disposable_async_drivers
                .get(&driver_id)
                .cloned()
                .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no disposal driver".into()))?;
            let mut driver = driver.borrow_mut();
            driver.index += 1;
            driver.completion = Some(match driver.completion.take() {
                None => new_error,
                Some(suppressed) => make_suppressed_error(agent, new_error, suppressed)?,
            });
            drop(driver);
            return drive_async_disposal(agent, driver_id);
        }
    };
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let promise = crate::promise::promise_resolve(agent, &promise_ctor, result)?;
    // Advance the index now; the continuation drives the next resource.
    {
        let driver = agent
            .disposable_async_drivers
            .get(&driver_id)
            .cloned()
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no disposal driver".into()))?;
        driver.borrow_mut().index += 1;
    }
    let closure = Function::create_builtin(
        Some(JsString::from_utf8("")),
        1,
        Box::new(|_, _| {
            Err(JsError::new(
                ErrorKind::TypeError,
                "disposeAsync continuation must be called through the agent".into(),
            ))
        }),
        None,
        None,
    )?;
    agent.disposable_async_cont.insert(closure.id(), driver_id);
    crate::promise::perform_promise_then(
        agent,
        &promise,
        Some(Value::Function(closure.clone())),
        Some(Value::Function(closure)),
        None,
    )?;
    Ok(Value::Undefined)
}

/// The `disposeAsync` continuation: one resource's result settled; keep
/// driving.
pub fn dispatch_continuation(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let Value::Function(function) = callee else {
        return None;
    };
    let driver_id = agent.disposable_async_cont.get(&function.id()).cloned()?;
    let _ = args;
    Some(drive_async_disposal(agent, driver_id))
}

/// Build a `SuppressedError` object with the given `error` and `suppressed`.
fn make_suppressed_error(
    agent: &mut Agent,
    error: Value,
    suppressed: Value,
) -> Result<Value, JsError> {
    let proto = agent
        .current_realm()?
        .intrinsics
        .get("%SuppressedError%")
        .and_then(|ctor| {
            crate::context::get_property(
                agent,
                &ctor,
                &JsString::from_utf8("prototype"),
                ctor.clone(),
            )
            .ok()
        })
        .and_then(|value| as_object(&value));
    let object = JsObject::ordinary_object_create(proto);
    object.create_data_property(&JsString::from_utf8("error"), error)?;
    object.create_data_property(&JsString::from_utf8("suppressed"), suppressed)?;
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
    fn dispose_runs_resources_in_reverse_order() {
        assert_eq!(
            run(concat!(
                "const stack = new DisposableStack();",
                "const order = [];",
                "stack.defer(() => order.push(3));",
                "stack.adopt({}, () => order.push(2));",
                "stack.use({ [Symbol.dispose]() { order.push(1); } });",
                "stack.dispose();",
                "JSON.stringify(order);"
            ))
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("[1,2,3]")))
        );
    }

    #[test]
    fn disposed_accessor_tracks_state() {
        assert_eq!(
            run(concat!(
                "const stack = new DisposableStack();",
                "stack.defer(() => {});",
                "const before = stack.disposed;",
                "stack.dispose();",
                "JSON.stringify([before, stack.disposed, stack.dispose()]);"
            ))
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("[false,true,null]")))
        );
    }

    #[test]
    fn move_transfers_resources_without_disposing() {
        assert_eq!(
            run(concat!(
                "const stack = new DisposableStack();",
                "const order = [];",
                "stack.defer(() => order.push(1));",
                "const moved = stack.move();",
                "moved.dispose();",
                "JSON.stringify([stack.disposed, moved.disposed, JSON.stringify(order)]);"
            ))
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("[true,true,\"[1]\"]")))
        );
    }

    #[test]
    fn multiple_errors_nest_suppressed_errors() {
        assert_eq!(
            run(concat!(
                "const stack = new DisposableStack();",
                "stack.defer(function () { throw new Error('x1'); });",
                "stack.defer(function () { throw new Error('x2'); });",
                "stack.defer(function () { throw new Error('x3'); });",
                "try { stack.dispose(); 'no-throw'; }",
                "catch (e) { JSON.stringify([e.error.message, e.suppressed.error.message, e.suppressed.suppressed.message]); }"
            ))
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("[\"x1\",\"x2\",\"x3\"]")))
        );
    }

    #[test]
    fn use_on_disposed_stack_throws_reference_error() {
        assert_eq!(
            run(concat!(
                "const stack = new DisposableStack();",
                "stack.dispose();",
                "let t = false;",
                "try { stack.use({ [Symbol.dispose]() {} }); } catch (e) { t = e instanceof ReferenceError; }",
                "t;"
            ))
            .unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn async_dispose_symbol_present() {
        assert_eq!(
            run(concat!(
                "const AsyncIteratorPrototype = Object.getPrototypeOf(Object.getPrototypeOf((async function* () {}).prototype));",
                "JSON.stringify([typeof AsyncIteratorPrototype[Symbol.asyncIterator], typeof AsyncIteratorPrototype[Symbol.asyncDispose]]);"
            ))
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("[\"function\",\"function\"]")))
        );
    }
}
