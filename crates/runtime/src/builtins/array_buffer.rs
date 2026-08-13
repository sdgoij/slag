//! The ArrayBuffer and SharedArrayBuffer built-ins (spec 25.1, 25.3): the
//! backing byte blocks of the Integer-Indexed and DataView exotics. The
//! [[ArrayBufferData]] byte vector lives in a crux `SharedBuffer` aliased by
//! every view; the per-buffer bookkeeping (`[[ArrayBufferByteLength]]`,
//! `[[ArrayBufferMaxByteLength]]`, resizable/growable/shared flags,
//! detachment) is a `BufferState` in the agent's `buffer_data` table keyed by
//! object identity.

use crux::convert::{to_index, to_integer_or_infinity};
use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::typed_array::SharedBuffer;
use crux::value::{Value, is_callable, is_constructor};

use crate::agent::Agent;
use crate::context::{as_object, get_property, get_property_key};
use crate::realm::Realm;

const ARRAY_BUFFER: &str = "%ArrayBuffer%";
const ARRAY_BUFFER_PROTO: &str = "%ArrayBuffer.prototype%";
const AB_IS_VIEW: &str = "%ArrayBuffer.isView%";
const AB_SPECIES: &str = "%get ArrayBuffer[Symbol.species]%";
const AB_BYTE_LENGTH: &str = "%get ArrayBuffer.prototype.byteLength%";
const AB_DETACHED: &str = "%get ArrayBuffer.prototype.detached%";
const AB_IMMUTABLE: &str = "%get ArrayBuffer.prototype.immutable%";
const AB_MAX_BYTE_LENGTH: &str = "%get ArrayBuffer.prototype.maxByteLength%";
const AB_RESIZABLE: &str = "%get ArrayBuffer.prototype.resizable%";
const AB_RESIZE: &str = "%ArrayBuffer.prototype.resize%";
const AB_SLICE: &str = "%ArrayBuffer.prototype.slice%";
const AB_SLICE_TO_IMMUTABLE: &str = "%ArrayBuffer.prototype.sliceToImmutable%";
const AB_TRANSFER: &str = "%ArrayBuffer.prototype.transfer%";
const AB_TRANSFER_TO_FIXED_LENGTH: &str = "%ArrayBuffer.prototype.transferToFixedLength%";
const AB_TRANSFER_TO_IMMUTABLE: &str = "%ArrayBuffer.prototype.transferToImmutable%";

const SHARED_ARRAY_BUFFER: &str = "%SharedArrayBuffer%";
const SHARED_ARRAY_BUFFER_PROTO: &str = "%SharedArrayBuffer.prototype%";
const SAB_SPECIES: &str = "%get SharedArrayBuffer[Symbol.species]%";
const SAB_BYTE_LENGTH: &str = "%get SharedArrayBuffer.prototype.byteLength%";
const SAB_GROWABLE: &str = "%get SharedArrayBuffer.prototype.growable%";
const SAB_MAX_BYTE_LENGTH: &str = "%get SharedArrayBuffer.prototype.maxByteLength%";
const SAB_GROW: &str = "%SharedArrayBuffer.prototype.grow%";
const SAB_SLICE: &str = "%SharedArrayBuffer.prototype.slice%";

/// The host's maximum byte block size (spec 6.2.6.1 CreateByteDataBlock:
/// "If it is impossible to create such a Data Block, throw a RangeError").
/// test262 allocates up to 7 PiB in `allocation-limit` fixtures and expects
/// RangeError, so the cap is well below the addressable space.
pub const MAX_BYTE_LENGTH: usize = 1 << 30;

/// The agent-side bookkeeping of one ArrayBuffer/SharedArrayBuffer instance
/// (spec 25.1.1): [[ArrayBufferData]] (the shared byte block), the current
/// byte length, the resizable/growable maximum, and the shared/detached
/// flags. Detachment is represented by `detached` (the byte block is kept
/// alive by views even when nulled out, so `IsDetachedBuffer` is tracked
/// separately rather than by dropping the `SharedBuffer`).
#[derive(Debug, Clone)]
pub struct BufferState {
    /// [[ArrayBufferData]]: the shared byte vector.
    pub shared: SharedBuffer,
    /// [[ArrayBufferByteLength]].
    pub byte_length: usize,
    /// [[ArrayBufferMaxByteLength]]: `None` for fixed-length buffers.
    pub max_byte_length: Option<usize>,
    /// The resizable ArrayBuffer flag ([[ArrayBufferMaxByteLength]] set).
    pub resizable: bool,
    /// The growable SharedArrayBuffer flag.
    pub growable: bool,
    /// IsSharedArrayBuffer.
    pub is_shared: bool,
    /// [[ArrayBufferData]] is null (IsDetachedBuffer).
    pub detached: bool,
    /// [[ArrayBufferImmutable]] (ES2026 transferToImmutable): writes through
    /// views throw a TypeError.
    pub immutable: bool,
}

impl BufferState {
    pub fn fixed(shared: SharedBuffer, byte_length: usize) -> Self {
        BufferState {
            shared,
            byte_length,
            max_byte_length: None,
            resizable: false,
            growable: false,
            is_shared: false,
            detached: false,
            immutable: false,
        }
    }
}

/// IsDetachedBuffer (spec 25.1.2.5): the object is a registered buffer whose
/// `[[ArrayBufferData]]` has been nulled.
pub fn is_detached(agent: &Agent, id: u64) -> bool {
    agent
        .buffer_data
        .get(&id)
        .map(|cell| cell.borrow().detached)
        .unwrap_or(true)
}

/// IsSharedArrayBuffer (spec 25.1.2.6).
pub fn is_shared(agent: &Agent, value: &Value) -> bool {
    match value {
        Value::Object(obj) => agent
            .buffer_data
            .get(&obj.id())
            .map(|cell| cell.borrow().is_shared)
            .unwrap_or(false),
        _ => false,
    }
}

/// RequireInternalSlot([[ArrayBufferData]]): `this` must be a registered
/// buffer object.
fn require_buffer_object(agent: &Agent, this: &Value) -> Result<Handle<JsObject>, JsError> {
    let object = as_object(this).ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "Receiver is not an ArrayBuffer object".into(),
        )
    })?;
    if !agent.buffer_data.contains_key(&object.id()) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Receiver is not an ArrayBuffer object".into(),
        ));
    }
    Ok(object)
}

/// The current state of a buffer, re-fetched from the agent (never keep a
/// cloned `RefCell` handle across mutations).
fn state(agent: &Agent, id: u64) -> Option<std::cell::Ref<'_, BufferState>> {
    agent.buffer_data.get(&id).map(|cell| cell.borrow())
}

/// GetArrayBufferMaxByteLengthOption (spec 25.1.2.1): the `maxByteLength`
/// option as a ToIndex length, or `None` when the option is absent. A
/// non-object option (null, a primitive, or undefined) means no maximum.
fn max_byte_length_option(agent: &mut Agent, options: &Value) -> Result<Option<usize>, JsError> {
    if !matches!(options, Value::Object(_) | Value::Function(_)) {
        return Ok(None);
    }
    let value = get_property(
        agent,
        options,
        &JsString::from_utf8("maxByteLength"),
        options.clone(),
    )?;
    if matches!(value, Value::Undefined) {
        return Ok(None);
    }
    // ToIndex: ToPrimitive(Number) first so object values reach their
    // valueOf/toString through the agent.
    let prim = crate::context::to_primitive(agent, &value, crux::convert::ToPrimitiveHint::Number)?;
    Ok(Some(to_index(&prim)? as usize))
}

/// AllocateArrayBuffer (spec 25.1.2.2): the byte block plus the agent-side
/// `BufferState` entry.
fn allocate_array_buffer(
    agent: &mut Agent,
    object: &Handle<JsObject>,
    byte_length: usize,
    is_resizable: bool,
    max_byte_length: Option<usize>,
) -> Result<(), JsError> {
    if byte_length > MAX_BYTE_LENGTH {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "ArrayBuffer length exceeds the host limit".into(),
        ));
    }
    let shared = SharedBuffer::new(byte_length);
    agent.buffer_data.insert(
        object.id(),
        std::cell::RefCell::new(BufferState {
            shared,
            byte_length,
            max_byte_length,
            resizable: is_resizable,
            growable: false,
            is_shared: false,
            detached: false,
            immutable: false,
        }),
    );
    Ok(())
}

/// AllocateSharedArrayBuffer (spec 25.3.2.1): like AllocateArrayBuffer with
/// the shared flag set.
fn allocate_shared_array_buffer(
    agent: &mut Agent,
    object: &Handle<JsObject>,
    byte_length: usize,
    is_growable: bool,
    max_byte_length: Option<usize>,
) -> Result<(), JsError> {
    if byte_length > MAX_BYTE_LENGTH {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "SharedArrayBuffer length exceeds the host limit".into(),
        ));
    }
    let shared = SharedBuffer::new(byte_length);
    agent.buffer_data.insert(
        object.id(),
        std::cell::RefCell::new(BufferState {
            shared,
            byte_length,
            max_byte_length,
            resizable: false,
            growable: is_growable,
            is_shared: true,
            detached: false,
            immutable: false,
        }),
    );
    Ok(())
}

/// DetachArrayBuffer (spec 25.1.2.7): null the data block and reset the
/// length. Views keep their `SharedBuffer` handles, so reads through them
/// must be guarded by `IsDetachedBuffer`; the crux `SharedBuffer` carries
/// the same flag so Integer-Indexed access from the object model rejects
/// views too.
fn detach_array_buffer(agent: &mut Agent, id: u64) {
    if let Some(cell) = agent.buffer_data.get(&id) {
        let mut state = cell.borrow_mut();
        state.shared.mark_detached();
        state.detached = true;
        state.byte_length = 0;
    }
}

/// The byte range copy behind `slice`/`transfer`: the clamped `start`/`end`
/// indices into the buffer (spec ToIntegerOrInfinity + clamping, 25.1.5.5).
fn clamped_bounds(agent: &mut Agent, args: &[Value], len: u64) -> Result<(u64, u64), JsError> {
    let relative_start = to_integer_or_infinity(crate::context::to_number(
        agent,
        &args.first().cloned().unwrap_or(Value::Undefined),
    )?);
    let start = if relative_start < 0.0 {
        (len as i64).saturating_add(relative_start as i64).max(0) as u64
    } else {
        (relative_start as u64).min(len)
    };
    let relative_end = match args.get(1) {
        None | Some(Value::Undefined) => len as f64,
        Some(value) => to_integer_or_infinity(crate::context::to_number(agent, value)?),
    };
    let end = if relative_end < 0.0 {
        (len as i64).saturating_add(relative_end as i64).max(0) as u64
    } else {
        (relative_end as u64).min(len)
    };
    Ok((start, end))
}

/// SpeciesConstructor (spec 9.3.10): `exemplar.constructor[Symbol.species]`
/// with the given default.
fn species_constructor(
    agent: &mut Agent,
    exemplar: &Value,
    default_ctor: Value,
) -> Result<Value, JsError> {
    let ctor = get_property(
        agent,
        exemplar,
        &JsString::from_utf8("constructor"),
        exemplar.clone(),
    )?;
    if matches!(ctor, Value::Undefined) {
        return Ok(default_ctor);
    }
    if !is_constructor(&ctor) && !is_callable(&ctor) && !matches!(ctor, Value::Object(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "constructor is not an object".into(),
        ));
    }
    let species_key = PropertyKey::Symbol(crux::symbol::well_known("species").as_ref().clone());
    let species = get_property_key(agent, &ctor, &species_key, ctor.clone())?;
    match species {
        Value::Null | Value::Undefined => Ok(default_ctor),
        value if is_constructor(&value) => Ok(value),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "species is not a constructor".into(),
        )),
    }
}

/// ArrayBufferCopyAndDetach (spec 25.1.2.2): copy `new_length` bytes into a
/// fresh (optionally resizable) buffer and detach the source.
fn array_buffer_copy_and_detach(
    agent: &mut Agent,
    source_id: u64,
    new_length: usize,
    preserve_resizability: bool,
) -> Result<Value, JsError> {
    if new_length > MAX_BYTE_LENGTH {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "ArrayBuffer length exceeds the host limit".into(),
        ));
    }
    let (resizable, max) = {
        let source = state(agent, source_id).ok_or_else(|| {
            JsError::new(ErrorKind::TypeError, "Source is not an ArrayBuffer".into())
        })?;
        if preserve_resizability
            && source.resizable
            && new_length > source.max_byte_length.unwrap_or(usize::MAX)
        {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "newLength exceeds the maximum".into(),
            ));
        }
        (
            preserve_resizability && source.resizable,
            source.max_byte_length,
        )
    };
    let bytes = {
        let source = state(agent, source_id).expect("source present");
        // Copy min(byteLength, newLength) bytes: growth zero-fills the tail
        // of the fresh buffer (spec 25.1.5.5 ArrayBufferCopyAndDetach).
        let copy_len = new_length.min(source.byte_length);
        source.shared.read(0, copy_len)?
    };
    detach_array_buffer(agent, source_id);
    let proto = agent
        .current_realm()?
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
    allocate_array_buffer(
        agent,
        &object,
        new_length,
        resizable,
        if resizable { max } else { None },
    )?;
    let shared = state(agent, object.id())
        .expect("fresh buffer")
        .shared
        .clone();
    shared.write(0, &bytes)?;
    Ok(Value::Object(object))
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

/// ToIndex (spec 7.1.20) with agent dispatch for object receivers; the crux
/// `to_index` cannot run user `valueOf`/`toString`.
fn to_index_agent(agent: &mut Agent, value: &Value) -> Result<u64, JsError> {
    if matches!(value, Value::Undefined) {
        return Ok(0);
    }
    let number = crate::context::to_number(agent, value)?;
    let integer = to_integer_or_infinity(number);
    if integer < 0.0 || integer >= 9007199254740991.0 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "Index out of range".into(),
        ));
    }
    Ok(integer as u64)
}

/// ArrayBuffer(length [, options]) (spec 25.1.4.1).
fn array_buffer_construct(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    if matches!(new_target, Value::Undefined) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "ArrayBuffer must be called with 'new'".into(),
        ));
    }
    let byte_length =
        to_index_agent(agent, &args.first().cloned().unwrap_or(Value::Undefined))? as usize;
    let requested_max = max_byte_length_option(agent, args.get(1).unwrap_or(&Value::Undefined))?;
    let is_resizable = requested_max.is_some();
    if let Some(max) = requested_max {
        // AllocateArrayBuffer: a maxByteLength beyond the host limit is a
        // RangeError before any data block is created.
        if max > MAX_BYTE_LENGTH {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "maxByteLength exceeds the host limit".into(),
            ));
        }
        if byte_length > max {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "byteLength exceeds maxByteLength".into(),
            ));
        }
    }
    let prototype = get_prototype_from_constructor(agent, new_target, ARRAY_BUFFER_PROTO)?;
    let object = JsObject::ordinary_object_create(Some(prototype));
    allocate_array_buffer(agent, &object, byte_length, is_resizable, requested_max)?;
    Ok(Value::Object(object))
}

/// ArrayBuffer.isView (spec 25.1.4.2).
fn is_view(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let _ = this;
    let arg = args.first().unwrap_or(&Value::Undefined);
    match arg {
        Value::Object(obj) => {
            let has_viewed = matches!(obj.kind, crux::object::ObjectKind::IntegerIndexed(_))
                || agent.dataview_data.contains_key(&obj.id());
            Ok(Value::Boolean(has_viewed))
        }
        _ => Ok(Value::Boolean(false)),
    }
}

/// ArrayBuffer.prototype.byteLength (spec 25.1.5.4).
fn get_byte_length(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let object = require_buffer_object(agent, this)?;
    if is_shared(agent, this) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "byteLength is not defined for SharedArrayBuffer".into(),
        ));
    }
    let state = state(agent, object.id()).expect("registered buffer");
    let length = if state.detached { 0 } else { state.byte_length };
    Ok(Value::Number(length as f64))
}

/// ArrayBuffer.prototype.detached (spec 25.1.5.3).
fn get_detached(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let object = require_buffer_object(agent, this)?;
    if is_shared(agent, this) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "detached is not defined for SharedArrayBuffer".into(),
        ));
    }
    Ok(Value::Boolean(is_detached(agent, object.id())))
}

/// ArrayBuffer.prototype.maxByteLength (spec 25.1.5.1).
fn get_max_byte_length(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let object = require_buffer_object(agent, this)?;
    if is_shared(agent, this) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "maxByteLength is not defined for SharedArrayBuffer".into(),
        ));
    }
    let state = state(agent, object.id()).expect("registered buffer");
    let length = if state.detached {
        0
    } else {
        state.max_byte_length.unwrap_or(state.byte_length)
    };
    Ok(Value::Number(length as f64))
}

/// ArrayBuffer.prototype.resizable (spec 25.1.5.5).
fn get_resizable(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let object = require_buffer_object(agent, this)?;
    if is_shared(agent, this) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "resizable is not defined for SharedArrayBuffer".into(),
        ));
    }
    let resizable = state(agent, object.id())
        .map(|state| state.resizable)
        .unwrap_or(false);
    Ok(Value::Boolean(resizable))
}

/// ArrayBuffer.prototype.immutable (ES2026 25.1.5.5): RequireInternalSlot,
/// then a SharedArrayBuffer receiver throws and the immutable flag is
/// reported (detached buffers report false).
fn get_immutable(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let object = require_buffer_object(agent, this)?;
    if is_shared(agent, this) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "immutable is not defined for SharedArrayBuffer".into(),
        ));
    }
    let immutable = state(agent, object.id())
        .map(|state| state.immutable)
        .unwrap_or(false);
    Ok(Value::Boolean(immutable))
}

/// ArrayBuffer.prototype.sliceToImmutable (ES2026 25.1.5.9): like slice,
/// but the fresh buffer is immutable and %ArrayBuffer% is used directly (no
/// species constructor). Bounds resolve against the length captured before
/// the argument coercion.
fn slice_to_immutable(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = require_buffer_object(agent, this)?;
    if is_shared(agent, this) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "sliceToImmutable is not defined for SharedArrayBuffer".into(),
        ));
    }
    let len = state(agent, object.id())
        .expect("registered buffer")
        .byte_length as u64;
    // Detachment is verified before the arguments are read (spec 25.1.5.9
    // step 4); the post-coercion re-check catches a detach during coercion.
    if is_detached(agent, object.id()) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "ArrayBuffer is detached".into(),
        ));
    }
    let (start, end) = clamped_bounds(agent, args, len)?;
    if is_detached(agent, object.id()) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "ArrayBuffer is detached".into(),
        ));
    }
    let new_len = end.saturating_sub(start) as usize;
    // Bounds were resolved against the pre-coercion length; a shrink below
    // the requested end is a RangeError (spec 25.1.5.9 step 14).
    let current_len = state(agent, object.id()).expect("present").byte_length as u64;
    if current_len < end {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "current length is smaller than the requested end".into(),
        ));
    }
    let bytes = {
        let source = state(agent, object.id()).expect("source present");
        source.shared.read(start as usize, new_len)?
    };
    let proto = agent
        .current_realm()?
        .intrinsics
        .get("%ArrayBuffer.prototype%")
        .and_then(|value| as_object(&value))
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "%ArrayBuffer.prototype% missing".into(),
            )
        })?;
    let new_object = JsObject::ordinary_object_create(Some(proto));
    allocate_array_buffer(agent, &new_object, new_len, false, None)?;
    if let Some(cell) = agent.buffer_data.get(&new_object.id()) {
        let mut state = cell.borrow_mut();
        state.shared.write(0, &bytes)?;
        state.immutable = true;
        state.shared.mark_immutable();
    }
    Ok(Value::Object(new_object))
}

/// ArrayBuffer.prototype.resize (spec 25.1.5.6).
fn resize(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = require_buffer_object(agent, this)?;
    if is_shared(agent, this) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "resize is not defined for SharedArrayBuffer".into(),
        ));
    }
    // Immutable buffers are verified before newLength is read (spec
    // 25.1.5.6: the immutable check precedes ToIndex).
    if state(agent, object.id())
        .map(|s| s.immutable)
        .unwrap_or(false)
    {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "ArrayBuffer is immutable".into(),
        ));
    }
    let new_length =
        to_index_agent(agent, &args.first().cloned().unwrap_or(Value::Undefined))? as usize;
    let (resizable, max, detached) = {
        let state = state(agent, object.id()).expect("registered buffer");
        (state.resizable, state.max_byte_length, state.detached)
    };
    if !resizable {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "ArrayBuffer is not resizable".into(),
        ));
    }
    if detached {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "ArrayBuffer is detached".into(),
        ));
    }
    if new_length > MAX_BYTE_LENGTH {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "ArrayBuffer length exceeds the host limit".into(),
        ));
    }
    if new_length > max.unwrap_or(usize::MAX) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "newLength exceeds maxByteLength".into(),
        ));
    }
    if let Some(cell) = agent.buffer_data.get(&object.id()) {
        let mut state = cell.borrow_mut();
        state.shared.resize(new_length)?;
        state.byte_length = new_length;
    }
    Ok(Value::Undefined)
}

/// ArrayBuffer.prototype.slice (spec 25.1.5.7).
fn slice(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = require_buffer_object(agent, this)?;
    if is_shared(agent, this) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "slice is not defined for SharedArrayBuffer".into(),
        ));
    }
    let (byte_length, detached) = {
        let state = state(agent, object.id()).expect("registered buffer");
        (state.byte_length, state.detached)
    };
    if detached {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "ArrayBuffer is detached".into(),
        ));
    }
    let (start, end) = clamped_bounds(agent, args, byte_length as u64)?;
    let new_len = end.saturating_sub(start) as usize;
    let default_ctor = agent
        .current_realm()?
        .intrinsics
        .get(ARRAY_BUFFER)
        .unwrap_or(Value::Undefined);
    let ctor = species_constructor(agent, this, default_ctor)?;
    let new = crate::function::construct(agent, &ctor, &[Value::Number(new_len as f64)], &ctor)?;
    let new_object = as_object(&new).ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "slice constructor returned a non-object".into(),
        )
    })?;
    if crux::ops::same_value(&new, this) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "slice constructor returned the receiver".into(),
        ));
    }
    if !agent.buffer_data.contains_key(&new_object.id()) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "slice constructor did not produce an ArrayBuffer".into(),
        ));
    }
    if is_shared(agent, &new) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "slice constructor produced a SharedArrayBuffer".into(),
        ));
    }
    let (new_length, new_detached) = {
        let state = state(agent, new_object.id()).expect("fresh buffer");
        (state.byte_length, state.detached)
    };
    if new_detached || new_length < new_len {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "slice constructor produced an invalid ArrayBuffer".into(),
        ));
    }
    if state(agent, new_object.id())
        .map(|s| s.immutable)
        .unwrap_or(false)
    {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "slice constructor produced an immutable ArrayBuffer".into(),
        ));
    }
    let bytes = {
        let source = state(agent, object.id()).expect("source present");
        source.shared.read(start as usize, new_len)?
    };
    let target = state(agent, new_object.id())
        .expect("fresh buffer")
        .shared
        .clone();
    target.write(0, &bytes)?;
    Ok(new)
}

/// ArrayBuffer.prototype.transfer (spec 25.1.5.8) / transferToFixedLength
/// (25.1.5.9): ArrayBufferCopyAndDetach with preserveResizability = isTransfer.
fn transfer(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    preserve_resizability: bool,
) -> Result<Value, JsError> {
    let object = require_buffer_object(agent, this)?;
    if is_shared(agent, this) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "transfer is not defined for SharedArrayBuffer".into(),
        ));
    }
    let new_length = match args.first() {
        None | Some(Value::Undefined) => state(agent, object.id())
            .map(|state| state.byte_length)
            .unwrap_or(0),
        Some(value) => to_index_agent(agent, value)? as usize,
    };
    if is_detached(agent, object.id()) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "ArrayBuffer is detached".into(),
        ));
    }
    if state(agent, object.id())
        .map(|s| s.immutable)
        .unwrap_or(false)
    {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "ArrayBuffer is immutable".into(),
        ));
    }
    array_buffer_copy_and_detach(agent, object.id(), new_length, preserve_resizability)
}

/// ArrayBuffer.prototype.transferToImmutable (ES2026 25.1.5.10): copy into a
/// fresh immutable buffer and detach the source.
fn transfer_to_immutable(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let object = require_buffer_object(agent, this)?;
    if is_shared(agent, this) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "transferToImmutable is not defined for SharedArrayBuffer".into(),
        ));
    }
    // ArrayBufferCopyAndDetach reads newLength before the detachability
    // checks (spec 25.1.2.2 steps 3-6).
    let new_length = match args.first() {
        None | Some(Value::Undefined) => state(agent, object.id())
            .map(|state| state.byte_length)
            .unwrap_or(0),
        Some(value) => to_index_agent(agent, value)? as usize,
    };
    if is_detached(agent, object.id()) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "ArrayBuffer is detached".into(),
        ));
    }
    if state(agent, object.id())
        .map(|s| s.immutable)
        .unwrap_or(false)
    {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "ArrayBuffer is immutable".into(),
        ));
    }
    let result = array_buffer_copy_and_detach(agent, object.id(), new_length, false)?;
    if let Value::Object(obj) = &result
        && let Some(cell) = agent.buffer_data.get(&obj.id())
    {
        let mut state = cell.borrow_mut();
        state.immutable = true;
        state.shared.mark_immutable();
    }
    Ok(result)
}

/// SharedArrayBuffer(length [, options]) (spec 25.3.3.1).
fn shared_array_buffer_construct(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    if matches!(new_target, Value::Undefined) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "SharedArrayBuffer must be called with 'new'".into(),
        ));
    }
    let byte_length =
        to_index_agent(agent, &args.first().cloned().unwrap_or(Value::Undefined))? as usize;
    let requested_max = max_byte_length_option(agent, args.get(1).unwrap_or(&Value::Undefined))?;
    let is_growable = requested_max.is_some();
    if let Some(max) = requested_max {
        if max > MAX_BYTE_LENGTH {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "maxByteLength exceeds the host limit".into(),
            ));
        }
        if byte_length > max {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "byteLength exceeds maxByteLength".into(),
            ));
        }
    }
    let prototype = get_prototype_from_constructor(agent, new_target, SHARED_ARRAY_BUFFER_PROTO)?;
    let object = JsObject::ordinary_object_create(Some(prototype));
    allocate_shared_array_buffer(agent, &object, byte_length, is_growable, requested_max)?;
    Ok(Value::Object(object))
}

/// Wrap an existing shared byte block as a SharedArrayBuffer object in this
/// agent (used by the worker machinery to hand a block shared with another
/// agent to a fresh realm; spec 25.3.2.1 with a supplied [[ArrayBufferData]]).
pub fn shared_array_buffer_from_block(
    agent: &mut Agent,
    shared: SharedBuffer,
    byte_length: usize,
) -> Result<Value, JsError> {
    let prototype = agent
        .current_realm()?
        .intrinsics
        .get(SHARED_ARRAY_BUFFER_PROTO)
        .and_then(|value| as_object(&value))
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "%SharedArrayBuffer.prototype% missing".into(),
            )
        })?;
    let object = JsObject::ordinary_object_create(Some(prototype));
    agent.buffer_data.insert(
        object.id(),
        std::cell::RefCell::new(BufferState {
            shared,
            byte_length,
            max_byte_length: None,
            resizable: false,
            growable: false,
            is_shared: true,
            detached: false,
            immutable: false,
        }),
    );
    Ok(Value::Object(object))
}

/// SharedArrayBuffer.prototype.byteLength (spec 25.3.5.1).
fn sab_get_byte_length(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let object = require_buffer_object(agent, this)?;
    if !is_shared(agent, this) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "byteLength is not defined for ArrayBuffer".into(),
        ));
    }
    let length = state(agent, object.id())
        .map(|state| state.byte_length)
        .unwrap_or(0);
    Ok(Value::Number(length as f64))
}

/// SharedArrayBuffer.prototype.growable (spec 25.3.5.2).
fn sab_get_growable(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let object = require_buffer_object(agent, this)?;
    if !is_shared(agent, this) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "growable is not defined for ArrayBuffer".into(),
        ));
    }
    let growable = state(agent, object.id())
        .map(|state| state.growable)
        .unwrap_or(false);
    Ok(Value::Boolean(growable))
}

/// SharedArrayBuffer.prototype.maxByteLength (spec 25.3.5.3).
fn sab_get_max_byte_length(
    agent: &mut Agent,
    this: &Value,
    _args: &[Value],
) -> Result<Value, JsError> {
    let object = require_buffer_object(agent, this)?;
    if !is_shared(agent, this) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "maxByteLength is not defined for ArrayBuffer".into(),
        ));
    }
    let state = state(agent, object.id()).expect("registered buffer");
    let length = if state.growable {
        state.max_byte_length.unwrap_or(state.byte_length)
    } else {
        state.byte_length
    };
    Ok(Value::Number(length as f64))
}

/// SharedArrayBuffer.prototype.grow (spec 25.3.5.4).
fn sab_grow(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = require_buffer_object(agent, this)?;
    if !is_shared(agent, this) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "grow is not defined for ArrayBuffer".into(),
        ));
    }
    let new_length =
        to_index_agent(agent, &args.first().cloned().unwrap_or(Value::Undefined))? as usize;
    let (growable, max, byte_length) = {
        let state = state(agent, object.id()).expect("registered buffer");
        (state.growable, state.max_byte_length, state.byte_length)
    };
    if !growable {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "SharedArrayBuffer is not growable".into(),
        ));
    }
    if new_length > MAX_BYTE_LENGTH {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "SharedArrayBuffer length exceeds the host limit".into(),
        ));
    }
    if new_length > max.unwrap_or(usize::MAX) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "newLength exceeds maxByteLength".into(),
        ));
    }
    if new_length < byte_length {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "newLength is smaller than the current byteLength".into(),
        ));
    }
    if let Some(cell) = agent.buffer_data.get(&object.id()) {
        let mut state = cell.borrow_mut();
        state.shared.resize(new_length)?;
        state.byte_length = new_length;
    }
    Ok(Value::Undefined)
}

/// SharedArrayBuffer.prototype.slice (spec 25.3.5.5).
fn sab_slice(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = require_buffer_object(agent, this)?;
    if !is_shared(agent, this) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "slice is not defined for ArrayBuffer".into(),
        ));
    }
    let byte_length = state(agent, object.id())
        .map(|state| state.byte_length)
        .unwrap_or(0);
    let (start, end) = clamped_bounds(agent, args, byte_length as u64)?;
    let new_len = end.saturating_sub(start) as usize;
    let default_ctor = agent
        .current_realm()?
        .intrinsics
        .get(SHARED_ARRAY_BUFFER)
        .unwrap_or(Value::Undefined);
    let ctor = species_constructor(agent, this, default_ctor)?;
    let new = crate::function::construct(agent, &ctor, &[Value::Number(new_len as f64)], &ctor)?;
    let new_object = as_object(&new).ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "slice constructor returned a non-object".into(),
        )
    })?;
    if crux::ops::same_value(&new, this) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "slice constructor returned the receiver".into(),
        ));
    }
    if !is_shared(agent, &new) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "slice constructor did not produce a SharedArrayBuffer".into(),
        ));
    }
    let new_length = state(agent, new_object.id())
        .map(|state| state.byte_length)
        .unwrap_or(0);
    if new_length < new_len {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "slice constructor produced an invalid SharedArrayBuffer".into(),
        ));
    }
    let bytes = {
        let source = state(agent, object.id()).expect("source present");
        source.shared.read(start as usize, new_len)?
    };
    let target = state(agent, new_object.id())
        .expect("fresh buffer")
        .shared
        .clone();
    target.write(0, &bytes)?;
    Ok(new)
}

/// GetPrototypeFromConstructor (spec 10.1.14): `constructor.prototype` when
/// it is an object, else the realm's default.
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

/// The species getter body: `return this` (spec 25.1.4.3, 25.3.3.2).
fn species_getter(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let _ = agent;
    Ok(this.clone())
}

pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));

    // ---- ArrayBuffer ----
    let ab_proto = JsObject::ordinary_object_create(object_proto.clone());
    let ab_proto_value = Value::Object(ab_proto.clone());
    let ab_ctor = Function::create_builtin(
        Some(JsString::from_utf8("ArrayBuffer")),
        1,
        placeholder("ArrayBuffer"),
        Some(Box::new(placeholder("ArrayBuffer"))),
        None,
    )?;
    let ab_ctor_value = Value::Function(ab_ctor.clone());
    realm.intrinsics.define(ARRAY_BUFFER, ab_ctor_value.clone());
    realm
        .intrinsics
        .define(ARRAY_BUFFER_PROTO, ab_proto_value.clone());
    ab_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(ab_proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    ab_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(ab_ctor_value.clone()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // ArrayBuffer statics: isView + @@species.
    let is_view_func = Function::create_builtin(
        Some(JsString::from_utf8("isView")),
        1,
        placeholder("isView"),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(AB_IS_VIEW, Value::Function(is_view_func.clone()));
    ab_ctor.define_property(
        &JsString::from_utf8("isView"),
        &PropertyDescriptor {
            value: Some(Value::Function(is_view_func)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    let ab_species = Function::create_builtin(
        Some(JsString::from_utf8("get [Symbol.species]")),
        0,
        placeholder("species"),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(AB_SPECIES, Value::Function(ab_species.clone()));
    ab_ctor.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("species").as_ref().clone()),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: Some(Value::Function(ab_species)),
            set: Some(Value::Undefined),
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // ArrayBuffer prototype accessors.
    let ab_accessors: [(&str, &str); 5] = [
        ("byteLength", AB_BYTE_LENGTH),
        ("detached", AB_DETACHED),
        ("immutable", AB_IMMUTABLE),
        ("maxByteLength", AB_MAX_BYTE_LENGTH),
        ("resizable", AB_RESIZABLE),
    ];
    for (name, intrinsic) in ab_accessors {
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
        ab_proto.define_property(
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

    // ArrayBuffer prototype methods.
    let ab_methods: [(&str, &str, u64); 6] = [
        ("resize", AB_RESIZE, 1),
        ("slice", AB_SLICE, 2),
        ("sliceToImmutable", AB_SLICE_TO_IMMUTABLE, 2),
        ("transfer", AB_TRANSFER, 0),
        ("transferToFixedLength", AB_TRANSFER_TO_FIXED_LENGTH, 0),
        ("transferToImmutable", AB_TRANSFER_TO_IMMUTABLE, 0),
    ];
    for (name, intrinsic, length) in ab_methods {
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
        ab_proto.define_property(
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
    ab_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(str("ArrayBuffer")),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // ---- SharedArrayBuffer ----
    let sab_proto = JsObject::ordinary_object_create(object_proto);
    let sab_proto_value = Value::Object(sab_proto.clone());
    let sab_ctor = Function::create_builtin(
        Some(JsString::from_utf8("SharedArrayBuffer")),
        1,
        placeholder("SharedArrayBuffer"),
        Some(Box::new(placeholder("SharedArrayBuffer"))),
        None,
    )?;
    let sab_ctor_value = Value::Function(sab_ctor.clone());
    realm
        .intrinsics
        .define(SHARED_ARRAY_BUFFER, sab_ctor_value.clone());
    realm
        .intrinsics
        .define(SHARED_ARRAY_BUFFER_PROTO, sab_proto_value.clone());
    sab_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(sab_proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    sab_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(sab_ctor_value.clone()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    let sab_species = Function::create_builtin(
        Some(JsString::from_utf8("get [Symbol.species]")),
        0,
        placeholder("species"),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(SAB_SPECIES, Value::Function(sab_species.clone()));
    sab_ctor.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("species").as_ref().clone()),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: Some(Value::Function(sab_species)),
            set: Some(Value::Undefined),
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    let sab_accessors: [(&str, &str); 3] = [
        ("byteLength", SAB_BYTE_LENGTH),
        ("growable", SAB_GROWABLE),
        ("maxByteLength", SAB_MAX_BYTE_LENGTH),
    ];
    for (name, intrinsic) in sab_accessors {
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
        sab_proto.define_property(
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
    for (name, intrinsic, length) in [("grow", SAB_GROW, 1), ("slice", SAB_SLICE, 2)] {
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
        sab_proto.define_property(
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
    sab_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(str("SharedArrayBuffer")),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    for (name, value) in [
        ("ArrayBuffer", ab_ctor_value),
        ("SharedArrayBuffer", sab_ctor_value),
    ] {
        realm.global_object.define_property_or_throw(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: Some(value),
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

/// The ArrayBuffer/SharedArrayBuffer members that need the agent, dispatched
/// by intrinsic identity from `runtime::function::call`/`construct`.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(ARRAY_BUFFER).as_ref() == Some(callee) {
        return Some(Err(JsError::new(
            ErrorKind::TypeError,
            "ArrayBuffer must be called with 'new'".into(),
        )));
    }
    if intrinsics.get(SHARED_ARRAY_BUFFER).as_ref() == Some(callee) {
        return Some(Err(JsError::new(
            ErrorKind::TypeError,
            "SharedArrayBuffer must be called with 'new'".into(),
        )));
    }
    if intrinsics.get(AB_IS_VIEW).as_ref() == Some(callee) {
        return Some(is_view(agent, this, args));
    }
    if intrinsics.get(AB_SPECIES).as_ref() == Some(callee)
        || intrinsics.get(SAB_SPECIES).as_ref() == Some(callee)
    {
        return Some(species_getter(agent, this, args));
    }
    if intrinsics.get(AB_BYTE_LENGTH).as_ref() == Some(callee) {
        return Some(get_byte_length(agent, this, args));
    }
    if intrinsics.get(AB_DETACHED).as_ref() == Some(callee) {
        return Some(get_detached(agent, this, args));
    }
    if intrinsics.get(AB_MAX_BYTE_LENGTH).as_ref() == Some(callee) {
        return Some(get_max_byte_length(agent, this, args));
    }
    if intrinsics.get(AB_RESIZABLE).as_ref() == Some(callee) {
        return Some(get_resizable(agent, this, args));
    }
    if intrinsics.get(AB_IMMUTABLE).as_ref() == Some(callee) {
        return Some(get_immutable(agent, this, args));
    }
    if intrinsics.get(AB_RESIZE).as_ref() == Some(callee) {
        return Some(resize(agent, this, args));
    }
    if intrinsics.get(AB_SLICE).as_ref() == Some(callee) {
        return Some(slice(agent, this, args));
    }
    if intrinsics.get(AB_SLICE_TO_IMMUTABLE).as_ref() == Some(callee) {
        return Some(slice_to_immutable(agent, this, args));
    }
    if intrinsics.get(AB_TRANSFER).as_ref() == Some(callee) {
        return Some(transfer(agent, this, args, true));
    }
    if intrinsics.get(AB_TRANSFER_TO_FIXED_LENGTH).as_ref() == Some(callee) {
        return Some(transfer(agent, this, args, false));
    }
    if intrinsics.get(AB_TRANSFER_TO_IMMUTABLE).as_ref() == Some(callee) {
        return Some(transfer_to_immutable(agent, this, args));
    }
    if intrinsics.get(SAB_BYTE_LENGTH).as_ref() == Some(callee) {
        return Some(sab_get_byte_length(agent, this, args));
    }
    if intrinsics.get(SAB_GROWABLE).as_ref() == Some(callee) {
        return Some(sab_get_growable(agent, this, args));
    }
    if intrinsics.get(SAB_MAX_BYTE_LENGTH).as_ref() == Some(callee) {
        return Some(sab_get_max_byte_length(agent, this, args));
    }
    if intrinsics.get(SAB_GROW).as_ref() == Some(callee) {
        return Some(sab_grow(agent, this, args));
    }
    if intrinsics.get(SAB_SLICE).as_ref() == Some(callee) {
        return Some(sab_slice(agent, this, args));
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
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(ARRAY_BUFFER).as_ref() == Some(callee) {
        return Some(array_buffer_construct(agent, args, new_target));
    }
    if intrinsics.get(SHARED_ARRAY_BUFFER).as_ref() == Some(callee) {
        return Some(shared_array_buffer_construct(agent, args, new_target));
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
    fn construct_byte_lengths() {
        assert_eq!(number("new ArrayBuffer(8).byteLength"), 8.0);
        assert_eq!(number("new ArrayBuffer(0).byteLength"), 0.0);
        // A missing length is ToIndex(undefined) = 0.
        assert_eq!(number("new ArrayBuffer().byteLength"), 0.0);
        // ToIndex truncates fractional lengths.
        assert_eq!(number("new ArrayBuffer(1.9).byteLength"), 1.0);
        assert!(matches!(
            run("new ArrayBuffer(-1)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        assert!(matches!(
            run("new ArrayBuffer(Infinity)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        // Calling without `new` is a TypeError.
        assert!(matches!(
            run("ArrayBuffer(8)"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
    }

    #[test]
    fn allocation_limit_throws_range_error() {
        // 2**31 is above the host cap (2**30); must be a RangeError, never a
        // panic or abort.
        assert!(matches!(
            run("new ArrayBuffer(2 ** 31)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        assert!(matches!(
            run("new ArrayBuffer(2 ** 32)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        assert!(matches!(
            run("new SharedArrayBuffer(2 ** 31)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
    }

    #[test]
    fn resizable_flags_and_max_byte_length() {
        assert!(bool("new ArrayBuffer(4, { maxByteLength: 8 }).resizable"));
        assert!(!bool("new ArrayBuffer(4).resizable"));
        // A non-object options argument means no maximum.
        assert!(!bool("new ArrayBuffer(4, null).resizable"));
        assert_eq!(
            number("new ArrayBuffer(4, { maxByteLength: 8 }).maxByteLength"),
            8.0
        );
        // maxByteLength equal to the length is allowed.
        assert!(bool("new ArrayBuffer(4, { maxByteLength: 4 }).resizable"));
        assert!(matches!(
            run("new ArrayBuffer(4, { maxByteLength: 2 })"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
    }

    #[test]
    fn resize_grows_and_zero_fills() {
        assert_eq!(
            number(
                "(function(){ var b = new ArrayBuffer(4, { maxByteLength: 8 }); b.resize(6); return b.byteLength; })()"
            ),
            6.0
        );
        // The grown tail is zero-filled for fresh views.
        assert_eq!(
            number(
                "(function(){ var b = new ArrayBuffer(4, { maxByteLength: 8 }); b.resize(6); return new Uint8Array(b)[4]; })()"
            ),
            0.0
        );
    }

    #[test]
    fn resize_shrinks() {
        assert_eq!(
            number(
                "(function(){ var b = new ArrayBuffer(4, { maxByteLength: 8 }); b.resize(2); return b.byteLength; })()"
            ),
            2.0
        );
    }

    #[test]
    fn resize_bounds_and_errors() {
        assert!(matches!(
            run("(function(){ var b = new ArrayBuffer(4, { maxByteLength: 8 }); b.resize(9); })()"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        assert!(matches!(
            run("new ArrayBuffer(4).resize(8)"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
        // resize takes ToIndex: negatives throw, fractions truncate.
        assert!(matches!(
            run("(function(){ var b = new ArrayBuffer(4, { maxByteLength: 8 }); b.resize(-1); })()"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        assert_eq!(
            number(
                "(function(){ var b = new ArrayBuffer(4, { maxByteLength: 8 }); b.resize(1.9); return b.byteLength; })()"
            ),
            1.0
        );
    }

    #[test]
    fn transfer_detaches_source() {
        assert_eq!(
            text(
                "(function(){ var b = new ArrayBuffer(4); var t = b.transfer(); return b.detached + ':' + t.detached + ':' + t.byteLength; })()"
            ),
            "true:false:4"
        );
        // A detached buffer reports byteLength (and maxByteLength) 0.
        assert_eq!(
            number(
                "(function(){ var b = new ArrayBuffer(4); b.transfer(); return b.byteLength; })()"
            ),
            0.0
        );
        assert_eq!(
            number(
                "(function(){ var b = new ArrayBuffer(4); b.transfer(); return b.maxByteLength; })()"
            ),
            0.0
        );
    }

    #[test]
    fn transfer_on_an_already_detached_buffer_throws() {
        // spec 25.1.3.3 ArrayBufferCopyAndDetach: transfer/transferToFixedLength
        // on a detached buffer is a TypeError.
        assert!(
            run("(function(){ var b = new ArrayBuffer(1); b.transfer(); b.transfer(); })()")
                .is_err()
        );
        assert!(run(
            "(function(){ var b = new ArrayBuffer(1); b.transfer(); b.transferToFixedLength(); })()"
        )
        .is_err());
    }

    #[test]
    fn transfer_copies_bytes() {
        assert_eq!(
            number(
                "(function(){ var b = new ArrayBuffer(4); var u = new Uint8Array(b); u[0] = 42; var t = b.transfer(); return new Uint8Array(t)[0]; })()"
            ),
            42.0
        );
    }

    #[test]
    fn transfer_with_explicit_length() {
        assert_eq!(number("new ArrayBuffer(8).transfer(4).byteLength"), 4.0);
        // Growing transfer zero-fills the tail beyond the source bytes.
        assert_eq!(
            text(
                "(function(){ var t = new ArrayBuffer(2).transfer(4); return t.byteLength + ':' + new Uint8Array(t)[2]; })()"
            ),
            "4:0"
        );
        // transfer preserves resizability and the max.
        assert_eq!(
            text(
                "(function(){ var t = new ArrayBuffer(4, { maxByteLength: 8 }).transfer(6); return t.resizable + ':' + t.maxByteLength + ':' + t.byteLength; })()"
            ),
            "true:8:6"
        );
        assert!(matches!(
            run("new ArrayBuffer(4, { maxByteLength: 8 }).transfer(9)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
    }

    #[test]
    fn transfer_to_fixed_length_loses_resizability() {
        assert_eq!(
            text(
                "(function(){ var b = new ArrayBuffer(4, { maxByteLength: 8 }); var t = b.transferToFixedLength(); return t.resizable + ':' + t.maxByteLength; })()"
            ),
            "false:4"
        );
    }

    #[test]
    fn slice_bounds() {
        assert_eq!(number("new ArrayBuffer(4).slice(1, 3).byteLength"), 2.0);
        assert_eq!(number("new ArrayBuffer(4).slice(-2).byteLength"), 2.0);
        assert_eq!(number("new ArrayBuffer(4).slice(10).byteLength"), 0.0);
    }

    #[test]
    fn slice_copies_bytes() {
        assert_eq!(
            text(
                "(function(){ var b = new ArrayBuffer(4); var u = new Uint8Array(b); u[0] = 1; u[1] = 2; u[2] = 3; u[3] = 4; return new Uint8Array(b.slice(1, 3)).join(','); })()"
            ),
            "2,3"
        );
    }

    #[test]
    fn detached_buffer_operations_throw() {
        assert!(matches!(
            run("(function(){ var b = new ArrayBuffer(4); b.transfer(); b.slice(0, 1); })()"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
        assert!(matches!(
            run("(function(){ var b = new ArrayBuffer(4); b.transfer(); new Uint8Array(b); })()"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
        // A detached resizable buffer cannot be resized.
        assert!(matches!(
            run("(function(){ var b = new ArrayBuffer(4, { maxByteLength: 8 }); b.transfer(); b.resize(4); })()"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
    }

    #[test]
    fn shared_array_buffer_construction() {
        assert_eq!(number("new SharedArrayBuffer(8).byteLength"), 8.0);
        assert_eq!(number("new SharedArrayBuffer(0).byteLength"), 0.0);
        assert!(matches!(
            run("new SharedArrayBuffer(-1)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        assert!(matches!(
            run("SharedArrayBuffer(8)"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
    }

    #[test]
    fn shared_array_buffer_growable() {
        assert!(bool(
            "new SharedArrayBuffer(4, { maxByteLength: 8 }).growable"
        ));
        assert!(!bool("new SharedArrayBuffer(4).growable"));
        assert_eq!(
            number("new SharedArrayBuffer(4, { maxByteLength: 8 }).maxByteLength"),
            8.0
        );
        assert_eq!(
            number(
                "(function(){ var s = new SharedArrayBuffer(4, { maxByteLength: 8 }); s.grow(6); return s.byteLength; })()"
            ),
            6.0
        );
        assert!(matches!(
            run("(function(){ var s = new SharedArrayBuffer(4, { maxByteLength: 8 }); s.grow(9); })()"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        // grow cannot shrink below the current length.
        assert!(matches!(
            run("(function(){ var s = new SharedArrayBuffer(4, { maxByteLength: 8 }); s.grow(2); })()"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        assert!(matches!(
            run("new SharedArrayBuffer(4).grow(8)"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
    }

    #[test]
    fn shared_array_buffer_slice_and_cross_type_methods() {
        assert_eq!(
            number("new SharedArrayBuffer(4).slice(1, 2).byteLength"),
            1.0
        );
        assert_eq!(text("typeof SharedArrayBuffer.prototype.grow"), "function");
        // resize is ArrayBuffer-only; grow is SharedArrayBuffer-only.
        assert!(matches!(
            run("ArrayBuffer.prototype.resize.call(new SharedArrayBuffer(4), 8)"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
        assert!(matches!(
            run("SharedArrayBuffer.prototype.grow.call(new ArrayBuffer(4), 8)"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
    }
}
