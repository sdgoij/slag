//! The DataView built-in (spec 25.4): an endian-aware byte view over an
//! ArrayBuffer/SharedArrayBuffer, defined by [[ViewedArrayBuffer]],
//! [[ByteLength]], and [[ByteOffset]] (the `dataview_data` agent table).
//! The element conversions reuse the crux typed-array codecs; only the byte
//! order changes (native vs. littleEndian).

use crux::convert::{to_boolean, to_index};
use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::typed_array::{ElementType, decode_element, encode_element};
use crux::value::Value;

use crate::agent::Agent;
use crate::context::{as_object, get_property_key};
use crate::realm::Realm;

const DATA_VIEW: &str = "%DataView%";
const DATA_VIEW_PROTO: &str = "%DataView.prototype%";
const DV_BUFFER: &str = "%get DataView.prototype.buffer%";
const DV_BYTE_LENGTH: &str = "%get DataView.prototype.byteLength%";
const DV_BYTE_OFFSET: &str = "%get DataView.prototype.byteOffset%";

/// The [[ViewedArrayBuffer]], [[ByteLength]], and [[ByteOffset]] of a
/// DataView instance (spec 25.4.1).
#[derive(Debug, Clone)]
pub struct DataViewState {
    pub buffer_object: Value,
    pub byte_length: usize,
    pub byte_offset: usize,
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
            let default = agent
                .current_realm()?
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
    if matches!(new_target, Value::Undefined) {
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
    if crate::builtins::array_buffer::is_detached(agent, buffer_object.id()) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "ArrayBuffer is detached".into(),
        ));
    }
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
        None | Some(Value::Undefined) => {
            if byte_offset > buffer_byte_length {
                return Err(JsError::new(
                    ErrorKind::RangeError,
                    "byteOffset exceeds the buffer length".into(),
                ));
            }
            buffer_byte_length - byte_offset
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
            length
        }
    };
    let prototype = get_prototype_from_constructor(agent, new_target, DATA_VIEW_PROTO)?;
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
    Ok(Value::Number(
        state(agent, object.id())
            .map(|state| state.byte_length as f64)
            .unwrap_or(0.0),
    ))
}

/// DataView.prototype.byteOffset (spec 25.4.4.3).
fn get_byte_offset(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let object = require_data_view(agent, this)?;
    Ok(Value::Number(
        state(agent, object.id())
            .map(|state| state.byte_offset as f64)
            .unwrap_or(0.0),
    ))
}

/// ValidateDataView + the offset/size bounds check (spec 25.4.4.6 step 1-8):
/// the view's buffer must not be detached and the element must fit in the
/// view's [[ByteLength]]. Returns the view's buffer id and absolute offset.
fn view_offset(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    size: usize,
) -> Result<(u64, usize), JsError> {
    let object = require_data_view(agent, this)?;
    let (buffer_object, byte_offset, byte_length) = {
        let state = state(agent, object.id()).expect("registered data view");
        (
            state.buffer_object.clone(),
            state.byte_offset,
            state.byte_length,
        )
    };
    let buffer_id = as_object(&buffer_object)
        .map(|object| object.id())
        .unwrap_or(u64::MAX);
    if crate::builtins::array_buffer::is_detached(agent, buffer_id) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "DataView buffer is detached".into(),
        ));
    }
    let offset = to_index(&args.first().cloned().unwrap_or(Value::Undefined))? as usize;
    if offset.checked_add(size).is_none_or(|end| end > byte_length) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "DataView element is out of bounds".into(),
        ));
    }
    Ok((buffer_id, byte_offset + offset))
}

/// The littleEndian option (ToBoolean of the second argument, default false).
fn little_endian(args: &[Value]) -> bool {
    to_boolean(&args.get(1).cloned().unwrap_or(Value::Undefined))
}

/// The read path of the get* methods (spec GetValueFromBuffer): fetch the
/// `size` bytes at the absolute offset, reorder for endianness, and decode.
fn read_element(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    element_type: ElementType,
) -> Result<Value, JsError> {
    let (buffer_id, offset) = view_offset(agent, this, args, element_type.size())?;
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

/// The write path of the set* methods (spec SetValueInBuffer): encode the
/// value natively, reorder for endianness, and store.
fn write_element(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    element_type: ElementType,
) -> Result<Value, JsError> {
    let (buffer_id, offset) = view_offset(agent, this, args, element_type.size())?;
    let value = args.get(1).cloned().unwrap_or(Value::Undefined);
    let mut bytes = encode_element(element_type, &value)?;
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
    let dv_proto_value = Value::Object(dv_proto.clone());
    let dv_ctor = Function::create_builtin(
        Some(JsString::from_utf8("DataView")),
        1,
        placeholder("DataView"),
        Some(Box::new(placeholder("DataView"))),
        None,
    )?;
    let dv_ctor_value = Value::Function(dv_ctor.clone());
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
        realm
            .intrinsics
            .define(intrinsic, Value::Function(func.clone()));
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
        realm
            .intrinsics
            .define(intrinsic, Value::Function(func.clone()));
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
        match run(source).unwrap() {
            Value::Number(n) => n,
            other => panic!("expected a number, got {other:?}"),
        }
    }

    fn bool(source: &str) -> bool {
        match run(source).unwrap() {
            Value::Boolean(b) => b,
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
}
