//! The Atomics namespace (spec 26.4): atomic read-modify-write operations on
//! the shared byte block of an Integer-Indexed view over a SharedArrayBuffer.
//! The runtime is single-agent (no real contention), so the accesses are
//! plain reads/writes; the agent's `[[CanBlock]]` is false, so `wait` throws
//! and `waitAsync` never suspends (its promise resolves immediately).

use crux::bigint;
use crux::convert::{to_big_int64, to_big_uint64, to_index, to_number};
use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::{JsObject, ObjectKind, TypedArraySlots};
use crux::ops::same_value_zero;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::typed_array::{ElementType, decode_element, encode_element};
use crux::value::Value;

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

/// ValidateIntegerTypedArray (spec 26.4.1) + the SharedArrayBuffer
/// requirement shared by every Atomics method (spec 26.4.2 step 2).
fn validate_shared_typed_array(
    agent: &mut Agent,
    args: &[Value],
    wait_type: bool,
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
    if !crate::builtins::array_buffer::is_shared(agent, &slots.buffer_object) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Atomics operation on a non-shared buffer".into(),
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

/// Read the element at `offset` (spec 26.4.2 GetValueFromBuffer).
fn read_element(slots: &TypedArraySlots, offset: usize) -> Result<Value, JsError> {
    let data = slots.buffer.0.borrow();
    decode_element(slots.element_type, &data, offset)
}

/// Write `value` (already converted to the element type) at `offset`.
fn write_element(slots: &TypedArraySlots, offset: usize, value: &Value) -> Result<(), JsError> {
    let bytes = encode_element(slots.element_type, value)?;
    let mut data = slots.buffer.0.borrow_mut();
    data[offset..offset + slots.element_type.size()].copy_from_slice(&bytes);
    Ok(())
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

/// The Number-side of a read-modify-write (spec 26.4.3-8): apply `op` to the
/// current element and the operand, both as Numbers; the wrapped result is
/// stored by `encode_element`.
fn number_rmw(
    slots: &TypedArraySlots,
    offset: usize,
    operand: f64,
    op: impl Fn(f64, f64) -> f64,
) -> Result<Value, JsError> {
    let current = to_number(&read_element(slots, offset)?)?;
    let next = op(current, operand);
    write_element(slots, offset, &Value::Number(next))?;
    Ok(Value::Number(current))
}

/// The BigInt-side of a read-modify-write: the operand converts with
/// ToBigInt64/ToBigUint64 and the result wraps through the element encode.
fn bigint_rmw(
    slots: &TypedArraySlots,
    offset: usize,
    operand: &Value,
    op: impl Fn(&crux::BigInt, &crux::BigInt) -> crux::BigInt,
) -> Result<Value, JsError> {
    let current = read_element(slots, offset)?;
    let current_big = match &current {
        Value::BigInt(big) => big.as_ref().clone(),
        _ => return Err(JsError::new(ErrorKind::TypeError, "BigInt expected".into())),
    };
    let operand_big = match slots.element_type {
        ElementType::BigInt64 => to_big_int64(operand)?,
        _ => to_big_uint64(operand)?,
    };
    let next = op(&current_big, &operand_big);
    write_element(slots, offset, &Value::BigInt(Handle::new(next)))?;
    Ok(current)
}

/// Atomics.add/sub/and/or/xor (spec 26.4.3-8): the read-modify-write ops.
fn binary_op(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    number_op: impl Fn(f64, f64) -> f64,
    bigint_op: impl Fn(&crux::BigInt, &crux::BigInt) -> crux::BigInt,
) -> Result<Value, JsError> {
    let _ = this;
    let slots = validate_shared_typed_array(agent, args, false)?;
    let offset = atomic_offset(&slots, args)?;
    let operand = args.get(2).cloned().unwrap_or(Value::Undefined);
    match slots.element_type {
        ElementType::BigInt64 | ElementType::BigUint64 => {
            bigint_rmw(&slots, offset, &operand, bigint_op)
        }
        _ => number_rmw(&slots, offset, to_number(&operand)?, number_op),
    }
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
    let slots = validate_shared_typed_array(agent, args, false)?;
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
    let slots = validate_shared_typed_array(agent, args, false)?;
    let offset = atomic_offset(&slots, args)?;
    let value = args.get(2).cloned().unwrap_or(Value::Undefined);
    let converted = converted_value(&slots, &value)?;
    write_element(&slots, offset, &converted)?;
    Ok(converted)
}

/// Atomics.exchange (spec 26.4.5): store the new value, return the old.
fn exchange(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let _ = this;
    let slots = validate_shared_typed_array(agent, args, false)?;
    let offset = atomic_offset(&slots, args)?;
    let old = read_element(&slots, offset)?;
    let value = args.get(2).cloned().unwrap_or(Value::Undefined);
    let converted = converted_value(&slots, &value)?;
    write_element(&slots, offset, &converted)?;
    Ok(old)
}

/// Atomics.compareExchange (spec 26.4.4): compare the current element to
/// `expected` and store `replacement` when equal; return the old value.
fn compare_exchange(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let _ = this;
    let slots = validate_shared_typed_array(agent, args, false)?;
    let offset = atomic_offset(&slots, args)?;
    let expected = args.get(2).cloned().unwrap_or(Value::Undefined);
    let replacement = args.get(3).cloned().unwrap_or(Value::Undefined);
    let old = read_element(&slots, offset)?;
    let expected_value = match slots.element_type {
        ElementType::BigInt64 | ElementType::BigUint64 => {
            let big = match slots.element_type {
                ElementType::BigInt64 => to_big_int64(&expected)?,
                _ => to_big_uint64(&expected)?,
            };
            Value::BigInt(Handle::new(big))
        }
        _ => Value::Number(to_number(&expected)?),
    };
    if same_value_zero(&old, &expected_value) {
        let converted = converted_value(&slots, &replacement)?;
        write_element(&slots, offset, &converted)?;
    }
    Ok(old)
}

/// Atomics.notify (spec 26.4.13): wake waiting agents. There are no waiters
/// (the runtime never suspends), so the count is always 0.
fn notify(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let _ = this;
    let slots = validate_shared_typed_array(agent, args, false)?;
    if slots.element_type != ElementType::Int32 {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Atomics.notify requires an Int32Array".into(),
        ));
    }
    atomic_offset(&slots, args)?;
    Ok(Value::Number(0.0))
}

/// Atomics.wait (spec 26.4.14): the agent cannot block, so the wait is
/// rejected before suspending.
fn wait(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let _ = this;
    let slots = validate_shared_typed_array(agent, args, true)?;
    atomic_offset(&slots, args)?;
    match slots.element_type {
        ElementType::Int32 => {
            to_number(&args.get(2).cloned().unwrap_or(Value::Undefined))?;
        }
        _ => {
            to_big_int64(&args.get(2).cloned().unwrap_or(Value::Undefined))?;
        }
    }
    if !agent.can_block {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Atomics.wait cannot be called on the main thread".into(),
        ));
    }
    Ok(str("timed-out"))
}

/// Atomics.waitAsync (spec 26.4.15): non-blocking; returns
/// `{ async: true, value: promise }` on the main agent. A value mismatch
/// resolves the promise with "not-equal"; otherwise it resolves "ok" (the
/// runtime never actually suspends, so there is no later notify to await).
fn wait_async(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let _ = this;
    let slots = validate_shared_typed_array(agent, args, true)?;
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
    let status = if same_value_zero(&current, &requested_value) {
        "ok"
    } else {
        "not-equal"
    };
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let capability = crate::promise::new_promise_capability(agent, &promise_ctor)?;
    crate::promise::resolve_promise(agent, &capability.promise, str(status))?;
    let result = JsObject::ordinary_object_create(
        agent
            .current_realm()?
            .intrinsics
            .get("%Object.prototype%")
            .and_then(|value| as_object(&value)),
    );
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
        ("pause", ATOMICS_PAUSE, 1),
        ("store", ATOMICS_STORE, 3),
        ("sub", ATOMICS_SUB, 3),
        ("wait", ATOMICS_WAIT, 4),
        ("waitAsync", ATOMICS_WAIT_ASYNC, 2),
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
        return Some(binary_op(agent, this, args, |a, b| a + b, bigint::add));
    }
    if intrinsics.get(ATOMICS_SUB).as_ref() == Some(callee) {
        return Some(binary_op(agent, this, args, |a, b| a - b, bigint::subtract));
    }
    if intrinsics.get(ATOMICS_AND).as_ref() == Some(callee) {
        return Some(binary_op(
            agent,
            this,
            args,
            |a, b| (a as i64 & b as i64) as f64,
            bigint::bitwise_and,
        ));
    }
    if intrinsics.get(ATOMICS_OR).as_ref() == Some(callee) {
        return Some(binary_op(
            agent,
            this,
            args,
            |a, b| (a as i64 | b as i64) as f64,
            bigint::bitwise_or,
        ));
    }
    if intrinsics.get(ATOMICS_XOR).as_ref() == Some(callee) {
        return Some(binary_op(
            agent,
            this,
            args,
            |a, b| (a as i64 ^ b as i64) as f64,
            bigint::bitwise_xor,
        ));
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
