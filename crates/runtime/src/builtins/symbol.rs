//! The `%Symbol%` intrinsic (spec 20.4): the non-constructible constructor,
//! the well-known symbol statics, the global registry (`Symbol.for`/`keyFor`),
//! and `%Symbol.prototype%`. Bodies are placeholders; `runtime::function::call`
//! dispatches by intrinsic identity (the %eval% pattern).

use crux::convert::to_string;
use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::symbol::{Symbol, descriptive_string, well_known};
use crux::value::{Value, ValueKind};

use crate::agent::Agent;
use crate::context::as_object;
use crate::realm::Realm;

const SYMBOL: &str = "%Symbol%";
const SYMBOL_PROTO: &str = "%Symbol.prototype%";
const FOR: &str = "%Symbol.for%";
const KEY_FOR: &str = "%Symbol.keyFor%";
const PROTO_TO_STRING: &str = "%Symbol.prototype.toString%";
const PROTO_VALUE_OF: &str = "%Symbol.prototype.valueOf%";
const PROTO_DESCRIPTION: &str = "%Symbol.prototype.description%";
const PROTO_TO_PRIMITIVE: &str = "%Symbol.prototype.@@toPrimitive%";

/// The well-known symbol statics installed on %Symbol% (spec 20.4.2).
const WELL_KNOWN_STATICS: &[&str] = &[
    "asyncDispose",
    "asyncIterator",
    "dispose",
    "hasInstance",
    "isConcatSpreadable",
    "iterator",
    "match",
    "matchAll",
    "replace",
    "search",
    "species",
    "split",
    "toPrimitive",
    "toStringTag",
    "unscopables",
];

fn placeholder(name: &'static str) -> NativeFn {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

/// Install the Symbol intrinsics and the global `Symbol` binding (spec
/// 20.4.1-20.4.3), during SetDefaultGlobalBindings.
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let symbol_proto = JsObject::ordinary_object_create(object_proto);
    let symbol_proto_value = Value::Object(symbol_proto);

    // %Symbol% (20.4.1): non-constructible when called with new (the body
    // throws), but it still has a [[Construct]] internal method so
    // IsConstructor(Symbol) is true (subclassable per spec; the proxy
    // construct trap fires over it).
    let symbol_ctor = Function::create_builtin(
        Some(JsString::from_utf8("Symbol")),
        0,
        Box::new(placeholder("Symbol")),
        Some(Box::new(|_new_target, _args| {
            Err(JsError::new(
                ErrorKind::TypeError,
                "Symbol is not a constructor".into(),
            ))
        })),
        None,
    )?;
    let symbol_ctor_value = Value::Function(symbol_ctor);

    realm.intrinsics.define(SYMBOL, symbol_ctor_value);
    realm.intrinsics.define(SYMBOL_PROTO, symbol_proto_value);

    // 20.4.2.11: Symbol.prototype is non-writable and non-configurable.
    symbol_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(symbol_proto_value),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    symbol_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(symbol_ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // The well-known symbols (20.4.2): static data properties whose values
    // are the canonical symbol singletons.
    for name in WELL_KNOWN_STATICS {
        symbol_ctor.define_property(
            &JsString::from_utf8(name),
            &PropertyDescriptor::none(Value::Symbol(well_known(name))),
        )?;
    }

    // Symbol.for / Symbol.keyFor (20.4.2.4, 20.4.2.8).
    for (name, length, key) in [("for", 1, FOR), ("keyFor", 1, KEY_FOR)] {
        let method = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            Box::new(placeholder(key)),
            None,
            None,
        )?;
        realm.intrinsics.define(key, Value::Function(method));
        symbol_ctor.define_property(
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

    // %Symbol.prototype% methods.
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
        symbol_proto.define_property(
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

    // The `description` accessor (20.4.3.2).
    let description_getter = Function::create_builtin(
        Some(JsString::from_utf8("get description")),
        0,
        placeholder(PROTO_DESCRIPTION),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(PROTO_DESCRIPTION, Value::Function(description_getter));
    symbol_proto.define_property(
        &JsString::from_utf8("description"),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: Some(Value::Function(description_getter)),
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // Symbol.prototype[@@toPrimitive] (20.4.3.5) and [@@toStringTag] (20.4.3.6).
    let to_primitive = Function::create_builtin(
        Some(JsString::from_utf8("[Symbol.toPrimitive]")),
        1,
        placeholder(PROTO_TO_PRIMITIVE),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(PROTO_TO_PRIMITIVE, Value::Function(to_primitive));
    symbol_proto.define_property_key(
        &PropertyKey::Symbol(well_known("toPrimitive")),
        &PropertyDescriptor {
            value: Some(Value::Function(to_primitive)),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    // Symbol.prototype[@@toStringTag] (20.4.3.6): "Symbol", writable and
    // enumerable false, configurable true.
    symbol_proto.define_property_key(
        &PropertyKey::Symbol(well_known("toStringTag")),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8("Symbol")))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("Symbol"),
        &PropertyDescriptor {
            value: Some(symbol_ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

fn symbol_value(agent: &Agent, value: &Value) -> Result<Symbol, JsError> {
    match value.kind() {
        ValueKind::Symbol(symbol) => Ok(symbol.as_ref().clone()),
        ValueKind::Object(obj) => agent.symbol_data.get(&obj.id()).cloned().ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "Symbol.prototype requires a Symbol".into(),
            )
        }),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "Symbol.prototype requires a Symbol".into(),
        )),
    }
}

pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(SYMBOL).as_ref() == Some(callee) {
        // Symbol(description) (spec 20.4.1.1): the description is ToString'd,
        // or *undefined* for no argument.
        return Some((|| {
            let description = match args.first() {
                None => None,
                Some(value) if value.is_undefined() => None,
                Some(value) => Some(to_string(value)?),
            };
            Ok(Value::Symbol(Handle::new(Symbol::new(description))))
        })());
    }
    if intrinsics.get(FOR).as_ref() == Some(callee) {
        return Some(symbol_for(agent, args));
    }
    if intrinsics.get(KEY_FOR).as_ref() == Some(callee) {
        return Some(symbol_key_for(agent, args));
    }
    if intrinsics.get(PROTO_TO_STRING).as_ref() == Some(callee) {
        return Some(symbol_value(agent, this).map(|symbol| {
            Value::String(Handle::new(JsString::from_utf8(&descriptive_string(
                &symbol,
            ))))
        }));
    }
    if intrinsics.get(PROTO_VALUE_OF).as_ref() == Some(callee) {
        return Some(symbol_value(agent, this).map(|symbol| Value::Symbol(Handle::new(symbol))));
    }
    if intrinsics.get(PROTO_TO_PRIMITIVE).as_ref() == Some(callee) {
        return Some(symbol_value(agent, this).map(|symbol| Value::Symbol(Handle::new(symbol))));
    }
    if intrinsics.get(PROTO_DESCRIPTION).as_ref() == Some(callee) {
        return Some(
            symbol_value(agent, this).map(|symbol| match symbol.description {
                Some(description) => Value::String(Handle::new(description)),
                None => Value::Undefined,
            }),
        );
    }
    None
}

pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    _args: &[Value],
    _new_target: &Value,
) -> Option<Result<Value, JsError>> {
    if agent
        .current_realm()
        .ok()
        .and_then(|realm| realm.intrinsics.get(SYMBOL))
        .as_ref()
        == Some(callee)
    {
        return Some(Err(JsError::new(
            ErrorKind::TypeError,
            "Symbol is not a constructor".into(),
        )));
    }
    None
}

/// Symbol.for (spec 20.4.2.4): the global-registry symbol for `key`,
/// creating it on first use.
fn symbol_for(agent: &mut Agent, args: &[Value]) -> Result<Value, JsError> {
    let key = match args.first() {
        None => JsString::from_utf8("undefined"),
        Some(value) if value.is_undefined() => JsString::from_utf8("undefined"),
        Some(value) => to_string(value)?,
    };
    let mut registry = agent.global_symbol_registry.borrow_mut();
    if let Some((_, symbol)) = registry.iter().find(|(k, _)| *k == key) {
        return Ok(Value::Symbol(Handle::new(symbol.clone())));
    }
    let symbol = Symbol::new(Some(key.clone()));
    registry.push((key, symbol.clone()));
    Ok(Value::Symbol(Handle::new(symbol)))
}

/// Symbol.keyFor (spec 20.4.2.8): the registry key of a symbol, or
/// *undefined* when it was not created by `Symbol.for`.
fn symbol_key_for(agent: &mut Agent, args: &[Value]) -> Result<Value, JsError> {
    let ValueKind::Symbol(symbol) = args.first().cloned().unwrap_or(Value::Undefined).kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Symbol.keyFor requires a symbol".into(),
        ));
    };
    let registry = agent.global_symbol_registry.borrow();
    match registry.iter().find(|(_, s)| s.id == symbol.id) {
        Some((key, _)) => Ok(Value::String(Handle::new(key.clone()))),
        None => Ok(Value::Undefined),
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
    fn symbols_are_unique_primitives() {
        assert_eq!(run("typeof Symbol('x')").unwrap(), str("symbol"));
        // Same description does not make the same symbol.
        assert_eq!(
            run("Symbol('x') === Symbol('x')").unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(run("Symbol() !== Symbol()").unwrap(), Value::Boolean(true));
    }

    #[test]
    fn symbol_is_not_constructible() {
        assert!(run("new Symbol()").is_err());
    }

    #[test]
    fn symbol_methods_and_description() {
        assert_eq!(run("Symbol('abc').toString()").unwrap(), str("Symbol(abc)"));
        assert_eq!(run("Symbol('abc').description").unwrap(), str("abc"));
        assert_eq!(run("Symbol().description").unwrap(), Value::Undefined);
        assert_eq!(
            run("Symbol.prototype.toString.call(Symbol('d'))").unwrap(),
            str("Symbol(d)")
        );
    }

    #[test]
    fn symbol_for_and_key_for_share_a_registry() {
        assert_eq!(
            run("Symbol.for('k') === Symbol.for('k')").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("Symbol.for('k') !== Symbol('k')").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(run("Symbol.keyFor(Symbol.for('k'))").unwrap(), str("k"));
        assert_eq!(run("Symbol.keyFor(Symbol('k'))").unwrap(), Value::Undefined);
        assert_eq!(run("Symbol.for('k').description").unwrap(), str("k"));
    }

    #[test]
    fn well_known_symbol_statics_exist() {
        assert_eq!(run("typeof Symbol.iterator").unwrap(), str("symbol"));
        assert_eq!(run("typeof Symbol.toStringTag").unwrap(), str("symbol"));
        assert_eq!(run("typeof Symbol.hasInstance").unwrap(), str("symbol"));
        assert_eq!(
            run("Symbol.iterator === Symbol.iterator").unwrap(),
            Value::Boolean(true)
        );
        // toStringTag is not enumerable.
        assert_eq!(
            run("Object.keys(Symbol).length").unwrap(),
            Value::Number(0.0)
        );
    }
}
