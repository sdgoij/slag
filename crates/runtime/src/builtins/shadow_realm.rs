//! The `ShadowRealm` API (the stage-3 ShadowRealm proposal): the
//! constructor, `evaluate`, `importValue`, and the WrappedFunction exotic
//! object the callable boundary produces.
//!
//! Instances store only their inner realm in the agent's `shadow_realms` map;
//! `evaluate` parses in the caller's realm (so a SyntaxError is the caller's)
//! and runs the body through the eval machinery with the inner realm current.
//! Values crossing the boundary are wrapped by `WrappedFunctionCreate`, whose
//! callable objects are ordinary builtins registered in `shadow_wrapped` and
//! dispatched by the `shadow_realm::dispatch_call` chain arm.

use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::heap::{GcAny, Trace};
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, ValueKind, is_callable};

use crate::agent::Agent;
use crate::realm::Realm;

pub const SHADOW_REALM: &str = "%ShadowRealm%";
pub const SHADOW_REALM_PROTO: &str = "%ShadowRealm.prototype%";
const EVALUATE: &str = "%ShadowRealm.prototype.evaluate%";
const IMPORT_VALUE: &str = "%ShadowRealm.prototype.importValue%";

/// The [[WrappedTargetFunction]] and [[Realm]] of a wrapped function exotic
/// object.
pub struct ShadowWrapped {
    pub target: Value,
    pub caller_realm: Handle<Realm>,
}

impl Trace for ShadowWrapped {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.target.trace(visit);
        self.caller_realm.trace(visit);
    }
}

/// The continuations `ShadowRealm.prototype.importValue` chains onto its
/// inner dynamic import.
pub enum ShadowHandler {
    /// Resolve the caller's promise with the named export, wrapped for the
    /// caller realm.
    ImportFulfilled {
        export_name: String,
        caller_realm: Handle<Realm>,
    },
    /// Reject the caller's promise with a caller-realm TypeError.
    ImportRejected { caller_realm: Handle<Realm> },
}

impl Trace for ShadowHandler {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        match self {
            ShadowHandler::ImportFulfilled { caller_realm, .. }
            | ShadowHandler::ImportRejected { caller_realm } => caller_realm.trace(visit),
        }
    }
}

fn type_error(message: &str) -> JsError {
    JsError::new(ErrorKind::TypeError, message.into())
}

/// Install `ShadowRealm` onto `realm` (spec sec-shadowrealm-constructor).
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| crate::context::as_object(&value));
    let function_proto = realm
        .intrinsics
        .get("%Function.prototype%")
        .and_then(|value| crate::context::as_object(&value));

    let proto = JsObject::ordinary_object_create(object_proto);
    let ctor = Function::create_builtin(
        Some(JsString::from_utf8("ShadowRealm")),
        0,
        Box::new(placeholder("ShadowRealm")),
        Some(Box::new(placeholder("ShadowRealm"))),
        function_proto,
    )?;
    let ctor_value = Value::Function(ctor);
    realm.intrinsics.define(SHADOW_REALM, ctor_value);
    realm
        .intrinsics
        .define(SHADOW_REALM_PROTO, Value::Object(proto));

    ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(Value::Object(proto)),
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
            value: Some(ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    for (name, length, key) in [("evaluate", 1, EVALUATE), ("importValue", 2, IMPORT_VALUE)] {
        let method = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            Box::new(placeholder(name)),
            None,
            function_proto,
        )?;
        realm.intrinsics.define(key, Value::Function(method));
        proto.define_property(
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

    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag")),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8(
                "ShadowRealm",
            )))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("ShadowRealm"),
        &PropertyDescriptor {
            value: Some(ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

fn placeholder(name: &str) -> NativeFn {
    let name = name.to_string();
    Box::new(move |_, _| Err(type_error(&format!("{name} must be dispatched"))))
}

/// `ShadowRealm` (spec sec-shadowrealm): construct an instance and its inner
/// realm.
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    _args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    if realm.intrinsics.get(SHADOW_REALM).as_ref() != Some(callee) {
        return None;
    }
    Some(construct(agent, new_target))
}

fn construct(agent: &mut Agent, new_target: &Value) -> Result<Value, JsError> {
    let proto = get_prototype_from_constructor(agent, new_target)?;
    let instance = JsObject::ordinary_object_create(Some(proto));
    // InitializeHostDefinedRealm: a fresh realm with its own intrinsics and
    // global object. The free function does not push an execution context, so
    // the caller's context stays running.
    let inner = crate::realm::initialize_host_defined_realm(agent)?;
    agent.shadow_realms.insert(instance.id(), inner);
    Ok(Value::Object(instance))
}

fn get_prototype_from_constructor(
    agent: &mut Agent,
    new_target: &Value,
) -> Result<Handle<JsObject>, JsError> {
    let proto = crate::context::get_property(
        agent,
        new_target,
        &JsString::from_utf8("prototype"),
        *new_target,
    )?;
    if let Some(object) = crate::context::as_object(&proto) {
        return Ok(object);
    }
    crate::context::get_function_realm(agent, new_target)?
        .intrinsics
        .get(SHADOW_REALM_PROTO)
        .and_then(|value| crate::context::as_object(&value))
        .ok_or_else(|| type_error("%ShadowRealm.prototype% missing"))
}

/// The dispatch for the ShadowRealm prototype methods and the wrapped
/// functions the callable boundary produces.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let ValueKind::Function(function) = callee.kind() else {
        return None;
    };
    if agent.shadow_wrapped.contains_key(&function.id()) {
        return Some(wrapped_function_call(agent, callee, this, args));
    }
    if agent.shadow_handlers.contains_key(&function.id()) {
        return Some(dispatch_shadow_handler(agent, callee, args));
    }
    let realm = agent.current_realm().ok()?;
    if realm.intrinsics.get(EVALUATE).as_ref() == Some(callee) {
        return Some(evaluate(agent, this, args));
    }
    if realm.intrinsics.get(IMPORT_VALUE).as_ref() == Some(callee) {
        return Some(import_value(agent, this, args));
    }
    None
}

fn shadow_realm_of(agent: &Agent, this: &Value) -> Result<Handle<Realm>, JsError> {
    let object =
        crate::context::as_object(this).ok_or_else(|| type_error("not a ShadowRealm object"))?;
    agent
        .shadow_realms
        .get(&object.id())
        .cloned()
        .ok_or_else(|| type_error("this is not a ShadowRealm object"))
}

/// `ShadowRealm.prototype.evaluate` (spec sec-shadowrealm.prototype.evaluate).
fn evaluate(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let inner = shadow_realm_of(agent, this)?;
    let caller_realm = agent.current_realm()?;
    let source = args.first().cloned().unwrap_or(Value::Undefined);
    let ValueKind::String(source_text) = source.kind() else {
        return Err(type_error(
            "ShadowRealm.prototype.evaluate: sourceText must be a string",
        ));
    };
    // PerformShadowRealmEval parses before switching realms, so a SyntaxError
    // is associated with the caller's realm.
    crate::script::validate_shadow_realm_source(&source_text)?;
    // Run the body with the inner realm current; `perform_eval`'s indirect-eval
    // environment is exactly GetShadowRealmContext's.
    agent.push_bootstrap_context(inner);
    let result = crate::script::perform_eval(agent, &source_text, false, false);
    agent.execution_context_stack.pop();
    match result {
        Ok(value) => get_wrapped_value(agent, caller_realm, caller_realm, value),
        // An abrupt completion becomes a TypeError copy in the caller realm
        // (CreateTypeErrorCopy).
        Err(_) => Err(crate::function::realm_throwable(
            agent,
            type_error("ShadowRealm evaluation threw"),
            caller_realm,
        )?),
    }
}

/// `ShadowRealm.prototype.importValue` (spec sec-shadowrealm.prototype.importvalue).
fn import_value(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let _inner = shadow_realm_of(agent, this)?;
    let caller_realm = agent.current_realm()?;
    let specifier = args.first().cloned().unwrap_or(Value::Undefined);
    // ToString(specifier) abrupts synchronously (spec step 3).
    let specifier_text = crate::context::to_string(agent, &specifier)?;
    let export_name = args.get(1).cloned().unwrap_or(Value::Undefined);
    let ValueKind::String(export_name_string) = export_name.kind() else {
        return Err(crate::function::realm_throwable(
            agent,
            type_error("ShadowRealm.prototype.importValue: exportName must be a string"),
            caller_realm,
        )?);
    };
    // Perform the dynamic import (the host loader resolves the specifier
    // against the running module) and chain the export lookup onto it.
    let promise = crate::module::dynamic_import(
        agent,
        &Value::String(Handle::new(specifier_text)),
        None,
        syntax::ast::ImportPhase::Import,
    )?;
    let on_fulfilled = make_shadow_handler(
        agent,
        ShadowHandler::ImportFulfilled {
            export_name: export_name_string.to_string_lossy(),
            caller_realm,
        },
    )?;
    let on_rejected = make_shadow_handler(agent, ShadowHandler::ImportRejected { caller_realm })?;
    let then =
        crate::context::get_property(agent, &promise, &JsString::from_utf8("then"), promise)?;
    crate::function::call(agent, &then, promise, &[on_fulfilled, on_rejected])
}

fn make_shadow_handler(agent: &mut Agent, handler: ShadowHandler) -> Result<Value, JsError> {
    let closure = Function::create_builtin(
        Some(JsString::from_utf8("")),
        1,
        Box::new(placeholder("shadow handler")),
        None,
        None,
    )?;
    agent.shadow_handlers.insert(closure.id(), handler);
    Ok(Value::Function(closure))
}

fn dispatch_shadow_handler(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let ValueKind::Function(function) = callee.kind() else {
        return Err(type_error("shadow handler is not a function"));
    };
    enum Which {
        Fulfilled,
        Rejected,
    }
    let (which, caller_realm) = match agent.shadow_handlers.get(&function.id()) {
        Some(ShadowHandler::ImportFulfilled { caller_realm, .. }) => {
            (Which::Fulfilled, *caller_realm)
        }
        Some(ShadowHandler::ImportRejected { caller_realm }) => (Which::Rejected, *caller_realm),
        None => return Err(type_error("shadow handler state")),
    };
    match which {
        Which::Rejected => Err(crate::function::realm_throwable(
            agent,
            type_error("ShadowRealm.prototype.importValue: import failed"),
            caller_realm,
        )?),
        Which::Fulfilled => {
            let ShadowHandler::ImportFulfilled { export_name, .. } =
                agent.shadow_handlers.get(&function.id()).expect("checked")
            else {
                unreachable!()
            };
            let export_name = export_name.clone();
            let namespace = args.first().cloned().unwrap_or(Value::Undefined);
            let has_own = crate::context::as_object(&namespace)
                .map(|object| {
                    object
                        .get_own_property(&JsString::from_utf8(&export_name))
                        .map(|property| property.is_some())
                })
                .transpose()?
                .unwrap_or(false);
            if !has_own {
                return Err(crate::function::realm_throwable(
                    agent,
                    type_error("ShadowRealm.prototype.importValue: export not found"),
                    caller_realm,
                )?);
            }
            let value = crate::context::get_property(
                agent,
                &namespace,
                &JsString::from_utf8(&export_name),
                namespace,
            )?;
            get_wrapped_value(agent, caller_realm, caller_realm, value)
        }
    }
}

/// GetWrappedValue (spec sec-getwrappedvalue): primitives pass through; a
/// callable is wrapped into `wrap_realm`; anything else throws. `error_realm`
/// is the realm the boundary associates its TypeErrors with.
fn get_wrapped_value(
    agent: &mut Agent,
    wrap_realm: Handle<Realm>,
    error_realm: Handle<Realm>,
    value: Value,
) -> Result<Value, JsError> {
    if matches!(value.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
        if !is_callable(&value) {
            return Err(crate::function::realm_throwable(
                agent,
                type_error("ShadowRealm boundary: non-callable object"),
                error_realm,
            )?);
        }
        return wrapped_function_create(agent, wrap_realm, error_realm, &value);
    }
    Ok(value)
}

/// WrappedFunctionCreate (spec sec-wrappedfunctioncreate): an anonymous
/// callable builtin with CopyNameAndLength applied, registered so its call
/// crosses the boundary.
fn wrapped_function_create(
    agent: &mut Agent,
    caller_realm: Handle<Realm>,
    error_realm: Handle<Realm>,
    target: &Value,
) -> Result<Value, JsError> {
    let (length, name) = copy_name_and_length(agent, target)
        .map_err(|_| type_error("ShadowRealm boundary: cannot wrap the callable"))?;
    let _ = error_realm;
    let function_proto = caller_realm
        .intrinsics
        .get("%Function.prototype%")
        .and_then(|value| crate::context::as_object(&value));
    let function = Function::create_builtin(
        None,
        0,
        Box::new(placeholder("wrapped function")),
        None,
        function_proto,
    )?;
    function.define_property(
        &JsString::from_utf8("length"),
        &PropertyDescriptor {
            value: Some(Value::Number(length)),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    function.define_property(
        &JsString::from_utf8("name"),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8(&name)))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    agent.shadow_wrapped.insert(
        function.id(),
        ShadowWrapped {
            target: *target,
            caller_realm,
        },
    );
    Ok(Value::Function(function))
}

/// CopyNameAndLength (spec sec-copynameandlength), returning `(length, name)`.
/// Any abrupt completion (a throwing accessor, a revoked proxy) propagates so
/// WrappedFunctionCreate can turn it into a TypeError.
fn copy_name_and_length(agent: &mut Agent, target: &Value) -> Result<(f64, String), JsError> {
    let object = crate::context::as_object(target)
        .ok_or_else(|| type_error("wrapped target is not an object"))?;
    let mut length = 0.0;
    if object
        .get_own_property(&JsString::from_utf8("length"))?
        .is_some()
    {
        let target_len =
            crate::context::get_property(agent, target, &JsString::from_utf8("length"), *target)?;
        if let ValueKind::Number(number) = target_len.kind() {
            length = if number == f64::INFINITY {
                f64::INFINITY
            } else if number == f64::NEG_INFINITY || number.is_nan() {
                0.0
            } else {
                number.trunc().max(0.0)
            };
        }
    }
    let target_name =
        crate::context::get_property(agent, target, &JsString::from_utf8("name"), *target)?;
    let name = target_name
        .as_string()
        .map(|text| text.to_string_lossy())
        .unwrap_or_default();
    Ok((length, name))
}

/// The [[Call]] of a wrapped function exotic object (spec
/// sec-ordinary-wrapped-function-call).
fn wrapped_function_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let ValueKind::Function(function) = callee.kind() else {
        return Err(type_error("wrapped callee is not a function"));
    };
    let (target, caller_realm) = {
        let entry = agent
            .shadow_wrapped
            .get(&function.id())
            .ok_or_else(|| type_error("wrapped function state"))?;
        (entry.target, entry.caller_realm)
    };
    let target_realm = match crate::context::get_function_realm(agent, &target) {
        Ok(realm) => realm,
        Err(error) => {
            return Err(crate::function::realm_throwable(
                agent,
                error,
                caller_realm,
            )?);
        }
    };
    let mut wrapped_args = Vec::with_capacity(args.len());
    for arg in args {
        // Errors after this point are associated with the wrapped function's
        // [[Realm]] (callerRealm), even while the target realm wraps values.
        wrapped_args.push(get_wrapped_value(agent, target_realm, caller_realm, *arg)?);
    }
    let wrapped_this = get_wrapped_value(agent, target_realm, caller_realm, *this)?;
    match crate::function::call(agent, &target, wrapped_this, &wrapped_args) {
        Ok(value) => get_wrapped_value(agent, caller_realm, caller_realm, value),
        Err(_) => Err(crate::function::realm_throwable(
            agent,
            type_error("ShadowRealm wrapped function threw"),
            caller_realm,
        )?),
    }
}
