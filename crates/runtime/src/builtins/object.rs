//! The `%Object%` intrinsic (spec 20.1): the constructor, `%Object.prototype%`
//! methods, and the statics. Every body is a placeholder; the runtime
//! call/construct dispatchers recognize the methods by intrinsic identity
//! (the %eval% pattern), because the operations reach user code and the
//! agent.

use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::ops::same_value;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::Value;

use crate::agent::Agent;
use crate::context::{as_object, to_object};
use crate::realm::Realm;

const OBJECT: &str = "%Object%";
const OBJECT_PROTO: &str = "%Object.prototype%";
const PROTO_TO_STRING: &str = "%Object.prototype.toString%";
const PROTO_VALUE_OF: &str = "%Object.prototype.valueOf%";
const PROTO_HAS_OWN: &str = "%Object.prototype.hasOwnProperty%";
const PROTO_IS_PROTO_OF: &str = "%Object.prototype.isPrototypeOf%";
const PROTO_PROP_IS_ENUM: &str = "%Object.prototype.propertyIsEnumerable%";
const PROTO_TO_LOCALE: &str = "%Object.prototype.toLocaleString%";
const PROTO_GET_PROTO: &str = "%Object.prototype.__proto__%";
const PROTO_SET_PROTO: &str = "%Object.prototype.__proto__set%";
const PROTO_DEFINE_GETTER: &str = "%Object.prototype.__defineGetter__%";
const PROTO_DEFINE_SETTER: &str = "%Object.prototype.__defineSetter__%";
const PROTO_LOOKUP_GETTER: &str = "%Object.prototype.__lookupGetter__%";
const PROTO_LOOKUP_SETTER: &str = "%Object.prototype.__lookupSetter__%";
const ASSIGN: &str = "%Object.assign%";
const CREATE: &str = "%Object.create%";
const DEFINE_PROPERTIES: &str = "%Object.defineProperties%";
const DEFINE_PROPERTY: &str = "%Object.defineProperty%";
const ENTRIES: &str = "%Object.entries%";
const FREEZE: &str = "%Object.freeze%";
const FROM_ENTRIES: &str = "%Object.fromEntries%";
const GET_OWN_DESC: &str = "%Object.getOwnPropertyDescriptor%";
const GET_OWN_DESCS: &str = "%Object.getOwnPropertyDescriptors%";
const GET_OWN_NAMES: &str = "%Object.getOwnPropertyNames%";
const GET_OWN_SYMBOLS: &str = "%Object.getOwnPropertySymbols%";
const GROUP_BY: &str = "%Object.groupBy%";
const GET_PROTO: &str = "%Object.getPrototypeOf%";
const HAS_OWN: &str = "%Object.hasOwn%";
const IS: &str = "%Object.is%";
const IS_EXTENSIBLE: &str = "%Object.isExtensible%";
const IS_FROZEN: &str = "%Object.isFrozen%";
const IS_SEALED: &str = "%Object.isSealed%";
const KEYS: &str = "%Object.keys%";
const PREVENT_EXTENSIONS: &str = "%Object.preventExtensions%";
const SEAL: &str = "%Object.seal%";
const SET_PROTO: &str = "%Object.setPrototypeOf%";
const VALUES: &str = "%Object.values%";

fn placeholder(name: &'static str) -> NativeFn {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

/// Install the Object intrinsics and the global `Object` binding (spec
/// 20.1.1-20.1.3), during SetDefaultGlobalBindings. Runs first so the other
/// built-ins can link their prototypes to `%Object.prototype%`.
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = JsObject::ordinary_object_create(None);
    // spec 20.1.3: %Object.prototype% is an immutable prototype exotic
    // object (9.4.7): its prototype is null and never changes.
    object_proto.mark_immutable_prototype();
    let object_proto_value = Value::Object(object_proto.clone());

    let object_ctor = Function::create_builtin(
        Some(JsString::from_utf8("Object")),
        1,
        Box::new(placeholder("Object")),
        Some(Box::new(placeholder("Object"))),
        None,
    )?;
    let object_ctor_value = Value::Function(object_ctor.clone());

    realm.intrinsics.define(OBJECT, object_ctor_value.clone());
    realm
        .intrinsics
        .define(OBJECT_PROTO, object_proto_value.clone());

    // 20.1.1.2: Object.prototype is non-writable, non-enumerable,
    // non-configurable (the only such prototype property in the spec).
    object_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(object_proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    // 20.1.3.1: the constructor back-reference.
    object_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(object_ctor_value.clone()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    install_prototype_methods(realm, &object_proto)?;
    install_statics(realm, &object_ctor.object)?;

    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("Object"),
        &PropertyDescriptor {
            value: Some(object_ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// A data method of %Object.prototype% or a static, registered by intrinsic
/// identity so the dispatcher can find it.
fn define_method(
    realm: &Handle<Realm>,
    owner: &JsObject,
    name: &str,
    length: u64,
    key: &'static str,
    enumerable: bool,
) -> Result<(), JsError> {
    let method = Function::create_builtin(
        Some(JsString::from_utf8(name)),
        length,
        placeholder(key),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(key, Value::Function(method.clone()));
    owner.define_property(
        &JsString::from_utf8(name),
        &PropertyDescriptor {
            value: Some(Value::Function(method)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(enumerable),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

fn install_prototype_methods(realm: &Handle<Realm>, proto: &JsObject) -> Result<(), JsError> {
    for (name, length, key) in [
        ("toString", 0, PROTO_TO_STRING),
        ("valueOf", 0, PROTO_VALUE_OF),
        ("hasOwnProperty", 1, PROTO_HAS_OWN),
        ("isPrototypeOf", 1, PROTO_IS_PROTO_OF),
        ("propertyIsEnumerable", 1, PROTO_PROP_IS_ENUM),
        ("toLocaleString", 0, PROTO_TO_LOCALE),
        // Annex B: the legacy accessor methods (spec B.2.2.2-B.2.2.5).
        ("__defineGetter__", 2, PROTO_DEFINE_GETTER),
        ("__defineSetter__", 2, PROTO_DEFINE_SETTER),
        ("__lookupGetter__", 1, PROTO_LOOKUP_GETTER),
        ("__lookupSetter__", 1, PROTO_LOOKUP_SETTER),
    ] {
        define_method(realm, proto, name, length, key, false)?;
    }
    // Annex B: the `__proto__` accessor pair (spec B.2.2.1).
    let getter = Function::create_builtin(
        Some(JsString::from_utf8("get __proto__")),
        0,
        placeholder(PROTO_GET_PROTO),
        None,
        None,
    )?;
    let setter = Function::create_builtin(
        Some(JsString::from_utf8("set __proto__")),
        1,
        placeholder(PROTO_SET_PROTO),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(PROTO_GET_PROTO, Value::Function(getter.clone()));
    realm
        .intrinsics
        .define(PROTO_SET_PROTO, Value::Function(setter.clone()));
    proto.define_property(
        &JsString::from_utf8("__proto__"),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: Some(Value::Function(getter)),
            set: Some(Value::Function(setter)),
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

fn install_statics(realm: &Handle<Realm>, ctor: &JsObject) -> Result<(), JsError> {
    for (name, length, key) in [
        ("assign", 2, ASSIGN),
        ("create", 2, CREATE),
        ("defineProperties", 2, DEFINE_PROPERTIES),
        ("defineProperty", 3, DEFINE_PROPERTY),
        ("entries", 1, ENTRIES),
        ("freeze", 1, FREEZE),
        ("fromEntries", 1, FROM_ENTRIES),
        ("getOwnPropertyDescriptor", 2, GET_OWN_DESC),
        ("getOwnPropertyDescriptors", 1, GET_OWN_DESCS),
        ("getOwnPropertyNames", 1, GET_OWN_NAMES),
        ("getOwnPropertySymbols", 1, GET_OWN_SYMBOLS),
        ("getPrototypeOf", 1, GET_PROTO),
        ("groupBy", 2, GROUP_BY),
        ("hasOwn", 2, HAS_OWN),
        ("is", 2, IS),
        ("isExtensible", 1, IS_EXTENSIBLE),
        ("isFrozen", 1, IS_FROZEN),
        ("isSealed", 1, IS_SEALED),
        ("keys", 1, KEYS),
        ("preventExtensions", 1, PREVENT_EXTENSIONS),
        ("seal", 1, SEAL),
        ("setPrototypeOf", 2, SET_PROTO),
        ("values", 1, VALUES),
    ] {
        define_method(realm, ctor, name, length, key, false)?;
    }
    Ok(())
}

/// Whether `callee` is the intrinsic registered under `key`.
fn is_intrinsic(agent: &Agent, callee: &Value, key: &str) -> bool {
    agent
        .current_realm()
        .ok()
        .and_then(|realm| realm.intrinsics.get(key))
        .as_ref()
        == Some(callee)
}

fn arg(args: &[Value], index: usize) -> &Value {
    args.get(index).unwrap_or(&Value::Undefined)
}

fn str(value: &str) -> Value {
    Value::String(Handle::new(JsString::from_utf8(value)))
}

/// Object.prototype.toString (spec 20.1.3.6): `[object Tag]` from the
/// value's kind, honoring the `@@toStringTag` override. Every value is
/// ToObject'd first; the built-in tag (steps 4-14) comes from IsArray, the
/// [[Call]]/[[ParameterMap]] slots, the boxed-primitive marker, the error/
/// RegExp brands, or the object kind, and a string @@toStringTag overrides it.
fn prototype_to_string(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let tag = match this {
        Value::Undefined => "Undefined".to_string(),
        Value::Null => "Null".to_string(),
        _ => {
            // spec step 3: ToObject. Function values pass through unchanged.
            let object = crate::context::to_object(agent, this)?;
            let builtin_tag = builtin_tag(agent, &object)?;
            // spec steps 15-16: an own or inherited @@toStringTag string
            // overrides the built-in tag.
            let tag = crate::context::get_property_key(
                agent,
                &object,
                &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
                object.clone(),
            )?;
            match tag {
                Value::String(text) => {
                    return Ok(str(&format!("[object {}]", text.to_string_lossy())));
                }
                _ => builtin_tag,
            }
        }
    };
    Ok(str(&format!("[object {tag}]")))
}

/// The built-in tag of a ToObject'd value (spec 20.1.3.6 steps 4-14). Note
/// that there is no BigInt built-in tag: BigInt wrappers fall to "Object".
fn builtin_tag(agent: &mut Agent, object: &Value) -> Result<String, JsError> {
    if is_array_for_to_string(object)? {
        return Ok("Array".to_string());
    }
    let Value::Object(obj) = object else {
        return Ok("Function".to_string());
    };
    if matches!(obj.kind, crux::object::ObjectKind::Arguments(_)) {
        return Ok("Arguments".to_string());
    }
    // spec step 7: [[Call]] — functions and proxies over callables.
    if crux::value::is_callable(object) {
        return Ok("Function".to_string());
    }
    if let Some(boxed) = &*obj.boxed.borrow() {
        return Ok(match boxed {
            crux::object::BoxedPrimitive::Boolean(_) => "Boolean",
            crux::object::BoxedPrimitive::Number(_) => "Number",
            crux::object::BoxedPrimitive::BigInt(_) => "Object",
        }
        .to_string());
    }
    if agent.error_data.contains(&obj.id()) {
        return Ok("Error".to_string());
    }
    if agent.date_data.contains_key(&obj.id()) {
        // The [[DateValue]] slot (spec 20.1.3.6 step 12).
        return Ok("Date".to_string());
    }
    if agent.regexp_data.contains_key(&obj.id()) {
        // RegExp is branded via its [[RegExpMatcher]] slot (its @@toStringTag
        // was removed from the spec).
        return Ok("RegExp".to_string());
    }
    if matches!(obj.kind, crux::object::ObjectKind::Proxy(_)) {
        // A proxy over a plain object has no built-in tag of its own
        // (spec 20.1.3.6 steps 6-14 check O's own slots).
        return Ok("Object".to_string());
    }
    Ok(obj.kind.name().to_string())
}

/// IsArray (spec 7.2.2) for the built-in tag: proxies recurse to their
/// target; a revoked proxy's target is empty.
fn is_array_for_to_string(value: &Value) -> Result<bool, JsError> {
    match value {
        Value::Object(obj) => match &obj.kind {
            crux::object::ObjectKind::Array => Ok(true),
            crux::object::ObjectKind::Proxy(slots) => {
                let Some(target) = slots.target.borrow().as_ref().cloned() else {
                    return Ok(false);
                };
                is_array_for_to_string(&target)
            }
            _ => Ok(false),
        },
        Value::Function(f) => Ok(matches!(f.object.kind, crux::object::ObjectKind::Array)),
        _ => Ok(false),
    }
}

/// EnumerableOwnPropertyNames (spec 7.3.23) restricted to string keys.
fn enumerable_string_keys(agent: &mut Agent, value: &Value) -> Result<Vec<JsString>, JsError> {
    let object = to_object(agent, value)?;
    let obj = as_object(&object)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "value is not an object".into()))?;
    let mut out = Vec::new();
    for key in obj.own_property_keys()? {
        let PropertyKey::String(id) = key else {
            continue;
        };
        if let Some(prop) = obj.get_own_property_key(&PropertyKey::String(id))?
            && prop.enumerable
        {
            out.push(crux::lookup(id));
        }
    }
    Ok(out)
}

/// Build an Array of values linked to %Array.prototype%.
fn array_of(agent: &mut Agent, values: &[Value]) -> Result<Value, JsError> {
    crate::builtins::array::array_from_values(agent, values)
}

/// The GetOwnPropertyKeys machinery shared by getOwnPropertyNames and
/// getOwnPropertySymbols (spec 20.1.2.11.1): the own keys of one type.
fn own_keys_of(agent: &mut Agent, value: &Value, want_symbols: bool) -> Result<Value, JsError> {
    let object = to_object(agent, value)?;
    let obj = as_object(&object)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "value is not an object".into()))?;
    let mut keys = Vec::new();
    for key in obj.own_property_keys()? {
        let matches = matches!(
            (&key, want_symbols),
            (PropertyKey::String(_), false) | (PropertyKey::Symbol(_), true)
        );
        if matches {
            let value = match key {
                PropertyKey::String(id) => {
                    let text = crux::lookup(id);
                    Value::String(Handle::new(text))
                }
                PropertyKey::Symbol(sym) => Value::Symbol(Handle::new(sym)),
            };
            keys.push(value);
        }
    }
    array_of(agent, &keys)
}

/// Object.freeze/Object.seal (spec 20.1.2.7 / 20.1.2.20): a primitive
/// receiver is returned as-is; SetIntegrityLevel failure throws a TypeError.
fn freeze_or_seal(value: &Value, freeze: bool) -> Result<Value, JsError> {
    // spec step 1: Type(O) is not Object → return O unchanged.
    if !matches!(value, Value::Object(_) | Value::Function(_)) {
        return Ok(value.clone());
    }
    if !set_integrity_level(value, freeze)? {
        return Err(JsError::new(
            ErrorKind::TypeError,
            if freeze {
                "Cannot freeze the object".into()
            } else {
                "Cannot seal the object".into()
            },
        ));
    }
    Ok(value.clone())
}

/// SetIntegrityLevel (spec 7.3.15): freeze (writable off too) or seal.
/// Returns the status; a failed [[PreventExtensions]] aborts with `false`.
fn set_integrity_level(value: &Value, freeze: bool) -> Result<bool, JsError> {
    let obj = as_object(value)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "value is not an object".into()))?;
    // spec steps 1-2: prevent extensions first; a false status returns false.
    if !obj.prevent_extensions()? {
        return Ok(false);
    }
    for key in obj.own_property_keys()? {
        let Some(current) = obj.get_own_property_key(&key)? else {
            continue;
        };
        // spec steps 4-5: seal sets only [[Configurable]] false; freeze also
        // sets [[Writable]] false on data properties. The descriptors are
        // partial, so a proxy defineProperty trap sees only those fields
        // (no value/enumerable/get/set).
        let desc = if current.is_accessor() {
            PropertyDescriptor {
                value: None,
                writable: None,
                get: None,
                set: None,
                enumerable: None,
                configurable: Some(false),
            }
        } else {
            PropertyDescriptor {
                value: None,
                writable: if freeze { Some(false) } else { None },
                get: None,
                set: None,
                enumerable: None,
                configurable: Some(false),
            }
        };
        // DefinePropertyOrThrow (spec steps 4.a.i / 5.b.iii).
        if !obj.define_property_key(&key, &desc)? {
            return Err(JsError::new(
                ErrorKind::TypeError,
                format!(
                    "Cannot define property {} on a non-extensible object",
                    key.display_string()
                ),
            ));
        }
    }
    Ok(true)
}

/// TestIntegrityLevel (spec 7.3.16).
fn test_integrity_level(agent: &mut Agent, value: &Value, freeze: bool) -> Result<bool, JsError> {
    // spec 20.1.2.12 step 1 / 20.1.2.14 step 1: a primitive is frozen and
    // sealed trivially (no own properties, not extensible).
    if !matches!(value, Value::Object(_) | Value::Function(_)) {
        return Ok(true);
    }
    let object = to_object(agent, value)?;
    let obj = match as_object(&object) {
        Some(obj) => obj,
        None => return Ok(false),
    };
    if !obj.is_extensible()? {
        for key in obj.own_property_keys()? {
            let Some(current) = obj.get_own_property_key(&key)? else {
                continue;
            };
            if current.configurable {
                return Ok(false);
            }
            if freeze && current.is_data() && current.writable().unwrap_or(true) {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    Ok(false)
}

/// ObjectDefineProperties (spec 20.1.2.3.1): define every own enumerable
/// key of `properties` on `obj`.
fn object_define_properties(
    agent: &mut Agent,
    object: &Value,
    properties: &Value,
) -> Result<Value, JsError> {
    let props = to_object(agent, properties)?;
    let props_obj = as_object(&props)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "value is not an object".into()))?;
    // spec 20.1.2.3 step 1: a primitive receiver throws (functions are
    // objects, so `as_object` accepts them).
    let target = as_object(object).ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "Object.defineProperties called on non-object".into(),
        )
    })?;
    for key in props_obj.own_property_keys()? {
        let Some(prop) = props_obj.get_own_property_key(&key)? else {
            continue;
        };
        if !prop.enumerable {
            continue;
        }
        let value = crate::context::get_property_key(agent, &props, &key, props.clone())?;
        let mut desc = crux::property::to_property_descriptor(&value)?;
        // ArraySetLength coerces an object [[Value]] through the agent
        // (spec 10.4.2.4 steps 3-4); crux cannot invoke user toString.
        if matches!(target.kind, crux::object::ObjectKind::Array)
            && key == PropertyKey::from_utf8("length")
            && let Some(length_value) = &desc.value
            && matches!(length_value, Value::Object(_) | Value::Function(_))
        {
            desc.value = Some(Value::Number(crate::context::to_number(
                agent,
                length_value,
            )?));
        }
        if !target.define_property_key(&key, &desc)? {
            return Err(JsError::new(
                ErrorKind::TypeError,
                format!(
                    "Cannot define property {} on a non-extensible object",
                    key.display_string()
                ),
            ));
        }
    }
    Ok(object.clone())
}

/// Dispatch a %Object% static or %Object.prototype% method call.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(OBJECT).as_ref() == Some(callee) {
        return Some(object_constructor(agent, callee, args));
    }
    if intrinsics.get(PROTO_TO_STRING).as_ref() == Some(callee) {
        return Some(prototype_to_string(agent, this));
    }
    if intrinsics.get(PROTO_VALUE_OF).as_ref() == Some(callee) {
        return Some(to_object(agent, this));
    }
    if intrinsics.get(PROTO_HAS_OWN).as_ref() == Some(callee) {
        return Some((|| {
            let key = crate::context::to_property_key(agent, arg(args, 0))?;
            let object = to_object(agent, this)?;
            let obj = as_object(&object).ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "value is not an object".into())
            })?;
            Ok(Value::Boolean(obj.has_own_property_key(&key)?))
        })());
    }
    if intrinsics.get(PROTO_IS_PROTO_OF).as_ref() == Some(callee) {
        return Some((|| {
            let candidate = arg(args, 0);
            let Some(candidate_obj) = crate::context::as_object(candidate) else {
                return Ok(Value::Boolean(false));
            };
            let object = to_object(agent, this)?;
            let Some(this_obj) = crate::context::as_object(&object) else {
                return Ok(Value::Boolean(false));
            };
            let mut proto = candidate_obj.get_prototype_of()?;
            while let Some(p) = proto {
                if Handle::ptr_eq(&p, &this_obj) {
                    return Ok(Value::Boolean(true));
                }
                proto = p.get_prototype_of()?;
            }
            Ok(Value::Boolean(false))
        })());
    }
    if intrinsics.get(PROTO_PROP_IS_ENUM).as_ref() == Some(callee) {
        return Some((|| {
            let key = crate::context::to_property_key(agent, arg(args, 0))?;
            let object = to_object(agent, this)?;
            let obj = as_object(&object).ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "value is not an object".into())
            })?;
            let enumerable = obj
                .get_own_property_key(&key)?
                .map(|prop| prop.enumerable)
                .unwrap_or(false);
            Ok(Value::Boolean(enumerable))
        })());
    }
    if intrinsics.get(PROTO_TO_LOCALE).as_ref() == Some(callee) {
        return Some((|| {
            let method = crate::context::get_property(
                agent,
                this,
                &JsString::from_utf8("toString"),
                this.clone(),
            )?;
            crate::function::call(agent, &method, this.clone(), &[])
        })());
    }
    if intrinsics.get(PROTO_GET_PROTO).as_ref() == Some(callee) {
        return Some(get_prototype_of(agent, this));
    }
    if intrinsics.get(PROTO_SET_PROTO).as_ref() == Some(callee) {
        return Some(proto_setter(this, arg(args, 0)));
    }
    if intrinsics.get(PROTO_DEFINE_GETTER).as_ref() == Some(callee) {
        return Some(define_legacy_accessor(agent, this, args, true));
    }
    if intrinsics.get(PROTO_DEFINE_SETTER).as_ref() == Some(callee) {
        return Some(define_legacy_accessor(agent, this, args, false));
    }
    if intrinsics.get(PROTO_LOOKUP_GETTER).as_ref() == Some(callee) {
        return Some(lookup_legacy_accessor(agent, this, arg(args, 0), true));
    }
    if intrinsics.get(PROTO_LOOKUP_SETTER).as_ref() == Some(callee) {
        return Some(lookup_legacy_accessor(agent, this, arg(args, 0), false));
    }
    if intrinsics.get(ASSIGN).as_ref() == Some(callee) {
        return Some(object_assign(agent, args));
    }
    if intrinsics.get(CREATE).as_ref() == Some(callee) {
        return Some((|| {
            let proto_value = arg(args, 0);
            let proto = match proto_value {
                Value::Object(obj) => Some(obj.clone()),
                Value::Null => None,
                _ => {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Object prototype may only be an Object or null".into(),
                    ));
                }
            };
            let obj = JsObject::ordinary_object_create(proto);
            let value = Value::Object(obj);
            // spec 20.1.2.2 step 3: undefined Properties are skipped.
            if args.len() > 1 {
                let properties = arg(args, 1);
                if !matches!(properties, Value::Undefined) {
                    object_define_properties(agent, &value, properties)?;
                }
            }
            Ok(value)
        })());
    }
    if intrinsics.get(DEFINE_PROPERTY).as_ref() == Some(callee) {
        return Some((|| {
            // spec 20.1.2.4 step 1: a primitive receiver throws (functions
            // are objects, so `as_object` accepts them).
            let receiver = arg(args, 0);
            let obj = as_object(receiver).ok_or_else(|| {
                JsError::new(
                    ErrorKind::TypeError,
                    "Object.defineProperty called on non-object".into(),
                )
            })?;
            let key = crate::context::to_property_key(agent, arg(args, 1))?;
            let mut desc = crux::property::to_property_descriptor(arg(args, 2))?;
            // ArraySetLength coerces an object [[Value]] through the agent
            // (spec 10.4.2.4 steps 3-4); crux cannot invoke user toString.
            if matches!(obj.kind, crux::object::ObjectKind::Array)
                && key == PropertyKey::from_utf8("length")
                && let Some(value) = &desc.value
                && matches!(value, Value::Object(_) | Value::Function(_))
            {
                desc.value = Some(Value::Number(crate::context::to_number(agent, value)?));
            }
            if !obj.define_property_key(&key, &desc)? {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    format!(
                        "Cannot define property {} on a non-extensible object",
                        key.display_string()
                    ),
                ));
            }
            Ok(receiver.clone())
        })());
    }
    if intrinsics.get(DEFINE_PROPERTIES).as_ref() == Some(callee) {
        return Some(object_define_properties(agent, arg(args, 0), arg(args, 1)));
    }
    if intrinsics.get(ENTRIES).as_ref() == Some(callee) {
        return Some((|| {
            let keys = enumerable_string_keys(agent, arg(args, 0))?;
            let object = to_object(agent, arg(args, 0))?;
            let mut entries = Vec::new();
            for key in keys {
                let value = crate::context::get_property(agent, &object, &key, object.clone())?;
                let pair = crate::builtins::array::array_create(agent, 2.0)?;
                pair.create_data_property(&JsString::from_utf8("0"), str(&key.to_string_lossy()))?;
                pair.create_data_property(&JsString::from_utf8("1"), value)?;
                entries.push(Value::Object(pair));
            }
            array_of(agent, &entries)
        })());
    }
    if intrinsics.get(VALUES).as_ref() == Some(callee) {
        return Some((|| {
            let keys = enumerable_string_keys(agent, arg(args, 0))?;
            let object = to_object(agent, arg(args, 0))?;
            let mut values = Vec::new();
            for key in keys {
                let value = crate::context::get_property(agent, &object, &key, object.clone())?;
                values.push(value);
            }
            array_of(agent, &values)
        })());
    }
    if intrinsics.get(KEYS).as_ref() == Some(callee) {
        return Some((|| {
            let keys = enumerable_string_keys(agent, arg(args, 0))?;
            let values: Vec<Value> = keys
                .into_iter()
                .map(|key| str(&key.to_string_lossy()))
                .collect();
            array_of(agent, &values)
        })());
    }
    if intrinsics.get(GET_OWN_NAMES).as_ref() == Some(callee) {
        return Some(own_keys_of(agent, arg(args, 0), false));
    }
    if intrinsics.get(GET_OWN_SYMBOLS).as_ref() == Some(callee) {
        return Some(own_keys_of(agent, arg(args, 0), true));
    }
    if intrinsics.get(GROUP_BY).as_ref() == Some(callee) {
        return Some(object_group_by(agent, args));
    }
    if intrinsics.get(GET_OWN_DESC).as_ref() == Some(callee) {
        return Some((|| {
            let object = to_object(agent, arg(args, 0))?;
            let key = crate::context::to_property_key(agent, arg(args, 1))?;
            let obj = as_object(&object).ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "value is not an object".into())
            })?;
            let Some(prop) = obj.get_own_property_key(&key)? else {
                return Ok(Value::Undefined);
            };
            let desc = prop.to_descriptor();
            crux::property::from_property_descriptor(
                &desc,
                realm
                    .intrinsics
                    .get(OBJECT_PROTO)
                    .and_then(|v| as_object(&v)),
            )
        })());
    }
    if intrinsics.get(GET_OWN_DESCS).as_ref() == Some(callee) {
        return Some((|| {
            let object = to_object(agent, arg(args, 0))?;
            let obj = as_object(&object).ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "value is not an object".into())
            })?;
            let result = JsObject::ordinary_object_create(
                realm
                    .intrinsics
                    .get(OBJECT_PROTO)
                    .and_then(|v| as_object(&v)),
            );
            for key in obj.own_property_keys()? {
                if let Some(prop) = obj.get_own_property_key(&key)? {
                    let desc = crux::property::from_property_descriptor(
                        &prop.to_descriptor(),
                        realm
                            .intrinsics
                            .get(OBJECT_PROTO)
                            .and_then(|v| as_object(&v)),
                    )?;
                    result.create_data_property_key(&key, desc)?;
                }
            }
            Ok(Value::Object(result))
        })());
    }
    if intrinsics.get(GET_PROTO).as_ref() == Some(callee) {
        return Some(get_prototype_of(agent, arg(args, 0)));
    }
    if intrinsics.get(SET_PROTO).as_ref() == Some(callee) {
        return Some(set_prototype_of(agent, arg(args, 0), arg(args, 1)));
    }
    if intrinsics.get(HAS_OWN).as_ref() == Some(callee) {
        return Some((|| {
            let object = to_object(agent, arg(args, 0))?;
            let key = crate::context::to_property_key(agent, arg(args, 1))?;
            let obj = as_object(&object).ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "value is not an object".into())
            })?;
            Ok(Value::Boolean(obj.has_own_property_key(&key)?))
        })());
    }
    if intrinsics.get(IS).as_ref() == Some(callee) {
        return Some(Ok(Value::Boolean(same_value(arg(args, 0), arg(args, 1)))));
    }
    if intrinsics.get(IS_EXTENSIBLE).as_ref() == Some(callee) {
        return Some((|| {
            let object = to_object(agent, arg(args, 0))?;
            let obj = as_object(&object).ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "value is not an object".into())
            })?;
            Ok(Value::Boolean(obj.is_extensible()?))
        })());
    }
    if intrinsics.get(PREVENT_EXTENSIONS).as_ref() == Some(callee) {
        return Some((|| {
            let object = to_object(agent, arg(args, 0))?;
            let obj = as_object(&object).ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "value is not an object".into())
            })?;
            // spec 20.1.2.18 step 4: a failed [[PreventExtensions]] (e.g. a
            // proxy trap returning false) is a TypeError.
            if !obj.prevent_extensions()? {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "Cannot prevent extensions of the object".into(),
                ));
            }
            Ok(object)
        })());
    }
    if intrinsics.get(FREEZE).as_ref() == Some(callee) {
        return Some(freeze_or_seal(arg(args, 0), true));
    }
    if intrinsics.get(SEAL).as_ref() == Some(callee) {
        return Some(freeze_or_seal(arg(args, 0), false));
    }
    if intrinsics.get(IS_FROZEN).as_ref() == Some(callee) {
        return Some((|| {
            Ok(Value::Boolean(test_integrity_level(
                agent,
                arg(args, 0),
                true,
            )?))
        })());
    }
    if intrinsics.get(IS_SEALED).as_ref() == Some(callee) {
        return Some((|| {
            Ok(Value::Boolean(test_integrity_level(
                agent,
                arg(args, 0),
                false,
            )?))
        })());
    }
    if intrinsics.get(FROM_ENTRIES).as_ref() == Some(callee) {
        return Some(from_entries(agent, arg(args, 0)));
    }
    None
}

/// Construct `new Object(...)` (spec 20.1.1.1): same wrapping behaviour as
/// the call form; derived-constructor reification is deferred.
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    _new_target: &Value,
) -> Option<Result<Value, JsError>> {
    if is_intrinsic(agent, callee, OBJECT) {
        return Some(object_constructor(agent, callee, args));
    }
    None
}

/// Object(value) (spec 20.1.1.1): undefined/null make a fresh object,
/// otherwise ToObject.
fn object_constructor(
    agent: &mut Agent,
    _callee: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    match args.first() {
        None | Some(Value::Undefined | Value::Null) => {
            let realm = agent.current_realm()?;
            let proto = realm
                .intrinsics
                .get(OBJECT_PROTO)
                .and_then(|v| as_object(&v));
            Ok(Value::Object(JsObject::ordinary_object_create(proto)))
        }
        Some(value) => to_object(agent, value),
    }
}

fn get_prototype_of(agent: &mut Agent, value: &Value) -> Result<Value, JsError> {
    let object = to_object(agent, value)?;
    let obj = as_object(&object)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "value is not an object".into()))?;
    match obj.get_prototype_of()? {
        Some(proto) => Ok(proto
            .function_value()
            .unwrap_or_else(|| Value::Object(proto))),
        None => Ok(Value::Null),
    }
}

fn set_prototype_of(
    agent: &mut Agent,
    value: &Value,
    proto_value: &Value,
) -> Result<Value, JsError> {
    let proto = match proto_value {
        Value::Object(obj) => Some(obj.clone()),
        Value::Null => None,
        _ => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Object prototype may only be an Object or null".into(),
            ));
        }
    };
    let object = to_object(agent, value)?;
    let obj = as_object(&object)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "value is not an object".into()))?;
    // Setting the same prototype is a no-op (spec 20.1.2.22 step 5).
    if obj.get_prototype_of()? == proto {
        return Ok(object);
    }
    if !obj.set_prototype_of(proto)? {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot set prototype of a non-extensible object".into(),
        ));
    }
    Ok(object)
}

/// Object.prototype.__proto__ setter (spec B.2.2.1.3): a silent no-op for
/// non-object receivers and non-object prototypes, throwing only when the
/// object's own [[SetPrototypeOf]] rejects the change.
fn proto_setter(this: &Value, proto_value: &Value) -> Result<Value, JsError> {
    // step 1: RequireObjectCoercible(this value).
    if matches!(this, Value::Undefined | Value::Null) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot convert undefined or null to object".into(),
        ));
    }
    // step 2: a prototype that is neither an Object nor null is ignored.
    let proto = match proto_value {
        Value::Object(obj) => Some(obj.clone()),
        Value::Null => None,
        _ => return Ok(Value::Undefined),
    };
    // step 3: a non-object receiver is ignored.
    let Some(obj) = as_object(this) else {
        return Ok(Value::Undefined);
    };
    // steps 4-5: [[SetPrototypeOf]] failure throws a TypeError.
    if !obj.set_prototype_of(proto)? {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot set prototype of an immutable or non-extensible object".into(),
        ));
    }
    Ok(Value::Undefined)
}

/// Object.prototype.__defineGetter__/__defineSetter__ (spec B.2.2.2.1 /
/// B.2.2.3.1): define an enumerable, configurable accessor for `P`.
fn define_legacy_accessor(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    getter: bool,
) -> Result<Value, JsError> {
    // step 1: ToObject(this value).
    let object = to_object(agent, this)?;
    let obj = as_object(&object)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "value is not an object".into()))?;
    // step 2: the accessor must be callable, checked before ToPropertyKey.
    let accessor = arg(args, 1);
    if !crux::value::is_callable(accessor) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            if getter {
                "Getter must be a function".into()
            } else {
                "Setter must be a function".into()
            },
        ));
    }
    // steps 3-4: the descriptor and the property key.
    let key = crate::context::to_property_key(agent, arg(args, 0))?;
    let desc = if getter {
        PropertyDescriptor::accessor(Some(accessor.clone()), None)
    } else {
        PropertyDescriptor::accessor(None, Some(accessor.clone()))
    };
    // step 5: DefinePropertyOrThrow.
    if !obj.define_property_key(&key, &desc)? {
        return Err(JsError::new(
            ErrorKind::TypeError,
            format!(
                "Cannot define property {} on a non-extensible object",
                key.display_string()
            ),
        ));
    }
    Ok(Value::Undefined)
}

/// Object.prototype.__lookupGetter__/__lookupSetter__ (spec B.2.2.4.1 /
/// B.2.2.5.1): walk the prototype chain for the first accessor named `P` and
/// return its getter/setter (or undefined for a data property / miss).
fn lookup_legacy_accessor(
    agent: &mut Agent,
    this: &Value,
    key_value: &Value,
    getter: bool,
) -> Result<Value, JsError> {
    let object = to_object(agent, this)?;
    let mut obj = as_object(&object)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "value is not an object".into()))?;
    let key = crate::context::to_property_key(agent, key_value)?;
    loop {
        let Some(property) = obj.get_own_property_key(&key)? else {
            // The chain walk dispatches through proxies, whose
            // [[GetPrototypeOf]] trap may throw.
            match obj.get_prototype_of()? {
                Some(next) => obj = next,
                None => return Ok(Value::Undefined),
            }
            continue;
        };
        if property.is_accessor() {
            let accessor = if getter {
                property.getter()
            } else {
                property.setter()
            };
            return Ok(accessor.unwrap_or(Value::Undefined));
        }
        return Ok(Value::Undefined);
    }
}

/// Object.groupBy (spec 20.1.2.9): GroupBy with ~property~ key coercion,
/// returned as an ordinary object with a null prototype whose own data
/// properties are the group arrays.
fn object_group_by(agent: &mut Agent, args: &[Value]) -> Result<Value, JsError> {
    let items = arg(args, 0).clone();
    let callback = arg(args, 1).clone();
    let groups = crate::builtins::keyed::group_by(agent, &items, &callback, |agent, key| {
        let key = crate::context::to_property_key(agent, &key)?;
        Ok(match key {
            PropertyKey::String(id) => Value::String(Handle::new(crux::string::lookup(id))),
            PropertyKey::Symbol(symbol) => Value::Symbol(Handle::new(symbol)),
        })
    })?;
    // spec 20.1.2.9 steps 2-4: OrdinaryObjectCreate(null), then one
    // CreateDataPropertyOrThrow per group.
    let obj = JsObject::ordinary_object_create(None);
    for (key, elements) in groups {
        let array = crate::builtins::array::array_from_values(agent, &elements)?;
        let key = match &key {
            Value::String(text) => PropertyKey::String(crux::intern(text.as_slice())),
            Value::Symbol(symbol) => PropertyKey::Symbol(symbol.as_ref().clone()),
            _ => continue,
        };
        obj.create_data_property_key(&key, array)?;
    }
    Ok(Value::Object(obj))
}

/// Object.assign (spec 20.1.2.1): copy the own enumerable properties of each
/// source onto the target, in source order.
fn object_assign(agent: &mut Agent, args: &[Value]) -> Result<Value, JsError> {
    let target = to_object(agent, arg(args, 0))?;
    for source_value in &args[1..] {
        if matches!(source_value, Value::Null | Value::Undefined) {
            continue;
        }
        let source = to_object(agent, source_value)?;
        let Value::Object(source_obj) = source.clone() else {
            continue;
        };
        let Value::Object(target_obj) = target.clone() else {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "assign target is not an object".into(),
            ));
        };
        for key in source_obj.own_property_keys()? {
            if let Some(prop) = source_obj.get_own_property_key(&key)?
                && prop.enumerable
            {
                let value = crate::context::get_property_key(agent, &source, &key, source.clone())?;
                target_obj.set_key(&key, value, true)?;
            }
        }
    }
    Ok(target)
}

/// Object.fromEntries (spec 20.1.2.7): walk an iterable of [key, value]
/// pairs and define each on a fresh object.
fn from_entries(agent: &mut Agent, iterable: &Value) -> Result<Value, JsError> {
    let iterator = crate::expr::get_iterator(agent, iterable)?;
    let realm = agent.current_realm()?;
    let proto = realm
        .intrinsics
        .get(OBJECT_PROTO)
        .and_then(|v| as_object(&v));
    let obj = JsObject::ordinary_object_create(proto);
    while let Some(entry) = crate::expr::iterator_step(agent, &iterator)? {
        // AddEntriesFromIterable (spec 10.1.4.3 step 4.d): a non-object
        // entry closes the iterator with a TypeError.
        if !matches!(entry, Value::Object(_) | Value::Function(_)) {
            let _ = crate::expr::iterator_close(agent, &iterator);
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Object.fromEntries requires object entries".into(),
            ));
        }
        // IfAbruptCloseIterator around the key read, the value read, and
        // the key coercion.
        let key = match crate::context::get_property(
            agent,
            &entry,
            &JsString::from_utf8("0"),
            entry.clone(),
        ) {
            Ok(key) => key,
            Err(error) => {
                let _ = crate::expr::iterator_close(agent, &iterator);
                return Err(error);
            }
        };
        let value = match crate::context::get_property(
            agent,
            &entry,
            &JsString::from_utf8("1"),
            entry.clone(),
        ) {
            Ok(value) => value,
            Err(error) => {
                let _ = crate::expr::iterator_close(agent, &iterator);
                return Err(error);
            }
        };
        let key = match crate::context::to_property_key(agent, &key) {
            Ok(key) => key,
            Err(error) => {
                let _ = crate::expr::iterator_close(agent, &iterator);
                return Err(error);
            }
        };
        obj.define_property_key(&key, &PropertyDescriptor::data(value))?;
    }
    Ok(Value::Object(obj))
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
    fn constructor_makes_fresh_objects_and_boxes() {
        // `Object()` and `Object(null/undefined)` make fresh objects; an
        // existing object passes through unchanged.
        assert_eq!(
            run("Object() instanceof Object").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("Object(null) instanceof Object").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("Object({ x: 1 }) === Object({ x: 1 })").unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            run("let o = {}; Object(o) === o").unwrap(),
            Value::Boolean(true)
        );
        // String boxing keeps the value reachable through the string object.
        assert_eq!(run("Object('abc').length").unwrap(), Value::Number(3.0));
    }

    #[test]
    fn object_literals_inherit_from_object_prototype() {
        assert_eq!(run("({}).toString()").unwrap(), str("[object Object]"));
        assert_eq!(
            run("({}).hasOwnProperty('x')").unwrap(),
            Value::Boolean(false)
        );
    }

    #[test]
    fn prototype_to_string_tags_primitives() {
        assert_eq!(
            run("Object.prototype.toString.call(null)").unwrap(),
            str("[object Null]")
        );
        assert_eq!(
            run("Object.prototype.toString.call(42)").unwrap(),
            str("[object Number]")
        );
        assert_eq!(
            run("Object.prototype.toString.call('s')").unwrap(),
            str("[object String]")
        );
        assert_eq!(
            run("Object.prototype.toString.call(true)").unwrap(),
            str("[object Boolean]")
        );
        assert_eq!(
            run("Object.prototype.toString.call(function () {})").unwrap(),
            str("[object Function]")
        );
        assert_eq!(
            run("Object.prototype.toString.call([])").unwrap(),
            str("[object Array]")
        );
    }

    #[test]
    fn keys_values_entries() {
        // Array.prototype methods arrive with Phase 12; inspect arrays by
        // index and length here.
        assert_eq!(
            run("Object.keys({ a: 1, b: 2 }).length").unwrap(),
            Value::Number(2.0)
        );
        assert_eq!(run("Object.keys({ a: 1, b: 2 })[1]").unwrap(), str("b"));
        assert_eq!(
            run("Object.values({ a: 1, b: 2 })[1]").unwrap(),
            Value::Number(2.0)
        );
        assert_eq!(
            run("Object.entries({ a: 1 })[0][0] + Object.entries({ a: 1 })[0][1]").unwrap(),
            str("a1")
        );
        // Non-enumerable and inherited properties are excluded.
        assert_eq!(
            run("Object.keys(Object.defineProperty({}, 'x', { value: 1, enumerable: false })).length")
                .unwrap(),
            Value::Number(0.0)
        );
    }

    #[test]
    fn get_own_property_operations() {
        assert_eq!(
            run("let o = { a: 1 }; \
                 let d = Object.getOwnPropertyDescriptor(o, 'a'); \
                 d.value + '|' + d.writable + '|' + d.enumerable + '|' + d.configurable")
            .unwrap(),
            str("1|true|true|true")
        );
        assert_eq!(
            run("Object.getOwnPropertyDescriptor({}, 'missing')").unwrap(),
            Value::Undefined
        );
        assert_eq!(
            run("Object.getOwnPropertyNames({ a: 1 }).length").unwrap(),
            Value::Number(1.0)
        );
        assert_eq!(
            run("Object.getOwnPropertySymbols({}).length").unwrap(),
            Value::Number(0.0)
        );
        assert_eq!(
            run("Object.hasOwn({ a: 1 }, 'a')").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("Object.hasOwn({}, 'toString')").unwrap(),
            Value::Boolean(false)
        );
    }

    #[test]
    fn define_property_and_define_properties() {
        assert_eq!(
            run("let o = {}; \
                 Object.defineProperty(o, 'x', { value: 9, writable: false }); \
                 o.x + '|' + o.hasOwnProperty('x')")
            .unwrap(),
            str("9|true")
        );
        assert_eq!(
            run("let o = {}; \
                 Object.defineProperties(o, { a: { value: 1 }, b: { value: 2 } }); \
                 o.a + o.b")
            .unwrap(),
            Value::Number(3.0)
        );
        // A non-writable property rejects writes in strict code.
        assert!(run("'use strict'; let o = {}; Object.defineProperty(o, 'x', { value: 1, writable: false }); o.x = 2").is_err());
    }

    #[test]
    fn create_and_prototypes() {
        assert_eq!(
            run("let proto = { greet: function () { return 'hi'; } }; \
                 let o = Object.create(proto); \
                 o.greet() + '|' + (Object.getPrototypeOf(o) === proto)")
            .unwrap(),
            str("hi|true")
        );
        assert_eq!(
            run("Object.getPrototypeOf({}) === Object.prototype").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("let o = Object.create(null); Object.getPrototypeOf(o) === null").unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn assign_copies_enumerable_properties() {
        assert_eq!(
            run("let t = { a: 0 }; Object.assign(t, { a: 1, b: 2 }); t.a + ',' + t.b").unwrap(),
            str("1,2")
        );
        assert_eq!(
            run("let t = { a: 0 }; Object.assign(t, null, { b: 3 }); t.a + t.b").unwrap(),
            Value::Number(3.0)
        );
    }

    #[test]
    fn object_is_uses_same_value() {
        assert_eq!(run("Object.is(1, 1)").unwrap(), Value::Boolean(true));
        assert_eq!(run("Object.is(0, -0)").unwrap(), Value::Boolean(false));
        assert_eq!(run("Object.is(NaN, NaN)").unwrap(), Value::Boolean(true));
        assert_eq!(run("Object.is('a', 'a')").unwrap(), Value::Boolean(true));
    }

    #[test]
    fn integrity_levels() {
        assert_eq!(
            run("Object.isExtensible({})").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("let o = {}; Object.preventExtensions(o); Object.isExtensible(o)").unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            run("let o = { a: 1 }; Object.freeze(o); Object.isFrozen(o) && !Object.isSealed(o)")
                .unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            run("let o = { a: 1 }; Object.freeze(o); Object.isSealed(o)").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("let o = { a: 1 }; Object.seal(o); Object.isSealed(o) && !Object.isFrozen(o)")
                .unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn prototype_methods() {
        assert_eq!(
            run("({ x: 1 }).hasOwnProperty('x')").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("({ x: 1 }).propertyIsEnumerable('x')").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run(
                "let p = { a: 1 }; let o = Object.create(p); p.isPrototypeOf(o) && !o.isPrototypeOf(p)"
            )
            .unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("({}).toLocaleString()").unwrap(),
            str("[object Object]")
        );
        // The __proto__ accessor reads and sets the prototype.
        assert_eq!(
            run("({}).__proto__ === Object.prototype").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("let o = {}; o.__proto__ = null; Object.getPrototypeOf(o) === null").unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn from_entries_builds_an_object() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let pairs = [("a", 1.0), ("b", 2.0)];
        let index = std::cell::Cell::new(0);
        let next = Function::create_builtin(
            Some(JsString::from_utf8("next")),
            0,
            Box::new(move |_, _| {
                let i = index.get();
                let result = JsObject::ordinary_object_create(None);
                if i < pairs.len() {
                    index.set(i + 1);
                    let pair = JsObject::array_create(None, 2.0)?;
                    pair.create_data_property(&JsString::from_utf8("0"), str(pairs[i].0))?;
                    pair.create_data_property(
                        &JsString::from_utf8("1"),
                        Value::Number(pairs[i].1),
                    )?;
                    result
                        .create_data_property(&JsString::from_utf8("value"), Value::Object(pair))?;
                    result.create_data_property(
                        &JsString::from_utf8("done"),
                        Value::Boolean(false),
                    )?;
                } else {
                    result
                        .create_data_property(&JsString::from_utf8("done"), Value::Boolean(true))?;
                }
                Ok(Value::Object(result))
            }),
            None,
            None,
        )
        .unwrap();
        let iterator = JsObject::ordinary_object_create(None);
        iterator
            .create_data_property(&JsString::from_utf8("next"), Value::Function(next))
            .unwrap();
        let iterable = JsObject::ordinary_object_create(None);
        let iterator_for_method = iterator.clone();
        iterable
            .define_property_key(
                &PropertyKey::Symbol(crux::symbol::well_known("iterator").as_ref().clone()),
                &crux::property::PropertyDescriptor::data(Value::Function(
                    Function::create_builtin(
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
        let global = agent.running_context().unwrap().realm.global_object.clone();
        global
            .create_data_property(&JsString::from_utf8("iter"), Value::Object(iterable))
            .unwrap();
        assert_eq!(
            agent
                .run_script("let o = Object.fromEntries(iter); o.a + ',' + o.b")
                .unwrap(),
            str("1,2")
        );
    }
}
