//! Execution contexts (spec 9.4) and the abstract operations that resolve
//! bindings and `this` from the running context.

use std::cell::RefCell;

use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::heap::{GcAny, Trace};
use crux::object::JsObject;
use crux::property::PropertyKey;
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use crate::agent::Agent;
use crate::env::{EnvRecord, EnvRef};
use crate::realm::Realm;
use crate::script::ScriptRecord;

/// The agent of the innermost enclosing [`crux::function::with_agent`]
/// window — the script/eval/call currently running. Native host closures
/// (the embedding API, the test262 harness) use this to reach the agent,
/// which their call signature does not receive. Errors when called outside
/// such a window.
pub fn current_agent_mut() -> Result<&'static mut Agent, JsError> {
    let agent = crux::function::current_agent();
    if agent.is_null() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "host global called outside an agent window".into(),
        ));
    }
    // SAFETY: `with_agent` guarantees a live `&mut Agent` for the duration of
    // the enclosing call; host closures only run inside those windows.
    Ok(unsafe { &mut *(agent as *mut Agent) })
}

/// GetFunctionRealm (spec 10.2.6): the realm the function object was
/// created in — the creation realm for ECMAScript functions, the owning
/// realm for builtins, recursing through bound functions and proxy targets
/// (a revoked proxy throws).
pub fn get_function_realm(agent: &mut Agent, value: &Value) -> Result<Handle<Realm>, JsError> {
    match value.kind() {
        ValueKind::Function(function) => match &function.kind {
            crux::function::FunctionKind::EcmaScript => agent
                .ecma_functions
                .get(&function.id())
                .map(|data| data.realm)
                .ok_or_else(|| {
                    JsError::new(
                        ErrorKind::TypeError,
                        "GetFunctionRealm: no realm for the ECMAScript function".into(),
                    )
                }),
            crux::function::FunctionKind::Bound { target, .. } => get_function_realm(agent, target),
            crux::function::FunctionKind::Builtin { .. } => {
                crate::function::owning_realm(agent, &function).ok_or_else(|| {
                    JsError::new(
                        ErrorKind::TypeError,
                        "GetFunctionRealm: no realm for the builtin".into(),
                    )
                })
            }
        },
        ValueKind::Object(obj) => match &obj.kind {
            crux::object::ObjectKind::Proxy(slots) => {
                let Some(target) = slots.target.borrow().clone() else {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "GetFunctionRealm: revoked proxy".into(),
                    ));
                };
                get_function_realm(agent, &target)
            }
            _ => Err(JsError::new(
                ErrorKind::TypeError,
                "GetFunctionRealm: not a function".into(),
            )),
        },
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "GetFunctionRealm: not a function".into(),
        )),
    }
}

/// The object side of a language value: ordinary objects directly, functions
/// through their embedded object part (functions are values distinct from
/// objects in this engine, but both carry a `Handle<JsObject>`).
pub fn as_object(value: &Value) -> Option<Handle<JsObject>> {
    match value.kind() {
        ValueKind::Object(obj) => Some(obj),
        ValueKind::Function(f) => f.object.handle(),
        _ => None,
    }
}

/// ToPrimitive (spec 7.1.1) with agent dispatch: an object's builtin
/// `toString`/`valueOf` are runtime-dispatched functions, so the crux-level
/// ordinary conversion (which calls native closures directly) would trip
/// their placeholders.
pub fn to_primitive(
    agent: &mut Agent,
    value: &Value,
    hint: crux::convert::ToPrimitiveHint,
) -> Result<Value, JsError> {
    if !matches!(value.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
        return Ok(value.clone());
    }
    // spec 7.1.1 step 1.a: the @@toPrimitive method runs first, and its
    // abrupt completion or object result decides. GetMethod semantics: only
    // undefined/null skip the hook; any other non-callable value throws.
    let exotic = get_property_key(
        agent,
        value,
        &PropertyKey::Symbol(crux::symbol::well_known("toPrimitive").as_ref().clone()),
        value.clone(),
    )?;
    if !matches!(exotic.kind(), ValueKind::Undefined | ValueKind::Null) {
        if !crux::value::is_callable(&exotic) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Symbol.toPrimitive is not a function".into(),
            ));
        }
        let hint_text = match hint {
            crux::convert::ToPrimitiveHint::String => "string",
            crux::convert::ToPrimitiveHint::Default => "default",
            crux::convert::ToPrimitiveHint::Number => "number",
        };
        let result = crate::function::call(
            agent,
            &exotic,
            value.clone(),
            &[Value::String(Handle::new(JsString::from_utf8(hint_text)))],
        )?;
        if matches!(result.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Cannot convert object to primitive value".into(),
            ));
        }
        return Ok(result);
    }
    // OrdinaryToPrimitive: only the string hint prefers toString; "default"
    // and "number" both try valueOf first.
    let (first, second) = match hint {
        crux::convert::ToPrimitiveHint::String => ("toString", "valueOf"),
        crux::convert::ToPrimitiveHint::Default | crux::convert::ToPrimitiveHint::Number => {
            ("valueOf", "toString")
        }
    };
    for name in [first, second] {
        let method = get_property_key(agent, value, &PropertyKey::from_utf8(name), value.clone())?;
        if crux::value::is_callable(&method) {
            let result = crate::function::call(agent, &method, value.clone(), &[])?;
            if !matches!(result.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
                return Ok(result);
            }
        }
    }
    Err(JsError::new(
        ErrorKind::TypeError,
        "Cannot convert object to primitive value".into(),
    ))
}

/// ToString (spec 7.1.17) with agent dispatch for object receivers.
pub fn to_string(agent: &mut Agent, value: &Value) -> Result<JsString, JsError> {
    match value.kind() {
        ValueKind::Object(_) | ValueKind::Function(_) => {
            let prim = to_primitive(agent, value, crux::convert::ToPrimitiveHint::String)?;
            to_string(agent, &prim)
        }
        _ => crux::convert::to_string(value),
    }
}

/// ToNumber (spec 7.1.4) with agent dispatch for object receivers.
pub fn to_number(agent: &mut Agent, value: &Value) -> Result<f64, JsError> {
    match value.kind() {
        ValueKind::Object(_) | ValueKind::Function(_) => {
            let prim = to_primitive(agent, value, crux::convert::ToPrimitiveHint::Number)?;
            to_number(agent, &prim)
        }
        _ => crux::convert::to_number(value),
    }
}

/// ToPropertyKey (spec 7.1.20) with agent dispatch for object receivers.
pub fn to_property_key(agent: &mut Agent, value: &Value) -> Result<PropertyKey, JsError> {
    let key = to_primitive(agent, value, crux::convert::ToPrimitiveHint::String)?;
    match key.kind() {
        ValueKind::Symbol(sym) => Ok(PropertyKey::Symbol(sym.as_ref().clone())),
        _ => {
            let text = to_string(agent, &key)?;
            Ok(PropertyKey::String(crux::intern(text.as_slice())))
        }
    }
}

/// ToIndex (spec 7.1.5) with the agent-aware ToNumber, so an object offset
/// reaches its valueOf/toString through the agent's dispatch (crux cannot
/// invoke the default built-in methods).
pub fn to_index(agent: &mut Agent, value: &Value) -> Result<u64, JsError> {
    let number = to_number(agent, value)?;
    crux::convert::to_index(&Value::Number(number))
}

/// ToBigInt (spec 7.1.16) with agent dispatch for object receivers.
pub fn to_big_int(agent: &mut Agent, value: &Value) -> Result<crux::BigInt, JsError> {
    let prim = to_primitive(agent, value, crux::convert::ToPrimitiveHint::Number)?;
    match prim.kind() {
        ValueKind::Undefined | ValueKind::Null => Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot convert undefined or null to a BigInt".into(),
        )),
        ValueKind::Boolean(true) => Ok(crux::BigInt::from(1u64)),
        ValueKind::Boolean(false) => Ok(crux::BigInt::from(0u64)),
        ValueKind::BigInt(b) => Ok(b.as_ref().clone()),
        ValueKind::String(_) => crux::convert::to_big_int(&prim),
        // spec 7.1.17 ToBigInt: a Number throws a TypeError (the integral
        // NumberToBigInt case belongs to the BigInt() constructor and the
        // TypedArray constructor paths, which special-case Numbers).
        ValueKind::Number(_) => Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot convert a Number to a BigInt".into(),
        )),
        ValueKind::Symbol(_) => Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot convert a Symbol to a BigInt".into(),
        )),
        ValueKind::Object(_) | ValueKind::Function(_) => Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot convert an object to a BigInt".into(),
        )),
    }
}

/// ToObject (spec 7.1.19): box a primitive; null/undefined throw. Boolean
/// and Symbol boxes carry their wrapped value in the agent tables so
/// `valueOf`/`toString`/`description` read it back; the Number/BigInt
/// wrappers arrive with their phases, until then the value rides in a plain
/// %Object.prototype% object.
pub fn to_object(agent: &mut Agent, value: &Value) -> Result<Value, JsError> {
    let realm = agent.current_realm()?;
    match value.kind() {
        ValueKind::Object(obj) => Ok(Value::Object(obj)),
        ValueKind::Function(function) => Ok(Value::Function(function)),
        ValueKind::Null | ValueKind::Undefined => Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot convert undefined or null to object".into(),
        )),
        ValueKind::String(s) => {
            let proto = realm
                .intrinsics
                .get("%String.prototype%")
                .and_then(|v| as_object(&v));
            Ok(Value::Object(JsObject::string_create(
                s.as_ref().clone(),
                proto,
            )?))
        }
        ValueKind::Boolean(b) => {
            let proto = realm
                .intrinsics
                .get("%Boolean.prototype%")
                .and_then(|v| as_object(&v));
            let object = JsObject::ordinary_object_create(proto);
            *object.boxed.borrow_mut() = Some(crux::object::BoxedPrimitive::Boolean(b));
            agent.boolean_data.insert(object.id(), b);
            Ok(Value::Object(object))
        }
        ValueKind::Symbol(symbol) => {
            let proto = realm
                .intrinsics
                .get("%Symbol.prototype%")
                .and_then(|v| as_object(&v));
            let object = JsObject::ordinary_object_create(proto);
            agent
                .symbol_data
                .insert(object.id(), symbol.as_ref().clone());
            Ok(Value::Object(object))
        }
        ValueKind::Number(n) => {
            let proto = realm
                .intrinsics
                .get("%Number.prototype%")
                .and_then(|v| as_object(&v));
            let object = JsObject::ordinary_object_create(proto);
            *object.boxed.borrow_mut() = Some(crux::object::BoxedPrimitive::Number(n));
            agent.number_data.insert(object.id(), n);
            Ok(Value::Object(object))
        }
        ValueKind::BigInt(b) => {
            let proto = realm
                .intrinsics
                .get("%BigInt.prototype%")
                .and_then(|v| as_object(&v));
            let object = JsObject::ordinary_object_create(proto);
            *object.boxed.borrow_mut() =
                Some(crux::object::BoxedPrimitive::BigInt(b.as_ref().clone()));
            agent.bigint_data.insert(object.id(), b.as_ref().clone());
            Ok(Value::Object(object))
        }
    }
}

/// The active script or module (spec 9.4.2): what the running code came from.
#[derive(Debug, Clone)]
pub enum ScriptOrModule {
    Script(Handle<ScriptRecord>),
    Module(Handle<crate::module::SourceTextModule>),
}

impl Trace for ScriptOrModule {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        match self {
            ScriptOrModule::Script(script) => script.trace(visit),
            ScriptOrModule::Module(module) => module.trace(visit),
        }
    }
}

/// An execution context (spec 9.4 tables): the Function, Realm,
/// ScriptOrModule, LexicalEnvironment, VariableEnvironment, and
/// PrivateEnvironment components.
#[derive(Debug, Clone)]
pub struct ExecutionContext {
    pub function: Option<Value>,
    pub realm: Handle<Realm>,
    pub script_or_module: Option<ScriptOrModule>,
    pub lexical_environment: EnvRef,
    pub variable_environment: EnvRef,
    pub private_environment: Option<Handle<PrivateEnvironment>>,
    /// The source text of the code currently running, when known — used to
    /// capture exact function sources for `Function.prototype.toString`.
    pub source: Option<JsString>,
    /// Annex B: the declarations whose block-level function hoist (B.3.3.x)
    /// is applicable in this execution, keyed by function span, consulted by
    /// B.3.2.1 at block entry.
    pub annex_b_hoistable: std::cell::RefCell<std::collections::HashSet<(u32, u32)>>,
}

impl Trace for ExecutionContext {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.function.trace(visit);
        self.realm.trace(visit);
        self.script_or_module.trace(visit);
        self.lexical_environment.trace(visit);
        self.variable_environment.trace(visit);
        self.private_environment.trace(visit);
        self.source.trace(visit);
    }
}

/// A PrivateEnvironment Record (spec 9.2.1): the Private Names declared by
/// the nearest containing class. Used by class evaluation (Phase 7).
#[derive(Debug, Default)]
pub struct PrivateEnvironment {
    pub outer: Option<Handle<PrivateEnvironment>>,
    pub names: RefCell<Vec<PrivateName>>,
}

impl Trace for PrivateEnvironment {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        if let Some(outer) = &self.outer {
            outer.trace(visit);
        }
        // PrivateName descriptions are JsStrings by value; a rope description
        // has heap edges.
        for name in &*self.names.borrow() {
            name.description.trace(visit);
        }
    }
}

/// A Private Name (spec 9.2.1 table): a unique id plus its description.
#[derive(Debug, Clone)]
pub struct PrivateName {
    pub id: u64,
    pub description: JsString,
}

static NEXT_PRIVATE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A fresh Private Name for a class body (unique per class definition).
pub fn new_private_name(description: JsString) -> PrivateName {
    PrivateName {
        id: NEXT_PRIVATE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        description,
    }
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
    /// The Private Name's id for `this.#x` references; `name` is unused
    /// when set (private access is not a property reference).
    pub private_name: Option<u64>,
}

impl Trace for ReferenceBase {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        match self {
            ReferenceBase::Environment(env) => env.trace(visit),
            ReferenceBase::Value(value) => value.trace(visit),
            ReferenceBase::Unresolvable => {}
        }
    }
}

impl Trace for Reference {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.base.trace(visit);
        self.name.trace(visit);
        if let Some(value) = &self.this_value {
            value.trace(visit);
        }
    }
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
                private_name: None,
            });
        };
        if env_record.has_binding(name)? {
            return Ok(Reference {
                base: ReferenceBase::Environment(env_record),
                name: crux::property::PropertyKey::from_js_string(name),
                strict,
                this_value: None,
                private_name: None,
            });
        }
        current = env_record.outer();
    }
}

/// spec 6.2.5.4 GetValue on a Reference Record. Accessor properties whose
/// getter is an ECMAScript function dispatch through the agent, and private
/// member references resolve through PrivateGet.
pub fn get_value(agent: &mut Agent, reference: &Reference) -> Result<Value, JsError> {
    if let Some(name_id) = reference.private_name {
        let ReferenceBase::Value(base) = &reference.base else {
            unreachable!("private references are property references")
        };
        return private_get(agent, base, name_id);
    }
    match &reference.base {
        ReferenceBase::Environment(env) => {
            env.get_binding_value(&string_name(&reference.name), reference.strict)
        }
        ReferenceBase::Value(base) => {
            // super references carry the receiver in [[ThisValue]] (spec
            // 13.3.6.2): a getter sees the current `this`, not the super
            // base.
            get_property_key(agent, base, &reference.name, get_this_value(reference))
        }
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
    // Module namespace objects read their exported bindings through the
    // module environment (spec 10.4.6.1). A deferred namespace triggers its
    // module's synchronous evaluation first (import-defer).
    if let ValueKind::Object(obj) = base.kind()
        && matches!(obj.kind, crux::object::ObjectKind::ModuleNamespace(_))
    {
        crate::module::ensure_deferred_namespace_evaluation_key(agent, &obj, key)?;
        let Some(module) = agent
            .module_namespaces
            .get(&obj.id())
            .cloned()
            .or_else(|| agent.deferred_namespaces.get(&obj.id()).cloned())
        else {
            return Ok(Value::Undefined);
        };
        // Symbol.toStringTag (spec 28.3.1): a data property on the namespace.
        if let crux::property::PropertyKey::Symbol(symbol) = key
            && symbol.id == crux::symbol::well_known("toStringTag").id
        {
            let tag = if agent.deferred_namespaces.contains_key(&obj.id()) {
                "Deferred Module"
            } else {
                "Module"
            };
            return Ok(Value::String(Handle::new(JsString::from_utf8(tag))));
        }
        let crux::property::PropertyKey::String(id) = key else {
            return Ok(Value::Undefined);
        };
        let name = crux::lookup(*id);
        if name == JsString::from_utf8("Symbol.toStringTag") {
            return Ok(Value::Undefined);
        }
        return crate::module::namespace_get(agent, &module, &name);
    }
    match base.kind() {
        ValueKind::Object(obj) => {
            // Fast path: an own data property on a plain object — nothing on
            // the prototype chain can fire, so skip the accessor scan and the
            // exotic dispatch. The receiver is only meaningful to accessors
            // and the arguments mapping, both excluded here.
            if matches!(
                obj.kind,
                crux::object::ObjectKind::Ordinary | crux::object::ObjectKind::Array
            ) && let Some(property) = obj.get_own_property_key(key)?
                && let crux::object::PropertyKind::Data { value, .. } = property.kind
            {
                return Ok(value);
            }
            // Accessors whose getter is an ECMAScript function cannot be
            // invoked by the crux layer (the body lives in the agent);
            // dispatch them through the evaluator (spec 8.12.2 step 6.b). The
            // Integer-Indexed exotic [[Get]] intercepts canonical numeric
            // index keys, so the dispatch must not run for them (the prototype
            // chain is not consulted).
            let typed_array_index = matches!(obj.kind, crux::object::ObjectKind::IntegerIndexed(_))
                && crux::object::is_canonical_index_key(key);
            if !typed_array_index
                && let Some(getter) = find_ecma_accessor(agent, &obj, key, AccessorKind::Get)?
            {
                return crate::function::call(agent, &getter, receiver, &[]);
            }
            obj.get_with_receiver_key(key, receiver)
        }
        ValueKind::Function(f) => {
            if let Some(getter) = find_ecma_accessor(agent, &f.object, key, AccessorKind::Get)? {
                return crate::function::call(agent, &getter, receiver, &[]);
            }
            f.object.get_with_receiver_key(key, receiver)
        }
        _ => {
            // Primitive bases are boxed for the property read (spec 7.3.2
            // step 5.b), but the receiver stays the primitive: accessors see
            // `this` as the primitive (spec 10.4.3.4 StringExoticObject.
            // [[Get]] passes Receiver through to OrdinaryGet).
            let object = to_object(agent, base)?;
            get_property_key(agent, &object, key, receiver)
        }
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
/// The first accessor on the prototype chain for `key` whose getter/setter
/// is callable. `None` when the chain holds no such accessor, so the crux
/// [[Get]]/[[Set]] handles plain data properties. Both ECMAScript accessors
/// and agent-dispatched builtins are returned; the caller runs them through
/// `runtime::function::call`, which handles either kind.
fn find_ecma_accessor(
    agent: &mut Agent,
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
        // A deferred namespace in the chain triggers its module's evaluation
        // on a non-symbol-like access (import-defer [[GetOwnProperty]]).
        crate::module::ensure_deferred_namespace_evaluation_key(agent, obj, key)?;
        // A proxy forwards [[Get]]/[[Set]] to its target when the handler
        // has no get/set trap (spec 10.5.8 step 5 / 10.5.9 step 5): the
        // target's own descriptor and prototype chain are then probed with
        // ordinary internal methods, never with the proxy's traps. When the
        // trap is present the crux [[Get]]/[[Set]] runs it, so report no
        // agent-dispatched accessor here.
        if let crux::object::ObjectKind::Proxy(slots) = &obj.kind {
            let (target, handler) = match (
                slots.target.borrow().clone(),
                slots.handler.borrow().clone(),
            ) {
                (Some(target), Some(handler)) => (target, handler),
                // A revoked proxy throws via the crux path; report no
                // accessor so [[Get]]/[[Set]] surfaces the TypeError.
                _ => return Ok(None),
            };
            let trap_name = match which {
                AccessorKind::Get => "get",
                AccessorKind::Set => "set",
            };
            // GetMethod on the handler (spec 10.5.8/10.5.9 step 2).
            let trap = match handler.kind() {
                ValueKind::Object(handler_obj) => {
                    handler_obj.get_method(&JsString::from_utf8(trap_name))?
                }
                _ => None,
            };
            if trap.is_some() {
                return Ok(None);
            }
            let target_obj = match target.kind() {
                ValueKind::Object(obj) => Some(obj),
                ValueKind::Function(f) => f.object.handle(),
                _ => None,
            };
            let Some(target_obj) = target_obj else {
                return Ok(None);
            };
            prototype = Some(target_obj);
            continue;
        }
        // The Integer-Indexed exotic intercepts canonical numeric index keys
        // (its [[Get]]/[[Set]] do not consult the prototype chain), so
        // neither its own nor any inherited accessor fires for them.
        if matches!(obj.kind, crux::object::ObjectKind::IntegerIndexed(_))
            && crux::object::is_canonical_index_key(key)
        {
            return Ok(None);
        }
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
            // The comment above promises a *callable* accessor; a stored
            // `undefined` getter/setter (an accessor redefine with
            // `get: undefined`) must fall through to the crux [[Get]]/[[Set]],
            // which returns undefined / ignores the write.
            if !crux::value::is_callable(&accessor) {
                return Ok(None);
            }
            return Ok(Some(accessor));
        }
        match obj.get_prototype_of()? {
            Some(proto) => prototype = Some(proto),
            None => return Ok(None),
        }
    }
}

/// spec 6.2.5.6 PutValue: strict mode throws on unresolvable references and
/// failed object writes; sloppy mode creates a global property instead.
/// Accessor properties with ECMAScript-function setters and private member
/// references dispatch through the evaluator.
pub fn put_value(agent: &mut Agent, reference: &Reference, value: Value) -> Result<(), JsError> {
    if let Some(name_id) = reference.private_name {
        let ReferenceBase::Value(base) = &reference.base else {
            unreachable!("private references are property references")
        };
        return private_set(agent, base, name_id, value);
    }
    match &reference.base {
        ReferenceBase::Environment(env) => {
            env.set_mutable_binding(&string_name(&reference.name), value, reference.strict)
        }
        ReferenceBase::Value(base) => {
            let key = &reference.name;
            // ModuleNamespace [[Set]] (spec 10.4.6.5): a direct write returns
            // false without consulting the exports or the prototype chain —
            // a (deferred) namespace never triggers evaluation on `[[Set]]`,
            // and the write fails (TypeError in strict mode).
            let base_is_namespace = match base.kind() {
                ValueKind::Object(obj) => {
                    matches!(obj.kind, crux::object::ObjectKind::ModuleNamespace(_))
                }
                ValueKind::Function(f) => {
                    matches!(f.object.kind, crux::object::ObjectKind::ModuleNamespace(_))
                }
                _ => false,
            };
            if base_is_namespace {
                return if reference.strict {
                    Err(JsError::new(
                        ErrorKind::TypeError,
                        format!(
                            "Cannot assign to read only property {:?}",
                            key.display_string()
                        ),
                    ))
                } else {
                    Ok(())
                };
            }
            // OrdinarySetWithOwnDescriptor (spec 7.3.3 step 2.c) reads the
            // receiver's own descriptor when the property is a data descriptor
            // or absent from the chain. A module-namespace receiver's
            // descriptor reads the live binding: an uninitialized export
            // throws a ReferenceError. An accessor in the chain runs its
            // setter instead (no receiver read), so it is not pre-empted.
            if let ValueKind::Object(obj) = get_this_value(reference).kind()
                && matches!(obj.kind, crux::object::ObjectKind::ModuleNamespace(_))
                && let PropertyKey::String(id) = key
                && let Some(module) = agent
                    .module_namespaces
                    .get(&obj.id())
                    .cloned()
                    .or_else(|| agent.deferred_namespaces.get(&obj.id()).cloned())
            {
                // A deferred namespace's [[Set]] returns false without
                // consulting the exports (import-defer): a direct write does
                // not trigger evaluation.
                let mut probe =
                    if matches!(base.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
                        Some(base.clone())
                    } else {
                        to_object(agent, base).ok()
                    };
                let mut accessor_in_chain = false;
                while let Some(obj) = probe
                    && let Some(obj) = obj.as_object()
                {
                    match obj.get_own_property_key(key)? {
                        Some(prop) if prop.is_accessor() => {
                            accessor_in_chain = true;
                            break;
                        }
                        Some(_) | None => {
                            probe = obj.get_prototype_of()?.map(Value::Object);
                        }
                    }
                }
                if !accessor_in_chain {
                    // The receiver's own-descriptor read
                    // (OrdinarySetWithOwnDescriptor step 2.c.vii) triggers a
                    // deferred namespace's evaluation for non-symbol-like keys
                    // (import-defer [[GetOwnProperty]]).
                    crate::module::ensure_deferred_namespace_evaluation_key(agent, &obj, key)?;
                    crate::module::namespace_get(agent, &module, &crux::lookup(*id))?;
                }
            }
            // Primitive bases are boxed for the write (spec 7.3.6 step 5.b),
            // but the receiver stays the primitive: an inherited accessor
            // setter sees `this` as the primitive, and a write that reaches
            // the end of the chain fails (strict) instead of landing on the
            // ephemeral wrapper.
            let receiver = get_this_value(reference);
            let base = if matches!(base.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
                base.clone()
            } else {
                to_object(agent, base)?
            };
            let result = match base.kind() {
                ValueKind::Object(obj) => {
                    let typed_array_index =
                        matches!(obj.kind, crux::object::ObjectKind::IntegerIndexed(_))
                            && crux::object::is_canonical_index_key(key);
                    if !typed_array_index
                        && let Some(setter) =
                            find_ecma_accessor(agent, &obj, key, AccessorKind::Set)?
                    {
                        crate::function::call(agent, &setter, receiver.clone(), &[value])?;
                        return Ok(());
                    }
                    obj.set_with_receiver_key(key, value, receiver.clone(), reference.strict)
                }
                ValueKind::Function(f) => {
                    if let Some(setter) =
                        find_ecma_accessor(agent, &f.object, key, AccessorKind::Set)?
                    {
                        crate::function::call(agent, &setter, receiver.clone(), &[value])?;
                        return Ok(());
                    }
                    f.object
                        .set_with_receiver_key(key, value, receiver.clone(), reference.strict)
                }
                _ => {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "to_object returned a function".into(),
                    ));
                }
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
pub fn delete_property_or_throw(agent: &mut Agent, reference: &Reference) -> Result<bool, JsError> {
    match &reference.base {
        ReferenceBase::Environment(env) => env.delete_binding(&string_name(&reference.name)),
        ReferenceBase::Value(base) => {
            let key = &reference.name;
            let deleted = match base.kind() {
                ValueKind::Object(obj) => {
                    // A deferred namespace triggers its module's evaluation on
                    // a non-symbol-like delete (import-defer).
                    crate::module::ensure_deferred_namespace_evaluation_key(agent, &obj, key)?;
                    obj.delete_key(key)?
                }
                ValueKind::Function(f) => f.object.delete_key(key)?,
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
/// GetThisValue of a reference (spec 6.2.5.7): a `super` reference carries
/// its own receiver, a property reference's base is the receiver, and an
/// environment reference is the environment's WithBaseObject — the binding
/// object of a `with` environment, *undefined* otherwise (spec 13.3.6.1
/// step 4.b.ii).
pub fn get_this_value(reference: &Reference) -> Value {
    if let Some(this) = &reference.this_value {
        return this.clone();
    }
    match &reference.base {
        ReferenceBase::Value(base) => base.clone(),
        ReferenceBase::Environment(env) => env.with_base_object(),
        ReferenceBase::Unresolvable => Value::Undefined,
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
pub fn get_active_script_or_module(agent: &Agent) -> Option<ScriptOrModule> {
    agent
        .execution_context_stack
        .iter()
        .rev()
        .find_map(|context| context.script_or_module.clone())
}

/// spec 9.4.2 ResolveBinding.
pub fn resolve_binding(agent: &Agent, name: &JsString, strict: bool) -> Result<Reference, JsError> {
    let env = agent.running_context()?.lexical_environment;
    get_identifier_reference(Some(env), name, strict)
}

/// spec 9.4.3 GetThisEnvironment: the innermost environment with a `this`
/// binding.
pub fn get_this_environment(agent: &Agent) -> Result<EnvRef, JsError> {
    let mut env = agent.running_context()?.lexical_environment;
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
    Ok(agent.running_context()?.realm.global_object)
}

/// Resolve a `#name` in the running context's PrivateEnvironment (spec
/// 9.2.1.2 ResolvePrivateIdentifier).
pub fn resolve_private_name(
    agent: &Agent,
    atom: crux::string::AtomId,
) -> Result<PrivateName, JsError> {
    let private_env = agent
        .running_context()?
        .private_environment
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::SyntaxError,
                "Private field access is only valid inside a class".into(),
            )
        })?;
    let description = JsString::from_utf8(&format!("#{}", crux::lookup(atom).to_string_lossy()));
    resolve_private_identifier(&private_env, &description)
}

/// PrivateGet (spec 10.2.9): read a private field/method/accessor from an
/// object's [[PrivateElements]].
pub fn private_get(agent: &mut Agent, obj: &Value, name_id: u64) -> Result<Value, JsError> {
    let Some(receiver) = private_object(obj) else {
        return Err(private_access_error(name_id, "Cannot read private member"));
    };
    let Some(element) = receiver.private_element(name_id) else {
        return Err(private_access_error(name_id, "Cannot read private member"));
    };
    match element.kind {
        crux::object::PrivateElementKind::Field(value)
        | crux::object::PrivateElementKind::Method(value) => Ok(value),
        crux::object::PrivateElementKind::Accessor {
            get: Some(getter), ..
        } => crate::function::call(agent, &getter, obj.clone(), &[]),
        // spec 10.2.9 step 6.b: a private accessor with no getter throws on
        // read (e.g. a setter-only accessor, possibly shadowing a getter in
        // an outer class).
        crux::object::PrivateElementKind::Accessor { .. } => {
            Err(private_access_error(name_id, "Cannot read private member"))
        }
    }
}

/// PrivateSet (spec 10.2.11): write a private field, or invoke a private
/// accessor's setter.
pub fn private_set(
    agent: &mut Agent,
    obj: &Value,
    name_id: u64,
    value: Value,
) -> Result<(), JsError> {
    let Some(receiver) = private_object(obj) else {
        return Err(private_access_error(name_id, "Cannot write private member"));
    };
    let Some(element) = receiver.private_element(name_id) else {
        return Err(private_access_error(name_id, "Cannot write private member"));
    };
    match element.kind {
        crux::object::PrivateElementKind::Field(_) => {
            let mut elements = receiver.private_elements.borrow_mut();
            if let Some(existing) = elements.iter_mut().find(|e| e.name_id == name_id) {
                existing.kind = crux::object::PrivateElementKind::Field(value);
            }
            Ok(())
        }
        crux::object::PrivateElementKind::Method(_) => {
            Err(private_access_error(name_id, "Cannot write private member"))
        }
        crux::object::PrivateElementKind::Accessor {
            set: Some(setter), ..
        } => {
            crate::function::call(agent, &setter, obj.clone(), &[value])?;
            Ok(())
        }
        crux::object::PrivateElementKind::Accessor { .. } => {
            Err(private_access_error(name_id, "Cannot write private member"))
        }
    }
}

/// The object part holding [[PrivateElements]] of a language value.
fn private_object(obj: &Value) -> Option<Handle<crux::object::JsObject>> {
    if let Some(obj) = obj.as_object() {
        Some(obj)
    } else {
        obj.as_function().map(|f| f.object)
    }
}

/// PrivateIn (spec 13.11.1): whether the object carries the private brand.
pub fn private_in(obj: &Value, name_id: u64) -> Result<bool, JsError> {
    match obj.kind() {
        ValueKind::Object(obj) => Ok(obj.has_private_element(name_id)),
        ValueKind::Function(function) => Ok(function.object.has_private_element(name_id)),
        _ => Ok(false),
    }
}

fn private_access_error(name_id: u64, what: &str) -> JsError {
    JsError::new(
        ErrorKind::TypeError,
        format!(
            "{what} #{} from an object whose class did not declare it",
            name_id
        ),
    )
}

/// GetSuperConstructor (spec 9.2.4.6): the active constructor's current
/// [[Prototype]] — `Object.setPrototypeOf` can change it after the class
/// definition — restricted to derived constructors.
pub fn get_super_constructor(agent: &Agent) -> Result<Value, JsError> {
    let env = get_this_environment(agent)?;
    let EnvRecord::Function(function_env) = &*env else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "super() is only valid inside a derived constructor".into(),
        ));
    };
    let ValueKind::Function(function) = function_env.function_object.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "super() is only valid inside a derived constructor".into(),
        ));
    };
    let derived = agent
        .ecma_functions
        .get(&function.id())
        .is_some_and(|data| data.super_constructor.is_some());
    if !derived {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "super() is only valid inside a derived constructor".into(),
        ));
    }
    match function.object.get_prototype_of()? {
        // The prototype may be a function's object part (a superclass
        // constructor): recover the function value so IsConstructor keeps
        // working (e.g. `class C extends B {}` with `super()`).
        Some(proto) => Ok(proto.function_value().unwrap_or(Value::Object(proto))),
        None => Err(JsError::new(
            ErrorKind::TypeError,
            "super() is only valid inside a derived constructor".into(),
        )),
    }
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
    let ValueKind::Function(function) = function_env.function_object.kind() else {
        return Ok(Value::Undefined);
    };
    let Some(home) = agent
        .ecma_functions
        .get(&function.id())
        .and_then(|data| data.home_object.clone())
    else {
        return Ok(Value::Undefined);
    };
    // The home object of a static method/accessor/static-block is the class
    // constructor (a Function); instance members use the class prototype.
    let home_object = match home.kind() {
        ValueKind::Object(obj) => obj,
        ValueKind::Function(f) => f.object,
        _ => return Ok(Value::Undefined),
    };
    Ok(home_object
        .get_prototype_of()?
        .map(|proto| proto.function_value().unwrap_or(Value::Object(proto)))
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

        let a = get_identifier_reference(Some(inner), &name("a"), true).unwrap();
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
        let global = agent.running_context().unwrap().realm.global_object;
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

    #[test]
    fn get_super_constructor_returns_the_function_value() {
        // GetSuperConstructor recovers the superclass as a Function value
        // (not its bare object part), so `super()` in a derived constructor
        // constructs it (statements/class/super/in-constructor.js).
        let value = crate::agent::evaluate(
            "class B {} class C extends B { constructor() { super(); } } new C() instanceof B",
        )
        .unwrap();
        assert_eq!(value, Value::Boolean(true));
    }

    #[test]
    fn relational_operators_to_primitive_the_left_operand_first() {
        // `>`/`<=` swap the operands for IsLessThan with leftFirst=false, but
        // the *source-left* operand's valueOf still runs first
        // (S11.8.2_A2.3_T1 / S11.8.3_A2.3_T1).
        let value = crate::agent::evaluate(
            "var x = { valueOf: function () { return 'x'; } }; \
             var y = { valueOf: function () { return 'y'; } }; \
             var log = []; \
             var a = { valueOf: function () { log.push('a'); return 1; } }; \
             var b = { valueOf: function () { log.push('b'); return 2; } }; \
             a > b; log.join(',')",
        )
        .unwrap();
        assert_eq!(
            value,
            Value::String(Handle::new(JsString::from_utf8("a,b")))
        );
    }
}
