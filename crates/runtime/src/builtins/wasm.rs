//! The WebAssembly JavaScript API (JS-API spec), installed as the
//! `WebAssembly` global (Cut 10). Like the other agent-dependent builtins,
//! every function is created with a placeholder native closure and
//! dispatched by intrinsic identity from `runtime::function::call` /
//! `construct`, where the agent — and the `wasm` engine — is reachable.
//!
//! Wave 1 landed the interface shapes, the WebAssembly error classes, and
//! `WebAssembly.validate`; wave 2 lands the `WebAssembly.Module` constructor
//! (compile) and its `exports`/`imports`/`customSections` accessors with the
//! ArrayBuffer-backed custom-section pipeline. Operations behind later waves
//! keep their shape and reject with a clear error until their wave.

use crux::convert::{to_boolean, to_int32, to_number, to_string as value_to_string};
use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::{JsObject, ObjectKind};
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::typed_array::SharedBuffer;
use crux::value::{Value, ValueKind};
use wasm::Value as WasmValue;
use wasm::exec::RunProgress;
use wasm::types::ValType;
use wasm::values::{ExternInner, FuncAddr, RefValue};

use crate::agent::Agent;
use crate::builtins::array::array_from_values;
use crate::builtins::array_buffer;
use crate::builtins::error;
use crate::context::as_object;
use crate::realm::Realm;
use wasm::module::{ExportKind, ImportDesc};

fn placeholder(name: &str) -> NativeFn {
    let message = format!("WebAssembly.{name} is not implemented in Cut 10 yet");
    Box::new(move |_, _| Err(JsError::new(ErrorKind::TypeError, message.clone())))
}

fn string(text: &str) -> Value {
    Value::String(Handle::new(JsString::from_utf8(text)))
}

fn object_proto(realm: &Realm) -> Option<Handle<JsObject>> {
    realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value))
}

fn error_proto(realm: &Realm) -> Option<Handle<JsObject>> {
    realm
        .intrinsics
        .get("%Error.prototype%")
        .and_then(|value| as_object(&value))
}

/// Create a built-in function, register it as an intrinsic, and return the
/// language value. Only the interface constructors receive a [[Construct]].
fn make_function(
    realm: &Handle<Realm>,
    name: &str,
    key: &'static str,
    length: u64,
    construct: bool,
) -> Result<Value, JsError> {
    let ctor = if construct {
        Some(placeholder(name))
    } else {
        None
    };
    let function = Function::create_builtin(
        Some(JsString::from_utf8(name)),
        length,
        placeholder(name),
        ctor,
        None,
    )?;
    let value = Value::Function(function);
    realm.intrinsics.define(key, value);
    Ok(value)
}

fn define_data(
    object: &Handle<JsObject>,
    name: &str,
    value: Value,
    writable: bool,
    enumerable: bool,
    configurable: bool,
) -> Result<(), JsError> {
    object.define_property(
        &JsString::from_utf8(name),
        &PropertyDescriptor {
            value: Some(value),
            writable: Some(writable),
            get: None,
            set: None,
            enumerable: Some(enumerable),
            configurable: Some(configurable),
        },
    )?;
    Ok(())
}

/// Define a namespace/static/prototype data method with the spec name and
/// length (own enumerable data property).
fn define_method(
    object: &Handle<JsObject>,
    realm: &Handle<Realm>,
    name: &str,
    key: &'static str,
    length: u64,
) -> Result<(), JsError> {
    let value = make_function(realm, name, key, length, false)?;
    define_data(object, name, value, true, true, true)
}

/// Define a prototype accessor whose getter (and, when the value is writable,
/// setter) functions carry the spec's `"get <name>"` / `"set <name>"` names.
fn define_accessor(
    object: &Handle<JsObject>,
    realm: &Handle<Realm>,
    name: &str,
    getter_key: &'static str,
    setter_key: Option<&'static str>,
    writable_value: bool,
) -> Result<(), JsError> {
    let get = Some(make_function(
        realm,
        &format!("get {name}"),
        getter_key,
        0,
        false,
    )?);
    let set = if writable_value {
        Some(make_function(
            realm,
            &format!("set {name}"),
            setter_key.expect("setter key"),
            1,
            false,
        )?)
    } else {
        None
    };
    object.define_property(
        &JsString::from_utf8(name),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get,
            set,
            enumerable: Some(true),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// Create a built-in interface: the constructor function plus its prototype
/// object, with `prototype`/`constructor` wired (MakeConstructor with a
/// non-writable `prototype` for built-ins) and the prototype's `@@toStringTag`
/// set to `tag` when given (the non-error interfaces). Registered as
/// intrinsics.
fn install_interface(
    realm: &Handle<Realm>,
    name: &str,
    ctor_key: &'static str,
    proto_key: &'static str,
    parent: Option<Handle<JsObject>>,
    tag: Option<&'static str>,
    length: u64,
) -> Result<Value, JsError> {
    let proto = JsObject::ordinary_object_create(parent);
    let proto_value = Value::Object(proto);
    let ctor = Function::create_builtin(
        Some(JsString::from_utf8(name)),
        length,
        placeholder(name),
        Some(placeholder(name)),
        None,
    )?;
    let ctor_value = Value::Function(ctor);

    realm.intrinsics.define(ctor_key, ctor_value);
    realm.intrinsics.define(proto_key, proto_value);

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
    define_data(&proto, "constructor", ctor_value, true, false, true)?;
    if let Some(tag) = tag {
        proto.define_property_key(
            &PropertyKey::Symbol(crux::symbol::well_known("toStringTag")),
            &PropertyDescriptor {
                value: Some(string(tag)),
                writable: Some(false),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
    }
    Ok(ctor_value)
}

/// GetPrototypeFromConstructor (spec 10.1.14): `newTarget.prototype`, with
/// the constructor's own intrinsic default prototype as the fallback when
/// the property is not an object.
fn instance_proto(
    agent: &mut Agent,
    new_target: &Value,
    default_key: &'static str,
) -> Result<Option<Handle<JsObject>>, JsError> {
    let proto = crate::context::get_property(
        agent,
        new_target,
        &JsString::from_utf8("prototype"),
        *new_target,
    )?;
    if let Some(object) = as_object(&proto) {
        return Ok(Some(object));
    }
    Ok(agent
        .current_realm()?
        .intrinsics
        .get(default_key)
        .and_then(|value| as_object(&value)))
}

/// Install the `WebAssembly` global (JS-API spec 4).
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = object_proto(realm);
    let error_proto = error_proto(realm);

    let namespace = JsObject::ordinary_object_create(object_proto);
    let namespace_value = Value::Object(namespace);

    // SetDefaultGlobalBindings: `WebAssembly` is a writable global whose
    // @@toStringTag is "WebAssembly".
    namespace.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag")),
        &PropertyDescriptor {
            value: Some(string("WebAssembly")),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("WebAssembly"),
        &PropertyDescriptor {
            value: Some(namespace_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // The constructors (non-error interfaces chain to %Object.prototype% and
    // carry a "WebAssembly.<name>" @@toStringTag on their prototypes). Per
    // the JS-API spec every constructor's `length` is 1 except Exception's,
    // which takes (tag, payload) and is 2.
    for (name, tag, ctor_key, proto_key, length) in [
        (
            "Module",
            "WebAssembly.Module",
            "%WebAssembly.Module%",
            "%WebAssembly.Module.prototype%",
            1u64,
        ),
        (
            "Instance",
            "WebAssembly.Instance",
            "%WebAssembly.Instance%",
            "%WebAssembly.Instance.prototype%",
            1u64,
        ),
        (
            "Memory",
            "WebAssembly.Memory",
            "%WebAssembly.Memory%",
            "%WebAssembly.Memory.prototype%",
            1u64,
        ),
        (
            "Table",
            "WebAssembly.Table",
            "%WebAssembly.Table%",
            "%WebAssembly.Table.prototype%",
            1u64,
        ),
        (
            "Global",
            "WebAssembly.Global",
            "%WebAssembly.Global%",
            "%WebAssembly.Global.prototype%",
            1u64,
        ),
        (
            "Tag",
            "WebAssembly.Tag",
            "%WebAssembly.Tag%",
            "%WebAssembly.Tag.prototype%",
            1u64,
        ),
        (
            "Exception",
            "WebAssembly.Exception",
            "%WebAssembly.Exception%",
            "%WebAssembly.Exception.prototype%",
            2u64,
        ),
    ] {
        let ctor = install_interface(
            realm,
            name,
            ctor_key,
            proto_key,
            object_proto,
            Some(tag),
            length,
        )?;
        define_data(&namespace, name, ctor, true, false, true)?;
    }

    // The error interfaces (native errors chaining to %Error.prototype%; no
    // custom toStringTag — they stringify as `Error` like every native error).
    for (name, ctor_key, proto_key, name_key) in [
        (
            "CompileError",
            "%WebAssembly.CompileError%",
            "%WebAssembly.CompileError.prototype%",
            "%CompileError.prototype%",
        ),
        (
            "LinkError",
            "%WebAssembly.LinkError%",
            "%WebAssembly.LinkError.prototype%",
            "%LinkError.prototype%",
        ),
        (
            "RuntimeError",
            "%WebAssembly.RuntimeError%",
            "%WebAssembly.RuntimeError.prototype%",
            "%RuntimeError.prototype%",
        ),
    ] {
        let ctor = install_interface(realm, name, ctor_key, proto_key, error_proto, None, 1)?;
        // The error machinery's GetPrototypeFromConstructor fallback looks up
        // `%<name>.prototype%` (like the native errors), and each prototype
        // exposes its own `name`.
        let proto = realm
            .intrinsics
            .get(proto_key)
            .and_then(|value| as_object(&value))
            .expect("just installed");
        realm.intrinsics.define(name_key, Value::Object(proto));
        define_data(&proto, "name", string(name), true, false, true)?;
        define_data(&namespace, name, ctor, true, false, true)?;
    }

    // WebAssembly.JSTag (JS-API 4.13): a Tag-shaped object whose engine cell
    // is allocated lazily on first use (install has no agent/store). It is the
    // tag arbitrary JS exception values are thrown with when they cross into
    // wasm, so a wasm `catch` for it intercepts a thrown JS value.
    let jstag_proto = realm
        .intrinsics
        .get("%WebAssembly.Tag.prototype%")
        .and_then(|value| as_object(&value))
        .expect("WebAssembly.Tag just installed");
    let jstag = JsObject::ordinary_object_create(Some(jstag_proto));
    let jstag_value = Value::Object(jstag);
    realm.intrinsics.define("%WebAssembly.JSTag%", jstag_value);
    define_data(&namespace, "JSTag", jstag_value, false, false, true)?;

    // Namespace operations.
    define_method(&namespace, realm, "validate", "%WebAssembly.validate%", 1)?;
    define_method(&namespace, realm, "compile", "%WebAssembly.compile%", 1)?;
    define_method(
        &namespace,
        realm,
        "instantiate",
        "%WebAssembly.instantiate%",
        1,
    )?;

    // WebAssembly.Module static methods.
    let module_ctor = realm
        .intrinsics
        .get("%WebAssembly.Module%")
        .and_then(|value| match value.kind() {
            ValueKind::Function(function) => Some(function),
            _ => None,
        })
        .expect("just installed");
    for (name, key, length) in [
        ("exports", "%WebAssembly.Module.exports%", 1u64),
        ("imports", "%WebAssembly.Module.imports%", 1),
        ("customSections", "%WebAssembly.Module.customSections%", 2),
    ] {
        define_method(&module_ctor.object, realm, name, key, length)?;
    }

    // Prototype methods.
    for (proto_key, name, key, length) in [
        (
            "%WebAssembly.Memory.prototype%",
            "grow",
            "%WebAssembly.Memory.prototype.grow%",
            1u64,
        ),
        (
            "%WebAssembly.Table.prototype%",
            "get",
            "%WebAssembly.Table.prototype.get%",
            1,
        ),
        (
            "%WebAssembly.Table.prototype%",
            "set",
            "%WebAssembly.Table.prototype.set%",
            1,
        ),
        (
            "%WebAssembly.Table.prototype%",
            "grow",
            "%WebAssembly.Table.prototype.grow%",
            1,
        ),
        (
            "%WebAssembly.Global.prototype%",
            "valueOf",
            "%WebAssembly.Global.prototype.valueOf%",
            0,
        ),
        (
            "%WebAssembly.Exception.prototype%",
            "is",
            "%WebAssembly.Exception.prototype.is%",
            1,
        ),
        (
            "%WebAssembly.Exception.prototype%",
            "getArg",
            "%WebAssembly.Exception.prototype.getArg%",
            2,
        ),
    ] {
        let object = realm
            .intrinsics
            .get(proto_key)
            .and_then(|value| as_object(&value))
            .expect("just installed");
        define_method(&object, realm, name, key, length)?;
    }

    // Prototype accessors (Instance.exports, Memory.buffer, Table.length are
    // getters; Global.value is a getter/setter pair).
    for (proto_key, name, getter_key, setter_key, writable_value) in [
        (
            "%WebAssembly.Instance.prototype%",
            "exports",
            "%get WebAssembly.Instance.prototype.exports%",
            None,
            false,
        ),
        (
            "%WebAssembly.Memory.prototype%",
            "buffer",
            "%get WebAssembly.Memory.prototype.buffer%",
            None,
            false,
        ),
        (
            "%WebAssembly.Table.prototype%",
            "length",
            "%get WebAssembly.Table.prototype.length%",
            None,
            false,
        ),
        (
            "%WebAssembly.Global.prototype%",
            "value",
            "%get WebAssembly.Global.prototype.value%",
            Some("%set WebAssembly.Global.prototype.value%"),
            true,
        ),
    ] {
        let object = realm
            .intrinsics
            .get(proto_key)
            .and_then(|value| as_object(&value))
            .expect("just installed");
        define_accessor(&object, realm, name, getter_key, setter_key, writable_value)?;
    }

    Ok(())
}

// ---- dispatch ----

/// Dispatch a WebAssembly namespace/static operation. Wave 1 implements
/// `validate`; wave 2 adds the Module accessors; the remaining operations
/// reject through their placeholders until their wave.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    // Every exported wasm function is dispatched here by its function id.
    let export_id = match callee.kind() {
        ValueKind::Function(function) => function.id(),
        _ => 0,
    };
    if let Some(&(instance, index)) = agent.wasm_exports.get(&export_id) {
        return Some(invoke_export(agent, args, instance, index));
    }
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get("%WebAssembly.validate%").as_ref() == Some(callee) {
        return Some(wasm_validate(agent, args));
    }
    if intrinsics.get("%WebAssembly.Module.exports%").as_ref() == Some(callee) {
        return Some(module_descriptors(agent, args, false));
    }
    if intrinsics.get("%WebAssembly.Module.imports%").as_ref() == Some(callee) {
        return Some(module_descriptors(agent, args, true));
    }
    if intrinsics
        .get("%WebAssembly.Module.customSections%")
        .as_ref()
        == Some(callee)
    {
        return Some(custom_sections(agent, args));
    }
    if intrinsics.get("%WebAssembly.compile%").as_ref() == Some(callee) {
        return Some(wasm_compile(agent, args));
    }
    if intrinsics.get("%WebAssembly.instantiate%").as_ref() == Some(callee) {
        return Some(wasm_instantiate(agent, args));
    }
    if intrinsics
        .get("%get WebAssembly.Instance.prototype.exports%")
        .as_ref()
        == Some(callee)
    {
        return Some(instance_exports(agent, this));
    }
    if intrinsics
        .get("%get WebAssembly.Memory.prototype.buffer%")
        .as_ref()
        == Some(callee)
    {
        return Some(memory_buffer_get(agent, this));
    }
    if intrinsics
        .get("%WebAssembly.Memory.prototype.grow%")
        .as_ref()
        == Some(callee)
    {
        return Some(memory_grow(agent, this, args));
    }
    if intrinsics
        .get("%get WebAssembly.Table.prototype.length%")
        .as_ref()
        == Some(callee)
    {
        return Some(table_length(agent, this));
    }
    if intrinsics.get("%WebAssembly.Table.prototype.get%").as_ref() == Some(callee) {
        return Some(table_get_element(agent, this, args));
    }
    if intrinsics.get("%WebAssembly.Table.prototype.set%").as_ref() == Some(callee) {
        return Some(table_set_element(agent, this, args));
    }
    if intrinsics
        .get("%WebAssembly.Table.prototype.grow%")
        .as_ref()
        == Some(callee)
    {
        return Some(table_grow_element(agent, this, args));
    }
    if intrinsics
        .get("%WebAssembly.Global.prototype.valueOf%")
        .as_ref()
        == Some(callee)
    {
        return Some(global_value_of(agent, this));
    }
    if intrinsics
        .get("%get WebAssembly.Global.prototype.value%")
        .as_ref()
        == Some(callee)
    {
        return Some(global_value_get(agent, this));
    }
    if intrinsics
        .get("%set WebAssembly.Global.prototype.value%")
        .as_ref()
        == Some(callee)
    {
        return Some(global_value_set(agent, this, args));
    }
    if intrinsics
        .get("%WebAssembly.Exception.prototype.is%")
        .as_ref()
        == Some(callee)
    {
        return Some(exception_is(agent, this, args));
    }
    if intrinsics
        .get("%WebAssembly.Exception.prototype.getArg%")
        .as_ref()
        == Some(callee)
    {
        return Some(exception_get_arg(agent, this, args));
    }
    None
}

/// Dispatch a WebAssembly constructor. Wave 2 constructs `Module` and the
/// three error classes; the interface constructors reject through their
/// placeholders until their wave.
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get("%WebAssembly.Module%").as_ref() == Some(callee) {
        return Some(module_construct(agent, args, new_target));
    }
    if intrinsics.get("%WebAssembly.Instance%").as_ref() == Some(callee) {
        return Some(instance_construct(agent, args, new_target));
    }
    if intrinsics.get("%WebAssembly.Memory%").as_ref() == Some(callee) {
        return Some(memory_construct(agent, args, new_target));
    }
    if intrinsics.get("%WebAssembly.Table%").as_ref() == Some(callee) {
        return Some(table_construct(agent, args, new_target));
    }
    if intrinsics.get("%WebAssembly.Global%").as_ref() == Some(callee) {
        return Some(global_construct(agent, args, new_target));
    }
    if intrinsics.get("%WebAssembly.Tag%").as_ref() == Some(callee) {
        return Some(tag_construct(agent, args, new_target));
    }
    if intrinsics.get("%WebAssembly.Exception%").as_ref() == Some(callee) {
        return Some(exception_construct(agent, args, new_target));
    }
    for (name, key) in [
        ("CompileError", "%WebAssembly.CompileError%"),
        ("LinkError", "%WebAssembly.LinkError%"),
        ("RuntimeError", "%WebAssembly.RuntimeError%"),
    ] {
        if intrinsics.get(key).as_ref() == Some(callee) {
            return Some(error::error_construct(
                agent,
                args,
                *new_target,
                name,
                false,
                false,
            ));
        }
    }
    None
}

// ---- WebAssembly.validate ----

/// The bytes of a BufferSource argument (JS-API spec 4.1): the whole content
/// of an ArrayBuffer/SharedArrayBuffer or of a typed-array view. Anything
/// else is a TypeError.
fn buffer_source_bytes(agent: &Agent, value: &Value) -> Result<Vec<u8>, JsError> {
    let not_a_buffer = || {
        JsError::new(
            ErrorKind::TypeError,
            "argument is not a BufferSource (ArrayBuffer, SharedArrayBuffer, TypedArray, or DataView)"
                .into(),
        )
    };
    let ValueKind::Object(object) = value.kind() else {
        return Err(not_a_buffer());
    };
    match &object.kind {
        ObjectKind::IntegerIndexed(slots) => {
            let slots = slots.as_ref().clone();
            slots.buffer.read(slots.byte_offset, slots.byte_length)
        }
        _ => {
            let id = object.id();
            let cell = agent.buffer_data.get(&id).ok_or_else(not_a_buffer)?;
            let state = cell.borrow();
            if state.detached {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "buffer is detached".into(),
                ));
            }
            state.shared.read(0, state.byte_length)
        }
    }
}

/// WebAssembly.validate (JS-API spec 4.1.3): compile `bytes` and report
/// whether they form a valid wasm module. Decode/validation failures return
/// false; only a non-BufferSource argument throws.
fn wasm_validate(agent: &Agent, args: &[Value]) -> Result<Value, JsError> {
    let arg = args.first().cloned().unwrap_or(Value::Undefined);
    let bytes = buffer_source_bytes(agent, &arg)?;
    Ok(Value::Boolean(match wasm::decode(&bytes) {
        Ok(module) => wasm::validate(&module).is_ok(),
        Err(_) => false,
    }))
}

// ---- WebAssembly.Module ----

/// The compiled-module argument of a Module static operation: an object whose
/// [[Module]] slot is registered (otherwise TypeError). The module is cloned
/// so the borrow does not outlive the agent access.
fn module_argument(agent: &Agent, args: &[Value]) -> Result<wasm::Module, JsError> {
    let error = || {
        JsError::new(
            ErrorKind::TypeError,
            "argument is not a WebAssembly.Module".into(),
        )
    };
    let Some(value) = args.first() else {
        return Err(error());
    };
    let ValueKind::Object(object) = value.kind() else {
        return Err(error());
    };
    agent
        .wasm_modules
        .get(&object.id())
        .cloned()
        .ok_or_else(error)
}

/// CompileError object with `message`, thrown by the Module constructor and
/// (later) compile/instantiate on invalid bytes.
fn compile_error(agent: &mut Agent, message: &str) -> Result<Value, JsError> {
    let realm = agent.current_realm()?;
    let ctor = realm
        .intrinsics
        .get("%WebAssembly.CompileError%")
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "%WebAssembly.CompileError% missing".into(),
            )
        })?;
    error::error_construct(
        agent,
        &[string(message)],
        ctor,
        "CompileError",
        false,
        false,
    )
}

/// The default `%WebAssembly.Module.prototype%` object.
fn module_proto(agent: &Agent) -> Result<Handle<JsObject>, JsError> {
    agent
        .current_realm()?
        .intrinsics
        .get("%WebAssembly.Module.prototype%")
        .and_then(|value| as_object(&value))
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "%WebAssembly.Module.prototype% missing".into(),
            )
        })
}

/// Compile (decode + validate) `bytes`, registering the [[Module]] slot on a
/// fresh ordinary object with `proto` (the Module default prototype when
/// None). Decode/validation failures throw a CompileError.
fn compile_module_bytes(
    agent: &mut Agent,
    bytes: &[u8],
    proto: Option<Handle<JsObject>>,
) -> Result<Value, JsError> {
    let module = match wasm::decode(bytes) {
        Ok(module) => module,
        Err(_) => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "wasm module bytes did not decode".to_string(),
            )
            .with_value(compile_error(agent, "wasm module decoding failed")?));
        }
    };
    if let Err(error) = wasm::validate(&module) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            format!("wasm module is invalid: {error}"),
        )
        .with_value(compile_error(
            agent,
            &format!("wasm module validation failed: {error}"),
        )?));
    }
    let object = JsObject::ordinary_object_create(proto);
    agent.wasm_modules.insert(object.id(), module);
    Ok(Value::Object(object))
}

/// `new WebAssembly.Module(bytes)` (JS-API spec 4.2.1): compile (decode +
/// validate) the bytes, registering the [[Module]] slot. Decode or validation
/// failures throw CompileError; a non-BufferSource argument throws TypeError.
fn module_construct(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    let bytes_value = args.first().cloned().unwrap_or(Value::Undefined);
    let bytes = buffer_source_bytes(agent, &bytes_value)?;
    let proto = instance_proto(agent, new_target, "%WebAssembly.Module.prototype%")?;
    compile_module_bytes(agent, &bytes, proto)
}

/// The JS-API kind string of a wasm export kind.
fn export_kind(kind: &ExportKind) -> &'static str {
    match kind {
        ExportKind::Func => "function",
        ExportKind::Table => "table",
        ExportKind::Memory => "memory",
        ExportKind::Global => "global",
        ExportKind::Tag => "tag",
    }
}

/// The JS-API kind string of a wasm import descriptor.
fn import_kind(desc: &ImportDesc) -> &'static str {
    match desc {
        ImportDesc::Func(_) => "function",
        ImportDesc::Table(_) => "table",
        ImportDesc::Memory(_) => "memory",
        ImportDesc::Global(_) => "global",
        ImportDesc::Tag(_) => "tag",
    }
}

/// An ordinary descriptor object with writable/enumerable/configurable data
/// properties in the given order.
fn descriptor_object(agent: &Agent, fields: &[(&str, Value)]) -> Result<Value, JsError> {
    let proto = agent
        .current_realm()?
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let object = JsObject::ordinary_object_create(proto);
    for (name, value) in fields {
        define_data(&object, name, *value, true, true, true)?;
    }
    Ok(Value::Object(object))
}

/// Module.exports / Module.imports (JS-API spec 4.2.4/4.2.5): an array of
/// descriptor records describing the module's exports (name, kind) or
/// imports (module, name, kind).
fn module_descriptors(agent: &mut Agent, args: &[Value], imports: bool) -> Result<Value, JsError> {
    let module = module_argument(agent, args)?;
    let mut records = Vec::new();
    if imports {
        for import in &module.imports {
            let desc = descriptor_object(
                agent,
                &[
                    ("module", string(&import.module)),
                    ("name", string(&import.name)),
                    ("kind", string(import_kind(&import.desc))),
                ],
            )?;
            records.push(desc);
        }
    } else {
        for export in &module.exports {
            let desc = descriptor_object(
                agent,
                &[
                    ("name", string(&export.name)),
                    ("kind", string(export_kind(&export.kind))),
                ],
            )?;
            records.push(desc);
        }
    }
    array_from_values(agent, &records)
}

/// The UTF-8 bytes of a JS string (custom-section names are raw UTF-8 byte
/// sequences in the wasm binary).
fn js_string_utf8(text: &JsString) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    for code_point in text.to_code_points() {
        if code_point < 0x80 {
            out.push(code_point as u8);
        } else if code_point < 0x800 {
            out.push(0xC0 | (code_point >> 6) as u8);
            out.push(0x80 | (code_point & 0x3F) as u8);
        } else if code_point < 0x10000 {
            out.push(0xE0 | (code_point >> 12) as u8);
            out.push(0x80 | ((code_point >> 6) & 0x3F) as u8);
            out.push(0x80 | (code_point & 0x3F) as u8);
        } else {
            out.push(0xF0 | (code_point >> 18) as u8);
            out.push(0x80 | ((code_point >> 12) & 0x3F) as u8);
            out.push(0x80 | ((code_point >> 6) & 0x3F) as u8);
            out.push(0x80 | (code_point & 0x3F) as u8);
        }
    }
    out
}

/// A fresh ArrayBuffer language value holding `bytes`.
fn array_buffer_of(agent: &mut Agent, bytes: &[u8]) -> Result<Value, JsError> {
    let realm = agent.current_realm()?;
    let proto = realm
        .intrinsics
        .get("%ArrayBuffer.prototype%")
        .and_then(|value| as_object(&value))
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "%ArrayBuffer.prototype% missing".into(),
            )
        })?;
    let object = JsObject::ordinary_object_create(Some(proto));
    array_buffer::allocate_array_buffer(agent, &object, bytes.len(), false, None)?;
    let shared = agent
        .buffer_data
        .get(&object.id())
        .expect("fresh buffer")
        .borrow()
        .shared
        .clone();
    shared.write(0, bytes)?;
    Ok(Value::Object(object))
}

/// Module.customSections (JS-API spec 4.2.6): an array holding one fresh
/// ArrayBuffer per custom section whose name equals `sectionName`.
fn custom_sections(agent: &mut Agent, args: &[Value]) -> Result<Value, JsError> {
    let module = module_argument(agent, args)?;
    // The section name argument is required (JS-API: a missing/undefined
    // sectionName is a TypeError, not ToString("undefined")).
    let Some(name) = args.get(1) else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "customSections requires a section name".into(),
        ));
    };
    if matches!(name.kind(), ValueKind::Undefined) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "customSections requires a section name".into(),
        ));
    }
    let text = value_to_string(name)?;
    let target = js_string_utf8(&text);
    let mut buffers = Vec::new();
    for section in &module.custom {
        if section.name == target {
            buffers.push(array_buffer_of(agent, &section.data)?);
        }
    }
    array_from_values(agent, &buffers)
}

// ---- WebAssembly.Instance / callable exports ----

/// Build the error instance for the given WebAssembly error class and throw it
/// as a language value (used by the constructors that compile/link).
fn wasm_error(
    agent: &mut Agent,
    name: &'static str,
    ctor_key: &str,
    message: &str,
) -> Result<Value, JsError> {
    let realm = agent.current_realm()?;
    let ctor = realm
        .intrinsics
        .get(ctor_key)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, format!("{ctor_key} missing")))?;
    error::error_construct(agent, &[string(message)], ctor, name, false, false)
}

/// The module argument of an Instance constructor: a `WebAssembly.Module`
/// object (its [[Module]] record) or BufferSource bytes to compile.
fn resolve_module_arg(agent: &mut Agent, value: &Value) -> Result<wasm::Module, JsError> {
    let ValueKind::Object(object) = value.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "argument must be a WebAssembly.Module or BufferSource".into(),
        ));
    };
    if let Some(module) = agent.wasm_modules.get(&object.id()).cloned() {
        return Ok(module);
    }
    let bytes = buffer_source_bytes(agent, value)?;
    match wasm::decode(&bytes) {
        Ok(module) if wasm::validate(&module).is_ok() => Ok(module),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "bytes are not a valid wasm module".into(),
        )
        .with_value(wasm_error(
            agent,
            "CompileError",
            "%WebAssembly.CompileError%",
            "bytes are not a valid wasm module",
        )?)),
    }
}

// ---- WebAssembly.Memory (Cut 10 wave 3b, slice 1) ----

/// The default `%WebAssembly.Memory.prototype%` object.
fn memory_proto(agent: &Agent) -> Result<Handle<JsObject>, JsError> {
    agent
        .current_realm()?
        .intrinsics
        .get("%WebAssembly.Memory.prototype%")
        .and_then(|value| as_object(&value))
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "%WebAssembly.Memory.prototype% missing".into(),
            )
        })
}

/// A `WebAssembly.Memory` wrapper object over engine cell `cell`: memoized
/// per cell so a memory surfaced through a constructor, an import, or several
/// exports keeps object identity, and registered (object id -> cell) so the
/// `buffer` accessor, the imports linking, and Instance exports share one
/// registry.
fn memory_wrapper(agent: &mut Agent, cell: usize) -> Result<Value, JsError> {
    if let Some(existing) = agent.wasm_memory_objects.get(&cell) {
        return Ok(*existing);
    }
    let proto = memory_proto(agent)?;
    let object = JsObject::ordinary_object_create(Some(proto));
    agent.wasm_memories.insert(object.id(), cell);
    let value = Value::Object(object);
    agent.wasm_memory_objects.insert(cell, value);
    Ok(value)
}

/// The engine cell of a `WebAssembly.Memory` receiver (the brand check shared
/// by `buffer` and `grow`).
fn memory_cell(agent: &Agent, this: &Value) -> Result<usize, JsError> {
    let error = || {
        JsError::new(
            ErrorKind::TypeError,
            "receiver is not a WebAssembly.Memory".into(),
        )
    };
    let ValueKind::Object(object) = this.kind() else {
        return Err(error());
    };
    agent
        .wasm_memories
        .get(&object.id())
        .copied()
        .ok_or_else(error)
}

/// The whole bytes of a memory cell (a snapshot; no store borrow is held).
fn memory_cell_bytes(agent: &Agent, cell: usize) -> Result<Vec<u8>, JsError> {
    agent
        .wasm_store
        .borrow()
        .memory_bytes(cell)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "memory cell vanished".into()))
}

/// Whether a memory cell is a JS-API shared memory (its buffer is a
/// SharedArrayBuffer whose block survives grows; only an unshared grow
/// detaches the previous buffer).
fn memory_is_shared(agent: &Agent, cell: usize) -> bool {
    agent
        .wasm_store
        .borrow()
        .memory_type(cell)
        .map(|ty| ty.limits.shared)
        .unwrap_or(false)
}

/// Materialize the current buffer of a memory cell: a fresh ArrayBuffer the
/// cell's bytes are copied into, registered as the cell's live buffer so
/// `buffer` keeps object identity until the memory grows. A shared memory's
/// buffer is a SharedArrayBuffer over a per-cell block that every grow-sized
/// view aliases (the JS-API shared-memory buffer rule).
fn materialize_memory_buffer(agent: &mut Agent, cell: usize) -> Result<Value, JsError> {
    let bytes = memory_cell_bytes(agent, cell)?;
    let buffer = if memory_is_shared(agent, cell) {
        // A shared memory's block must absorb every later grow in place (the
        // workers `SharedBuffer` block is fixed-capacity and `resize` refuses
        // to grow past it), so allocate the block at the declared maximum
        // page count up front rather than at the current byte length.
        let capacity = agent
            .wasm_store
            .borrow()
            .memory_type(cell)
            .and_then(|ty| ty.limits.max)
            .map(|pages| (pages as usize).saturating_mul(wasm::exec::PAGE_SIZE as usize))
            .unwrap_or(bytes.len())
            .max(bytes.len());
        let block = SharedBuffer::new_with_capacity(bytes.len(), capacity);
        block.write(0, &bytes)?;
        array_buffer::shared_array_buffer_from_block(agent, block, bytes.len())?
    } else {
        array_buffer_of(agent, &bytes)?
    };
    agent.wasm_memory_buffers.insert(cell, buffer);
    Ok(buffer)
}

/// A JS-API `[EnforceRange] unsigned long`-shaped whole number in [0, 2^32)
/// (the i32-address index domain). A BigInt is a TypeError (ToNumber of a
/// BigInt throws).
fn enforce_u32(value: &Value, what: &str) -> Result<u64, JsError> {
    let number = to_number(value)?;
    if !number.is_finite() || number < 0.0 || number.fract() != 0.0 || number >= 4_294_967_296.0 {
        return Err(JsError::new(
            ErrorKind::TypeError,
            format!("{what} is out of range"),
        ));
    }
    Ok(number as u64)
}

/// The address-space flavor of a JS `Memory`/`Table` (the descriptor's
/// `address` member, defaulting to `"i32"`). Memory64/table64 objects index
/// with BigInts and hold 64-bit page counts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Address {
    I32,
    I64,
}

/// Read a descriptor's `address` member (`"i32"` when absent; anything else
/// is a TypeError).
fn descriptor_address(agent: &mut Agent, descriptor: &Value) -> Result<Address, JsError> {
    let value = crate::context::get_property(
        agent,
        descriptor,
        &JsString::from_utf8("address"),
        *descriptor,
    )?;
    if matches!(value.kind(), ValueKind::Undefined) {
        return Ok(Address::I32);
    }
    match js_string_text(&value_to_string(&value)?).as_str() {
        "i32" => Ok(Address::I32),
        "i64" => Ok(Address::I64),
        other => Err(JsError::new(
            ErrorKind::TypeError,
            format!("'{other}' is not a valid memory/table address; expected \"i32\" or \"i64\""),
        )),
    }
}

/// A page count or index in the address's domain: i32-address values are
/// `[EnforceRange]` Numbers in [0, 2^32); i64-address values are BigInts
/// (through ToBigInt) in [0, 2^64). Required members (a missing/`undefined`
/// value) are a TypeError.
fn required_pages(
    agent: &mut Agent,
    address: Address,
    value: &Value,
    what: &str,
) -> Result<u64, JsError> {
    match address {
        Address::I32 => enforce_u32(value, what),
        Address::I64 => {
            let big = webidl_bigint(agent, value)?;
            big.to_u64().ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, format!("{what} is out of range"))
            })
        }
    }
}

/// WebIDL `bigint` conversion for the wasm JS-API: like ToBigInt, except an
/// unparseable string is a TypeError (WebIDL StringToBigInt) rather than the
/// SyntaxError the JS `ToBigInt` abstract operation throws.
fn webidl_bigint(agent: &mut Agent, value: &Value) -> Result<crux::BigInt, JsError> {
    let type_error = |what: &str| JsError::new(ErrorKind::TypeError, what.into());
    let prim = crate::context::to_primitive(agent, value, crux::convert::ToPrimitiveHint::Number)?;
    match prim.kind() {
        ValueKind::Undefined | ValueKind::Null => {
            Err(type_error("Cannot convert undefined or null to a BigInt"))
        }
        ValueKind::Boolean(true) => Ok(crux::BigInt::from(1u64)),
        ValueKind::Boolean(false) => Ok(crux::BigInt::from(0u64)),
        ValueKind::BigInt(b) => Ok(b.as_ref().clone()),
        ValueKind::String(s) => crux::convert::string_to_bigint(&s)
            .ok_or_else(|| type_error("Cannot convert the string to a BigInt")),
        ValueKind::Number(_) => Err(type_error("Cannot convert a Number to a BigInt")),
        ValueKind::Symbol(_) => Err(type_error("Cannot convert a Symbol to a BigInt")),
        ValueKind::Object(_) | ValueKind::Function(_) => {
            Err(type_error("Cannot convert an object to a BigInt"))
        }
    }
}

/// An optional page count (`initial`/`maximum`): `undefined` is `None`.
fn optional_pages(
    agent: &mut Agent,
    address: Address,
    value: &Value,
    what: &str,
) -> Result<Option<u64>, JsError> {
    if matches!(value.kind(), ValueKind::Undefined) {
        return Ok(None);
    }
    Ok(Some(required_pages(agent, address, value, what)?))
}

/// The descriptor of a `Memory` (JS-API 4.4.1): its address flavor plus the
/// required `initial` and optional `maximum` page counts (read in spec order:
/// `address`, then `initial`, then `maximum`). A page count whose bytes would
/// exceed the host's ArrayBuffer limit is rejected up front.
fn memory_limits(
    agent: &mut Agent,
    descriptor: &Value,
) -> Result<(Address, u64, Option<u64>, bool), JsError> {
    let ValueKind::Object(_) = descriptor.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Memory descriptor is not an object".into(),
        ));
    };
    let address = descriptor_address(agent, descriptor)?;
    let initial_value = crate::context::get_property(
        agent,
        descriptor,
        &JsString::from_utf8("initial"),
        *descriptor,
    )?;
    let initial =
        optional_pages(agent, address, &initial_value, "Memory initial")?.ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "Memory requires an 'initial' page count".into(),
            )
        })?;
    let maximum_value = crate::context::get_property(
        agent,
        descriptor,
        &JsString::from_utf8("maximum"),
        *descriptor,
    )?;
    let maximum = optional_pages(agent, address, &maximum_value, "Memory maximum")?;
    if maximum.is_some_and(|maximum| maximum < initial) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "'maximum' is smaller than 'initial'".into(),
        ));
    }
    if initial
        .checked_mul(wasm::exec::PAGE_SIZE)
        .is_none_or(|bytes| bytes > crate::builtins::array_buffer::MAX_BYTE_LENGTH as u64)
    {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "memory exceeds the host's ArrayBuffer limit".into(),
        ));
    }
    let shared_value = crate::context::get_property(
        agent,
        descriptor,
        &JsString::from_utf8("shared"),
        *descriptor,
    )?;
    let shared = to_boolean(&shared_value);
    // A shared memory (a SharedArrayBuffer-backed linear memory) must declare
    // a maximum page count: the wasm shared-memory type requires one, and the
    // JS-API enforces it at the descriptor.
    if shared && maximum.is_none() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "a shared memory requires a 'maximum' page count".into(),
        ));
    }
    Ok((address, initial, maximum, shared))
}

/// `new WebAssembly.Memory(descriptor)` (JS-API spec 4.4.1): allocate a
/// standalone memory cell in the agent's engine store and register the
/// wrapper. Its ArrayBuffer materializes on the first `buffer` access.
fn memory_construct(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    let descriptor = args.first().cloned().unwrap_or(Value::Undefined);
    let (address, initial, maximum, shared) = memory_limits(agent, &descriptor)?;
    let mem_type = wasm::types::MemType {
        limits: wasm::types::Limits {
            min: initial,
            max: maximum,
            shared,
        },
        memory64: address == Address::I64,
    };
    let cell = agent
        .wasm_store
        .borrow_mut()
        .memory(mem_type)
        .map_err(|fail| {
            JsError::new(
                ErrorKind::TypeError,
                format!("memory allocation failed: {fail:?}"),
            )
        })?;
    let proto = instance_proto(agent, new_target, "%WebAssembly.Memory.prototype%")?;
    let object = JsObject::ordinary_object_create(proto);
    agent.wasm_memories.insert(object.id(), cell);
    let value = Value::Object(object);
    agent.wasm_memory_objects.insert(cell, value);
    Ok(value)
}

/// `get Memory.prototype.buffer` (JS-API spec 4.4.2): the memory's current
/// ArrayBuffer (SharedArrayBuffer when shared). The same object is returned
/// until the memory grows; an unshared grow detaches the old one, a shared
/// grow returns a fresh view over the same block.
fn memory_buffer_get(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let cell = memory_cell(agent, this)?;
    if let Some(buffer) = agent.wasm_memory_buffers.get(&cell) {
        return Ok(*buffer);
    }
    materialize_memory_buffer(agent, cell)
}

/// `Memory.prototype.grow(delta)` (JS-API spec 4.4.3): grow the cell by
/// `delta` pages (a Number for an i32-address memory, a BigInt for an
/// i64-address one) and return the old page count in the same domain. An
/// unshared grow detaches the previous buffer and materializes a fresh one on
/// the next `buffer` access; a shared grow keeps the old buffer attached and
/// points `buffer` at a fresh SharedArrayBuffer over the same (resized) block,
/// so old and new buffers keep aliasing the memory.
fn memory_grow(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let cell = memory_cell(agent, this)?;
    let address = agent
        .wasm_store
        .borrow()
        .memory_type(cell)
        .map(|ty| ty.memory64)
        .unwrap_or(false);
    let delta_value = args.first().cloned().unwrap_or(Value::Undefined);
    let delta = required_pages(
        agent,
        if address { Address::I64 } else { Address::I32 },
        &delta_value,
        "Memory.grow delta",
    )?;
    let old = agent.wasm_store.borrow_mut().grow_memory(cell, delta);
    let old = old.ok_or_else(|| {
        JsError::new(
            ErrorKind::RangeError,
            "Unable to grow instance memory".into(),
        )
    })?;
    if let Some(buffer) = agent.wasm_memory_buffers.get(&cell).copied() {
        let ValueKind::Object(object) = buffer.kind() else {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "memory buffer is not an object".into(),
            ));
        };
        let id = object.id();
        if memory_is_shared(agent, cell) {
            let new_pages = old.checked_add(delta).ok_or_else(|| {
                JsError::new(ErrorKind::RangeError, "memory grow overflow".into())
            })?;
            let new_len = usize::try_from(new_pages.saturating_mul(wasm::exec::PAGE_SIZE))
                .map_err(|_| JsError::new(ErrorKind::RangeError, "memory grow overflow".into()))?;
            let mut block = {
                let state = agent.buffer_data.get(&id).ok_or_else(|| {
                    JsError::new(ErrorKind::TypeError, "memory buffer state vanished".into())
                })?;
                state.borrow().shared.clone()
            };
            block.resize(new_len)?;
            let fresh = array_buffer::shared_array_buffer_from_block(agent, block, new_len)?;
            agent.wasm_memory_buffers.insert(cell, fresh);
        } else {
            agent.wasm_memory_buffers.remove(&cell);
            array_buffer::detach_array_buffer(agent, id);
        }
    }
    if address {
        Ok(Value::BigInt(Handle::new(crux::BigInt::from(old))))
    } else {
        Ok(Value::Number(old as f64))
    }
}

// ---- the JS <-> wasm memory bridge (Cut 10 wave 3b, slice 1) ----
//
// A `WebAssembly.Memory`'s ArrayBuffer (SharedArrayBuffer for a shared
// memory) is a cache of its engine cell: JS writes through buffer views land
// in the buffer, wasm writes land in the cell. The two are reconciled around
// every wasm run — the buffer is flushed into the cell first, then refreshed
// from it — so at every JS boundary the buffer shows the memory exactly. A
// cell that outgrew its buffer (a `memory.grow` inside wasm) replaces the
// buffer, detaching the stale one for an unshared memory (the JS-API
// detach-on-grow rule); a shared memory keeps every prior buffer attached,
// resizing the shared block so old and new views keep aliasing.

/// Push every live memory buffer into its engine cell (before wasm runs).
fn memory_buffers_to_store(agent: &mut Agent) -> Result<(), JsError> {
    let live: Vec<(usize, Value)> = agent
        .wasm_memory_buffers
        .iter()
        .map(|(&cell, &buffer)| (cell, buffer))
        .collect();
    if live.is_empty() {
        return Ok(());
    }
    let mut writes = Vec::with_capacity(live.len());
    for (cell, buffer) in live {
        writes.push((cell, buffer_source_bytes(agent, &buffer)?));
    }
    let mut store = agent.wasm_store.borrow_mut();
    for (cell, bytes) in writes {
        store.write_memory(cell, &bytes);
    }
    Ok(())
}

/// Refresh every live memory buffer from its engine cell (after wasm ran).
fn memory_buffers_from_store(agent: &mut Agent) -> Result<(), JsError> {
    let live: Vec<(usize, Value)> = agent
        .wasm_memory_buffers
        .iter()
        .map(|(&cell, &buffer)| (cell, buffer))
        .collect();
    if live.is_empty() {
        return Ok(());
    }
    let mut snapshots = Vec::with_capacity(live.len());
    {
        let store = agent.wasm_store.borrow();
        for &(cell, _) in &live {
            if let Some(bytes) = store.memory_bytes(cell) {
                snapshots.push((cell, bytes.len(), bytes.to_vec()));
            }
        }
    }
    for (cell, byte_len, bytes) in snapshots {
        let Some(&buffer) = agent.wasm_memory_buffers.get(&cell) else {
            continue;
        };
        let ValueKind::Object(object) = buffer.kind() else {
            continue;
        };
        let id = object.id();
        let same_size = agent
            .buffer_data
            .get(&id)
            .map(|state| state.borrow().byte_length == byte_len)
            .unwrap_or(false);
        if same_size {
            let shared = agent
                .buffer_data
                .get(&id)
                .expect("buffer present")
                .borrow()
                .shared
                .clone();
            shared.write(0, &bytes)?;
        } else if memory_is_shared(agent, cell) {
            // A shared memory that grew inside wasm: resize the block and
            // point `buffer` at a fresh SAB over it; the stale buffer stays
            // attached and keeps aliasing the memory.
            let mut block = agent
                .buffer_data
                .get(&id)
                .expect("buffer present")
                .borrow()
                .shared
                .clone();
            block.resize(byte_len)?;
            let fresh = array_buffer::shared_array_buffer_from_block(agent, block, byte_len)?;
            agent.wasm_memory_buffers.insert(cell, fresh);
        } else {
            array_buffer::detach_array_buffer(agent, id);
            agent.wasm_memory_buffers.remove(&cell);
            let fresh = array_buffer_of(agent, &bytes)?;
            agent.wasm_memory_buffers.insert(cell, fresh);
        }
    }
    Ok(())
}

// ---- WebAssembly.Tag / WebAssembly.Exception (Cut 10 wave 4, slice 2) ----

fn tag_proto(agent: &Agent) -> Result<Handle<JsObject>, JsError> {
    agent
        .current_realm()?
        .intrinsics
        .get("%WebAssembly.Tag.prototype%")
        .and_then(|value| as_object(&value))
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "%WebAssembly.Tag.prototype% missing".into(),
            )
        })
}

fn exception_proto(agent: &Agent) -> Result<Handle<JsObject>, JsError> {
    agent
        .current_realm()?
        .intrinsics
        .get("%WebAssembly.Exception.prototype%")
        .and_then(|value| as_object(&value))
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "%WebAssembly.Exception.prototype% missing".into(),
            )
        })
}

/// The engine tag cell of a `WebAssembly.Tag` receiver (the brand check).
fn tag_cell_of(agent: &Agent, value: &Value) -> Result<usize, JsError> {
    let error = || {
        JsError::new(
            ErrorKind::TypeError,
            "value is not a WebAssembly.Tag".into(),
        )
    };
    let ValueKind::Object(object) = value.kind() else {
        return Err(error());
    };
    agent.wasm_tags.get(&object.id()).copied().ok_or_else(error)
}

/// The `WebAssembly.JSTag` object installed by [`install`].
fn js_tag_object(agent: &Agent) -> Result<Value, JsError> {
    agent
        .current_realm()?
        .intrinsics
        .get("%WebAssembly.JSTag%")
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "%WebAssembly.JSTag% missing".into()))
}

/// Whether `value` is the `WebAssembly.JSTag` object (object identity).
fn is_js_tag(agent: &Agent, value: &Value) -> bool {
    let Ok(tag) = js_tag_object(agent) else {
        return false;
    };
    let ValueKind::Object(object) = value.kind() else {
        return false;
    };
    matches!(tag.kind(), ValueKind::Object(tag) if tag.id() == object.id())
}

/// The `WebAssembly.JSTag` engine cell, materialized on first use: allocate
/// the standalone externref-payload tag cell and register `WebAssembly.JSTag`
/// as its wrapper so tag imports and `is`/`getArg` resolve it, and so an
/// escaping JS-tag exception is recognized (run_failure unwraps it).
fn js_tag_cell(agent: &mut Agent) -> Result<usize, JsError> {
    if let Some(cell) = agent.wasm_js_tag_cell {
        return Ok(cell);
    }
    let tag_value = js_tag_object(agent)?;
    let cell = agent.wasm_store.borrow_mut().tag(wasm::types::FuncType {
        params: vec![ValType::Ref(wasm::types::RefType::EXTERN)],
        results: Vec::new(),
    });
    let ValueKind::Object(object) = tag_value.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "%WebAssembly.JSTag% is not an object".into(),
        ));
    };
    agent.wasm_tags.insert(object.id(), cell);
    agent.wasm_tag_objects.insert(cell, tag_value);
    agent.wasm_js_tag_cell = Some(cell);
    Ok(cell)
}

/// A tag candidate → its engine cell, materializing `WebAssembly.JSTag`
/// when it is the candidate (a plain [`tag_cell_of`] would reject it before
/// its first use has allocated the cell).
fn resolve_tag_cell(agent: &mut Agent, value: &Value) -> Result<usize, JsError> {
    if is_js_tag(agent, value) {
        return js_tag_cell(agent);
    }
    tag_cell_of(agent, value)
}

/// One memoized `WebAssembly.Tag` wrapper per engine tag cell: a tag surfaced
/// through the constructor, an import, or exports keeps object identity.
fn tag_wrapper_memo(agent: &mut Agent, cell: usize) -> Result<Value, JsError> {
    if let Some(existing) = agent.wasm_tag_objects.get(&cell) {
        return Ok(*existing);
    }
    let proto = tag_proto(agent)?;
    let object = JsObject::ordinary_object_create(Some(proto));
    let value = Value::Object(object);
    agent.wasm_tags.insert(object.id(), cell);
    agent.wasm_tag_objects.insert(cell, value);
    Ok(value)
}

/// One `WebAssembly.Tag` parameter value type (the numeric types plus
/// `externref`, whose payload is a JS value; the other references/v128 are
/// not exception payloads yet).
fn tag_parameter_type(text: &str) -> Result<ValType, JsError> {
    Ok(match text {
        "i32" => ValType::I32,
        "i64" => ValType::I64,
        "f32" => ValType::F32,
        "f64" => ValType::F64,
        "externref" => ValType::Ref(wasm::types::RefType::EXTERN),
        other => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                format!(
                    "'{other}' is not a supported WebAssembly.Tag parameter type in Cut 10 \
                     wave 4 (references/v128 are not supported)"
                ),
            ));
        }
    })
}

/// Read an array-like JS value's elements (used for tag `parameters` and the
/// exception `payload`), returning the element language values.
fn array_elements(agent: &mut Agent, value: &Value, what: &str) -> Result<Vec<Value>, JsError> {
    let ValueKind::Object(_) = value.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            format!("{what} is not an array"),
        ));
    };
    let length_value =
        crate::context::get_property(agent, value, &JsString::from_utf8("length"), *value)?;
    let length = to_number(&length_value)?;
    if !length.is_finite() || length < 0.0 || length.fract() != 0.0 || length > 4_294_967_296.0 {
        return Err(JsError::new(
            ErrorKind::TypeError,
            format!("{what} has an invalid length"),
        ));
    }
    let mut elements = Vec::with_capacity(length as usize);
    for index in 0..length as u32 {
        let element = crate::context::get_property(
            agent,
            value,
            &JsString::from_utf8(&index.to_string()),
            *value,
        )?;
        elements.push(element);
    }
    Ok(elements)
}

/// `new WebAssembly.Tag(descriptor)` (JS-API spec 4.8.1): create a standalone
/// engine tag cell whose payload type is the descriptor's `parameters`.
fn tag_construct(agent: &mut Agent, args: &[Value], new_target: &Value) -> Result<Value, JsError> {
    let descriptor = args.first().cloned().unwrap_or(Value::Undefined);
    let ValueKind::Object(_) = descriptor.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Tag descriptor is not an object".into(),
        ));
    };
    let parameters_value = crate::context::get_property(
        agent,
        &descriptor,
        &JsString::from_utf8("parameters"),
        descriptor,
    )?;
    if matches!(parameters_value.kind(), ValueKind::Undefined) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Tag descriptor requires 'parameters'".into(),
        ));
    }
    let mut params = Vec::new();
    for element in array_elements(agent, &parameters_value, "Tag parameters")? {
        params.push(tag_parameter_type(&js_string_text(&value_to_string(
            &element,
        )?))?);
    }
    let cell = agent.wasm_store.borrow_mut().tag(wasm::types::FuncType {
        params,
        results: Vec::new(),
    });
    let proto = instance_proto(agent, new_target, "%WebAssembly.Tag.prototype%")?;
    let object = JsObject::ordinary_object_create(proto);
    let value = Value::Object(object);
    agent.wasm_tags.insert(object.id(), cell);
    agent.wasm_tag_objects.entry(cell).or_insert(value);
    Ok(value)
}

/// A fresh `WebAssembly.Exception` wrapper over engine tag cell `tag` with
/// payload `args` (registered so `is`/`getArg` and the JS->wasm throw path
/// can resolve it).
fn exception_object(agent: &mut Agent, tag: usize, args: Vec<WasmValue>) -> Result<Value, JsError> {
    let proto = exception_proto(agent)?;
    let object = JsObject::ordinary_object_create(Some(proto));
    agent.wasm_exceptions.insert(object.id(), (tag, args));
    Ok(Value::Object(object))
}

/// `new WebAssembly.Exception(tag, payload)` (JS-API spec 4.9.1): a JS-side
/// exception whose payload is converted (and validated) by the tag's type.
fn exception_construct(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    let tag_value = args.first().cloned().unwrap_or(Value::Undefined);
    // A WebAssembly.Exception cannot be constructed with the JSTag: it is the
    // tag the engine throws JS exception values with, never one JS builds
    // (JS-API 4.9.1 throws a TypeError before the payload is touched).
    if is_js_tag(agent, &tag_value) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "WebAssembly.Exception cannot be constructed with the JSTag".into(),
        ));
    }
    let cell = tag_cell_of(agent, &tag_value)?;
    let params = agent
        .wasm_store
        .borrow()
        .tag_type(cell)
        .map(|ty| ty.params)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "tag cell vanished".into()))?;
    let payload_value = args.get(1).cloned().unwrap_or(Value::Undefined);
    if matches!(payload_value.kind(), ValueKind::Undefined) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "WebAssembly.Exception requires a payload".into(),
        ));
    }
    let elements = array_elements(agent, &payload_value, "Exception payload")?;
    if elements.len() != params.len() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            format!(
                "Exception payload has {} values but the tag expects {}",
                elements.len(),
                params.len()
            ),
        ));
    }
    let mut engine_args = Vec::with_capacity(elements.len());
    for (element, param) in elements.iter().zip(&params) {
        engine_args.push(wasm_arg(agent, param, element)?);
    }
    let proto = instance_proto(agent, new_target, "%WebAssembly.Exception.prototype%")?;
    let object = JsObject::ordinary_object_create(proto);
    agent
        .wasm_exceptions
        .insert(object.id(), (cell, engine_args));
    Ok(Value::Object(object))
}

/// `Exception.prototype.is(tag)` (JS-API spec 4.9.2): whether `tag` is a
/// `WebAssembly.Tag` matching this exception's tag. A non-Tag argument
/// (including a missing one) is a TypeError, per the spec's type checks.
fn exception_is(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let exception = exception_of(agent, this)?;
    let candidate = args.first().cloned().unwrap_or(Value::Undefined);
    let cell = resolve_tag_cell(agent, &candidate)?;
    Ok(Value::Boolean(cell == exception))
}

/// `Exception.prototype.getArg(tag, index)` (JS-API spec 4.9.3): one payload
/// value converted to JS. The tag must be this exception's own tag.
fn exception_get_arg(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let exception_cell = exception_of(agent, this)?;
    let tag_value = args.first().cloned().unwrap_or(Value::Undefined);
    let tag_cell = resolve_tag_cell(agent, &tag_value)?;
    if tag_cell != exception_cell {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "getArg called with a tag that does not match the exception".into(),
        ));
    }
    let index = u32_argument(
        &args.get(1).cloned().unwrap_or(Value::Undefined),
        "getArg index",
    )?;
    let ValueKind::Object(object) = this.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "receiver is not a WebAssembly.Exception".into(),
        ));
    };
    let values = agent
        .wasm_exceptions
        .get(&object.id())
        .map(|(_, values)| values.clone())
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "receiver is not a WebAssembly.Exception".into(),
            )
        })?;
    match values.get(index as usize) {
        Some(value) => wasm_result(agent, *value),
        None => Err(JsError::new(
            ErrorKind::RangeError,
            "exception payload index is out of bounds".into(),
        )),
    }
}

/// The tag cell of an `Exception` receiver (its brand check).
fn exception_of(agent: &Agent, this: &Value) -> Result<usize, JsError> {
    let error = || {
        JsError::new(
            ErrorKind::TypeError,
            "receiver is not a WebAssembly.Exception".into(),
        )
    };
    let ValueKind::Object(object) = this.kind() else {
        return Err(error());
    };
    agent
        .wasm_exceptions
        .get(&object.id())
        .map(|(tag, _)| *tag)
        .ok_or_else(error)
}

// ---- WebAssembly.compile / WebAssembly.instantiate (Cut 10 wave 4) ----

/// Run `body` and return a fresh promise that settles with its result —
/// fulfilled with the value, or rejected with the error's throwable — without
/// ever throwing synchronously (the JS-API's promise-based entry points).
fn async_operation(
    agent: &mut Agent,
    body: impl FnOnce(&mut Agent) -> Result<Value, JsError>,
) -> Result<Value, JsError> {
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let capability = crate::promise::new_promise_capability(agent, &promise_ctor)?;
    match body(agent) {
        Ok(value) => {
            crate::function::call(agent, &capability.resolve, Value::Undefined, &[value])?;
        }
        Err(error) => {
            let reason = crate::promise::error_value(agent, &error);
            crate::function::call(agent, &capability.reject, Value::Undefined, &[reason])?;
        }
    }
    Ok(capability.promise)
}

/// Like [`async_operation`], but the body runs in a later generic job (a
/// microtask), not on the current synchronous turn. The JS-API bytes
/// overloads must not touch their `imports` object until then (the
/// `Synchronous options handling` fixtures); the caller copies the byte
/// argument before enqueueing so a later in-place mutation cannot affect it.
fn deferred_operation(
    agent: &mut Agent,
    body: impl FnOnce(&mut Agent) -> Result<Value, JsError> + 'static,
) -> Result<Value, JsError> {
    let realm = agent.current_realm()?;
    let promise_ctor = realm
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let capability = crate::promise::new_promise_capability(agent, &promise_ctor)?;
    let resolve = capability.resolve;
    let reject = capability.reject;
    agent.enqueue_generic_job(Some(realm), move |agent| match body(agent) {
        Ok(value) => {
            crate::function::call(agent, &resolve, Value::Undefined, &[value])?;
            Ok(Value::Undefined)
        }
        Err(error) => {
            let reason = crate::promise::error_value(agent, &error);
            crate::function::call(agent, &reject, Value::Undefined, &[reason])?;
            Ok(Value::Undefined)
        }
    });
    Ok(capability.promise)
}

/// `WebAssembly.compile(bytes)` (JS-API spec 4.1.2): compile the bytes into a
/// `WebAssembly.Module`. The returned promise resolves with the module or
/// rejects with CompileError (TypeError for a non-BufferSource argument).
fn wasm_compile(agent: &mut Agent, args: &[Value]) -> Result<Value, JsError> {
    async_operation(agent, |agent| {
        let bytes_value = args.first().cloned().unwrap_or(Value::Undefined);
        let bytes = buffer_source_bytes(agent, &bytes_value)?;
        let proto = module_proto(agent)?;
        compile_module_bytes(agent, &bytes, Some(proto))
    })
}

/// `WebAssembly.instantiate(moduleOrBytes, imports)` (JS-API spec 4.1.4): for
/// a BufferSource, compile and instantiate it, resolving with
/// `{ module, instance }`; for a `WebAssembly.Module`, instantiate it,
/// resolving with the `Instance`. Failures reject the returned promise
/// (CompileError, LinkError, RuntimeError, or TypeError for a bad argument).
fn wasm_instantiate(agent: &mut Agent, args: &[Value]) -> Result<Value, JsError> {
    let first = args.first().cloned().unwrap_or(Value::Undefined);
    let imports = args.get(1).cloned().unwrap_or(Value::Undefined);
    // Module-object overload: instantiate it directly (its imports are read
    // synchronously, per the JS-API).
    if let ValueKind::Object(object) = first.kind()
        && let Some(module) = agent.wasm_modules.get(&object.id()).cloned()
    {
        return async_operation(agent, move |agent| {
            let proto = instance_proto_default(agent)?;
            instantiate_module(agent, &module, &imports, Some(proto))
        });
    }
    // BufferSource overload: copy the bytes now, then compile and instantiate
    // (and read `imports`) in a later microtask, resolving with the pair.
    let bytes = match buffer_source_bytes(agent, &first) {
        Ok(bytes) => bytes,
        Err(error) => {
            return deferred_operation(agent, move |_agent| Err(error));
        }
    };
    deferred_operation(agent, move |agent| {
        let module_value = {
            let proto = module_proto(agent)?;
            compile_module_bytes(agent, &bytes, Some(proto))?
        };
        let ValueKind::Object(module_object) = module_value.kind() else {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "compile did not yield a Module".into(),
            ));
        };
        let module = agent
            .wasm_modules
            .get(&module_object.id())
            .cloned()
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "compiled module vanished".into()))?;
        let instance_value = {
            let proto = instance_proto_default(agent)?;
            instantiate_module(agent, &module, &imports, Some(proto))?
        };
        let realm = agent.current_realm()?;
        let object_proto = object_proto(&realm);
        let result = JsObject::ordinary_object_create(object_proto);
        define_data(&result, "module", module_value, true, true, true)?;
        define_data(&result, "instance", instance_value, true, true, true)?;
        Ok(Value::Object(result))
    })
}

// ---- WebAssembly.Table / WebAssembly.Global (Cut 10 wave 3b, slice 2) ----

/// A JS-created table's allocation cap (the engine materializes the whole
/// element vector up front; tables beyond this are far beyond any wasm use).
const MAX_JS_TABLE_ELEMENTS: u64 = 1 << 28;

fn table_proto(agent: &Agent) -> Result<Handle<JsObject>, JsError> {
    agent
        .current_realm()?
        .intrinsics
        .get("%WebAssembly.Table.prototype%")
        .and_then(|value| as_object(&value))
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "%WebAssembly.Table.prototype% missing".into(),
            )
        })
}

fn global_proto(agent: &Agent) -> Result<Handle<JsObject>, JsError> {
    agent
        .current_realm()?
        .intrinsics
        .get("%WebAssembly.Global.prototype%")
        .and_then(|value| as_object(&value))
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "%WebAssembly.Global.prototype% missing".into(),
            )
        })
}

/// The UTF-16-code-unit contents of a JS string as a Rust `String` (used to
/// parse the value-type/element strings of descriptors).
fn js_string_text(text: &JsString) -> String {
    let mut out = String::new();
    for code_point in text.to_code_points() {
        if let Some(ch) = char::from_u32(code_point) {
            out.push(ch);
        }
    }
    out
}

/// A `WebAssembly.Table` wrapper over engine cell `cell`: memoized per cell
/// (a constructor, an import, or several exports surface one object), and
/// registered (object id -> cell) so the prototype methods, imports, and
/// Instance exports resolve it.
fn table_wrapper(agent: &mut Agent, cell: usize) -> Result<Value, JsError> {
    if let Some(existing) = agent.wasm_table_objects.get(&cell) {
        return Ok(*existing);
    }
    let proto = table_proto(agent)?;
    let object = JsObject::ordinary_object_create(Some(proto));
    agent.wasm_tables.insert(object.id(), cell);
    let value = Value::Object(object);
    agent.wasm_table_objects.insert(cell, value);
    Ok(value)
}

/// A fresh `WebAssembly.Global` wrapper over engine cell `cell`.
fn global_wrapper(agent: &mut Agent, cell: usize) -> Result<Value, JsError> {
    if let Some(existing) = agent.wasm_global_objects.get(&cell) {
        return Ok(*existing);
    }
    let proto = global_proto(agent)?;
    let object = JsObject::ordinary_object_create(Some(proto));
    agent.wasm_globals.insert(object.id(), cell);
    let value = Value::Object(object);
    agent.wasm_global_objects.insert(cell, value);
    Ok(value)
}

/// The engine cell of a `WebAssembly.Table` receiver (the brand check).
fn table_cell(agent: &Agent, this: &Value) -> Result<usize, JsError> {
    let error = || {
        JsError::new(
            ErrorKind::TypeError,
            "receiver is not a WebAssembly.Table".into(),
        )
    };
    let ValueKind::Object(object) = this.kind() else {
        return Err(error());
    };
    agent
        .wasm_tables
        .get(&object.id())
        .copied()
        .ok_or_else(error)
}

/// The engine cell of a `WebAssembly.Global` receiver (the brand check).
fn global_cell(agent: &Agent, this: &Value) -> Result<usize, JsError> {
    let error = || {
        JsError::new(
            ErrorKind::TypeError,
            "receiver is not a WebAssembly.Global".into(),
        )
    };
    let ValueKind::Object(object) = this.kind() else {
        return Err(error());
    };
    agent
        .wasm_globals
        .get(&object.id())
        .copied()
        .ok_or_else(error)
}

/// The `value` type of a Global descriptor: the numeric value types plus
/// `externref` (references/v128 are not JS-API globals).
fn global_value_type(text: &str) -> Result<ValType, JsError> {
    Ok(match text {
        "i32" => ValType::I32,
        "i64" => ValType::I64,
        "f32" => ValType::F32,
        "f64" => ValType::F64,
        "externref" => ValType::Ref(wasm::types::RefType::EXTERN),
        other => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                format!(
                    "'{other}' is not a supported WebAssembly.Global value type in Cut 10 \
                     wave 3b (reference/v128 globals are not supported)"
                ),
            ));
        }
    })
}

/// The default (zero) engine value of a numeric value type.
fn default_wasm_value(ty: &ValType) -> WasmValue {
    match ty {
        ValType::I32 => WasmValue::I32(0),
        ValType::I64 => WasmValue::I64(0),
        ValType::F32 => WasmValue::F32(0),
        ValType::F64 => WasmValue::F64(0),
        _ => WasmValue::I32(0),
    }
}

/// `new WebAssembly.Global(descriptor, value)` (JS-API spec 4.5.1): allocate a
/// standalone global cell of the descriptor's value type, optionally mutable,
/// seeded with `value` (zero when omitted).
fn global_construct(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    let descriptor = args.first().cloned().unwrap_or(Value::Undefined);
    let ValueKind::Object(_) = descriptor.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Global descriptor is not an object".into(),
        ));
    };
    // Spec order: the descriptor's `mutable` member is read and converted
    // first, then `value` (whose ToString is the type name).
    let mutable_value = crate::context::get_property(
        agent,
        &descriptor,
        &JsString::from_utf8("mutable"),
        descriptor,
    )?;
    let mutable = to_boolean(&mutable_value);
    let type_value = crate::context::get_property(
        agent,
        &descriptor,
        &JsString::from_utf8("value"),
        descriptor,
    )?;
    if matches!(type_value.kind(), ValueKind::Undefined) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Global descriptor requires a 'value' type".into(),
        ));
    }
    let value_type = global_value_type(&js_string_text(&value_to_string(&type_value)?))?;
    let initial_value = args.get(1).cloned().unwrap_or(Value::Undefined);
    let initial = if matches!(initial_value.kind(), ValueKind::Undefined) {
        // An externref global defaults to `undefined` (the JS-API's externref
        // is a JS-value cell; a wasm null reference is not its default).
        if matches!(value_type, ValType::Ref(wasm::types::RefType::EXTERN)) {
            WasmValue::Ref(to_externref(agent, Value::Undefined))
        } else {
            default_wasm_value(&value_type)
        }
    } else {
        wasm_arg(agent, &value_type, &initial_value)?
    };
    let ty = wasm::types::GlobalType {
        value: value_type,
        mutable,
    };
    let cell = agent.wasm_store.borrow_mut().global(ty, initial);
    let proto = instance_proto(agent, new_target, "%WebAssembly.Global.prototype%")?;
    let object = JsObject::ordinary_object_create(proto);
    agent.wasm_globals.insert(object.id(), cell);
    let value = Value::Object(object);
    agent.wasm_global_objects.insert(cell, value);
    Ok(value)
}

/// `get Global.prototype.value`: the cell's current value converted to JS.
fn global_value_get(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let cell = global_cell(agent, this)?;
    let value = agent
        .wasm_store
        .borrow()
        .global_value(cell)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "global cell vanished".into()))?;
    wasm_result(agent, value)
}

/// `set Global.prototype.value`: convert the argument by the cell's value
/// type and write it back (only mutable globals). An immutable global
/// throws before any conversion of the argument (spec: the setter rejects
/// a non-mutable cell without touching the value).
fn global_value_set(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let cell = global_cell(agent, this)?;
    let ty = agent
        .wasm_store
        .borrow()
        .global_type(cell)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "global cell vanished".into()))?;
    if !ty.mutable {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "cannot set the value of an immutable WebAssembly.Global".into(),
        ));
    }
    let new_value = wasm_arg(
        agent,
        &ty.value,
        &args.first().cloned().unwrap_or(Value::Undefined),
    )?;
    let mut store = agent.wasm_store.borrow_mut();
    store.set_global(cell, new_value);
    Ok(Value::Undefined)
}

/// `Global.prototype.valueOf`: the current value (JS-API spec 4.5.4).
fn global_value_of(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    global_value_get(agent, this)
}

/// A required `[EnforceRange] unsigned long` argument (table indices,
/// exception payload indices): a whole number in [0, 2^32).
fn u32_argument(value: &Value, what: &str) -> Result<u64, JsError> {
    enforce_u32(value, what)
}

/// One JS table element → funcref: null, or an exported-wasm-function wrapper.
/// Raw JS closures as table elements need a typed `WebAssembly.Function`
/// wrapper (a later cut) and are rejected clearly.
fn js_to_funcref(agent: &Agent, value: &Value) -> Result<RefValue, JsError> {
    if matches!(value.kind(), ValueKind::Null) {
        return Ok(RefValue::Null);
    }
    match exported_function_target(agent, value) {
        Some((instance, index)) => Ok(RefValue::Func(FuncAddr { instance, index })),
        None => Err(JsError::new(
            ErrorKind::TypeError,
            "table elements must be null or an exported wasm function; raw JS closures \
             in funcref tables need typed WebAssembly.Function wrappers (Cut 10 wave 3b)"
                .into(),
        )),
    }
}

/// One funcref table element → JS: a memoized function wrapper, or null.
fn funcref_to_js(agent: &mut Agent, reference: RefValue) -> Result<Value, JsError> {
    match reference {
        RefValue::Null => Ok(Value::Null),
        RefValue::Func(addr) => function_wrapper(agent, addr.instance, addr.index, ""),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "only funcref table elements are supported in Cut 10 wave 3b".into(),
        )),
    }
}

/// Store a JS value as a wasm externref — the JS-API's `externref` is an
/// arbitrary JS value (null and undefined included, round-tripping
/// distinctly), kept alive by the agent — and return the engine reference
/// (an opaque `ExternInner::Host` payload).
fn to_externref(agent: &mut Agent, value: Value) -> RefValue {
    let token = agent.wasm_extern_seq;
    agent.wasm_extern_seq = agent.wasm_extern_seq.wrapping_add(1);
    agent.wasm_extern_values.insert(token, value);
    RefValue::Extern(ExternInner::Host(token))
}

/// One wasm externref → the JS value it holds (a null reference is JS null;
/// an internal GC or function reference is not a JS-API value yet).
fn externref_to_js(agent: &Agent, reference: RefValue) -> Result<Value, JsError> {
    match reference {
        RefValue::Null => Ok(Value::Null),
        RefValue::Extern(ExternInner::Host(token)) => agent
            .wasm_extern_values
            .get(&token)
            .copied()
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "unknown externref payload".into())),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "only externref references are supported at the JS boundary".into(),
        )),
    }
}

/// `new WebAssembly.Table(descriptor, init)` (JS-API spec 4.6.1): allocate a
/// standalone funcref table cell of `initial` (optionally `maximum`) slots,
/// filled with `init` (null by default).
fn table_construct(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    let descriptor = args.first().cloned().unwrap_or(Value::Undefined);
    let ValueKind::Object(_) = descriptor.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Table descriptor is not an object".into(),
        ));
    };
    let element_value = crate::context::get_property(
        agent,
        &descriptor,
        &JsString::from_utf8("element"),
        descriptor,
    )?;
    if matches!(element_value.kind(), ValueKind::Undefined) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Table descriptor requires an 'element' type".into(),
        ));
    }
    let element = match js_string_text(&value_to_string(&element_value)?).as_str() {
        "funcref" | "anyfunc" => wasm::types::RefType::FUNC,
        "externref" => wasm::types::RefType::EXTERN,
        other => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                format!(
                    "'{other}' tables are not supported in Cut 10 wave 3b yet \
                     (only funcref/externref)"
                ),
            ));
        }
    };
    let address = descriptor_address(agent, &descriptor)?;
    let initial_value = crate::context::get_property(
        agent,
        &descriptor,
        &JsString::from_utf8("initial"),
        descriptor,
    )?;
    let initial =
        optional_pages(agent, address, &initial_value, "Table initial")?.ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "Table requires an 'initial' length".into(),
            )
        })?;
    if initial > MAX_JS_TABLE_ELEMENTS {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "table exceeds the host allocation limit".into(),
        ));
    }
    let maximum_value = crate::context::get_property(
        agent,
        &descriptor,
        &JsString::from_utf8("maximum"),
        descriptor,
    )?;
    let maximum = optional_pages(agent, address, &maximum_value, "Table maximum")?;
    if maximum.is_some_and(|maximum| maximum < initial) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "'maximum' is smaller than 'initial'".into(),
        ));
    }
    let fill_value = args.get(1).cloned().unwrap_or(Value::Undefined);
    let fill = if element == wasm::types::RefType::EXTERN {
        if matches!(fill_value.kind(), ValueKind::Undefined) {
            RefValue::Null
        } else {
            to_externref(agent, fill_value)
        }
    } else if matches!(fill_value.kind(), ValueKind::Undefined | ValueKind::Null) {
        RefValue::Null
    } else {
        js_to_funcref(agent, &fill_value)?
    };
    let table_type = wasm::types::TableType {
        element,
        limits: wasm::types::Limits {
            min: initial,
            max: maximum,
            shared: false,
        },
        table64: address == Address::I64,
    };
    let cell = agent
        .wasm_store
        .borrow_mut()
        .table(table_type, fill)
        .map_err(|fail| {
            JsError::new(
                ErrorKind::TypeError,
                format!("table allocation failed: {fail:?}"),
            )
        })?;
    let proto = instance_proto(agent, new_target, "%WebAssembly.Table.prototype%")?;
    let object = JsObject::ordinary_object_create(proto);
    agent.wasm_tables.insert(object.id(), cell);
    let value = Value::Object(object);
    agent.wasm_table_objects.insert(cell, value);
    Ok(value)
}

/// `get Table.prototype.length`: the table's current length (a BigInt for a
/// table64 table).
fn table_length(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let cell = table_cell(agent, this)?;
    let table64 = agent
        .wasm_store
        .borrow()
        .table_type(cell)
        .map(|ty| ty.table64)
        .unwrap_or(false);
    let size = agent
        .wasm_store
        .borrow()
        .table_size(cell)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "table cell vanished".into()))?;
    if table64 {
        Ok(Value::BigInt(Handle::new(crux::BigInt::from(size))))
    } else {
        Ok(Value::Number(size as f64))
    }
}

/// `Table.prototype.get(index)`: the element at `index` (a function or null).
fn table_get_element(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let cell = table_cell(agent, this)?;
    let table64 = agent
        .wasm_store
        .borrow()
        .table_type(cell)
        .map(|ty| ty.table64)
        .unwrap_or(false);
    let index = required_pages(
        agent,
        if table64 { Address::I64 } else { Address::I32 },
        &args.first().cloned().unwrap_or(Value::Undefined),
        "table index",
    )?;
    let entry = agent
        .wasm_store
        .borrow()
        .table_get(cell, index)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "table cell vanished".into()))?
        .map_err(|_| JsError::new(ErrorKind::RangeError, "table index is out of bounds".into()))?;
    let element = agent
        .wasm_store
        .borrow()
        .table_type(cell)
        .map(|ty| ty.element)
        .unwrap_or(wasm::types::RefType::FUNC);
    if element == wasm::types::RefType::EXTERN {
        externref_to_js(agent, entry)
    } else {
        funcref_to_js(agent, entry)
    }
}

/// `Table.prototype.set(index, value)`: store a function or null at `index`.
fn table_set_element(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let cell = table_cell(agent, this)?;
    let table64 = agent
        .wasm_store
        .borrow()
        .table_type(cell)
        .map(|ty| ty.table64)
        .unwrap_or(false);
    let index = required_pages(
        agent,
        if table64 { Address::I64 } else { Address::I32 },
        &args.first().cloned().unwrap_or(Value::Undefined),
        "table index",
    )?;
    let element = agent
        .wasm_store
        .borrow()
        .table_type(cell)
        .map(|ty| ty.element)
        .unwrap_or(wasm::types::RefType::FUNC);
    // The value is optional: an omitted argument defaults per element type
    // (funcref -> the null reference, externref -> undefined as a value). An
    // explicitly-`undefined` funcref value is a TypeError (it is neither a
    // function nor null), while externref accepts any JS value.
    let supplied = args.get(1).copied();
    let reference = if element == wasm::types::RefType::EXTERN {
        to_externref(agent, supplied.unwrap_or(Value::Undefined))
    } else {
        match supplied {
            None => RefValue::Null,
            Some(value) if matches!(value.kind(), ValueKind::Null) => RefValue::Null,
            Some(value) if matches!(value.kind(), ValueKind::Undefined) => {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "table elements must be null or an exported wasm function; raw JS closures \
                     in funcref tables need typed WebAssembly.Function wrappers (Cut 10 wave 3b)"
                        .into(),
                ));
            }
            Some(value) => js_to_funcref(agent, &value)?,
        }
    };
    let mut store = agent.wasm_store.borrow_mut();
    let slot = store
        .table_set(cell, index, reference)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "table cell vanished".into()))?;
    slot.map_err(|_| JsError::new(ErrorKind::RangeError, "table index is out of bounds".into()))?;
    Ok(Value::Undefined)
}

/// `Table.prototype.grow(delta, init)`: grow by `delta` slots (filled with
/// `init`, null by default) and return the old length (a BigInt for a table64
/// table).
fn table_grow_element(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let cell = table_cell(agent, this)?;
    let table64 = agent
        .wasm_store
        .borrow()
        .table_type(cell)
        .map(|ty| ty.table64)
        .unwrap_or(false);
    let delta_value = args.first().cloned().unwrap_or(Value::Undefined);
    let delta = required_pages(
        agent,
        if table64 { Address::I64 } else { Address::I32 },
        &delta_value,
        "Table.grow delta",
    )?;
    let fill_value = args.get(1).cloned().unwrap_or(Value::Undefined);
    let element = agent
        .wasm_store
        .borrow()
        .table_type(cell)
        .map(|ty| ty.element)
        .unwrap_or(wasm::types::RefType::FUNC);
    let fill = if element == wasm::types::RefType::EXTERN {
        if matches!(fill_value.kind(), ValueKind::Undefined) {
            RefValue::Null
        } else {
            to_externref(agent, fill_value)
        }
    } else if matches!(fill_value.kind(), ValueKind::Undefined | ValueKind::Null) {
        RefValue::Null
    } else {
        js_to_funcref(agent, &fill_value)?
    };
    let old = agent
        .wasm_store
        .borrow_mut()
        .grow_table(cell, delta, fill)
        .ok_or_else(|| JsError::new(ErrorKind::RangeError, "Unable to grow table".into()))?;
    if table64 {
        Ok(Value::BigInt(Handle::new(crux::BigInt::from(old))))
    } else {
        Ok(Value::Number(old as f64))
    }
}

/// `new WebAssembly.Instance(module, imports)` (JS-API spec 4.3.1): compile
/// `module` (a Module object or bytes), instantiate it in the agent's engine
/// store, and build the `exports` object. Function exports are memoized JS
/// wrappers; memory/table/global exports are the matching `WebAssembly`
/// wrapper objects over the instance's cells (tag exports land in wave 4).
/// Imports resolve against the imports object (exported wasm functions, raw
/// JS closures as external engine host functions, and `Memory`/`Table`/
/// `Global` wrappers); the buffer bridge runs around instantiation so a
/// start function's writes reach the JS-visible buffers.
/// The default `%WebAssembly.Instance.prototype%` object (used by the
/// promise-based `WebAssembly.instantiate`, which has no `new.target`).
fn instance_proto_default(agent: &Agent) -> Result<Handle<JsObject>, JsError> {
    agent
        .current_realm()?
        .intrinsics
        .get("%WebAssembly.Instance.prototype%")
        .and_then(|value| as_object(&value))
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "%WebAssembly.Instance.prototype% missing".into(),
            )
        })
}

fn instance_construct(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    let first = args.first().cloned().unwrap_or(Value::Undefined);
    let module = resolve_module_arg(agent, &first)?;
    let proto = instance_proto(agent, new_target, "%WebAssembly.Instance.prototype%")?;
    let imports_value = args.get(1).cloned().unwrap_or(Value::Undefined);
    instantiate_module(agent, &module, &imports_value, proto)
}

/// The shared instantiation body of the `Instance` constructor and the
/// promise-based `WebAssembly.instantiate` entry points (the module-object
/// overload): instantiate `module` in the agent's engine store against
/// `imports_value` and build the `exports` object.
fn instantiate_module(
    agent: &mut Agent,
    module: &wasm::Module,
    imports_value: &Value,
    proto: Option<Handle<JsObject>>,
) -> Result<Value, JsError> {
    let object = JsObject::ordinary_object_create(proto);

    let needs_imports = !module.imports.is_empty();
    // The imports argument must be an object when present; a module that
    // declares imports requires one (JS-API argument checks are TypeErrors,
    // not link failures).
    if matches!(imports_value.kind(), ValueKind::Undefined) {
        if needs_imports {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "module requires an imports object".into(),
            ));
        }
    } else if !matches!(imports_value.kind(), ValueKind::Object(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "imports argument must be an object".into(),
        ));
    }
    let resolutions = if needs_imports {
        build_imports(agent, module, imports_value)?
    } else {
        Vec::new()
    };

    // Instantiate in the engine store (one borrow window), then collect the
    // exports (another). All store access ends before any agent work below.
    memory_buffers_to_store(agent)?;
    let instantiated = {
        let mut store = agent.wasm_store.borrow_mut();
        store.instantiate(module, &mut |module_name: &str, name: &str| {
            resolutions
                .iter()
                .find(|((import_module, import_name), _)| {
                    import_module == module_name && import_name == name
                })
                .map(|(_, value)| *value)
        })
    };
    let instance_id = match instantiated {
        Ok(id) => id,
        Err(_) => {
            memory_buffers_from_store(agent)?;
            return Err(link_failure(
                agent,
                "module instantiation failed (only exported-wasm-function and \
                 WebAssembly.Memory imports are supported in Cut 10 wave 3b slice 1)",
            )?);
        }
    };
    memory_buffers_from_store(agent)?;

    let exported: Vec<(String, wasm::ExternVal)> = {
        let store = agent.wasm_store.borrow();
        module
            .exports
            .iter()
            .filter_map(|export| {
                store
                    .export(instance_id, &export.name)
                    .map(|value| (export.name.clone(), value))
            })
            .collect()
    };

    // The exports object: a null-prototype, non-extensible object with one
    // non-writable, non-configurable, enumerable data property per export
    // (the JS-API module-exports shape). Function wrappers are memoized by
    // canonical engine function; memory/table/global exports get the cell's
    // memoized wrapper (constructor-, import-, or export-created), so an
    // import/re-export chain surfaces one object per underlying cell. Store
    // access ends here.
    let exports = JsObject::ordinary_object_create(None);
    for (name, value) in exported {
        let property_value = match value {
            wasm::ExternVal::Func { instance, index } => {
                function_wrapper(agent, instance, index, &name)?
            }
            wasm::ExternVal::Memory(cell) => memory_wrapper(agent, cell)?,
            wasm::ExternVal::Table(cell) => table_wrapper(agent, cell)?,
            wasm::ExternVal::Global(cell) => global_wrapper(agent, cell)?,
            wasm::ExternVal::Tag(cell) => tag_wrapper_memo(agent, cell)?,
            _ => {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    format!("export '{name}' has an unsupported kind"),
                ));
            }
        };
        define_data(&exports, &name, property_value, false, true, false)?;
    }
    exports.prevent_extensions()?;
    let exports_value = Value::Object(exports);
    agent
        .wasm_instance_exports
        .insert(object.id(), exports_value);

    Ok(Value::Object(object))
}

/// The `exports` object accessor: the prebuilt exports object.
fn instance_exports(agent: &Agent, this: &Value) -> Result<Value, JsError> {
    let error = || {
        JsError::new(
            ErrorKind::TypeError,
            "receiver is not a WebAssembly.Instance".into(),
        )
    };
    let ValueKind::Object(object) = this.kind() else {
        return Err(error());
    };
    agent
        .wasm_instance_exports
        .get(&object.id())
        .copied()
        .ok_or_else(error)
}

/// Whether `value` is one of this agent's exported-wasm-function wrappers,
/// and the engine (instance, index) it calls if so.
fn exported_function_target(agent: &Agent, value: &Value) -> Option<(usize, usize)> {
    let ValueKind::Function(function) = value.kind() else {
        return None;
    };
    agent.wasm_exports.get(&function.id()).copied()
}

/// One memoized JS wrapper per engine function, keyed by the function's
/// canonical [`wasm::FuncKey`]: the same function surfaced through instance
/// exports, table slots, and import/re-export chains is one Function object
/// (JS-API [[FuncObj]] memoization).
fn function_wrapper(
    agent: &mut Agent,
    instance: usize,
    index: usize,
    name: &str,
) -> Result<Value, JsError> {
    let key = agent
        .wasm_store
        .borrow()
        .func_key(instance, index)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "unknown wasm function".into()))?;
    if let Some(existing) = agent.wasm_func_objects.get(&key) {
        return Ok(*existing);
    }
    // Exported functions take the module function's parameter count as
    // their `length` and chain to %Function.prototype% (the JS-API only
    // gives them WebAssembly.Function.prototype when `WebAssembly.Function`
    // exists, which it does not in this engine). Their `name` is the
    // function's index in the module's function index space (the same
    // wrapper object is shared by every export name for the function).
    let length = agent
        .wasm_store
        .borrow()
        .func_params(instance, index)
        .map(|params| params.len() as u64)
        .unwrap_or(0);
    let function_proto = agent
        .current_realm()?
        .intrinsics
        .get("%Function.prototype%")
        .and_then(|value| as_object(&value));
    let function = Function::create_builtin(
        Some(JsString::from_utf8(&index.to_string())),
        length,
        placeholder(&format!("{name} export")),
        None,
        function_proto,
    )?;
    let id = function.id();
    agent.wasm_exports.insert(id, (instance, index));
    let value = Value::Function(function);
    agent.wasm_func_objects.insert(key, value);
    Ok(value)
}

/// A LinkError value thrown as the attached value of a TypeError-kind
/// `JsError` (the error-machinery pattern for the WebAssembly error classes).
fn link_failure(agent: &mut Agent, message: &str) -> Result<JsError, JsError> {
    let value = wasm_error(agent, "LinkError", "%WebAssembly.LinkError%", message)?;
    Ok(JsError::new(ErrorKind::TypeError, message.to_string()).with_value(value))
}

/// One import resolved against the JS imports object: (module, field name)
/// -> engine value.
type ResolvedImports = Vec<((String, String), wasm::ExternVal)>;

/// Resolve a module's imports against the JS imports object (JS-API spec
/// 4.3.1 step 5-8): read each import's module object and name value and map
/// them to engine import values. Function imports are exported wasm functions
/// (engine functions) or any other JS callable (registered as an *external*
/// engine host function the resumable driver runs); memory/table/global
/// imports must be the matching `WebAssembly` wrapper object; tag imports
/// (and their `WebAssembly.Tag`/`Exception` objects) land in wave 4. A wrong
/// or missing import value is a LinkError.
fn build_imports(
    agent: &mut Agent,
    module: &wasm::Module,
    imports: &Value,
) -> Result<ResolvedImports, JsError> {
    let mut resolved = Vec::new();
    for import in &module.imports {
        let module_value = crate::context::get_property(
            agent,
            imports,
            &JsString::from_utf8(&import.module),
            *imports,
        )?;
        // An import's module namespace must be an object: a missing or
        // primitive namespace value is a TypeError (spec), not a LinkError.
        if !matches!(module_value.kind(), ValueKind::Object(_)) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                format!("import module '{}' is not an object", import.module),
            ));
        }
        let name_value = crate::context::get_property(
            agent,
            &module_value,
            &JsString::from_utf8(&import.name),
            module_value,
        )?;
        if matches!(name_value.kind(), ValueKind::Undefined) {
            return Err(link_failure(
                agent,
                &format!("import '{}' is not provided", import.name),
            )?);
        }
        let value = match &import.desc {
            wasm::module::ImportDesc::Func(type_index) => {
                match exported_function_target(agent, &name_value) {
                    Some((instance, index)) => wasm::ExternVal::Func { instance, index },
                    None if matches!(name_value.kind(), ValueKind::Function(_)) => {
                        // A raw JS closure: register it as an external engine
                        // host function typed by the import declaration; the
                        // resumable driver (invoke_export) runs it when wasm
                        // calls it, converting by this type.
                        let expected = module.func_at_cloned(*type_index).ok_or_else(|| {
                            JsError::new(ErrorKind::TypeError, "unknown function type".into())
                        })?;
                        let token = agent.wasm_host_seq;
                        agent.wasm_host_seq += 1;
                        agent
                            .wasm_host_functions
                            .insert(token, (name_value, expected.clone()));
                        let host_id = {
                            let mut store = agent.wasm_store.borrow_mut();
                            store.external_host(expected, token)
                        };
                        wasm::ExternVal::HostFunc(host_id)
                    }
                    None => {
                        return Err(link_failure(
                            agent,
                            &format!("import '{}' is not a function", import.name),
                        )?);
                    }
                }
            }
            wasm::module::ImportDesc::Memory(_) => {
                let ValueKind::Object(object) = name_value.kind() else {
                    return Err(link_failure(
                        agent,
                        &format!("import '{}' is not a WebAssembly.Memory", import.name),
                    )?);
                };
                match agent.wasm_memories.get(&object.id()) {
                    Some(cell) => wasm::ExternVal::Memory(*cell),
                    None => {
                        return Err(link_failure(
                            agent,
                            &format!("import '{}' is not a WebAssembly.Memory", import.name),
                        )?);
                    }
                }
            }
            wasm::module::ImportDesc::Table(_) => {
                let ValueKind::Object(object) = name_value.kind() else {
                    return Err(link_failure(
                        agent,
                        &format!("import '{}' is not a WebAssembly.Table", import.name),
                    )?);
                };
                match agent.wasm_tables.get(&object.id()) {
                    Some(cell) => wasm::ExternVal::Table(*cell),
                    None => {
                        return Err(link_failure(
                            agent,
                            &format!("import '{}' is not a WebAssembly.Table", import.name),
                        )?);
                    }
                }
            }
            wasm::module::ImportDesc::Global(declared) => {
                // A `WebAssembly.Global` wrapper aliases its cell (the engine
                // type-checks the match). Any other value is converted by the
                // declared type and wrapped in a fresh *immutable* global
                // (JS-API: a non-Global import value may only satisfy an
                // immutable global import).
                let wrapper_cell = match name_value.kind() {
                    ValueKind::Object(object) => agent.wasm_globals.get(&object.id()).copied(),
                    _ => None,
                };
                match wrapper_cell {
                    Some(cell) => wasm::ExternVal::Global(cell),
                    None if declared.mutable => {
                        return Err(link_failure(
                            agent,
                            &format!("import '{}' is not a WebAssembly.Global", import.name),
                        )?);
                    }
                    None => {
                        // A non-Global value may only satisfy an immutable
                        // global import, and only with the exact JS kind for
                        // the value type (Number for i32/f32/f64, BigInt for
                        // i64); anything else is a LinkError, not a TypeError.
                        let kind_ok = match declared.value {
                            ValType::I32 | ValType::F32 | ValType::F64 => {
                                matches!(name_value.kind(), ValueKind::Number(_))
                            }
                            ValType::I64 => matches!(name_value.kind(), ValueKind::BigInt(_)),
                            _ => false,
                        };
                        if !kind_ok {
                            return Err(link_failure(
                                agent,
                                &format!("import '{}' is not a WebAssembly.Global", import.name),
                            )?);
                        }
                        let value = wasm_arg(agent, &declared.value, &name_value)?;
                        let ty = wasm::types::GlobalType {
                            value: declared.value,
                            mutable: false,
                        };
                        let cell = agent.wasm_store.borrow_mut().global(ty, value);
                        wasm::ExternVal::Global(cell)
                    }
                }
            }
            wasm::module::ImportDesc::Tag(_) => match resolve_tag_cell(agent, &name_value) {
                Ok(cell) => wasm::ExternVal::Tag(cell),
                Err(_) => {
                    return Err(link_failure(
                        agent,
                        &format!("import '{}' is not a WebAssembly.Tag", import.name),
                    )?);
                }
            },
        };
        resolved.push(((import.module.clone(), import.name.clone()), value));
    }
    Ok(resolved)
}

/// One JS argument → wasm value. i64 converts through ToBigInt (wrapping
/// modulo 2^64); the numeric types convert by ToNumber; references/v128 are
/// not JS-API values yet.
fn wasm_arg(agent: &mut Agent, param: &ValType, value: &Value) -> Result<WasmValue, JsError> {
    let unsupported = |name: &str| {
        JsError::new(
            ErrorKind::TypeError,
            format!("wasm {name} values are not supported yet (Cut 10 wave 3b)"),
        )
    };
    Ok(match param {
        ValType::I32 => {
            let number = to_number(value)?;
            WasmValue::I32(to_int32(number))
        }
        ValType::I64 => {
            let big = webidl_bigint(agent, value)?;
            WasmValue::I64(big.to_i64_wrapping())
        }
        ValType::F32 => {
            let number = to_number(value)? as f32;
            WasmValue::F32(number.to_bits())
        }
        ValType::F64 => WasmValue::F64(to_number(value)?.to_bits()),
        ValType::V128 => return Err(unsupported("v128")),
        ValType::Ref(reference) if *reference == wasm::types::RefType::EXTERN => {
            WasmValue::Ref(to_externref(agent, *value))
        }
        ValType::Ref(_) => return Err(unsupported("reference")),
    })
}

/// An imported JS function's return value → the wasm results of its declared
/// function type (JS-API 4.9.5): zero results ignore the value; a single
/// result converts it directly; multiple results iterate the value —
/// GetIterator then whole-sequence IteratorStep, converting each element by
/// the result types in order once iteration completes.
fn js_function_results(
    agent: &mut Agent,
    fty: &wasm::types::FuncType,
    value: &Value,
) -> Result<Vec<WasmValue>, JsError> {
    if fty.results.is_empty() {
        return Ok(Vec::new());
    }
    if let [single] = fty.results.as_slice() {
        return Ok(vec![wasm_arg(agent, single, value)?]);
    }
    let iterator = crate::expr::get_iterator(agent, value)?;
    let mut collected = Vec::new();
    while let Some(element) = crate::expr::iterator_step(agent, &iterator)? {
        collected.push(element);
    }
    let mut results = Vec::with_capacity(fty.results.len());
    for (index, result) in fty.results.iter().enumerate() {
        let element = collected.get(index).copied().unwrap_or(Value::Undefined);
        results.push(wasm_arg(agent, result, &element)?);
    }
    Ok(results)
}

/// One wasm result value → JS (i64 becomes a BigInt; the numeric types are
/// numbers; externref becomes the JS value it holds).
fn wasm_result(agent: &Agent, value: WasmValue) -> Result<Value, JsError> {
    Ok(match value {
        WasmValue::I32(v) => Value::Number(f64::from(v)),
        WasmValue::I64(v) => Value::BigInt(Handle::new(crux::BigInt::from(v))),
        WasmValue::F32(bits) => Value::Number(f64::from(f32::from_bits(bits))),
        WasmValue::F64(bits) => Value::Number(f64::from_bits(bits)),
        WasmValue::Ref(reference) => return externref_to_js(agent, reference),
        _ => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "wasm v128/reference results are not supported yet (Cut 10 wave 3b)".into(),
            ));
        }
    })
}

/// Call an exported wasm function from JS: flush the memory buffers in,
/// convert the arguments by the function's declared type, run it in the
/// agent's engine store, refresh the buffers, and convert the results back.
/// Turn an engine run failure into a JS exception: wasm traps surface as
/// `WebAssembly.RuntimeError`; a tagged wasm exception reifies as a
/// `WebAssembly.Exception` (its tag needs no JS wrapper), except a
/// wasm-originated JS-tag throw, which escapes as the JS value in its
/// externref payload; an engine exception that re-entered wasm from a JS
/// throw rethrows the original JS value; unsupported features stay
/// TypeErrors.
fn run_failure(agent: &mut Agent, fail: wasm::ExecFail) -> Result<JsError, JsError> {
    let message = format!("{fail:?}");
    let runtime_error = |agent: &mut Agent, reason: &str| {
        wasm_error(agent, "RuntimeError", "%WebAssembly.RuntimeError%", reason)
    };
    match fail {
        wasm::ExecFail::Trap(_) => Ok(JsError::new(ErrorKind::TypeError, message)
            .with_value(runtime_error(agent, "wasm execution trapped")?)),
        wasm::ExecFail::Exception(exn) => {
            // A JS exception that entered wasm and escaped uncaught rethrows
            // with its identity preserved.
            if let Some(original) = agent.wasm_js_exceptions.remove(&exn) {
                return Ok(JsError::new(
                    ErrorKind::TypeError,
                    "wasm rethrew a JS exception".into(),
                )
                .with_value(original));
            }
            let (cell, args) = {
                let store = agent.wasm_store.borrow();
                (store.exception_tag(exn), store.exception_args(exn))
            };
            let Some(cell) = cell else {
                return Ok(JsError::new(ErrorKind::TypeError, message)
                    .with_value(runtime_error(agent, "wasm threw an unknown exception")?));
            };
            // A wasm-originated JS-tag throw (a JS value the module received
            // as an externref and threw with the JS tag) escapes as that JS
            // value, not as a WebAssembly.Exception (JS-API 4.13).
            if agent.wasm_js_tag_cell == Some(cell) {
                let args = args.unwrap_or_default();
                if let Some(WasmValue::Ref(reference)) = args.first() {
                    return Ok(
                        JsError::new(ErrorKind::TypeError, "wasm threw a JS value".into())
                            .with_value(externref_to_js(agent, *reference)?),
                    );
                }
                return Ok(JsError::new(ErrorKind::TypeError, message)
                    .with_value(runtime_error(agent, "invalid JS-tag payload")?));
            }
            // Any tagged wasm exception reifies as a `WebAssembly.Exception`
            // (JS-API): JS needs no `WebAssembly.Tag` wrapper for the tag — an
            // internal tag that was neither imported nor exported is simply
            // one JS cannot name, so `.is`/`.getArg` on the reified object
            // stay usable only through a matching wrapper. Escaping
            // untagged/unknown engine exceptions surface as RuntimeError.
            let args = args.unwrap_or_default();
            let value = exception_object(agent, cell, args)?;
            Ok(JsError::new(ErrorKind::TypeError, "wasm exception".into()).with_value(value))
        }
        other => Ok(JsError::new(
            ErrorKind::TypeError,
            format!("wasm execution failed: {other:?}"),
        )),
    }
}

/// Call an exported wasm function from JS: flush the memory buffers in,
/// convert the arguments by the function's declared type, run it through the
/// resumable driver (parking at every external host function — a JS closure
/// the module imported — and running it with the store unborrowed, refreshing
/// the memory buffers around each host call so host JS sees wasm writes and
/// vice versa), then refresh the buffers and convert the results back.
fn invoke_export(
    agent: &mut Agent,
    args: &[Value],
    instance: usize,
    index: usize,
) -> Result<Value, JsError> {
    let ty = agent
        .wasm_store
        .borrow()
        .func_type(instance, index)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "unknown wasm function".into()))?;
    let mut wasm_args = Vec::with_capacity(args.len());
    for (position, argument) in args.iter().enumerate() {
        let param = ty.params.get(position).cloned().unwrap_or(ValType::I32);
        wasm_args.push(wasm_arg(agent, &param, argument)?);
    }
    memory_buffers_to_store(agent)?;
    let mut progress = {
        let mut store = agent.wasm_store.borrow_mut();
        store.start(instance, index, &wasm_args)
    };
    let results = loop {
        let step = match progress {
            Ok(step) => step,
            Err(fail) => {
                memory_buffers_from_store(agent)?;
                return Err(run_failure(agent, fail)?);
            }
        };
        match step {
            RunProgress::Finished(results) => break results,
            RunProgress::Host(request) => {
                // Refresh the buffers so the host JS closure sees the wasm
                // writes so far; run the closure; then flush its buffer
                // writes back into the cells before resuming wasm.
                memory_buffers_from_store(agent)?;
                let reply = {
                    let (function, fty) = agent
                        .wasm_host_functions
                        .get(&request.token)
                        .map(|(function, fty)| (*function, fty.clone()))
                        .ok_or_else(|| {
                            JsError::new(ErrorKind::TypeError, "unknown wasm host function".into())
                        })?;
                    let mut js_args = Vec::with_capacity(request.args.len());
                    for value in &request.args {
                        js_args.push(wasm_result(agent, *value)?);
                    }
                    crate::function::call(agent, &function, Value::Undefined, &js_args)
                        .map(|value| (value, fty))
                };
                memory_buffers_to_store(agent)?;
                match reply {
                    Ok((value, fty)) => {
                        let wasm_results = js_function_results(agent, &fty, &value)?;
                        progress = {
                            let mut store = agent.wasm_store.borrow_mut();
                            store.resume(Ok(wasm_results))
                        };
                    }
                    Err(error) => {
                        // A JS exception thrown by the imported function. A
                        // `WebAssembly.Exception` is delivered as an in-flight
                        // wasm exception tagged by its own tag; any other JS
                        // value is wrapped in the JS tag (`WebAssembly.JSTag`)
                        // so a matching `try_table` catch — or a `catch_all` —
                        // can intercept it. When wasm does not catch an
                        // injected exception, the escaping engine exception
                        // rethrows the original JS value with identity
                        // preserved. An error with no thrown JS value
                        // (machine-level, not user) abandons the run and
                        // propagates unchanged.
                        match error.value {
                            Some(value) => match value.kind() {
                                ValueKind::Object(object)
                                    if agent.wasm_exceptions.contains_key(&object.id()) =>
                                {
                                    let (cell, args) = agent
                                        .wasm_exceptions
                                        .get(&object.id())
                                        .map(|(cell, args)| (*cell, args.clone()))
                                        .expect("just checked");
                                    let mut store = agent.wasm_store.borrow_mut();
                                    let exn = store.new_exception(cell, args);
                                    agent.wasm_js_exceptions.insert(exn, value);
                                    progress = store.resume_exception(exn);
                                }
                                _ => {
                                    // Wrap the arbitrary JS value in the JS
                                    // tag as an externref payload.
                                    let cell = js_tag_cell(agent)?;
                                    let payload = to_externref(agent, value);
                                    let args = vec![WasmValue::Ref(payload)];
                                    let mut store = agent.wasm_store.borrow_mut();
                                    let exn = store.new_exception(cell, args);
                                    agent.wasm_js_exceptions.insert(exn, value);
                                    progress = store.resume_exception(exn);
                                }
                            },
                            None => {
                                let mut store = agent.wasm_store.borrow_mut();
                                store.abandon();
                                return Err(error);
                            }
                        }
                    }
                }
            }
        }
    };
    memory_buffers_from_store(agent)?;
    match results.as_slice() {
        [] => Ok(Value::Undefined),
        [single] => wasm_result(agent, *single),
        // A multi-value export returns a fresh Array of the converted
        // results in order (JS-API: only a function's `WebAssembly.Function`
        // wrapper reports the multi-result type; direct calls get an array).
        many => {
            let values = many
                .iter()
                .map(|value| wasm_result(agent, *value))
                .collect::<Result<Vec<_>, _>>()?;
            array_from_values(agent, &values)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::embed::Context;

    fn eval_true(context: &mut Context, source: &str) {
        let value = context
            .eval(source)
            .unwrap_or_else(|error| panic!("eval failed for {source:?}: {error}"));
        assert_eq!(
            value.as_boolean(),
            Some(true),
            "expected `true` from: {source}"
        );
    }

    /// Evaluate a statement that sets up `globalThis` state for a later
    /// assertion (panics when the script itself throws).
    fn eval_ok(context: &mut Context, source: &str) {
        context
            .eval(source)
            .unwrap_or_else(|error| panic!("eval failed for {source:?}: {error}"));
    }

    #[test]
    fn wasm_namespace_shape() {
        let mut context = Context::new().unwrap();
        eval_true(&mut context, "typeof WebAssembly === 'object'");
        eval_true(
            &mut context,
            "Object.prototype.toString.call(WebAssembly) === '[object WebAssembly]'",
        );
        for name in [
            "Module",
            "Instance",
            "Memory",
            "Table",
            "Global",
            "Tag",
            "Exception",
            "CompileError",
            "LinkError",
            "RuntimeError",
        ] {
            eval_true(
                &mut context,
                &format!("typeof WebAssembly.{name} === 'function'"),
            );
            let d = format!("Object.getOwnPropertyDescriptor(WebAssembly, '{name}')");
            eval_true(
                &mut context,
                &format!("{d}.writable && !{d}.enumerable && {d}.configurable"),
            );
            eval_true(
                &mut context,
                &format!("WebAssembly.{name}.prototype.constructor === WebAssembly.{name}"),
            );
        }
        eval_true(&mut context, "WebAssembly.validate.length === 1");
        eval_true(&mut context, "WebAssembly.Module.exports.length === 1");
        eval_true(
            &mut context,
            "WebAssembly.Module.customSections.length === 2",
        );
        eval_true(
            &mut context,
            "WebAssembly.Global.prototype.valueOf.length === 0",
        );
        eval_true(
            &mut context,
            "Object.getOwnPropertyDescriptor(WebAssembly.Instance.prototype, 'exports').get.name === 'get exports'",
        );
        eval_true(
            &mut context,
            "WebAssembly.Module.prototype[Symbol.toStringTag] === 'WebAssembly.Module'",
        );
    }

    #[test]
    fn validate_reports_wellformedness() {
        let mut context = Context::new().unwrap();
        eval_true(
            &mut context,
            "WebAssembly.validate(new Uint8Array([0,97,115,109,1,0,0,0]))",
        );
        eval_true(
            &mut context,
            "WebAssembly.validate(new Uint8Array([0,1,2])) === false",
        );
        eval_true(
            &mut context,
            "WebAssembly.validate(new ArrayBuffer(8)) === false",
        );
        eval_true(
            &mut context,
            "(function(){ try { WebAssembly.validate({}); return false; } catch (e) { return e instanceof TypeError; } })()",
        );
    }

    #[test]
    fn wasm_error_classes_are_native_errors() {
        let mut context = Context::new().unwrap();
        eval_true(
            &mut context,
            "new WebAssembly.CompileError('x') instanceof Error",
        );
        eval_true(
            &mut context,
            "new WebAssembly.CompileError('x') instanceof WebAssembly.CompileError",
        );
        eval_true(
            &mut context,
            "new WebAssembly.CompileError('boom').name === 'CompileError'",
        );
        eval_true(
            &mut context,
            "new WebAssembly.CompileError('boom').message === 'boom'",
        );
        eval_true(
            &mut context,
            "new WebAssembly.RuntimeError().message === ''",
        );
    }

    #[test]
    fn module_compiles_and_accessors_report_records() {
        let mut context = Context::new().unwrap();
        // Empty module: compiles, class-stringifies, and has empty descriptor
        // lists and no custom sections.
        eval_true(
            &mut context,
            concat!(
                "(function(){ const m = new WebAssembly.Module(new Uint8Array([0,97,115,109,1,0,0,0]));",
                " return m instanceof WebAssembly.Module && Object.prototype.toString.call(m) === '[object WebAssembly.Module]'; })()"
            ),
        );
        eval_true(
            &mut context,
            concat!(
                "(function(){ const m = new WebAssembly.Module(new Uint8Array([0,97,115,109,1,0,0,0]));",
                " return WebAssembly.Module.exports(m).length === 0 && WebAssembly.Module.imports(m).length === 0",
                " && WebAssembly.Module.customSections(m, '').length === 0; })()"
            ),
        );
        // Fresh arrays on every accessor call.
        eval_true(
            &mut context,
            concat!(
                "(function(){ const m = new WebAssembly.Module(new Uint8Array([0,97,115,109,1,0,0,0]));",
                " return WebAssembly.Module.exports(m) !== WebAssembly.Module.exports(m); })()"
            ),
        );
        // Garbage bytes throw CompileError, not TypeError.
        eval_true(
            &mut context,
            concat!(
                "(function(){ try { new WebAssembly.Module(new Uint8Array([0,1,2])); return false; }",
                " catch (e) { return e instanceof WebAssembly.CompileError; } })()"
            ),
        );
        // Missing/invalid arguments throw TypeError.
        eval_true(
            &mut context,
            "(function(){ try { new WebAssembly.Module(); return false; } catch (e) { return e instanceof TypeError; } })()",
        );
        eval_true(
            &mut context,
            concat!(
                "(function(){ const m = new WebAssembly.Module(new Uint8Array([0,97,115,109,1,0,0,0]));",
                " try { WebAssembly.Module.exports({}); return false; } catch (e) { return e instanceof TypeError; } })()"
            ),
        );
        // Calling without `new` throws TypeError.
        eval_true(
            &mut context,
            concat!(
                "(function(){ try { WebAssembly.Module(new Uint8Array([0,97,115,109,1,0,0,0])); return false; }",
                " catch (e) { return e instanceof TypeError; } })()"
            ),
        );
        // customSections returns fresh ArrayBuffers with the section bytes.
        eval_true(
            &mut context,
            concat!(
                "(function(){ const b = [0,97,115,109,1,0,0,0, 0,6, 3,102,111,111, 170,187];",
                " const m = new WebAssembly.Module(new Uint8Array(b));",
                " const s = WebAssembly.Module.customSections(m, 'foo');",
                " if (s.length !== 1) return false;",
                " const u = new Uint8Array(s[0]);",
                " return u.length === 2 && u[0] === 170 && u[1] === 187; })()"
            ),
        );
        // The section-name argument is required.
        eval_true(
            &mut context,
            concat!(
                "(function(){ const m = new WebAssembly.Module(new Uint8Array([0,97,115,109,1,0,0,0]));",
                " try { WebAssembly.Module.customSections(m); return false; }",
                " catch (e) { return e instanceof TypeError; } })()"
            ),
        );
    }

    #[test]
    fn instance_exports_run_numeric_functions() {
        let mut context = Context::new().unwrap();
        // A minimal module exporting `add(i32, i32) -> i32`.
        let module_bytes = concat!(
            "[0,97,115,109,1,0,0,0",
            // type section: (func (param i32 i32) (result i32))
            ",1,7,1,96,2,127,127,1,127",
            // function section: one function of type 0
            ",3,2,1,0",
            // export section: "add" -> func 0
            ",7,7,1,3,97,100,100,0,0",
            // code section: local.get 0, local.get 1, i32.add, end
            ",10,9,1,7,0,32,0,32,1,106,11]"
        );
        let wrap = |head: &str, tail: &'static str| format!("{head}{module_bytes}{tail}");
        eval_true(
            &mut context,
            &wrap(
                "(function(){ const bytes = new Uint8Array(",
                concat!(
                    "); const m = new WebAssembly.Module(bytes);",
                    " const i = new WebAssembly.Instance(m);",
                    " if (!(i instanceof WebAssembly.Instance)) return false;",
                    " if (typeof i.exports.add !== 'function') return false;",
                    " if (i.exports.add(2, 3) !== 5) return false;",
                    " if (i.exports.add(-1, 1) !== 0) return false;",
                    " return i.exports.add === i.exports.add; })()"
                ),
            ),
        );
        // Instantiating from raw bytes (compiling on the fly) works too.
        eval_true(
            &mut context,
            &wrap(
                "(function(){ const bytes = new Uint8Array(",
                concat!(
                    "); const i = new WebAssembly.Instance(bytes);",
                    " return i.exports.add(40, 2) === 42; })()"
                ),
            ),
        );
        // Exported-function wrappers reuse the memoized dispatch (warm path).
        eval_true(
            &mut context,
            &wrap(
                "(function(){ const bytes = new Uint8Array(",
                concat!(
                    "); const i = new WebAssembly.Instance(bytes);",
                    " const f = i.exports.add; return f(2,3) === 5 && f(3,4) === 7; })()"
                ),
            ),
        );
    }

    // ---- wave 3b slice 1: the Memory wrapper + the JS<->wasm buffer bridge ----

    /// Compile `.wat` module text to a JS `Uint8Array([..])` byte literal via
    /// the in-process wast encoder (the one wasmtest's converter uses).
    fn wat_module_bytes(source: &str) -> String {
        let buffer = wast::parser::ParseBuffer::new(source).expect("wat parse buffer");
        let mut module: wast::Wat = wast::parser::parse(&buffer).expect("wat parse");
        let bytes = module.encode().expect("wat encode");
        let mut literal = String::with_capacity(bytes.len() * 4);
        for (index, byte) in bytes.iter().enumerate() {
            if index > 0 {
                literal.push(',');
            }
            literal.push_str(&byte.to_string());
        }
        literal
    }

    #[test]
    fn memory_wrapper_grows_and_detaches_buffers() {
        let mut context = Context::new().unwrap();
        eval_true(
            &mut context,
            concat!(
                "(function(){ const m = new WebAssembly.Memory({ initial: 1 });",
                " if (!(m instanceof WebAssembly.Memory)) return false;",
                " if (Object.prototype.toString.call(m) !== '[object WebAssembly.Memory]') return false;",
                " if (!(m.buffer instanceof ArrayBuffer)) return false;",
                " if (m.buffer.byteLength !== 65536) return false;",
                " if (m.buffer !== m.buffer) return false;",
                " const old = m.buffer;",
                " if (m.grow(1) !== 1) return false;",
                " if (old.byteLength !== 0) return false;",
                " if (m.buffer === old) return false;",
                " if (m.buffer.byteLength !== 131072) return false;",
                " return true; })()"
            ),
        );
        // Grow beyond the declared maximum throws RangeError.
        eval_true(
            &mut context,
            concat!(
                "(function(){ const m = new WebAssembly.Memory({ initial: 1, maximum: 1 });",
                " try { m.grow(1); return false; } catch (e) { return e instanceof RangeError; } })()"
            ),
        );
        // Descriptor rules: object required, `initial` required, and a
        // maximum below the initial is a RangeError.
        eval_true(
            &mut context,
            concat!(
                "(function(){",
                " try { new WebAssembly.Memory(); return false; } catch (e) { if (!(e instanceof TypeError)) return false; }",
                " try { new WebAssembly.Memory({ maximum: 2 }); return false; } catch (e) { if (!(e instanceof TypeError)) return false; }",
                " try { new WebAssembly.Memory({ initial: 3, maximum: 2 }); return false; } catch (e) { return e instanceof RangeError; }",
                "})()"
            ),
        );
        // A shared memory is a frozen SharedArrayBuffer whose grow keeps the
        // old buffer attached and aliasing (the JS-API shared grow rule).
        eval_true(
            &mut context,
            concat!(
                "(function(){ const m = new WebAssembly.Memory({ initial: 1, maximum: 2, shared: true });",
                " const sab = m.buffer;",
                " if (Object.prototype.toString.call(sab) !== '[object SharedArrayBuffer]') return false;",
                " if (Object.isFrozen(sab) || !Object.isExtensible(sab)) return false;",
                " if (m.grow(1) !== 1) return false;",
                " const cur = m.buffer;",
                " if (sab === cur || sab.byteLength !== 65536 || cur.byteLength !== 131072) return false;",
                " const a = new Uint8Array(sab); const b = new Uint8Array(cur);",
                " a[0] = 5;",
                " return b[0] === 5 && a[0] === 5; })()"
            ),
        );
        // A shared memory still reads and writes through wasm (the bridge
        // aliases the block).
        let shared_io = wat_module_bytes(concat!(
            "(module",
            "  (import \"js\" \"mem\" (memory 1))",
            "  (func (export \"store\") (param i32 i32)",
            "    local.get 0",
            "    local.get 1",
            "    i32.store)",
            "  (func (export \"load\") (param i32) (result i32)",
            "    local.get 0",
            "    i32.load))"
        ));
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const mem = new WebAssembly.Memory({{ initial: 1, maximum: 2, shared: true }});",
                    " const i = new WebAssembly.Instance(new Uint8Array({shared_io}), {{ js: {{ mem }} }});",
                    " const view = new Int32Array(mem.buffer);",
                    " i.exports.store(0, 77);",
                    " if (view[0] !== 77) return false;",
                    " view[1] = 88;",
                    " return i.exports.load(4) === 88; }})()"
                ),
                shared_io = shared_io,
            ),
        );
        // The `buffer` accessor brand-checks its receiver.
        eval_true(
            &mut context,
            concat!(
                "(function(){ try {",
                " Object.getOwnPropertyDescriptor(WebAssembly.Memory.prototype, 'buffer').get.call({});",
                " return false; } catch (e) { return e instanceof TypeError; } })()"
            ),
        );
    }

    #[test]
    fn memory64_and_table64_index_objects() {
        // Memory64/table64 descriptors (`address: "i64"`) use BigInt page
        // counts and grow deltas; grow returns a BigInt. Grow/limit argument
        // errors are TypeErrors (WebIDL EnforceRange), exceeding a maximum a
        // RangeError; i32-address grow rejects BigInts and out-of-range
        // Numbers with a TypeError.
        let mut context = Context::new().unwrap();
        eval_true(
            &mut context,
            concat!(
                "(function(){",
                " const m = new WebAssembly.Memory({ address: 'i64', initial: 1n, maximum: 3n });",
                " if (m.buffer.byteLength !== 65536) return false;",
                " if (m.grow(1n) !== 1n) return false;",
                " if (m.buffer.byteLength !== 131072) return false;",
                " try { m.grow(4n); return false; } catch (e) { if (!(e instanceof RangeError)) return false; }",
                " try { m.grow(-1n); return false; } catch (e) { if (!(e instanceof TypeError)) return false; }",
                " try { m.grow(2); return false; } catch (e) { if (!(e instanceof TypeError)) return false; }",
                " const t = new WebAssembly.Table({ element: 'funcref', address: 'i64', initial: 2n });",
                " if (t.length !== 2n) return false;",
                " if (t.get(0n) !== null) return false;",
                " if (t.grow(1n) !== 2n) return false;",
                " if (t.length !== 3n) return false;",
                " try { t.set(0n, 42); return false; } catch (e) { if (!(e instanceof TypeError)) return false; }",
                " try { t.get(3n); return false; } catch (e) { if (!(e instanceof RangeError)) return false; }",
                " try { new WebAssembly.Table({ element: 'funcref', address: 'i64', initial: 2n, maximum: 1n }); return false; }",
                " catch (e) { if (!(e instanceof RangeError)) return false; }",
                " try { new WebAssembly.Memory({ address: 'bogus', initial: 0 }); return false; }",
                " catch (e) { if (!(e instanceof TypeError)) return false; }",
                " const g32 = new WebAssembly.Memory({ initial: 0 });",
                " try { g32.grow(); return false; } catch (e) { if (!(e instanceof TypeError)) return false; }",
                " try { g32.grow(-1); return false; } catch (e) { if (!(e instanceof TypeError)) return false; }",
                " try { g32.grow(1n); return false; } catch (e) { if (!(e instanceof TypeError)) return false; }",
                " return true; })()"
            ),
        );
    }

    #[test]
    fn memory_export_roundtrips_bytes_with_wasm() {
        let mut context = Context::new().unwrap();
        let module = wat_module_bytes(concat!(
            "(module",
            "  (memory (export \"mem\") 1)",
            "  (func (export \"load\") (param i32) (result i32)",
            "    local.get 0",
            "    i32.load)",
            "  (func (export \"store\") (param i32 i32)",
            "    local.get 0",
            "    local.get 1",
            "    i32.store))"
        ));
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const bytes = new Uint8Array({module});",
                    " const i = new WebAssembly.Instance(bytes);",
                    " if (!(i.exports.mem instanceof WebAssembly.Memory)) return false;",
                    " const view = new Int32Array(i.exports.mem.buffer);",
                    " view[0] = 7;",
                    " if (i.exports.load(0) !== 7) return false;",
                    " i.exports.store(4, 99);",
                    " if (view[1] !== 99) return false;",
                    " return i.exports.load(4) === 99; }})()"
                ),
                module = module,
            ),
        );
    }

    #[test]
    fn memory_imports_link_and_alias_the_js_cell() {
        let mut context = Context::new().unwrap();
        let importer = wat_module_bytes(concat!(
            "(module",
            "  (import \"js\" \"mem\" (memory 1))",
            "  (func (export \"write\") (param i32 i32)",
            "    local.get 0",
            "    local.get 1",
            "    i32.store)",
            "  (func (export \"read\") (param i32) (result i32)",
            "    local.get 0",
            "    i32.load))"
        ));
        // A JS-created memory backs the import: wasm writes reach the
        // buffer, and buffer writes reach wasm.
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const mem = new WebAssembly.Memory({{ initial: 1 }});",
                    " const i = new WebAssembly.Instance(new WebAssembly.Module(new Uint8Array({importer})), {{ js: {{ mem }} }});",
                    " i.exports.write(8, 1234);",
                    " if (new Int32Array(mem.buffer)[2] !== 1234) return false;",
                    " new Int32Array(mem.buffer)[3] = 5;",
                    " if (i.exports.read(12) !== 5) return false;",
                    " return i.exports.read(8) === 1234; }})()"
                ),
                importer = importer,
            ),
        );
        // An exported memory of another instance links the same way (cells
        // alias across modules through the store).
        let exporter = wat_module_bytes(concat!(
            "(module",
            "  (memory (export \"mem\") 1)",
            "  (func (export \"store\") (param i32 i32)",
            "    local.get 0",
            "    local.get 1",
            "    i32.store))"
        ));
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const exporter = new WebAssembly.Instance(new Uint8Array({exporter}));",
                    " const importer = new WebAssembly.Instance(new Uint8Array({importer}), {{ js: {{ mem: exporter.exports.mem }} }});",
                    " importer.exports.write(16, 5);",
                    " if (new Int32Array(exporter.exports.mem.buffer)[4] !== 5) return false;",
                    " return true; }})()"
                ),
                exporter = exporter,
                importer = importer,
            ),
        );
        // A module with imports needs an imports object (TypeError per the
        // JS-API argument checks); an import value of the wrong kind is a
        // LinkError.
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const m = new WebAssembly.Module(new Uint8Array({importer}));",
                    " try {{ new WebAssembly.Instance(m); return false; }}",
                    " catch (e) {{ if (!(e instanceof TypeError)) return false; }}",
                    " try {{ new WebAssembly.Instance(m, {{ js: {{ mem: 42 }} }}); return false; }}",
                    " catch (e) {{ return e instanceof WebAssembly.LinkError; }} }})()"
                ),
                importer = importer,
            ),
        );
    }

    // ---- wave 3b slice 2: Table/Global wrappers + JS function imports ----

    #[test]
    fn global_wrapper_and_module_cells() {
        let mut context = Context::new().unwrap();
        eval_true(
            &mut context,
            concat!(
                "(function(){ const g = new WebAssembly.Global({ value: 'i32', mutable: true }, 5);",
                " if (!(g instanceof WebAssembly.Global)) return false;",
                " if (Object.prototype.toString.call(g) !== '[object WebAssembly.Global]') return false;",
                " if (g.value !== 5 || g.valueOf() !== 5) return false;",
                " g.value = 9;",
                " if (g.value !== 9) return false;",
                " const im = new WebAssembly.Global({ value: 'i32' });",
                " if (im.value !== 0) return false;",
                " try { im.value = 1; return false; } catch (e) { if (!(e instanceof TypeError)) return false; }",
                // An immutable set must reject before any ToNumber of the
                // argument (spec): the value's valueOf never runs.
                " let converted = false;",
                " try { im.value = { valueOf: function () { converted = true; return 2; } }; return false; }",
                " catch (e) { if (!(e instanceof TypeError)) return false; }",
                " if (converted || im.value !== 0) return false;",
                // i64 globals convert their initial value through ToBigInt.
                " const big = new WebAssembly.Global({ value: 'i64' }, 1n);",
                " if (big.value !== 1n || big.valueOf() !== 1n) return false;",
                " const bigDefault = new WebAssembly.Global({ value: 'i64' });",
                " if (bigDefault.value !== 0n) return false;",
                " try { new WebAssembly.Global({ value: 'i64' }, 1); return false; }",
                " catch (e) { if (!(e instanceof TypeError)) return false; }",
                " return true;",
                "})()"
            ),
        );
        // A module exporting a mutable global: the exported function and the
        // `WebAssembly.Global` wrapper share one cell.
        let module = wat_module_bytes(concat!(
            "(module",
            "  (global (export \"g\") (mut i32) (i32.const 7))",
            "  (func (export \"read\") (result i32) global.get 0)",
            "  (func (export \"write\") (param i32) local.get 0 global.set 0))"
        ));
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const i = new WebAssembly.Instance(new Uint8Array({module}));",
                    " if (!(i.exports.g instanceof WebAssembly.Global)) return false;",
                    " if (i.exports.g.value !== 7 || i.exports.read() !== 7) return false;",
                    " i.exports.write(11);",
                    " if (i.exports.g.value !== 11) return false;",
                    " i.exports.g.value = 20;",
                    " return i.exports.read() === 20; }})()"
                ),
                module = module,
            ),
        );
        // A module importing a JS-created mutable global aliases its cell.
        let importer = wat_module_bytes(concat!(
            "(module",
            "  (import \"js\" \"g\" (global (mut i32)))",
            "  (func (export \"read\") (result i32) global.get 0)",
            "  (func (export \"write\") (param i32) local.get 0 global.set 0))"
        ));
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const g = new WebAssembly.Global({{ value: 'i32', mutable: true }}, 5);",
                    " const i = new WebAssembly.Instance(new WebAssembly.Module(new Uint8Array({importer})), {{ js: {{ g }} }});",
                    " if (i.exports.read() !== 5) return false;",
                    " i.exports.write(30);",
                    " return g.value === 30; }})()"
                ),
                importer = importer,
            ),
        );
    }

    #[test]
    fn global_descriptor_order_of_evaluation() {
        // The descriptor's `mutable` member is read first, then `value`
        // (ToString), then the second argument is converted (spec): the
        // getters run in that order.
        let mut context = Context::new().unwrap();
        eval_true(
            &mut context,
            concat!(
                "(function(){",
                " const order = [];",
                " new WebAssembly.Global({",
                "   get mutable() { order.push('descriptor mutable'); return false; },",
                "   get value() {",
                "     order.push('descriptor value');",
                "     return { toString() { order.push('descriptor value toString'); return 'f64'; } };",
                "   },",
                " }, { valueOf() { order.push('value valueOf()'); } });",
                " return order.join('|') === 'descriptor mutable|descriptor value|descriptor value toString|value valueOf()';",
                "})()"
            ),
        );
    }

    #[test]
    fn table_wrapper_and_module_cells() {
        let mut context = Context::new().unwrap();
        eval_true(
            &mut context,
            concat!(
                "(function(){",
                " const t = new WebAssembly.Table({ element: 'funcref', initial: 1 });",
                " if (!(t instanceof WebAssembly.Table)) return false;",
                " if (Object.prototype.toString.call(t) !== '[object WebAssembly.Table]') return false;",
                " if (t.length !== 1) return false;",
                " if (t.get(0) !== null) return false;",
                " if (t.grow(2) !== 1 || t.length !== 3) return false;",
                " try { t.set(0, 42); return false; } catch (e) { if (!(e instanceof TypeError)) return false; }",
                " return true; })()"
            ),
        );
        // An externref table holds JS values (identity preserved), with null
        // as the initial element and undefined round-tripping distinctly.
        eval_true(
            &mut context,
            concat!(
                "(function(){ const et = new WebAssembly.Table({ element: 'externref', initial: 1 });",
                " if (et.get(0) !== null) return false;",
                " const obj = { x: 1 };",
                " et.set(0, obj);",
                " if (et.get(0) !== obj) return false;",
                " et.set(0, undefined);",
                " if (et.get(0) !== undefined) return false;",
                " return true; })()"
            ),
        );
        let producer = wat_module_bytes(concat!(
            "(module",
            "  (type $t (func (param i32) (result i32)))",
            "  (func $double (export \"double\") (type $t) (param i32) (result i32)",
            "    local.get 0",
            "    i32.const 2",
            "    i32.mul)",
            "  (table (export \"tab\") 2 funcref)",
            "  (elem (i32.const 0) $double))"
        ));
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const i = new WebAssembly.Instance(new Uint8Array({producer}));",
                    " if (!(i.exports.tab instanceof WebAssembly.Table)) return false;",
                    " if (i.exports.tab.length !== 2) return false;",
                    " const f = i.exports.tab.get(0);",
                    " if (typeof f !== 'function' || f(21) !== 42) return false;",
                    " if (f !== i.exports.double) return false;",
                    " i.exports.tab.set(1, i.exports.double);",
                    " if (i.exports.tab.get(1) !== i.exports.double) return false;",
                    " if (i.exports.tab.grow(1) !== 2 || i.exports.tab.length !== 3) return false;",
                    " i.exports.tab.set(2, null);",
                    " return i.exports.tab.get(2) === null; }})()"
                ),
                producer = producer,
            ),
        );
        // A JS-created Table imported into a module works through
        // call_indirect (inter-instance, no host call).
        let consumer = wat_module_bytes(concat!(
            "(module",
            "  (type $t (func (param i32) (result i32)))",
            "  (import \"js\" \"tab\" (table 1 funcref))",
            "  (func (export \"apply\") (param i32) (result i32)",
            "    local.get 0",
            "    i32.const 0",
            "    call_indirect (type $t)))"
        ));
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const producer = new WebAssembly.Instance(new Uint8Array({producer}));",
                    " const tab = new WebAssembly.Table({{ element: 'funcref', initial: 1 }});",
                    " tab.set(0, producer.exports.double);",
                    " const consumer = new WebAssembly.Instance(new WebAssembly.Module(new Uint8Array({consumer})), {{ js: {{ tab }} }});",
                    " return consumer.exports.apply(21) === 42; }})()"
                ),
                producer = producer,
                consumer = consumer,
            ),
        );
    }

    #[test]
    fn function_imports_run_js_closures() {
        let mut context = Context::new().unwrap();
        // A module calling an imported JS closure.
        let double = wat_module_bytes(concat!(
            "(module",
            "  (import \"js\" \"double\" (func $double (param i32) (result i32)))",
            "  (func (export \"run\") (param i32) (result i32)",
            "    local.get 0",
            "    call $double))"
        ));
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ let calls = 0;",
                    " const i = new WebAssembly.Instance(new WebAssembly.Module(new Uint8Array({double})), {{ js: {{ double: (x) => {{ calls++; return x * 2; }} }} }});",
                    " if (i.exports.run(21) !== 42) return false;",
                    " if (i.exports.run(1) !== 2) return false;",
                    " return calls === 2; }})()"
                ),
                double = double,
            ),
        );
        // Reentrancy: the closure calls another wasm export while the outer
        // wasm run is suspended.
        let inner_module = wat_module_bytes(concat!(
            "(module",
            "  (func (export \"add\") (param i32 i32) (result i32)",
            "    local.get 0",
            "    local.get 1",
            "    i32.add))"
        ));
        let outer_module = wat_module_bytes(concat!(
            "(module",
            "  (import \"js\" \"id\" (func $id (param i32) (result i32)))",
            "  (func (export \"run\") (param i32) (result i32)",
            "    local.get 0",
            "    call $id))"
        ));
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const inner = new WebAssembly.Instance(new Uint8Array({inner_module}));",
                    " const outer = new WebAssembly.Instance(new WebAssembly.Module(new Uint8Array({outer_module})),",
                    "   {{ js: {{ id: (x) => inner.exports.add(x, 2) }} }});",
                    " return outer.exports.run(40) === 42; }})()"
                ),
                inner_module = inner_module,
                outer_module = outer_module,
            ),
        );
        // A throwing closure abandons the run and propagates to JS.
        let boom = wat_module_bytes(concat!(
            "(module",
            "  (import \"js\" \"boom\" (func $boom))",
            "  (func (export \"run\") (result i32)",
            "    call $boom",
            "    i32.const 1))"
        ));
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const i = new WebAssembly.Instance(new WebAssembly.Module(new Uint8Array({boom})),",
                    "   {{ js: {{ boom: () => {{ throw new TypeError('boom'); }} }} }});",
                    " try {{ i.exports.run(); return false; }} catch (e) {{ return e instanceof TypeError && e.message === 'boom'; }} }})()"
                ),
                boom = boom,
            ),
        );
        // A closure reading the imported memory sees the wasm writes (the
        // buffer bridge runs around each host call).
        let readmem = wat_module_bytes(concat!(
            "(module",
            "  (import \"js\" \"mem\" (memory 1))",
            "  (import \"js\" \"load\" (func $load (result i32)))",
            "  (func (export \"run\") (result i32)",
            "    call $load))"
        ));
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const mem = new WebAssembly.Memory({{ initial: 1 }});",
                    " new Int32Array(mem.buffer)[0] = 42;",
                    " const i = new WebAssembly.Instance(new WebAssembly.Module(new Uint8Array({readmem})),",
                    "   {{ js: {{ mem, load: () => new Int32Array(mem.buffer)[0] }} }});",
                    " return i.exports.run() === 42; }})()"
                ),
                readmem = readmem,
            ),
        );
    }

    // ---- wave 4 slice 1: promise-returning compile/instantiate ----

    #[test]
    fn multi_value_results_cross_the_js_boundary() {
        let mut context = Context::new().unwrap();
        // An export with two results returns a fresh Array from JS.
        let swap = wat_module_bytes(concat!(
            "(module",
            "  (func (export \"swap\") (param f64 i32) (result i32 f64)",
            "    local.get 1",
            "    local.get 0",
            "    return))"
        ));
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const i = new WebAssembly.Instance(new Uint8Array({swap}));",
                    " const swapped = i.exports.swap(4.2, 7);",
                    " return Array.isArray(swapped)",
                    "   && Object.getPrototypeOf(swapped) === Array.prototype",
                    "   && swapped.length === 2 && swapped[0] === 7 && swapped[1] === 4.2; }})()"
                ),
                swap = swap,
            ),
        );
        // An imported JS function with two results is iterated (GetIterator +
        // IteratorStep) and its elements converted by the result types.
        let callfn = wat_module_bytes(concat!(
            "(module",
            "  (type $t (func (param f64 i32) (result i32 f64)))",
            "  (import \"js\" \"fn\" (func $fn (type $t)))",
            "  (func (export \"run\") (result i32)",
            "    f64.const 4.2",
            "    i32.const 7",
            "    call $fn",
            "    drop",
            "    return))"
        ));
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const seen = [];",
                    " const fn = () => ({{ get [Symbol.iterator]() {{ seen.push('iter'); return function() {{ let n = 0;",
                    "   const values = [2, 7.3];",
                    "   return {{ next: () => {{ seen.push('next'); const done = n >= values.length;",
                    "     const value = done ? undefined : values[n++];",
                    "     return {{ done, value: {{ valueOf: () => value }} }}; }} }}; }}; }} }});",
                    " const i = new WebAssembly.Instance(new Uint8Array({callfn}), {{ js: {{ fn }} }});",
                    " const out = i.exports.run();",
                    " return out === 2 && seen.join(',') === 'iter,next,next,next'; }})()"
                ),
                callfn = callfn,
            ),
        );
    }

    #[test]
    fn compile_and_instantiate_return_promises() {
        let mut context = Context::new().unwrap();
        let add_module = wat_module_bytes(concat!(
            "(module",
            "  (func (export \"add\") (param i32 i32) (result i32)",
            "    local.get 0",
            "    local.get 1",
            "    i32.add))"
        ));
        let import_module = wat_module_bytes(concat!(
            "(module",
            "  (import \"js\" \"double\" (func $double (param i32) (result i32)))",
            "  (func (export \"run\") (param i32) (result i32)",
            "    local.get 0",
            "    call $double))"
        ));
        // compile resolves with a Module (and returns a promise immediately).
        eval_true(
            &mut context,
            &format!("WebAssembly.compile(new Uint8Array({add_module})) instanceof Promise"),
        );
        eval_ok(
            &mut context,
            &format!(
                concat!(
                    "globalThis.__w = 'pending';",
                    "(async () => {{ const m = await WebAssembly.compile(new Uint8Array({add_module}));",
                    " globalThis.__w = m instanceof WebAssembly.Module ? 'ok' : 'bad'; }})();"
                ),
                add_module = add_module,
            ),
        );
        eval_true(&mut context, "globalThis.__w === 'ok'");
        // compile failures reject (CompileError for bad bytes, TypeError for
        // a non-BufferSource argument).
        eval_ok(
            &mut context,
            concat!(
                "globalThis.__e = 'none';",
                "(async () => { try { await WebAssembly.compile(new Uint8Array([0, 1, 2])); }",
                " catch (e) { globalThis.__e = e instanceof WebAssembly.CompileError ? 'ok' : 'bad'; } })();"
            ),
        );
        eval_true(&mut context, "globalThis.__e === 'ok'");
        eval_ok(
            &mut context,
            concat!(
                "globalThis.__e = 'none';",
                "(async () => { try { await WebAssembly.compile({}); }",
                " catch (e) { globalThis.__e = e instanceof TypeError ? 'ok' : 'bad'; } })();"
            ),
        );
        eval_true(&mut context, "globalThis.__e === 'ok'");
        // instantiate(bytes) resolves with { module, instance } whose exports
        // run (and a wasm trap surfaces as RuntimeError from the call).
        eval_ok(
            &mut context,
            &format!(
                concat!(
                    "globalThis.__w = 'pending';",
                    "(async () => {{ const r = await WebAssembly.instantiate(new Uint8Array({add_module}));",
                    " globalThis.__w = (r.module instanceof WebAssembly.Module",
                    "   && r.instance instanceof WebAssembly.Instance",
                    "   && r.instance.exports.add(2, 3) === 5) ? 'ok' : 'bad'; }})();"
                ),
                add_module = add_module,
            ),
        );
        eval_true(&mut context, "globalThis.__w === 'ok'");
        // instantiate(module, imports?) resolves with the Instance.
        eval_ok(
            &mut context,
            &format!(
                concat!(
                    "globalThis.__w = 'pending';",
                    "(async () => {{ const m = new WebAssembly.Module(new Uint8Array({add_module}));",
                    " const i = await WebAssembly.instantiate(m);",
                    " globalThis.__w = (i instanceof WebAssembly.Instance && i.exports.add(40, 2) === 42) ? 'ok' : 'bad'; }})();"
                ),
                add_module = add_module,
            ),
        );
        eval_true(&mut context, "globalThis.__w === 'ok'");
        // instantiate(bytes, imports) links JS closures. A module that
        // declares imports but is instantiated with no imports argument
        // rejects with a TypeError (JS-API argument check).
        eval_ok(
            &mut context,
            &format!(
                concat!(
                    "globalThis.__w = 'pending';",
                    "(async () => {{ const r = await WebAssembly.instantiate(new Uint8Array({import_module}),",
                    "   {{ js: {{ double: (x) => x * 3 }} }});",
                    " globalThis.__w = r.instance.exports.run(14) === 42 ? 'ok' : 'bad'; }})();"
                ),
                import_module = import_module,
            ),
        );
        eval_true(&mut context, "globalThis.__w === 'ok'");
        eval_ok(
            &mut context,
            &format!(
                concat!(
                    "globalThis.__e = 'none';",
                    "(async () => {{ try {{ await WebAssembly.instantiate(new Uint8Array({import_module})); }}",
                    " catch (e) {{ globalThis.__e = e instanceof TypeError ? 'ok' : 'bad'; }} }})();"
                ),
                import_module = import_module,
            ),
        );
        eval_true(&mut context, "globalThis.__e === 'ok'");
        // A trap while running an exported function is a RuntimeError.
        let trap_module =
            wat_module_bytes(concat!("(module", "  (func (export \"run\") unreachable))"));
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const i = new WebAssembly.Instance(new Uint8Array({trap_module}));",
                    " try {{ i.exports.run(); return false; }} catch (e) {{ return e instanceof WebAssembly.RuntimeError; }} }})()"
                ),
                trap_module = trap_module,
            ),
        );
    }

    // ---- wave 4 slice 2: WebAssembly.Tag / WebAssembly.Exception ----

    #[test]
    fn tag_and_exception_objects() {
        let mut context = Context::new().unwrap();
        eval_true(
            &mut context,
            concat!(
                "(function(){",
                " const tag = new WebAssembly.Tag({ parameters: ['i32'] });",
                " if (!(tag instanceof WebAssembly.Tag)) return false;",
                " if (Object.prototype.toString.call(tag) !== '[object WebAssembly.Tag]') return false;",
                " const e = new WebAssembly.Exception(tag, [5]);",
                " if (!(e instanceof WebAssembly.Exception)) return false;",
                " if (Object.prototype.toString.call(e) !== '[object WebAssembly.Exception]') return false;",
                " if (!e.is(tag)) return false;",
                " if (e.getArg(tag, 0) !== 5) return false;",
                // `is` requires a Tag argument (spec): a non-Tag value or a
                // missing argument is a TypeError, a different Tag is false.
                " try { e.is({}); return false; } catch (err) { if (!(err instanceof TypeError)) return false; }",
                " try { e.is(); return false; } catch (err) { if (!(err instanceof TypeError)) return false; }",
                " if (e.is(new WebAssembly.Tag({ parameters: ['f64'] }))) return false;",
                " try { e.getArg(new WebAssembly.Tag({ parameters: ['f32'] }), 0); return false; }",
                " catch (err) { if (!(err instanceof TypeError)) return false; }",
                " try { e.getArg(tag, 1); return false; } catch (err) { if (!(err instanceof RangeError)) return false; }",
                " try { new WebAssembly.Exception(tag, [1, 2]); return false; } catch (err) { if (!(err instanceof TypeError)) return false; }",
                " try { new WebAssembly.Tag({}); return false; } catch (err) { if (!(err instanceof TypeError)) return false; }",
                // i64 tag parameters convert payloads through BigInt.
                " const i64Tag = new WebAssembly.Tag({ parameters: ['i64'] });",
                " const i64e = new WebAssembly.Exception(i64Tag, [42n]);",
                " if (i64e.getArg(i64Tag, 0) !== 42n) return false;",
                " try { new WebAssembly.Exception(i64Tag, [42]); return false; }",
                " catch (err) { if (!(err instanceof TypeError)) return false; }",
                " try { new WebAssembly.Exception({}, [1]); return false; } catch (err) { if (!(err instanceof TypeError)) return false; }",
                " return true;",
                "})()"
            ),
        );
    }

    #[test]
    fn wasm_exceptions_surface_as_js_and_pass_through() {
        let mut context = Context::new().unwrap();
        // A module importing a JS-created Tag and throwing it: the escaping
        // exception surfaces as a catchable WebAssembly.Exception.
        let thrower = wat_module_bytes(concat!(
            "(module",
            "  (import \"js\" \"tag\" (tag $t (param i32)))",
            "  (func (export \"run\") (param i32)",
            "    local.get 0",
            "    throw $t))"
        ));
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const tag = new WebAssembly.Tag({{ parameters: ['i32'] }});",
                    " const i = new WebAssembly.Instance(new WebAssembly.Module(new Uint8Array({thrower})), {{ js: {{ tag }} }});",
                    " try {{ i.exports.run(42); return false; }}",
                    " catch (e) {{ return e instanceof WebAssembly.Exception && e.is(tag) && e.getArg(tag, 0) === 42; }} }})()"
                ),
                thrower = thrower,
            ),
        );
        // A module declaring and exporting its own Tag behaves the same
        // through the exported Tag object.
        let own_tag = wat_module_bytes(concat!(
            "(module",
            "  (tag $t (export \"tag\") (param i32))",
            "  (func (export \"run\") (param i32)",
            "    local.get 0",
            "    throw $t))"
        ));
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const i = new WebAssembly.Instance(new Uint8Array({own_tag}));",
                    " if (!(i.exports.tag instanceof WebAssembly.Tag)) return false;",
                    " try {{ i.exports.run(7); return false; }}",
                    " catch (e) {{ return e instanceof WebAssembly.Exception && e.is(i.exports.tag) && e.getArg(i.exports.tag, 0) === 7; }} }})()"
                ),
                own_tag = own_tag,
            ),
        );
        // A module-private tag (declared but neither imported nor exported)
        // still reifies as a WebAssembly.Exception when its throw escapes:
        // JS needs no Tag wrapper to catch it as one.
        let private_tag = wat_module_bytes(concat!(
            "(module",
            "  (tag $t (param i32))",
            "  (func (export \"run\") (param i32)",
            "    local.get 0",
            "    throw $t))"
        ));
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const i = new WebAssembly.Instance(new Uint8Array({private_tag}));",
                    " try {{ i.exports.run(7); return false; }}",
                    " catch (e) {{ return e instanceof WebAssembly.Exception; }} }})()"
                ),
                private_tag = private_tag,
            ),
        );
        // A JS-thrown WebAssembly.Exception that wasm does not catch passes
        // through with its identity preserved.
        let passthrough = wat_module_bytes(concat!(
            "(module",
            "  (import \"js\" \"tag\" (tag $t (param i32)))",
            "  (import \"js\" \"raise\" (func $raise))",
            "  (func (export \"run\")",
            "    call $raise))"
        ));
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const tag = new WebAssembly.Tag({{ parameters: ['i32'] }});",
                    " const original = new WebAssembly.Exception(tag, [99]);",
                    " const i = new WebAssembly.Instance(new WebAssembly.Module(new Uint8Array({passthrough})),",
                    "   {{ js: {{ tag, raise: () => {{ throw original; }} }} }});",
                    " try {{ i.exports.run(); return false; }}",
                    " catch (e) {{ return e === original && e.is(tag) && e.getArg(tag, 0) === 99; }} }})()"
                ),
                passthrough = passthrough,
            ),
        );
        // A matching try_table catches a JS-thrown WebAssembly.Exception and
        // receives its payload as the branch values.
        let catcher = wat_module_bytes(concat!(
            "(module",
            "  (type $t (func (param i32)))",
            "  (import \"js\" \"tag\" (tag $t (param i32)))",
            "  (import \"js\" \"raise\" (func $raise))",
            "  (func (export \"run\") (result i32)",
            "    (block $h (result i32)",
            "      (try_table (result i32) (catch $t $h)",
            "        (call $raise)",
            "        (unreachable))))) "
        ));
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const tag = new WebAssembly.Tag({{ parameters: ['i32'] }});",
                    " const i = new WebAssembly.Instance(new WebAssembly.Module(new Uint8Array({catcher})),",
                    "   {{ js: {{ tag, raise: () => {{ throw new WebAssembly.Exception(tag, [77]); }} }} }});",
                    " return i.exports.run() === 77; }})()"
                ),
                catcher = catcher,
            ),
        );
    }

    #[test]
    fn reexport_wrappers_keep_object_identity() {
        let mut context = Context::new().unwrap();
        // JS-API wrapper caching: a module re-exporting a function/global/
        // memory/table it imported from another instance surfaces the same JS
        // objects the importer passed in (constructor-caching fixture).
        let a = wat_module_bytes(concat!(
            "(module",
            "  (func (export \"fn\"))",
            "  (global (export \"global\") i32 (i32.const 0))",
            "  (memory (export \"memory\") 1)",
            "  (table (export \"table\") 1 funcref))"
        ));
        let b = wat_module_bytes(concat!(
            "(module",
            "  (import \"m\" \"fn\" (func))",
            "  (import \"m\" \"global\" (global i32))",
            "  (import \"m\" \"memory\" (memory 1))",
            "  (import \"m\" \"table\" (table 1 funcref))",
            "  (export \"fn\" (func 0))",
            "  (export \"global\" (global 0))",
            "  (export \"memory\" (memory 0))",
            "  (export \"table\" (table 0)))"
        ));
        for expr in [
            "b.exports.fn === a.exports.fn",
            "b.exports.global === a.exports.global",
            "b.exports.memory === a.exports.memory",
            "b.exports.table === a.exports.table",
        ] {
            eval_true(
                &mut context,
                &format!(
                    concat!(
                        "(function(){{ const a = new WebAssembly.Instance(new Uint8Array({a}));",
                        " const b = new WebAssembly.Instance(new Uint8Array({b}),",
                        "   {{ m: {{ fn: a.exports.fn, global: a.exports.global,",
                        "     memory: a.exports.memory, table: a.exports.table }} }});",
                        " return {expr}; }})()"
                    ),
                    a = a,
                    b = b,
                    expr = expr,
                ),
            );
        }
    }

    #[test]
    fn js_tag_roundtrips_js_values_through_wasm() {
        let mut context = Context::new().unwrap();
        // The JSTag is not a constructible exception tag.
        eval_true(
            &mut context,
            concat!(
                "(function(){ try { new WebAssembly.Exception(WebAssembly.JSTag, [{}]); return false; }",
                " catch (e) { return e instanceof TypeError; } })()"
            ),
        );
        // A wasm `throw` of the JSTag with a JS value escapes as that value.
        let thrower = wat_module_bytes(concat!(
            "(module",
            "  (import \"js\" \"JSTag\" (tag $jst (param externref)))",
            "  (func (export \"throw_js\") (param externref)",
            "    local.get 0",
            "    throw $jst))"
        ));
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const obj = {{}};",
                    " const i = new WebAssembly.Instance(new Uint8Array({thrower}), {{ js: {{ JSTag: WebAssembly.JSTag }} }});",
                    " try {{ i.exports.throw_js(obj); return false; }} catch (e) {{ return e === obj; }} }})()"
                ),
                thrower = thrower,
            ),
        );
        // A JS closure throwing an arbitrary value is catchable by a wasm
        // try_table `catch` for the JSTag, which receives the value as its
        // externref payload.
        let catcher = wat_module_bytes(concat!(
            "(module",
            "  (import \"js\" \"JSTag\" (tag $jst (param externref)))",
            "  (import \"js\" \"throw_ref\" (func $throw_ref (param externref) (result externref)))",
            "  (func (export \"catch_js_value\") (param externref) (result externref)",
            "    (block $h (result externref)",
            "      (try_table (result externref) (catch $jst $h)",
            "        (local.get 0)",
            "        (call $throw_ref)",
            "        (unreachable)))))"
        ));
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const obj = {{}};",
                    " const i = new WebAssembly.Instance(new Uint8Array({catcher}),",
                    "   {{ js: {{ JSTag: WebAssembly.JSTag, throw_ref: (x) => {{ throw x; }} }} }});",
                    " if (i.exports.catch_js_value(obj) !== obj) return false;",
                    " const wasmTag = new WebAssembly.Tag({{ parameters: ['externref'] }});",
                    " const exn = new WebAssembly.Exception(wasmTag, [obj]);",
                    " try {{ i.exports.catch_js_value(exn); return false; }}",
                    " catch (e) {{ return e === exn; }} }})()"
                ),
                catcher = catcher,
            ),
        );
    }

    #[test]
    fn funcref_table_set_clears_to_null_and_reaccepts() {
        let mut context = Context::new().unwrap();
        // The fixture's anyfunc-table set round-trip: constructing a table
        // with an exported wasm function as the fill, clearing a slot with an
        // omitted value (undefined -> null), and setting the function back
        // all keep the function's wrapper identity through `get`.
        let module = wat_module_bytes(concat!("(module", "  (func (export \"fn\"))", ")"));
        eval_true(
            &mut context,
            &format!(
                concat!(
                    "(function(){{ const fn = new WebAssembly.Instance(new Uint8Array({module})).exports.fn;",
                    " const t = new WebAssembly.Table({{ element: 'anyfunc', initial: 1 }}, fn);",
                    " if (t.get(0) !== fn) return false;",
                    " t.set(0);",
                    " if (t.get(0) !== null) return false;",
                    " try {{ t.set(0, undefined); return false; }} catch (e) {{ if (!(e instanceof TypeError)) return false; }}",
                    " t.set(0, fn);",
                    " if (t.get(0) !== fn) return false;",
                    " try {{ t.set(0, {{}}); return false; }} catch (e) {{ return e instanceof TypeError; }} }})()"
                ),
                module = module,
            ),
        );
    }
}
