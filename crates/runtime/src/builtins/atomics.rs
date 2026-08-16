//! The Atomics namespace (spec 26.4): atomic read-modify-write operations on
//! the shared byte block of an Integer-Indexed view over a SharedArrayBuffer.
//! The runtime is single-agent (no real contention), so the accesses are
//! plain reads/writes; the agent's `[[CanBlock]]` is false, so `wait` throws
//! and `waitAsync` never suspends (its promise resolves immediately).

use crux::convert::{to_big_int64, to_big_uint64, to_index, to_number};
use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::{JsObject, ObjectKind, TypedArraySlots};
use crux::ops::same_value_zero;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::typed_array::{AtomicOp, ElementType, decode_element, encode_element};
use crux::value::Value;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::agent::Agent;
use crate::context::as_object;
use crate::realm::Realm;

const ATOMICS: &str = "%Atomics%";
const ATOMICS_ADD: &str = "%Atomics.add%";
const ATOMICS_AND: &str = "%Atomics.and%";
const ATOMICS_COMPARE_EXCHANGE: &str = "%Atomics.compareExchange%";
const ATOMICS_EXCHANGE: &str = "%Atomics.exchange%";
const ATOMICS_IS_LOCK_FREE: &str = "%Atomics.isLockFree%";
const ATOMICS_LOAD: &str = "%Atomics.load%";
const ATOMICS_NOTIFY: &str = "%Atomics.notify%";
const ATOMICS_OR: &str = "%Atomics.or%";
const ATOMICS_PAUSE: &str = "%Atomics.pause%";
const ATOMICS_STORE: &str = "%Atomics.store%";
const ATOMICS_SUB: &str = "%Atomics.sub%";
const ATOMICS_WAIT: &str = "%Atomics.wait%";
const ATOMICS_WAIT_ASYNC: &str = "%Atomics.waitAsync%";
const ATOMICS_XOR: &str = "%Atomics.xor%";

/// The integer element kinds Atomics accepts (spec 26.4.1): everything but
/// Uint8Clamped and the float kinds.
fn is_integer_type(element_type: ElementType) -> bool {
    !matches!(
        element_type,
        ElementType::Uint8Clamped
            | ElementType::Float16
            | ElementType::Float32
            | ElementType::Float64
    )
}

/// Atomics.wait/waitAsync only accept Int32 and BigInt64 (spec 26.4.14.3,
/// 26.4.15.3).
fn is_wait_type(element_type: ElementType) -> bool {
    matches!(element_type, ElementType::Int32 | ElementType::BigInt64)
}

fn typed_array_slots(value: &Value) -> Option<TypedArraySlots> {
    let Value::Object(obj) = value else {
        return None;
    };
    let ObjectKind::IntegerIndexed(slots) = &obj.kind else {
        return None;
    };
    Some(slots.as_ref().clone())
}

/// ValidateIntegerTypedArray (spec 26.4.1): an integer TypedArray whose
/// buffer is not detached or (for write access) immutable. The modern
/// Atomics operations also run on non-shared buffers; only `wait`/
/// `waitAsync` (which require a SharedArrayBuffer) and `notify` (which
/// returns 0) check sharing.
fn validate_integer_typed_array(
    agent: &mut Agent,
    args: &[Value],
    wait_type: bool,
    write: bool,
) -> Result<TypedArraySlots, JsError> {
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    let slots = typed_array_slots(&value).ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "Method called on an incompatible receiver".into(),
        )
    })?;
    let slots = slots.clone();
    if !is_integer_type(slots.element_type) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Atomics requires an integer TypedArray".into(),
        ));
    }
    if wait_type && !is_wait_type(slots.element_type) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Atomics.wait/waitAsync require an Int32Array or BigInt64Array".into(),
        ));
    }
    let buffer_id = as_object(&slots.buffer_object)
        .map(|object| object.id())
        .unwrap_or(u64::MAX);
    if !agent.buffer_data.contains_key(&buffer_id)
        || crate::builtins::array_buffer::is_detached(agent, buffer_id)
    {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray buffer is detached".into(),
        ));
    }
    // ValidateTypedArray (spec 26.4.1): a write through an immutable buffer
    // throws before any argument is read (immutable-buffer.js).
    if write
        && agent
            .buffer_data
            .get(&buffer_id)
            .is_some_and(|cell| cell.borrow().immutable)
    {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Atomics operation on an immutable buffer".into(),
        ));
    }
    Ok(slots)
}

/// ValidateAtomicAccess (spec 26.4.1.1): the clamped byte offset of element
/// `request_index`.
fn atomic_offset(slots: &TypedArraySlots, args: &[Value]) -> Result<usize, JsError> {
    let index = to_index(&args.get(1).cloned().unwrap_or(Value::Undefined))? as usize;
    if index >= slots.array_length {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "Atomics index out of bounds".into(),
        ));
    }
    Ok(slots.byte_offset + index * slots.element_type.size())
}

/// Read the element at `offset` atomically (spec 26.4.2 GetValueFromBuffer
/// with the atomic read mode).
fn read_element(slots: &TypedArraySlots, offset: usize) -> Result<Value, JsError> {
    let size = slots.element_type.size();
    let raw = slots.buffer.atomic_load(offset, size)?;
    raw_element(slots.element_type, raw)
}

/// Write `value` (already converted to the element type) at `offset`
/// atomically (spec SetValueInBuffer with the atomic write mode).
fn write_element(slots: &TypedArraySlots, offset: usize, value: &Value) -> Result<(), JsError> {
    let size = slots.element_type.size();
    let raw = element_raw(slots.element_type, value)?;
    slots.buffer.atomic_store(offset, size, raw)
}

/// The native-order integer the element's bytes encode (the raw word the
/// Atomics operations read-modify-write).
fn element_raw(element_type: ElementType, value: &Value) -> Result<u64, JsError> {
    let bytes = encode_element(element_type, value)?;
    let mut buf = [0u8; 8];
    buf[..bytes.len()].copy_from_slice(&bytes);
    Ok(u64::from_ne_bytes(buf))
}

/// The element value stored in the raw word (the inverse of `element_raw`).
fn raw_element(element_type: ElementType, raw: u64) -> Result<Value, JsError> {
    let size = element_type.size();
    let bytes = raw.to_ne_bytes()[..size].to_vec();
    decode_element(element_type, &bytes, 0)
}

/// The convert-then-store used by `store` (spec 26.4.11): the element type
/// decides the conversion, and the converted value is returned.
fn converted_value(slots: &TypedArraySlots, value: &Value) -> Result<Value, JsError> {
    if matches!(
        slots.element_type,
        ElementType::BigInt64 | ElementType::BigUint64
    ) {
        let big = match slots.element_type {
            ElementType::BigInt64 => to_big_int64(value)?,
            _ => to_big_uint64(value)?,
        };
        Ok(Value::BigInt(Handle::new(big)))
    } else {
        Ok(Value::Number(to_number(value)?))
    }
}

/// The atomic read-modify-write of `op`: convert the operand to the raw
/// word, perform the RMW, and decode the old value. BigInt and Number
/// operands both funnel through `element_raw`, which wraps to the element
/// type (two's complement for the signed kinds).
fn raw_rmw(
    slots: &TypedArraySlots,
    offset: usize,
    operand: &Value,
    op: AtomicOp,
    expected: Option<&Value>,
) -> Result<Value, JsError> {
    let size = slots.element_type.size();
    let operand_raw = element_raw(slots.element_type, operand)?;
    let expected_raw = match expected {
        Some(value) => Some(element_raw(slots.element_type, value)?),
        None => None,
    };
    let old = slots
        .buffer
        .atomic_rmw(op, offset, size, operand_raw, expected_raw)?;
    raw_element(slots.element_type, old)
}

/// Atomics.add/sub/and/or/xor (spec 26.4.3-8): the read-modify-write ops.
fn binary_op(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    op: AtomicOp,
) -> Result<Value, JsError> {
    let _ = this;
    let slots = validate_integer_typed_array(agent, args, false, true)?;
    let offset = atomic_offset(&slots, args)?;
    let operand = args.get(2).cloned().unwrap_or(Value::Undefined);
    raw_rmw(&slots, offset, &operand, op, None)
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

/// Atomics.load (spec 26.4.9).
fn load(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let _ = this;
    // A read: immutable buffers are readable.
    let slots = validate_integer_typed_array(agent, args, false, false)?;
    let offset = atomic_offset(&slots, args)?;
    read_element(&slots, offset)
}

/// Atomics.isLockFree (spec 26.4.10).
fn is_lock_free(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let _ = (agent, this);
    let size = to_number(&args.first().cloned().unwrap_or(Value::Undefined))?;
    let free =
        matches!(size, 1.0 | 2.0 | 4.0) || (size == 8.0 && cfg!(target_pointer_width = "64"));
    Ok(Value::Boolean(free))
}

/// Atomics.store (spec 26.4.11).
fn store(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let _ = this;
    let slots = validate_integer_typed_array(agent, args, false, true)?;
    let offset = atomic_offset(&slots, args)?;
    let value = args.get(2).cloned().unwrap_or(Value::Undefined);
    let converted = converted_value(&slots, &value)?;
    write_element(&slots, offset, &converted)?;
    // spec 26.4.11 step 8: the returned value is ToIntegerOrInfinity of the
    // input (normalizing -0 to +0, spec 26.4.1.6).
    match converted {
        Value::Number(_) => Ok(Value::Number(crux::convert::to_integer_or_infinity(
            crate::context::to_number(agent, &value)?,
        ))),
        // spec 26.4.11 step 8: the returned value is ToBigInt (unwrapped,
        // unlike the stored element).
        Value::BigInt(_) => Ok(Value::BigInt(Handle::new(crate::context::to_big_int(
            agent, &value,
        )?))),
        _ => unreachable!("store converts to Number or BigInt"),
    }
}

/// Atomics.exchange (spec 26.4.5): store the new value, return the old.
fn exchange(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let _ = this;
    let slots = validate_integer_typed_array(agent, args, false, true)?;
    let offset = atomic_offset(&slots, args)?;
    let value = args.get(2).cloned().unwrap_or(Value::Undefined);
    raw_rmw(&slots, offset, &value, AtomicOp::Exchange, None)
}

/// Atomics.compareExchange (spec 26.4.4): compare the current element to
/// `expected` and store `replacement` when equal; return the old value.
fn compare_exchange(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let _ = this;
    let slots = validate_integer_typed_array(agent, args, false, true)?;
    let offset = atomic_offset(&slots, args)?;
    let expected = args.get(2).cloned().unwrap_or(Value::Undefined);
    let replacement = args.get(3).cloned().unwrap_or(Value::Undefined);
    raw_rmw(
        &slots,
        offset,
        &replacement,
        AtomicOp::CompareExchange,
        Some(&expected),
    )
}

/// The wait registry: pending `Atomics.wait` suspensions keyed by
/// (byte-block, byte offset), so `notify` on any agent can wake them.
struct WaiterEvent {
    condvar: Condvar,
    notified: Mutex<bool>,
    /// For `waitAsync` waiters: the `agent.wait_async` key of the resolve
    /// function, resolved to *"ok"* by `notify` (a blocking `wait` event has
    /// `None`).
    async_key: Option<u64>,
}

type WaitQueue = VecDeque<Arc<WaiterEvent>>;
type WaitRegistry = HashMap<(usize, usize), WaitQueue>;

impl WaiterEvent {
    fn new() -> Self {
        WaiterEvent {
            condvar: Condvar::new(),
            notified: Mutex::new(false),
            async_key: None,
        }
    }
}

fn registry() -> &'static Mutex<WaitRegistry> {
    static WAIT_REGISTRY: OnceLock<Mutex<WaitRegistry>> = OnceLock::new();
    WAIT_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Atomics.notify (spec 26.4.13): wake up to `count` waiting agents on the
/// byte offset and return how many were woken. A non-shared buffer returns 0
/// after the index/count coercions (spec 26.4.13 steps 2-7).
fn notify(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let _ = this;
    // ValidateIntegerTypedArray(typedArray, true): Int32 and BigInt64 only.
    let slots = validate_integer_typed_array(agent, args, true, false)?;
    let offset = atomic_offset(&slots, args)?;
    let count_arg = args.get(2).cloned().unwrap_or(Value::Undefined);
    // spec 26.4.13 step 3: an undefined count is +∞ (wakes all waiters);
    // otherwise ToIntegerOrInfinity, whose NaN maps to 0.
    let count = if matches!(count_arg, Value::Undefined) {
        f64::INFINITY
    } else {
        crux::convert::to_integer_or_infinity(crate::context::to_number(agent, &count_arg)?)
    };
    let count = if count.is_infinite() {
        usize::MAX
    } else {
        count.max(0.0) as usize
    };
    if !crate::builtins::array_buffer::is_shared(agent, &slots.buffer_object) {
        return Ok(Value::Number(0.0));
    }
    let key = (slots.buffer.block_id(), offset);
    let mut events = Vec::new();
    {
        let mut registry = registry()
            .lock()
            .map_err(|_| JsError::new(ErrorKind::TypeError, "wait registry poisoned".into()))?;
        if let Some(queue) = registry.get_mut(&key) {
            for _ in 0..count {
                let Some(event) = queue.pop_front() else {
                    break;
                };
                events.push(event);
            }
        }
    }
    for event in &events {
        // A waitAsync waiter's promise resolves *"ok"* (spec 26.4.15 DoWait
        // step 20); the blocking `wait` path below is woken by the flag.
        if let Some(key) = event.async_key
            && let Some(resolve) = agent.wait_async.remove(&key)
        {
            crate::function::call(agent, &resolve, Value::Undefined, &[str("ok")])?;
        }
        let mut notified = event
            .notified
            .lock()
            .map_err(|_| JsError::new(ErrorKind::TypeError, "wait registry poisoned".into()))?;
        *notified = true;
        drop(notified);
        event.condvar.notify_one();
    }
    Ok(Value::Number(events.len() as f64))
}

/// The raw expected value of a wait/waitAsync: the Int32/BigInt64 element
/// encoding of `requested`.
fn wait_expected_raw(slots: &TypedArraySlots, requested: &Value) -> Result<u64, JsError> {
    match slots.element_type {
        ElementType::Int32 => {
            element_raw(ElementType::Int32, &Value::Number(to_number(requested)?))
        }
        _ => element_raw(
            ElementType::BigInt64,
            &Value::BigInt(Handle::new(to_big_int64(requested)?)),
        ),
    }
}

/// Atomics.wait (spec 26.4.14): the agent suspends until the value changes,
/// a notify arrives, or the timeout expires. The shared-buffer check runs
/// before any argument is read (ValidateSharedIntegerTypedArray), and agents
/// with [[CanBlock]] false throw before the zero-timeout fast path.
fn wait(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let _ = this;
    let slots = validate_integer_typed_array(agent, args, true, false)?;
    // spec 26.4.14 step 9: wait requires a SharedArrayBuffer (checked
    // before the index/value/timeout coercions, non-shared-bufferdata-throws).
    if !crate::builtins::array_buffer::is_shared(agent, &slots.buffer_object) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Atomics.wait on a non-shared buffer".into(),
        ));
    }
    let offset = atomic_offset(&slots, args)?;
    let requested = args.get(2).cloned().unwrap_or(Value::Undefined);
    let requested_raw = wait_expected_raw(&slots, &requested)?;
    let timeout_arg = args.get(3).cloned().unwrap_or(Value::Number(f64::INFINITY));
    let timeout_ms = crate::context::to_number(agent, &timeout_arg)?;
    // DoWait step 5: a NaN timeout is +∞ (spec: "If q is NaN, let t be +∞,
    // else let t be max(q, 0)").
    let timeout_ms = if timeout_ms.is_nan() {
        f64::INFINITY
    } else {
        timeout_ms.max(0.0)
    };
    let size = slots.element_type.size();
    let key = (slots.buffer.block_id(), offset);
    // spec steps 13-14: a value mismatch is "not-equal".
    let current = slots.buffer.atomic_load(offset, size)?;
    if current != requested_raw {
        return Ok(str("not-equal"));
    }
    // spec steps 16-17: an agent that cannot suspend throws even for a
    // zero timeout (cannot-suspend-throws.js).
    if !agent.can_block {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Atomics.wait cannot be called on the main thread".into(),
        ));
    }
    // spec step 15: a zero timeout returns "timed-out" without suspending.
    if timeout_ms == 0.0 {
        return Ok(str("timed-out"));
    }
    let deadline = if timeout_ms.is_infinite() {
        None
    } else {
        Some(Instant::now() + Duration::from_millis(timeout_ms.max(0.0) as u64))
    };
    let status = loop {
        let current = slots.buffer.atomic_load(offset, size)?;
        if current != requested_raw {
            break "not-equal";
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break "timed-out";
        }
        // Register for this suspension; notify may pop the event before we
        // start waiting, in which case the flag lets us re-check the value.
        let event = Arc::new(WaiterEvent::new());
        registry()
            .lock()
            .map_err(|_| JsError::new(ErrorKind::TypeError, "wait registry poisoned".into()))?
            .entry(key)
            .or_default()
            .push_back(event.clone());
        let mut notified = event
            .notified
            .lock()
            .map_err(|_| JsError::new(ErrorKind::TypeError, "wait registry poisoned".into()))?;
        while !*notified {
            let remaining = match deadline {
                Some(deadline) => deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(0)),
                None => Duration::MAX,
            };
            let (guard, _) = event
                .condvar
                .wait_timeout(notified, remaining)
                .map_err(|_| JsError::new(ErrorKind::TypeError, "wait registry poisoned".into()))?;
            notified = guard;
            if *notified || deadline.is_some_and(|d| Instant::now() >= d) {
                break;
            }
        }
        *notified = false;
        drop(notified);
        // Remove this suspension's event (already popped by notify, or self).
        if let Ok(mut registry) = registry().lock()
            && let Some(queue) = registry.get_mut(&key)
        {
            queue.retain(|e| !Arc::ptr_eq(e, &event));
        }
    };
    Ok(str(status))
}

/// Atomics.waitAsync (spec 26.4.15): non-blocking. An immediate outcome
/// (value mismatch, or a match with a zero timeout) returns
/// `{ async: false, value: "not-equal" | "timed-out" }`; a match with a
/// positive timeout would wait for a notify the runtime cannot deliver, so it
/// returns `{ async: true, value: promise }` left pending.
fn wait_async(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let _ = this;
    let slots = validate_integer_typed_array(agent, args, true, false)?;
    // spec 26.4.14 step 9: waitAsync requires a SharedArrayBuffer (checked
    // before the argument coercions, non-shared-bufferdata-throws).
    if !crate::builtins::array_buffer::is_shared(agent, &slots.buffer_object) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Atomics.waitAsync on a non-shared buffer".into(),
        ));
    }
    let offset = atomic_offset(&slots, args)?;
    let requested = args.get(2).cloned().unwrap_or(Value::Undefined);
    let current = read_element(&slots, offset)?;
    let requested_value = match slots.element_type {
        ElementType::Int32 => Value::Number(to_number(&requested)?),
        _ => {
            let big = to_big_int64(&requested)?;
            Value::BigInt(Handle::new(big))
        }
    };
    let timeout_arg = args.get(3).cloned().unwrap_or(Value::Number(f64::INFINITY));
    let timeout_ms = crate::context::to_number(agent, &timeout_arg)?;
    // DoWait step 5: a NaN timeout is +∞.
    let timeout_ms = if timeout_ms.is_nan() {
        f64::INFINITY
    } else {
        timeout_ms.max(0.0)
    };
    let object_proto = agent
        .current_realm()?
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    // spec DoWait steps 15-17: a mismatch is "not-equal"; a match with a
    // zero timeout is "timed-out"; both are immediate (async: false).
    if !same_value_zero(&current, &requested_value) || timeout_ms == 0.0 {
        let status = if same_value_zero(&current, &requested_value) {
            "timed-out"
        } else {
            "not-equal"
        };
        let result = JsObject::ordinary_object_create(object_proto);
        result
            .create_data_property_or_throw(&JsString::from_utf8("async"), Value::Boolean(false))?;
        result.create_data_property_or_throw(&JsString::from_utf8("value"), str(status))?;
        return Ok(Value::Object(result));
    }
    // A match with a positive timeout: register with the wait registry so a
    // later `Atomics.notify` resolves the promise with *"ok"* (spec 26.4.15
    // DoWait step 20); a finite timeout also schedules a "timed-out"
    // resolution. Reported as async: true.
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let capability = crate::promise::new_promise_capability(agent, &promise_ctor)?;
    let resolve_id = match &capability.resolve {
        Value::Function(function) => function.id(),
        _ => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "waitAsync resolve is not a function".into(),
            ));
        }
    };
    agent
        .wait_async
        .insert(resolve_id, capability.resolve.clone());
    let key = (slots.buffer.block_id(), offset);
    let event = Arc::new(WaiterEvent {
        condvar: Condvar::new(),
        notified: Mutex::new(false),
        async_key: Some(resolve_id),
    });
    registry()
        .lock()
        .map_err(|_| JsError::new(ErrorKind::TypeError, "wait registry poisoned".into()))?
        .entry(key)
        .or_default()
        .push_back(event.clone());
    if timeout_ms.is_finite() {
        let realm = agent.current_realm().ok();
        let event = event.clone();
        agent.enqueue_timeout_job(realm, timeout_ms as u64, move |agent| {
            // A notify may already have resolved the wait and removed the
            // entry; the timeout is then a no-op.
            if let Some(resolve) = agent.wait_async.remove(&resolve_id) {
                crate::function::call(agent, &resolve, Value::Undefined, &[str("timed-out")])?;
            }
            if let Ok(mut registry) = registry().lock()
                && let Some(queue) = registry.get_mut(&key)
            {
                queue.retain(|e| !Arc::ptr_eq(e, &event));
            }
            Ok(Value::Undefined)
        });
    }
    let result = JsObject::ordinary_object_create(object_proto);
    result.create_data_property_or_throw(&JsString::from_utf8("async"), Value::Boolean(true))?;
    result.create_data_property_or_throw(&JsString::from_utf8("value"), capability.promise)?;
    Ok(Value::Object(result))
}

/// Atomics.pause (spec 26.4.12): a hint that yields CPU; a no-op here.
fn pause(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let _ = (agent, this);
    to_number(&args.first().cloned().unwrap_or(Value::Undefined))?;
    Ok(Value::Undefined)
}

pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let atomics = JsObject::ordinary_object_create(
        realm
            .intrinsics
            .get("%Object.prototype%")
            .and_then(|value| as_object(&value)),
    );
    realm
        .intrinsics
        .define(ATOMICS, Value::Object(atomics.clone()));

    let methods: [(&str, &str, u64); 14] = [
        ("add", ATOMICS_ADD, 3),
        ("and", ATOMICS_AND, 3),
        ("compareExchange", ATOMICS_COMPARE_EXCHANGE, 4),
        ("exchange", ATOMICS_EXCHANGE, 3),
        ("isLockFree", ATOMICS_IS_LOCK_FREE, 1),
        ("load", ATOMICS_LOAD, 2),
        ("notify", ATOMICS_NOTIFY, 3),
        ("or", ATOMICS_OR, 3),
        ("pause", ATOMICS_PAUSE, 0),
        ("store", ATOMICS_STORE, 3),
        ("sub", ATOMICS_SUB, 3),
        ("wait", ATOMICS_WAIT, 4),
        ("waitAsync", ATOMICS_WAIT_ASYNC, 4),
        ("xor", ATOMICS_XOR, 3),
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
        atomics.define_property(
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
    atomics.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(str("Atomics")),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("Atomics"),
        &PropertyDescriptor {
            value: Some(Value::Object(atomics)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// The Atomics members that need the agent, dispatched by intrinsic identity
/// from `runtime::function::call`.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(ATOMICS_ADD).as_ref() == Some(callee) {
        return Some(binary_op(agent, this, args, AtomicOp::Add));
    }
    if intrinsics.get(ATOMICS_SUB).as_ref() == Some(callee) {
        return Some(binary_op(agent, this, args, AtomicOp::Sub));
    }
    if intrinsics.get(ATOMICS_AND).as_ref() == Some(callee) {
        return Some(binary_op(agent, this, args, AtomicOp::And));
    }
    if intrinsics.get(ATOMICS_OR).as_ref() == Some(callee) {
        return Some(binary_op(agent, this, args, AtomicOp::Or));
    }
    if intrinsics.get(ATOMICS_XOR).as_ref() == Some(callee) {
        return Some(binary_op(agent, this, args, AtomicOp::Xor));
    }
    if intrinsics.get(ATOMICS_LOAD).as_ref() == Some(callee) {
        return Some(load(agent, this, args));
    }
    if intrinsics.get(ATOMICS_IS_LOCK_FREE).as_ref() == Some(callee) {
        return Some(is_lock_free(agent, this, args));
    }
    if intrinsics.get(ATOMICS_STORE).as_ref() == Some(callee) {
        return Some(store(agent, this, args));
    }
    if intrinsics.get(ATOMICS_EXCHANGE).as_ref() == Some(callee) {
        return Some(exchange(agent, this, args));
    }
    if intrinsics.get(ATOMICS_COMPARE_EXCHANGE).as_ref() == Some(callee) {
        return Some(compare_exchange(agent, this, args));
    }
    if intrinsics.get(ATOMICS_NOTIFY).as_ref() == Some(callee) {
        return Some(notify(agent, this, args));
    }
    if intrinsics.get(ATOMICS_WAIT).as_ref() == Some(callee) {
        return Some(wait(agent, this, args));
    }
    if intrinsics.get(ATOMICS_WAIT_ASYNC).as_ref() == Some(callee) {
        return Some(wait_async(agent, this, args));
    }
    if intrinsics.get(ATOMICS_PAUSE).as_ref() == Some(callee) {
        return Some(pause(agent, this, args));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::evaluate;

    fn run(source: &str) -> Result<Value, JsError> {
        evaluate(source)
    }

    fn number(value: f64) -> Value {
        Value::Number(value)
    }

    #[test]
    fn atomic_rmw_semantics() {
        // store returns the converted value; load reads it back.
        assert_eq!(
            run("const ta = new Int32Array(new SharedArrayBuffer(16)); Atomics.store(ta, 0, 5); Atomics.load(ta, 0)")
                .unwrap(),
            number(5.0)
        );
        // The RMW ops return the OLD value.
        assert_eq!(
            run("const ta = new Int32Array(new SharedArrayBuffer(16)); Atomics.store(ta, 0, 5); Atomics.add(ta, 0, 3)")
                .unwrap(),
            number(5.0)
        );
        assert_eq!(
            run("const ta = new Int32Array(new SharedArrayBuffer(16)); Atomics.store(ta, 0, 5); Atomics.add(ta, 0, 3); Atomics.load(ta, 0)")
                .unwrap(),
            number(8.0)
        );
        assert_eq!(
            run("const ta = new Int32Array(new SharedArrayBuffer(16)); Atomics.store(ta, 0, 8); Atomics.sub(ta, 0, 3); Atomics.load(ta, 0)")
                .unwrap(),
            number(5.0)
        );
        assert_eq!(
            run("const ta = new Int32Array(new SharedArrayBuffer(16)); Atomics.store(ta, 0, 0xFF); Atomics.and(ta, 0, 0x0F); Atomics.load(ta, 0)")
                .unwrap(),
            number(15.0)
        );
        assert_eq!(
            run("const ta = new Int32Array(new SharedArrayBuffer(16)); Atomics.store(ta, 0, 0x0F); Atomics.or(ta, 0, 0xF0); Atomics.load(ta, 0)")
                .unwrap(),
            number(255.0)
        );
        assert_eq!(
            run("const ta = new Int32Array(new SharedArrayBuffer(16)); Atomics.store(ta, 0, 0xFF); Atomics.xor(ta, 0, 0x0F); Atomics.load(ta, 0)")
                .unwrap(),
            number(240.0)
        );
        // exchange stores the new value and returns the old.
        assert_eq!(
            run("const ta = new Int32Array(new SharedArrayBuffer(16)); Atomics.store(ta, 0, 5); [Atomics.exchange(ta, 0, 9), Atomics.load(ta, 0)].join(',')")
                .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("5,9")))
        );
        // compareExchange replaces only when equal, returning the old value.
        assert_eq!(
            run("const ta = new Int32Array(new SharedArrayBuffer(16)); Atomics.store(ta, 0, 5); [Atomics.compareExchange(ta, 0, 5, 9), Atomics.load(ta, 0)].join(',')")
                .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("5,9")))
        );
        assert_eq!(
            run("const ta = new Int32Array(new SharedArrayBuffer(16)); Atomics.store(ta, 0, 5); [Atomics.compareExchange(ta, 0, 6, 9), Atomics.load(ta, 0)].join(',')")
                .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("5,5")))
        );
        // Int32 wrapping is two's complement.
        assert_eq!(
            run("const ta = new Int32Array(new SharedArrayBuffer(16)); Atomics.store(ta, 0, 2147483647); Atomics.add(ta, 0, 1); Atomics.load(ta, 0)")
                .unwrap(),
            number(-2147483648.0)
        );
    }

    #[test]
    fn atomic_rmw_bigint64() {
        assert_eq!(
            run("const ta = new BigInt64Array(new SharedArrayBuffer(16)); Atomics.store(ta, 0, 5n); Atomics.add(ta, 0, 3n)")
                .unwrap(),
            run("5n").unwrap()
        );
        assert_eq!(
            run("const ta = new BigInt64Array(new SharedArrayBuffer(16)); Atomics.store(ta, 0, 5n); Atomics.add(ta, 0, 3n); Atomics.load(ta, 0)")
                .unwrap(),
            run("8n").unwrap()
        );
        assert_eq!(
            run("const ta = new BigInt64Array(new SharedArrayBuffer(16)); Atomics.store(ta, 0, 5n); [Atomics.compareExchange(ta, 0, 5n, 9n), Atomics.load(ta, 0)].join(',')")
                .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("5,9")))
        );
    }

    #[test]
    fn atomics_reject_invalid_receivers() {
        // The RMW/read/write ops run on non-shared buffers (the modern spec).
        assert_eq!(
            run("Atomics.load(new Int32Array(4), 0)").unwrap(),
            Value::Number(0.0)
        );
        // Float element kinds are rejected.
        assert!(run("Atomics.add(new Float64Array(new SharedArrayBuffer(16)), 0, 1)").is_err());
        // wait/waitAsync accept only Int32/BigInt64.
        assert!(run("Atomics.wait(new Uint8Array(new SharedArrayBuffer(4)), 0, 0)").is_err());
        assert!(run("Atomics.waitAsync(new Uint8Array(new SharedArrayBuffer(4)), 0, 0)").is_err());
        // wait on the main thread ([[CanBlock]] false) throws — even for a
        // zero timeout (cannot-suspend-throws.js).
        assert!(run("Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 0)").is_err());
        assert!(run("Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100)").is_err());
        // notify accepts Int32 and BigInt64 (returns 0 with no waiters).
        assert_eq!(
            run("Atomics.notify(new Int32Array(new SharedArrayBuffer(4)), 0)").unwrap(),
            Value::Number(0.0)
        );
        assert_eq!(
            run("Atomics.notify(new BigInt64Array(new SharedArrayBuffer(16)), 0)").unwrap(),
            Value::Number(0.0)
        );
        // notify on a non-shared buffer returns 0.
        assert_eq!(
            run("Atomics.notify(new Int32Array(4), 0)").unwrap(),
            Value::Number(0.0)
        );
    }

    #[test]
    fn wait_async_returns_async_promise() {
        // Immediate outcomes are reported as { async: false, value: string }:
        // "timed-out" for a match, "not-equal" for a mismatch.
        assert_eq!(
            run(
                "const ta = new Int32Array(new SharedArrayBuffer(4)); Atomics.store(ta, 0, 0); Atomics.waitAsync(ta, 0, 0, 0).async"
            )
            .unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            run(
                "const ta = new Int32Array(new SharedArrayBuffer(4)); Atomics.store(ta, 0, 0); Atomics.waitAsync(ta, 0, 0, 0).value"
            )
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("timed-out")))
        );
        assert_eq!(
            run(
                "const ta = new Int32Array(new SharedArrayBuffer(4)); Atomics.store(ta, 0, 1); Atomics.waitAsync(ta, 0, 0, 0).value"
            )
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("not-equal")))
        );
    }

    #[test]
    fn notify_on_main_thread_returns_zero() {
        assert_eq!(
            run("Atomics.notify(new Int32Array(new SharedArrayBuffer(4)), 0)").unwrap(),
            number(0.0)
        );
        assert_eq!(
            run("Atomics.notify(new Int32Array(new SharedArrayBuffer(4)), 0, 5)").unwrap(),
            number(0.0)
        );
    }
}
