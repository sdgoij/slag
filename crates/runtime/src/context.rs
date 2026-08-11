//! Execution contexts (spec 9.4) and the abstract operations that resolve
//! bindings and `this` from the running context.

use std::cell::RefCell;

use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::string::JsString;
use crux::value::Value;

use crate::agent::Agent;
use crate::env::{EnvRecord, EnvRef};
use crate::realm::Realm;
use crate::script::ScriptRecord;

/// An execution context (spec 9.4 tables): the Function, Realm,
/// ScriptOrModule, LexicalEnvironment, VariableEnvironment, and
/// PrivateEnvironment components.
#[derive(Debug)]
pub struct ExecutionContext {
    pub function: Option<Value>,
    pub realm: Handle<Realm>,
    pub script_or_module: Option<Handle<ScriptRecord>>,
    pub lexical_environment: EnvRef,
    pub variable_environment: EnvRef,
    pub private_environment: Option<Handle<PrivateEnvironment>>,
}

/// A PrivateEnvironment Record (spec 9.2.1): the Private Names declared by
/// the nearest containing class. Used by class evaluation (Phase 7).
#[derive(Debug, Default)]
pub struct PrivateEnvironment {
    pub outer: Option<Handle<PrivateEnvironment>>,
    pub names: RefCell<Vec<PrivateName>>,
}

/// A Private Name (spec 9.2.1 table): a unique id plus its description.
#[derive(Debug, Clone)]
pub struct PrivateName {
    pub id: u64,
    pub description: JsString,
}

/// NewPrivateEnvironment (spec 9.2.1.1).
pub fn new_private_environment(
    outer: Option<Handle<PrivateEnvironment>>,
) -> Handle<PrivateEnvironment> {
    Handle::new(PrivateEnvironment {
        outer,
        names: RefCell::new(Vec::new()),
    })
}

/// ResolvePrivateIdentifier (spec 9.2.1.2): the Private Name with the given
/// description, searching outer private environments.
pub fn resolve_private_identifier(
    private_env: &Handle<PrivateEnvironment>,
    identifier: &JsString,
) -> Result<PrivateName, JsError> {
    let mut current: Option<&Handle<PrivateEnvironment>> = Some(private_env);
    while let Some(env) = current {
        if let Some(name) = env
            .names
            .borrow()
            .iter()
            .find(|n| &n.description == identifier)
        {
            return Ok(name.clone());
        }
        current = env.outer.as_ref();
    }
    Err(JsError::new(
        ErrorKind::SyntaxError,
        format!(
            "Private field {:?} is not declared",
            identifier.to_string_lossy()
        ),
    ))
}

/// The base of a Reference Record (spec 6.2.5): an Environment Record, an
/// object value (member-expression references), or the unresolvable
/// sentinel.
#[derive(Debug, Clone)]
pub enum ReferenceBase {
    Environment(EnvRef),
    /// A property reference: the receiver of the [[Get]]/[[Set]].
    Value(Value),
    Unresolvable,
}

/// A Reference Record (spec 6.2.5). The referenced name is a property key
/// (a String for identifier bindings, a String or Symbol for member
/// accesses).
#[derive(Debug, Clone)]
pub struct Reference {
    pub base: ReferenceBase,
    pub name: crux::property::PropertyKey,
    pub strict: bool,
    /// [[ThisValue]]: present only for `super` property references, so
    /// method calls through `super` receive the current `this` instead of
    /// the super base (spec 13.3.6.2).
    pub this_value: Option<Value>,
}

/// The property key of a reference.
pub fn reference_key(reference: &Reference) -> &crux::property::PropertyKey {
    &reference.name
}

/// The string name of a String-keyed reference (environment bindings are
/// always String-keyed).
fn string_name(key: &crux::property::PropertyKey) -> JsString {
    match key {
        crux::property::PropertyKey::String(id) => crux::lookup(*id),
        crux::property::PropertyKey::Symbol(_) => JsString::from_utf8(""),
    }
}

/// GetIdentifierReference (spec 9.2.7.1): walks the environment chain from
/// `env` looking for a binding of `name`.
pub fn get_identifier_reference(
    env: Option<EnvRef>,
    name: &JsString,
    strict: bool,
) -> Result<Reference, JsError> {
    let mut current = env;
    loop {
        let Some(env_record) = current else {
            return Ok(Reference {
                base: ReferenceBase::Unresolvable,
                name: crux::property::PropertyKey::from_js_string(name),
                strict,
                this_value: None,
            });
        };
        if env_record.has_binding(name)? {
            return Ok(Reference {
                base: ReferenceBase::Environment(env_record),
                name: crux::property::PropertyKey::from_js_string(name),
                strict,
                this_value: None,
            });
        }
        current = env_record.outer();
    }
}

/// spec 6.2.5.4 GetValue on a Reference Record. Accessor properties whose
/// getter is an ECMAScript function dispatch through the agent.
pub fn get_value(agent: &mut Agent, reference: &Reference) -> Result<Value, JsError> {
    match &reference.base {
        ReferenceBase::Environment(env) => {
            env.get_binding_value(&string_name(&reference.name), reference.strict)
        }
        ReferenceBase::Value(base) => get_property_key(agent, base, &reference.name, base.clone()),
        ReferenceBase::Unresolvable => Err(undefined_error(&string_name(&reference.name))),
    }
}

/// GetValue restricted to callable values (spec 7.3.12 GetValue): used by
/// call evaluation to reject non-callable callees.
pub fn get_value_callable(agent: &mut Agent, reference: &Reference) -> Result<Value, JsError> {
    let value = get_value(agent, reference)?;
    if crux::value::is_callable(&value) {
        Ok(value)
    } else {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{} is not a function", crux::value::type_of(&value)),
        ))
    }
}

/// `base.[[Get]](P, receiver)` through a language value.
pub fn get_property(
    agent: &mut Agent,
    base: &Value,
    key: &crux::string::JsString,
    receiver: Value,
) -> Result<Value, JsError> {
    get_property_key(
        agent,
        base,
        &crux::property::PropertyKey::from_js_string(key),
        receiver,
    )
}

/// `base.[[Get]](P, receiver)` for any property key (string or symbol).
pub fn get_property_key(
    agent: &mut Agent,
    base: &Value,
    key: &crux::property::PropertyKey,
    receiver: Value,
) -> Result<Value, JsError> {
    match base {
        Value::Object(obj) => {
            // Accessors whose getter is an ECMAScript function cannot be
            // invoked by the crux layer (the body lives in the agent);
            // dispatch them through the evaluator (spec 8.12.2 step 6.b).
            if let Some(getter) = find_ecma_accessor(obj, key, AccessorKind::Get)? {
                return crate::function::call(agent, &getter, receiver, &[]);
            }
            obj.get_with_receiver_key(key, receiver)
        }
        Value::Function(f) => {
            if let Some(getter) = find_ecma_accessor(&f.object, key, AccessorKind::Get)? {
                return crate::function::call(agent, &getter, receiver, &[]);
            }
            f.object.get_with_receiver_key(key, receiver)
        }
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "value is not an object".into(),
        )),
    }
}

/// Which half of an accessor property the runtime dispatches for.
#[derive(Clone, Copy)]
enum AccessorKind {
    Get,
    Set,
}

/// The first accessor on the prototype chain for `key` whose getter/setter
/// is an ECMAScript function. `None` when the chain holds no such accessor,
/// so the crux [[Get]]/[[Set]] handles data and builtin-accessor properties.
fn find_ecma_accessor(
    object: &crux::object::JsObject,
    key: &crux::property::PropertyKey,
    which: AccessorKind,
) -> Result<Option<Value>, JsError> {
    // The starting object may be an owned JsObject (a Function's object
    // part); prototype links are Handles.
    let mut prototype: Option<Handle<crux::object::JsObject>> = None;
    loop {
        let obj = match &prototype {
            None => object,
            Some(handle) => handle,
        };
        if let Some(property) = obj.get_own_property_key(key)? {
            let crux::object::PropertyKind::Accessor { get, set } = property.kind else {
                return Ok(None);
            };
            let accessor = match which {
                AccessorKind::Get => get,
                AccessorKind::Set => set,
            };
            let Some(accessor) = accessor else {
                return Ok(None);
            };
            if let Value::Function(function) = &accessor
                && matches!(&function.kind, crux::function::FunctionKind::EcmaScript)
            {
                return Ok(Some(accessor));
            }
            return Ok(None);
        }
        match obj.get_prototype_of()? {
            Some(proto) => prototype = Some(proto),
            None => return Ok(None),
        }
    }
}

/// spec 6.2.5.6 PutValue: strict mode throws on unresolvable references and
/// failed object writes; sloppy mode creates a global property instead.
/// Accessor properties with ECMAScript-function setters dispatch through the
/// evaluator.
pub fn put_value(agent: &mut Agent, reference: &Reference, value: Value) -> Result<(), JsError> {
    match &reference.base {
        ReferenceBase::Environment(env) => {
            env.set_mutable_binding(&string_name(&reference.name), value, reference.strict)
        }
        ReferenceBase::Value(base) => {
            let key = &reference.name;
            let result = match base {
                Value::Object(obj) => {
                    if let Some(setter) = find_ecma_accessor(obj, key, AccessorKind::Set)? {
                        crate::function::call(agent, &setter, base.clone(), &[value])?;
                        return Ok(());
                    }
                    obj.set_with_receiver_key(key, value, base.clone(), reference.strict)
                }
                Value::Function(f) => {
                    if let Some(setter) = find_ecma_accessor(&f.object, key, AccessorKind::Set)? {
                        crate::function::call(agent, &setter, base.clone(), &[value])?;
                        return Ok(());
                    }
                    f.object
                        .set_with_receiver_key(key, value, base.clone(), reference.strict)
                }
                _ => Err(JsError::new(
                    ErrorKind::TypeError,
                    "value is not an object".into(),
                )),
            };
            // spec step 5: a failed [[Set]] is a TypeError in strict mode.
            if !result? && reference.strict {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    format!(
                        "Cannot assign to read only property {:?}",
                        key.display_string()
                    ),
                ));
            }
            Ok(())
        }
        ReferenceBase::Unresolvable => {
            if reference.strict {
                return Err(undefined_error(&string_name(&reference.name)));
            }
            // Sloppy: Set on the global object (spec 6.2.5.6 step 3.a.ii).
            let global_env = agent.running_context()?.realm.global_env();
            global_env.set_mutable_binding(&string_name(&reference.name), value, false)
        }
    }
}

/// spec 7.3.8 DeletePropertyOrThrow.
pub fn delete_property_or_throw(reference: &Reference) -> Result<bool, JsError> {
    match &reference.base {
        ReferenceBase::Environment(env) => env.delete_binding(&string_name(&reference.name)),
        ReferenceBase::Value(base) => {
            let key = &reference.name;
            let deleted = match base {
                Value::Object(obj) => obj.delete_key(key)?,
                Value::Function(f) => f.object.delete_key(key)?,
                _ => true,
            };
            if !deleted && reference.strict {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    format!("Cannot delete property {:?}", key.display_string()),
                ));
            }
            Ok(deleted)
        }
        ReferenceBase::Unresolvable => Ok(true),
    }
}

/// spec 6.2.5.8 InitializeReferencedBinding.
pub fn initialize_referenced_binding(reference: &Reference, value: Value) -> Result<(), JsError> {
    match &reference.base {
        ReferenceBase::Environment(env) => {
            env.initialize_binding(&string_name(&reference.name), value)
        }
        ReferenceBase::Value(_) => Err(JsError::new(
            ErrorKind::ReferenceError,
            format!(
                "Cannot initialize property reference {:?}",
                reference.name.display_string()
            ),
        )),
        ReferenceBase::Unresolvable => Err(undefined_error(&string_name(&reference.name))),
    }
}

/// spec 6.2.5.10 GetThisValue: the base value of a property reference, or
/// the [[ThisValue]] carried by `super` references.
pub fn get_this_value(reference: &Reference) -> Value {
    if let Some(this) = &reference.this_value {
        return this.clone();
    }
    match &reference.base {
        ReferenceBase::Value(base) => base.clone(),
        _ => Value::Undefined,
    }
}

fn undefined_error(name: &JsString) -> JsError {
    JsError::new(
        ErrorKind::ReferenceError,
        format!("{:?} is not defined", name.to_string_lossy()),
    )
}

/// spec 9.4.1 GetActiveScriptOrModule: the topmost execution context whose
/// ScriptOrModule is not null, if any.
pub fn get_active_script_or_module(agent: &Agent) -> Option<Handle<ScriptRecord>> {
    agent
        .execution_context_stack
        .iter()
        .rev()
        .find_map(|context| context.script_or_module.clone())
}

/// spec 9.4.2 ResolveBinding.
pub fn resolve_binding(agent: &Agent, name: &JsString, strict: bool) -> Result<Reference, JsError> {
    let env = agent.running_context()?.lexical_environment.clone();
    get_identifier_reference(Some(env), name, strict)
}

/// spec 9.4.3 GetThisEnvironment: the innermost environment with a `this`
/// binding.
pub fn get_this_environment(agent: &Agent) -> Result<EnvRef, JsError> {
    let mut env = agent.running_context()?.lexical_environment.clone();
    loop {
        if env.has_this_binding() {
            return Ok(env);
        }
        let Some(outer) = env.outer() else {
            return Err(JsError::new(
                ErrorKind::ReferenceError,
                "No this binding".into(),
            ));
        };
        env = outer;
    }
}

/// spec 9.4.4 ResolveThisBinding.
pub fn resolve_this_binding(agent: &Agent) -> Result<Value, JsError> {
    let env = get_this_environment(agent)?;
    env.get_this_binding()
}

/// spec 9.4.5 GetNewTarget.
pub fn get_new_target(agent: &Agent) -> Result<Value, JsError> {
    let env = get_this_environment(agent)?;
    env.get_new_target()
}

/// spec 9.4.6 GetGlobalObject.
pub fn get_global_object(agent: &Agent) -> Result<Handle<crux::object::JsObject>, JsError> {
    Ok(agent.running_context()?.realm.global_object.clone())
}

/// GetSuperConstructor (spec 9.2.4.6): the heritage constructor of the
/// current derived constructor, used by `super()` calls.
pub fn get_super_constructor(agent: &Agent) -> Result<Value, JsError> {
    let env = get_this_environment(agent)?;
    let EnvRecord::Function(function_env) = &*env else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "super() is only valid inside a derived constructor".into(),
        ));
    };
    let Value::Function(function) = &function_env.function_object else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "super() is only valid inside a derived constructor".into(),
        ));
    };
    agent
        .ecma_functions
        .get(&function.id())
        .and_then(|data| data.super_constructor.clone())
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "super() is only valid inside a derived constructor".into(),
            )
        })
}

/// GetSuperBase (spec 9.2.4.5): the prototype of the nearest method's
/// [[HomeObject]]. `get_this_environment` skips arrow environments, so
/// arrows inside a method share its HomeObject.
pub fn get_super_base(agent: &Agent) -> Result<Value, JsError> {
    let env = get_this_environment(agent)?;
    if !env.has_super_binding(agent) {
        return Err(JsError::new(
            ErrorKind::ReferenceError,
            "super is only valid inside methods".into(),
        ));
    }
    let EnvRecord::Function(function_env) = &*env else {
        return Err(JsError::new(
            ErrorKind::ReferenceError,
            "super is only valid inside methods".into(),
        ));
    };
    let Value::Function(function) = &function_env.function_object else {
        return Ok(Value::Undefined);
    };
    let Some(home) = agent
        .ecma_functions
        .get(&function.id())
        .and_then(|data| data.home_object.clone())
    else {
        return Ok(Value::Undefined);
    };
    let Value::Object(home_object) = home else {
        return Ok(Value::Undefined);
    };
    Ok(home_object
        .get_prototype_of()?
        .map(Value::Object)
        .unwrap_or(Value::Undefined))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::env::new_declarative_environment;
    use crate::realm::initialize_host_defined_realm;

    fn name(text: &str) -> JsString {
        JsString::from_utf8(text)
    }

    #[test]
    fn identifier_reference_walks_the_chain() {
        let mut agent = Agent::new();
        let outer = new_declarative_environment(None);
        outer.create_mutable_binding(&name("a"), false).unwrap();
        outer
            .initialize_binding(&name("a"), Value::Number(1.0))
            .unwrap();
        let inner = new_declarative_environment(Some(outer));
        inner.create_mutable_binding(&name("b"), false).unwrap();
        inner
            .initialize_binding(&name("b"), Value::Number(2.0))
            .unwrap();

        let a = get_identifier_reference(Some(inner.clone()), &name("a"), true).unwrap();
        assert!(matches!(a.base, ReferenceBase::Environment(_)));
        assert_eq!(get_value(&mut agent, &a).unwrap(), Value::Number(1.0));

        let b = get_identifier_reference(Some(inner), &name("b"), true).unwrap();
        assert_eq!(get_value(&mut agent, &b).unwrap(), Value::Number(2.0));

        let missing = get_identifier_reference(None, &name("nope"), true).unwrap();
        assert!(matches!(missing.base, ReferenceBase::Unresolvable));
        assert!(get_value(&mut agent, &missing).is_err());
    }

    #[test]
    fn put_value_on_unresolvable_obeys_strictness() {
        let mut agent = Agent::new();
        let realm = initialize_host_defined_realm(&agent).unwrap();
        agent.push_bootstrap_context(realm);

        let sloppy = get_identifier_reference(None, &name("x"), false).unwrap();
        put_value(&mut agent, &sloppy, Value::Number(5.0)).unwrap();
        // The sloppy write created a property on the global object.
        let global = agent.running_context().unwrap().realm.global_object.clone();
        assert_eq!(global.get(&name("x")).unwrap(), Value::Number(5.0));

        let strict = get_identifier_reference(None, &name("y"), true).unwrap();
        assert!(put_value(&mut agent, &strict, Value::Number(6.0)).is_err());
    }

    #[test]
    fn this_and_global_object_resolve_from_the_bootstrap_context() {
        let mut agent = Agent::new();
        let realm = initialize_host_defined_realm(&agent).unwrap();
        agent.push_bootstrap_context(realm);
        let this = resolve_this_binding(&agent).unwrap();
        let global = get_global_object(&agent).unwrap();
        assert_eq!(this, Value::Object(global));
        assert!(get_active_script_or_module(&agent).is_none());
    }

    #[test]
    fn private_identifier_resolution() {
        let inner = new_private_environment(None);
        inner.names.borrow_mut().push(PrivateName {
            id: 1,
            description: name("#x"),
        });
        let resolved = resolve_private_identifier(&inner, &name("#x")).unwrap();
        assert_eq!(resolved.id, 1);
        assert!(resolve_private_identifier(&inner, &name("#y")).is_err());

        // Outer environments are searched too.
        let outer = new_private_environment(None);
        outer.names.borrow_mut().push(PrivateName {
            id: 2,
            description: name("#z"),
        });
        let nested = new_private_environment(Some(outer));
        assert_eq!(
            resolve_private_identifier(&nested, &name("#z")).unwrap().id,
            2
        );
    }
}
