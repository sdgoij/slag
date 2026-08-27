//! The `%Reflect%` intrinsic (spec 28.1): thin, non-throwing wrappers over
//! the object internal methods. `Reflect.apply`/`construct`/… keep the `?`
//! semantics (errors propagate); the boolean-returning methods surface the
//! internal-method result instead of throwing on `false`.

use crux::convert::{to_length, to_number};
use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, ValueKind, is_callable, is_constructor};

use crate::agent::Agent;
use crate::context::as_object;
use crate::realm::Realm;

const REFLECT: &str = "%Reflect%";

const METHODS: &[(&str, &str, u64)] = &[
    ("apply", "apply", 3),
    ("construct", "construct", 2),
    ("defineProperty", "defineProperty", 3),
    ("deleteProperty", "deleteProperty", 2),
    ("get", "get", 2),
    ("getOwnPropertyDescriptor", "getOwnPropertyDescriptor", 2),
    ("getPrototypeOf", "getPrototypeOf", 1),
    ("has", "has", 2),
    ("isExtensible", "isExtensible", 1),
    ("ownKeys", "ownKeys", 1),
    ("preventExtensions", "preventExtensions", 1),
    ("set", "set", 3),
    ("setPrototypeOf", "setPrototypeOf", 2),
];

fn placeholder(name: &'static str) -> NativeFn {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

/// Install the Reflect intrinsics and the global `Reflect` binding (spec
/// 28.1.1): an ordinary object carrying the 13 static methods.
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let reflect = JsObject::ordinary_object_create(object_proto);
    let reflect_value = Value::Object(reflect);
    realm.intrinsics.define(REFLECT, reflect_value);
    // spec 28.1.3.2: Reflect[@@toStringTag] = "Reflect", non-writable,
    // non-enumerable, configurable.
    reflect.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag")),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8("Reflect")))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    for (name, _key, length) in METHODS {
        let method = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            *length,
            placeholder(name),
            None,
            None,
        )?;
        realm
            .intrinsics
            .define(&format!("%Reflect.{name}%"), Value::Function(method));
        reflect.define_property(
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
        &JsString::from_utf8("Reflect"),
        &PropertyDescriptor {
            value: Some(reflect_value),
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
    _this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    for (name, _key, _length) in METHODS {
        let key = format!("%Reflect.{name}%");
        if intrinsics.get(&key).as_ref() == Some(callee) {
            return Some(reflect_method(agent, name, args));
        }
    }
    None
}

/// The object half of a Reflect target: `TypeError` for primitives.
fn object_of(value: &Value) -> Result<Handle<JsObject>, JsError> {
    match value.kind() {
        ValueKind::Object(obj) => Ok(obj),
        ValueKind::Function(f) => f.object.handle().ok_or_else(|| {
            JsError::new(ErrorKind::TypeError, "Reflect target has no object".into())
        }),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "Reflect target must be an object".into(),
        )),
    }
}

/// CreateListFromArrayLike (spec 7.3.18) with the ~anything~ element kind,
/// as `Reflect.apply`/`construct` require. Only primitives are rejected: a
/// function value is an Object (its function-object part reads the
/// array-like indices).
fn list_from_array_like(agent: &mut Agent, value: &Value) -> Result<Vec<Value>, JsError> {
    if !matches!(value.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "argumentsList must be an object".into(),
        ));
    }
    let length =
        crate::context::get_property(agent, value, &JsString::from_utf8("length"), *value)?;
    let length = to_length(to_number(&length)?);
    let mut list = Vec::with_capacity(length as usize);
    for index in 0..length {
        let element = crate::context::get_property(
            agent,
            value,
            &JsString::from_utf8(&index.to_string()),
            *value,
        )?;
        list.push(element);
    }
    Ok(list)
}

fn reflect_method(agent: &mut Agent, name: &str, args: &[Value]) -> Result<Value, JsError> {
    let arg = |index: usize| args.get(index).cloned().unwrap_or(Value::Undefined);
    match name {
        "apply" => {
            let target = arg(0);
            let this_argument = arg(1);
            if !is_callable(&target) {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "Reflect.apply requires a callable target".into(),
                ));
            }
            let list = list_from_array_like(agent, &arg(2))?;
            crate::function::call(agent, &target, this_argument, &list)
        }
        "construct" => {
            let target = arg(0);
            if !is_constructor(&target) {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "Reflect.construct requires a constructor target".into(),
                ));
            }
            let new_target = match arg(2) {
                v if v.is_undefined() => target,
                other => {
                    if !is_constructor(&other) {
                        return Err(JsError::new(
                            ErrorKind::TypeError,
                            "Reflect.construct newTarget must be a constructor".into(),
                        ));
                    }
                    other
                }
            };
            let list = list_from_array_like(agent, &arg(1))?;
            crate::function::construct(agent, &target, &list, &new_target)
        }
        "defineProperty" => {
            let obj = object_of(&arg(0))?;
            let key = crate::context::to_property_key(agent, &arg(1))?;
            let desc = crux::property::to_property_descriptor(&arg(2))?;
            let status = obj.define_property_key(&key, &desc)?;
            Ok(Value::Boolean(status))
        }
        "deleteProperty" => {
            let obj = object_of(&arg(0))?;
            let key = crate::context::to_property_key(agent, &arg(1))?;
            Ok(Value::Boolean(obj.delete_key(&key)?))
        }
        "get" => {
            let obj = object_of(&arg(0))?;
            let key = crate::context::to_property_key(agent, &arg(1))?;
            let receiver = match arg(2) {
                v if v.is_undefined() => Value::Object(obj),
                other => other,
            };
            crate::context::get_property_key(agent, &Value::Object(obj), &key, receiver)
        }
        "getOwnPropertyDescriptor" => {
            let obj = object_of(&arg(0))?;
            let key = crate::context::to_property_key(agent, &arg(1))?;
            // A deferred namespace's [[GetOwnProperty]] triggers its module's
            // evaluation for non-symbol-like keys, and the descriptor reads
            // the live binding (import-defer).
            crate::module::ensure_deferred_namespace_evaluation_key(agent, &obj, &key)?;
            let Some(property) = obj.get_own_property_key(&key)? else {
                return Ok(Value::Undefined);
            };
            let desc =
                crate::builtins::object::namespace_live_descriptor(agent, &obj, &key, &property)?;
            let prototype = agent
                .current_realm()?
                .intrinsics
                .get("%Object.prototype%")
                .and_then(|v| crate::context::as_object(&v));
            crux::property::from_property_descriptor(&desc, prototype)
        }
        "getPrototypeOf" => {
            let obj = object_of(&arg(0))?;
            Ok(match obj.get_prototype_of()? {
                Some(proto) => proto
                    .function_value()
                    .unwrap_or_else(|| Value::Object(proto)),
                None => Value::Null,
            })
        }
        "has" => {
            let obj = object_of(&arg(0))?;
            let key = crate::context::to_property_key(agent, &arg(1))?;
            Ok(Value::Boolean(obj.has_property_key(&key)?))
        }
        "isExtensible" => {
            let obj = object_of(&arg(0))?;
            Ok(Value::Boolean(obj.is_extensible()?))
        }
        "ownKeys" => {
            let obj = object_of(&arg(0))?;
            // A deferred namespace's [[OwnPropertyKeys]] triggers its module's
            // evaluation (import-defer).
            crate::module::ensure_deferred_namespace_evaluation(agent, &obj)?;
            let keys = obj.own_property_keys()?;
            // GC-2: define each key on the result array as it is boxed — a
            // local `Vec<Value>` of freshly-boxed keys would sit in a heap
            // buffer the stack scan cannot see across the boxings (each
            // `Handle::new` fires a per-allocation stress collection). The
            // array handle is a stack local, so the scan roots it and its
            // property vector keeps the defined elements.
            let array = crate::builtins::array::array_create(agent, keys.len() as f64)?;
            for (index, key) in keys.into_iter().enumerate() {
                let value = match key {
                    PropertyKey::String(id) => Value::String(Handle::new(crux::lookup(id))),
                    PropertyKey::Symbol(symbol) => Value::Symbol(symbol),
                };
                array.create_data_property(
                    &crux::string::JsString::from_utf8(&index.to_string()),
                    value,
                )?;
            }
            Ok(Value::Object(array))
        }
        "preventExtensions" => {
            let obj = object_of(&arg(0))?;
            Ok(Value::Boolean(obj.prevent_extensions()?))
        }
        "set" => {
            let obj = object_of(&arg(0))?;
            let key = crate::context::to_property_key(agent, &arg(1))?;
            let value = arg(2);
            let receiver = match arg(3) {
                v if v.is_undefined() => Value::Object(obj),
                other => other,
            };
            let status = obj.set_with_receiver_key(&key, value, receiver, false)?;
            Ok(Value::Boolean(status))
        }
        "setPrototypeOf" => {
            let obj = object_of(&arg(0))?;
            let proto = match arg(1).kind() {
                ValueKind::Object(proto) => Some(proto),
                ValueKind::Null => None,
                _ => {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Reflect.setPrototypeOf requires an object or null prototype".into(),
                    ));
                }
            };
            Ok(Value::Boolean(obj.set_prototype_of(proto)?))
        }
        _ => unreachable!("only the registered Reflect methods dispatch here"),
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
    fn apply_and_construct() {
        assert_eq!(
            run("Reflect.apply(Math.max, null, [1, 5, 3])").unwrap(),
            Value::Number(5.0)
        );
        assert_eq!(
            run("Reflect.apply(function (a, b) { return this.x + a + b; }, { x: 10 }, [1, 2])")
                .unwrap(),
            Value::Number(13.0)
        );
        assert_eq!(
            run("Reflect.construct(Date, []).getTime() > 0").unwrap(),
            Value::Boolean(true)
        );
        // A custom newTarget: the instance inherits from its prototype.
        assert_eq!(
            run(
                "class A { constructor() { this.tag = 'a'; } } class B { constructor() { this.tag = 'b'; } } const o = Reflect.construct(A, [], B); [o.tag, o instanceof A, o instanceof B].join(',')"
            )
            .unwrap(),
            str("a,false,true")
        );
    }

    #[test]
    fn boolean_operations_return_results() {
        assert_eq!(
            run(
                "const o = {}; Reflect.defineProperty(o, 'x', { value: 3, enumerable: true }); JSON.stringify([Reflect.has(o, 'x'), Reflect.getOwnPropertyDescriptor(o, 'x').value, Reflect.isExtensible(o), Reflect.preventExtensions(o), Reflect.isExtensible(o)])"
            )
            .unwrap(),
            str("[true,3,true,true,false]")
        );
        assert_eq!(
            run("Reflect.deleteProperty({ x: 1 }, 'x')").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("Reflect.deleteProperty({}, 'missing')").unwrap(),
            Value::Boolean(true)
        );
        // defineProperty returns false (not throwing) for a rejected define.
        assert_eq!(
            run("const o = Object.freeze({ x: 1 }); Reflect.defineProperty(o, 'x', { value: 2 })")
                .unwrap(),
            Value::Boolean(false)
        );
        // set returns false (not throwing) for a read-only property.
        assert_eq!(
            run("Reflect.set(Object.freeze({ x: 1 }), 'x', 2)").unwrap(),
            Value::Boolean(false)
        );
    }

    #[test]
    fn get_set_with_receiver() {
        assert_eq!(
            run(
                "const p = { get x() { return this.receiver; } }; Reflect.get(p, 'x', { receiver: 'yes' })"
            )
            .unwrap(),
            str("yes")
        );
        assert_eq!(
            run(
                "const p = { set x(v) { this.saw = v; } }; const r = {}; Reflect.set(p, 'x', 9, r); r.saw"
            )
            .unwrap(),
            Value::Number(9.0)
        );
    }

    #[test]
    fn prototype_and_own_keys() {
        assert_eq!(
            run("const o = { a: 1, b: 2 }; Reflect.ownKeys(o).join(',')").unwrap(),
            str("a,b")
        );
        assert_eq!(
            run(
                "const o = {}; const p = { x: 1 }; Reflect.setPrototypeOf(o, p); Reflect.getPrototypeOf(o) === p"
            )
            .unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("Reflect.getPrototypeOf({}) === Object.prototype").unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn reflects_work_over_proxies() {
        assert_eq!(
            run(
                "const p = new Proxy({ x: 1 }, { get(t, k) { return k === 'y' ? 9 : t[k]; } }); JSON.stringify([Reflect.get(p, 'x'), Reflect.get(p, 'y'), Reflect.has(p, 'x')])"
            )
            .unwrap(),
            str("[1,9,true]")
        );
    }

    #[test]
    fn non_object_targets_throw() {
        for source in [
            "Reflect.get(1, 'x')",
            "Reflect.set('s', 'x', 1)",
            "Reflect.ownKeys(true)",
            "Reflect.getPrototypeOf(null)",
        ] {
            assert!(run(source).is_err(), "{source} should throw");
        }
    }
}
