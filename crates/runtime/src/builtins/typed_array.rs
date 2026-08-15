//! The TypedArray built-ins (spec ch. 25.2): the `%TypedArray%` intrinsic,
//! the twelve per-kind constructors, the integer-indexed exotic's real
//! element storage (the crux buffer-backed `IntegerIndexed` kind), the
//! shared prototype surface, and the ES2026 `Uint8Array` hex/base64 methods.

use std::cmp::Ordering;

use crux::convert::{to_boolean, to_integer_or_infinity};
use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::{JsObject, ObjectKind, TypedArraySlots, typed_array_effective_length};
use crux::ops::{is_strictly_equal, same_value_zero};
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::typed_array::{ContentType, ElementType, SharedBuffer};
use crux::value::{Value, is_callable, is_constructor};

use crate::agent::Agent;
use crate::context::{as_object, get_property};
use crate::expr::{IteratorRecord, get_method};
use crate::realm::Realm;

const TYPED_ARRAY: &str = "%TypedArray%";
const TYPED_ARRAY_PROTO: &str = "%TypedArray.prototype%";
const FROM: &str = "%TypedArray.from%";
const OF: &str = "%TypedArray.of%";
const SPECIES: &str = "%TypedArray.prototype[Symbol.species]%";
const GET_TO_STRING_TAG: &str = "%TypedArray.prototype[Symbol.toStringTag]%";
const AT: &str = "%TypedArray.prototype.at%";
const COPY_WITHIN: &str = "%TypedArray.prototype.copyWithin%";
const ENTRIES: &str = "%TypedArray.prototype.entries%";
const EVERY: &str = "%TypedArray.prototype.every%";
const FILL: &str = "%TypedArray.prototype.fill%";
const FILTER: &str = "%TypedArray.prototype.filter%";
const FIND: &str = "%TypedArray.prototype.find%";
const FIND_INDEX: &str = "%TypedArray.prototype.findIndex%";
const FIND_LAST: &str = "%TypedArray.prototype.findLast%";
const FIND_LAST_INDEX: &str = "%TypedArray.prototype.findLastIndex%";
const FOR_EACH: &str = "%TypedArray.prototype.forEach%";
const INCLUDES: &str = "%TypedArray.prototype.includes%";
const INDEX_OF: &str = "%TypedArray.prototype.indexOf%";
const JOIN: &str = "%TypedArray.prototype.join%";
const KEYS: &str = "%TypedArray.prototype.keys%";
const LAST_INDEX_OF: &str = "%TypedArray.prototype.lastIndexOf%";
const MAP: &str = "%TypedArray.prototype.map%";
const REDUCE: &str = "%TypedArray.prototype.reduce%";
const REDUCE_RIGHT: &str = "%TypedArray.prototype.reduceRight%";
const REVERSE: &str = "%TypedArray.prototype.reverse%";
const SET: &str = "%TypedArray.prototype.set%";
const SLICE: &str = "%TypedArray.prototype.slice%";
const SOME: &str = "%TypedArray.prototype.some%";
const SORT: &str = "%TypedArray.prototype.sort%";
const SUBARRAY: &str = "%TypedArray.prototype.subarray%";
const TO_LOCALE_STRING: &str = "%TypedArray.prototype.toLocaleString%";
const TO_REVERSED: &str = "%TypedArray.prototype.toReversed%";
const TO_SORTED: &str = "%TypedArray.prototype.toSorted%";
const VALUES: &str = "%TypedArray.prototype.values%";
const WITH: &str = "%TypedArray.prototype.with%";
const ITERATOR: &str = "%TypedArray.prototype[Symbol.iterator]%";
const GET_LENGTH: &str = "%TypedArray.prototype.length%";
const GET_BUFFER: &str = "%TypedArray.prototype.buffer%";
const GET_BYTE_LENGTH: &str = "%TypedArray.prototype.byteLength%";
const GET_BYTE_OFFSET: &str = "%TypedArray.prototype.byteOffset%";
const FROM_HEX: &str = "%Uint8Array.fromHex%";
const FROM_BASE64: &str = "%Uint8Array.fromBase64%";
const TO_HEX: &str = "%Uint8Array.prototype.toHex%";
const TO_BASE64: &str = "%Uint8Array.prototype.toBase64%";
const SET_FROM_HEX: &str = "%Uint8Array.prototype.setFromHex%";
const SET_FROM_BASE64: &str = "%Uint8Array.prototype.setFromBase64%";

/// The twelve concrete TypedArray kinds (spec 25.2.1 table).
#[derive(Clone, Copy)]
struct KindSpec {
    element_type: ElementType,
    ctor: &'static str,
    proto: &'static str,
    tag: &'static str,
}

const KINDS: [KindSpec; 12] = [
    KindSpec {
        element_type: ElementType::Int8,
        ctor: "%Int8Array%",
        proto: "%Int8Array.prototype%",
        tag: "Int8Array",
    },
    KindSpec {
        element_type: ElementType::Uint8,
        ctor: "%Uint8Array%",
        proto: "%Uint8Array.prototype%",
        tag: "Uint8Array",
    },
    KindSpec {
        element_type: ElementType::Uint8Clamped,
        ctor: "%Uint8ClampedArray%",
        proto: "%Uint8ClampedArray.prototype%",
        tag: "Uint8ClampedArray",
    },
    KindSpec {
        element_type: ElementType::Int16,
        ctor: "%Int16Array%",
        proto: "%Int16Array.prototype%",
        tag: "Int16Array",
    },
    KindSpec {
        element_type: ElementType::Uint16,
        ctor: "%Uint16Array%",
        proto: "%Uint16Array.prototype%",
        tag: "Uint16Array",
    },
    KindSpec {
        element_type: ElementType::Int32,
        ctor: "%Int32Array%",
        proto: "%Int32Array.prototype%",
        tag: "Int32Array",
    },
    KindSpec {
        element_type: ElementType::Uint32,
        ctor: "%Uint32Array%",
        proto: "%Uint32Array.prototype%",
        tag: "Uint32Array",
    },
    KindSpec {
        element_type: ElementType::Float16,
        ctor: "%Float16Array%",
        proto: "%Float16Array.prototype%",
        tag: "Float16Array",
    },
    KindSpec {
        element_type: ElementType::Float32,
        ctor: "%Float32Array%",
        proto: "%Float32Array.prototype%",
        tag: "Float32Array",
    },
    KindSpec {
        element_type: ElementType::Float64,
        ctor: "%Float64Array%",
        proto: "%Float64Array.prototype%",
        tag: "Float64Array",
    },
    KindSpec {
        element_type: ElementType::BigInt64,
        ctor: "%BigInt64Array%",
        proto: "%BigInt64Array.prototype%",
        tag: "BigInt64Array",
    },
    KindSpec {
        element_type: ElementType::BigUint64,
        ctor: "%BigUint64Array%",
        proto: "%BigUint64Array.prototype%",
        tag: "BigUint64Array",
    },
];

fn placeholder(name: &'static str) -> NativeFn {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

fn key(index: u64) -> JsString {
    JsString::from_utf8(&index.to_string())
}

/// IsTypedArray (spec 25.2.4.4): an Integer-Indexed exotic object.
fn is_typed_array(value: &Value) -> bool {
    match value {
        Value::Object(obj) => matches!(obj.kind, ObjectKind::IntegerIndexed(_)),
        _ => false,
    }
}

/// The Integer-Indexed slots of a TypedArray value.
fn typed_array_slots(value: &Value) -> Option<Handle<TypedArraySlots>> {
    match value {
        Value::Object(obj) => match &obj.kind {
            ObjectKind::IntegerIndexed(slots) => Some(slots.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// ValidateTypedArray (spec 25.2.4.3): the value is a TypedArray whose
/// buffer is not detached.
/// IsTypedArrayOutOfBounds (spec 10.4.5.2 with resizable buffers): a fixed
/// view whose byte range exceeds the buffer, or an auto view whose byte
/// offset exceeds the buffer.
fn typed_array_out_of_bounds(agent: &Agent, slots: &TypedArraySlots) -> bool {
    if typed_array_buffer_detached(agent, slots) {
        return false;
    }
    if slots.auto_length {
        slots.byte_offset > slots.buffer.byte_length()
    } else {
        slots.byte_length > 0 && slots.byte_offset + slots.byte_length > slots.buffer.byte_length()
    }
}

/// ValidateTypedArray(O, write) (spec 25.2.4.5 with immutable buffers): the
/// writing methods reject an immutable buffer before any argument coercion.
fn validate_typed_array_write(
    agent: &mut Agent,
    value: &Value,
) -> Result<Handle<TypedArraySlots>, JsError> {
    let slots = validate_typed_array(agent, value)?;
    if slots.buffer.is_immutable() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray buffer is immutable".into(),
        ));
    }
    Ok(slots)
}

fn validate_typed_array(
    agent: &mut Agent,
    value: &Value,
) -> Result<Handle<TypedArraySlots>, JsError> {
    let slots = typed_array_slots_required(value)?;
    if typed_array_buffer_detached(agent, &slots) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray buffer is detached".into(),
        ));
    }
    // A view whose byte range no longer fits the (resized) buffer is out of
    // bounds and throws on validate (spec 25.2.4.5 step 5).
    if typed_array_out_of_bounds(agent, &slots) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray view is out of bounds".into(),
        ));
    }
    Ok(slots)
}

/// RequireInternalSlot([[TypedArrayName]]): the value is a TypedArray. The
/// `length`/`byteLength`/`byteOffset`/`buffer` accessors validate this way —
/// they do not reject a detached buffer (spec 25.2.3.1-4).
fn typed_array_slots_required(value: &Value) -> Result<Handle<TypedArraySlots>, JsError> {
    typed_array_slots(value).ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "Method called on an incompatible receiver".into(),
        )
    })
}

/// Whether the TypedArray's backing buffer is detached (missing from the
/// buffer table or marked detached).
fn typed_array_buffer_detached(agent: &Agent, slots: &TypedArraySlots) -> bool {
    let buffer_id = as_object(&slots.buffer_object)
        .map(|object| object.id())
        .unwrap_or(u64::MAX);
    !agent.buffer_data.contains_key(&buffer_id)
        || crate::builtins::array_buffer::is_detached(agent, buffer_id)
}

/// IsArrayBuffer: an object registered in the buffer table (Phase 14 adds
/// the full ArrayBuffer builtin; Phase 12 buffers back TypedArrays).
fn is_array_buffer(agent: &Agent, value: &Value) -> bool {
    match value {
        Value::Object(obj) => agent.buffer_data.contains_key(&obj.id()),
        _ => false,
    }
}

/// Get (spec 7.3.1) with the base as receiver.
fn get(agent: &mut Agent, value: &Value, name: &JsString) -> Result<Value, JsError> {
    get_property(agent, value, name, value.clone())
}

/// Set (spec 7.3.3) with `throw = true`.
fn set_property(value: &Value, name: &JsString, v: Value) -> Result<(), JsError> {
    match value {
        Value::Object(obj) => {
            obj.set(name, v, true)?;
            Ok(())
        }
        Value::Function(function) => {
            function.object.set(name, v, true)?;
            Ok(())
        }
        _ => Ok(()),
    }
}

/// LengthOfArrayLike (spec 7.3.22).
fn length_of_array_like(agent: &mut Agent, value: &Value) -> Result<u64, JsError> {
    let length = get_property(agent, value, &JsString::from_utf8("length"), value.clone())?;
    Ok(crux::convert::to_length(crate::context::to_number(
        agent, &length,
    )?))
}

/// GetPrototypeFromConstructor (spec 10.1.14): `constructor.prototype` when
/// it is an object, else the realm's default.
fn get_prototype_from_constructor(
    agent: &mut Agent,
    constructor: &Value,
    default_name: &str,
) -> Result<Handle<JsObject>, JsError> {
    let proto = crate::context::get_property_key(
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

/// AllocateTypedArrayBuffer (spec 25.2.2.4): a fresh zero-filled view with
/// the given element type and length. The buffer object's agent entry and the
/// slots share the same `SharedBuffer` storage.
fn allocate_typed_array_buffer(
    agent: &mut Agent,
    prototype: Handle<JsObject>,
    element_type: ElementType,
    length: usize,
) -> Result<Value, JsError> {
    let element_size = element_type.size();
    let byte_length = length
        .checked_mul(element_size)
        .ok_or_else(|| JsError::new(ErrorKind::RangeError, "TypedArray length overflow".into()))?;
    if byte_length > crate::builtins::array_buffer::MAX_BYTE_LENGTH {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "TypedArray length exceeds the host limit".into(),
        ));
    }
    let buffer = SharedBuffer::new(byte_length);
    // The backing store is a real ArrayBuffer object (spec 25.2.2.4): it
    // must carry %ArrayBuffer.prototype% so `buffer.byteLength` and friends
    // resolve through the usual prototype chain.
    let buffer_proto = agent
        .current_realm()?
        .intrinsics
        .get("%ArrayBuffer.prototype%")
        .and_then(|value| as_object(&value));
    let buffer_object = JsObject::ordinary_object_create(buffer_proto);
    agent.buffer_data.insert(
        buffer_object.id(),
        std::cell::RefCell::new(crate::builtins::array_buffer::BufferState::fixed(
            buffer.clone(),
            byte_length,
        )),
    );
    let slots = TypedArraySlots {
        buffer_object: Value::Object(buffer_object),
        buffer,
        element_type,
        byte_length,
        byte_offset: 0,
        array_length: length,
        auto_length: false,
    };
    let object = JsObject::integer_indexed_object_create(slots, Some(prototype))?;
    Ok(Value::Object(object))
}

/// The clamped end of `slice`/`fill`/`copyWithin` (undefined means `len`).
fn clamped_end(agent: &mut Agent, args: &[Value], len: u64) -> Result<u64, JsError> {
    match args.get(1) {
        None | Some(Value::Undefined) => Ok(len),
        Some(value) => {
            let n = to_integer_or_infinity(crate::context::to_number(agent, value)?);
            Ok(if n < 0.0 {
                (len as i64).saturating_add(n as i64).max(0) as u64
            } else {
                (n as u64).min(len)
            })
        }
    }
}

/// ToIntegerOrInfinity of the first argument (undefined → 0), clamped to
/// `[0, len]`.
fn clamped_start(agent: &mut Agent, args: &[Value], len: u64) -> Result<u64, JsError> {
    let n = to_integer_or_infinity(crate::context::to_number(
        agent,
        &args.first().cloned().unwrap_or(Value::Undefined),
    )?);
    Ok(if n < 0.0 {
        (len as i64).saturating_add(n as i64).max(0) as u64
    } else {
        (n as u64).min(len)
    })
}

/// TypedArrayCreate (spec 25.2.4.1): construct `constructor` with « length »,
/// then validate the result: it must be a TypedArray of the requested length
/// whose buffer is not detached.
fn typed_array_create(
    agent: &mut Agent,
    constructor: &Value,
    length: usize,
) -> Result<Value, JsError> {
    let result = crate::function::construct(
        agent,
        constructor,
        &[Value::Number(length as f64)],
        constructor,
    )?;
    let result_slots = typed_array_slots_required(&result)?;
    validate_typed_array(agent, &result)?;
    // The created array is the write destination of the caller, so an
    // immutable buffer is rejected here (spec 25.2.4.1 with accessMode
    // write).
    if result_slots.buffer.is_immutable() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray buffer is immutable".into(),
        ));
    }
    if typed_array_effective_length(&result_slots) < length {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArrayCreate produced a shorter TypedArray".into(),
        ));
    }
    Ok(result)
}

/// SpeciesConstructor (spec 7.3.24): the exemplar's `constructor` (or the
/// default when it is undefined), then its `[Symbol.species]`.
fn species_constructor(
    agent: &mut Agent,
    exemplar: &Value,
    default_ctor: Value,
) -> Result<Value, JsError> {
    let ctor = get(agent, exemplar, &JsString::from_utf8("constructor"))?;
    if matches!(ctor, Value::Undefined) {
        return Ok(default_ctor);
    }
    if !is_constructor(&ctor) && !matches!(ctor, Value::Object(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "constructor is not an object".into(),
        ));
    }
    let species_key = PropertyKey::Symbol(crux::symbol::well_known("species").as_ref().clone());
    let species = crate::context::get_property_key(agent, &ctor, &species_key, ctor.clone())?;
    match species {
        Value::Null | Value::Undefined => Ok(default_ctor),
        value if is_constructor(&value) => Ok(value),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "species is not a constructor".into(),
        )),
    }
}

/// The default constructor for `exemplar`'s element type (its realm's kind
/// constructor).
fn default_species_ctor(agent: &mut Agent, exemplar_slots: &TypedArraySlots) -> Value {
    let kind = KINDS
        .iter()
        .find(|k| k.element_type == exemplar_slots.element_type)
        .expect("known element type");
    agent
        .current_realm()
        .ok()
        .and_then(|realm| realm.intrinsics.get(kind.ctor))
        .unwrap_or(Value::Undefined)
}

/// TypedArraySpeciesCreate (spec 25.2.4.2): the species of `exemplar`
/// constructed with « length », with the content type preserved. The default
/// (no custom species) is the exemplar's own kind constructor, which yields
/// the same-kind result.
fn typed_array_species_create(
    agent: &mut Agent,
    exemplar: &Value,
    length: usize,
) -> Result<Value, JsError> {
    let exemplar_slots = typed_array_slots(exemplar)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "Incompatible receiver".into()))?;
    let default_ctor = default_species_ctor(agent, &exemplar_slots);
    let constructor = species_constructor(agent, exemplar, default_ctor)?;
    let result = typed_array_create(agent, &constructor, length)?;
    assert_same_content_type(&result, &exemplar_slots)?;
    Ok(result)
}

/// TypedArraySpeciesCreate over an existing buffer (spec 25.2.3.30
/// `subarray` step 17): the species constructed with « buffer, byteOffset,
/// length » so the result shares the source buffer.
fn typed_array_species_create_view(
    agent: &mut Agent,
    exemplar: &Value,
    buffer: Value,
    byte_offset: usize,
    length: usize,
) -> Result<Value, JsError> {
    let exemplar_slots = typed_array_slots(exemplar)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "Incompatible receiver".into()))?;
    let default_ctor = default_species_ctor(agent, &exemplar_slots);
    let constructor = species_constructor(agent, exemplar, default_ctor)?;
    let result = crate::function::construct(
        agent,
        &constructor,
        &[
            buffer,
            Value::Number(byte_offset as f64),
            Value::Number(length as f64),
        ],
        &constructor,
    )?;
    assert_same_content_type(&result, &exemplar_slots)?;
    Ok(result)
}

/// The species result must be a TypedArray with the exemplar's content type
/// (spec 25.2.4.2 steps 8-9).
fn assert_same_content_type(
    result: &Value,
    exemplar_slots: &TypedArraySlots,
) -> Result<(), JsError> {
    let result_slots = typed_array_slots(result).ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "TypedArrayCreate produced a non-TypedArray".into(),
        )
    })?;
    if result_slots.element_type.content_type() != exemplar_slots.element_type.content_type() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Species constructor changed the content type".into(),
        ));
    }
    Ok(())
}

/// The TypedArray constructor (spec 25.2.2.1): `new` only, with the length,
/// object (typed-array / buffer / iterable / array-like), and multiple-args
/// paths.
fn typed_array_construct(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
    element_type: ElementType,
) -> Result<Value, JsError> {
    if matches!(new_target, Value::Undefined) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray constructors must be invoked with 'new'".into(),
        ));
    }
    let kind = KINDS
        .iter()
        .find(|k| k.element_type == element_type)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "Unknown element type".into()))?;
    // The argument is classified (ToIndex for a non-object, a content-type
    // check for a typed-array source) before the constructor's prototype is
    // read, so e.g. `new TA(Symbol())` throws the ToIndex TypeError first
    // (spec 25.2.2.1).
    if let Some(first) = args.first()
        && !matches!(first, Value::Object(_) | Value::Function(_))
        && args.len() == 1
    {
        let length = crate::context::to_index(agent, first)? as usize;
        let prototype = get_prototype_from_constructor(agent, new_target, kind.proto)?;
        return allocate_typed_array_buffer(agent, prototype, element_type, length);
    }
    let prototype = get_prototype_from_constructor(agent, new_target, kind.proto)?;
    if args.is_empty() {
        return allocate_typed_array_buffer(agent, prototype, element_type, 0);
    }
    if let Some(first) = args.first()
        && matches!(first, Value::Object(_) | Value::Function(_))
    {
        if is_typed_array(first) && args.len() == 1 {
            return copy_typed_array(agent, prototype, element_type, first);
        }
        if is_array_buffer(agent, first) {
            // spec 25.2.3.x step 5.b.ii: the buffer view takes byteOffset and
            // length from the remaining arguments.
            return typed_array_buffer_path(agent, prototype, element_type, first, &args[1..]);
        }
        if args.len() == 1 {
            return iterate_source(agent, prototype, element_type, first);
        }
    }
    // Multiple arguments: the argument list is the element list (spec step 7).
    let dst = allocate_typed_array_buffer(agent, prototype, element_type, args.len())?;
    for (k, value) in args.iter().enumerate() {
        set_property(&dst, &key(k as u64), value.clone())?;
    }
    Ok(dst)
}

/// The single-object path of the TypedArray constructor: iterate (or treat
/// as an array-like) the source (spec 25.2.2.1 step 5.c).
fn iterate_source(
    agent: &mut Agent,
    prototype: Handle<JsObject>,
    element_type: ElementType,
    items: &Value,
) -> Result<Value, JsError> {
    if let Some(method) = get_method(agent, items, "@@iterator")? {
        let iterator = crate::function::call(agent, &method, items.clone(), &[])?;
        let next = get_property(
            agent,
            &iterator,
            &JsString::from_utf8("next"),
            iterator.clone(),
        )?;
        if !is_callable(&next) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Iterator's next method is not callable".into(),
            ));
        }
        let record = IteratorRecord { iterator, next };
        let mut values = Vec::new();
        while let Some(value) = crate::expr::iterator_step(agent, &record)? {
            values.push(value);
        }
        let dst = allocate_typed_array_buffer(agent, prototype, element_type, values.len())?;
        for (k, value) in values.into_iter().enumerate() {
            set_property(&dst, &key(k as u64), value)?;
        }
        return Ok(dst);
    }
    let array_like = crate::context::to_object(agent, items)?;
    let length = length_of_array_like(agent, &array_like)? as usize;
    let dst = allocate_typed_array_buffer(agent, prototype, element_type, length)?;
    for k in 0..length {
        let value = get(agent, &array_like, &key(k as u64))?;
        set_property(&dst, &key(k as u64), value)?;
    }
    Ok(dst)
}

/// TypedArray(typedArray) (spec 25.2.2.1 step 5.a): copy the elements,
/// converting between element types (a content-type mismatch throws).
fn copy_typed_array(
    agent: &mut Agent,
    prototype: Handle<JsObject>,
    element_type: ElementType,
    source: &Value,
) -> Result<Value, JsError> {
    let source_slots = validate_typed_array(agent, source)?;
    if source_slots.element_type.content_type() != element_type.content_type() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot mix BigInt and Number typed arrays".into(),
        ));
    }
    let source_length = typed_array_effective_length(&source_slots);
    let dst = allocate_typed_array_buffer(agent, prototype, element_type, source_length)?;
    if source_slots.element_type == element_type {
        // Same element type: copy the byte range directly.
        let start = source_slots.byte_offset;
        let data = source_slots.buffer.read(start, source_slots.byte_length)?;
        let dst_slots = typed_array_slots(&dst).expect("fresh typed array");
        dst_slots.buffer.write(0, &data)?;
    } else {
        for k in 0..source_length {
            let value = get(agent, source, &key(k as u64))?;
            set_property(&dst, &key(k as u64), value)?;
        }
    }
    Ok(dst)
}

/// TypedArray(buffer [, byteOffset [, length]]) (spec 25.2.2.1 step 5.b):
/// view a byte range of the buffer.
fn typed_array_buffer_path(
    agent: &mut Agent,
    prototype: Handle<JsObject>,
    element_type: ElementType,
    buffer: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let Value::Object(buffer_object) = buffer else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "ArrayBuffer expected".into(),
        ));
    };
    let (resizable, shared, buffer_byte_length) = {
        let state = agent
            .buffer_data
            .get(&buffer_object.id())
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "Expected an ArrayBuffer".into()))?;
        if state.borrow().detached {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "ArrayBuffer is detached".into(),
            ));
        }
        let state = state.borrow();
        (state.resizable, state.shared.clone(), state.byte_length)
    };
    let element_size = element_type.size();
    let byte_offset = match args.first() {
        None | Some(Value::Undefined) => 0,
        Some(value) => {
            let offset = crate::context::to_index(agent, value)? as usize;
            if !offset.is_multiple_of(element_size) {
                return Err(JsError::new(
                    ErrorKind::RangeError,
                    "byteOffset must be a multiple of the element size".into(),
                ));
            }
            offset
        }
    };
    // The byteOffset coercion may have detached the buffer (spec 25.2.2.1
    // step 5.b: IsDetachedBuffer throws a TypeError).
    if agent
        .buffer_data
        .get(&buffer_object.id())
        .map(|state| state.borrow().detached)
        .unwrap_or(false)
    {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "ArrayBuffer is detached".into(),
        ));
    }
    let explicit_length = args.len() > 1 && !matches!(args[1], Value::Undefined);
    let byte_length = if explicit_length {
        let length = crate::context::to_index(agent, &args[1])? as usize;
        // The length coercion may have detached the buffer (spec 25.2.2.1
        // step 5.b: IsDetachedBuffer throws a TypeError).
        if agent
            .buffer_data
            .get(&buffer_object.id())
            .map(|state| state.borrow().detached)
            .unwrap_or(false)
        {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "ArrayBuffer is detached".into(),
            ));
        }
        let bytes = length.checked_mul(element_size).ok_or_else(|| {
            JsError::new(ErrorKind::RangeError, "TypedArray length overflow".into())
        })?;
        if byte_offset + bytes > buffer_byte_length {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "TypedArray range exceeds the buffer".into(),
            ));
        }
        bytes
    } else {
        if byte_offset > buffer_byte_length {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "byteOffset exceeds the buffer".into(),
            ));
        }
        // A fixed-length view's byte range must be a whole number of
        // elements (spec 25.2.2.1 step 13); a resizable buffer view is auto.
        if !resizable && (buffer_byte_length - byte_offset) % element_size != 0 {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "byteLength must be a multiple of the element size".into(),
            ));
        }
        buffer_byte_length - byte_offset
    };
    let array_length = byte_length / element_size;
    let slots = TypedArraySlots {
        buffer_object: buffer.clone(),
        buffer: shared,
        element_type,
        byte_length,
        byte_offset,
        array_length,
        // A view over a resizable buffer without an explicit length tracks
        // the buffer (spec 25.2.2.1: [[ArrayLength]] is auto).
        auto_length: resizable && !explicit_length,
    };
    let object = JsObject::integer_indexed_object_create(slots, Some(prototype))?;
    Ok(Value::Object(object))
}

/// spec 25.2.3.2 (per-kind) length accessor.
fn get_length(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let slots = typed_array_slots_required(this)?;
    if typed_array_buffer_detached(agent, &slots) {
        return Ok(Value::Number(0.0));
    }
    Ok(Value::Number(typed_array_effective_length(&slots) as f64))
}

fn get_buffer(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let slots = typed_array_slots_required(this)?;
    let _ = agent;
    Ok(slots.buffer_object.clone())
}

fn get_byte_length(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let slots = typed_array_slots_required(this)?;
    if typed_array_buffer_detached(agent, &slots) {
        return Ok(Value::Number(0.0));
    }
    if typed_array_out_of_bounds(agent, &slots) {
        return Ok(Value::Number(0.0));
    }
    if slots.auto_length {
        // An auto-length view reports the current remaining buffer bytes,
        // rounded down to a whole element.
        let bytes = slots.buffer.byte_length().saturating_sub(slots.byte_offset);
        let whole = bytes / slots.element_type.size() * slots.element_type.size();
        return Ok(Value::Number(whole as f64));
    }
    Ok(Value::Number(slots.byte_length as f64))
}

fn get_byte_offset(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let slots = typed_array_slots_required(this)?;
    if typed_array_buffer_detached(agent, &slots) {
        return Ok(Value::Number(0.0));
    }
    if typed_array_out_of_bounds(agent, &slots) {
        return Ok(Value::Number(0.0));
    }
    Ok(Value::Number(slots.byte_offset as f64))
}

/// spec 25.2.3.5 TypedArray.prototype.at.
fn at(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_typed_array(agent, this)?;
    let length = typed_array_effective_length(&slots) as u64;
    let relative = to_integer_or_infinity(crate::context::to_number(
        agent,
        &args.first().cloned().unwrap_or(Value::Undefined),
    )?);
    let k = if relative >= 0.0 {
        relative as u64
    } else {
        (length as i64).saturating_add(relative as i64).max(0) as u64
    };
    if k >= length {
        return Ok(Value::Undefined);
    }
    get(agent, this, &key(k))
}

/// spec 25.2.3.32 get %TypedArray%.prototype[@@toStringTag]: the element
/// type's name, or *undefined* when `this` is not a TypedArray.
fn get_to_string_tag(_agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let Some(slots) = typed_array_slots(this) else {
        return Ok(Value::Undefined);
    };
    let kind = KINDS
        .iter()
        .find(|k| k.element_type == slots.element_type)
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "Unknown element type".into()))?;
    Ok(Value::String(Handle::new(JsString::from_utf8(kind.tag))))
}

/// spec 25.2.3.6 TypedArray.prototype.copyWithin.
fn copy_within(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_typed_array_write(agent, this)?;
    let length = typed_array_effective_length(&slots) as u64;
    let relative_target = to_integer_or_infinity(crate::context::to_number(
        agent,
        &args.first().cloned().unwrap_or(Value::Undefined),
    )?);
    let to = if relative_target < 0.0 {
        (length as i64)
            .saturating_add(relative_target as i64)
            .max(0) as u64
    } else {
        (relative_target as u64).min(length)
    };
    let relative_start = to_integer_or_infinity(crate::context::to_number(
        agent,
        &args.get(1).cloned().unwrap_or(Value::Undefined),
    )?);
    let from = if relative_start < 0.0 {
        (length as i64).saturating_add(relative_start as i64).max(0) as u64
    } else {
        (relative_start as u64).min(length)
    };
    let final_index = clamped_end(agent, args.get(1..).unwrap_or(&[]), length)?;
    if typed_array_buffer_detached(agent, &slots) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray buffer is detached".into(),
        ));
    }
    let count = final_index
        .saturating_sub(from)
        .min(length.saturating_sub(to));
    if count > 0 {
        let mut from = from;
        let mut to = to;
        let direction = if from < to && to < from + count {
            from = from + count - 1;
            to = to + count - 1;
            -1i64
        } else {
            1i64
        };
        for _ in 0..count {
            let value = get(agent, this, &key(from))?;
            set_property(this, &key(to), value)?;
            from = (from as i64 + direction) as u64;
            to = (to as i64 + direction) as u64;
        }
    }
    Ok(this.clone())
}

/// spec 25.2.3.7 TypedArray.prototype.entries.
fn entries(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    validate_typed_array(agent, this)?;
    crate::builtins::array::create_array_iterator(
        agent,
        this.clone(),
        crate::builtins::array::ArrayIterationKind::KeyValue,
    )
}

/// spec 25.2.3.8 TypedArray.prototype.every.
fn every(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_typed_array(agent, this)?;
    let callbackfn = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&callbackfn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray.prototype.every: callbackfn is not a function".into(),
        ));
    }
    let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    for k in 0..typed_array_effective_length(&slots) as u64 {
        let k_value = get(agent, this, &key(k))?;
        let test = crate::function::call(
            agent,
            &callbackfn,
            this_arg.clone(),
            &[k_value, Value::Number(k as f64), this.clone()],
        )?;
        if !to_boolean(&test) {
            return Ok(Value::Boolean(false));
        }
    }
    Ok(Value::Boolean(true))
}

/// spec 25.2.3.9 TypedArray.prototype.fill.
fn fill(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_typed_array_write(agent, this)?;
    let length = typed_array_effective_length(&slots) as u64;
    // The value is coerced exactly once, before the index arguments (spec
    // 25.2.3.9 step 4); a coercion that detaches the buffer is caught by
    // the detached check below.
    let value = match slots.element_type.content_type() {
        ContentType::BigInt => Value::BigInt(Handle::new(crate::context::to_big_int(
            agent,
            &args.first().cloned().unwrap_or(Value::Undefined),
        )?)),
        ContentType::Number => Value::Number(crate::context::to_number(
            agent,
            &args.first().cloned().unwrap_or(Value::Undefined),
        )?),
    };
    let k = clamped_start(agent, args.get(1..).unwrap_or(&[]), length)?;
    let final_index = clamped_end(agent, args.get(1..).unwrap_or(&[]), length)?;
    if typed_array_buffer_detached(agent, &slots) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray buffer is detached".into(),
        ));
    }
    for k in k..final_index {
        set_property(this, &key(k), value.clone())?;
    }
    Ok(this.clone())
}

/// spec 25.2.3.10 TypedArray.prototype.filter.
fn filter(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_typed_array(agent, this)?;
    let callbackfn = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&callbackfn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray.prototype.filter: callbackfn is not a function".into(),
        ));
    }
    let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    let mut kept = Vec::new();
    for k in 0..typed_array_effective_length(&slots) as u64 {
        let k_value = get(agent, this, &key(k))?;
        let selected = crate::function::call(
            agent,
            &callbackfn,
            this_arg.clone(),
            &[k_value.clone(), Value::Number(k as f64), this.clone()],
        )?;
        if to_boolean(&selected) {
            kept.push(k_value);
        }
    }
    let result = typed_array_species_create(agent, this, kept.len())?;
    for (k, value) in kept.into_iter().enumerate() {
        set_property(&result, &key(k as u64), value)?;
    }
    Ok(result)
}

/// The shared forward search of find/findIndex.
fn find_common(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    want_index: bool,
) -> Result<Value, JsError> {
    let slots = validate_typed_array(agent, this)?;
    let predicate = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&predicate) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "predicate is not a function".into(),
        ));
    }
    let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    for k in 0..typed_array_effective_length(&slots) as u64 {
        let k_value = get(agent, this, &key(k))?;
        let test = crate::function::call(
            agent,
            &predicate,
            this_arg.clone(),
            &[k_value.clone(), Value::Number(k as f64), this.clone()],
        )?;
        if to_boolean(&test) {
            return Ok(if want_index {
                Value::Number(k as f64)
            } else {
                k_value
            });
        }
    }
    Ok(if want_index {
        Value::Number(-1.0)
    } else {
        Value::Undefined
    })
}

/// spec 25.2.3.11 TypedArray.prototype.find.
fn find(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    find_common(agent, this, args, false)
}

/// spec 25.2.3.12 TypedArray.prototype.findIndex.
fn find_index(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    find_common(agent, this, args, true)
}

/// The shared descending search of findLast/findLastIndex.
fn find_last_common(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    want_index: bool,
) -> Result<Value, JsError> {
    let slots = validate_typed_array(agent, this)?;
    let predicate = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&predicate) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "predicate is not a function".into(),
        ));
    }
    let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    let mut k = typed_array_effective_length(&slots) as i64 - 1;
    while k >= 0 {
        let k_value = get(agent, this, &key(k as u64))?;
        let test = crate::function::call(
            agent,
            &predicate,
            this_arg.clone(),
            &[k_value.clone(), Value::Number(k as f64), this.clone()],
        )?;
        if to_boolean(&test) {
            return Ok(if want_index {
                Value::Number(k as f64)
            } else {
                k_value
            });
        }
        k -= 1;
    }
    Ok(if want_index {
        Value::Number(-1.0)
    } else {
        Value::Undefined
    })
}

/// spec 25.2.3.13 TypedArray.prototype.findLast.
fn find_last(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    find_last_common(agent, this, args, false)
}

/// spec 25.2.3.14 TypedArray.prototype.findLastIndex.
fn find_last_index(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    find_last_common(agent, this, args, true)
}

/// spec 25.2.3.15 TypedArray.prototype.forEach.
fn for_each(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_typed_array(agent, this)?;
    let callbackfn = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&callbackfn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray.prototype.forEach: callbackfn is not a function".into(),
        ));
    }
    let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    for k in 0..typed_array_effective_length(&slots) as u64 {
        let k_value = get(agent, this, &key(k))?;
        crate::function::call(
            agent,
            &callbackfn,
            this_arg.clone(),
            &[k_value, Value::Number(k as f64), this.clone()],
        )?;
    }
    Ok(Value::Undefined)
}

/// spec 25.2.3.16 TypedArray.prototype.includes.
fn includes(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_typed_array(agent, this)?;
    let search_element = args.first().cloned().unwrap_or(Value::Undefined);
    let length = typed_array_effective_length(&slots) as u64;
    if length == 0 {
        return Ok(Value::Boolean(false));
    }
    let n = to_integer_or_infinity(crate::context::to_number(
        agent,
        &args.get(1).cloned().unwrap_or(Value::Undefined),
    )?);
    let k = if n >= 0.0 {
        n as u64
    } else {
        (length as i64).saturating_add(n as i64).max(0) as u64
    };
    for k in k..length {
        let element = get(agent, this, &key(k))?;
        if same_value_zero(&element, &search_element) {
            return Ok(Value::Boolean(true));
        }
    }
    Ok(Value::Boolean(false))
}

/// spec 25.2.3.17 TypedArray.prototype.indexOf.
fn index_of(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_typed_array(agent, this)?;
    let search_element = args.first().cloned().unwrap_or(Value::Undefined);
    let length = typed_array_effective_length(&slots) as u64;
    if length == 0 {
        return Ok(Value::Number(-1.0));
    }
    let n = to_integer_or_infinity(crate::context::to_number(
        agent,
        &args.get(1).cloned().unwrap_or(Value::Undefined),
    )?);
    let k = if n >= 0.0 {
        n as u64
    } else {
        (length as i64).saturating_add(n as i64).max(0) as u64
    };
    for k in k..length {
        let present = as_object(this)
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "Incompatible receiver".into()))?
            .has_property_key(&PropertyKey::from_utf8(&k.to_string()))?;
        if present {
            let element = get(agent, this, &key(k))?;
            if is_strictly_equal(&element, &search_element) {
                return Ok(Value::Number(k as f64));
            }
        }
    }
    Ok(Value::Number(-1.0))
}

/// spec 25.2.3.18 TypedArray.prototype.join.
fn join(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_typed_array(agent, this)?;
    let length = typed_array_effective_length(&slots) as u64;
    let separator = match args.first() {
        Some(Value::Undefined) | None => ",".to_string(),
        Some(value) => crate::context::to_string(agent, value)?.to_string_lossy(),
    };
    let mut result = String::new();
    for k in 0..length {
        if k > 0 {
            result.push_str(&separator);
        }
        let element = get(agent, this, &key(k))?;
        let text = if matches!(element, Value::Undefined | Value::Null) {
            String::new()
        } else {
            crate::context::to_string(agent, &element)?.to_string_lossy()
        };
        result.push_str(&text);
    }
    Ok(Value::String(Handle::new(JsString::from_utf8(&result))))
}

/// spec 25.2.3.19 TypedArray.prototype.keys.
fn keys(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    validate_typed_array(agent, this)?;
    crate::builtins::array::create_array_iterator(
        agent,
        this.clone(),
        crate::builtins::array::ArrayIterationKind::Key,
    )
}

/// spec 25.2.3.20 TypedArray.prototype.lastIndexOf.
fn last_index_of(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_typed_array(agent, this)?;
    let search_element = args.first().cloned().unwrap_or(Value::Undefined);
    let length = typed_array_effective_length(&slots) as u64;
    if length == 0 {
        return Ok(Value::Number(-1.0));
    }
    let n = if args.len() < 2 {
        length as f64 - 1.0
    } else {
        to_integer_or_infinity(crate::context::to_number(agent, &args[1])?)
    };
    let mut k: i64 = if n >= 0.0 {
        (n as u64).min(length - 1) as i64
    } else {
        // Spec: k = len + n (no clamp to 0); a negative k skips the loop.
        (length as i64).saturating_add(n as i64)
    };
    while k >= 0 {
        let present = as_object(this)
            .ok_or_else(|| JsError::new(ErrorKind::TypeError, "Incompatible receiver".into()))?
            .has_property_key(&PropertyKey::from_utf8(&k.to_string()))?;
        if present {
            let element = get(agent, this, &key(k as u64))?;
            if is_strictly_equal(&element, &search_element) {
                return Ok(Value::Number(k as f64));
            }
        }
        k -= 1;
    }
    Ok(Value::Number(-1.0))
}

/// spec 25.2.3.21 TypedArray.prototype.map.
fn map(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_typed_array(agent, this)?;
    let callbackfn = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&callbackfn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray.prototype.map: callbackfn is not a function".into(),
        ));
    }
    let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    let result = typed_array_species_create(agent, this, typed_array_effective_length(&slots))?;
    for k in 0..typed_array_effective_length(&slots) as u64 {
        let k_value = get(agent, this, &key(k))?;
        let mapped = crate::function::call(
            agent,
            &callbackfn,
            this_arg.clone(),
            &[k_value, Value::Number(k as f64), this.clone()],
        )?;
        set_property(&result, &key(k), mapped)?;
    }
    Ok(result)
}

/// spec 25.2.3.22 TypedArray.prototype.reduce.
fn reduce(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_typed_array(agent, this)?;
    let callbackfn = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&callbackfn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray.prototype.reduce: callbackfn is not a function".into(),
        ));
    }
    let length = typed_array_effective_length(&slots) as u64;
    let mut k = 0u64;
    let mut accumulator: Value;
    if args.len() >= 2 {
        accumulator = args[1].clone();
    } else {
        if length == 0 {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Reduce of empty TypedArray with no initial value".into(),
            ));
        }
        accumulator = get(agent, this, &key(0))?;
        k = 1;
    }
    while k < length {
        let k_value = get(agent, this, &key(k))?;
        accumulator = crate::function::call(
            agent,
            &callbackfn,
            Value::Undefined,
            &[accumulator, k_value, Value::Number(k as f64), this.clone()],
        )?;
        k += 1;
    }
    Ok(accumulator)
}

/// spec 25.2.3.23 TypedArray.prototype.reduceRight.
fn reduce_right(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_typed_array(agent, this)?;
    let callbackfn = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&callbackfn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray.prototype.reduceRight: callbackfn is not a function".into(),
        ));
    }
    let length = typed_array_effective_length(&slots) as u64;
    let mut k = length as i64 - 1;
    let mut accumulator: Value;
    if args.len() >= 2 {
        accumulator = args[1].clone();
    } else {
        if length == 0 {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Reduce of empty TypedArray with no initial value".into(),
            ));
        }
        accumulator = get(agent, this, &key(length - 1))?;
        k -= 1;
    }
    while k >= 0 {
        let k_value = get(agent, this, &key(k as u64))?;
        accumulator = crate::function::call(
            agent,
            &callbackfn,
            Value::Undefined,
            &[accumulator, k_value, Value::Number(k as f64), this.clone()],
        )?;
        k -= 1;
    }
    Ok(accumulator)
}

/// spec 25.2.3.24 TypedArray.prototype.reverse.
fn reverse(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_typed_array_write(agent, this)?;
    let length = typed_array_effective_length(&slots) as u64;
    let mut lower = 0u64;
    while lower < length / 2 {
        let upper = length - 1 - lower;
        let lower_value = get(agent, this, &key(lower))?;
        let upper_value = get(agent, this, &key(upper))?;
        set_property(this, &key(lower), upper_value)?;
        set_property(this, &key(upper), lower_value)?;
        lower += 1;
    }
    Ok(this.clone())
}

/// spec 25.2.3.25 TypedArray.prototype.set.
fn set(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let target_slots = validate_typed_array_write(agent, this)?;
    let source = args.first().cloned().unwrap_or(Value::Undefined);
    let target_length = typed_array_effective_length(&target_slots);
    if is_typed_array(&source) {
        let source_slots = validate_typed_array(agent, &source)?;
        let offset = match args.get(1) {
            None | Some(Value::Undefined) => 0,
            Some(value) => crate::context::to_index(agent, value)? as usize,
        };
        if offset > target_length {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "Offset is out of bounds".into(),
            ));
        }
        let source_length = typed_array_effective_length(&source_slots);
        if source_length > target_length - offset {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "Source is too large".into(),
            ));
        }
        // The offset coercion may have detached either buffer (spec
        // 25.2.3.25.2 steps 9-10).
        if typed_array_buffer_detached(agent, &target_slots)
            || typed_array_buffer_detached(agent, &source_slots)
        {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "TypedArray buffer is detached".into(),
            ));
        }
        if target_slots.element_type == source_slots.element_type {
            // Same type: byte copy (aliasing is handled by the copy). The
            // destination space is clamped to the live buffer bytes so a
            // resized-shrunk buffer fails cleanly instead of panicking.
            let start = source_slots.byte_offset;
            let end = (start + source_slots.byte_length).min(source_slots.buffer.byte_length());
            let data = source_slots.buffer.read(start, end - start)?;
            let dst_start = target_slots.byte_offset + offset * target_slots.element_type.size();
            let count = data
                .len()
                .min(target_slots.buffer.byte_length().saturating_sub(dst_start));
            target_slots.buffer.write(dst_start, &data[..count])?;
        } else {
            for k in 0..source_length {
                let value = get(agent, &source, &key(k as u64))?;
                set_property(this, &key((offset + k) as u64), value)?;
            }
        }
        return Ok(Value::Undefined);
    }
    let source_object = crate::context::to_object(agent, &source)?;
    let source_length = length_of_array_like(agent, &source_object)? as usize;
    let offset = match args.get(1) {
        None | Some(Value::Undefined) => 0,
        Some(value) => crate::context::to_index(agent, value)? as usize,
    };
    if typed_array_buffer_detached(agent, &target_slots) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray buffer is detached".into(),
        ));
    }
    if offset > target_length || source_length > target_length - offset {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "Source is too large".into(),
        ));
    }
    for k in 0..source_length {
        let value = get(agent, &source_object, &key(k as u64))?;
        // TypedArraySetElement coerces the source element through the agent
        // (an element may be an object whose valueOf/toString are intrinsics).
        let coerced = match target_slots.element_type.content_type() {
            ContentType::BigInt => {
                Value::BigInt(Handle::new(crate::context::to_big_int(agent, &value)?))
            }
            ContentType::Number => Value::Number(crate::context::to_number(agent, &value)?),
        };
        set_property(this, &key((offset + k) as u64), coerced)?;
    }
    Ok(Value::Undefined)
}

/// spec 25.2.3.26 TypedArray.prototype.slice.
fn slice(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_typed_array(agent, this)?;
    let length = typed_array_effective_length(&slots) as u64;
    let k = clamped_start(agent, args, length)?;
    let final_index = clamped_end(agent, args, length)?;
    let count = final_index.saturating_sub(k);
    let result = typed_array_species_create(agent, this, count as usize)?;
    // The species constructor may detach the source buffer; per spec the
    // copy only proceeds over a live buffer.
    if count > 0 && typed_array_buffer_detached(agent, &slots) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray buffer is detached".into(),
        ));
    }
    // A resize during the start/end coercion may have shrunk the buffer: the
    // result keeps its length but only the elements still in bounds are
    // copied (spec 25.2.3.26 with resizable buffers).
    let current = typed_array_effective_length(&slots) as u64;
    let copy_count = count.min(current.saturating_sub(k));
    for i in 0..copy_count {
        let value = get(agent, this, &key(k + i))?;
        set_property(&result, &key(i), value)?;
    }
    Ok(result)
}

/// spec 25.2.3.27 TypedArray.prototype.some.
fn some(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_typed_array(agent, this)?;
    let callbackfn = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&callbackfn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray.prototype.some: callbackfn is not a function".into(),
        ));
    }
    let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    for k in 0..typed_array_effective_length(&slots) as u64 {
        let k_value = get(agent, this, &key(k))?;
        let test = crate::function::call(
            agent,
            &callbackfn,
            this_arg.clone(),
            &[k_value, Value::Number(k as f64), this.clone()],
        )?;
        if to_boolean(&test) {
            return Ok(Value::Boolean(true));
        }
    }
    Ok(Value::Boolean(false))
}

/// TypedArray SortCompare (spec 25.2.3.28.2): numeric default (NaN sorts
/// last), with the optional comparator.
fn typed_sort_compare(
    agent: &mut Agent,
    comparefn: &Value,
    x: &Value,
    y: &Value,
) -> Result<f64, JsError> {
    let x_nan = matches!(x, Value::Number(n) if n.is_nan());
    let y_nan = matches!(y, Value::Number(n) if n.is_nan());
    if x_nan && y_nan {
        return Ok(0.0);
    }
    if x_nan {
        return Ok(1.0);
    }
    if y_nan {
        return Ok(-1.0);
    }
    if !matches!(comparefn, Value::Undefined) {
        let v = crate::function::call(agent, comparefn, Value::Undefined, &[x.clone(), y.clone()])?;
        let v = crate::context::to_number(agent, &v)?;
        return Ok(if v.is_nan() { 0.0 } else { v });
    }
    let order = match (x, y) {
        (Value::BigInt(a), Value::BigInt(b)) => a.as_ref().0.cmp(&b.as_ref().0),
        (Value::Number(a), Value::Number(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
        _ => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Incompatible sort values".into(),
            ));
        }
    };
    Ok(match order {
        Ordering::Less => -1.0,
        Ordering::Greater => 1.0,
        Ordering::Equal => 0.0,
    })
}

/// SortIndexedProperties for TypedArrays: collect, stable-sort, write back.
fn sort_indexed_properties(
    agent: &mut Agent,
    object: &Value,
    length: u64,
    comparefn: &Value,
) -> Result<(), JsError> {
    let mut items: Vec<Value> = Vec::with_capacity(length as usize);
    for k in 0..length {
        items.push(get(agent, object, &key(k))?);
    }
    // Insertion sort: a comparefn abrupt completion propagates immediately,
    // so no further comparisons happen (spec 25.2.3.29: the sort stops on
    // the first error).
    for i in 1..items.len() {
        let mut j = i;
        while j > 0 {
            let cmp = typed_sort_compare(agent, comparefn, &items[j - 1], &items[j])?;
            if cmp > 0.0 {
                items.swap(j - 1, j);
                j -= 1;
            } else {
                break;
            }
        }
    }
    for (k, item) in items.into_iter().enumerate() {
        set_property(object, &key(k as u64), item)?;
    }
    Ok(())
}

/// spec 25.2.3.28 TypedArray.prototype.sort.
fn sort(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_typed_array_write(agent, this)?;
    let comparefn = args.first().cloned().unwrap_or(Value::Undefined);
    if !matches!(comparefn, Value::Undefined) && !is_callable(&comparefn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray.prototype.sort: comparefn is not a function".into(),
        ));
    }
    sort_indexed_properties(
        agent,
        this,
        typed_array_effective_length(&slots) as u64,
        &comparefn,
    )?;
    Ok(this.clone())
}

/// spec 25.2.3.29 TypedArray.prototype.subarray.
fn subarray(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let slots = typed_array_slots_required(this)?;
    let length = typed_array_effective_length(&slots) as u64;
    let begin = clamped_start(agent, args, length)?;
    let end = clamped_end(agent, args, length)?;
    let count = end.saturating_sub(begin);
    let new_byte_offset = slots.byte_offset + begin as usize * slots.element_type.size();
    let buffer = slots.buffer_object.clone();
    if slots.auto_length && matches!(args.get(1), None | Some(Value::Undefined)) {
        // An auto-length view with no explicit end yields an auto-length
        // result: species create with « buffer, byteOffset » (spec 25.2.3.30
        // step 15).
        let default_ctor = default_species_ctor(agent, &slots);
        let constructor = species_constructor(agent, this, default_ctor)?;
        let result = crate::function::construct(
            agent,
            &constructor,
            &[buffer, Value::Number(new_byte_offset as f64)],
            &constructor,
        )?;
        assert_same_content_type(&result, &slots)?;
        return Ok(result);
    }
    // The result is a view over the same buffer, constructed through the
    // species (spec 25.2.3.30 step 17: TypedArraySpeciesCreate with the
    // buffer, byte offset, and new length).
    typed_array_species_create_view(agent, this, buffer, new_byte_offset, count as usize)
}

/// spec 25.2.3.30 TypedArray.prototype.toLocaleString.
fn to_locale_string(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_typed_array(agent, this)?;
    let mut result = String::new();
    for k in 0..typed_array_effective_length(&slots) as u64 {
        if k > 0 {
            result.push(',');
        }
        let element = get(agent, this, &key(k))?;
        if matches!(element, Value::Undefined | Value::Null) {
            continue;
        }
        let boxed = crate::context::to_object(agent, &element)?;
        let method = get(agent, &boxed, &JsString::from_utf8("toLocaleString"))?;
        let text = crate::function::call(agent, &method, boxed, &[])?;
        result.push_str(&crate::context::to_string(agent, &text)?.to_string_lossy());
    }
    Ok(Value::String(Handle::new(JsString::from_utf8(&result))))
}

/// spec 25.2.3.31 TypedArray.prototype.toReversed.
fn to_reversed(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_typed_array(agent, this)?;
    let length = typed_array_effective_length(&slots);
    let result = typed_array_create_same_type(agent, &slots, length)?;
    for k in 0..length as u64 {
        let value = get(agent, this, &key(length as u64 - 1 - k))?;
        set_property(&result, &key(k), value)?;
    }
    Ok(result)
}

/// spec 25.2.3.32 TypedArray.prototype.toSorted.
fn to_sorted(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_typed_array(agent, this)?;
    let comparefn = args.first().cloned().unwrap_or(Value::Undefined);
    if !matches!(comparefn, Value::Undefined) && !is_callable(&comparefn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray.prototype.toSorted: comparefn is not a function".into(),
        ));
    }
    let result = typed_array_create_same_type(agent, &slots, typed_array_effective_length(&slots))?;
    for k in 0..typed_array_effective_length(&slots) as u64 {
        let value = get(agent, this, &key(k))?;
        set_property(&result, &key(k), value)?;
    }
    sort_indexed_properties(
        agent,
        &result,
        typed_array_effective_length(&slots) as u64,
        &comparefn,
    )?;
    Ok(result)
}

/// spec 25.2.3.33 TypedArray.prototype.values.
fn values(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    validate_typed_array(agent, this)?;
    crate::builtins::array::create_array_iterator(
        agent,
        this.clone(),
        crate::builtins::array::ArrayIterationKind::Value,
    )
}

/// TypedArrayCreateSameType (spec 25.2.3.34 step 10 and the other
/// change-array-by-copy methods): the result is always the exemplar's own
/// kind constructor, ignoring @@species.
fn typed_array_create_same_type(
    agent: &mut Agent,
    slots: &TypedArraySlots,
    length: usize,
) -> Result<Value, JsError> {
    let ctor = default_species_ctor(agent, slots);
    typed_array_create(agent, &ctor, length)
}

/// spec 25.2.3.34 TypedArray.prototype.with.
fn with(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_typed_array(agent, this)?;
    let length = typed_array_effective_length(&slots) as u64;
    let relative_index = to_integer_or_infinity(crate::context::to_number(
        agent,
        &args.first().cloned().unwrap_or(Value::Undefined),
    )?);
    let actual_index = if relative_index >= 0.0 {
        relative_index as u64
    } else {
        (length as i64).saturating_add(relative_index as i64) as u64
    };
    // The value is coerced before the index is validated (spec 25.2.3.34
    // steps 8-9): a throwing valueOf wins over a RangeError, and a resize
    // during coercion changes the current length the index is checked
    // against.
    let value = match slots.element_type.content_type() {
        ContentType::BigInt => Value::BigInt(Handle::new(crate::context::to_big_int(
            agent,
            &args.get(1).cloned().unwrap_or(Value::Undefined),
        )?)),
        ContentType::Number => Value::Number(crate::context::to_number(
            agent,
            &args.get(1).cloned().unwrap_or(Value::Undefined),
        )?),
    };
    let current_slots = typed_array_slots_required(this)?;
    if actual_index >= typed_array_effective_length(&current_slots) as u64 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "Index out of range".into(),
        ));
    }
    let result = typed_array_create_same_type(agent, &slots, length as usize)?;
    for k in 0..length {
        let new_value = if k == actual_index {
            value.clone()
        } else {
            get(agent, this, &key(k))?
        };
        set_property(&result, &key(k), new_value)?;
    }
    Ok(result)
}

/// spec 25.2.3.35 TypedArray.prototype[@@iterator].
fn iterator(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    values(agent, this, args)
}

/// spec 25.2.3.36 %TypedArray%.of.
fn of(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    if !is_constructor(this) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray.of: this is not a constructor".into(),
        ));
    }
    let length = args.len();
    let target = typed_array_create(agent, this, length)?;
    for (k, item) in args.iter().enumerate() {
        set_property(&target, &key(k as u64), item.clone())?;
    }
    Ok(target)
}

/// spec 25.2.3.37 %TypedArray%.from.
fn from(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    if !is_constructor(this) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray.from: this is not a constructor".into(),
        ));
    }
    let items = args.first().cloned().unwrap_or(Value::Undefined);
    let mapfn = args.get(1).cloned().unwrap_or(Value::Undefined);
    let this_arg = args.get(2).cloned().unwrap_or(Value::Undefined);
    let mapping = !matches!(mapfn, Value::Undefined);
    if mapping && !is_callable(&mapfn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray.from: mapfn is not a function".into(),
        ));
    }
    let using_iterator = get_method(agent, &items, "@@iterator")?;
    if let Some(method) = using_iterator {
        let iterator = crate::function::call(agent, &method, items.clone(), &[])?;
        let next = get_property(
            agent,
            &iterator,
            &JsString::from_utf8("next"),
            iterator.clone(),
        )?;
        if !is_callable(&next) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Iterator's next method is not callable".into(),
            ));
        }
        let record = IteratorRecord { iterator, next };
        let mut values = Vec::new();
        while let Some(value) = crate::expr::iterator_step(agent, &record)? {
            values.push(value);
        }
        let target = typed_array_create(agent, this, values.len())?;
        for (k, value) in values.into_iter().enumerate() {
            let mapped = if mapping {
                crate::function::call(
                    agent,
                    &mapfn,
                    this_arg.clone(),
                    &[value, Value::Number(k as f64)],
                )?
            } else {
                value
            };
            set_property(&target, &key(k as u64), mapped)?;
        }
        return Ok(target);
    }
    let array_like = crate::context::to_object(agent, &items)?;
    let length = length_of_array_like(agent, &array_like)? as usize;
    let target = typed_array_create(agent, this, length)?;
    for k in 0..length {
        let value = get(agent, &array_like, &key(k as u64))?;
        let mapped = if mapping {
            crate::function::call(
                agent,
                &mapfn,
                this_arg.clone(),
                &[value, Value::Number(k as f64)],
            )?
        } else {
            value
        };
        set_property(&target, &key(k as u64), mapped)?;
    }
    Ok(target)
}

/// The `%TypedArray.prototype[@@species]%` getter (spec 25.2.3.38): `this`.
fn species_getter(_agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    Ok(this.clone())
}

/// The base64 alphabets of `toBase64`/`setFromBase64` (spec 25.2.3.44-45).
fn base64_alphabet(url: bool) -> &'static [u8] {
    if url {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
    } else {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
    }
}

/// Validate that `this` is a Uint8Array (spec 25.2.3.43-46).
fn validate_uint8(agent: &mut Agent, this: &Value) -> Result<Handle<TypedArraySlots>, JsError> {
    let slots = validate_typed_array(agent, this)?;
    if slots.element_type != ElementType::Uint8 {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Method is only defined on Uint8Array".into(),
        ));
    }
    Ok(slots)
}

/// The internal-slot and element-type check without the detachment test:
/// toBase64/setFromBase64 must run their option getters before the spec's
/// later detached-buffer throw (toBase64/detached-buffer.js asserts the
/// getter runs exactly once even on a pre-detached receiver).
fn validate_uint8_slot(value: &Value) -> Result<Handle<TypedArraySlots>, JsError> {
    let slots = typed_array_slots_required(value)?;
    if slots.element_type != ElementType::Uint8 {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Method is only defined on Uint8Array".into(),
        ));
    }
    Ok(slots)
}

/// spec 25.2.3.44 Uint8Array.prototype.toHex.
fn to_hex(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_uint8(agent, this)?;
    let start = slots.byte_offset;
    let bytes = slots.buffer.read(start, slots.byte_length)?;
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in &bytes {
        out.push_str(&format!("{:02x}", byte));
    }
    Ok(Value::String(Handle::new(JsString::from_utf8(&out))))
}

/// spec 25.2.3.45 Uint8Array.prototype.toBase64.
fn to_base64(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_uint8_slot(this)?;
    let options = get_options_object(agent, args.first())?;
    let alphabet = base64_alphabet_option(agent, &options)?;
    let padding = get(agent, &options, &JsString::from_utf8("omitPadding"))?;
    let omit_padding = to_boolean(&padding);
    // spec step 5: the detached check follows the option reads (a getter can
    // detach the buffer; a pre-detached buffer still runs the getters).
    if slots.buffer.is_detached() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray buffer is detached".into(),
        ));
    }
    let start = slots.byte_offset;
    let bytes = slots.buffer.read(start, slots.byte_length)?;
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        out.push(alphabet[(b0 >> 2) as usize] as char);
        out.push(alphabet[(((b0 & 0x3) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(alphabet[(((b1 & 0xF) << 2) | (b2 >> 6)) as usize] as char);
        } else if !omit_padding {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(alphabet[(b2 & 0x3F) as usize] as char);
        } else if !omit_padding {
            out.push('=');
        }
    }
    Ok(Value::String(Handle::new(JsString::from_utf8(&out))))
}

fn hex_digit(unit: u16) -> Option<u8> {
    match unit {
        d if (b'0' as u16..=b'9' as u16).contains(&d) => Some((d - b'0' as u16) as u8),
        d if (b'a' as u16..=b'f' as u16).contains(&d) => Some((d - b'a' as u16 + 10) as u8),
        d if (b'A' as u16..=b'F' as u16).contains(&d) => Some((d - b'A' as u16 + 10) as u8),
        _ => None,
    }
}

fn base64_digit(alphabet: &[u8], unit: u16) -> Option<u8> {
    alphabet
        .iter()
        .position(|&c| c as u16 == unit)
        .map(|i| i as u8)
}

/// A {written, read} result object (spec 25.2.3.46-47).
fn written_read_result(agent: &Agent, written: usize, read: usize) -> Result<Value, JsError> {
    let proto = agent
        .current_realm()?
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let object = JsObject::ordinary_object_create(proto);
    object.create_data_property(
        &JsString::from_utf8("written"),
        Value::Number(written as f64),
    )?;
    object.create_data_property(&JsString::from_utf8("read"), Value::Number(read as f64))?;
    Ok(Value::Object(object))
}

/// A decode result mirroring the FromHex/FromBase64 Record (spec 25.2.4.6,
/// 25.2.4.9): the decoded bytes, the source units read, and the SyntaxError
/// to throw after the bytes were written (so partial output survives bad
/// input).
struct DecodeResult {
    bytes: Vec<u8>,
    read: usize,
    error: Option<JsError>,
}

fn syntax_error(message: &str) -> JsError {
    JsError::new(ErrorKind::SyntaxError, message.into())
}

/// GetOptionsObject (spec 25.2.3.45 step 2): undefined/null → a fresh
/// empty object, otherwise ToObject.
fn get_options_object(agent: &mut Agent, arg: Option<&Value>) -> Result<Value, JsError> {
    match arg {
        None | Some(Value::Undefined) | Some(Value::Null) => {
            Ok(Value::Object(JsObject::ordinary_object_create(None)))
        }
        Some(value) => crate::context::to_object(agent, value),
    }
}

/// The `alphabet` option (spec 25.2.3.45 steps 3-6): undefined → "base64";
/// anything but a String equal to "base64"/"base64url" is a TypeError.
fn base64_alphabet_option(agent: &mut Agent, options: &Value) -> Result<&'static [u8], JsError> {
    let alphabet = get(agent, options, &JsString::from_utf8("alphabet"))?;
    match alphabet {
        Value::Undefined => Ok(base64_alphabet(false)),
        Value::String(text) => match text.to_string_lossy().as_str() {
            "base64" => Ok(base64_alphabet(false)),
            "base64url" => Ok(base64_alphabet(true)),
            _ => Err(JsError::new(
                ErrorKind::TypeError,
                "alphabet must be 'base64' or 'base64url'".into(),
            )),
        },
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "alphabet must be a String".into(),
        )),
    }
}

/// The `lastChunkHandling` option (spec 25.2.3.45 steps 7-10): undefined →
/// "loose"; anything else must be one of the three values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastChunkHandling {
    Loose,
    Strict,
    StopBeforePartial,
}

fn last_chunk_handling_option(
    agent: &mut Agent,
    options: &Value,
) -> Result<LastChunkHandling, JsError> {
    let handling = get(agent, options, &JsString::from_utf8("lastChunkHandling"))?;
    match handling {
        Value::Undefined => Ok(LastChunkHandling::Loose),
        Value::String(text) => match text.to_string_lossy().as_str() {
            "loose" => Ok(LastChunkHandling::Loose),
            "strict" => Ok(LastChunkHandling::Strict),
            "stop-before-partial" => Ok(LastChunkHandling::StopBeforePartial),
            _ => Err(JsError::new(
                ErrorKind::TypeError,
                "lastChunkHandling must be 'loose', 'strict', or 'stop-before-partial'".into(),
            )),
        },
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "lastChunkHandling must be a String".into(),
        )),
    }
}

/// The string argument of the hex/base64 methods: a raw String check that
/// throws a TypeError instead of coercing (spec 25.2.4.4-5, 25.2.4.7-8).
fn string_arg(args: &[Value]) -> Result<Handle<JsString>, JsError> {
    match args.first() {
        Some(Value::String(string)) => Ok(string.clone()),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "The string argument must be a String".into(),
        )),
    }
}

/// SkipAsciiWhitespace (spec 25.2.4.6): TAB/LF/FF/CR/SPACE only.
fn skip_ascii_whitespace(units: &[u16], mut index: usize) -> usize {
    while index < units.len() && matches!(units[index], 0x0009 | 0x000A | 0x000C | 0x000D | 0x0020)
    {
        index += 1;
    }
    index
}

/// DecodeFullLengthBase64Chunk (spec 25.2.4.6): 4 digits → 3 bytes.
fn decode_full_chunk(digits: &[u8; 4]) -> [u8; 3] {
    [
        (digits[0] << 2) | (digits[1] >> 4),
        (digits[1] << 4) | (digits[2] >> 2),
        (digits[2] << 6) | digits[3],
    ]
}

/// DecodeFinalBase64Chunk (spec 25.2.4.6): a 2- or 3-digit final chunk,
/// padded with zero digits; `throwOnExtraBits` rejects non-zero padding
/// bits (the strict mode of the padding path).
fn decode_final_chunk(
    digits: &[u8],
    throw_on_extra_bits: bool,
    bytes: &mut Vec<u8>,
) -> Result<(), JsError> {
    let mut padded = [0u8; 4];
    padded[..digits.len()].copy_from_slice(digits);
    let decoded = decode_full_chunk(&padded);
    if digits.len() == 2 {
        if throw_on_extra_bits && decoded[1] != 0 {
            return Err(syntax_error("Non-zero base64 padding bits"));
        }
        bytes.push(decoded[0]);
    } else {
        if throw_on_extra_bits && decoded[2] != 0 {
            return Err(syntax_error("Non-zero base64 padding bits"));
        }
        bytes.push(decoded[0]);
        bytes.push(decoded[1]);
    }
    Ok(())
}

/// FromHex (spec 25.2.4.9): decode pairs up to `max_length` bytes; an odd
/// length errors before any write, an invalid digit after the preceding
/// pairs.
fn decode_hex(source: &[u16], max_length: usize) -> DecodeResult {
    if !source.len().is_multiple_of(2) {
        return DecodeResult {
            bytes: Vec::new(),
            read: 0,
            error: Some(syntax_error("Hex string length must be even")),
        };
    }
    let mut read = 0usize;
    let mut bytes = Vec::new();
    while read < source.len() && bytes.len() < max_length {
        let (Some(hi), Some(lo)) = (hex_digit(source[read]), hex_digit(source[read + 1])) else {
            return DecodeResult {
                bytes,
                read,
                error: Some(syntax_error("Invalid hex digit")),
            };
        };
        bytes.push((hi << 4) | lo);
        read += 2;
    }
    DecodeResult {
        bytes,
        read,
        error: None,
    }
}

/// FromBase64 (spec 25.2.4.6): decode 4-digit chunks (whitespace skipped,
/// `=` padding validated) up to `max_length` bytes. An invalid character
/// errors with the bytes decoded so far; a full target stops the scan so
/// trailing garbage is ignored.
fn decode_base64(
    source: &[u16],
    alphabet: &[u8],
    last_chunk: LastChunkHandling,
    max_length: usize,
) -> DecodeResult {
    if max_length == 0 {
        return DecodeResult {
            bytes: Vec::new(),
            read: 0,
            error: None,
        };
    }
    let length = source.len();
    let mut read = 0usize;
    let mut bytes = Vec::new();
    let mut digits: [u8; 4] = [0; 4];
    let mut chunk_length = 0usize;
    let mut index = 0usize;
    loop {
        index = skip_ascii_whitespace(source, index);
        if index == length {
            // End of input: finish a trailing chunk per lastChunkHandling.
            if chunk_length > 0 {
                match last_chunk {
                    LastChunkHandling::StopBeforePartial => {
                        return DecodeResult {
                            bytes,
                            read,
                            error: None,
                        };
                    }
                    LastChunkHandling::Strict => {
                        return DecodeResult {
                            bytes,
                            read,
                            error: Some(syntax_error("Final base64 chunk is incomplete")),
                        };
                    }
                    LastChunkHandling::Loose => {
                        if chunk_length == 1 {
                            return DecodeResult {
                                bytes,
                                read,
                                error: Some(syntax_error(
                                    "A single base64 character cannot form a chunk",
                                )),
                            };
                        }
                        if let Err(error) =
                            decode_final_chunk(&digits[..chunk_length], false, &mut bytes)
                        {
                            return DecodeResult {
                                bytes,
                                read,
                                error: Some(error),
                            };
                        }
                    }
                }
            }
            return DecodeResult {
                bytes,
                read: length,
                error: None,
            };
        }
        let unit = source[index];
        // Set index to index + 1 before handling the unit (spec FromBase64
        // step 10.d: the char was already consumed).
        index += 1;
        if unit == b'=' as u16 {
            if chunk_length < 2 {
                return DecodeResult {
                    bytes,
                    read,
                    error: Some(syntax_error("Unexpected base64 padding")),
                };
            }
            index = skip_ascii_whitespace(source, index);
            if chunk_length == 2 {
                if index == length {
                    if last_chunk == LastChunkHandling::StopBeforePartial {
                        return DecodeResult {
                            bytes,
                            read,
                            error: None,
                        };
                    }
                    return DecodeResult {
                        bytes,
                        read,
                        error: Some(syntax_error("Invalid base64 padding")),
                    };
                }
                if source[index] == b'=' as u16 {
                    index = skip_ascii_whitespace(source, index + 1);
                }
            }
            if index < length {
                return DecodeResult {
                    bytes,
                    read,
                    error: Some(syntax_error("Invalid base64 padding")),
                };
            }
            let throw_on_extra = last_chunk == LastChunkHandling::Strict;
            if let Err(error) =
                decode_final_chunk(&digits[..chunk_length], throw_on_extra, &mut bytes)
            {
                return DecodeResult {
                    bytes,
                    read,
                    error: Some(error),
                };
            }
            return DecodeResult {
                bytes,
                read: length,
                error: None,
            };
        }
        let Some(digit) = base64_digit(alphabet, unit) else {
            return DecodeResult {
                bytes,
                read,
                error: Some(syntax_error("Invalid base64 character")),
            };
        };
        // Stop before a chunk that would overflow the target (spec
        // FromBase64 step 10.h): read stays at the last full chunk.
        let remaining = max_length - bytes.len();
        if (remaining == 1 && chunk_length == 2) || (remaining == 2 && chunk_length == 3) {
            return DecodeResult {
                bytes,
                read,
                error: None,
            };
        }
        digits[chunk_length] = digit;
        chunk_length += 1;
        if chunk_length == 4 {
            for byte in decode_full_chunk(&digits) {
                bytes.push(byte);
            }
            chunk_length = 0;
            read = index;
            if bytes.len() == max_length {
                return DecodeResult {
                    bytes,
                    read,
                    error: None,
                };
            }
        }
    }
}

/// SetUint8ArrayBytes (spec 25.2.4.2): copy the decoded bytes into the
/// view's buffer range. `bytes.len()` never exceeds the array length.
fn write_uint8_bytes(slots: &TypedArraySlots, bytes: &[u8]) -> Result<(), JsError> {
    // SetValueInBuffer semantics: a buffer detached (possibly by an options
    // getter) or made immutable before the write throws a TypeError.
    if slots.buffer.is_detached() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray buffer is detached".into(),
        ));
    }
    if slots.buffer.is_immutable() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray buffer is immutable".into(),
        ));
    }
    if bytes.len() > typed_array_effective_length(slots) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "Decoded bytes exceed the target length".into(),
        ));
    }
    slots.buffer.write(slots.byte_offset, bytes)?;
    Ok(())
}

/// spec 25.2.4.4 Uint8Array.fromHex: the result is created after decoding,
/// with exactly the decoded length.
fn from_hex(agent: &mut Agent, _this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let hex = string_arg(args)?;
    let result = decode_hex(hex.as_slice(), usize::MAX);
    if let Some(error) = result.error {
        return Err(error);
    }
    let proto = agent
        .current_realm()?
        .intrinsics
        .get("%Uint8Array.prototype%")
        .and_then(|value| as_object(&value))
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "%Uint8Array.prototype% missing".into(),
            )
        })?;
    let result_value =
        allocate_typed_array_buffer(agent, proto, ElementType::Uint8, result.bytes.len())?;
    let slots = validate_uint8(agent, &result_value)?;
    write_uint8_bytes(&slots, &result.bytes)?;
    Ok(result_value)
}

/// spec 25.2.4.5 Uint8Array.fromBase64.
fn from_base64(agent: &mut Agent, _this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let source = string_arg(args)?;
    let options = get_options_object(agent, args.get(1))?;
    let alphabet = base64_alphabet_option(agent, &options)?;
    let last_chunk = last_chunk_handling_option(agent, &options)?;
    let result = decode_base64(source.as_slice(), alphabet, last_chunk, usize::MAX);
    if let Some(error) = result.error {
        return Err(error);
    }
    let proto = agent
        .current_realm()?
        .intrinsics
        .get("%Uint8Array.prototype%")
        .and_then(|value| as_object(&value))
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "%Uint8Array.prototype% missing".into(),
            )
        })?;
    let result_value =
        allocate_typed_array_buffer(agent, proto, ElementType::Uint8, result.bytes.len())?;
    let slots = validate_uint8(agent, &result_value)?;
    write_uint8_bytes(&slots, &result.bytes)?;
    Ok(result_value)
}

/// SetUint8ArrayFromHex (spec 25.2.4.7): write the decoded pairs, then
/// throw if the input was invalid (writes survive the error).
fn set_from_hex(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_uint8(agent, this)?;
    // spec 25.2.4.6: a write-mode target backed by an immutable buffer throws
    // before any decoding (immutable-arraybuffer fixtures).
    if slots.buffer.is_immutable() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray buffer is immutable".into(),
        ));
    }
    let hex = string_arg(args)?;
    let result = decode_hex(hex.as_slice(), typed_array_effective_length(&slots));
    let written = result.bytes.len();
    let read = result.read;
    write_uint8_bytes(&slots, &result.bytes)?;
    if let Some(error) = result.error {
        return Err(error);
    }
    written_read_result(agent, written, read)
}

/// spec 25.2.4.8 SetUint8ArrayFromBase64: decode into the target (up to
/// its length), then throw if the input was invalid.
fn set_from_base64(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let slots = validate_uint8_slot(this)?;
    // spec 25.2.4.8: write-mode ValidateTypedArray rejects an immutable
    // backing buffer before any argument is read (the immutable fixture
    // asserts the option getters never run); the detached check comes later,
    // after the getters, per the spec's step ordering.
    if slots.buffer.is_immutable() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray buffer is immutable".into(),
        ));
    }
    let source = string_arg(args)?;
    let options = get_options_object(agent, args.get(1))?;
    let alphabet = base64_alphabet_option(agent, &options)?;
    let last_chunk = last_chunk_handling_option(agent, &options)?;
    let result = decode_base64(
        source.as_slice(),
        alphabet,
        last_chunk,
        typed_array_effective_length(&slots),
    );
    let written = result.bytes.len();
    let read = result.read;
    write_uint8_bytes(&slots, &result.bytes)?;
    if let Some(error) = result.error {
        return Err(error);
    }
    written_read_result(agent, written, read)
}

/// Install the TypedArray intrinsics and the twelve global constructors
/// (spec 25.2) during SetDefaultGlobalBindings.
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let typed_array_proto = JsObject::ordinary_object_create(object_proto.clone());
    let typed_array_proto_value = Value::Object(typed_array_proto.clone());

    let typed_array_ctor = Function::create_builtin(
        Some(JsString::from_utf8("TypedArray")),
        0,
        placeholder("TypedArray"),
        Some(Box::new(placeholder("TypedArray"))),
        None,
    )?;
    let typed_array_ctor_value = Value::Function(typed_array_ctor.clone());
    realm
        .intrinsics
        .define(TYPED_ARRAY, typed_array_ctor_value.clone());
    realm
        .intrinsics
        .define(TYPED_ARRAY_PROTO, typed_array_proto_value.clone());
    // spec 25.2.2: %TypedArray% is exposed as the `TypedArray` global.
    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("TypedArray"),
        &PropertyDescriptor {
            value: Some(typed_array_ctor_value.clone()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    typed_array_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(typed_array_proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    typed_array_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(typed_array_ctor_value.clone()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // The shared prototype methods, one intrinsic per method.
    let methods: [(&str, &str, u64); 30] = [
        ("at", AT, 1),
        ("copyWithin", COPY_WITHIN, 2),
        ("entries", ENTRIES, 0),
        ("every", EVERY, 1),
        ("fill", FILL, 1),
        ("filter", FILTER, 1),
        ("find", FIND, 1),
        ("findIndex", FIND_INDEX, 1),
        ("findLast", FIND_LAST, 1),
        ("findLastIndex", FIND_LAST_INDEX, 1),
        ("forEach", FOR_EACH, 1),
        ("includes", INCLUDES, 1),
        ("indexOf", INDEX_OF, 1),
        ("join", JOIN, 1),
        ("keys", KEYS, 0),
        ("lastIndexOf", LAST_INDEX_OF, 1),
        ("map", MAP, 1),
        ("reduce", REDUCE, 1),
        ("reduceRight", REDUCE_RIGHT, 1),
        ("reverse", REVERSE, 0),
        ("set", SET, 1),
        ("slice", SLICE, 2),
        ("some", SOME, 1),
        ("sort", SORT, 1),
        ("subarray", SUBARRAY, 2),
        ("toLocaleString", TO_LOCALE_STRING, 0),
        ("toReversed", TO_REVERSED, 0),
        ("toSorted", TO_SORTED, 1),
        ("values", VALUES, 0),
        ("with", WITH, 2),
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
        typed_array_proto.define_property(
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

    // spec 25.2.3.33: %TypedArray%.prototype.toString is the Array one.
    let array_to_string = realm
        .intrinsics
        .get("%Array.prototype.toString%")
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "%Array.prototype.toString% missing".into(),
            )
        })?;
    typed_array_proto.define_property(
        &JsString::from_utf8("toString"),
        &PropertyDescriptor {
            value: Some(array_to_string),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // @@iterator = %TypedArray.prototype.values%.
    let values_func = realm.intrinsics.get(VALUES).ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "%TypedArray.prototype.values% missing".into(),
        )
    })?;
    realm.intrinsics.define(ITERATOR, values_func.clone());
    typed_array_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("iterator").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(values_func),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // @@species getter (returns `this`).
    let species_func = Function::create_builtin(
        Some(JsString::from_utf8("get [Symbol.species]")),
        0,
        Box::new(|this, _| Ok(this.clone())),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(SPECIES, Value::Function(species_func.clone()));
    typed_array_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("species").as_ref().clone()),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: Some(Value::Function(species_func.clone())),
            set: Some(Value::Undefined),
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    // The same accessor on the %TypedArray% constructor (spec 25.2.3.31);
    // the kind constructors inherit it through [[Prototype]] = %TypedArray%.
    typed_array_ctor.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("species").as_ref().clone()),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: Some(Value::Function(species_func)),
            set: Some(Value::Undefined),
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // The accessors.
    let accessors: [(&str, &str); 4] = [
        ("length", GET_LENGTH),
        ("buffer", GET_BUFFER),
        ("byteLength", GET_BYTE_LENGTH),
        ("byteOffset", GET_BYTE_OFFSET),
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
        typed_array_proto.define_property(
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

    // @@toStringTag: a single accessor on %TypedArray%.prototype that reads
    // the instance's [[TypedArrayName]] (spec 25.2.3.32), not a data property
    // per concrete prototype.
    let tag_func = Function::create_builtin(
        Some(JsString::from_utf8("get [Symbol.toStringTag]")),
        0,
        placeholder("toStringTag"),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(GET_TO_STRING_TAG, Value::Function(tag_func.clone()));
    typed_array_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: Some(Value::Function(tag_func)),
            set: Some(Value::Undefined),
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // Statics: from/of.
    for (name, intrinsic, length) in [("from", FROM, 1), ("of", OF, 0)] {
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
        typed_array_ctor.define_property(
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

    // The twelve concrete kinds; each constructor's [[Prototype]] is
    // %TypedArray% (spec 25.2.1), so `Int8Array.from` etc. resolve.
    let typed_array_object = as_object(&typed_array_ctor_value).ok_or_else(|| {
        JsError::new(ErrorKind::TypeError, "%TypedArray% is not an object".into())
    })?;
    for kind in KINDS {
        let kind_proto = JsObject::ordinary_object_create(Some(typed_array_proto.clone()));
        let kind_proto_value = Value::Object(kind_proto.clone());
        let ctor = Function::create_builtin(
            Some(JsString::from_utf8(kind.tag)),
            3,
            placeholder(kind.tag),
            Some(Box::new(placeholder(kind.tag))),
            None,
        )?;
        let ctor_value = Value::Function(ctor.clone());
        // spec 25.2.1: the kind constructors inherit %TypedArray%.
        ctor.object
            .set_prototype_of(Some(typed_array_object.clone()))?;
        realm.intrinsics.define(kind.ctor, ctor_value.clone());
        realm
            .intrinsics
            .define(kind.proto, kind_proto_value.clone());
        // spec 25.2.1 table: BYTES_PER_ELEMENT is a non-writable,
        // non-enumerable, non-configurable data property of the constructor.
        ctor.define_property(
            &JsString::from_utf8("BYTES_PER_ELEMENT"),
            &PropertyDescriptor {
                value: Some(Value::Number(kind.element_type.size() as f64)),
                writable: Some(false),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(false),
            },
        )?;
        ctor.define_property(
            &JsString::from_utf8("prototype"),
            &PropertyDescriptor {
                value: Some(kind_proto_value.clone()),
                writable: Some(false),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(false),
            },
        )?;
        // spec 25.2.1 table: BYTES_PER_ELEMENT is also a non-writable,
        // non-enumerable, non-configurable data property of the prototype.
        kind_proto.define_property(
            &JsString::from_utf8("BYTES_PER_ELEMENT"),
            &PropertyDescriptor {
                value: Some(Value::Number(kind.element_type.size() as f64)),
                writable: Some(false),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(false),
            },
        )?;
        kind_proto.define_property(
            &JsString::from_utf8("constructor"),
            &PropertyDescriptor {
                value: Some(ctor_value.clone()),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
        // Species per kind prototype.
        let species_func = Function::create_builtin(
            Some(JsString::from_utf8("get [Symbol.species]")),
            0,
            Box::new(|this, _| Ok(this.clone())),
            None,
            None,
        )?;
        // CreateBuiltinFunction (spec 10.2.3): the [[Prototype]] is
        // %Function.prototype%. The realm post-pass only links intrinsics-table
        // functions; this getter lives on the kind prototype alone, so link it
        // here (spec 10.2.3 step 1).
        if let Some(function_proto) =
            realm
                .intrinsics
                .get("%Function.prototype%")
                .and_then(|value| match value {
                    Value::Function(function) => function.object.handle(),
                    _ => None,
                })
        {
            species_func.object.set_prototype_of(Some(function_proto))?;
        }
        kind_proto.define_property_key(
            &PropertyKey::Symbol(crux::symbol::well_known("species").as_ref().clone()),
            &PropertyDescriptor {
                value: None,
                writable: None,
                get: Some(Value::Function(species_func)),
                set: Some(Value::Undefined),
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
        realm.global_object.define_property_or_throw(
            &JsString::from_utf8(kind.tag),
            &PropertyDescriptor {
                value: Some(ctor_value),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
        // ES2026 statics on the Uint8Array constructor (spec 25.2.4.4-5).
        if kind.tag == "Uint8Array" {
            for (name, intrinsic, length) in
                [("fromHex", FROM_HEX, 1), ("fromBase64", FROM_BASE64, 1)]
            {
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
                ctor.define_property(
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
        }
    }

    // The Uint8Array hex/base64 methods (ES2026, spec 25.2.3.44-47).
    let uint8_proto = realm
        .intrinsics
        .get("%Uint8Array.prototype%")
        .and_then(|value| as_object(&value))
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "%Uint8Array.prototype% missing".into(),
            )
        })?;
    for (name, intrinsic, length) in [
        ("toHex", TO_HEX, 0),
        ("toBase64", TO_BASE64, 0),
        ("setFromHex", SET_FROM_HEX, 1),
        ("setFromBase64", SET_FROM_BASE64, 1),
    ] {
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
        uint8_proto.define_property(
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

    Ok(())
}

/// The TypedArray members that need the agent, dispatched by intrinsic
/// identity from `runtime::function::call`/`construct`.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(TYPED_ARRAY).as_ref() == Some(callee) {
        return Some(Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray constructor cannot be called without 'new'".into(),
        )));
    }
    if intrinsics.get(FROM).as_ref() == Some(callee) {
        return Some(from(agent, this, args));
    }
    if intrinsics.get(OF).as_ref() == Some(callee) {
        return Some(of(agent, this, args));
    }
    if intrinsics.get(AT).as_ref() == Some(callee) {
        return Some(at(agent, this, args));
    }
    if intrinsics.get(COPY_WITHIN).as_ref() == Some(callee) {
        return Some(copy_within(agent, this, args));
    }
    if intrinsics.get(ENTRIES).as_ref() == Some(callee) {
        return Some(entries(agent, this, args));
    }
    if intrinsics.get(EVERY).as_ref() == Some(callee) {
        return Some(every(agent, this, args));
    }
    if intrinsics.get(FILL).as_ref() == Some(callee) {
        return Some(fill(agent, this, args));
    }
    if intrinsics.get(FILTER).as_ref() == Some(callee) {
        return Some(filter(agent, this, args));
    }
    if intrinsics.get(FIND).as_ref() == Some(callee) {
        return Some(find(agent, this, args));
    }
    if intrinsics.get(FIND_INDEX).as_ref() == Some(callee) {
        return Some(find_index(agent, this, args));
    }
    if intrinsics.get(FIND_LAST).as_ref() == Some(callee) {
        return Some(find_last(agent, this, args));
    }
    if intrinsics.get(FIND_LAST_INDEX).as_ref() == Some(callee) {
        return Some(find_last_index(agent, this, args));
    }
    if intrinsics.get(FOR_EACH).as_ref() == Some(callee) {
        return Some(for_each(agent, this, args));
    }
    if intrinsics.get(INCLUDES).as_ref() == Some(callee) {
        return Some(includes(agent, this, args));
    }
    if intrinsics.get(INDEX_OF).as_ref() == Some(callee) {
        return Some(index_of(agent, this, args));
    }
    if intrinsics.get(JOIN).as_ref() == Some(callee) {
        return Some(join(agent, this, args));
    }
    if intrinsics.get(KEYS).as_ref() == Some(callee) {
        return Some(keys(agent, this, args));
    }
    if intrinsics.get(LAST_INDEX_OF).as_ref() == Some(callee) {
        return Some(last_index_of(agent, this, args));
    }
    if intrinsics.get(MAP).as_ref() == Some(callee) {
        return Some(map(agent, this, args));
    }
    if intrinsics.get(REDUCE).as_ref() == Some(callee) {
        return Some(reduce(agent, this, args));
    }
    if intrinsics.get(REDUCE_RIGHT).as_ref() == Some(callee) {
        return Some(reduce_right(agent, this, args));
    }
    if intrinsics.get(REVERSE).as_ref() == Some(callee) {
        return Some(reverse(agent, this, args));
    }
    if intrinsics.get(SET).as_ref() == Some(callee) {
        return Some(set(agent, this, args));
    }
    if intrinsics.get(SLICE).as_ref() == Some(callee) {
        return Some(slice(agent, this, args));
    }
    if intrinsics.get(SOME).as_ref() == Some(callee) {
        return Some(some(agent, this, args));
    }
    if intrinsics.get(SORT).as_ref() == Some(callee) {
        return Some(sort(agent, this, args));
    }
    if intrinsics.get(SUBARRAY).as_ref() == Some(callee) {
        return Some(subarray(agent, this, args));
    }
    if intrinsics.get(TO_LOCALE_STRING).as_ref() == Some(callee) {
        return Some(to_locale_string(agent, this, args));
    }
    if intrinsics.get(TO_REVERSED).as_ref() == Some(callee) {
        return Some(to_reversed(agent, this, args));
    }
    if intrinsics.get(TO_SORTED).as_ref() == Some(callee) {
        return Some(to_sorted(agent, this, args));
    }
    if intrinsics.get(VALUES).as_ref() == Some(callee) {
        return Some(values(agent, this, args));
    }
    if intrinsics.get(WITH).as_ref() == Some(callee) {
        return Some(with(agent, this, args));
    }
    if intrinsics.get(ITERATOR).as_ref() == Some(callee) {
        return Some(iterator(agent, this, args));
    }
    if intrinsics.get(SPECIES).as_ref() == Some(callee) {
        return Some(species_getter(agent, this, args));
    }
    if intrinsics.get(GET_LENGTH).as_ref() == Some(callee) {
        return Some(get_length(agent, this, args));
    }
    if intrinsics.get(GET_BUFFER).as_ref() == Some(callee) {
        return Some(get_buffer(agent, this, args));
    }
    if intrinsics.get(GET_BYTE_LENGTH).as_ref() == Some(callee) {
        return Some(get_byte_length(agent, this, args));
    }
    if intrinsics.get(GET_BYTE_OFFSET).as_ref() == Some(callee) {
        return Some(get_byte_offset(agent, this, args));
    }
    if intrinsics.get(GET_TO_STRING_TAG).as_ref() == Some(callee) {
        return Some(get_to_string_tag(agent, this, args));
    }
    if intrinsics.get(FROM_HEX).as_ref() == Some(callee) {
        return Some(from_hex(agent, this, args));
    }
    if intrinsics.get(FROM_BASE64).as_ref() == Some(callee) {
        return Some(from_base64(agent, this, args));
    }
    if intrinsics.get(TO_HEX).as_ref() == Some(callee) {
        return Some(to_hex(agent, this, args));
    }
    if intrinsics.get(TO_BASE64).as_ref() == Some(callee) {
        return Some(to_base64(agent, this, args));
    }
    if intrinsics.get(SET_FROM_HEX).as_ref() == Some(callee) {
        return Some(set_from_hex(agent, this, args));
    }
    if intrinsics.get(SET_FROM_BASE64).as_ref() == Some(callee) {
        return Some(set_from_base64(agent, this, args));
    }
    None
}

/// The twelve concrete TypedArray constructors' [[Construct]] (spec 25.2.2.1).
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(TYPED_ARRAY).as_ref() == Some(callee) {
        return Some(Err(JsError::new(
            ErrorKind::TypeError,
            "%TypedArray% is not a constructor".into(),
        )));
    }
    for kind in KINDS {
        if intrinsics.get(kind.ctor).as_ref() == Some(callee) {
            return Some(typed_array_construct(
                agent,
                args,
                new_target,
                kind.element_type,
            ));
        }
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

    fn text(source: &str) -> String {
        match run(source).unwrap() {
            Value::String(s) => s.to_string_lossy(),
            other => panic!("expected a string, got {other:?}"),
        }
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
    fn constructors_and_element_types() {
        assert_eq!(number("new Int8Array(4).length"), 4.0);
        assert_eq!(number("new Int16Array(4).byteLength"), 8.0);
        assert_eq!(number("new Float32Array(4).byteLength"), 16.0);
        assert_eq!(number("new Float16Array(2).byteLength"), 4.0);
        assert_eq!(number("new BigInt64Array(2).byteLength"), 16.0);
        assert_eq!(
            text("new Int8Array([128, -1, 255]).join(',')"),
            "-128,-1,-1"
        );
        assert_eq!(text("new Uint16Array([65535, 1]).join(',')"), "65535,1");
        assert_eq!(
            text("new Uint8ClampedArray([-1, 300, 127.5]).join(',')"),
            "0,255,128"
        );
        assert_eq!(text("new Float32Array([1.5]).join(',')"), "1.5");
        assert_eq!(text("new Float64Array([1.5]).join(',')"), "1.5");
        assert_eq!(text("new BigInt64Array([1n, 2n]).join(',')"), "1,2");
        assert_eq!(
            text("new BigUint64Array([1n, 18446744073709551615n]).join(',')"),
            "1,18446744073709551615"
        );
        // The call form throws; multiple args are the element list.
        assert!(run("Int8Array(3)").is_err());
        assert_eq!(text("new Uint8Array(1, 2, 3).join(',')"), "1,2,3");
        // Array-like without an iterator.
        assert_eq!(
            text("new Uint8Array({length: 2, 0: 7, 1: 8}).join(',')"),
            "7,8"
        );
        // A BigInt cannot coerce to a Number element; an integral Number does
        // coerce to a BigInt element, while a fractional one throws a
        // ToBigInt rejects Numbers (spec 7.1.17), in the constructor's
        // element lists too (ctors-bigint/object-arg/number-tobigint); a
        // BigInt cannot coerce to a Number element either.
        assert!(run("new Int8Array([1n])").is_err());
        assert!(run("new BigInt64Array([1])").is_err());
        assert_eq!(text("new BigInt64Array([1n, 2n]).join(',')"), "1,2");
        assert!(run("new BigInt64Array([1.5])").is_err());
    }

    #[test]
    fn buffer_views_and_subarray() {
        assert_eq!(
            text("new Int16Array(new Uint8Array([1, 2, 3]).buffer, 0, 1).join(',')"),
            "513"
        );
        assert!(run("new Uint16Array(new Uint8Array(4).buffer, 1, 1)").is_err());
        assert_eq!(
            text(
                "(function(){ var a = new Uint8Array([1, 2, 3, 4]); var s = a.subarray(1, 3); return s.join(',') + ':' + s.length + ':' + s.byteOffset; })()"
            ),
            "2,3:2:1"
        );
        // subarray aliases the buffer.
        assert_eq!(
            text(
                "(function(){ var a = new Uint8Array([1, 2, 3]); var s = a.subarray(1); s[0] = 9; return a.join(','); })()"
            ),
            "1,9,3"
        );
        // Copying between views of the same buffer.
        assert_eq!(
            text(
                "(function(){ var a = new Uint8Array([1, 2, 3, 4]); var b = a.subarray(1, 3); b.set(a.subarray(0, 2)); return a.join(','); })()"
            ),
            "1,1,2,4"
        );
    }

    #[test]
    fn prototype_methods() {
        assert_eq!(text("new Uint8Array([3, 1, 2]).sort().join(',')"), "1,2,3");
        assert_eq!(
            text("new Int8Array([3, -2, 1]).sort((a, b) => a - b).join(',')"),
            "-2,1,3"
        );
        assert_eq!(
            text("new Uint8Array([1, 2, 3]).map(x => x * 2).join(',')"),
            "2,4,6"
        );
        assert_eq!(
            text("new Uint8Array([1, 2, 3]).filter(x => x > 1).join(',')"),
            "2,3"
        );
        assert_eq!(
            number("new Uint8Array([1, 2, 3, 4]).reduce((a, b) => a + b)"),
            10.0
        );
        assert_eq!(
            text("new Uint8Array([1, 2, 3]).reverse().join(',')"),
            "3,2,1"
        );
        assert_eq!(
            text("new Uint8Array([1, 2, 3, 4]).slice(1, 3).join(',')"),
            "2,3"
        );
        assert_eq!(
            text("new Uint8Array([1, 2, 3]).fill(9, 1).join(',')"),
            "1,9,9"
        );
        assert_eq!(
            text("new Uint8Array([1, 2, 3, 4]).copyWithin(0, 2).join(',')"),
            "3,4,3,4"
        );
        assert_eq!(number("new Uint8Array([1, 2, 3]).indexOf(3)"), 2.0);
        assert!(bool("new Uint8Array([1, 2, 3]).includes(2)"));
        assert_eq!(number("new Uint8Array([1, 2, 3]).at(-1)"), 3.0);
        assert_eq!(number("new Uint8Array([1, 2, 3]).find(x => x > 1)"), 2.0);
        assert_eq!(
            number("new Uint8Array([1, 2, 3]).findIndex(x => x > 1)"),
            1.0
        );
        assert_eq!(
            number("new Uint8Array([1, 2, 3]).findLast(x => x < 3)"),
            2.0
        );
        assert_eq!(
            number("new Uint8Array([1, 2, 3]).findLastIndex(x => x > 1)"),
            2.0
        );
        assert!(bool("new Uint8Array([1, 2, 3]).every(x => x > 0)"));
        assert!(bool("new Uint8Array([1, 2, 3]).some(x => x > 2)"));
        assert_eq!(text("new Uint8Array([1, 2, 3]).join('-')"), "1-2-3");
        assert_eq!(
            text("new Uint8Array([1, 2, 3]).toReversed().join(',')"),
            "3,2,1"
        );
        assert_eq!(
            text("new Uint8Array([3, 1, 2]).toSorted().join(',')"),
            "1,2,3"
        );
        assert_eq!(
            text("new Uint8Array([1, 2, 3]).with(1, 9).join(',')"),
            "1,9,3"
        );
        assert_eq!(
            text("new Uint8Array([1, 2, 3]).entries().next().value.join(':')"),
            "0:1"
        );
        assert_eq!(number("new Uint8Array([1, 2, 3]).keys().next().value"), 0.0);
        assert_eq!(
            number("new Uint8Array([1, 2, 3]).values().next().value"),
            1.0
        );
        // Numeric default sort (NaN last).
        assert_eq!(
            text("new Float64Array([NaN, 2, 1]).sort().join(',')"),
            "1,2,NaN"
        );
    }

    #[test]
    fn from_of_and_species() {
        assert_eq!(text("Int8Array.from([1, 2, 3]).join(',')"), "1,2,3");
        assert_eq!(text("Uint8Array.of(1, 2, 3).join(',')"), "1,2,3");
        assert_eq!(
            text("Uint8Array.from({length: 2}, (_, i) => i * 10).join(',')"),
            "0,10"
        );
        // map keeps the kind via the species.
        assert_eq!(
            text("new Uint8Array([1, 2]).map(x => x).constructor === Uint8Array ? 'y' : 'n'"),
            "y"
        );
        assert_eq!(
            text("new Int16Array(2).map(x => x).constructor === Int16Array ? 'y' : 'n'"),
            "y"
        );
    }

    #[test]
    fn accessors_and_tags() {
        assert_eq!(number("new Uint8Array([1, 2, 3]).byteOffset"), 0.0);
        assert!(bool("new Uint8Array(3).buffer instanceof Object"));
        assert_eq!(
            text("Object.prototype.toString.call(new Int16Array(2))"),
            "[object Int16Array]"
        );
        assert_eq!(
            text("Object.prototype.toString.call(new BigUint64Array(2))"),
            "[object BigUint64Array]"
        );
        assert!(bool("new Uint8Array(2) instanceof Uint8Array"));
        assert!(!bool("new Uint8Array(2) instanceof Int8Array"));
        assert!(bool(
            "Uint8Array.prototype.isPrototypeOf(new Uint8Array(2))"
        ));
        // The kind constructors inherit %TypedArray%.
        assert_eq!(
            text(
                "Object.getPrototypeOf(Uint8Array) === Object.getPrototypeOf(Int8Array) ? 'y' : 'n'"
            ),
            "y"
        );
    }

    #[test]
    fn uint8_hex_and_base64() {
        assert_eq!(text("new Uint8Array([0, 255, 16]).toHex()"), "00ff10");
        assert_eq!(text("new Uint8Array([104, 105]).toHex()"), "6869");
        assert_eq!(text("new Uint8Array([97, 98, 99]).toBase64()"), "YWJj");
        assert_eq!(
            text("new Uint8Array([251, 255]).toBase64({alphabet: 'base64url'})"),
            "-_8="
        );
        assert_eq!(
            text("new Uint8Array([97]).toBase64({omitPadding: true})"),
            "YQ"
        );
        assert_eq!(
            text(
                "(function(){ var u = new Uint8Array(3); var r = u.setFromHex('6869'); return r.written + ':' + r.read + ':' + u.join(','); })()"
            ),
            "2:4:104,105,0"
        );
        // An invalid digit throws after the preceding pairs were written
        // (spec 25.2.4.7; test262 writes-up-to-error.js).
        assert!(run("new Uint8Array(3).setFromHex('68zz69')").is_err());
        assert_eq!(
            text(
                "(function(){ var u = new Uint8Array(3); try { u.setFromHex('68zz69'); } catch (e) {} return u.join(','); })()"
            ),
            "104,0,0"
        );
        assert!(run("new Uint8Array(3).setFromHex('683')").is_err());
        assert_eq!(
            text(
                "(function(){ var u = new Uint8Array(3); var r = u.setFromBase64('YWJj'); return r.written + ':' + r.read + ':' + u.join(','); })()"
            ),
            "3:4:97,98,99"
        );
        assert_eq!(
            text(
                "(function(){ var u = new Uint8Array(3); var r = u.setFromBase64('YWI='); return r.written + ':' + u.join(','); })()"
            ),
            "2:97,98,0"
        );
        assert_eq!(
            text(
                "(function(){ var u = new Uint8Array(2); var r = u.setFromBase64('YQ', {lastChunkHandling: 'loose'}); return r.written + ':' + r.read; })()"
            ),
            "1:2"
        );
        assert!(
            run("new Uint8Array(4).setFromBase64('YWJjZA', {lastChunkHandling: 'strict'})")
                .is_err()
        );
        // fromBase64 returns exactly the decoded bytes (spec 25.2.4.5).
        assert_eq!(number("Uint8Array.fromBase64('YQ==').length"), 1.0);
        assert_eq!(text("Uint8Array.fromBase64('YQ==').join(',')"), "97");
        assert_eq!(
            text("Uint8Array.fromBase64('Zm9vYmFy').join(',')"),
            "102,111,111,98,97,114"
        );
        assert_eq!(text("Uint8Array.fromHex('6869').join(',')"), "104,105");
        // A chunk that does not fit the target stops without consuming it
        // (spec 25.2.4.8; test262 target-size.js).
        assert_eq!(
            text(
                "(function(){ var u = new Uint8Array([255, 255, 255, 255, 255]); var r = u.setFromBase64('Zm9vYmFy'); return r.read + ':' + r.written + ':' + u.join(','); })()"
            ),
            "4:3:102,111,111,255,255"
        );
        // Only Uint8Array has the hex/base64 methods.
        assert!(run("new Uint16Array(2).toHex()").is_err());
    }

    #[test]
    fn bounds_detach_and_aliasing() {
        // Out-of-bounds element writes are silently ignored.
        assert_eq!(
            text("(function(){ var a = new Uint8Array(3); a[3] = 5; return String(a[3]); })()"),
            "undefined"
        );
        assert_eq!(
            number("(function(){ var a = new Uint8Array(3); a[3] = 5; return a.length; })()"),
            3.0
        );
        // Constructor buffer bounds.
        assert!(run("new Uint8Array(new ArrayBuffer(4), 3, 2)").is_err());
        assert!(run("new Uint8Array(new ArrayBuffer(4), -1)").is_err());
        assert!(run("new Int32Array(new ArrayBuffer(8), 2)").is_err());
        // set out of range throws.
        assert!(run("new Uint8Array(3).set(new Uint8Array([1, 2, 3]), 2)").is_err());
        // Empty constructors.
        assert_eq!(number("new Uint8Array().length"), 0.0);
        assert_eq!(number("new Uint8Array(0).length"), 0.0);
        // Two views over one buffer alias each other.
        assert_eq!(
            number(
                "(function(){ var b = new ArrayBuffer(4); var a = new Uint8Array(b); var c = new Uint8Array(b); a[0] = 42; return c[0]; })()"
            ),
            42.0
        );
        // subarray on a multi-byte kind shares the buffer with a byte offset.
        assert_eq!(number("new Int32Array([1, 2, 3]).subarray(1).length"), 2.0);
        assert_eq!(
            number("new Int32Array([1, 2, 3]).subarray(1).byteOffset"),
            4.0
        );
        assert!(bool(
            "(function(){ var a = new Int32Array([1, 2, 3]); return a.subarray(1).buffer === a.buffer; })()"
        ));
        // Element conversion wraps modulo and truncates.
        assert_eq!(text("new Uint8Array([256, -1, 1.5]).join(',')"), "0,255,1");
        assert_eq!(
            text("new Uint8Array(new Uint16Array([1, 2, 3])).join(',')"),
            "1,2,3"
        );
        assert_eq!(
            number("new Float32Array([0.1, 0.2])[0]"),
            0.10000000149011612
        );
        // fill with a negative start.
        assert_eq!(
            text("new Uint8Array([1, 2, 3, 4]).fill(5, -2).join(',')"),
            "1,2,5,5"
        );
        // TypedArray sort is numeric, not lexicographic.
        assert_eq!(
            text("new Uint8Array([10, 1, 3]).sort().join(',')"),
            "1,3,10"
        );
        // transfer detaches the old buffer: methods on the old view throw.
        assert_eq!(
            number(
                "(function(){ var buf = new ArrayBuffer(4); var t = new Uint8Array(buf); var nb = buf.transfer(); return nb.byteLength; })()"
            ),
            4.0
        );
        assert!(run(
            "(function(){ var buf = new ArrayBuffer(4); var t = new Uint8Array(buf); buf.transfer(); return t.join(','); })()"
        )
        .is_err());
        assert_eq!(
            text(
                "(function(){ var buf = new ArrayBuffer(4); var t = new Uint8Array(buf); buf.transfer(); return String(t.length); })()"
            ),
            "0"
        );
    }

    #[test]
    fn backing_buffer_is_a_real_array_buffer() {
        // The buffer of a self-allocated TypedArray must carry
        // %ArrayBuffer.prototype% (spec 25.2.2.4).
        assert_eq!(number("new Uint8Array(3).buffer.byteLength"), 3.0);
        assert!(bool("new Uint8Array(3).buffer instanceof ArrayBuffer"));
        assert!(bool("new Uint8Array(3).buffer.constructor === ArrayBuffer"));
    }

    #[test]
    fn element_access_on_detached_views_is_a_noop() {
        // spec 10.4.7 (align-detached-buffer-semantics-with-web-reality): the
        // integer-indexed [[Get]]/[[Set]] on a detached view read *undefined*
        // and ignore writes (no TypeError) for any canonical index.
        assert_eq!(
            text(
                "(function(){ var buf = new ArrayBuffer(4); var t = new Uint8Array(buf); t[0] = 7; buf.transfer(); return String(t[0]); })()"
            ),
            "undefined"
        );
        assert_eq!(
            text(
                "(function(){ var buf = new ArrayBuffer(4); var t = new Uint8Array(buf); buf.transfer(); return String(t[99]); })()"
            ),
            "undefined"
        );
        assert_eq!(
            text(
                "(function(){ var buf = new ArrayBuffer(4); var t = new Uint8Array(buf); buf.transfer(); t[0] = 1; return String(t.length); })()"
            ),
            "0"
        );
    }
}
