//! The `%Boolean%` intrinsic (spec 20.3): the constructor, `%Boolean.prototype%`
//! methods, and the wrapper objects created by `new Boolean(v)`. Bodies are
//! placeholders; `runtime::function::call`/`construct` dispatch by intrinsic
//! identity (the %eval% pattern).

use crux::convert::to_boolean;
use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::PropertyDescriptor;
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use crate::agent::Agent;
use crate::context::as_object;
use crate::realm::Realm;

const BOOLEAN: &str = "%Boolean%";
const BOOLEAN_PROTO: &str = "%Boolean.prototype%";
const PROTO_TO_STRING: &str = "%Boolean.prototype.toString%";
const PROTO_VALUE_OF: &str = "%Boolean.prototype.valueOf%";

fn placeholder(name: &'static str) -> NativeFn {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

/// Install the Boolean intrinsics and the global `Boolean` binding (spec
/// 20.3.1-20.3.3), during SetDefaultGlobalBindings.
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let boolean_proto = JsObject::ordinary_object_create(object_proto);
    // spec 20.3.3: %Boolean.prototype% is a Boolean object wrapping false, so
    // `%Object.prototype.toString%` reports "[object Boolean]" and the `==`
    // coercion yields false.
    boolean_proto
        .boxed
        .set(Some(crux::object::BoxedPrimitive::Boolean(false)));
    let boolean_proto_value = Value::Object(boolean_proto);

    let boolean_ctor = Function::create_builtin(
        Some(JsString::from_utf8("Boolean")),
        1,
        Box::new(placeholder("Boolean")),
        Some(Box::new(placeholder("Boolean"))),
        None,
    )?;
    let boolean_ctor_value = Value::Function(boolean_ctor);

    realm.intrinsics.define(BOOLEAN, boolean_ctor_value);
    realm.intrinsics.define(BOOLEAN_PROTO, boolean_proto_value);

    boolean_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(boolean_proto_value),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    boolean_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(boolean_ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    for (name, length, key) in [
        ("toString", 0, PROTO_TO_STRING),
        ("valueOf", 0, PROTO_VALUE_OF),
    ] {
        let method = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            Box::new(placeholder(key)),
            None,
            None,
        )?;
        realm.intrinsics.define(key, Value::Function(method));
        boolean_proto.define_property(
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

    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("Boolean"),
        &PropertyDescriptor {
            value: Some(boolean_ctor_value),
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
    if intrinsics.get(BOOLEAN).as_ref() == Some(callee) {
        // Boolean(value) (spec 20.3.1.1): ToBoolean.
        let value = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(Ok(Value::Boolean(to_boolean(&value))));
    }
    if intrinsics.get(PROTO_TO_STRING).as_ref() == Some(callee) {
        return Some(this_boolean_value(agent, this).map(|b| {
            Value::String(Handle::new(JsString::from_utf8(if b {
                "true"
            } else {
                "false"
            })))
        }));
    }
    if intrinsics.get(PROTO_VALUE_OF).as_ref() == Some(callee) {
        return Some(this_boolean_value(agent, this).map(Value::Boolean));
    }
    None
}

pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    if agent
        .current_realm()
        .ok()
        .and_then(|realm| realm.intrinsics.get(BOOLEAN))
        .as_ref()
        == Some(callee)
    {
        return Some((|| {
            let value = args.first().cloned().unwrap_or(Value::Undefined);
            // GetPrototypeFromConstructor (spec 10.1.14): subclasses of
            // Boolean get the newTarget's prototype, not %Boolean.prototype%
            // (subclass-Boolean.js).
            let proto = crate::context::get_property(
                agent,
                new_target,
                &JsString::from_utf8("prototype"),
                *new_target,
            )?;
            let proto = match as_object(&proto) {
                Some(object) => object,
                None => {
                    // GetPrototypeFromConstructor fallback (spec 10.1.14):
                    // the newTarget's realm's %Boolean.prototype%.
                    crate::context::get_function_realm(agent, new_target)?
                        .intrinsics
                        .get(BOOLEAN_PROTO)
                        .and_then(|value| as_object(&value))
                        .ok_or_else(|| {
                            JsError::new(
                                ErrorKind::TypeError,
                                "%Boolean.prototype% is not defined".into(),
                            )
                        })?
                }
            };
            let object = JsObject::ordinary_object_create(Some(proto));
            object
                .boxed
                .set(Some(crux::object::BoxedPrimitive::Boolean(to_boolean(
                    &value,
                ))));
            agent.boolean_data.insert(object.id(), to_boolean(&value));
            Ok(Value::Object(object))
        })());
    }
    None
}

/// ThisBooleanValue (spec 20.3.3.3.1): the wrapped boolean of a Boolean
/// object, or the primitive itself. `%Boolean.prototype%` wraps *false* per
/// spec; the agent tables are populated only by `new Boolean(v)`, so the
/// prototype is special-cased here.
fn this_boolean_value(agent: &Agent, this: &Value) -> Result<bool, JsError> {
    match this.kind() {
        ValueKind::Boolean(b) => Ok(b),
        ValueKind::Object(obj) => {
            let is_prototype = agent
                .current_realm()
                .ok()
                .and_then(|realm| realm.intrinsics.get(BOOLEAN_PROTO))
                .as_ref()
                == Some(this);
            if is_prototype {
                return Ok(false);
            }
            agent.boolean_data.get(&obj.id()).copied().ok_or_else(|| {
                JsError::new(
                    ErrorKind::TypeError,
                    "Boolean.prototype.valueOf requires a Boolean object".into(),
                )
            })
        }
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "Boolean.prototype.valueOf requires a Boolean object".into(),
        )),
    }
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
    fn call_form_converts() {
        assert_eq!(run("Boolean(1)").unwrap(), Value::Boolean(true));
        assert_eq!(run("Boolean(0)").unwrap(), Value::Boolean(false));
        assert_eq!(run("Boolean('')").unwrap(), Value::Boolean(false));
        assert_eq!(run("Boolean('x')").unwrap(), Value::Boolean(true));
        assert_eq!(run("Boolean(null)").unwrap(), Value::Boolean(false));
        assert_eq!(run("Boolean({})").unwrap(), Value::Boolean(true));
    }

    #[test]
    fn construct_form_wraps() {
        assert_eq!(
            run("new Boolean(1).valueOf()").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("new Boolean(0).valueOf()").unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(run("new Boolean(1).toString()").unwrap(), str("true"));
        // A Boolean object is truthy even when it wraps false.
        assert_eq!(
            run("Boolean(new Boolean(false))").unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn primitive_and_wrapper_methods() {
        assert_eq!(run("true.toString()").unwrap(), str("true"));
        assert_eq!(run("false.valueOf()").unwrap(), Value::Boolean(false));
        assert_eq!(
            run("Boolean.prototype.valueOf.call(true)").unwrap(),
            Value::Boolean(true)
        );
        // The prototype itself wraps false.
        assert_eq!(
            run("Boolean.prototype.valueOf()").unwrap(),
            Value::Boolean(false)
        );
    }

    #[test]
    fn wrapper_is_an_object_with_prototype() {
        assert_eq!(
            run("Object.prototype.toString.call(new Boolean(1))").unwrap(),
            str("[object Boolean]")
        );
        assert_eq!(
            run("new Boolean(true) instanceof Boolean").unwrap(),
            Value::Boolean(true)
        );
    }
}
