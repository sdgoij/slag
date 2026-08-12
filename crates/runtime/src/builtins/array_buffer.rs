//! The ArrayBuffer and SharedArrayBuffer built-ins (spec 25.1, 25.3): the
//! backing byte blocks of the Integer-Indexed and DataView exotics. The
//! [[ArrayBufferData]] byte vector lives in a crux `SharedBuffer` aliased by
//! every view; the per-buffer bookkeeping (`[[ArrayBufferByteLength]]`,
//! `[[ArrayBufferMaxByteLength]]`, resizable/growable/shared flags,
//! detachment) is a `BufferState` in the agent's `buffer_data` table keyed by
//! object identity.

use crux::convert::{to_index, to_integer_or_infinity, to_number};
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
const AB_MAX_BYTE_LENGTH: &str = "%get ArrayBuffer.prototype.maxByteLength%";
const AB_RESIZABLE: &str = "%get ArrayBuffer.prototype.resizable%";
const AB_RESIZE: &str = "%ArrayBuffer.prototype.resize%";
const AB_SLICE: &str = "%ArrayBuffer.prototype.slice%";
const AB_TRANSFER: &str = "%ArrayBuffer.prototype.transfer%";
const AB_TRANSFER_TO_FIXED_LENGTH: &str = "%ArrayBuffer.prototype.transferToFixedLength%";

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
        }),
    );
    Ok(())
}

/// DetachArrayBuffer (spec 25.1.2.7): null the data block and reset the
/// length. Views keep their `SharedBuffer` handles, so reads through them
/// must be guarded by `IsDetachedBuffer`.
fn detach_array_buffer(agent: &mut Agent, id: u64) {
    if let Some(cell) = agent.buffer_data.get(&id) {
        let mut state = cell.borrow_mut();
        state.detached = true;
        state.byte_length = 0;
    }
}

/// The byte range copy behind `slice`/`transfer`: the clamped `start`/`end`
/// indices into the buffer (spec ToIntegerOrInfinity + clamping, 25.1.5.5).
fn clamped_bounds(args: &[Value], len: u64) -> Result<(u64, u64), JsError> {
    let relative_start = to_integer_or_infinity(to_number(
        &args.first().cloned().unwrap_or(Value::Undefined),
    )?);
    let start = if relative_start < 0.0 {
        (len as i64).saturating_add(relative_start as i64).max(0) as u64
    } else {
        (relative_start as u64).min(len)
    };
    let relative_end = match args.get(1) {
        None | Some(Value::Undefined) => len as f64,
        Some(value) => to_integer_or_infinity(to_number(value)?),
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
    let byte_length = to_index(&args.first().cloned().unwrap_or(Value::Undefined))? as usize;
    let requested_max = max_byte_length_option(agent, args.get(1).unwrap_or(&Value::Undefined))?;
    let is_resizable = requested_max.is_some();
    if let Some(max) = requested_max
        && byte_length > max
    {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "byteLength exceeds maxByteLength".into(),
        ));
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

/// ArrayBuffer.prototype.resize (spec 25.1.5.6).
fn resize(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = require_buffer_object(agent, this)?;
    if is_shared(agent, this) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "resize is not defined for SharedArrayBuffer".into(),
        ));
    }
    let new_length = to_index(&args.first().cloned().unwrap_or(Value::Undefined))? as usize;
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
    let (start, end) = clamped_bounds(args, byte_length as u64)?;
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
        Some(value) => to_index(value)? as usize,
    };
    if is_detached(agent, object.id()) {
        return Ok(this.clone());
    }
    array_buffer_copy_and_detach(agent, object.id(), new_length, preserve_resizability)
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
    let byte_length = to_index(&args.first().cloned().unwrap_or(Value::Undefined))? as usize;
    let requested_max = max_byte_length_option(agent, args.get(1).unwrap_or(&Value::Undefined))?;
    let is_growable = requested_max.is_some();
    if let Some(max) = requested_max
        && byte_length > max
    {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "byteLength exceeds maxByteLength".into(),
        ));
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
    let new_length = to_index(&args.first().cloned().unwrap_or(Value::Undefined))? as usize;
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
    let (start, end) = clamped_bounds(args, byte_length as u64)?;
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
    let ab_accessors: [(&str, &str); 4] = [
        ("byteLength", AB_BYTE_LENGTH),
        ("detached", AB_DETACHED),
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
    let ab_methods: [(&str, &str, u64); 4] = [
        ("resize", AB_RESIZE, 1),
        ("slice", AB_SLICE, 2),
        ("transfer", AB_TRANSFER, 1),
        ("transferToFixedLength", AB_TRANSFER_TO_FIXED_LENGTH, 1),
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
    if intrinsics.get(AB_RESIZE).as_ref() == Some(callee) {
        return Some(resize(agent, this, args));
    }
    if intrinsics.get(AB_SLICE).as_ref() == Some(callee) {
        return Some(slice(agent, this, args));
    }
    if intrinsics.get(AB_TRANSFER).as_ref() == Some(callee) {
        return Some(transfer(agent, this, args, true));
    }
    if intrinsics.get(AB_TRANSFER_TO_FIXED_LENGTH).as_ref() == Some(callee) {
        return Some(transfer(agent, this, args, false));
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
