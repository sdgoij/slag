//! The `%AbstractModuleSource%` intrinsic (the source-phase-imports
//! proposal): the constructor whose instances wrap a module's source
//! (CreateModuleSourceObject). `toString` returns the wrapped module's
//! source text. Bodies are placeholders; `runtime::function::call` dispatches
//! by intrinsic identity (the %eval% pattern).

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

const ABSTRACT_MODULE_SOURCE: &str = "%AbstractModuleSource%";
const ABSTRACT_MODULE_SOURCE_PROTO: &str = "%AbstractModuleSource.prototype%";
const PROTO_TO_STRING: &str = "%AbstractModuleSource.prototype.toString%";
const PROTO_TO_STRING_TAG: &str = "%AbstractModuleSource.prototype[@@toStringTag]%";

fn placeholder(name: &'static str) -> NativeFn {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

/// Install the `%AbstractModuleSource%` intrinsic (SetDefaultGlobalBindings).
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let proto = JsObject::ordinary_object_create(object_proto);
    let proto_value = Value::Object(proto);

    let ctor = Function::create_builtin(
        Some(JsString::from_utf8("AbstractModuleSource")),
        0,
        Box::new(placeholder("AbstractModuleSource")),
        Some(Box::new(placeholder("AbstractModuleSource"))),
        None,
    )?;
    let ctor_value = Value::Function(ctor);

    realm
        .intrinsics
        .define(ABSTRACT_MODULE_SOURCE, ctor_value);
    realm
        .intrinsics
        .define(ABSTRACT_MODULE_SOURCE_PROTO, proto_value);

    ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(proto_value),
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

    // `toString` returns the module source text of the wrapped module
    // (RequireInternalSlot + GetModuleSource).
    let to_string = Function::create_builtin(
        Some(JsString::from_utf8("toString")),
        0,
        Box::new(placeholder("AbstractModuleSource.prototype.toString")),
        None,
        None,
    )?;
    proto.create_data_property_or_throw(
        &JsString::from_utf8("toString"),
        Value::Function(to_string),
    )?;
    realm
        .intrinsics
        .define(PROTO_TO_STRING, Value::Function(to_string));

    // `%AbstractModuleSource%.prototype[@@toStringTag]` (spec 28.3.3.2): an
    // accessor returning the [[ModuleSourceClassName]], or *undefined* for a
    // non-slot receiver. Attributes: non-enumerable, configurable.
    let to_string_tag = Function::create_builtin(
        Some(JsString::from_utf8("get [Symbol.toStringTag]")),
        0,
        Box::new(placeholder("AbstractModuleSource.prototype[@@toStringTag]")),
        None,
        None,
    )?;
    proto.define_property_key(
        &crux::property::PropertyKey::Symbol(
            crux::symbol::well_known("toStringTag").as_ref().clone(),
        ),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: Some(Value::Function(to_string_tag)),
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    realm
        .intrinsics
        .define(PROTO_TO_STRING_TAG, Value::Function(to_string_tag));

    Ok(())
}

/// Dispatch the `%AbstractModuleSource%` methods by intrinsic identity.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    _args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(PROTO_TO_STRING).as_ref() == Some(callee) {
        return Some(module_source_to_string(agent, this));
    }
    if intrinsics.get(PROTO_TO_STRING_TAG).as_ref() == Some(callee) {
        return Some(module_source_to_string_tag(agent, this));
    }
    if intrinsics.get(ABSTRACT_MODULE_SOURCE).as_ref() == Some(callee) {
        // The constructor is only reachable through CreateModuleSourceObject;
        // calling or constructing it directly throws a TypeError.
        return Some(Err(JsError::new(
            ErrorKind::TypeError,
            "AbstractModuleSource is not constructible".into(),
        )));
    }
    None
}

fn module_source_to_string_tag(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    // spec 28.3.3.2 steps 1-3: a non-object receiver, or one without a
    // [[ModuleSourceClassName]] internal slot, reads as *undefined*.
    let ValueKind::Object(obj) = this.kind() else {
        return Ok(Value::Undefined);
    };
    let Some(module) = agent.module_sources.get(&obj.id()).cloned() else {
        return Ok(Value::Undefined);
    };
    let name = match module.kind {
        crate::module::ModuleKind::Json => "json",
        crate::module::ModuleKind::Text => "text",
        crate::module::ModuleKind::Bytes => "bytes",
        crate::module::ModuleKind::Js => "module",
    };
    Ok(Value::String(Handle::new(JsString::from_utf8(name))))
}

fn module_source_to_string(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let ValueKind::Object(obj) = this.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "AbstractModuleSource.prototype.toString requires a ModuleSource".into(),
        ));
    };
    let module = agent
        .module_sources
        .get(&obj.id())
        .cloned()
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "AbstractModuleSource.prototype.toString requires a ModuleSource".into(),
            )
        })?;
    let realm = agent.current_realm()?;
    let specifier = realm
        .loaded_modules
        .borrow()
        .iter()
        .find(|(_, m)| Handle::ptr_eq(**m, module))
        .map(|(specifier, _)| specifier.clone());
    let bytes = specifier
        .and_then(|specifier| agent.host_modules.borrow().get(&specifier).cloned())
        .map(|entry| entry.bytes)
        .unwrap_or_default();
    Ok(Value::String(Handle::new(JsString::from_utf8(
        &String::from_utf8_lossy(&bytes),
    ))))
}
