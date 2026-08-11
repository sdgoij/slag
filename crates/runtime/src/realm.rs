//! Realms (spec 9.3): Realm Records, the intrinsic registry, and the
//! bootstrap pipeline (CreateIntrinsics, NewGlobalEnvironment,
//! SetDefaultGlobalBindings).

use std::cell::RefCell;
use std::collections::HashMap;

use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::PropertyDescriptor;
use crux::string::JsString;
use crux::value::Value;

use crate::agent::Agent;
use crate::env::{EnvRef, new_global_environment};

/// A Realm Record (spec 9.3 table): the intrinsic registry, the global
/// object, and the global environment.
#[derive(Debug)]
pub struct Realm {
    pub agent_signifier: u64,
    pub intrinsics: Intrinsics,
    pub global_object: Handle<JsObject>,
    pub global_env: EnvRef,
}

impl Realm {
    pub fn global_env(&self) -> EnvRef {
        self.global_env.clone()
    }
}

/// The intrinsic registry (spec 9.3.1): %-named values installed by each
/// built-in phase, in spec bootstrap order.
#[derive(Debug, Default)]
pub struct Intrinsics {
    entries: RefCell<HashMap<JsString, Value>>,
}

impl Intrinsics {
    pub fn get(&self, name: &str) -> Option<Value> {
        self.entries
            .borrow()
            .get(&JsString::from_utf8(name))
            .cloned()
    }

    pub fn define(&self, name: &str, value: Value) {
        self.entries
            .borrow_mut()
            .insert(JsString::from_utf8(name), value);
    }

    pub fn is_empty(&self) -> bool {
        self.entries.borrow().is_empty()
    }
}

fn as_object(value: &Value) -> Option<Handle<JsObject>> {
    match value {
        Value::Object(obj) => Some(obj.clone()),
        _ => None,
    }
}

/// InitializeHostDefinedRealm (spec 9.3.4): CreateIntrinsics, the global
/// object, NewGlobalEnvironment, and SetDefaultGlobalBindings. The caller
/// pushes the bootstrap execution context.
pub fn initialize_host_defined_realm(agent: &Agent) -> Result<Handle<Realm>, JsError> {
    let intrinsics = Intrinsics::default();
    // The global object's prototype is %Object.prototype% once the intrinsic
    // table is populated (Phase 5+); until then it is null.
    let global = JsObject::ordinary_object_create(
        intrinsics
            .get("%Object.prototype%")
            .and_then(|v| as_object(&v)),
    );
    let global_env = new_global_environment(global.clone(), global.clone());
    let realm = Handle::new(Realm {
        agent_signifier: agent.signifier,
        intrinsics,
        global_object: global.clone(),
        global_env,
    });
    set_default_global_bindings(&realm)?;
    Ok(realm)
}

/// SetDefaultGlobalBindings (spec 9.3.5). Phase 4 installs the global
/// object's value properties (spec sec-value-properties-of-the-global-object);
/// the function and constructor properties arrive with their built-ins
/// (Phase 8+).
fn set_default_global_bindings(realm: &Handle<Realm>) -> Result<(), JsError> {
    let global = &realm.global_object;
    global.define_property_or_throw(
        &JsString::from_utf8("globalThis"),
        &PropertyDescriptor {
            value: Some(Value::Object(global.clone())),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    for (name, value) in [
        ("Infinity", Value::Number(f64::INFINITY)),
        ("NaN", Value::Number(f64::NAN)),
        ("undefined", Value::Undefined),
    ] {
        global.define_property_or_throw(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: Some(value),
                writable: Some(false),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(false),
            },
        )?;
    }
    // %eval% (spec 19.2.1): a global whose identity the call evaluator
    // recognizes to perform direct and indirect eval (sec 13.3.6.1). Its
    // native body is a placeholder; dispatch happens before it runs.
    let eval_func = Function::create_builtin(
        Some(JsString::from_utf8("eval")),
        1,
        Box::new(|_, _| {
            Err(JsError::new(
                ErrorKind::TypeError,
                "eval must be called through the evaluator".into(),
            ))
        }),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define("%eval%", Value::Function(eval_func.clone()));
    global.define_property_or_throw(
        &JsString::from_utf8("eval"),
        &PropertyDescriptor {
            value: Some(Value::Function(eval_func)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    crate::builtins::function::install(realm)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;

    #[test]
    fn intrinsics_store_and_lookup_percent_names() {
        let intrinsics = Intrinsics::default();
        assert!(intrinsics.is_empty());
        assert!(intrinsics.get("%Object.prototype%").is_none());
        intrinsics.define("%Object.prototype%", Value::Undefined);
        assert!(!intrinsics.is_empty());
        assert_eq!(intrinsics.get("%Object.prototype%"), Some(Value::Undefined));
    }

    #[test]
    fn host_defined_realm_installs_value_properties() {
        let agent = Agent::new();
        let realm = initialize_host_defined_realm(&agent).unwrap();
        let global = &realm.global_object;
        // globalThis points back at the global object...
        assert_eq!(
            global.get(&JsString::from_utf8("globalThis")).unwrap(),
            Value::Object(global.clone())
        );
        // ...and the non-configurable value properties exist.
        assert_eq!(
            global.get(&JsString::from_utf8("Infinity")).unwrap(),
            Value::Number(f64::INFINITY)
        );
        assert!(matches!(
            global.get(&JsString::from_utf8("NaN")).unwrap(),
            Value::Number(n) if n.is_nan()
        ));
        assert_eq!(
            global.get(&JsString::from_utf8("undefined")).unwrap(),
            Value::Undefined
        );
        let infinity = global
            .get_own_property(&JsString::from_utf8("Infinity"))
            .unwrap()
            .unwrap();
        assert_eq!(infinity.writable(), Some(false));
        assert!(!infinity.enumerable && !infinity.configurable);
        // The global environment is reachable and supplies `this`.
        assert_eq!(
            realm.global_env.get_this_binding().unwrap(),
            Value::Object(global.clone())
        );
        // Binding an identifier through the global env works end to end.
        realm
            .global_env
            .create_mutable_binding(&JsString::from_utf8("x"), false)
            .unwrap();
        realm
            .global_env
            .initialize_binding(&JsString::from_utf8("x"), Value::Number(42.0))
            .unwrap();
        assert_eq!(
            realm
                .global_env
                .get_binding_value(&JsString::from_utf8("x"), true)
                .unwrap(),
            Value::Number(42.0)
        );
    }
}
