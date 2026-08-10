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

/// The base of a Reference Record (spec 6.2.5): an Environment Record or the
/// unresolvable sentinel. Object bases join with member evaluation (Phase 6).
#[derive(Debug, Clone)]
pub enum ReferenceBase {
    Environment(EnvRef),
    Unresolvable,
}

/// A Reference Record (spec 6.2.5) for identifier references.
#[derive(Debug, Clone)]
pub struct Reference {
    pub base: ReferenceBase,
    pub name: JsString,
    pub strict: bool,
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
                name: name.clone(),
                strict,
            });
        };
        if env_record.has_binding(name)? {
            return Ok(Reference {
                base: ReferenceBase::Environment(env_record),
                name: name.clone(),
                strict,
            });
        }
        current = env_record.outer();
    }
}

/// spec 6.2.5.4 GetValue on a Reference Record.
pub fn get_value(reference: &Reference) -> Result<Value, JsError> {
    match &reference.base {
        ReferenceBase::Environment(env) => env.get_binding_value(&reference.name, reference.strict),
        ReferenceBase::Unresolvable => Err(undefined_error(&reference.name)),
    }
}

/// spec 6.2.5.8 InitializeReferencedBinding.
pub fn initialize_referenced_binding(reference: &Reference, value: Value) -> Result<(), JsError> {
    match &reference.base {
        ReferenceBase::Environment(env) => env.initialize_binding(&reference.name, value),
        ReferenceBase::Unresolvable => Err(JsError::new(
            ErrorKind::ReferenceError,
            format!("{:?} is not defined", reference.name.to_string_lossy()),
        )),
    }
}

/// spec 6.2.5.6 PutValue: strict mode throws on unresolvable references;
/// sloppy mode creates a global property instead.
pub fn put_value(agent: &Agent, reference: &Reference, value: Value) -> Result<(), JsError> {
    match &reference.base {
        ReferenceBase::Environment(env) => {
            env.set_mutable_binding(&reference.name, value, reference.strict)
        }
        ReferenceBase::Unresolvable => {
            if reference.strict {
                return Err(undefined_error(&reference.name));
            }
            // Sloppy: Set on the global object (spec 6.2.5.6 step 3.a.ii).
            let global_env = agent.running_context()?.realm.global_env();
            global_env.set_mutable_binding(&reference.name, value, false)
        }
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

/// GetSuperBase (spec 9.2.4.5): the base for `super` property accesses.
/// Phase 4 function values have no [[HomeObject]], so this is *undefined*.
pub fn get_super_base(agent: &Agent) -> Result<Value, JsError> {
    let env = get_this_environment(agent)?;
    match &*env {
        EnvRecord::Function(_) => Ok(Value::Undefined),
        _ => Err(JsError::new(
            ErrorKind::ReferenceError,
            "super is only valid inside methods".into(),
        )),
    }
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
        assert_eq!(get_value(&a).unwrap(), Value::Number(1.0));

        let b = get_identifier_reference(Some(inner), &name("b"), true).unwrap();
        assert_eq!(get_value(&b).unwrap(), Value::Number(2.0));

        let missing = get_identifier_reference(None, &name("nope"), true).unwrap();
        assert!(matches!(missing.base, ReferenceBase::Unresolvable));
        assert!(get_value(&missing).is_err());
    }

    #[test]
    fn put_value_on_unresolvable_obeys_strictness() {
        let mut agent = Agent::new();
        let realm = initialize_host_defined_realm(&agent).unwrap();
        agent.push_bootstrap_context(realm);

        let sloppy = get_identifier_reference(None, &name("x"), false).unwrap();
        put_value(&agent, &sloppy, Value::Number(5.0)).unwrap();
        // The sloppy write created a property on the global object.
        let global = agent.running_context().unwrap().realm.global_object.clone();
        assert_eq!(global.get(&name("x")).unwrap(), Value::Number(5.0));

        let strict = get_identifier_reference(None, &name("y"), true).unwrap();
        assert!(put_value(&agent, &strict, Value::Number(6.0)).is_err());
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
