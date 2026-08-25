//! The Error family (spec 20.5): `%Error%`, the six native error
//! constructors (`TypeError`, `RangeError`, ...), `AggregateError`, and
//! `SuppressedError`, sharing one constructor machinery (`%NativeError%`),
//! plus `[[ErrorData]]` tracking and V8-style stack capture. Bodies are
//! placeholders; `runtime::function::call`/`construct` dispatch by intrinsic
//! identity (the %eval% pattern).

use crux::convert::{to_length, to_number, to_string};
use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, ValueKind, is_constructor};

use crate::agent::Agent;
use crate::context::as_object;
use crate::realm::Realm;

const ERROR: &str = "%Error%";
const ERROR_PROTO: &str = "%Error.prototype%";
const ERROR_TO_STRING: &str = "%Error.prototype.toString%";
const ERROR_IS_ERROR: &str = "%Error.isError%";
const EVAL_ERROR: &str = "%EvalError%";
const RANGE_ERROR: &str = "%RangeError%";
const REFERENCE_ERROR: &str = "%ReferenceError%";
const SYNTAX_ERROR: &str = "%SyntaxError%";
const TYPE_ERROR: &str = "%TypeError%";
const URI_ERROR: &str = "%URIError%";
const AGGREGATE_ERROR: &str = "%AggregateError%";
const SUPPRESSED_ERROR: &str = "%SuppressedError%";
const GET_STACK: &str = "%get Error.prototype.stack%";
const SET_STACK: &str = "%set Error.prototype.stack%";

/// (constructor intrinsic key, prototype name, has an `errors` list arg,
/// is the SuppressedError shape).
const ERROR_CTORS: &[(&str, &str, bool, bool)] = &[
    (ERROR, "Error", false, false),
    (EVAL_ERROR, "EvalError", false, false),
    (RANGE_ERROR, "RangeError", false, false),
    (REFERENCE_ERROR, "ReferenceError", false, false),
    (SYNTAX_ERROR, "SyntaxError", false, false),
    (TYPE_ERROR, "TypeError", false, false),
    (URI_ERROR, "URIError", false, false),
    (AGGREGATE_ERROR, "AggregateError", true, false),
    (SUPPRESSED_ERROR, "SuppressedError", false, true),
];

fn placeholder(name: &'static str) -> NativeFn {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

fn kind_name(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::EvalError => "EvalError",
        ErrorKind::RangeError => "RangeError",
        ErrorKind::ReferenceError => "ReferenceError",
        ErrorKind::SyntaxError => "SyntaxError",
        ErrorKind::TypeError => "TypeError",
        ErrorKind::UriError => "URIError",
    }
}

/// The intrinsic key of the constructor for an engine error kind.
fn ctor_key(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::EvalError => EVAL_ERROR,
        ErrorKind::RangeError => RANGE_ERROR,
        ErrorKind::ReferenceError => REFERENCE_ERROR,
        ErrorKind::SyntaxError => SYNTAX_ERROR,
        ErrorKind::TypeError => TYPE_ERROR,
        ErrorKind::UriError => URI_ERROR,
    }
}

/// Install the Error family and the global bindings (spec 20.5), during
/// SetDefaultGlobalBindings.
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));

    let error_proto = JsObject::ordinary_object_create(object_proto);
    let error_proto_value = Value::Object(error_proto);

    let error_ctor = Function::create_builtin(
        Some(JsString::from_utf8("Error")),
        1,
        Box::new(placeholder("Error")),
        Some(Box::new(placeholder("Error"))),
        None,
    )?;
    let error_ctor_value = Value::Function(error_ctor);

    realm.intrinsics.define(ERROR, error_ctor_value.clone());
    realm
        .intrinsics
        .define(ERROR_PROTO, error_proto_value.clone());

    error_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(error_proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    // %Error.prototype% properties (20.5.3).
    error_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(error_ctor_value.clone()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    error_proto.define_property(
        &JsString::from_utf8("name"),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8("Error")))),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    error_proto.define_property(
        &JsString::from_utf8("message"),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8("")))),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // %Error.prototype.stack% (spec 20.5.3.4-5): an accessor served by the
    // agent-dispatched getter/setter.
    let stack_getter = Function::create_builtin(
        Some(JsString::from_utf8("get stack")),
        0,
        Box::new(placeholder("get stack")),
        None,
        None,
    )?;
    let stack_setter = Function::create_builtin(
        Some(JsString::from_utf8("set stack")),
        1,
        Box::new(placeholder("set stack")),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(GET_STACK, Value::Function(stack_getter));
    realm
        .intrinsics
        .define(SET_STACK, Value::Function(stack_setter));
    error_proto.define_property(
        &JsString::from_utf8("stack"),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: Some(Value::Function(stack_getter)),
            set: Some(Value::Function(stack_setter)),
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // %Error.prototype.toString% (20.5.3.4) and %Error.isError% (20.5.2.1).
    let to_string = Function::create_builtin(
        Some(JsString::from_utf8("toString")),
        0,
        Box::new(placeholder("Error.prototype.toString")),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(ERROR_TO_STRING, Value::Function(to_string));
    error_proto.define_property(
        &JsString::from_utf8("toString"),
        &PropertyDescriptor {
            value: Some(Value::Function(to_string)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    let is_error = Function::create_builtin(
        Some(JsString::from_utf8("isError")),
        1,
        Box::new(placeholder("Error.isError")),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(ERROR_IS_ERROR, Value::Function(is_error));
    error_ctor.define_property(
        &JsString::from_utf8("isError"),
        &PropertyDescriptor {
            value: Some(Value::Function(is_error)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // The six native error constructors plus AggregateError/SuppressedError:
    // each prototype inherits %Error.prototype% and overrides `name`, and
    // each constructor inherits the %Error% constructor (spec 20.5.6).
    let error_ctor_object = as_object(&error_ctor_value)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "%Error% is not an object".into()))?;
    for (key, name, aggregate, suppressed) in ERROR_CTORS {
        if *key == ERROR {
            continue;
        }
        // spec 20.5.1: Error.length is 1, AggregateError.length is 2,
        // SuppressedError.length is 3.
        let length = if *aggregate {
            2
        } else if *suppressed {
            3
        } else {
            1
        };
        let ctor = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            Box::new(placeholder(name)),
            Some(Box::new(placeholder(name))),
            None,
        )?;
        let ctor_value = Value::Function(ctor);
        ctor.object.set_prototype_of(Some(error_ctor_object))?;
        realm.intrinsics.define(key, ctor_value.clone());

        let proto = JsObject::ordinary_object_create(Some(error_proto));
        let proto_value = Value::Object(proto);
        realm
            .intrinsics
            .define(&format!("%{name}.prototype%"), proto_value.clone());
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
        proto.define_property(
            &JsString::from_utf8("name"),
            &PropertyDescriptor {
                value: Some(Value::String(Handle::new(JsString::from_utf8(name)))),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
        proto.define_property(
            &JsString::from_utf8("message"),
            &PropertyDescriptor {
                value: Some(Value::String(Handle::new(JsString::from_utf8("")))),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
        let _ = (aggregate, suppressed);
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
    }

    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("Error"),
        &PropertyDescriptor {
            value: Some(error_ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    for (key, name, aggregate, suppressed) in ERROR_CTORS {
        if intrinsics.get(key).as_ref() == Some(callee) {
            // The call form has no newTarget: the instance takes the
            // constructor's own prototype (spec 20.5.1.1).
            return Some(error_construct(
                agent,
                args,
                callee.clone(),
                name,
                *aggregate,
                *suppressed,
            ));
        }
    }
    if intrinsics.get(ERROR_TO_STRING).as_ref() == Some(callee) {
        return Some(error_prototype_to_string(agent, this));
    }
    if intrinsics.get(ERROR_IS_ERROR).as_ref() == Some(callee) {
        return Some(Ok(Value::Boolean(is_error(
            agent,
            args.first().cloned().unwrap_or(Value::Undefined),
        ))));
    }
    if intrinsics.get(GET_STACK).as_ref() == Some(callee) {
        return Some(stack_getter(agent, this, args));
    }
    if intrinsics.get(SET_STACK).as_ref() == Some(callee) {
        return Some(stack_setter(agent, this, args));
    }
    None
}

pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    for (key, name, aggregate, suppressed) in ERROR_CTORS {
        if intrinsics.get(key).as_ref() == Some(callee) {
            return Some(error_construct(
                agent,
                args,
                new_target.clone(),
                name,
                *aggregate,
                *suppressed,
            ));
        }
    }
    None
}

/// Error(native) constructors (spec 20.5.1.1, 20.5.6.1.1, 20.5.7.1,
/// 20.5.8.1): a fresh [[ErrorData]] object whose prototype comes from
/// GetPrototypeFromConstructor, with `message`, `cause`, `stack` (and
/// `errors`/`error`/`suppressed` for the exotic shapes).
fn error_construct(
    agent: &mut Agent,
    args: &[Value],
    new_target: Value,
    name: &str,
    aggregate: bool,
    suppressed: bool,
) -> Result<Value, JsError> {
    let proto = instance_proto(agent, &new_target, &format!("%{name}.prototype%"))?;
    let object = JsObject::ordinary_object_create(proto);
    agent.error_data.insert(object.id());

    if aggregate {
        // AggregateError(errors, message, options) (spec 20.5.7.1): the
        // message ToString and InstallErrorCause run *before* the errors
        // iteration.
        define_message(&object, args.get(1))?;
        install_cause(agent, &object, args.get(2))?;
        let errors = args.first().cloned().unwrap_or(Value::Undefined);
        let errors_value = list_to_array(agent, &errors)?;
        object.create_data_property(&JsString::from_utf8("errors"), errors_value)?;
    } else if suppressed {
        // SuppressedError(error, suppressed, message) (spec 20.5.8.1): own
        // properties are created message, then error, then suppressed
        // (order-of-args-evaluation.js checks the property order).
        let error = args.first().cloned().unwrap_or(Value::Undefined);
        let suppressed_value = args.get(1).cloned().unwrap_or(Value::Undefined);
        define_message(&object, args.get(2))?;
        object.create_data_property(&JsString::from_utf8("error"), error)?;
        object.create_data_property(&JsString::from_utf8("suppressed"), suppressed_value)?;
        install_cause(agent, &object, args.get(2))?;
    } else {
        define_message(&object, args.first())?;
        install_cause(agent, &object, args.get(1))?;
    }
    define_stack(agent, &object, name)?;
    Ok(Value::Object(object))
}

/// GetPrototypeFromConstructor (spec 10.1.14): `newTarget.prototype`, with
/// the constructor's own intrinsic default prototype as the fallback when
/// it is not an object.
fn instance_proto(
    agent: &mut Agent,
    new_target: &Value,
    default_proto_key: &str,
) -> Result<Option<Handle<JsObject>>, JsError> {
    let proto = crate::context::get_property(
        agent,
        new_target,
        &JsString::from_utf8("prototype"),
        new_target.clone(),
    )?;
    if let Some(object) = as_object(&proto) {
        return Ok(Some(object));
    }
    // spec 10.1.14 steps 3-4: a non-object `prototype` falls back to the
    // constructor's intrinsic default prototype.
    Ok(crate::context::get_function_realm(agent, new_target)?
        .intrinsics
        .get(default_proto_key)
        .and_then(|value| as_object(&value)))
}

fn define_message(object: &JsObject, message: Option<&Value>) -> Result<(), JsError> {
    let Some(message) = message else {
        return Ok(());
    };
    if matches!(message.kind(), ValueKind::Undefined) {
        return Ok(());
    }
    let text = to_string(message)?;
    object.define_property(
        &JsString::from_utf8("message"),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(text))),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// InstallErrorCause (spec 20.5.9.1): `options.cause` when options is an
/// object and the property is present (even when its value is undefined).
fn install_cause(
    agent: &mut Agent,
    object: &JsObject,
    options: Option<&Value>,
) -> Result<(), JsError> {
    let Some(options) = options else {
        return Ok(());
    };
    let Some(options_object) = as_object(options) else {
        return Ok(());
    };
    if !options_object.has_property(&JsString::from_utf8("cause"))? {
        return Ok(());
    }
    let cause = crate::context::get_property(
        agent,
        options,
        &JsString::from_utf8("cause"),
        options.clone(),
    )?;
    object.define_property(
        &JsString::from_utf8("cause"),
        &PropertyDescriptor {
            value: Some(cause),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// A V8-style stack trace captured at construction time (host-defined, spec
/// 20.5.4): the header plus the active function frames. Stored per-instance
/// and served by the `%Error.prototype.stack%` accessor (the property itself
/// is not an own data property). The header uses the already-coerced own
/// `message` property so the constructor's single ToString (spec 20.5.1.1
/// step 2a) is not repeated.
fn define_stack(agent: &mut Agent, object: &JsObject, name: &str) -> Result<(), JsError> {
    let message = object
        .get_own_property_key(&PropertyKey::from_utf8("message"))?
        .and_then(|property| match &property.kind {
            crux::object::PropertyKind::Data { value, .. } => {
                value.as_string().map(|text| text.to_string_lossy())
            }
            _ => None,
        })
        .unwrap_or_default();
    let header = if message.is_empty() {
        name.to_string()
    } else {
        format!("{name}: {message}")
    };
    let mut lines = vec![header];
    for context in agent.execution_context_stack.iter().rev() {
        let frame = context
            .function
            .as_ref()
            .and_then(|function| match function.kind() {
                ValueKind::Function(f) => f.name.clone(),
                _ => None,
            })
            .map(|name| name.to_string_lossy())
            .unwrap_or_else(|| "<anonymous>".into());
        lines.push(format!("    at {frame}"));
    }
    agent
        .error_stack
        .insert(object.id(), JsString::from_utf8(&lines.join("\n")));
    Ok(())
}

/// get %Error.prototype.stack% (spec 20.5.3.4): a TypeError for non-object
/// receivers, *undefined* for objects without [[ErrorData]], else the
/// captured stack string.
fn stack_getter(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let Some(object) = crate::context::as_object(this) else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Error.prototype.stack getter called on a non-object".into(),
        ));
    };
    if !is_error(agent, this.clone()) {
        return Ok(Value::Undefined);
    }
    let stack = agent
        .error_stack
        .get(&object.id())
        .cloned()
        .unwrap_or_else(|| JsString::from_utf8(""));
    Ok(Value::String(Handle::new(stack)))
}

/// set %Error.prototype.stack% (spec 20.5.3.5): SetterThatIgnores
/// PrototypeProperties — a TypeError for non-object receivers or non-string
/// values or when the receiver is %Error.prototype% itself; otherwise an own
/// `stack` data property is created or the existing one is set.
fn stack_setter(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let Some(object) = crate::context::as_object(this) else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Error.prototype.stack setter called on a non-object".into(),
        ));
    };
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    if !matches!(value.kind(), ValueKind::String(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Error.prototype.stack setter expects a string".into(),
        ));
    }
    let error_proto = agent
        .current_realm()?
        .intrinsics
        .get("%Error.prototype%")
        .and_then(|value| as_object(&value));
    if error_proto
        .as_ref()
        .map(|proto| Handle::ptr_eq(*proto, object))
        .unwrap_or(false)
    {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot set stack on %Error.prototype%".into(),
        ));
    }
    let name = JsString::from_utf8("stack");
    if object
        .get_own_property_key(&PropertyKey::from_utf8("stack"))?
        .is_none()
    {
        object.create_data_property_or_throw(&name, value)?;
    } else {
        object.set(&name, value, true)?;
    }
    Ok(Value::Undefined)
}

/// IterableToList for the AggregateError `errors` argument (spec 20.5.7.1):
/// iterate when the value has @@iterator, otherwise fall back to the
/// array-like copy.
fn list_to_array(agent: &mut Agent, value: &Value) -> Result<Value, JsError> {
    if !matches!(value.kind(), ValueKind::Object(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "AggregateError requires an array-like errors argument".into(),
        ));
    }
    if let Some(method) = crate::expr::get_method(agent, value, "@@iterator")? {
        let iterator = crate::function::call(agent, &method, value.clone(), &[])?;
        let next = crate::context::get_property(
            agent,
            &iterator,
            &JsString::from_utf8("next"),
            iterator.clone(),
        )?;
        if !crux::value::is_callable(&next) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Iterator's next method is not callable".into(),
            ));
        }
        let record = crate::expr::IteratorRecord { iterator, next };
        let mut values = Vec::new();
        while let Some(item) = crate::expr::iterator_step(agent, &record)? {
            values.push(item);
        }
        let array = crate::builtins::array::array_create(agent, values.len() as f64)?;
        for (index, item) in values.into_iter().enumerate() {
            array.create_data_property(&JsString::from_utf8(&index.to_string()), item)?;
        }
        return Ok(Value::Object(array));
    }
    let length_value =
        crate::context::get_property(agent, value, &JsString::from_utf8("length"), value.clone())?;
    let length = to_length(to_number(&length_value)?);
    let array = crate::builtins::array::array_create(agent, length as f64)?;
    for index in 0..length {
        let element = crate::context::get_property(
            agent,
            value,
            &JsString::from_utf8(&index.to_string()),
            value.clone(),
        )?;
        array.create_data_property(&JsString::from_utf8(&index.to_string()), element)?;
    }
    Ok(Value::Object(array))
}

/// Whether the value is an [[ErrorData]] object (spec 20.5.2.1).
pub fn is_error(agent: &Agent, value: Value) -> bool {
    match value.kind() {
        ValueKind::Object(obj) => agent.error_data.contains(&obj.id()),
        _ => false,
    }
}

/// Error.prototype.toString (spec 20.5.3.4): `name + ": " + message` with
/// the empty-string fallbacks.
fn error_prototype_to_string(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    if !matches!(this.kind(), ValueKind::Object(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Error.prototype.toString requires an object".into(),
        ));
    }
    let name =
        crate::context::get_property(agent, this, &JsString::from_utf8("name"), this.clone())?;
    let name = match name.kind() {
        ValueKind::Undefined => "Error".to_string(),
        _ => to_string(&name)?.to_string_lossy(),
    };
    let message =
        crate::context::get_property(agent, this, &JsString::from_utf8("message"), this.clone())?;
    let message = match message.kind() {
        ValueKind::Undefined => String::new(),
        _ => to_string(&message)?.to_string_lossy(),
    };
    let text = if name.is_empty() {
        message
    } else if message.is_empty() {
        name
    } else {
        format!("{name}: {message}")
    };
    Ok(Value::String(Handle::new(JsString::from_utf8(&text))))
}

/// Convert an engine `JsError` into a real Error object (spec ch. 17: every
/// thrown native error is an instance of its NativeError constructor). Falls
/// back to the message string when the Error built-ins are not installed yet.
pub fn to_throwable(agent: &mut Agent, error: &JsError) -> Result<Value, JsError> {
    if let Some(value) = &error.value {
        return Ok(value.clone());
    }
    let realm = agent.current_realm()?;
    let ctor = realm
        .intrinsics
        .get(ctor_key(error.kind))
        .unwrap_or(Value::Undefined);
    if !is_constructor(&ctor) {
        return Ok(Value::String(Handle::new(JsString::from_utf8(&format!(
            "{}: {}",
            kind_name(error.kind),
            error.message
        )))));
    }
    let message = Value::String(Handle::new(JsString::from_utf8(&error.message)));
    crate::function::construct(agent, &ctor, &[message], &ctor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::evaluate;

    fn run(source: &str) -> Result<Value, JsError> {
        evaluate(source)
    }

    fn str(value: &str) -> Value {
        Value::String(Handle::new(JsString::from_utf8(value)))
    }

    #[test]
    fn error_constructs_with_message() {
        assert_eq!(run("new Error('boom').message").unwrap(), str("boom"));
        assert_eq!(run("Error('boom').message").unwrap(), str("boom"));
        assert_eq!(run("new Error().message").unwrap(), str(""));
        assert_eq!(run("new Error('b').name").unwrap(), str("Error"));
        assert_eq!(
            run("new Error('boom').toString()").unwrap(),
            str("Error: boom")
        );
    }

    #[test]
    fn native_error_subtypes() {
        assert_eq!(run("new TypeError('bad').name").unwrap(), str("TypeError"));
        assert_eq!(run("new RangeError('r').message").unwrap(), str("r"));
        assert_eq!(
            run("new ReferenceError('x') instanceof ReferenceError").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("new TypeError('x') instanceof Error").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("new TypeError('x') instanceof RangeError").unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            run("new SyntaxError('s').toString()").unwrap(),
            str("SyntaxError: s")
        );
    }

    #[test]
    fn error_cause() {
        assert_eq!(
            run("new Error('e', { cause: 42 }).cause").unwrap(),
            Value::Number(42.0)
        );
        assert_eq!(
            run("new Error('e', { cause: 42 }) instanceof Error").unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn error_is_error() {
        assert_eq!(
            run("Error.isError(new TypeError('x'))").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(run("Error.isError({})").unwrap(), Value::Boolean(false));
        assert_eq!(run("Error.isError('nope')").unwrap(), Value::Boolean(false));
    }

    #[test]
    fn engine_errors_are_real_error_objects() {
        // A TypeError thrown by the engine catches as an instance.
        assert_eq!(
            run("try { null.x; } catch (e) { e instanceof TypeError }").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("try { null.x; } catch (e) { e instanceof RangeError }").unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            run("try { null.x; } catch (e) { e.name + ':' + (typeof e.message) }").unwrap(),
            str("TypeError:string")
        );
    }

    #[test]
    fn aggregate_error() {
        assert_eq!(
            run("new AggregateError([1, 2], 'multi').errors.length").unwrap(),
            Value::Number(2.0)
        );
        assert_eq!(
            run("new AggregateError([1, 2], 'multi').message").unwrap(),
            str("multi")
        );
        assert_eq!(
            run("new AggregateError([], 'm') instanceof Error").unwrap(),
            Value::Boolean(true)
        );
        // No errors argument throws (CreateListFromArrayLike).
        assert!(run("new AggregateError()").is_err());
    }

    #[test]
    fn stack_is_captured() {
        assert_eq!(run("typeof new Error('s').stack").unwrap(), str("string"));
        assert_eq!(
            run("new Error('s').stack.length > 0").unwrap(),
            Value::Boolean(true)
        );
    }
}
