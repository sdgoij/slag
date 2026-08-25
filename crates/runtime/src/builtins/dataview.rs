//! The DataView built-in (spec 25.4): an endian-aware byte view over an
//! ArrayBuffer/SharedArrayBuffer, defined by [[ViewedArrayBuffer]],
//! [[ByteLength]], and [[ByteOffset]] (the `dataview_data` agent table).
//! The element conversions reuse the crux typed-array codecs; only the byte
//! order changes (native vs. littleEndian).

use crux::convert::{to_boolean, to_index};
use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::heap::{GcAny, Trace};
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::typed_array::{ElementType, decode_element, encode_element};
use crux::value::{Value, ValueKind};

use crate::agent::Agent;
use crate::context::{as_object, get_property_key};
use crate::realm::Realm;

const DATA_VIEW: &str = "%DataView%";
const DATA_VIEW_PROTO: &str = "%DataView.prototype%";
const DV_BUFFER: &str = "%get DataView.prototype.buffer%";
const DV_BYTE_LENGTH: &str = "%get DataView.prototype.byteLength%";
const DV_BYTE_OFFSET: &str = "%get DataView.prototype.byteOffset%";

/// The [[ViewedArrayBuffer]], [[ByteLength]], and [[ByteOffset]] of a
/// DataView instance (spec 25.4.1). [[ByteLength]] is `None` for the auto
/// length of a resizable-buffer view created without a length argument
/// (spec 25.4.2.1 step 8.b): the view's byte length then tracks the buffer.
#[derive(Debug, Clone)]
pub struct DataViewState {
    pub buffer_object: Value,
    pub byte_length: Option<usize>,
    pub byte_offset: usize,
}

impl Trace for DataViewState {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.buffer_object.trace(visit);
    }
}

/// The element types a DataView can address (spec 25.4.3 table): all of the
/// TypedArray kinds except Uint8Clamped.
fn element_type(name: &str) -> Option<ElementType> {
    Some(match name {
        "Int8" => ElementType::Int8,
        "Uint8" => ElementType::Uint8,
        "Int16" => ElementType::Int16,
        "Uint16" => ElementType::Uint16,
        "Int32" => ElementType::Int32,
        "Uint32" => ElementType::Uint32,
        "Float16" => ElementType::Float16,
        "Float32" => ElementType::Float32,
        "Float64" => ElementType::Float64,
        "BigInt64" => ElementType::BigInt64,
        "BigUint64" => ElementType::BigUint64,
        _ => return None,
    })
}

/// RequireInternalSlot([[DataView]]): `this` must be a registered DataView.
fn require_data_view(agent: &Agent, this: &Value) -> Result<Handle<JsObject>, JsError> {
    let object = as_object(this).ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "Receiver is not a DataView object".into(),
        )
    })?;
    if !agent.dataview_data.contains_key(&object.id()) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Receiver is not a DataView object".into(),
        ));
    }
    Ok(object)
}

/// The state re-fetched from the agent (never keep a cloned `RefCell` handle
/// across mutations).
fn state(agent: &Agent, id: u64) -> Option<std::cell::Ref<'_, DataViewState>> {
    agent.dataview_data.get(&id).map(|cell| cell.borrow())
}

fn placeholder(name: &'static str) -> NativeFn {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

fn str(text: &str) -> Value {
    Value::String(Handle::new(JsString::from_utf8(text)))
}

/// GetPrototypeFromConstructor (spec 10.1.14).
fn get_prototype_from_constructor(
    agent: &mut Agent,
    constructor: &Value,
    default_name: &str,
) -> Result<Handle<JsObject>, JsError> {
    let proto = get_property_key(
        agent,
        constructor,
        &PropertyKey::from_utf8("prototype"),
        constructor.clone(),
    )?;
    match as_object(&proto) {
        Some(object) => Ok(object),
        None => {
            let default = crate::context::get_function_realm(agent, constructor)?
                .intrinsics
                .get(default_name)
                .and_then(|value| as_object(&value))
                .ok_or_else(|| {
                    JsError::new(
                        ErrorKind::TypeError,
                        format!("{default_name} is not defined"),
                    )
                })?;
            Ok(default)
        }
    }
}

/// DataView(buffer [, byteOffset [, length]]) (spec 25.4.2.1).
fn data_view_construct(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    if matches!(new_target.kind(), ValueKind::Undefined) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "DataView must be called with 'new'".into(),
        ));
    }
    let buffer = args.first().cloned().unwrap_or(Value::Undefined);
    let buffer_object = as_object(&buffer).ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "First argument is not an object".into(),
        )
    })?;
    if !agent.buffer_data.contains_key(&buffer_object.id()) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "First argument is not an ArrayBuffer".into(),
        ));
    }
    // spec 25.4.2.1 steps 3-4: ToIndex(byteOffset) runs before the first
    // detached check (the argument's valueOf may itself detach or throw).
    let byte_offset = to_index(&args.get(1).cloned().unwrap_or(Value::Undefined))? as usize;
    if crate::builtins::array_buffer::is_detached(agent, buffer_object.id()) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "ArrayBuffer is detached".into(),
        ));
    }
    let buffer_byte_length = agent
        .buffer_data
        .get(&buffer_object.id())
        .map(|cell| cell.borrow().byte_length)
        .unwrap_or(0);
    let byte_length = match args.get(2) {
        None => {
            // step 8: an omitted length is auto; the offset must still fit.
            if byte_offset > buffer_byte_length {
                return Err(JsError::new(
                    ErrorKind::RangeError,
                    "byteOffset exceeds the buffer length".into(),
                ));
            }
            None
        }
        Some(value) if value.is_undefined() => {
            // step 8: an omitted length is auto; the offset must still fit.
            if byte_offset > buffer_byte_length {
                return Err(JsError::new(
                    ErrorKind::RangeError,
                    "byteOffset exceeds the buffer length".into(),
                ));
            }
            None
        }
        Some(value) => {
            let length = to_index(value)? as usize;
            if byte_offset
                .checked_add(length)
                .is_none_or(|end| end > buffer_byte_length)
            {
                return Err(JsError::new(
                    ErrorKind::RangeError,
                    "DataView range exceeds the buffer".into(),
                ));
            }
            Some(length)
        }
    };
    let prototype = get_prototype_from_constructor(agent, new_target, DATA_VIEW_PROTO)?;
    // spec 25.4.2.1 steps 11-12: OrdinaryCreateFromConstructor ran user code
    // (the prototype getter) that may have detached or resized the buffer;
    // a detached buffer is a TypeError, an out-of-bounds view a RangeError.
    if crate::builtins::array_buffer::is_detached(agent, buffer_object.id()) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "ArrayBuffer is detached".into(),
        ));
    }
    if view_out_of_bounds(agent, buffer_object.id(), byte_offset, byte_length) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "DataView is out of bounds".into(),
        ));
    }
    let object = JsObject::ordinary_object_create(Some(prototype));
    agent.dataview_data.insert(
        object.id(),
        std::cell::RefCell::new(DataViewState {
            buffer_object: buffer,
            byte_length,
            byte_offset,
        }),
    );
    Ok(Value::Object(object))
}

/// DataView.prototype.buffer (spec 25.4.4.1).
fn get_buffer(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let object = require_data_view(agent, this)?;
    Ok(state(agent, object.id())
        .map(|state| state.buffer_object.clone())
        .unwrap_or(Value::Undefined))
}

/// DataView.prototype.byteLength (spec 25.4.4.2).
fn get_byte_length(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let object = require_data_view(agent, this)?;
    let (buffer_object, byte_length, byte_offset) = {
        let state = state(agent, object.id()).expect("registered data view");
        (
            state.buffer_object.clone(),
            state.byte_length,
            state.byte_offset,
        )
    };
    let buffer_id = as_object(&buffer_object)
        .map(|object| object.id())
        .unwrap_or(u64::MAX);
    // spec 25.4.4.2 steps 2-3: a detached or out-of-bounds view throws.
    if crate::builtins::array_buffer::is_detached(agent, buffer_id)
        || view_out_of_bounds(agent, buffer_id, byte_offset, byte_length)
    {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "DataView view is out of bounds".into(),
        ));
    }
    let length = match byte_length {
        Some(length) => length,
        // auto: the view's byte length tracks the buffer (spec 25.4.4.2
        // step 4); the checks above guarantee no underflow.
        None => {
            let buffer_length = agent
                .buffer_data
                .get(&buffer_id)
                .map(|cell| cell.borrow().byte_length)
                .unwrap_or(0);
            buffer_length - byte_offset
        }
    };
    Ok(Value::Number(length as f64))
}

/// DataView.prototype.byteOffset (spec 25.4.4.3).
fn get_byte_offset(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let object = require_data_view(agent, this)?;
    let (buffer_object, byte_length, byte_offset) = {
        let state = state(agent, object.id()).expect("registered data view");
        (
            state.buffer_object.clone(),
            state.byte_length,
            state.byte_offset,
        )
    };
    let buffer_id = as_object(&buffer_object)
        .map(|object| object.id())
        .unwrap_or(u64::MAX);
    if crate::builtins::array_buffer::is_detached(agent, buffer_id)
        || view_out_of_bounds(agent, buffer_id, byte_offset, byte_length)
    {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "DataView view is out of bounds".into(),
        ));
    }
    Ok(Value::Number(byte_offset as f64))
}

/// IsViewOutOfBounds (spec 25.4.1.5): the view's byte range no longer fits
/// the (possibly resized or detached) buffer. An auto-length view tracks the
/// buffer, so only the offset can push it out of bounds.
fn view_out_of_bounds(
    agent: &Agent,
    buffer_id: u64,
    byte_offset: usize,
    byte_length: Option<usize>,
) -> bool {
    let Some(buffer) = agent.buffer_data.get(&buffer_id) else {
        return false;
    };
    let buffer = buffer.borrow();
    match byte_length {
        None => byte_offset > buffer.byte_length,
        Some(length) => byte_offset + length > buffer.byte_length,
    }
}

/// RequireInternalSlot([[DataView]]) (spec 25.4.2.2/25.4.2.3 step 1): the
/// view's buffer id, byte offset, and byte length.
fn view_state(agent: &Agent, this: &Value) -> Result<(u64, usize, Option<usize>), JsError> {
    let object = require_data_view(agent, this)?;
    let (buffer_object, byte_length, byte_offset) = {
        let state = state(agent, object.id()).expect("registered data view");
        (
            state.buffer_object.clone(),
            state.byte_length,
            state.byte_offset,
        )
    };
    let buffer_id = as_object(&buffer_object)
        .map(|object| object.id())
        .unwrap_or(u64::MAX);
    Ok((buffer_id, byte_offset, byte_length))
}

/// The remaining checks of GetViewValue/SetViewValue (spec 25.4.2.2 steps
/// 14-16 / 25.4.2.3 steps 14-16): the buffer must not be detached, the view
/// must not be out of bounds, and the element must fit in the view's byte
/// length. Returns the absolute byte offset into the buffer.
fn check_view(
    agent: &Agent,
    buffer_id: u64,
    byte_offset: usize,
    byte_length: Option<usize>,
    index: usize,
    size: usize,
) -> Result<usize, JsError> {
    if crate::builtins::array_buffer::is_detached(agent, buffer_id) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "DataView buffer is detached".into(),
        ));
    }
    if view_out_of_bounds(agent, buffer_id, byte_offset, byte_length) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "DataView view is out of bounds".into(),
        ));
    }
    let view_size = match byte_length {
        Some(length) => length,
        // auto: the view's byte length tracks the buffer (spec 25.4.2.2
        // step 9); the OOB check above guarantees no underflow.
        None => {
            let buffer_length = agent
                .buffer_data
                .get(&buffer_id)
                .map(|cell| cell.borrow().byte_length)
                .unwrap_or(0);
            buffer_length - byte_offset
        }
    };
    if index.checked_add(size).is_none_or(|end| end > view_size) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "DataView element is out of bounds".into(),
        ));
    }
    Ok(byte_offset + index)
}

/// The littleEndian option (ToBoolean of the second argument, default false).
fn little_endian(args: &[Value]) -> bool {
    to_boolean(&args.get(1).cloned().unwrap_or(Value::Undefined))
}

/// The read path of the get* methods (spec GetViewValue): fetch the
/// `size` bytes at the absolute offset, reorder for endianness, and decode.
fn read_element(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    element_type: ElementType,
) -> Result<Value, JsError> {
    let (buffer_id, byte_offset, byte_length) = view_state(agent, this)?;
    let index = to_index(&args.first().cloned().unwrap_or(Value::Undefined))? as usize;
    let offset = check_view(
        agent,
        buffer_id,
        byte_offset,
        byte_length,
        index,
        element_type.size(),
    )?;
    let mut bytes = {
        let buffer = agent.buffer_data.get(&buffer_id).ok_or_else(|| {
            JsError::new(ErrorKind::TypeError, "DataView buffer is detached".into())
        })?;
        buffer.borrow().shared.read(offset, element_type.size())?
    };
    if little_endian(args) != agent.little_endian {
        bytes.reverse();
    }
    decode_element(element_type, &bytes, 0)
}

/// The write path of the set* methods (spec SetViewValue): encode the
/// value natively, reorder for endianness, and store. The immutable check
/// precedes ToIndex(requestIndex) (spec 25.4.2.3 step 3) and the value
/// conversion runs before the detached/out-of-bounds/bounds checks (steps
/// 5-6).
fn write_element(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    element_type: ElementType,
) -> Result<Value, JsError> {
    let (buffer_id, byte_offset, byte_length) = view_state(agent, this)?;
    if agent
        .buffer_data
        .get(&buffer_id)
        .is_some_and(|cell| cell.borrow().immutable)
    {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "DataView buffer is immutable".into(),
        ));
    }
    let index = to_index(&args.first().cloned().unwrap_or(Value::Undefined))? as usize;
    let value = args.get(1).cloned().unwrap_or(Value::Undefined);
    let converted = if matches!(element_type, ElementType::BigInt64 | ElementType::BigUint64) {
        Value::BigInt(Handle::new(crate::context::to_big_int(agent, &value)?))
    } else {
        Value::Number(crate::context::to_number(agent, &value)?)
    };
    let offset = check_view(
        agent,
        buffer_id,
        byte_offset,
        byte_length,
        index,
        element_type.size(),
    )?;
    let mut bytes = encode_element(element_type, &converted)?;
    // The isLittleEndian flag is the third parameter, read past the value.
    if little_endian(args.get(1..).unwrap_or(&[])) != agent.little_endian {
        bytes.reverse();
    }
    let buffer = agent
        .buffer_data
        .get(&buffer_id)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "DataView buffer is detached".into()))?;
    buffer.borrow().shared.write(offset, &bytes)?;
    Ok(Value::Undefined)
}

pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let dv_proto = JsObject::ordinary_object_create(object_proto);
    let dv_proto_value = Value::Object(dv_proto);
    let dv_ctor = Function::create_builtin(
        Some(JsString::from_utf8("DataView")),
        1,
        placeholder("DataView"),
        Some(Box::new(placeholder("DataView"))),
        None,
    )?;
    let dv_ctor_value = Value::Function(dv_ctor);
    realm.intrinsics.define(DATA_VIEW, dv_ctor_value.clone());
    realm
        .intrinsics
        .define(DATA_VIEW_PROTO, dv_proto_value.clone());
    dv_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(dv_proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    dv_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(dv_ctor_value.clone()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    let accessors: [(&str, &str); 3] = [
        ("buffer", DV_BUFFER),
        ("byteLength", DV_BYTE_LENGTH),
        ("byteOffset", DV_BYTE_OFFSET),
    ];
    for (name, intrinsic) in accessors {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(&format!("get {name}"))),
            0,
            placeholder(name),
            None,
            None,
        )?;
        realm.intrinsics.define(intrinsic, Value::Function(func));
        dv_proto.define_property(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: None,
                writable: None,
                get: Some(Value::Function(func)),
                set: Some(Value::Undefined),
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
    }

    let methods: [(&str, &str, u64); 22] = [
        ("getInt8", "getInt8", 1),
        ("getUint8", "getUint8", 1),
        ("getInt16", "getInt16", 1),
        ("getUint16", "getUint16", 1),
        ("getInt32", "getInt32", 1),
        ("getUint32", "getUint32", 1),
        ("getFloat16", "getFloat16", 1),
        ("getFloat32", "getFloat32", 1),
        ("getFloat64", "getFloat64", 1),
        ("getBigInt64", "getBigInt64", 1),
        ("getBigUint64", "getBigUint64", 1),
        ("setInt8", "setInt8", 2),
        ("setUint8", "setUint8", 2),
        ("setInt16", "setInt16", 2),
        ("setUint16", "setUint16", 2),
        ("setInt32", "setInt32", 2),
        ("setUint32", "setUint32", 2),
        ("setFloat16", "setFloat16", 2),
        ("setFloat32", "setFloat32", 2),
        ("setFloat64", "setFloat64", 2),
        ("setBigInt64", "setBigInt64", 2),
        ("setBigUint64", "setBigUint64", 2),
    ];
    for (name, intrinsic, length) in methods {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            placeholder(name),
            None,
            None,
        )?;
        realm.intrinsics.define(intrinsic, Value::Function(func));
        dv_proto.define_property(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: Some(Value::Function(func)),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
    }
    dv_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(str("DataView")),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("DataView"),
        &PropertyDescriptor {
            value: Some(dv_ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// The DataView members that need the agent, dispatched by intrinsic identity
/// from `runtime::function::call`/`construct`.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(DATA_VIEW).as_ref() == Some(callee) {
        return Some(Err(JsError::new(
            ErrorKind::TypeError,
            "DataView must be called with 'new'".into(),
        )));
    }
    let kind = [
        ("getInt8", "getInt8"),
        ("getUint8", "getUint8"),
        ("getInt16", "getInt16"),
        ("getUint16", "getUint16"),
        ("getInt32", "getInt32"),
        ("getUint32", "getUint32"),
        ("getFloat16", "getFloat16"),
        ("getFloat32", "getFloat32"),
        ("getFloat64", "getFloat64"),
        ("getBigInt64", "getBigInt64"),
        ("getBigUint64", "getBigUint64"),
    ]
    .iter()
    .find(|(name, _)| intrinsics.get(name).as_ref() == Some(callee))
    .map(|(name, _)| name.to_string());
    if let Some(name) = kind {
        let element_type = element_type(&name[3..]).expect("known getter");
        return Some(read_element(agent, this, args, element_type));
    }
    if intrinsics.get(DV_BUFFER).as_ref() == Some(callee) {
        return Some(get_buffer(agent, this, args));
    }
    if intrinsics.get(DV_BYTE_LENGTH).as_ref() == Some(callee) {
        return Some(get_byte_length(agent, this, args));
    }
    if intrinsics.get(DV_BYTE_OFFSET).as_ref() == Some(callee) {
        return Some(get_byte_offset(agent, this, args));
    }
    let kind = [
        ("setInt8", "setInt8"),
        ("setUint8", "setUint8"),
        ("setInt16", "setInt16"),
        ("setUint16", "setUint16"),
        ("setInt32", "setInt32"),
        ("setUint32", "setUint32"),
        ("setFloat16", "setFloat16"),
        ("setFloat32", "setFloat32"),
        ("setFloat64", "setFloat64"),
        ("setBigInt64", "setBigInt64"),
        ("setBigUint64", "setBigUint64"),
    ]
    .iter()
    .find(|(name, _)| intrinsics.get(name).as_ref() == Some(callee))
    .map(|(name, _)| name.to_string());
    if let Some(name) = kind {
        let element_type = element_type(&name[3..]).expect("known setter");
        return Some(write_element(agent, this, args, element_type));
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
    if realm.intrinsics.get(DATA_VIEW).as_ref() == Some(callee) {
        return Some(data_view_construct(agent, args, new_target));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;

    fn run(source: &str) -> Result<Value, JsError> {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm()?;
        agent.run_script(source)
    }

    fn number(source: &str) -> f64 {
        match run(source).unwrap().kind() {
            ValueKind::Number(n) => n,
            other => panic!("expected a number, got {other:?}"),
        }
    }

    fn bool(source: &str) -> bool {
        match run(source).unwrap().kind() {
            ValueKind::Boolean(b) => b,
            other => panic!("expected a boolean, got {other:?}"),
        }
    }

    #[test]
    fn construction_and_lengths() {
        assert_eq!(number("new DataView(new ArrayBuffer(8)).byteLength"), 8.0);
        assert_eq!(
            number("new DataView(new ArrayBuffer(8), 4).byteLength"),
            4.0
        );
        assert_eq!(
            number("new DataView(new ArrayBuffer(8), 4, 2).byteLength"),
            2.0
        );
        assert_eq!(
            number("new DataView(new ArrayBuffer(8), 4).byteOffset"),
            4.0
        );
        // Out-of-range offsets and lengths are RangeErrors.
        assert!(matches!(
            run("new DataView(new ArrayBuffer(4), 5)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        assert!(matches!(
            run("new DataView(new ArrayBuffer(4), 2, 3)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        assert!(matches!(
            run("new DataView(new ArrayBuffer(4), -1)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        // The first argument must be a buffer object.
        assert!(matches!(
            run("new DataView(123)"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
        assert!(matches!(
            run("DataView(new ArrayBuffer(8))"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
    }

    #[test]
    fn accessors_reflect_constructor_args() {
        assert!(bool(
            "(function(){ var b = new ArrayBuffer(8); var dv = new DataView(b, 4, 2); return dv.buffer === b; })()"
        ));
        assert_eq!(
            number("new DataView(new ArrayBuffer(8), 4, 2).byteOffset"),
            4.0
        );
        assert_eq!(
            number("new DataView(new ArrayBuffer(8), 4, 2).byteLength"),
            2.0
        );
    }

    #[test]
    fn integer_round_trips() {
        assert!(bool(
            "(function(){ var dv = new DataView(new ArrayBuffer(16)); dv.setInt8(0, -5); dv.setUint8(1, 200); dv.setInt16(2, -300); dv.setUint16(4, 60000); dv.setInt32(8, 0x12345678); dv.setUint32(12, 0xDEADBEEF); return dv.getInt8(0) === -5 && dv.getUint8(1) === 200 && dv.getInt16(2) === -300 && dv.getUint16(4) === 60000 && dv.getInt32(8) === 0x12345678 && dv.getUint32(12) === 0xDEADBEEF; })()"
        ));
    }

    #[test]
    fn float_round_trips() {
        assert_eq!(
            number(
                "(function(){ var dv = new DataView(new ArrayBuffer(8)); dv.setFloat64(0, Math.PI); return dv.getFloat64(0); })()"
            ),
            std::f64::consts::PI
        );
        assert_eq!(
            number(
                "(function(){ var dv = new DataView(new ArrayBuffer(8)); dv.setFloat32(0, 1.5); return dv.getFloat32(0); })()"
            ),
            1.5
        );
    }

    #[test]
    fn bigint_round_trips() {
        assert!(bool(
            "(function(){ var dv = new DataView(new ArrayBuffer(16)); dv.setBigInt64(0, 0x1122334455667788n); dv.setBigUint64(8, 0xFFFFFFFFFFFFFFFFn); return dv.getBigInt64(0) === 0x1122334455667788n && dv.getBigUint64(8) === 0xFFFFFFFFFFFFFFFFn; })()"
        ));
    }

    #[test]
    fn element_bounds_checks() {
        assert!(matches!(
            run("(function(){ var dv = new DataView(new ArrayBuffer(8)); dv.getInt8(8); })()"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        assert!(matches!(
            run("(function(){ var dv = new DataView(new ArrayBuffer(8)); dv.setInt8(-1, 1); })()"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        // A Uint16 needs 2 bytes: offset 7 leaves only 1 in an 8-byte view.
        assert!(matches!(
            run("(function(){ var dv = new DataView(new ArrayBuffer(8)); dv.setUint16(7, 1); })()"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
    }

    #[test]
    fn little_endian_flag() {
        // Big-endian default write, little-endian read: bytes swap.
        assert_eq!(
            number(
                "(function(){ var dv = new DataView(new ArrayBuffer(2)); dv.setUint16(0, 0x0102); return dv.getUint16(0, true); })()"
            ),
            513.0
        );
        // Little-endian write, big-endian (default) read: bytes swap.
        assert_eq!(
            number(
                "(function(){ var dv = new DataView(new ArrayBuffer(2)); dv.setUint16(0, 0x0102, true); return dv.getUint16(0); })()"
            ),
            513.0
        );
        assert_eq!(
            number(
                "(function(){ var dv = new DataView(new ArrayBuffer(2)); dv.setUint16(0, 0x0102, true); return dv.getUint16(0, true); })()"
            ),
            258.0
        );
    }

    #[test]
    fn unaligned_access_is_allowed() {
        // DataView has no alignment requirement: an Int32 at byte offset 1
        // of the buffer works.
        assert_eq!(
            number(
                "(function(){ var dv = new DataView(new ArrayBuffer(8), 1); dv.setInt32(0, 0x12345678); return dv.getInt32(0); })()"
            ),
            305419896.0
        );
    }

    #[test]
    fn detached_buffer_access_throws() {
        assert!(matches!(
            run("(function(){ var b = new ArrayBuffer(8); var dv = new DataView(b); b.transfer(); dv.getInt8(0); })()"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
        assert!(matches!(
            run("(function(){ var b = new ArrayBuffer(8); var dv = new DataView(b, 4); b.transfer(); dv.setInt32(0, 1); })()"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
        assert!(matches!(
            run("(function(){ var b = new ArrayBuffer(8); b.transfer(); new DataView(b); })()"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
    }

    #[test]
    fn setter_rejects_immutable_buffer_before_argument_coercion() {
        // SetViewValue step 3: the immutable TypeError precedes ToIndex and
        // the value conversion, so no user code runs (the `calls` array must
        // stay empty). Reads on an immutable buffer still work.
        assert!(bool(concat!(
            "(function(){\n",
            "  var iab = (new ArrayBuffer(8)).transferToImmutable();\n",
            "  var view = new DataView(iab);\n",
            "  var calls = [];\n",
            "  var byteOffset = { valueOf: function(){ calls.push('offset'); return 0; } };\n",
            "  var value = { valueOf: function(){ calls.push('value'); return 1; } };\n",
            "  var threw = false;\n",
            "  try { view.setInt32(byteOffset, value); } catch (e) { threw = e instanceof TypeError; }\n",
            "  return threw && calls.length === 0 && view.getInt32(0) === 0;\n",
            "})()",
        )));
    }

    #[test]
    fn setter_value_conversion_precedes_detached_and_bounds_checks() {
        // SetViewValue steps 5-6: ToNumber(value) runs before the detached
        // check and before the RangeError bounds check, so a poisoned
        // valueOf wins over both.
        assert!(bool(concat!(
            "(function(){\n",
            "  var b = new ArrayBuffer(8);\n",
            "  var view = new DataView(b);\n",
            "  var poisoned = { valueOf: function(){ throw new TypeError('poison'); } };\n",
            "  var caught = null;\n",
            "  try { view.setInt32(100, poisoned); } catch (e) { caught = e.message; }\n",
            "  b.transfer();\n",
            "  try { view.setInt32(0, poisoned); } catch (e) { caught = caught + '|' + e.message; }\n",
            "  return caught === 'poison|poison';\n",
            "})()",
        )));
    }

    #[test]
    fn resizable_view_out_of_bounds() {
        // A fixed-length view whose range no longer fits the shrunken buffer
        // is out of bounds: get/set and the length/offset accessors throw a
        // TypeError, and the constructor RangeErrors when the prototype
        // getter shrank the buffer under the view.
        assert!(bool(concat!(
            "(function(){\n",
            "  var ab = new ArrayBuffer(24, { maxByteLength: 32 });\n",
            "  var view = new DataView(ab, 0, 16);\n",
            "  ab.resize(8);\n",
            "  var threw = false;\n",
            "  try { view.setInt8(0, 1); } catch (e) { threw = e instanceof TypeError; }\n",
            "  var threwLength = false;\n",
            "  try { view.byteLength; } catch (e) { threwLength = e instanceof TypeError; }\n",
            "  return threw && threwLength;\n",
            "})()",
        )));
        assert!(matches!(
            run(concat!(
                "(function(){\n",
                "  var b = new ArrayBuffer(3, { maxByteLength: 3 });\n",
                "  var t = function(){}.bind(null);\n",
                "  Object.defineProperty(t, 'prototype', { get: function(){ b.resize(1); } });\n",
                "  Reflect.construct(DataView, [b, 2], t);\n",
                "})()",
            )),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
    }

    #[test]
    fn auto_byte_length_tracks_resizable_buffer() {
        // A view created without a length argument has an auto [[ByteLength]]
        // that follows the buffer (spec 25.4.2.1 step 8.b).
        assert!(bool(concat!(
            "(function(){\n",
            "  var ab = new ArrayBuffer(4, { maxByteLength: 5 });\n",
            "  var view = new DataView(ab, 1);\n",
            "  ab.resize(5);\n",
            "  return view.byteLength === 4 && view.byteOffset === 1;\n",
            "})()",
        )));
    }
}
