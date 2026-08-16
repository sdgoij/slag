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

/// How a dispose method is invoked (spec 27.4.1.3 `Dispose` and the
/// adopt/defer closures).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DisposalCall {
    /// `Call(method, V)`: the `use` resource's method receives the value.
    Receiver,
    /// The adopt closure: `Call(onDispose, undefined, « value »)`.
    Argument,
    /// The defer closure: `Call(onDispose, undefined)`.
    Plain,
}

/// A disposable resource: the value the method is called on, the dispose
/// method, the hint (true = async-dispose), and how the method is invoked.
#[derive(Debug, Clone)]
pub struct DisposableResource {
    pub value: Value,
    pub method: Value,
    pub hint: bool,
    pub call: DisposalCall,
}

/// [[DisposableState]] and [[DisposeCapability]] of a stack instance. The
/// `is_async` flag brands the stack so a sync method rejects an async
/// instance and vice versa (RequireInternalSlot).
#[derive(Debug)]
pub struct DisposableStackData {
    pub disposed: bool,
    pub resources: Vec<DisposableResource>,
    pub is_async: bool,
}

/// The driver of an in-flight `disposeAsync`.
#[derive(Debug)]
pub struct AsyncDisposalDriver {
    pub stack_id: u64,
    pub index: usize,
    /// The accumulated completion: `None` while clean, `Some(error)` once a
    /// disposal threw.
    pub completion: Option<Value>,
    /// Whether a null/undefined async-hint resource was seen (its dispose is
    /// a no-op call but still implies an await, spec 27.4.1.3 step 3.f).
    pub needs_await: bool,
    /// Whether any async-dispose result was actually awaited.
    pub has_awaited: bool,
    /// The single trailing `Await(undefined)` for a stack whose resources
    /// were all null/undefined (spec 27.4.1.3 step 4).
    pub final_await: bool,
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

    // Methods: adopt, defer, move, use, and the dispose/disposeAsync method
    // (also aliased as the Symbol.dispose / Symbol.asyncDispose property —
    // the same function object, spec 27.4.2.7/27.4.3.8).
    for (method_name, length) in [("adopt", 2), ("defer", 1), ("move", 0), ("use", 1)] {
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
    let dispose_method = Function::create_builtin(
        Some(JsString::from_utf8(dispose_name)),
        0,
        Box::new(placeholder(dispose_name.to_string())),
        None,
        None,
    )?;
    realm.intrinsics.define(
        &format!("%{name}.prototype.{dispose_name}%"),
        Value::Function(dispose_method.clone()),
    );
    proto.define_property(
        &JsString::from_utf8(dispose_name),
        &PropertyDescriptor {
            value: Some(Value::Function(dispose_method.clone())),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    realm.intrinsics.define(
        &format!("%{name}.prototype.@@{dispose_symbol}%"),
        Value::Function(dispose_method.clone()),
    );
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known(dispose_symbol).as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::Function(dispose_method)),
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
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8(tag)))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
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
            return Some(disposed_getter(agent, this, is_async));
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
    for (key, proto_key, is_async) in [
        (DISPOSABLE_STACK, DISPOSABLE_STACK_PROTO, false),
        (ASYNC_DISPOSABLE_STACK, ASYNC_DISPOSABLE_STACK_PROTO, true),
    ] {
        if realm.intrinsics.get(key).as_ref() == Some(callee) {
            return Some(stack_construct(agent, new_target, proto_key, is_async));
        }
    }
    None
}

fn stack_construct(
    agent: &mut Agent,
    new_target: &Value,
    proto_key: &str,
    is_async: bool,
) -> Result<Value, JsError> {
    let proto = crate::context::get_property_key(
        agent,
        new_target,
        &PropertyKey::from_utf8("prototype"),
        new_target.clone(),
    )?;
    let proto = match as_object(&proto) {
        Some(handle) => handle,
        None => crate::context::get_function_realm(agent, new_target)?
            .intrinsics
            .get(proto_key)
            .and_then(|value| as_object(&value))
            .ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, format!("{proto_key} is not defined"))
            })?,
    };
    let object = JsObject::ordinary_object_create(Some(proto));
    agent.disposable_stacks.insert(
        object.id(),
        RefCell::new(DisposableStackData {
            disposed: false,
            resources: Vec::new(),
            is_async,
        }),
    );
    Ok(Value::Object(object))
}

/// RequireInternalSlot on the stack.
fn stack_data(agent: &Agent, this: &Value, is_async: bool) -> Result<u64, JsError> {
    let Value::Object(obj) = this else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Method called on a non-object".into(),
        ));
    };
    let Some(data) = agent.disposable_stacks.get(&obj.id()) else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Method called on a non-stack object".into(),
        ));
    };
    // A sync method rejects an async stack and vice versa (RequireInternalSlot
    // checks the sync/async state slot).
    if data.borrow().is_async != is_async {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Method called on a non-stack object".into(),
        ));
    }
    Ok(obj.id())
}

fn disposed_getter(agent: &mut Agent, this: &Value, is_async: bool) -> Result<Value, JsError> {
    let id = stack_data(agent, this, is_async)?;
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
    if is_async && matches!(method, Value::Undefined | Value::Null) {
        // async-dispose falls back to the sync @@dispose method (an async
        // context can dispose sync resources, spec 27.4.1.1).
        return get_dispose_method(agent, value, false);
    }
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
    let id = stack_data(agent, this, is_async)?;
    check_not_disposed(agent, id)?;
    let on_dispose = args.get(1).cloned().unwrap_or(Value::Undefined);
    if !is_callable(&on_dispose) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "DisposableStack.prototype.adopt requires a callable onDispose".into(),
        ));
    }
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    // The adopt closure calls `onDispose(undefined, « value »)`.
    add_resource(
        agent,
        id,
        value.clone(),
        on_dispose,
        is_async,
        DisposalCall::Argument,
    )?;
    Ok(value)
}

fn defer(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    is_async: bool,
) -> Result<Value, JsError> {
    let id = stack_data(agent, this, is_async)?;
    check_not_disposed(agent, id)?;
    let on_dispose = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&on_dispose) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "DisposableStack.prototype.defer requires a callable onDispose".into(),
        ));
    }
    // The defer closure calls `onDispose(undefined)`.
    add_resource(
        agent,
        id,
        Value::Undefined,
        on_dispose,
        is_async,
        DisposalCall::Plain,
    )?;
    Ok(Value::Undefined)
}

fn use_value(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    is_async: bool,
) -> Result<Value, JsError> {
    let id = stack_data(agent, this, is_async)?;
    check_not_disposed(agent, id)?;
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    if matches!(value, Value::Undefined | Value::Null) {
        if is_async {
            // An async stack registers null/undefined as a no-method
            // async-hint resource: disposeAsync still awaits (spec 27.4.1.2
            // + 27.4.1.3 step 3.f).
            add_resource(
                agent,
                id,
                value.clone(),
                Value::Undefined,
                true,
                DisposalCall::Receiver,
            )?;
        }
        return Ok(value);
    }
    // GetDisposeMethod throws for a value with no (matching) dispose
    // method; primitives box during the property lookup and land here too.
    let method = get_dispose_method(agent, &value, is_async)?;
    if matches!(method, Value::Undefined) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "value is not disposable".into(),
        ));
    }
    add_resource(
        agent,
        id,
        value.clone(),
        method,
        is_async,
        DisposalCall::Receiver,
    )?;
    Ok(value)
}

/// Throw a ReferenceError when the stack is already disposed (spec 27.4.2.3
/// step 3 and the sibling methods).
fn check_not_disposed(agent: &Agent, id: u64) -> Result<(), JsError> {
    let data = agent
        .disposable_stacks
        .get(&id)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not a stack".into()))?;
    if data.borrow().disposed {
        return Err(JsError::new(
            ErrorKind::ReferenceError,
            "DisposableStack is already disposed".into(),
        ));
    }
    Ok(())
}

/// AddDisposableResource (spec 27.4.1.2): append `{ value, method, hint }`
/// to the stack, throwing when the stack is disposed.
fn add_resource(
    agent: &mut Agent,
    id: u64,
    value: Value,
    method: Value,
    is_async: bool,
    call: DisposalCall,
) -> Result<(), JsError> {
    check_not_disposed(agent, id)?;
    let data = agent
        .disposable_stacks
        .get(&id)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not a stack".into()))?;
    let mut data = data.borrow_mut();
    data.resources.push(DisposableResource {
        value,
        method,
        hint: is_async,
        call,
    });
    Ok(())
}

fn move_stack(
    agent: &mut Agent,
    this: &Value,
    _args: &[Value],
    is_async: bool,
) -> Result<Value, JsError> {
    let id = stack_data(agent, this, is_async)?;
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
            is_async,
        }),
    );
    Ok(Value::Object(object))
}

/// DisposeResources (spec 27.4.1.3): run the resources in reverse order,
/// chaining multiple failures into SuppressedError objects.
fn dispose_resources(agent: &mut Agent, id: u64) -> Result<Option<Value>, JsError> {
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
        let result = crate::function::call(
            agent,
            &resource.method,
            resource_receiver(resource),
            &resource_arguments(resource),
        );
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
    }
    Ok(completion)
}

/// The receiver of a resource's dispose method per its call kind (spec
/// 27.4.1.3 and the adopt/defer closures).
fn resource_receiver(resource: &DisposableResource) -> Value {
    match resource.call {
        DisposalCall::Receiver => resource.value.clone(),
        DisposalCall::Argument | DisposalCall::Plain => Value::Undefined,
    }
}

/// The argument list of a resource's dispose method per its call kind.
fn resource_arguments(resource: &DisposableResource) -> Vec<Value> {
    match resource.call {
        DisposalCall::Argument => vec![resource.value.clone()],
        DisposalCall::Receiver | DisposalCall::Plain => vec![],
    }
}

fn dispose_method(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let id = stack_data(agent, this, false)?;
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
    let completion = dispose_resources(agent, id)?;
    match completion {
        Some(error) => Err(
            JsError::new(ErrorKind::TypeError, "Uncaught disposal error".into()).with_value(error),
        ),
        None => Ok(Value::Undefined),
    }
}

fn dispose_async_method(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let capability = crate::promise::new_promise_capability(agent, &promise_ctor)?;
    let result_promise = capability.promise.clone();
    // RequireInternalSlot: a non-object or non-stack receiver rejects the
    // returned promise (spec 27.4.3.3 steps 1-2), it does not throw
    // synchronously.
    let id = match stack_data(agent, this, true) {
        Ok(id) => id,
        Err(error) => {
            let rejection = crate::promise::error_value(agent, &error);
            crate::function::call(agent, &capability.reject, Value::Undefined, &[rejection])?;
            return Ok(result_promise);
        }
    };
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
            needs_await: false,
            has_awaited: false,
            final_await: false,
        })),
    );
    agent
        .disposable_async_caps
        .insert(driver.id(), capability.clone());
    drive_async_disposal(agent, driver.id())?;
    Ok(result_promise)
}

/// Drive one step of `disposeAsync`: dispose the next resource (in reverse
/// registration order), awaiting async results through promise
/// continuations, then settle the capability.
fn drive_async_disposal(agent: &mut Agent, driver_id: u64) -> Result<Value, JsError> {
    let (resources, index, completion, needs_await, has_awaited, final_await) = {
        let driver = agent
            .disposable_async_drivers
            .get(&driver_id)
            .cloned()
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no disposal driver".into()))?;
        let driver = driver.borrow();
        let mut resources = agent
            .disposable_stacks
            .get(&driver.stack_id)
            .map(|data| data.borrow().resources.clone())
            .unwrap_or_default();
        // DisposeResources walks the stack in reverse (spec 27.4.1.3).
        resources.reverse();
        (
            resources,
            driver.index,
            driver.completion.clone(),
            driver.needs_await,
            driver.has_awaited,
            driver.final_await,
        )
    };
    if index >= resources.len() {
        if !final_await && needs_await && !has_awaited {
            // Only null/undefined resources were disposed: a lone
            // `Await(undefined)` still runs (spec 27.4.1.3 step 4), then the
            // capability settles.
            let promise_ctor = agent
                .current_realm()?
                .intrinsics
                .get("%Promise%")
                .unwrap_or(Value::Undefined);
            let promise = crate::promise::promise_resolve(agent, &promise_ctor, Value::Undefined)?;
            {
                let driver = agent
                    .disposable_async_drivers
                    .get(&driver_id)
                    .cloned()
                    .ok_or_else(|| {
                        JsError::new(ErrorKind::TypeError, "no disposal driver".into())
                    })?;
                driver.borrow_mut().final_await = true;
            }
            let (on_fulfilled, on_rejected) = make_async_disposal_continuations(agent, driver_id)?;
            crate::promise::perform_promise_then(
                agent,
                &promise,
                Some(on_fulfilled),
                Some(on_rejected),
                None,
            )?;
            return Ok(Value::Undefined);
        }
        let capability = agent
            .disposable_async_caps
            .get(&driver_id)
            .cloned()
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no disposal capability".into()))?;
        match &completion {
            Some(error) => {
                crate::function::call(
                    agent,
                    &capability.reject,
                    Value::Undefined,
                    std::slice::from_ref(error),
                )?;
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
    if matches!(resource.method, Value::Undefined) {
        // Dispose with no method: a no-op, but an async hint still implies
        // the trailing await (spec 27.4.1.3 step 3.f).
        let driver = agent
            .disposable_async_drivers
            .get(&driver_id)
            .cloned()
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no disposal driver".into()))?;
        let mut driver = driver.borrow_mut();
        driver.index += 1;
        driver.needs_await = true;
        drop(driver);
        return drive_async_disposal(agent, driver_id);
    }
    let result = crate::function::call(
        agent,
        &resource.method,
        resource_receiver(&resource),
        &resource_arguments(&resource),
    );
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
    {
        let driver = agent
            .disposable_async_drivers
            .get(&driver_id)
            .cloned()
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no disposal driver".into()))?;
        driver.borrow_mut().has_awaited = true;
    }
    let (on_fulfilled, on_rejected) = make_async_disposal_continuations(agent, driver_id)?;
    crate::promise::perform_promise_then(
        agent,
        &promise,
        Some(on_fulfilled),
        Some(on_rejected),
        None,
    )?;
    Ok(Value::Undefined)
}

/// The onFulfilled/onRejected closures of one awaited async dispose: each
/// maps to the driver with its reject flag, so the continuation folds a
/// rejected dispose as a throwing disposal.
fn make_async_disposal_continuations(
    agent: &mut Agent,
    driver_id: u64,
) -> Result<(Value, Value), JsError> {
    let mut make = |is_reject: bool| -> Result<Value, JsError> {
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
        agent
            .disposable_async_cont
            .insert(closure.id(), (driver_id, is_reject));
        Ok(Value::Function(closure))
    };
    Ok((make(false)?, make(true)?))
}

/// The `disposeAsync` continuation: one resource's result settled; fold a
/// rejection into the completion, advance, and keep driving.
pub fn dispatch_continuation(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let Value::Function(function) = callee else {
        return None;
    };
    let (driver_id, is_reject) = agent.disposable_async_cont.get(&function.id()).cloned()?;
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    let driver = agent.disposable_async_drivers.get(&driver_id).cloned()?;
    if driver.borrow().final_await {
        // The trailing `Await(undefined)` settled: settle the capability.
        let mut driver = driver.borrow_mut();
        driver.final_await = false;
        driver.has_awaited = true;
        drop(driver);
        return Some(drive_async_disposal(agent, driver_id));
    }
    {
        let mut driver = driver.borrow_mut();
        driver.index += 1;
        if is_reject {
            driver.completion = Some(match driver.completion.take() {
                None => value,
                Some(suppressed) => match make_suppressed_error(agent, value, suppressed) {
                    Ok(error) => error,
                    Err(error) => return Some(Err(error)),
                },
            });
        }
    }
    Some(drive_async_disposal(agent, driver_id))
}

/// Build a `SuppressedError` object with the given `error` and `suppressed`.
pub fn make_suppressed_error(
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

/// How an async body's completion settles after the awaited disposal of its
/// `using` resources.
#[derive(Debug, Clone)]
pub enum AsyncBodySettlement {
    /// Async function: settle the promise with the final completion.
    Function { resolve: Value, reject: Value },
    /// Async generator: complete the current request and drain the queue.
    Generator { object_id: u64 },
    /// Module body: settle the module record with the final completion.
    Module {
        module: std::rc::Rc<crate::module::SourceTextModule>,
        state: std::rc::Rc<std::cell::RefCell<crate::async_await::AsyncFunctionState>>,
    },
}

/// The driver of an in-flight awaited disposal of an async body's `using`
/// resources (spec 9.4.3 DisposeResources with async-dispose hints).
#[derive(Debug)]
pub struct AsyncBodyDisposalDriver {
    pub resources: Vec<crate::env::DisposableResource>,
    pub index: usize,
    pub completion: crate::flow::Completion,
    pub settlement: AsyncBodySettlement,
}

/// Dispose an async body's `using` resources before its promise settles: the
/// driver runs the methods in reverse order, awaiting async-dispose results
/// through promise continuations, then settles the body's completion.
pub fn dispose_async_body_resources(
    agent: &mut Agent,
    resources: Vec<crate::env::DisposableResource>,
    completion: crate::flow::Completion,
    settlement: AsyncBodySettlement,
) -> Result<(), JsError> {
    // DisposeResources runs in reverse registration order (spec 9.4.3), but
    // the driver consumes resources in order: reverse here so callers can
    // pass the drained (registration-order) stack.
    let mut resources = resources;
    resources.reverse();
    let driver = JsObject::ordinary_object_create(None);
    agent.async_body_disposal.insert(
        driver.id(),
        Rc::new(RefCell::new(AsyncBodyDisposalDriver {
            resources,
            index: 0,
            completion,
            settlement,
        })),
    );
    drive_async_body_disposal(agent, driver.id())?;
    Ok(())
}

/// Drive one step: dispose the next resource (awaiting async-dispose
/// results), or settle the body's completion when the stack is drained.
fn drive_async_body_disposal(agent: &mut Agent, driver_id: u64) -> Result<Value, JsError> {
    let (resources, index, completion, settlement) = {
        let driver = agent
            .async_body_disposal
            .get(&driver_id)
            .cloned()
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no disposal driver".into()))?;
        let driver = driver.borrow();
        (
            driver.resources.clone(),
            driver.index,
            driver.completion.clone(),
            driver.settlement.clone(),
        )
    };
    if index >= resources.len() {
        settle_async_body_completion(agent, &settlement, completion)?;
        return Ok(Value::Undefined);
    }
    let resource = resources[index].clone();
    let method_result = if matches!(resource.method, Value::Undefined) {
        if resource.hint == crate::env::DisposalHint::Sync {
            // Dispose with an undefined method and sync hint: no call, no
            // await (spec 9.4.4 steps 1-4).
            return drive_async_body_step(agent, driver_id, Ok(Value::Undefined));
        }
        // Async hint: Dispose returns undefined but still Await(undefined),
        // so the continuation runs in a later microtask (spec 9.4.4 step
        // 3.a) — `await using _ = null` implies an await.
        Ok(Value::Undefined)
    } else {
        crate::function::call(agent, &resource.method, resource.value.clone(), &[])
    };
    if resource.hint == crate::env::DisposalHint::Sync {
        let result = method_result.map_err(|error| crate::promise::error_value(agent, &error));
        return drive_async_body_step(agent, driver_id, result);
    }
    let result = match method_result {
        Ok(result) => result,
        Err(error) => {
            let new_error = crate::promise::error_value(agent, &error);
            return drive_async_body_step(agent, driver_id, Err(new_error));
        }
    };
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let promise = crate::promise::promise_resolve(agent, &promise_ctor, result)?;
    let mut make_continuation = |is_reject: bool| -> Result<Value, JsError> {
        let closure = Function::create_builtin(
            Some(JsString::from_utf8("")),
            1,
            Box::new(|_, _| {
                Err(JsError::new(
                    ErrorKind::TypeError,
                    "async disposal continuation must be called through the agent".into(),
                ))
            }),
            None,
            None,
        )?;
        agent
            .async_body_disposal_cont
            .insert(closure.id(), (driver_id, is_reject));
        Ok(Value::Function(closure))
    };
    let on_fulfilled = make_continuation(false)?;
    let on_rejected = make_continuation(true)?;
    crate::promise::perform_promise_then(
        agent,
        &promise,
        Some(on_fulfilled),
        Some(on_rejected),
        None,
    )?;
    Ok(Value::Undefined)
}

/// Fold one disposal's outcome into the carried completion and keep driving
/// (spec 9.4.3: a throwing disposal replaces a normal completion, and a
/// second throw becomes a SuppressedError).
fn drive_async_body_step(
    agent: &mut Agent,
    driver_id: u64,
    result: Result<Value, Value>,
) -> Result<Value, JsError> {
    let driver = agent
        .async_body_disposal
        .get(&driver_id)
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "no disposal driver".into()))?;
    let mut driver = driver.borrow_mut();
    driver.index += 1;
    if let Err(new_error) = result {
        driver.completion =
            match std::mem::replace(&mut driver.completion, crate::flow::Completion::Empty) {
                crate::flow::Completion::Throw(original) => crate::flow::Completion::Throw(
                    make_suppressed_error(agent, new_error, original)?,
                ),
                _ => crate::flow::Completion::Throw(new_error),
            };
    }
    drop(driver);
    drive_async_body_disposal(agent, driver_id)
}

/// The `await using` continuation: one resource's result settled; keep
/// driving.
pub fn dispatch_async_body_disposal(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let Value::Function(function) = callee else {
        return None;
    };
    let driver_id = agent
        .async_body_disposal_cont
        .get(&function.id())
        .cloned()?;
    let (driver_id, is_reject) = driver_id;
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    let result = if is_reject { Err(value) } else { Ok(value) };
    Some(drive_async_body_step(agent, driver_id, result))
}

/// Settle the async body with the carried completion (spec 9.4.3 returns the
/// completion to the caller of DisposeResources).
fn settle_async_body_completion(
    agent: &mut Agent,
    settlement: &AsyncBodySettlement,
    completion: crate::flow::Completion,
) -> Result<(), JsError> {
    match settlement {
        AsyncBodySettlement::Function { resolve, reject } => match completion {
            crate::flow::Completion::Return(value) => {
                crate::function::call(agent, resolve, Value::Undefined, &[value])?;
            }
            crate::flow::Completion::Normal(_) | crate::flow::Completion::Empty => {
                crate::function::call(agent, resolve, Value::Undefined, &[Value::Undefined])?;
            }
            crate::flow::Completion::Throw(value) => {
                crate::function::call(agent, reject, Value::Undefined, &[value])?;
            }
            crate::flow::Completion::Break { .. } | crate::flow::Completion::Continue { .. } => {
                crate::function::call(
                    agent,
                    reject,
                    Value::Undefined,
                    &[Value::String(Handle::new(JsString::from_utf8(
                        "Illegal control flow in an async body",
                    )))],
                )?;
            }
        },
        AsyncBodySettlement::Generator { object_id } => {
            crate::async_generator::complete_current_request(agent, *object_id, completion)?;
        }
        AsyncBodySettlement::Module { module, state } => {
            crate::module::finish_module_evaluation(agent, module, state, completion)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::evaluate;
    use crate::promise::PromiseState;

    fn run(source: &str) -> Result<Value, JsError> {
        evaluate(source)
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

    #[test]
    fn async_using_disposed_at_end_of_async_function_body() {
        // spec 15.8.5.2 / 9.4.3: an async body's `await using` resources are
        // disposed when the body completes, even with awaits in between.
        assert_eq!(
            settle(concat!(
                "var resource = { disposed: false, async [Symbol.asyncDispose]() { this.disposed = true; } };",
                "var release1, suspend1 = new Promise(function (resolve) { release1 = resolve; });",
                "var release2, suspend2 = new Promise(function (resolve) { release2 = resolve; });",
                "var before1, before2;",
                "async function f() {",
                "  await using _ = resource;",
                "  await suspend1;",
                "  before1 = resource.disposed;",
                "  await suspend2;",
                "  before2 = resource.disposed;",
                "}",
                "var p = f();",
                "release1();",
                "release2();",
                "p.then(function () { return JSON.stringify([before1, before2, resource.disposed]); });"
            ))
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("[false,false,true]")))
        );
    }

    #[test]
    fn await_using_null_implies_await() {
        // spec 9.4.3/9.4.4: `await using _ = null` registers an async-hint
        // resource whose undefined-method dispose still awaits at scope exit,
        // so statements after the block run in a later microtask (the
        // statements inside the block are unaffected).
        assert_eq!(
            settle(concat!(
                "var same = true, following = true, after = true;",
                "async function f() {",
                "  { await using _ = null; following = same; }",
                "  after = same;",
                "}",
                "var p = f();",
                "same = false;",
                "p.then(function () { return JSON.stringify([following, after]); });"
            ))
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("[true,false]")))
        );
    }

    #[test]
    fn async_using_disposal_error_nests_suppressed_error() {
        // spec 9.4.3: a throwing async-dispose on an already-throwing body
        // nests a SuppressedError, disposed before the catch runs.
        assert_eq!(
            settle(concat!(
                "class MyError extends Error {}",
                "var e1 = new MyError();",
                "var e2 = new MyError();",
                "var e3 = new MyError();",
                "(async function () {",
                "  try {",
                "    await using _1 = { async [Symbol.asyncDispose]() { throw e1; } };",
                "    await using _2 = { [Symbol.dispose]() { throw e2; } };",
                "    throw e3;",
                "  } catch (e) {",
                "    return [e instanceof SuppressedError, e.error === e1,",
                "      e.suppressed instanceof SuppressedError, e.suppressed.error === e2,",
                "      e.suppressed.suppressed === e3].join(',');",
                "  }",
                "})()"
            ))
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("true,true,true,true,true")))
        );
    }

    #[test]
    fn async_using_disposed_at_end_of_async_generator_body() {
        assert_eq!(
            settle(concat!(
                "var resource = { disposed: false, async [Symbol.asyncDispose]() { this.disposed = true; } };",
                "async function* g() {",
                "  await using _ = resource;",
                "  yield 1;",
                "}",
                "var it = g();",
                "it.next().then(function () { return it.next(); })",
                "  .then(function () { return resource.disposed; });"
            ))
            .unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn async_dispose_rejects_with_suppressed_error() {
        // spec 27.4.1.3: a throwing async-dispose on an already-throwing
        // disposal nests a SuppressedError; the first error rejects as-is.
        assert_eq!(
            settle(concat!(
                "class MyError extends Error {}",
                "var e1 = new MyError();",
                "var e2 = new MyError();",
                "var stack = new AsyncDisposableStack();",
                "stack.use({ async [Symbol.asyncDispose]() { throw e1; } });",
                "stack.use({ [Symbol.dispose]() { throw e2; } });",
                "stack.disposeAsync().then(",
                "  function () { return 'resolved'; },",
                "  function (e) { return [e instanceof SuppressedError, e.error === e1, e.suppressed === e2].join(','); }",
                ");"
            ))
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("true,true,true")))
        );
    }

    #[test]
    fn async_dispose_awaits_null_resource() {
        // spec 27.4.1.3 step 4: an AsyncDisposableStack holding only a
        // null/undefined resource still awaits once, so `disposeAsync`
        // resolves in a later microtask.
        assert_eq!(
            settle(concat!(
                "var sequence = [];",
                "var stack = new AsyncDisposableStack();",
                "stack.use(null);",
                "Promise.all([",
                "  Promise.resolve().then(function () { return 0; }).then(function () { sequence.push(1); }),",
                "  stack.disposeAsync().then(function () { sequence.push(2); }),",
                "  Promise.resolve().then(function () { return 0; }).then(function () { sequence.push(3); })",
                "]).then(function () { return JSON.stringify(sequence); });"
            ))
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("[1,2,3]")))
        );
    }

    #[test]
    fn async_dispose_non_object_rejects() {
        // spec 27.4.3.3: RequireInternalSlot failures reject the returned
        // promise instead of throwing synchronously.
        assert_eq!(
            settle(concat!(
                "var disposeAsync = AsyncDisposableStack.prototype.disposeAsync;",
                "disposeAsync.call(undefined).then(",
                "  function () { return 'resolved'; },",
                "  function (e) { return e instanceof TypeError; }",
                ");"
            ))
            .unwrap(),
            Value::Boolean(true)
        );
    }
}
