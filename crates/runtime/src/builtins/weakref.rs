//! WeakRef and FinalizationRegistry (spec 26.1, 26.2): the full API surface
//! with documented no-GC semantics — without a collector (PLAN §4.3),
//! WeakRef targets never die (so `deref()` always returns the target) and
//! FinalizationRegistry cells never clear, so `HostEnqueueFinalizationRegistryCleanupJob`
//! never fires. Bodies are placeholders; `runtime::function::call`/`construct`
//! dispatch by intrinsic identity (the %eval% pattern).

use std::cell::RefCell;
use std::rc::Rc;

use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::ops::same_value;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, is_callable};

use crate::agent::Agent;
use crate::context::as_object;
use crate::realm::Realm;

const WEAK_REF: &str = "%WeakRef%";
const WEAK_REF_PROTO: &str = "%WeakRef.prototype%";
const WEAK_REF_DEREF: &str = "%WeakRef.prototype.deref%";
const FINALIZATION_REGISTRY: &str = "%FinalizationRegistry%";
const FINALIZATION_REGISTRY_PROTO: &str = "%FinalizationRegistry.prototype%";
const FR_REGISTER: &str = "%FinalizationRegistry.prototype.register%";
const FR_UNREGISTER: &str = "%FinalizationRegistry.prototype.unregister%";

/// One FinalizationRegistry cell (spec 26.2.1.2): the weakly-held target's
/// identity, the held value, and the optional unregister token's identity.
#[derive(Debug, Clone)]
pub struct FrCell {
    pub target_id: u64,
    pub held_value: Value,
    pub unregister_token: Option<u64>,
}

/// [[Cells]] and [[CleanupCallback]] of a FinalizationRegistry instance.
#[derive(Debug)]
pub struct FinalizationData {
    pub callback: Value,
    pub cells: Vec<FrCell>,
}

fn placeholder(name: &'static str) -> NativeFn {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

/// The object handle of a target, or an error for non-weakly-holdable values.
fn weakly_holdable(value: &Value) -> Result<Handle<JsObject>, JsError> {
    match value {
        Value::Object(obj) => Ok(obj.clone()),
        Value::Function(function) => function.object.handle().ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "cannot hold a function without an object part".into(),
            )
        }),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "WeakRef and FinalizationRegistry targets must be objects".into(),
        )),
    }
}

/// Install WeakRef and FinalizationRegistry (spec 26.1, 26.2) during
/// SetDefaultGlobalBindings.
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));

    // %WeakRef% (26.1.1) and %WeakRef.prototype% (26.1.3).
    let weak_ref_proto = JsObject::ordinary_object_create(object_proto.clone());
    let weak_ref_proto_value = Value::Object(weak_ref_proto.clone());
    let weak_ref_ctor = Function::create_builtin(
        Some(JsString::from_utf8("WeakRef")),
        1,
        Box::new(placeholder("WeakRef")),
        Some(Box::new(placeholder("WeakRef"))),
        None,
    )?;
    let weak_ref_ctor_value = Value::Function(weak_ref_ctor.clone());
    realm
        .intrinsics
        .define(WEAK_REF, weak_ref_ctor_value.clone());
    realm
        .intrinsics
        .define(WEAK_REF_PROTO, weak_ref_proto_value.clone());
    weak_ref_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(weak_ref_proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    weak_ref_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(weak_ref_ctor_value.clone()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    let deref = Function::create_builtin(
        Some(JsString::from_utf8("deref")),
        0,
        Box::new(placeholder("WeakRef.prototype.deref")),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(WEAK_REF_DEREF, Value::Function(deref.clone()));
    weak_ref_proto.define_property(
        &JsString::from_utf8("deref"),
        &PropertyDescriptor {
            value: Some(Value::Function(deref)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    weak_ref_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor::none(Value::String(Handle::new(JsString::from_utf8("WeakRef")))),
    )?;

    // %FinalizationRegistry% (26.2.1) and %FinalizationRegistry.prototype%
    // (26.2.3).
    let fr_proto = JsObject::ordinary_object_create(object_proto);
    let fr_proto_value = Value::Object(fr_proto.clone());
    let fr_ctor = Function::create_builtin(
        Some(JsString::from_utf8("FinalizationRegistry")),
        1,
        Box::new(placeholder("FinalizationRegistry")),
        Some(Box::new(placeholder("FinalizationRegistry"))),
        None,
    )?;
    let fr_ctor_value = Value::Function(fr_ctor.clone());
    realm
        .intrinsics
        .define(FINALIZATION_REGISTRY, fr_ctor_value.clone());
    realm
        .intrinsics
        .define(FINALIZATION_REGISTRY_PROTO, fr_proto_value.clone());
    fr_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(fr_proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    fr_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(fr_ctor_value.clone()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    for (name, length, key) in [
        ("register", 3, FR_REGISTER),
        ("unregister", 1, FR_UNREGISTER),
    ] {
        let method = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            Box::new(placeholder(key)),
            None,
            None,
        )?;
        realm
            .intrinsics
            .define(key, Value::Function(method.clone()));
        fr_proto.define_property(
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
    fr_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor::none(Value::String(Handle::new(JsString::from_utf8(
            "FinalizationRegistry",
        )))),
    )?;

    for (name, value) in [
        ("WeakRef", weak_ref_ctor_value),
        ("FinalizationRegistry", fr_ctor_value),
    ] {
        realm.global_object.define_property_or_throw(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: Some(value),
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

/// GetPrototypeFromConstructor (spec 10.1.14): `newTarget.prototype`.
fn instance_proto(agent: &mut Agent, new_target: &Value) -> Result<Handle<JsObject>, JsError> {
    let proto = crate::context::get_property(
        agent,
        new_target,
        &JsString::from_utf8("prototype"),
        new_target.clone(),
    )?;
    as_object(&proto).ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "Prototype must be an object or null".into(),
        )
    })
}

pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    // Both constructors reject bare calls (spec 26.1.1.1 / 26.2.1.1).
    if intrinsics.get(WEAK_REF).as_ref() == Some(callee)
        || intrinsics.get(FINALIZATION_REGISTRY).as_ref() == Some(callee)
    {
        return Some(Err(JsError::new(
            ErrorKind::TypeError,
            "must be called with new".into(),
        )));
    }
    if intrinsics.get(WEAK_REF_DEREF).as_ref() == Some(callee) {
        return Some((|| {
            let Value::Object(obj) = this else {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "WeakRef.prototype.deref requires a WeakRef".into(),
                ));
            };
            Ok(agent
                .weak_ref_targets
                .get(&obj.id())
                .cloned()
                .unwrap_or(Value::Undefined))
        })());
    }
    if intrinsics.get(FR_REGISTER).as_ref() == Some(callee) {
        return Some(finalization_register(agent, this, args));
    }
    if intrinsics.get(FR_UNREGISTER).as_ref() == Some(callee) {
        return Some(finalization_unregister(agent, this, args));
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
    if intrinsics.get(WEAK_REF).as_ref() == Some(callee) {
        return Some((|| {
            let target = args.first().cloned().unwrap_or(Value::Undefined);
            weakly_holdable(&target)?;
            let proto = instance_proto(agent, new_target)?;
            let object = JsObject::ordinary_object_create(Some(proto));
            agent.weak_ref_targets.insert(object.id(), target);
            Ok(Value::Object(object))
        })());
    }
    if intrinsics.get(FINALIZATION_REGISTRY).as_ref() == Some(callee) {
        return Some((|| {
            let callback = args.first().cloned().unwrap_or(Value::Undefined);
            if !is_callable(&callback) {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "FinalizationRegistry requires a callable callback".into(),
                ));
            }
            let proto = instance_proto(agent, new_target)?;
            let object = JsObject::ordinary_object_create(Some(proto));
            agent.finalization_registries.insert(
                object.id(),
                Rc::new(RefCell::new(FinalizationData {
                    callback,
                    cells: Vec::new(),
                })),
            );
            Ok(Value::Object(object))
        })());
    }
    None
}

/// FinalizationRegistry.prototype.register (spec 26.2.3.2).
fn finalization_register(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let Value::Object(obj) = this else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "FinalizationRegistry.prototype.register requires a registry".into(),
        ));
    };
    let Some(data) = agent.finalization_registries.get(&obj.id()).cloned() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "FinalizationRegistry.prototype.register requires a registry".into(),
        ));
    };
    let target = args.first().cloned().unwrap_or(Value::Undefined);
    let target_id = weakly_holdable(&target)?.id();
    let held_value = args.get(1).cloned().unwrap_or(Value::Undefined);
    if same_value(&target, &held_value) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "heldValue must not be the target".into(),
        ));
    }
    let token = args.get(2).cloned().unwrap_or(Value::Undefined);
    let token_id = match token {
        Value::Undefined => None,
        _ => Some(weakly_holdable(&token)?.id()),
    };
    data.borrow_mut().cells.push(FrCell {
        target_id,
        held_value,
        unregister_token: token_id,
    });
    Ok(Value::Undefined)
}

/// FinalizationRegistry.prototype.unregister (spec 26.2.3.3).
fn finalization_unregister(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let Value::Object(obj) = this else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "FinalizationRegistry.prototype.unregister requires a registry".into(),
        ));
    };
    let Some(data) = agent.finalization_registries.get(&obj.id()).cloned() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "FinalizationRegistry.prototype.unregister requires a registry".into(),
        ));
    };
    let token = args.first().cloned().unwrap_or(Value::Undefined);
    let token_id = weakly_holdable(&token)?.id();
    let mut data = data.borrow_mut();
    let before = data.cells.len();
    data.cells
        .retain(|cell| cell.unregister_token != Some(token_id));
    Ok(Value::Boolean(data.cells.len() != before))
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
    fn weak_ref_holds_its_target() {
        // No GC: deref always returns the target.
        assert_eq!(
            run("let t = {}; let w = new WeakRef(t); w.deref() === t").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("new WeakRef({}).deref() instanceof Object").unwrap(),
            Value::Boolean(true)
        );
        // Non-object targets throw.
        assert!(run("new WeakRef(42)").is_err());
        assert!(run("new WeakRef('s')").is_err());
        // Bare calls throw.
        assert!(run("WeakRef({})").is_err());
    }

    #[test]
    fn finalization_registry_register_and_unregister() {
        assert_eq!(
            run("let fr = new FinalizationRegistry(function () {}); \
                 let t = {}; let token = {}; \
                 fr.register(t, 'held', token); fr.unregister(token)")
            .unwrap(),
            Value::Boolean(true)
        );
        // Unregistering twice reports nothing was removed.
        assert_eq!(
            run("let fr = new FinalizationRegistry(function () {}); \
                 let t = {}; let token = {}; \
                 fr.register(t, 'held', token); fr.unregister(token); fr.unregister(token)")
            .unwrap(),
            Value::Boolean(false)
        );
        // register without a token leaves no token to unregister with.
        assert_eq!(
            run("let fr = new FinalizationRegistry(function () {}); \
                 let t = {}; fr.register(t, 'held'); fr.unregister(t)")
            .unwrap(),
            Value::Boolean(false)
        );
    }

    #[test]
    fn finalization_registry_validations() {
        // The callback must be callable.
        assert!(run("new FinalizationRegistry({})").is_err());
        // The target must be an object.
        assert!(
            run("let fr = new FinalizationRegistry(function () {}); fr.register(1, 'x')").is_err()
        );
        // heldValue must differ from the target.
        assert!(
            run("let fr = new FinalizationRegistry(function () {}); let t = {}; fr.register(t, t)")
                .is_err()
        );
        // Bare calls throw.
        assert!(run("FinalizationRegistry(function () {})").is_err());
    }

    #[test]
    fn prototype_surface() {
        assert_eq!(
            run("new WeakRef({}) instanceof WeakRef").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("new FinalizationRegistry(function () {}) instanceof FinalizationRegistry")
                .unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("WeakRef.prototype.deref.length").unwrap(),
            Value::Number(0.0)
        );
        assert_eq!(
            run("FinalizationRegistry.prototype.register.length").unwrap(),
            Value::Number(3.0)
        );
        assert_eq!(
            run("Object.prototype.toString.call(new WeakRef({}))").unwrap(),
            str("[object WeakRef]")
        );
    }
}
