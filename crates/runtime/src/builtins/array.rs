//! The Array built-in (spec ch. 23.1): the constructor, the statics
//! (`isArray`/`of`/`from`/`fromAsync`), the full prototype surface, the
//! array iterator, and the `%Array.prototype%` link-up of runtime-created
//! arrays (literals, `Object.entries` pairs, RegExp match arrays, ...).

use std::cell::RefCell;
use std::rc::Rc;

use crux::convert::{
    require_object_coercible, to_boolean, to_integer_or_infinity, to_length, to_number, to_uint32,
};
use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::{JsObject, ObjectKind};
use crux::ops::{is_strictly_equal, same_value_zero};
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, is_callable, is_constructor};

use crate::agent::Agent;
use crate::context::{as_object, get_property};
use crate::expr::{IteratorRecord, get_method};
use crate::promise::PromiseCapability;
use crate::realm::Realm;

const ARRAY: &str = "%Array%";
const ARRAY_PROTO: &str = "%Array.prototype%";
const IS_ARRAY: &str = "%Array.isArray%";
const OF: &str = "%Array.of%";
const FROM: &str = "%Array.from%";
const FROM_ASYNC: &str = "%Array.fromAsync%";
const AT: &str = "%Array.prototype.at%";
const CONCAT: &str = "%Array.prototype.concat%";
const COPY_WITHIN: &str = "%Array.prototype.copyWithin%";
const ENTRIES: &str = "%Array.prototype.entries%";
const EVERY: &str = "%Array.prototype.every%";
const FILL: &str = "%Array.prototype.fill%";
const FILTER: &str = "%Array.prototype.filter%";
const FIND: &str = "%Array.prototype.find%";
const FIND_INDEX: &str = "%Array.prototype.findIndex%";
const FIND_LAST: &str = "%Array.prototype.findLast%";
const FIND_LAST_INDEX: &str = "%Array.prototype.findLastIndex%";
const FLAT: &str = "%Array.prototype.flat%";
const FLAT_MAP: &str = "%Array.prototype.flatMap%";
const FOR_EACH: &str = "%Array.prototype.forEach%";
const INCLUDES: &str = "%Array.prototype.includes%";
const INDEX_OF: &str = "%Array.prototype.indexOf%";
const JOIN: &str = "%Array.prototype.join%";
const KEYS: &str = "%Array.prototype.keys%";
const LAST_INDEX_OF: &str = "%Array.prototype.lastIndexOf%";
const MAP: &str = "%Array.prototype.map%";
const POP: &str = "%Array.prototype.pop%";
const PUSH: &str = "%Array.prototype.push%";
const REDUCE: &str = "%Array.prototype.reduce%";
const REDUCE_RIGHT: &str = "%Array.prototype.reduceRight%";
const REVERSE: &str = "%Array.prototype.reverse%";
const SHIFT: &str = "%Array.prototype.shift%";
const SLICE: &str = "%Array.prototype.slice%";
const SOME: &str = "%Array.prototype.some%";
const SORT: &str = "%Array.prototype.sort%";
const SPLICE: &str = "%Array.prototype.splice%";
const TO_LOCALE_STRING: &str = "%Array.prototype.toLocaleString%";
const TO_REVERSED: &str = "%Array.prototype.toReversed%";
const TO_SORTED: &str = "%Array.prototype.toSorted%";
const TO_SPLICED: &str = "%Array.prototype.toSpliced%";
const TO_STRING: &str = "%Array.prototype.toString%";
const UNSHIFT: &str = "%Array.prototype.unshift%";
const VALUES: &str = "%Array.prototype.values%";
const WITH: &str = "%Array.prototype.with%";
const ITERATOR: &str = "%Array.prototype[Symbol.iterator]%";
const SPECIES: &str = "%Array.prototype[Symbol.species]%";
const ARRAY_ITERATOR: &str = "%ArrayIteratorPrototype%";
const ARRAY_ITERATOR_NEXT: &str = "%ArrayIteratorPrototype.next%";

/// The [[ArrayIterationKind]] of an array iterator (spec 23.1.5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrayIterationKind {
    KeyValue = 0,
    Key = 1,
    Value = 2,
}

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

/// IsArray (spec 7.2.2): Array exotics and proxies whose target is an
/// array (recursively).
pub fn is_array(value: &Value) -> bool {
    match value {
        Value::Object(obj) => match &obj.kind {
            ObjectKind::Array => true,
            ObjectKind::Proxy(slots) => slots
                .target
                .borrow()
                .as_ref()
                .map(is_array)
                .unwrap_or(false),
            _ => false,
        },
        Value::Function(function) => matches!(function.object.kind, ObjectKind::Array),
        _ => false,
    }
}

/// IsArray (spec 7.2.2) that reports a revoked proxy as a TypeError (step
/// 3.a) instead of false.
pub fn is_array_or_throw(value: &Value) -> Result<bool, JsError> {
    match value {
        Value::Object(obj) => match &obj.kind {
            ObjectKind::Array => Ok(true),
            ObjectKind::Proxy(slots) => {
                let Some(target) = slots.target.borrow().as_ref().cloned() else {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Cannot perform operation on a revoked Proxy".into(),
                    ));
                };
                is_array_or_throw(&target)
            }
            _ => Ok(false),
        },
        Value::Function(function) => Ok(matches!(function.object.kind, ObjectKind::Array)),
        _ => Ok(false),
    }
}

/// ArrayCreate with the realm's `%Array.prototype%` (spec 10.4.2.2).
pub fn array_create(agent: &Agent, length: f64) -> Result<Handle<JsObject>, JsError> {
    let proto = agent
        .current_realm()?
        .intrinsics
        .get(ARRAY_PROTO)
        .and_then(|value| as_object(&value));
    JsObject::array_create(proto, length)
}

/// CreateArrayFromList (spec 7.3.15): a fresh `%Array.prototype%`-linked
/// array holding the values.
pub fn array_from_values(agent: &Agent, values: &[Value]) -> Result<Value, JsError> {
    let array = array_create(agent, values.len() as f64)?;
    for (index, value) in values.iter().enumerate() {
        array.create_data_property(&key(index as u64), value.clone())?;
    }
    Ok(Value::Object(array))
}

/// LengthOfArrayLike (spec 7.3.22).
fn length_of_array_like(agent: &mut Agent, value: &Value) -> Result<u64, JsError> {
    let length = get_property(agent, value, &JsString::from_utf8("length"), value.clone())?;
    Ok(to_length(crate::context::to_number(agent, &length)?))
}

/// The clamped end index of `slice`/`fill`/`copyWithin`: an omitted (or
/// undefined) argument means the full length; otherwise ToIntegerOrInfinity
/// clamped to `[0, len]`.
fn clamped_end(args: &[Value], len: u64) -> Result<u64, JsError> {
    match args.get(1) {
        None | Some(Value::Undefined) => Ok(len),
        Some(value) => {
            let n = to_integer_or_infinity(to_number(value)?);
            Ok(if n < 0.0 {
                (len as i64).saturating_add(n as i64).max(0) as u64
            } else {
                (n as u64).min(len)
            })
        }
    }
}

/// HasProperty (spec 7.3.13) on a language value.
fn has_property(value: &Value, name: &JsString) -> Result<bool, JsError> {
    match value {
        Value::Object(obj) => obj.has_property_key(&PropertyKey::from_js_string(name)),
        Value::Function(function) => function
            .object
            .has_property_key(&PropertyKey::from_js_string(name)),
        _ => Ok(false),
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

/// DeletePropertyOrThrow (spec 7.3.6).
fn delete_property_or_throw(value: &Value, name: &JsString) -> Result<(), JsError> {
    let deleted = match value {
        Value::Object(obj) => obj.delete(name)?,
        Value::Function(function) => function.object.delete(name)?,
        _ => true,
    };
    if deleted {
        Ok(())
    } else {
        Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot delete a non-configurable property".into(),
        ))
    }
}

/// The object half of a value (spec ToObject requires the caller to have
/// coerced primitives already).
fn object_of(value: &Value) -> Result<Handle<JsObject>, JsError> {
    match value {
        Value::Object(obj) => Ok(obj.clone()),
        Value::Function(_) => Err(JsError::new(
            ErrorKind::TypeError,
            "expected an object receiver".into(),
        )),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "expected an object receiver".into(),
        )),
    }
}

/// GetPrototypeFromConstructor (spec 10.1.14): `constructor.prototype` when
/// it is an object, else the realm's `%Array.prototype%`.
fn get_prototype_from_constructor(
    agent: &mut Agent,
    constructor: &Value,
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
                .get(ARRAY_PROTO)
                .and_then(|value| as_object(&value))
                .ok_or_else(|| {
                    JsError::new(
                        ErrorKind::TypeError,
                        "%Array.prototype% is not defined".into(),
                    )
                })?;
            Ok(default)
        }
    }
}

/// ArraySpeciesCreate (spec 9.4.2.3): the species constructor of
/// `original_array`, or a plain `%Array.prototype%`-linked ArrayCreate.
/// A null or primitive `constructor` throws; an object's `@@species` of
/// null/undefined falls back to ArrayCreate; anything else must be a
/// constructor.
fn array_species_create(
    agent: &mut Agent,
    original_array: &Value,
    length: f64,
) -> Result<Handle<JsObject>, JsError> {
    if !is_array(original_array) {
        return array_create(agent, length);
    }
    let mut c = get(agent, original_array, &JsString::from_utf8("constructor"))?;
    // spec 9.4.2.3 steps 6-7: a constructor from another realm that *is*
    // that realm's %Array% collapses to undefined — the foreign species
    // getter is skipped and the result uses this realm's prototype.
    if is_constructor(&c) {
        let realm_c = crate::context::get_function_realm(agent, &c).ok();
        let this_realm = agent.current_realm().ok();
        let different_realm = match (&this_realm, &realm_c) {
            (Some(this_realm), Some(realm_c)) => {
                this_realm.global_object.id() != realm_c.global_object.id()
            }
            _ => false,
        };
        let foreign_array = realm_c
            .as_ref()
            .and_then(|realm| realm.intrinsics.get(ARRAY))
            .is_some_and(|array| crux::ops::same_value(&array, &c));
        if different_realm && foreign_array {
            c = Value::Undefined;
        }
    }
    let species = match c {
        Value::Undefined => return array_create(agent, length),
        Value::Object(_) | Value::Function(_) => {
            let species_key =
                PropertyKey::Symbol(crux::symbol::well_known("species").as_ref().clone());
            crate::context::get_property_key(agent, &c, &species_key, c.clone())?
        }
        _ => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Array species constructor is not an object".into(),
            ));
        }
    };
    let ctor = match species {
        Value::Null | Value::Undefined => return array_create(agent, length),
        value if is_constructor(&value) => value,
        _ => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Array species constructor returned a non-constructible value".into(),
            ));
        }
    };
    let result = crate::function::construct(agent, &ctor, &[Value::Number(length)], &ctor)?;
    object_of(&result)
}

/// The Array constructor (spec 23.1.1.1), used for both call and construct.
fn array_construct(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    let proto = get_prototype_from_constructor(agent, new_target)?;
    if args.is_empty() {
        return Ok(Value::Object(JsObject::array_create(Some(proto), 0.0)?));
    }
    if args.len() == 1
        && let Value::Number(number) = args[0]
    {
        let int_length = to_uint32(number);
        if !same_value_zero(&Value::Number(int_length as f64), &Value::Number(number)) {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "Invalid array length".into(),
            ));
        }
        return Ok(Value::Object(JsObject::array_create(
            Some(proto),
            int_length as f64,
        )?));
    }
    let array = JsObject::array_create(Some(proto), args.len() as f64)?;
    for (index, item) in args.iter().enumerate() {
        array.create_data_property_or_throw(&key(index as u64), item.clone())?;
    }
    Ok(Value::Object(array))
}

fn array_call(agent: &mut Agent, args: &[Value]) -> Result<Value, JsError> {
    let ctor = agent
        .current_realm()?
        .intrinsics
        .get(ARRAY)
        .unwrap_or(Value::Undefined);
    crate::function::construct(agent, &ctor, args, &ctor)
}

/// spec 23.1.2.2 Array.isArray.
fn array_is_array(_agent: &mut Agent, _this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let result = args
        .first()
        .map(is_array_or_throw)
        .transpose()?
        .unwrap_or(false);
    Ok(Value::Boolean(result))
}

/// spec 23.1.2.3 Array.of.
/// spec 23.2.2.2 Array.of: the receiver is the constructor (not the
/// species); a non-constructor receiver makes a plain ArrayCreate.
fn array_of(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    // spec steps 3-5: Construct(C, « len ») or ArrayCreate(len).
    let length = args.len() as f64;
    let array = if is_constructor(this) {
        crate::function::construct(agent, this, &[Value::Number(length)], this)?
    } else {
        Value::Object(array_create(agent, length)?)
    };
    let obj = as_object(&array).ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "Array.of result is not an object".into(),
        )
    })?;
    // spec steps 7-8: CreateDataPropertyOrThrow per item, then Set the
    // length (which invokes an own length setter).
    for (index, item) in args.iter().enumerate() {
        obj.create_data_property_or_throw(&key(index as u64), item.clone())?;
    }
    obj.set(&JsString::from_utf8("length"), Value::Number(length), true)?;
    Ok(array)
}

/// spec 23.1.2.1 Array.from.
fn array_from(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let items = args.first().cloned().unwrap_or(Value::Undefined);
    let mapfn = args.get(1).cloned().unwrap_or(Value::Undefined);
    let this_arg = args.get(2).cloned().unwrap_or(Value::Undefined);
    let mapping = if matches!(mapfn, Value::Undefined) {
        false
    } else {
        if !is_callable(&mapfn) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Array.from: mapfn is not a function".into(),
            ));
        }
        true
    };
    let using_iterator = get_method(agent, &items, "@@iterator")?;
    if let Some(iterator_method) = using_iterator {
        // spec 23.1.2.2 step 4.a: the constructor is invoked (with no
        // arguments) before the iterator method runs.
        let array = if is_constructor(this) {
            crate::function::construct(agent, this, &[], this)?
        } else {
            Value::Object(array_create(agent, 0.0)?)
        };
        let iterator = crate::function::call(agent, &iterator_method, items.clone(), &[])?;
        if !matches!(iterator, Value::Object(_)) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Iterator is not an object".into(),
            ));
        }
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
        let iterator_record = IteratorRecord { iterator, next };
        let mut k = 0u64;
        loop {
            let next_value = match crate::expr::iterator_step(agent, &iterator_record)? {
                Some(value) => value,
                None => {
                    set_property(
                        &array,
                        &JsString::from_utf8("length"),
                        Value::Number(k as f64),
                    )?;
                    return Ok(array);
                }
            };
            // The mapfn receives « nextValue, k » and an abrupt completion
            // closes the iterator (spec 23.1.2.2 step 4.g).
            let mapped_value = if mapping {
                match crate::function::call(
                    agent,
                    &mapfn,
                    this_arg.clone(),
                    &[next_value, Value::Number(k as f64)],
                ) {
                    Ok(value) => value,
                    Err(error) => {
                        let _ = crate::expr::iterator_close_throw(agent, &iterator_record);
                        return Err(error);
                    }
                }
            } else {
                next_value
            };
            if let Err(error) =
                object_of(&array)?.create_data_property_or_throw(&key(k), mapped_value)
            {
                let _ = crate::expr::iterator_close_throw(agent, &iterator_record);
                return Err(error);
            }
            k += 1;
        }
    }
    let array_like = crate::context::to_object(agent, &items)?;
    let length = length_of_array_like(agent, &array_like)?;
    let array = if is_constructor(this) {
        crate::function::construct(agent, this, &[Value::Number(length as f64)], this)?
    } else {
        Value::Object(array_create(agent, length as f64)?)
    };
    for k in 0..length {
        let k_value = get(agent, &array_like, &key(k))?;
        let mapped_value = if mapping {
            crate::function::call(
                agent,
                &mapfn,
                this_arg.clone(),
                &[k_value, Value::Number(k as f64)],
            )?
        } else {
            k_value
        };
        object_of(&array)?.create_data_property_or_throw(&key(k), mapped_value)?;
    }
    set_property(
        &array,
        &JsString::from_utf8("length"),
        Value::Number(length as f64),
    )?;
    Ok(array)
}

/// spec 23.1.3.1 Array.prototype.at.
fn at(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let length = length_of_array_like(agent, &object)?;
    let relative = to_integer_or_infinity(to_number(
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
    get(agent, &object, &key(k))
}

/// IsConcatSpreadable (spec 23.1.3.2.2).
fn is_concat_spreadable(agent: &mut Agent, value: &Value) -> Result<bool, JsError> {
    if !matches!(value, Value::Object(_) | Value::Function(_)) {
        return Ok(false);
    }
    let spreadable_key = PropertyKey::Symbol(
        crux::symbol::well_known("isConcatSpreadable")
            .as_ref()
            .clone(),
    );
    let spreadable =
        crate::context::get_property_key(agent, value, &spreadable_key, value.clone())?;
    if !matches!(spreadable, Value::Undefined) {
        return Ok(to_boolean(&spreadable));
    }
    is_array_or_throw(value)
}

/// spec 23.1.3.2 Array.prototype.concat.
fn concat(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let array = array_species_create(agent, &object, 0.0)?;
    let mut n = 0u64;
    let mut elements = vec![object];
    elements.extend_from_slice(args);
    for element in elements {
        if is_concat_spreadable(agent, &element)? {
            let length = length_of_array_like(agent, &element)?;
            if n + length > 9007199254740991.0 as u64 {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "Array length overflow".into(),
                ));
            }
            for k in 0..length {
                if has_property(&element, &key(k))? {
                    let value = get(agent, &element, &key(k))?;
                    array.create_data_property_or_throw(&key(n + k), value)?;
                }
            }
            n += length;
        } else {
            if n >= 9007199254740991.0 as u64 {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "Array length overflow".into(),
                ));
            }
            array.create_data_property_or_throw(&key(n), element)?;
            n += 1;
        }
    }
    array.set(
        &JsString::from_utf8("length"),
        Value::Number(n as f64),
        true,
    )?;
    Ok(Value::Object(array))
}

/// spec 23.1.3.3 Array.prototype.copyWithin.
fn copy_within(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let length = length_of_array_like(agent, &object)?;
    let relative_target = to_integer_or_infinity(to_number(
        &args.first().cloned().unwrap_or(Value::Undefined),
    )?);
    let to = if relative_target < 0.0 {
        (length as i64)
            .saturating_add(relative_target as i64)
            .max(0) as u64
    } else {
        (relative_target as u64).min(length)
    };
    let relative_start = to_integer_or_infinity(to_number(
        &args.get(1).cloned().unwrap_or(Value::Undefined),
    )?);
    let from = if relative_start < 0.0 {
        (length as i64).saturating_add(relative_start as i64).max(0) as u64
    } else {
        (relative_start as u64).min(length)
    };
    let final_index = clamped_end(args.get(1..).unwrap_or(&[]), length)?;
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
            let from_present = has_property(&object, &key(from))?;
            if from_present {
                let value = get(agent, &object, &key(from))?;
                set_property(&object, &key(to), value)?;
            } else {
                delete_property_or_throw(&object, &key(to))?;
            }
            from = (from as i64 + direction) as u64;
            to = (to as i64 + direction) as u64;
        }
    }
    Ok(object)
}

/// spec 23.1.5.1 CreateArrayIterator.
pub(crate) fn create_array_iterator(
    agent: &mut Agent,
    array: Value,
    kind: ArrayIterationKind,
) -> Result<Value, JsError> {
    let proto = agent
        .current_realm()?
        .intrinsics
        .get(ARRAY_ITERATOR)
        .and_then(|value| as_object(&value));
    let iterator = JsObject::ordinary_object_create(proto);
    agent
        .array_iter_data
        .insert(iterator.id(), (array, 0usize, kind as u32));
    Ok(Value::Object(iterator))
}

/// spec 23.1.3.4 Array.prototype.entries.
fn entries(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    create_array_iterator(agent, object, ArrayIterationKind::KeyValue)
}

/// spec 23.1.3.5 Array.prototype.every.
fn every(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    // spec: the length is read before the callback is checked.
    let length = length_of_array_like(agent, &object)?;
    let callbackfn = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&callbackfn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Array.prototype.every: callbackfn is not a function".into(),
        ));
    }
    let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    for k in 0..length {
        if has_property(&object, &key(k))? {
            let k_value = get(agent, &object, &key(k))?;
            let test = crate::function::call(
                agent,
                &callbackfn,
                this_arg.clone(),
                &[k_value, Value::Number(k as f64), object.clone()],
            )?;
            if !to_boolean(&test) {
                return Ok(Value::Boolean(false));
            }
        }
    }
    Ok(Value::Boolean(true))
}

/// spec 23.1.3.6 Array.prototype.fill.
fn fill(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    let length = length_of_array_like(agent, &object)?;
    let relative_start = to_integer_or_infinity(to_number(
        &args.get(1).cloned().unwrap_or(Value::Undefined),
    )?);
    let k = if relative_start < 0.0 {
        (length as i64).saturating_add(relative_start as i64).max(0) as u64
    } else {
        (relative_start as u64).min(length)
    };
    let final_index = clamped_end(args.get(1..).unwrap_or(&[]), length)?;
    for k in k..final_index {
        set_property(&object, &key(k), value.clone())?;
    }
    Ok(object)
}

/// spec 23.1.3.7 Array.prototype.filter.
fn filter(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    // spec: the length is read before the callback is checked.
    let length = length_of_array_like(agent, &object)?;
    let callbackfn = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&callbackfn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Array.prototype.filter: callbackfn is not a function".into(),
        ));
    }
    let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    let array = array_species_create(agent, &object, 0.0)?;
    let mut to_index = 0u64;
    for k in 0..length {
        if has_property(&object, &key(k))? {
            let k_value = get(agent, &object, &key(k))?;
            let selected = crate::function::call(
                agent,
                &callbackfn,
                this_arg.clone(),
                &[k_value.clone(), Value::Number(k as f64), object.clone()],
            )?;
            if to_boolean(&selected) {
                array.create_data_property_or_throw(&key(to_index), k_value)?;
                to_index += 1;
            }
        }
    }
    array.set(
        &JsString::from_utf8("length"),
        Value::Number(to_index as f64),
        true,
    )?;
    Ok(Value::Object(array))
}

/// spec 23.1.3.8 Array.prototype.find.
fn find(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    // spec: the length is read before the predicate is checked.
    let length = length_of_array_like(agent, &object)?;
    let predicate = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&predicate) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Array.prototype.find: predicate is not a function".into(),
        ));
    }
    let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    for k in 0..length {
        let k_value = get(agent, &object, &key(k))?;
        let test = crate::function::call(
            agent,
            &predicate,
            this_arg.clone(),
            &[k_value.clone(), Value::Number(k as f64), object.clone()],
        )?;
        if to_boolean(&test) {
            return Ok(k_value);
        }
    }
    Ok(Value::Undefined)
}

/// spec 23.1.3.9 Array.prototype.findIndex.
fn find_index(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    // spec: the length is read before the predicate is checked.
    let length = length_of_array_like(agent, &object)?;
    let predicate = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&predicate) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Array.prototype.findIndex: predicate is not a function".into(),
        ));
    }
    let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    for k in 0..length {
        let k_value = get(agent, &object, &key(k))?;
        let test = crate::function::call(
            agent,
            &predicate,
            this_arg.clone(),
            &[k_value, Value::Number(k as f64), object.clone()],
        )?;
        if to_boolean(&test) {
            return Ok(Value::Number(k as f64));
        }
    }
    Ok(Value::Number(-1.0))
}

/// The shared descending search of findLast/findLastIndex.
fn find_last_common(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    want_index: bool,
) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    // spec: the length is read before the predicate is checked.
    let length = length_of_array_like(agent, &object)?;
    let predicate = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&predicate) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Array.prototype.findLast: predicate is not a function".into(),
        ));
    }
    let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    let mut k = length as i64 - 1;
    while k >= 0 {
        let k_value = get(agent, &object, &key(k as u64))?;
        let test = crate::function::call(
            agent,
            &predicate,
            this_arg.clone(),
            &[k_value.clone(), Value::Number(k as f64), object.clone()],
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

/// spec 23.1.3.10 Array.prototype.findLast.
fn find_last(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    find_last_common(agent, this, args, false)
}

/// spec 23.1.3.11 Array.prototype.findLastIndex.
fn find_last_index(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    find_last_common(agent, this, args, true)
}

/// FlattenIntoArray (spec 23.1.3.12.1).
#[allow(clippy::too_many_arguments)]
fn flatten_into_array(
    agent: &mut Agent,
    target: &Handle<JsObject>,
    source: &Value,
    source_len: u64,
    start: u64,
    depth: u64,
    mapper: Option<(&Value, &Value)>,
) -> Result<u64, JsError> {
    let mut target_index = start;
    for source_index in 0..source_len {
        let name = key(source_index);
        if !has_property(source, &name)? {
            continue;
        }
        let mut element = get(agent, source, &name)?;
        if let Some((mapfn, this_arg)) = mapper {
            element = crate::function::call(
                agent,
                mapfn,
                this_arg.clone(),
                &[element, Value::Number(source_index as f64), source.clone()],
            )?;
        }
        if depth > 0 && is_array(&element) {
            let element_len = length_of_array_like(agent, &element)?;
            target_index = flatten_into_array(
                agent,
                target,
                &element,
                element_len,
                target_index,
                depth - 1,
                None,
            )?;
        } else {
            target.create_data_property_or_throw(&key(target_index), element)?;
            target_index += 1;
        }
    }
    Ok(target_index)
}

/// spec 23.1.3.12 Array.prototype.flat.
fn flat(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let source_len = length_of_array_like(agent, &object)?;
    let depth_number = match args.first() {
        None | Some(Value::Undefined) => 1.0,
        Some(value) => to_integer_or_infinity(to_number(value)?),
    };
    let array = array_species_create(agent, &object, 0.0)?;
    let depth = depth_number.max(0.0) as u64;
    flatten_into_array(agent, &array, &object, source_len, 0, depth, None)?;
    Ok(Value::Object(array))
}

/// spec 23.1.3.13 Array.prototype.flatMap.
fn flat_map(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let mapfn = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&mapfn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Array.prototype.flatMap: mapfn is not a function".into(),
        ));
    }
    let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    let source_len = length_of_array_like(agent, &object)?;
    let array = array_species_create(agent, &object, 0.0)?;
    flatten_into_array(
        agent,
        &array,
        &object,
        source_len,
        0,
        1,
        Some((&mapfn, &this_arg)),
    )?;
    Ok(Value::Object(array))
}

/// spec 23.1.3.14 Array.prototype.forEach.
fn for_each(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    // spec: the length is read before the callback is checked.
    let length = length_of_array_like(agent, &object)?;
    let callbackfn = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&callbackfn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Array.prototype.forEach: callbackfn is not a function".into(),
        ));
    }
    let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    for k in 0..length {
        if has_property(&object, &key(k))? {
            let k_value = get(agent, &object, &key(k))?;
            crate::function::call(
                agent,
                &callbackfn,
                this_arg.clone(),
                &[k_value, Value::Number(k as f64), object.clone()],
            )?;
        }
    }
    Ok(Value::Undefined)
}

/// spec 23.1.3.15 Array.prototype.includes.
fn includes(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let search_element = args.first().cloned().unwrap_or(Value::Undefined);
    let length = length_of_array_like(agent, &object)?;
    if length == 0 {
        return Ok(Value::Boolean(false));
    }
    let n = to_integer_or_infinity(to_number(
        &args.get(1).cloned().unwrap_or(Value::Undefined),
    )?);
    let k = if n >= 0.0 {
        n as u64
    } else {
        (length as i64).saturating_add(n as i64).max(0) as u64
    };
    for k in k..length {
        let element = get(agent, &object, &key(k))?;
        if same_value_zero(&element, &search_element) {
            return Ok(Value::Boolean(true));
        }
    }
    Ok(Value::Boolean(false))
}

/// spec 23.1.3.16 Array.prototype.indexOf.
fn index_of(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let search_element = args.first().cloned().unwrap_or(Value::Undefined);
    let length = length_of_array_like(agent, &object)?;
    if length == 0 {
        return Ok(Value::Number(-1.0));
    }
    let n = to_integer_or_infinity(to_number(
        &args.get(1).cloned().unwrap_or(Value::Undefined),
    )?);
    let k = if n >= 0.0 {
        n as u64
    } else {
        (length as i64).saturating_add(n as i64).max(0) as u64
    };
    for k in k..length {
        if has_property(&object, &key(k))? {
            let element = get(agent, &object, &key(k))?;
            if is_strictly_equal(&element, &search_element) {
                return Ok(Value::Number(k as f64));
            }
        }
    }
    Ok(Value::Number(-1.0))
}

/// spec 23.1.3.17 Array.prototype.join.
fn join(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let separator = match args.first() {
        Some(Value::Undefined) | None => ",".to_string(),
        Some(value) => crate::context::to_string(agent, value)?.to_string_lossy(),
    };
    let length = length_of_array_like(agent, &object)?;
    let mut result = String::new();
    for k in 0..length {
        if k > 0 {
            result.push_str(&separator);
        }
        let element = get(agent, &object, &key(k))?;
        if matches!(element, Value::Undefined | Value::Null) {
            continue;
        }
        result.push_str(&crate::context::to_string(agent, &element)?.to_string_lossy());
    }
    Ok(Value::String(Handle::new(JsString::from_utf8(&result))))
}

/// spec 23.1.3.18 Array.prototype.keys.
fn keys(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    create_array_iterator(agent, object, ArrayIterationKind::Key)
}

/// spec 23.1.3.19 Array.prototype.lastIndexOf.
fn last_index_of(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let search_element = args.first().cloned().unwrap_or(Value::Undefined);
    let length = length_of_array_like(agent, &object)?;
    if length == 0 {
        return Ok(Value::Number(-1.0));
    }
    let n = if args.len() < 2 {
        length as f64 - 1.0
    } else {
        to_integer_or_infinity(to_number(&args[1])?)
    };
    let mut k = if n >= 0.0 {
        (n as u64).min(length - 1)
    } else {
        // spec step 8: a negative fromIndex is added to the length; a
        // still-negative result means "not found" (15.4.4.15-5-13.js).
        let kf = length as f64 + n;
        if kf < 0.0 {
            return Ok(Value::Number(-1.0));
        }
        kf as u64
    };
    loop {
        if has_property(&object, &key(k))? {
            let element = get(agent, &object, &key(k))?;
            if is_strictly_equal(&element, &search_element) {
                return Ok(Value::Number(k as f64));
            }
        }
        if k == 0 {
            break;
        }
        k -= 1;
    }
    Ok(Value::Number(-1.0))
}

/// spec 23.1.3.20 Array.prototype.map.
fn map(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    // spec steps 3-4: the length is read before the callback is checked, so
    // a length getter's side effects and errors are visible when the
    // callback is missing or non-callable.
    let length = length_of_array_like(agent, &object)?;
    let callbackfn = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&callbackfn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Array.prototype.map: callbackfn is not a function".into(),
        ));
    }
    let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    let array = array_species_create(agent, &object, length as f64)?;
    for k in 0..length {
        if has_property(&object, &key(k))? {
            let k_value = get(agent, &object, &key(k))?;
            let mapped = crate::function::call(
                agent,
                &callbackfn,
                this_arg.clone(),
                &[k_value, Value::Number(k as f64), object.clone()],
            )?;
            array.create_data_property_or_throw(&key(k), mapped)?;
        }
    }
    Ok(Value::Object(array))
}

/// spec 23.1.3.21 Array.prototype.pop.
fn pop(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let length = length_of_array_like(agent, &object)?;
    if length == 0 {
        set_property(&object, &JsString::from_utf8("length"), Value::Number(0.0))?;
        return Ok(Value::Undefined);
    }
    let index = length - 1;
    let element = get(agent, &object, &key(index))?;
    delete_property_or_throw(&object, &key(index))?;
    set_property(
        &object,
        &JsString::from_utf8("length"),
        Value::Number(index as f64),
    )?;
    Ok(element)
}

/// spec 23.1.3.22 Array.prototype.push.
fn push(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let mut length = length_of_array_like(agent, &object)?;
    // spec 23.1.3.22 step 5: len + argCount > 2^53-1 throws before any
    // write (throws-if-integer-limit-exceeded.js).
    if length.saturating_add(args.len() as u64) > 9007199254740991 {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Array length exceeds 2^53-1".into(),
        ));
    }
    for item in args {
        set_property(&object, &key(length), item.clone())?;
        length += 1;
    }
    set_property(
        &object,
        &JsString::from_utf8("length"),
        Value::Number(length as f64),
    )?;
    Ok(Value::Number(length as f64))
}

/// spec 23.1.3.23 Array.prototype.reduce.
fn reduce(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    // spec: the length is read before the callback is checked.
    let length = length_of_array_like(agent, &object)?;
    let callbackfn = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&callbackfn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Array.prototype.reduce: callbackfn is not a function".into(),
        ));
    }
    let mut k = 0u64;
    let mut accumulator: Option<Value> = None;
    if args.len() >= 2 {
        accumulator = Some(args[1].clone());
    } else {
        let mut k_present = false;
        while k < length && !k_present {
            k_present = has_property(&object, &key(k))?;
            if k_present {
                accumulator = Some(get(agent, &object, &key(k))?);
            }
            k += 1;
        }
        if !k_present {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Reduce of empty array with no initial value".into(),
            ));
        }
    }
    while k < length {
        if has_property(&object, &key(k))? {
            let k_value = get(agent, &object, &key(k))?;
            let current = accumulator.take().unwrap_or(Value::Undefined);
            accumulator = Some(crate::function::call(
                agent,
                &callbackfn,
                Value::Undefined,
                &[current, k_value, Value::Number(k as f64), object.clone()],
            )?);
        }
        k += 1;
    }
    Ok(accumulator.unwrap_or(Value::Undefined))
}

/// spec 23.1.3.24 Array.prototype.reduceRight.
fn reduce_right(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    // spec: the length is read before the callback is checked.
    let length = length_of_array_like(agent, &object)?;
    let callbackfn = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&callbackfn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Array.prototype.reduceRight: callbackfn is not a function".into(),
        ));
    }
    let mut k = length as i64 - 1;
    let mut accumulator: Option<Value> = None;
    if args.len() >= 2 {
        accumulator = Some(args[1].clone());
    } else {
        let mut k_present = false;
        while k >= 0 && !k_present {
            k_present = has_property(&object, &key(k as u64))?;
            if k_present {
                accumulator = Some(get(agent, &object, &key(k as u64))?);
            }
            k -= 1;
        }
        if !k_present {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Reduce of empty array with no initial value".into(),
            ));
        }
    }
    while k >= 0 {
        if has_property(&object, &key(k as u64))? {
            let k_value = get(agent, &object, &key(k as u64))?;
            let current = accumulator.take().unwrap_or(Value::Undefined);
            accumulator = Some(crate::function::call(
                agent,
                &callbackfn,
                Value::Undefined,
                &[current, k_value, Value::Number(k as f64), object.clone()],
            )?);
        }
        k -= 1;
    }
    Ok(accumulator.unwrap_or(Value::Undefined))
}

/// spec 23.1.3.25 Array.prototype.reverse.
fn reverse(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let length = length_of_array_like(agent, &object)?;
    let mut lower = 0u64;
    while lower < length / 2 {
        let upper = length - 1 - lower;
        let lower_name = key(lower);
        let upper_name = key(upper);
        // spec steps 7.d-7.i: the lower value is read before HasProperty of
        // the upper index, so a getter that mutates the array (e.g. its
        // length) is observed (get_if_present_with_delete.js).
        let lower_exists = has_property(&object, &lower_name)?;
        let lower_value = if lower_exists {
            Some(get(agent, &object, &lower_name)?)
        } else {
            None
        };
        let upper_exists = has_property(&object, &upper_name)?;
        let upper_value = if upper_exists {
            Some(get(agent, &object, &upper_name)?)
        } else {
            None
        };
        if lower_exists && upper_exists {
            set_property(&object, &lower_name, upper_value.unwrap())?;
            set_property(&object, &upper_name, lower_value.unwrap())?;
        } else if lower_exists {
            // spec 23.1.3.25 step 5.10: DeletePropertyOrThrow of the lower
            // index runs before Set of the upper (delete-first ordering is
            // observable through proxies, see
            // length-exceeding-integer-limit-with-proxy.js).
            delete_property_or_throw(&object, &lower_name)?;
            set_property(&object, &upper_name, lower_value.unwrap())?;
        } else if upper_exists {
            set_property(&object, &lower_name, upper_value.unwrap())?;
            delete_property_or_throw(&object, &upper_name)?;
        }
        lower += 1;
    }
    Ok(object)
}

/// spec 23.1.3.26 Array.prototype.shift.
fn shift(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let length = length_of_array_like(agent, &object)?;
    if length == 0 {
        set_property(&object, &JsString::from_utf8("length"), Value::Number(0.0))?;
        return Ok(Value::Undefined);
    }
    let first = get(agent, &object, &key(0))?;
    for k in 1..length {
        let from_name = key(k);
        let to_name = key(k - 1);
        if has_property(&object, &from_name)? {
            let value = get(agent, &object, &from_name)?;
            set_property(&object, &to_name, value)?;
        } else {
            delete_property_or_throw(&object, &to_name)?;
        }
    }
    delete_property_or_throw(&object, &key(length - 1))?;
    set_property(
        &object,
        &JsString::from_utf8("length"),
        Value::Number((length - 1) as f64),
    )?;
    Ok(first)
}

/// spec 23.1.3.27 Array.prototype.slice.
fn slice(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let length = length_of_array_like(agent, &object)?;
    let relative_start = to_integer_or_infinity(to_number(
        &args.first().cloned().unwrap_or(Value::Undefined),
    )?);
    let mut k = if relative_start < 0.0 {
        (length as i64).saturating_add(relative_start as i64).max(0) as u64
    } else {
        (relative_start as u64).min(length)
    };
    let final_index = clamped_end(args, length)?;
    let count = final_index.saturating_sub(k);
    let array = array_species_create(agent, &object, count as f64)?;
    let mut n = 0u64;
    while k < final_index {
        if has_property(&object, &key(k))? {
            let value = get(agent, &object, &key(k))?;
            array.create_data_property_or_throw(&key(n), value)?;
        }
        k += 1;
        n += 1;
    }
    array.set(
        &JsString::from_utf8("length"),
        Value::Number(n as f64),
        true,
    )?;
    Ok(Value::Object(array))
}

/// spec 23.1.3.28 Array.prototype.some.
fn some(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    // spec: the length is read before the callback is checked.
    let length = length_of_array_like(agent, &object)?;
    let callbackfn = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&callbackfn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Array.prototype.some: callbackfn is not a function".into(),
        ));
    }
    let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    for k in 0..length {
        if has_property(&object, &key(k))? {
            let k_value = get(agent, &object, &key(k))?;
            let test = crate::function::call(
                agent,
                &callbackfn,
                this_arg.clone(),
                &[k_value, Value::Number(k as f64), object.clone()],
            )?;
            if to_boolean(&test) {
                return Ok(Value::Boolean(true));
            }
        }
    }
    Ok(Value::Boolean(false))
}

/// SortCompare (spec 23.1.3.29.2).
fn sort_compare(
    agent: &mut Agent,
    comparefn: &Value,
    x: &Value,
    y: &Value,
) -> Result<f64, JsError> {
    if matches!(x, Value::Undefined) && matches!(y, Value::Undefined) {
        return Ok(0.0);
    }
    if matches!(x, Value::Undefined) {
        return Ok(1.0);
    }
    if matches!(y, Value::Undefined) {
        return Ok(-1.0);
    }
    if !matches!(comparefn, Value::Undefined) {
        let v = crate::function::call(agent, comparefn, Value::Undefined, &[x.clone(), y.clone()])?;
        let v = to_number(&v)?;
        return Ok(if v.is_nan() { 0.0 } else { v });
    }
    let x_text = crate::context::to_string(agent, x)?;
    let y_text = crate::context::to_string(agent, y)?;
    Ok(match x_text.as_slice().cmp(y_text.as_slice()) {
        std::cmp::Ordering::Less => -1.0,
        std::cmp::Ordering::Greater => 1.0,
        std::cmp::Ordering::Equal => 0.0,
    })
}

/// SortIndexedProperties (spec 23.1.3.29.1): collect the present elements,
/// sort them stably with SortCompare, write them back, and delete the tail.
fn sort_indexed_properties(
    agent: &mut Agent,
    object: &Value,
    length: u64,
    comparefn: &Value,
) -> Result<(), JsError> {
    let mut items: Vec<Value> = Vec::new();
    for k in 0..length {
        let name = key(k);
        if has_property(object, &name)? {
            items.push(get(agent, object, &name)?);
        }
    }
    let mut error: Option<JsError> = None;
    items.sort_by(|a, b| {
        // comparefn-stop-after-error.js: no further calls once a comparison
        // returned an abrupt completion.
        if error.is_some() {
            return std::cmp::Ordering::Equal;
        }
        match sort_compare(agent, comparefn, a, b) {
            Ok(v) => v.partial_cmp(&0.0).unwrap_or(std::cmp::Ordering::Equal),
            Err(e) => {
                error = Some(e);
                std::cmp::Ordering::Equal
            }
        }
    });
    if let Some(e) = error {
        return Err(e);
    }
    for (j, item) in items.iter().enumerate() {
        set_property(object, &key(j as u64), item.clone())?;
    }
    for k in items.len() as u64..length {
        delete_property_or_throw(object, &key(k))?;
    }
    Ok(())
}

/// spec 23.1.3.29 Array.prototype.sort.
fn sort(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let comparefn = args.first().cloned().unwrap_or(Value::Undefined);
    if !matches!(comparefn, Value::Undefined) && !is_callable(&comparefn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Array.prototype.sort: comparefn is not a function".into(),
        ));
    }
    let length = length_of_array_like(agent, &object)?;
    sort_indexed_properties(agent, &object, length, &comparefn)?;
    Ok(object)
}

/// spec 23.1.3.30 Array.prototype.splice.
fn splice(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let length = length_of_array_like(agent, &object)?;
    let relative_start = to_integer_or_infinity(to_number(
        &args.first().cloned().unwrap_or(Value::Undefined),
    )?);
    let actual_start = if relative_start < 0.0 {
        (length as i64).saturating_add(relative_start as i64).max(0) as u64
    } else {
        (relative_start as u64).min(length)
    };
    let item_count = args.len().saturating_sub(2) as u64;
    let actual_delete_count = if args.is_empty() {
        // spec step 5: no arguments deletes nothing (clamps-length-to-
        // integer-limit.js).
        0
    } else if args.len() < 2 {
        length - actual_start
    } else {
        let delete_count = to_integer_or_infinity(to_number(&args[1])?);
        (delete_count.max(0.0) as u64).min(length - actual_start)
    };
    // spec 23.1.3.30 step 8: the new length must not exceed 2^53-1 (the
    // fixture `throws-if-integer-limit-exceeded` relies on this throwing
    // before any shifting, which would otherwise loop over the huge tail).
    let new_length = length + item_count - actual_delete_count;
    if new_length > 9007199254740991 {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Array length exceeds 2^53-1".into(),
        ));
    }
    let removed = array_species_create(agent, &object, actual_delete_count as f64)?;
    for k in 0..actual_delete_count {
        let from_name = key(actual_start + k);
        if has_property(&object, &from_name)? {
            let value = get(agent, &object, &from_name)?;
            removed.create_data_property_or_throw(&key(k), value)?;
        }
    }
    removed.set(
        &JsString::from_utf8("length"),
        Value::Number(actual_delete_count as f64),
        true,
    )?;
    if item_count < actual_delete_count {
        for k in actual_start..(length - actual_delete_count) {
            let from_name = key(k + actual_delete_count);
            let to_name = key(k + item_count);
            if has_property(&object, &from_name)? {
                let value = get(agent, &object, &from_name)?;
                set_property(&object, &to_name, value)?;
            } else {
                delete_property_or_throw(&object, &to_name)?;
            }
        }
        let mut k = length;
        while k > length - actual_delete_count + item_count {
            k -= 1;
            delete_property_or_throw(&object, &key(k))?;
        }
    } else if item_count > actual_delete_count {
        // spec steps 13.a-b: shift the tail up backwards (k decrements after
        // the copy, so the last iteration moves O[actualStart + count]).
        let mut k = length - actual_delete_count;
        while k > actual_start {
            let from_name = key(k + actual_delete_count - 1);
            let to_name = key(k + item_count - 1);
            if has_property(&object, &from_name)? {
                let value = get(agent, &object, &from_name)?;
                set_property(&object, &to_name, value)?;
            } else {
                delete_property_or_throw(&object, &to_name)?;
            }
            k -= 1;
        }
    }
    for (j, item) in args.iter().skip(2).enumerate() {
        set_property(&object, &key(actual_start + j as u64), item.clone())?;
    }
    let new_length = length - actual_delete_count + item_count;
    set_property(
        &object,
        &JsString::from_utf8("length"),
        Value::Number(new_length as f64),
    )?;
    Ok(Value::Object(removed))
}

/// spec 23.1.3.31 Array.prototype.toLocaleString.
fn to_locale_string(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let length = length_of_array_like(agent, &object)?;
    let mut result = String::new();
    for k in 0..length {
        if k > 0 {
            result.push(',');
        }
        let element = get(agent, &object, &key(k))?;
        if matches!(element, Value::Undefined | Value::Null) {
            continue;
        }
        let boxed = crate::context::to_object(agent, &element)?;
        let method = get(agent, &boxed, &JsString::from_utf8("toLocaleString"))?;
        // spec steps 10-12: the method is invoked with the *element* as the
        // receiver (not the box), so a primitives' overridden toString sees
        // the primitive (primitive_this_value.js).
        let text = crate::function::call(agent, &method, element, &[])?;
        result.push_str(&crate::context::to_string(agent, &text)?.to_string_lossy());
    }
    Ok(Value::String(Handle::new(JsString::from_utf8(&result))))
}

/// spec 23.1.3.32 Array.prototype.toReversed.
fn to_reversed(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let length = length_of_array_like(agent, &object)?;
    // spec step 3: ArrayCreate — @@species is ignored (ignores-species.js).
    let array = array_create(agent, length as f64)?;
    for k in 0..length {
        let from_name = key(length - 1 - k);
        let value = get(agent, &object, &from_name)?;
        array.create_data_property_or_throw(&key(k), value)?;
    }
    Ok(Value::Object(array))
}

/// spec 23.1.3.33 Array.prototype.toSorted.
fn to_sorted(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let comparefn = args.first().cloned().unwrap_or(Value::Undefined);
    if !matches!(comparefn, Value::Undefined) && !is_callable(&comparefn) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Array.prototype.toSorted: comparefn is not a function".into(),
        ));
    }
    let length = length_of_array_like(agent, &object)?;
    // spec step 6: ArrayCreate — @@species is ignored (ignores-species.js).
    let array = array_create(agent, length as f64)?;
    for k in 0..length {
        let value = get(agent, &object, &key(k))?;
        array.create_data_property_or_throw(&key(k), value)?;
    }
    let copy = Value::Object(array.clone());
    sort_indexed_properties(agent, &copy, length, &comparefn)?;
    Ok(copy)
}

/// spec 23.1.3.34 Array.prototype.toSpliced.
fn to_spliced(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let length = length_of_array_like(agent, &object)?;
    let relative_start = to_integer_or_infinity(to_number(
        &args.first().cloned().unwrap_or(Value::Undefined),
    )?);
    let actual_start = if relative_start < 0.0 {
        (length as i64).saturating_add(relative_start as i64).max(0) as u64
    } else {
        (relative_start as u64).min(length)
    };
    let insert_count = args.len().saturating_sub(2) as u64;
    // spec steps 8-9: no arguments delete nothing; a missing deleteCount
    // (start only) deletes the tail — unlike splice's no-arg case.
    let actual_delete_count = if args.is_empty() {
        0
    } else if args.len() < 2 {
        length - actual_start
    } else {
        let delete_count = to_integer_or_infinity(to_number(&args[1])?);
        (delete_count.max(0.0) as u64).min(length - actual_start)
    };
    let new_length = length - actual_delete_count + insert_count;
    // spec step 12: the new length must not exceed 2^53-1 (TypeError), and
    // ArrayCreate rejects lengths over 2^32-1 with a RangeError
    // (length-exceeding-array-length-limit.js).
    if new_length > 9007199254740991 {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Array length exceeds 2^53-1".into(),
        ));
    }
    // spec step 13: ArrayCreate — @@species is ignored (ignores-species.js).
    let array = array_create(agent, new_length as f64)?;
    // spec steps 11-14: copy the prefix, insert the items, copy the suffix.
    let mut i = 0u64;
    while i < actual_start {
        let value = get(agent, &object, &key(i))?;
        array.create_data_property_or_throw(&key(i), value)?;
        i += 1;
    }
    for item in args.iter().skip(2) {
        array.create_data_property_or_throw(&key(i), item.clone())?;
        i += 1;
    }
    let mut r = actual_start + actual_delete_count;
    while r < length {
        let value = get(agent, &object, &key(r))?;
        array.create_data_property_or_throw(&key(i), value)?;
        i += 1;
        r += 1;
    }
    Ok(Value::Object(array))
}

/// spec 23.1.3.35 Array.prototype.toString.
fn to_string_method(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let join_method = get(agent, &object, &JsString::from_utf8("join"))?;
    let func = if is_callable(&join_method) {
        join_method
    } else {
        agent
            .current_realm()?
            .intrinsics
            .get("%Object.prototype.toString%")
            .unwrap_or(Value::Undefined)
    };
    crate::function::call(agent, &func, object, &[])
}

/// spec 23.1.3.36 Array.prototype.unshift.
fn unshift(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let length = length_of_array_like(agent, &object)?;
    let arg_count = args.len() as u64;
    if arg_count > 0 {
        if length + arg_count > 9007199254740991.0 as u64 {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Array length overflow".into(),
            ));
        }
        let mut k = length;
        while k > 0 {
            k -= 1;
            let from_name = key(k);
            let to_name = key(k + arg_count);
            if has_property(&object, &from_name)? {
                let value = get(agent, &object, &from_name)?;
                set_property(&object, &to_name, value)?;
            } else {
                delete_property_or_throw(&object, &to_name)?;
            }
        }
        for (j, item) in args.iter().enumerate() {
            set_property(&object, &key(j as u64), item.clone())?;
        }
    }
    let new_length = length + arg_count;
    set_property(
        &object,
        &JsString::from_utf8("length"),
        Value::Number(new_length as f64),
    )?;
    Ok(Value::Number(new_length as f64))
}

/// spec 23.1.3.37 Array.prototype.values.
fn values(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    create_array_iterator(agent, object, ArrayIterationKind::Value)
}

/// spec 23.1.3.38 Array.prototype.with.
fn with(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    require_object_coercible(this)?;
    let object = crate::context::to_object(agent, this)?;
    let length = length_of_array_like(agent, &object)?;
    let relative_index = to_integer_or_infinity(to_number(
        &args.first().cloned().unwrap_or(Value::Undefined),
    )?);
    let actual_index = if relative_index >= 0.0 {
        relative_index as u64
    } else {
        (length as i64).saturating_add(relative_index as i64) as u64
    };
    if actual_index >= length {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "Index out of range".into(),
        ));
    }
    let value = args.get(1).cloned().unwrap_or(Value::Undefined);
    // spec step 8: ArrayCreate — @@species is ignored (ignores-species.js).
    let array = array_create(agent, length as f64)?;
    for i in 0..length {
        let name = key(i);
        let new_value = if i == actual_index {
            value.clone()
        } else {
            get(agent, &object, &name)?
        };
        array.create_data_property_or_throw(&name, new_value)?;
    }
    Ok(Value::Object(array))
}

/// CreateIterResultObject (spec 7.3.17).
fn iter_result(agent: &Agent, value: Value, done: bool) -> Result<Value, JsError> {
    let proto = agent
        .current_realm()?
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|v| as_object(&v));
    let result = JsObject::ordinary_object_create(proto);
    result.create_data_property(&JsString::from_utf8("value"), value)?;
    result.create_data_property(&JsString::from_utf8("done"), Value::Boolean(done))?;
    Ok(Value::Object(result))
}

/// spec 23.1.5.2.1 ArrayIterator.prototype.next.
fn array_iterator_next(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let Value::Object(obj) = this else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Array Iterator.prototype.next requires an Array Iterator".into(),
        ));
    };
    let Some((array, index, kind)) = agent.array_iter_data.get(&obj.id()).cloned() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Incompatible receiver".into(),
        ));
    };
    if matches!(array, Value::Undefined) {
        return iter_result(agent, Value::Undefined, true);
    }
    // spec %ArrayIteratorPrototype%.next step 8: a TypedArray whose buffer
    // was detached mid-iteration throws (detach-typedarray-in-progress.js).
    if let Value::Object(iter_obj) = &array
        && let crux::object::ObjectKind::IntegerIndexed(slots) = &iter_obj.kind
        && slots.buffer.is_detached()
    {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "TypedArray buffer is detached".into(),
        ));
    }
    let length = length_of_array_like(agent, &array)?;
    let next_index = index as u64;
    if next_index >= length {
        agent
            .array_iter_data
            .insert(obj.id(), (Value::Undefined, index, kind));
        return iter_result(agent, Value::Undefined, true);
    }
    let name = key(next_index);
    let value = match kind {
        0 => {
            let element = get_property(agent, &array, &name, array.clone())?;
            array_from_values(agent, &[Value::Number(next_index as f64), element])?
        }
        1 => Value::Number(next_index as f64),
        _ => get_property(agent, &array, &name, array.clone())?,
    };
    agent
        .array_iter_data
        .insert(obj.id(), (array, next_index as usize + 1, kind));
    iter_result(agent, value, false)
}

/// The `Array.fromAsync` continuation phases (spec 23.1.2.4.1): which Await
/// the pending handler is waiting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FromAsyncPhase {
    /// Awaiting the IteratorStep result.
    Step,
    /// Awaiting a raw array-like element value (before mapping).
    Element,
    /// Awaiting the mapped value.
    Mapped,
}

/// The mutable state of the `Array.fromAsync` closure, shared across the
/// job-queue continuations.
#[derive(Debug)]
pub struct FromAsyncState {
    pub array: Value,
    pub items: Value,
    pub mapfn: Value,
    pub this_arg: Value,
    pub is_array_like: bool,
    pub len: u64,
    pub iterator: Option<IteratorRecord>,
    pub k: u64,
    pub capability: PromiseCapability,
    pub(crate) phase: FromAsyncPhase,
}

/// AsyncIteratorClose with a throw completion (spec 27.1.4.1 steps 6-7),
/// best-effort: the original error always wins, so a throwing `return` (or a
/// non-object `return` result) is swallowed.
fn from_async_iterator_close(agent: &mut Agent, iterator: &IteratorRecord) {
    let _ = crate::expr::iterator_close_throw(agent, iterator);
}

/// Attach the next Await of the fromAsync loop: `value` becomes a promise,
/// and the fresh handlers resume `state` (or reject the capability).
fn attach_from_async_await(
    agent: &mut Agent,
    state: Rc<RefCell<FromAsyncState>>,
    value: Value,
) -> Result<(), JsError> {
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let promise = crate::promise::promise_resolve(agent, &promise_ctor, value)?;
    let on_fulfilled = make_from_async_handler(agent, state.clone(), false)?;
    let on_rejected = make_from_async_handler(agent, state.clone(), true)?;
    crate::promise::perform_promise_then(
        agent,
        &promise,
        Some(on_fulfilled),
        Some(on_rejected),
        None,
    )?;
    Ok(())
}

fn make_from_async_handler(
    agent: &mut Agent,
    state: Rc<RefCell<FromAsyncState>>,
    is_reject: bool,
) -> Result<Value, JsError> {
    let closure = Function::create_builtin(
        Some(JsString::from_utf8("")),
        1,
        placeholder("fromAsync handler"),
        None,
        None,
    )?;
    agent
        .array_from_async
        .insert(closure.id(), (state, is_reject));
    Ok(Value::Function(closure))
}

/// Reject the fromAsync capability with `error`, closing the iterator first
/// when it is being driven (IfAbruptCloseAsyncIterator with a throw
/// completion: the original error wins, a throwing `return` is swallowed).
fn from_async_reject(
    agent: &mut Agent,
    state: &Rc<RefCell<FromAsyncState>>,
    error: JsError,
) -> Result<(), JsError> {
    let (reject, iterator) = {
        let state = state.borrow();
        (state.capability.reject.clone(), state.iterator.clone())
    };
    if let Some(record) = iterator {
        from_async_iterator_close(agent, &record);
    }
    let rejection = crate::promise::error_value(agent, &error);
    crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
    Ok(())
}

/// Set the result's `length` (spec 23.1.2.4.1 steps 13.d.i / 14.j) and
/// resolve the capability. A failing Set (e.g. a read-only length or a
/// throwing setter) rejects instead.
fn from_async_finish(
    agent: &mut Agent,
    state: &Rc<RefCell<FromAsyncState>>,
    len: u64,
) -> Result<Value, JsError> {
    let (array, resolve) = {
        let state = state.borrow();
        (state.array.clone(), state.capability.resolve.clone())
    };
    if let Err(error) = object_of(&array)?.set(
        &JsString::from_utf8("length"),
        Value::Number(len as f64),
        true,
    ) {
        from_async_reject(agent, state, error)?;
        return Ok(Value::Undefined);
    }
    crate::function::call(agent, &resolve, Value::Undefined, &[array])?;
    Ok(Value::Undefined)
}

/// Define `array[k] = value`, advance k, and continue the loop: the
/// array-like path awaits the next raw element, the iterator path takes the
/// next IteratorStep (spec 23.1.2.4.1 steps 13.g-i / 14.h-i).
fn from_async_define_and_advance(
    agent: &mut Agent,
    state: &Rc<RefCell<FromAsyncState>>,
    value: Value,
) -> Result<Value, JsError> {
    let k = state.borrow().k;
    let array = state.borrow().array.clone();
    if let Err(error) = object_of(&array)?.create_data_property_or_throw(&key(k), value.clone()) {
        from_async_reject(agent, state, error)?;
        return Ok(Value::Undefined);
    }
    state.borrow_mut().k = k + 1;
    let (is_array_like, len, items) = {
        let state = state.borrow();
        (state.is_array_like, state.len, state.items.clone())
    };
    if is_array_like {
        let k = state.borrow().k;
        if k >= len {
            return from_async_finish(agent, state, len);
        }
        let next = match get(agent, &items, &key(k)) {
            Ok(next) => next,
            Err(error) => {
                from_async_reject(agent, state, error)?;
                return Ok(Value::Undefined);
            }
        };
        state.borrow_mut().phase = FromAsyncPhase::Element;
        if let Err(error) = attach_from_async_await(agent, state.clone(), next) {
            from_async_reject(agent, state, error)?;
        }
        return Ok(Value::Undefined);
    }
    // Iterator path: Await(IteratorStep(iteratorRecord)).
    let (iterator, next) = {
        let state = state.borrow();
        let record = state.iterator.as_ref().ok_or_else(|| {
            JsError::new(ErrorKind::TypeError, "fromAsync iterator missing".into())
        })?;
        (record.iterator.clone(), record.next.clone())
    };
    let step_promise = match crate::function::call(agent, &next, iterator, &[]) {
        Ok(promise) => promise,
        Err(error) => {
            from_async_reject(agent, state, error)?;
            return Ok(Value::Undefined);
        }
    };
    state.borrow_mut().phase = FromAsyncPhase::Step;
    if let Err(error) = attach_from_async_await(agent, state.clone(), step_promise) {
        from_async_reject(agent, state, error)?;
    }
    Ok(Value::Undefined)
}

/// Resume the fromAsync loop with the resolved await value.
fn from_async_resume(
    agent: &mut Agent,
    state: Rc<RefCell<FromAsyncState>>,
    value: Value,
) -> Result<Value, JsError> {
    let phase = state.borrow().phase;
    match phase {
        FromAsyncPhase::Step => {
            // `value` is the resolved iterator result object.
            let done = to_boolean(&get_property(
                agent,
                &value,
                &JsString::from_utf8("done"),
                value.clone(),
            )?);
            if done {
                // spec step 13.d: Set(A, "length", k, true) before returning.
                let k = state.borrow().k;
                return from_async_finish(agent, &state, k);
            }
            let step_value =
                get_property(agent, &value, &JsString::from_utf8("value"), value.clone())?;
            let mapping = !matches!(state.borrow().mapfn, Value::Undefined);
            if mapping {
                from_async_map_and_await(agent, &state, step_value)
            } else {
                // spec steps 13.f-g: without a mapper the iterator value is
                // defined directly — it must not be awaited.
                from_async_define_and_advance(agent, &state, step_value)
            }
        }
        FromAsyncPhase::Element => {
            // `value` is the resolved array-like element (spec step 14.e);
            // map it when a mapper is present, else define it directly.
            let mapping = !matches!(state.borrow().mapfn, Value::Undefined);
            if mapping {
                from_async_map_and_await(agent, &state, value)
            } else {
                from_async_define_and_advance(agent, &state, value)
            }
        }
        FromAsyncPhase::Mapped => from_async_define_and_advance(agent, &state, value),
    }
}

/// Map the step/element value (when a mapper is present) and await it. The
/// array-like path defines its element directly after the raw-element await
/// (spec 23.1.2.4.1 steps 14.e-h), and the iterator path without a mapper
/// defines its value without awaiting at all (steps 13.f-g); both skip this
/// when no mapper is present, so the mapfn here is always defined.
fn from_async_map_and_await(
    agent: &mut Agent,
    state: &Rc<RefCell<FromAsyncState>>,
    value: Value,
) -> Result<Value, JsError> {
    let (mapfn, this_arg, k, array) = {
        let state = state.borrow();
        (
            state.mapfn.clone(),
            state.this_arg.clone(),
            state.k,
            state.array.clone(),
        )
    };
    let mapped = if matches!(mapfn, Value::Undefined) {
        value
    } else {
        match crate::function::call(
            agent,
            &mapfn,
            this_arg,
            &[value, Value::Number(k as f64), array],
        ) {
            Ok(mapped) => mapped,
            Err(error) => {
                from_async_reject(agent, state, error)?;
                return Ok(Value::Undefined);
            }
        }
    };
    state.borrow_mut().phase = FromAsyncPhase::Mapped;
    if let Err(error) = attach_from_async_await(agent, state.clone(), mapped) {
        from_async_reject(agent, state, error)?;
    }
    Ok(Value::Undefined)
}

/// spec 23.1.2.4 Array.fromAsync. Any failure rejects the capability — the
/// call never throws synchronously (an async-test harness asserts that).
fn from_async(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let capability = crate::promise::new_promise_capability(agent, &promise_ctor)?;
    let reject = capability.reject.clone();
    let promise = capability.promise.clone();
    let result = (|| -> Result<(), JsError> {
        let items = args.first().cloned().unwrap_or(Value::Undefined);
        let mapfn = args.get(1).cloned().unwrap_or(Value::Undefined);
        let this_arg = args.get(2).cloned().unwrap_or(Value::Undefined);
        let mapping = !matches!(mapfn, Value::Undefined);
        if mapping && !is_callable(&mapfn) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Array.fromAsync: mapfn is not a function".into(),
            ));
        }
        // spec 23.1.2.4 steps 3-7: @@asyncIterator, then @@iterator (wrapped
        // in an AsyncFromSyncIterator), then the array-like path over
        // ToObject(items).
        let using_async = get_method(agent, &items, "@@asyncIterator")?;
        let using_sync = if using_async.is_some() {
            None
        } else {
            get_method(agent, &items, "@@iterator")?
        };
        let mut iterator = None;
        let (is_array_like, len, items, array) = if using_async.is_some() || using_sync.is_some() {
            iterator = Some(async_iterator_from(agent, &items)?);
            // spec step 4: Construct(C) with no arguments (no @@species).
            let array = if is_constructor(this) {
                crate::function::construct(agent, this, &[], this)?
            } else {
                Value::Object(array_create(agent, 0.0)?)
            };
            (false, 0, items, array)
        } else {
            // Array-like path: ToObject, then LengthOfArrayLike (primitives
            // get their wrapper, so Number.prototype[0] etc. are visible),
            // then Construct(C, « len ») or ArrayCreate(len).
            let array_like = crate::context::to_object(agent, &items)?;
            let len = length_of_array_like(agent, &array_like)?;
            let array = if is_constructor(this) {
                crate::function::construct(agent, this, &[Value::Number(len as f64)], this)?
            } else {
                Value::Object(array_create(agent, len as f64)?)
            };
            (true, len, array_like, array)
        };
        let state = Rc::new(RefCell::new(FromAsyncState {
            array,
            items,
            mapfn,
            this_arg,
            is_array_like,
            len,
            iterator,
            k: 0,
            capability,
            phase: FromAsyncPhase::Mapped,
        }));
        // Kick the loop: the first Await is on the raw element 0
        // (array-like) or on the first IteratorStep (iterator path).
        if is_array_like {
            if len == 0 {
                // spec step 14.j: no elements are read for a 0-length
                // array-like; the length is set and the loop resolves.
                let (array, resolve) = {
                    let state = state.borrow();
                    (state.array.clone(), state.capability.resolve.clone())
                };
                if let Err(error) =
                    object_of(&array)?.set(&JsString::from_utf8("length"), Value::Number(0.0), true)
                {
                    from_async_reject(agent, &state, error)?;
                } else {
                    crate::function::call(agent, &resolve, Value::Undefined, &[array])?;
                }
                return Ok(());
            }
            let items = state.borrow().items.clone();
            let next = get(agent, &items, &key(0))?;
            state.borrow_mut().phase = FromAsyncPhase::Element;
            if let Err(error) = attach_from_async_await(agent, state.clone(), next) {
                from_async_reject(agent, &state, error)?;
            }
        } else {
            state.borrow_mut().phase = FromAsyncPhase::Step;
            let (iterator, next) = {
                let state = state.borrow();
                let record = state.iterator.as_ref().ok_or_else(|| {
                    JsError::new(ErrorKind::TypeError, "fromAsync iterator missing".into())
                })?;
                (record.iterator.clone(), record.next.clone())
            };
            let step_promise = crate::function::call(agent, &next, iterator, &[])?;
            attach_from_async_await(agent, state.clone(), step_promise)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        let rejection = crate::promise::error_value(agent, &error);
        crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
    }
    Ok(promise)
}

/// GetIterator for fromAsync: prefer @@asyncIterator, fall back to a
/// @@iterator wrapped in an AsyncFromSyncIterator (spec 23.1.2.4.1 steps
/// 10-11).
fn async_iterator_from(agent: &mut Agent, items: &Value) -> Result<IteratorRecord, JsError> {
    let async_method = get_method(agent, items, "@@asyncIterator")?;
    if let Some(method) = async_method {
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
                "Async iterator's next method is not callable".into(),
            ));
        }
        return Ok(IteratorRecord { iterator, next });
    }
    let sync_method = get_method(agent, items, "@@iterator")?;
    let Some(sync_method) = sync_method else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Value is not async iterable or iterable".into(),
        ));
    };
    let sync_iterator = crate::function::call(agent, &sync_method, items.clone(), &[])?;
    let next = get_property(
        agent,
        &sync_iterator,
        &JsString::from_utf8("next"),
        sync_iterator.clone(),
    )?;
    if !is_callable(&next) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator's next method is not callable".into(),
        ));
    }
    let sync_record = IteratorRecord {
        iterator: sync_iterator,
        next,
    };
    let async_iterator = crate::async_await::async_from_sync_iterator(agent, &sync_record)?;
    let async_value = Value::Object(async_iterator.clone());
    let next = get_property(
        agent,
        &async_value,
        &JsString::from_utf8("next"),
        async_value.clone(),
    )?;
    if !is_callable(&next) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Async iterator's next method is not callable".into(),
        ));
    }
    Ok(IteratorRecord {
        iterator: Value::Object(async_iterator),
        next,
    })
}

/// The `%Array.prototype[@@species]%` getter (spec 23.1.3.41): returns `this`.
fn species_getter(_agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    Ok(this.clone())
}

/// Install the Array intrinsics and the global `Array` binding (spec 23.1)
/// during SetDefaultGlobalBindings. `%Object.prototype%` must exist first.
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let array_proto = JsObject::array_create(object_proto.clone(), 0.0)?;
    let array_proto_value = Value::Object(array_proto.clone());

    let array_ctor = Function::create_builtin(
        Some(JsString::from_utf8("Array")),
        1,
        placeholder("Array"),
        Some(Box::new(placeholder("Array"))),
        None,
    )?;
    let array_ctor_value = Value::Function(array_ctor.clone());

    realm.intrinsics.define(ARRAY, array_ctor_value.clone());
    realm
        .intrinsics
        .define(ARRAY_PROTO, array_proto_value.clone());

    array_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(array_proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    array_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(array_ctor_value.clone()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // spec 23.1.2: the statics.
    let statics: [(&str, &str, u64); 4] = [
        ("isArray", IS_ARRAY, 1),
        ("of", OF, 0),
        ("from", FROM, 1),
        ("fromAsync", FROM_ASYNC, 1),
    ];
    for (name, intrinsic, length) in statics {
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
        array_ctor.define_property(
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

    // spec 23.1.3: the prototype methods, all agent-dispatched.
    let methods: [(&str, &str, u64); 33] = [
        ("at", AT, 1),
        ("concat", CONCAT, 1),
        ("copyWithin", COPY_WITHIN, 2),
        ("entries", ENTRIES, 0),
        ("every", EVERY, 1),
        ("fill", FILL, 1),
        ("filter", FILTER, 1),
        ("find", FIND, 1),
        ("findIndex", FIND_INDEX, 1),
        ("findLast", FIND_LAST, 1),
        ("findLastIndex", FIND_LAST_INDEX, 1),
        ("flat", FLAT, 0),
        ("flatMap", FLAT_MAP, 1),
        ("forEach", FOR_EACH, 1),
        ("includes", INCLUDES, 1),
        ("indexOf", INDEX_OF, 1),
        ("join", JOIN, 1),
        ("keys", KEYS, 0),
        ("lastIndexOf", LAST_INDEX_OF, 1),
        ("map", MAP, 1),
        ("pop", POP, 0),
        ("push", PUSH, 1),
        ("reduce", REDUCE, 1),
        ("reduceRight", REDUCE_RIGHT, 1),
        ("reverse", REVERSE, 0),
        ("shift", SHIFT, 0),
        ("slice", SLICE, 2),
        ("some", SOME, 1),
        ("sort", SORT, 1),
        ("splice", SPLICE, 2),
        ("toSpliced", TO_SPLICED, 2),
        ("toString", TO_STRING, 0),
        ("unshift", UNSHIFT, 1),
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
        array_proto.define_property(
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
    let second_wave: [(&str, &str, u64); 5] = [
        ("values", VALUES, 0),
        ("with", WITH, 2),
        ("toLocaleString", TO_LOCALE_STRING, 0),
        ("toReversed", TO_REVERSED, 0),
        ("toSorted", TO_SORTED, 1),
    ];
    for (name, intrinsic, length) in second_wave {
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
        array_proto.define_property(
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

    // spec 23.1.3.39: @@iterator is %Array.prototype.values%.
    let values_func = realm.intrinsics.get(VALUES).ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "%Array.prototype.values% missing".into(),
        )
    })?;
    realm.intrinsics.define(ITERATOR, values_func.clone());
    array_proto.define_property_key(
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

    // spec 23.1.2.5: get Array [ @@species ] — an accessor on the Array
    // constructor returning `this` (the species-using methods consult
    // constructor[@@species] through ArraySpeciesCreate).
    let species_func = Function::create_builtin(
        Some(JsString::from_utf8("get [Symbol.species]")),
        0,
        placeholder("species"),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(SPECIES, Value::Function(species_func.clone()));
    array_ctor.define_property_key(
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

    // spec 23.1.3.40: @@unscopables, an ordinary object with a null prototype.
    let unscopables = JsObject::ordinary_object_create(None);
    for name in [
        "at",
        "copyWithin",
        "entries",
        "every",
        "fill",
        "filter",
        "find",
        "findIndex",
        "findLast",
        "findLastIndex",
        "flat",
        "flatMap",
        "forEach",
        "includes",
        "indexOf",
        "keys",
        "lastIndexOf",
        "map",
        "reduce",
        "reduceRight",
        "reverse",
        "slice",
        "some",
        "sort",
        "splice",
        "toLocaleString",
        "toReversed",
        "toSorted",
        "toSpliced",
        "values",
    ] {
        unscopables.create_data_property(&JsString::from_utf8(name), Value::Boolean(true))?;
    }
    array_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("unscopables").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::Object(unscopables)),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // spec 23.1.5: %ArrayIteratorPrototype%.
    let iterator_proto = JsObject::ordinary_object_create(object_proto.clone());
    let iterator_proto_value = Value::Object(iterator_proto.clone());
    realm
        .intrinsics
        .define(ARRAY_ITERATOR, iterator_proto_value.clone());
    let next_func = Function::create_builtin(
        Some(JsString::from_utf8("next")),
        0,
        placeholder("next"),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(ARRAY_ITERATOR_NEXT, Value::Function(next_func.clone()));
    iterator_proto.define_property(
        &JsString::from_utf8("next"),
        &PropertyDescriptor {
            value: Some(Value::Function(next_func)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    iterator_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8(
                "Array Iterator",
            )))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("Array"),
        &PropertyDescriptor {
            value: Some(array_ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// The Array members that need the agent, dispatched by intrinsic identity
/// from `runtime::function::call`/`construct`.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(ARRAY).as_ref() == Some(callee) {
        return Some(array_call(agent, args));
    }
    if intrinsics.get(IS_ARRAY).as_ref() == Some(callee) {
        return Some(array_is_array(agent, this, args));
    }
    if intrinsics.get(OF).as_ref() == Some(callee) {
        return Some(array_of(agent, this, args));
    }
    if intrinsics.get(FROM).as_ref() == Some(callee) {
        return Some(array_from(agent, this, args));
    }
    if intrinsics.get(FROM_ASYNC).as_ref() == Some(callee) {
        return Some(from_async(agent, this, args));
    }
    if intrinsics.get(AT).as_ref() == Some(callee) {
        return Some(at(agent, this, args));
    }
    if intrinsics.get(CONCAT).as_ref() == Some(callee) {
        return Some(concat(agent, this, args));
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
    if intrinsics.get(FLAT).as_ref() == Some(callee) {
        return Some(flat(agent, this, args));
    }
    if intrinsics.get(FLAT_MAP).as_ref() == Some(callee) {
        return Some(flat_map(agent, this, args));
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
    if intrinsics.get(POP).as_ref() == Some(callee) {
        return Some(pop(agent, this, args));
    }
    if intrinsics.get(PUSH).as_ref() == Some(callee) {
        return Some(push(agent, this, args));
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
    if intrinsics.get(SHIFT).as_ref() == Some(callee) {
        return Some(shift(agent, this, args));
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
    if intrinsics.get(SPLICE).as_ref() == Some(callee) {
        return Some(splice(agent, this, args));
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
    if intrinsics.get(TO_SPLICED).as_ref() == Some(callee) {
        return Some(to_spliced(agent, this, args));
    }
    if intrinsics.get(TO_STRING).as_ref() == Some(callee) {
        return Some(to_string_method(agent, this, args));
    }
    if intrinsics.get(UNSHIFT).as_ref() == Some(callee) {
        return Some(unshift(agent, this, args));
    }
    if intrinsics.get(VALUES).as_ref() == Some(callee) {
        return Some(values(agent, this, args));
    }
    if intrinsics.get(WITH).as_ref() == Some(callee) {
        return Some(with(agent, this, args));
    }
    if intrinsics.get(ITERATOR).as_ref() == Some(callee) {
        return Some(values(agent, this, args));
    }
    if intrinsics.get(SPECIES).as_ref() == Some(callee) {
        return Some(species_getter(agent, this, args));
    }
    if intrinsics.get(ARRAY_ITERATOR_NEXT).as_ref() == Some(callee) {
        return Some(array_iterator_next(agent, this, args));
    }
    // The fromAsync continuations, keyed by function identity.
    if let Value::Function(function) = callee
        && let Some((state, is_reject)) = agent.array_from_async.get(&function.id()).cloned()
    {
        if is_reject {
            // Reject the capability with the awaited value, closing the
            // iterator first (IfAbruptCloseAsyncIterator, spec 23.1.2.4.1).
            let rejection = args.first().cloned().unwrap_or(Value::Undefined);
            let error = JsError::new(ErrorKind::TypeError, "fromAsync rejected".into())
                .with_value(rejection);
            return Some(from_async_reject(agent, &state, error).map(|_| Value::Undefined));
        }
        let value = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(from_async_resume(agent, state, value));
    }
    None
}

/// The Array constructor's [[Construct]] (spec 23.1.1.1 with the given
/// newTarget).
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    if realm.intrinsics.get(ARRAY).as_ref() == Some(callee) {
        return Some(array_construct(agent, args, new_target));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::promise::PromiseState;

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

    fn joined(source: &str) -> String {
        text(&format!("{source}.join(\",\")"))
    }

    /// Evaluate a script ending in a promise, drain the jobs, and return the
    /// settled value.
    fn settle(source: &str) -> Result<Value, JsError> {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm()?;
        let value = agent.run_script(source)?;
        agent.run_jobs()?;
        settled(&agent, &value)
    }

    fn settled(agent: &Agent, value: &Value) -> Result<Value, JsError> {
        let Value::Object(obj) = value else {
            return Ok(value.clone());
        };
        let Some(data) = agent.promises.get(&obj.id()) else {
            return Ok(value.clone());
        };
        match &data.borrow().state {
            PromiseState::Fulfilled(v) => Ok(v.clone()),
            PromiseState::Rejected(v) => Ok(v.clone()),
            PromiseState::Pending { .. } => Err(JsError::new(
                ErrorKind::TypeError,
                "promise still pending".into(),
            )),
        }
    }

    #[test]
    fn constructor_and_is_array() {
        assert_eq!(run("typeof Array").unwrap().to_string(), "function");
        assert_eq!(number("Array(3).length"), 3.0);
        assert_eq!(number("new Array(1, 2, 3).length"), 3.0);
        assert_eq!(number("Array(\"3\").length"), 1.0);
        assert_eq!(joined("Array.of(1, 2, 3)"), "1,2,3");
        assert_eq!(number("Array.of().length"), 0.0);
        assert!(bool("Array.isArray([])"));
        assert!(!bool("Array.isArray({})"));
        assert!(bool("Array.isArray(new Array(3))"));
        assert!(!bool("Array.isArray(\"abc\")"));
        assert!(run("Array(3.5)").is_err());
        assert_eq!(joined("[1, 2, 3]"), "1,2,3");
        assert_eq!(number("[1, 2, 3].length"), 3.0);
    }

    #[test]
    fn from_uses_iterator_and_array_like_paths() {
        assert_eq!(joined("Array.from(\"ab\")"), "a,b");
        assert_eq!(joined("Array.from([1, 2, 3])"), "1,2,3");
        assert_eq!(joined("Array.from([1, 2, 3], x => x * 2)"), "2,4,6");
        assert_eq!(
            joined("Array.from({length: 3}, (_, i) => i * 10)"),
            "0,10,20"
        );
        assert_eq!(joined("Array.from({length: 2, 0: \"a\"})"), "a,");
        assert_eq!(number("Array.from([1,,3]).length"), 3.0);
        // Holes in the source read as undefined through the iterator.
        assert_eq!(
            joined("Array.from([1,,3]).map(x => x === undefined ? \"u\" : x)"),
            "1,u,3"
        );
    }

    #[test]
    fn mutation_methods() {
        assert_eq!(number("[1, 2].push(3)"), 3.0);
        assert_eq!(
            joined("(function(){ var a = [1, 2]; a.push(3); return a; })()"),
            "1,2,3"
        );
        assert_eq!(
            joined("(function(){ var a = [1, 2]; a.unshift(0); return a; })()"),
            "0,1,2"
        );
        assert_eq!(number("[1, 2, 3].pop()"), 3.0);
        assert_eq!(number("[1, 2, 3].shift()"), 1.0);
        assert_eq!(joined("[1, 2, 3].reverse()"), "3,2,1");
        assert_eq!(joined("[1, 2, 3, 4].copyWithin(0, 2)"), "3,4,3,4");
        assert_eq!(joined("[1, 2, 3].fill(9, 1)"), "1,9,9");
        assert_eq!(joined("[1, 2, 3].fill(0)"), "0,0,0");
        assert_eq!(joined("[1, 2, 3, 4, 5].splice(1, 2)"), "2,3");
        assert_eq!(
            joined("(function(){ var a = [1, 2, 3, 4]; a.splice(1, 1, 9); return a; })()"),
            "1,9,3,4"
        );
    }

    #[test]
    fn iteration_methods() {
        assert_eq!(joined("[1, 2, 3].map(x => x + 1)"), "2,3,4");
        assert_eq!(joined("[1, 2, 3, 4].filter(x => x % 2 === 0)"), "2,4");
        assert_eq!(number("[1, 2, 3].reduce((a, b) => a + b)"), 6.0);
        assert_eq!(number("[5, 4, 3].reduceRight((a, b) => a - b)"), -6.0);
        assert_eq!(number("[].reduce((a, b) => a + b, 7)"), 7.0);
        assert!(bool("[1, 2, 3].every(x => x > 0)"));
        assert!(!bool("[1, 2, 3].every(x => x > 1)"));
        assert!(bool("[1, 2, 3].some(x => x > 2)"));
        assert_eq!(number("[1, 2, 3].find(x => x > 1)"), 2.0);
        assert_eq!(number("[1, 2, 3].findIndex(x => x > 1)"), 1.0);
        assert_eq!(number("[1, 2, 3, 4, 5].findLast(x => x < 4)"), 3.0);
        assert_eq!(number("[1, 2, 3, 4, 5].findLastIndex(x => x > 2)"), 4.0);
        // Holes are skipped by the iteration methods that check HasProperty.
        assert_eq!(joined("[1,,3].map(x => x)"), "1,,3");
        assert!(bool("[1,,3].every(x => true)"));
        assert_eq!(joined("[1,,3].filter(x => true)"), "1,3");
        // find reads holes as undefined.
        assert_eq!(
            joined("[1,,3].find(x => x === undefined) === undefined ? \"y\" : \"n\""),
            "y"
        );
    }

    #[test]
    fn search_methods() {
        assert!(bool("[1, 2, 3].includes(2)"));
        assert!(!bool("[1, 2, 3].includes(2, 2)"));
        assert_eq!(number("[1, 2, 3, 4].indexOf(3)"), 2.0);
        assert_eq!(number("[1, 2, 3, 4].indexOf(2, 2)"), -1.0);
        assert_eq!(number("[1, 2, 3].lastIndexOf(1)"), 0.0);
        assert_eq!(number("[1, 2, 3, 4, 5].lastIndexOf(5)"), 4.0);
        assert!(!bool("[1, 2, 3].includes(undefined)"));
    }

    #[test]
    fn slicing_and_joining() {
        assert_eq!(joined("[1, 2, 3, 4, 5].slice(1, 3)"), "2,3");
        assert_eq!(joined("[1, 2, 3, 4, 5].slice(1)"), "2,3,4,5");
        assert_eq!(joined("[1, 2, 3].slice(-2)"), "2,3");
        assert_eq!(number("[1, 2, 3].slice(1).length"), 2.0);
        assert_eq!(joined("[1, 2, 3].concat([4, 5], 6)"), "1,2,3,4,5,6");
        assert_eq!(joined("[0].concat([1, 2, 3])"), "0,1,2,3");
        assert_eq!(text("[1, 2, 3].join(\"-\")"), "1-2-3");
        assert_eq!(text("[1, 2, 3].join()"), "1,2,3");
        assert_eq!(text("[1, [2, 3]].toString()"), "1,2,3");
        assert_eq!(text("[].toString()"), "");
        assert_eq!(text("[1, 2, 3] + \"\""), "1,2,3");
        assert_eq!(text("String([1, 2, 3])"), "1,2,3");
    }

    #[test]
    fn sort_is_stable_and_spec_ordered() {
        assert_eq!(joined("[3, 1, 2].sort()"), "1,2,3");
        assert_eq!(joined("[3, 1, 2].sort((a, b) => b - a)"), "3,2,1");
        assert_eq!(joined("[10, 9, 8].sort()"), "10,8,9");
        assert_eq!(number("[undefined, 2, undefined].sort().length"), 3.0);
        assert_eq!(joined("[undefined, 1].sort()"), "1,");
        // Stability: equal keys keep their relative order.
        assert_eq!(
            number("[[1,3],[1,2],[1,1],[2,0]].sort((a, b) => a[0] - b[0])[1][1]"),
            2.0
        );
        assert_eq!(joined("[1, 2, 3].toSorted((a, b) => b - a)"), "3,2,1");
        assert_eq!(joined("[1, 2, 3].toReversed()"), "3,2,1");
        assert_eq!(joined("[1, 2, 3, 4].toSpliced(1, 2, 9, 9)"), "1,9,9,4");
        assert_eq!(joined("[1, 2, 3].with(1, 99)"), "1,99,3");
    }

    #[test]
    fn flattening() {
        assert_eq!(joined("[1, [2, [3]]].flat()"), "1,2,3");
        assert_eq!(joined("[1, [2, [3]]].flat(2)"), "1,2,3");
        assert_eq!(joined("[1, [2, [3]]].flat(0)"), "1,2,3");
        assert_eq!(joined("[1, 2].flatMap(x => [x, x])"), "1,1,2,2");
        assert_eq!(number("[1, [2, [3]]].flat().length"), 3.0);
        // Holes in the source are skipped by flat.
        assert_eq!(joined("[1,,3].flat()"), "1,3");
    }

    #[test]
    fn array_iterator_protocol() {
        assert_eq!(number("[1, 2, 3].values().next().value"), 1.0);
        assert_eq!(number("[1, 2, 3].keys().next().value"), 0.0);
        assert_eq!(joined("[10, 20].entries().next().value"), "0,10");
        assert_eq!(number("[10, 20].entries().next().value[1]"), 10.0);
        assert_eq!(
            text(
                "(function(){ var it = [7, 8].values(); it.next(); return String(it.next().value); })()"
            ),
            "8"
        );
        assert!(bool("[].values().next().done"));
        assert!(!bool("[1].values().next().done"));
        assert!(bool(
            "(function(){ var it = [1].values(); it.next(); return it.next().done; })()"
        ));
        // for-of over arrays and strings.
        assert_eq!(
            number(
                "(function(){ var s = 0; for (var x of [1, 2, 3, 4]) { s += x; } return s; })()"
            ),
            10.0
        );
        assert_eq!(
            text("(function(){ var s = \"\"; for (var x of \"ab\") { s += x; } return s; })()"),
            "ab"
        );
    }

    #[test]
    fn prototype_surface() {
        assert_eq!(text("[1, 2, 3].constructor === Array ? \"y\" : \"n\""), "y");
        assert_eq!(
            text("Array.prototype.constructor === Array ? \"y\" : \"n\""),
            "y"
        );
        assert_eq!(number("[1, 2, 3].at(-1)"), 3.0);
        assert_eq!(text("String([1, 2, 3].at(9))"), "undefined");
        assert_eq!(text("[1, 2, 3].constructor.name"), "Array");
        assert_eq!(
            text("Object.prototype.toString.call([1, 2])"),
            "[object Array]"
        );
        // The literal's prototype is %Array.prototype%.
        assert!(bool("Array.prototype.isPrototypeOf([1, 2])"));
        assert!(bool("[1, 2] instanceof Array"));
    }

    #[test]
    fn from_async_resolves_with_the_mapped_values() {
        let value = settle("Array.fromAsync([1, 2, 3])").unwrap();
        assert_eq!(joined_value(&value), "1,2,3");
        let value = settle("Array.fromAsync([1, 2, 3], x => x * 2)").unwrap();
        assert_eq!(joined_value(&value), "2,4,6");
        // Non-array array-likes are not (async) iterable: rejected per spec.
        assert!(matches!(
            settle("Array.fromAsync({length: 2}, (_, i) => i + 1)"),
            Ok(Value::Object(_))
        ));
        // A sync iterable is wrapped in an AsyncFromSyncIterator.
        let value = settle("Array.fromAsync(\"ab\")").unwrap();
        assert_eq!(joined_value(&value), "a,b");
        // The mapper may return a promise.
        let value = settle("Array.fromAsync([1, 2], x => Promise.resolve(x + 1))").unwrap();
        assert_eq!(joined_value(&value), "2,3");
    }

    fn joined_value(value: &Value) -> String {
        let Value::Object(obj) = value else {
            panic!("expected an array");
        };
        let length = obj.get(&JsString::from_utf8("length")).unwrap();
        let Value::Number(n) = length else {
            panic!("expected a numeric length");
        };
        let mut parts = Vec::new();
        for i in 0..n as u64 {
            let element = obj.get(&key(i)).unwrap();
            parts.push(
                crux::convert::to_string(&element)
                    .unwrap()
                    .to_string_lossy(),
            );
        }
        parts.join(",")
    }

    #[test]
    fn holes_and_sparse_arrays() {
        // `Array(3)` has length 3 and no own elements.
        assert_eq!(number("Array(3).length"), 3.0);
        assert_eq!(text("JSON.stringify(Array(3))"), "[null,null,null]");
        assert!(!bool("Array(3).hasOwnProperty(0)"));
        // map preserves holes but keeps the length.
        assert_eq!(joined("[,1,,3].map(x => x)"), ",1,,3");
        assert_eq!(number("[,1,,3].map(x => x).length"), 4.0);
        // join renders holes as empty segments.
        assert_eq!(text("[,,].join('-')"), "-");
        assert_eq!(text("[1,,3].join('-')"), "1--3");
        // for-of reads holes as undefined.
        assert_eq!(
            text(
                "(function(){ var s = ''; for (var x of [1,,3]) { s += x === undefined ? 'u' : x; } return s; })()"
            ),
            "1u3"
        );
        // forEach skips holes.
        assert_eq!(
            number("(function(){ var c = 0; [1,,3].forEach(function(){ c++; }); return c; })()"),
            2.0
        );
        assert!(!bool("[1,,3].hasOwnProperty(1)"));
        // Array.from and spread materialize holes as undefined.
        assert_eq!(joined("Array.from({length: 3})"), ",,");
        assert!(bool(
            "(function(){ var a = [...Array(3)]; return a.length === 3 && a[0] === undefined && a[2] === undefined; })()"
        ));
    }

    #[test]
    fn length_mutation_during_iteration() {
        // forEach captures the length up front: elements pushed beyond it are
        // never visited.
        assert_eq!(
            number(
                "(function(){ var a = [1, 2, 3]; var c = 0; a.forEach(function(){ c++; a.push(4); }); return c; })()"
            ),
            3.0
        );
        assert_eq!(
            number(
                "(function(){ var a = [1, 2, 3]; a.forEach(function(){ a.push(4); }); return a.length; })()"
            ),
            6.0
        );
        // Popping during forEach can remove elements before the iteration
        // reaches them (the captured length stays 3, but hasProperty fails).
        assert_eq!(
            number(
                "(function(){ var a = [1, 2, 3]; var c = 0; a.forEach(function(){ c++; a.pop(); }); return c; })()"
            ),
            2.0
        );
        assert_eq!(
            joined(
                "(function(){ var a = [1, 2, 3]; a.forEach(function(){ a.pop(); }); return a; })()"
            ),
            "1"
        );
        // The values iterator reads the current index each step, so shifting
        // during for-of skips the elements that move past it (1, then 3).
        assert_eq!(
            number(
                "(function(){ var a = [1, 2, 3]; var s = 0; for (var v of a) { s += v; a.shift(); } return s; })()"
            ),
            4.0
        );
    }

    #[test]
    fn species_constructor() {
        // The default species of an Array subclass is the subclass itself.
        assert!(bool(
            "class MyArr extends Array {}; new MyArr(1, 2, 3).map(x => x) instanceof MyArr"
        ));
        assert!(bool(
            "class MyArr extends Array {}; new MyArr(1, 2, 3).map(x => x).constructor === MyArr"
        ));
        // concat on a plain array keeps the plain-array species; the subclass
        // argument is spread in.
        assert_eq!(
            joined(
                "(function(){ class MyArr extends Array {}; return [1, 2, 3].concat(new MyArr([4])); })()"
            ),
            "1,2,3,4"
        );
        assert!(!bool(
            "(function(){ class MyArr extends Array {}; return [1, 2, 3].concat(new MyArr([4])) instanceof MyArr; })()"
        ));
    }

    #[test]
    fn species_accessor_lives_on_the_constructor() {
        // spec 23.1.2.5: get Array [ @@species ] returns the constructor.
        assert!(bool("Array[Symbol.species] === Array"));
        assert_eq!(
            run("typeof Array[Symbol.species]").unwrap().to_string(),
            "function"
        );
        // Array.prototype has no @@species of its own.
        assert_eq!(
            run("Object.hasOwn(Array.prototype, Symbol.species)").unwrap(),
            Value::Boolean(false)
        );
        // An overridden @@species is honored by species-creating methods.
        assert!(bool(
            "class MyArr extends Array { static get [Symbol.species]() { return Array; } } !(new MyArr(1,2,3).map(x => x) instanceof MyArr)"
        ));
        assert!(bool(
            "class MyArr extends Array { static get [Symbol.species]() { return Array; } } new MyArr(1,2,3).map(x => x) instanceof Array"
        ));
        assert!(bool(
            "class MyArr extends Array { static get [Symbol.species]() { return Array; } } new MyArr(1,2).slice(0) instanceof Array"
        ));
    }

    #[test]
    fn splice_rejects_lengths_beyond_2_pow_53() {
        // spec 23.1.3.30 step 8: a resulting length above 2^53-1 throws
        // TypeError before any shifting (which would loop over the huge
        // tail).
        for source in [
            "(function(){ var a = {}; a.length = 2 ** 53 - 1; Array.prototype.splice.call(a, 0, 0, null); })()",
            "(function(){ var a = {}; a.length = 2 ** 53; Array.prototype.splice.call(a, 0, 0, null); })()",
            "(function(){ var a = {}; a.length = 2 ** 53 + 2; Array.prototype.splice.call(a, 0, 0, null); })()",
            "(function(){ var a = {}; a.length = Infinity; Array.prototype.splice.call(a, 0, 0, null); })()",
        ] {
            assert!(matches!(
                run(source),
                Err(error) if error.kind == crux::ErrorKind::TypeError
            ));
        }
    }

    #[test]
    fn sort_comparator_and_hole_edge_cases() {
        // Stability: equal keys keep their relative order.
        assert_eq!(
            text(
                "(function(){ var a = [{k:1,v:'a'},{k:1,v:'b'}]; a.sort(function(x,y){ return x.k - y.k; }); return a[0].v + a[1].v; })()"
            ),
            "ab"
        );
        // A comparator returning NaN treats the pair as equal.
        assert_eq!(joined("[2, 1, 3].sort(function(){ return NaN; })"), "2,1,3");
        // The default sort is lexicographic.
        assert_eq!(joined("[10, 1, 3].sort()"), "1,10,3");
        // undefined sinks to the end; null sorts as the string "null".
        assert_eq!(joined("[undefined, null, 1].sort()"), "1,,");
        // A non-callable comparator throws a TypeError.
        assert!(run("[1, 2, 3].sort(null)").is_err());
        // Holes are skipped and land at the tail of the sorted array.
        assert_eq!(number("[,1].sort().length"), 2.0);
        assert_eq!(number("[,1].sort()[0]"), 1.0);
        assert!(!bool("[,1].sort().hasOwnProperty(1)"));
    }

    #[test]
    fn from_iteration_and_reduce_edge_cases() {
        assert_eq!(joined("Array.from('abc')"), "a,b,c");
        assert_eq!(joined("Array.from(new Set([1, 2]))"), "1,2");
        assert_eq!(joined("Array.from({0:'a', 1:'b', length: 2})"), "a,b");
        // mapFn with an explicit thisArg.
        assert_eq!(
            joined("Array.from([1, 2, 3], function(x){ return x + this.base; }, {base: 10})"),
            "11,12,13"
        );
        // flat with an explicit depth.
        assert!(bool("[[1],[2,[3]]].flat(1)[2] instanceof Array"));
        assert!(!bool("[[1],[2,[3]]].flat(2)[2] instanceof Array"));
        assert_eq!(joined("[[1],[2,[3]]].flat(2)"), "1,2,3");
        // flatMap skips holes.
        assert_eq!(joined("[1,,2].flatMap(x => [x])"), "1,2");
        // %Array.prototype[@@iterator]% is the values method.
        assert!(bool("[1, 2, 3][Symbol.iterator] === [1, 2, 3].values"));
        // reduce without an initial value on an empty array throws.
        assert!(run("[].reduce(function(a, b){ return a + b; })").is_err());
        // reduceRight folds from the right.
        assert_eq!(
            number("[1, 2, 3].reduceRight(function(a, b){ return a - b; })"),
            0.0
        );
        // indexOf uses strict equality; includes uses SameValueZero.
        assert_eq!(number("[NaN].indexOf(NaN)"), -1.0);
        assert!(bool("[NaN].includes(NaN)"));
        // lastIndexOf with a negative fromIndex.
        assert_eq!(number("[1, 2, 3].lastIndexOf(1, -1)"), 0.0);
        // find returns undefined both for a found undefined and for no match.
        assert!(bool("[undefined].find(x => x === undefined) === undefined"));
        assert!(bool("[1, 2].find(x => x > 9) === undefined"));
    }
}
