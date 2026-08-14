//! The keyed-collection built-ins (spec ch. 24): Map, Set, WeakMap, and
//! WeakSet, their iterators, and the ES2025 set methods (`union`,
//! `intersection`, `difference`, `symmetricDifference`, `isSubsetOf`,
//! `isSupersetOf`, `isDisjointFrom`). Instances are ordinary objects whose
//! `[[MapData]]`-style List lives in the agent's `*_data` tables keyed by
//! object identity; deleted entries stay in the List as ~empty~ (`None`)
//! slots so suspended iterators keep working, exactly like the spec.

use std::cell::RefCell;

use crux::convert::{require_object_coercible, to_boolean, to_integer_or_infinity};
use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::ops::same_value;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, is_callable};

use crate::agent::Agent;
use crate::context::{as_object, get_property};
use crate::expr::IteratorRecord;
use crate::realm::Realm;

const MAP: &str = "%Map%";
const MAP_PROTO: &str = "%Map.prototype%";
const MAP_CLEAR: &str = "%Map.prototype.clear%";
const MAP_DELETE: &str = "%Map.prototype.delete%";
const MAP_ENTRIES: &str = "%Map.prototype.entries%";
const MAP_FOR_EACH: &str = "%Map.prototype.forEach%";
const MAP_GET: &str = "%Map.prototype.get%";
const MAP_GET_OR_INSERT: &str = "%Map.prototype.getOrInsert%";
const MAP_GET_OR_INSERT_COMPUTED: &str = "%Map.prototype.getOrInsertComputed%";
const MAP_HAS: &str = "%Map.prototype.has%";
const MAP_KEYS: &str = "%Map.prototype.keys%";
const MAP_SET: &str = "%Map.prototype.set%";
const MAP_VALUES: &str = "%Map.prototype.values%";
const MAP_SIZE: &str = "%get Map.prototype.size%";
const MAP_GROUP_BY: &str = "%Map.groupBy%";
const MAP_SPECIES: &str = "%get Map[Symbol.species]%";
const MAP_ITERATOR: &str = "%MapIteratorPrototype%";
const MAP_ITERATOR_NEXT: &str = "%MapIteratorPrototype.next%";

const SET: &str = "%Set%";
const SET_PROTO: &str = "%Set.prototype%";
const SET_ADD: &str = "%Set.prototype.add%";
const SET_CLEAR: &str = "%Set.prototype.clear%";
const SET_DELETE: &str = "%Set.prototype.delete%";
const SET_DIFFERENCE: &str = "%Set.prototype.difference%";
const SET_ENTRIES: &str = "%Set.prototype.entries%";
const SET_FOR_EACH: &str = "%Set.prototype.forEach%";
const SET_HAS: &str = "%Set.prototype.has%";
const SET_INTERSECTION: &str = "%Set.prototype.intersection%";
const SET_IS_DISJOINT_FROM: &str = "%Set.prototype.isDisjointFrom%";
const SET_IS_SUBSET_OF: &str = "%Set.prototype.isSubsetOf%";
const SET_IS_SUPERSET_OF: &str = "%Set.prototype.isSupersetOf%";
const SET_SYMMETRIC_DIFFERENCE: &str = "%Set.prototype.symmetricDifference%";
const SET_UNION: &str = "%Set.prototype.union%";
const SET_VALUES: &str = "%Set.prototype.values%";
const SET_SIZE: &str = "%get Set.prototype.size%";
const SET_SPECIES: &str = "%get Set[Symbol.species]%";
const SET_ITERATOR: &str = "%SetIteratorPrototype%";
const SET_ITERATOR_NEXT: &str = "%SetIteratorPrototype.next%";

const WEAK_MAP: &str = "%WeakMap%";
const WEAK_MAP_PROTO: &str = "%WeakMap.prototype%";
const WEAK_MAP_DELETE: &str = "%WeakMap.prototype.delete%";
const WEAK_MAP_GET: &str = "%WeakMap.prototype.get%";
const WEAK_MAP_GET_OR_INSERT: &str = "%WeakMap.prototype.getOrInsert%";
const WEAK_MAP_GET_OR_INSERT_COMPUTED: &str = "%WeakMap.prototype.getOrInsertComputed%";
const WEAK_MAP_HAS: &str = "%WeakMap.prototype.has%";
const WEAK_MAP_SET: &str = "%WeakMap.prototype.set%";

const WEAK_SET: &str = "%WeakSet%";
const WEAK_SET_PROTO: &str = "%WeakSet.prototype%";
const WEAK_SET_ADD: &str = "%WeakSet.prototype.add%";
const WEAK_SET_DELETE: &str = "%WeakSet.prototype.delete%";
const WEAK_SET_HAS: &str = "%WeakSet.prototype.has%";

/// The [[MapIterationKind]] of a Map iterator (spec 24.1.6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MapIterationKind {
    KeyValue,
    Key,
    Value,
}

impl MapIterationKind {
    fn code(self) -> u8 {
        match self {
            MapIterationKind::KeyValue => 0,
            MapIterationKind::Key => 1,
            MapIterationKind::Value => 2,
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            1 => MapIterationKind::Key,
            2 => MapIterationKind::Value,
            _ => MapIterationKind::KeyValue,
        }
    }
}

/// The [[SetIterationKind]] of a Set iterator (spec 24.2.6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetIterationKind {
    KeyValue,
    Value,
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

/// The default prototype (spec OrdinaryCreateFromConstructor): `prototype`
/// from NewTarget when it is an object, else the realm's named prototype.
fn get_prototype_from_constructor(
    agent: &mut Agent,
    constructor: &Value,
    default: &str,
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
            let default = agent
                .current_realm()?
                .intrinsics
                .get(default)
                .and_then(|value| as_object(&value))
                .ok_or_else(|| {
                    JsError::new(ErrorKind::TypeError, format!("{default} is not defined"))
                })?;
            Ok(default)
        }
    }
}

/// CanonicalizeKeyedCollectionKey (spec 24.1.1.1): -0𝔽 → +0𝔽.
fn canonicalize_key(key: Value) -> Value {
    match key {
        Value::Number(number) if number == 0.0 && number.is_sign_negative() => Value::Number(0.0),
        other => other,
    }
}

/// Find the index of the entry whose key SameValue-matches (after
/// canonicalization), or `None` (spec SetDataIndex/MapData scan).
fn find_index(map: &[Option<(Value, Value)>], key: &Value) -> Option<usize> {
    map.iter().position(|entry| match entry {
        Some((existing, _)) => same_value(existing, key),
        None => false,
    })
}

/// Find the index of a Set element, or `None` (spec SetDataIndex).
fn find_set_index(set: &[Option<Value>], value: &Value) -> Option<usize> {
    set.iter()
        .position(|entry| matches!(entry, Some(existing) if same_value(existing, value)))
}

/// The number of live (non-~empty~) entries (spec MapDataSize/SetDataSize).
fn live_count(map: &[Option<(Value, Value)>]) -> usize {
    map.iter().filter(|entry| entry.is_some()).count()
}

/// RequireInternalSlot: `this` is an object registered in the map table.
fn map_of(agent: &Agent, this: &Value) -> Result<Handle<JsObject>, JsError> {
    let Value::Object(object) = this else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Method called on an incompatible receiver".into(),
        ));
    };
    if !agent.map_data.contains_key(&object.id()) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Method called on an incompatible receiver".into(),
        ));
    }
    Ok(object.clone())
}

fn set_of(agent: &Agent, this: &Value) -> Result<Handle<JsObject>, JsError> {
    let Value::Object(object) = this else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Method called on an incompatible receiver".into(),
        ));
    };
    if !agent.set_data.contains_key(&object.id()) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Method called on an incompatible receiver".into(),
        ));
    }
    Ok(object.clone())
}

/// The number of live elements of a Set's [[SetData]].
fn set_data_count(agent: &Agent, id: u64) -> usize {
    agent
        .set_data
        .get(&id)
        .map(|cell| cell.borrow().iter().filter(|entry| entry.is_some()).count())
        .unwrap_or(0)
}

/// The element at `index` of the live [[SetData]], or `None` past the end.
/// The set-methods' per-element loops re-read the list each step, because a
/// user `has`/`keys` call can mutate the receiver (set-like-class-mutation).
fn set_data_at(agent: &Agent, id: u64, index: usize) -> Option<Value> {
    agent
        .set_data
        .get(&id)?
        .borrow()
        .get(index)
        .cloned()
        .flatten()
}

/// Whether the live [[SetData]] contains `value`.
fn set_data_contains(agent: &Agent, id: u64, value: &Value) -> bool {
    let data = agent.set_data.get(&id).map(|cell| cell.borrow()).unwrap();
    find_set_index(&data, value).is_some()
}

fn weak_map_of(agent: &Agent, this: &Value) -> Result<Handle<JsObject>, JsError> {
    let Value::Object(object) = this else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Method called on an incompatible receiver".into(),
        ));
    };
    if !agent.weak_map_data.contains_key(&object.id()) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Method called on an incompatible receiver".into(),
        ));
    }
    Ok(object.clone())
}

fn weak_set_of(agent: &Agent, this: &Value) -> Result<Handle<JsObject>, JsError> {
    let Value::Object(object) = this else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Method called on an incompatible receiver".into(),
        ));
    };
    if !agent.weak_set_data.contains_key(&object.id()) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Method called on an incompatible receiver".into(),
        ));
    }
    Ok(object.clone())
}

/// CanBeHeldWeakly (spec 26.1.1): Object, or a Symbol without a global
/// registry entry (`Symbol.for` symbols lack language identity).
pub(crate) fn can_be_held_weakly(agent: &Agent, value: &Value) -> bool {
    match value {
        Value::Object(_) | Value::Function(_) => true,
        Value::Symbol(symbol) => {
            let registry = agent.global_symbol_registry.borrow();
            !registry.iter().any(|(_, s)| s.id == symbol.id)
        }
        _ => false,
    }
}

/// A CreateIteratorResultObject (spec 7.4.7.2).
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

/// Map.prototype.set (spec 24.1.3.11): update in place or append.
fn map_set(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = map_of(agent, this)?;
    let key = canonicalize_key(args.first().cloned().unwrap_or(Value::Undefined));
    let value = args.get(1).cloned().unwrap_or(Value::Undefined);
    let mut data = agent.map_data.get(&object.id()).unwrap().borrow_mut();
    if let Some(index) = find_index(&data, &key) {
        if let Some(entry) = &mut data[index] {
            entry.1 = value;
        }
    } else {
        data.push(Some((key, value)));
    }
    Ok(this.clone())
}

/// Map.prototype.delete (spec 24.1.3.3): mark the entry ~empty~.
fn map_delete(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = map_of(agent, this)?;
    let key = canonicalize_key(args.first().cloned().unwrap_or(Value::Undefined));
    let mut data = agent.map_data.get(&object.id()).unwrap().borrow_mut();
    if let Some(index) = find_index(&data, &key) {
        data[index] = None;
        Ok(Value::Boolean(true))
    } else {
        Ok(Value::Boolean(false))
    }
}

/// Map.prototype.get (spec 24.1.3.6).
fn map_get(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = map_of(agent, this)?;
    let key = canonicalize_key(args.first().cloned().unwrap_or(Value::Undefined));
    let data = agent.map_data.get(&object.id()).unwrap().borrow();
    Ok(match find_index(&data, &key) {
        Some(index) => data[index]
            .as_ref()
            .map(|entry| entry.1.clone())
            .unwrap_or(Value::Undefined),
        None => Value::Undefined,
    })
}

/// Map.prototype.has (spec 24.1.3.7).
fn map_has(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = map_of(agent, this)?;
    let key = canonicalize_key(args.first().cloned().unwrap_or(Value::Undefined));
    let data = agent.map_data.get(&object.id()).unwrap().borrow();
    Ok(Value::Boolean(find_index(&data, &key).is_some()))
}

/// Map.prototype.clear (spec 24.1.3.1): every entry becomes ~empty~ but the
/// List is preserved for suspended iterators.
fn map_clear(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let object = map_of(agent, this)?;
    let mut data = agent.map_data.get(&object.id()).unwrap().borrow_mut();
    for entry in data.iter_mut() {
        *entry = None;
    }
    Ok(Value::Undefined)
}

/// get Map.prototype.size (spec 24.1.3.9).
fn map_size(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let object = map_of(agent, this)?;
    let data = agent.map_data.get(&object.id()).unwrap().borrow();
    Ok(Value::Number(live_count(&data) as f64))
}

/// Map.prototype.getOrInsert (spec 24.1.3.6.1).
fn map_get_or_insert(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = map_of(agent, this)?;
    let key = canonicalize_key(args.first().cloned().unwrap_or(Value::Undefined));
    let value = args.get(1).cloned().unwrap_or(Value::Undefined);
    let mut data = agent.map_data.get(&object.id()).unwrap().borrow_mut();
    if let Some(index) = find_index(&data, &key) {
        let value = data[index]
            .as_ref()
            .map(|entry| entry.1.clone())
            .unwrap_or(Value::Undefined);
        return Ok(value);
    }
    data.push(Some((key, value.clone())));
    Ok(value)
}

/// Map.prototype.getOrInsertComputed (spec 24.1.3.7.1): the callback runs
/// only on a miss; the Map may change while it runs.
fn map_get_or_insert_computed(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let object = map_of(agent, this)?;
    let key = canonicalize_key(args.first().cloned().unwrap_or(Value::Undefined));
    let callback = args.get(1).cloned().unwrap_or(Value::Undefined);
    if !is_callable(&callback) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "getOrInsertComputed: callback is not a function".into(),
        ));
    }
    {
        let data = agent.map_data.get(&object.id()).unwrap().borrow();
        if let Some(index) = find_index(&data, &key) {
            let value = data[index]
                .as_ref()
                .map(|entry| entry.1.clone())
                .unwrap_or(Value::Undefined);
            return Ok(value);
        }
    }
    let value = crate::function::call(
        agent,
        &callback,
        Value::Undefined,
        std::slice::from_ref(&key),
    )?;
    let mut data = agent.map_data.get(&object.id()).unwrap().borrow_mut();
    if let Some(index) = find_index(&data, &key) {
        if let Some(entry) = &mut data[index] {
            entry.1 = value.clone();
        }
    } else {
        data.push(Some((key, value.clone())));
    }
    Ok(value)
}

/// Map.prototype.forEach (spec 24.1.3.5): visit live entries in insertion
/// order; the count refreshes after each callback so new keys are visited.
fn map_for_each(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = map_of(agent, this)?;
    let callback = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&callback) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Map.prototype.forEach: callback is not a function".into(),
        ));
    }
    let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    let mut index = 0usize;
    loop {
        let (entry, count) = {
            let data = agent.map_data.get(&object.id()).unwrap().borrow();
            (data.get(index).cloned().flatten(), data.len())
        };
        if index >= count {
            break;
        }
        index += 1;
        if let Some((key, value)) = entry {
            crate::function::call(
                agent,
                &callback,
                this_arg.clone(),
                &[value, key, this.clone()],
            )?;
        }
    }
    Ok(Value::Undefined)
}

/// CreateMapIterator (spec 24.1.6.1): an ordinary object holding the map,
/// the next index, and the kind in the agent's `map_iter_data`.
fn create_map_iterator(
    agent: &mut Agent,
    map: &Value,
    kind: MapIterationKind,
) -> Result<Value, JsError> {
    map_of(agent, map)?;
    let proto = agent
        .current_realm()?
        .intrinsics
        .get(MAP_ITERATOR)
        .and_then(|value| as_object(&value))
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "%MapIteratorPrototype% missing".into(),
            )
        })?;
    let iterator = JsObject::ordinary_object_create(Some(proto));
    agent.map_iter_data.insert(
        iterator.id(),
        RefCell::new((Some(map.clone()), 0usize, kind.code())),
    );
    Ok(Value::Object(iterator))
}

/// %MapIteratorPrototype%.next (spec 24.1.6.1): scan forward from the next
/// index, skipping ~empty~ slots and wrapping within the current List length;
/// a full pass without a live entry finishes the iterator.
fn map_iterator_next(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let Value::Object(iterator) = this else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "%MapIteratorPrototype%.next called on an incompatible receiver".into(),
        ));
    };
    let id = iterator.id();
    if !agent.map_iter_data.contains_key(&id) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "%MapIteratorPrototype%.next called on an incompatible receiver".into(),
        ));
    }
    let (map, mut index, kind_code) = {
        let state = agent.map_iter_data.get(&id).unwrap().borrow();
        state.clone()
    };
    let Some(map_value) = map else {
        return iter_result(agent, Value::Undefined, true);
    };
    let kind = MapIterationKind::from_code(kind_code);
    let map_object = as_object(&map_value).ok_or_else(|| {
        JsError::new(ErrorKind::TypeError, "Iterated Map is not an object".into())
    })?;
    // A per-call snapshot: no user code runs between the read and the
    // result, so entries added after this call are seen by the next one.
    let data = agent
        .map_data
        .get(&map_object.id())
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "Iterated Map is not a Map".into()))?
        .borrow()
        .clone();
    let len = data.len();
    while index < len {
        let entry = data[index].clone();
        index += 1;
        if let Some((key, value)) = entry {
            let result = match kind {
                MapIterationKind::KeyValue => {
                    crate::builtins::array::array_from_values(agent, &[key, value])?
                }
                MapIterationKind::Key => key,
                MapIterationKind::Value => value,
            };
            let mut state = agent.map_iter_data.get(&id).unwrap().borrow_mut();
            state.0 = Some(map_value);
            state.1 = index;
            return iter_result(agent, result, false);
        }
    }
    let mut state = agent.map_iter_data.get(&id).unwrap().borrow_mut();
    state.0 = None;
    iter_result(agent, Value::Undefined, true)
}

/// Map.prototype.entries (spec 24.1.3.4).
fn map_entries(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    map_of(agent, this)?;
    create_map_iterator(agent, this, MapIterationKind::KeyValue)
}

/// Map.prototype.keys (spec 24.1.3.8).
fn map_keys(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    map_of(agent, this)?;
    create_map_iterator(agent, this, MapIterationKind::Key)
}

/// Map.prototype.values (spec 24.1.3.12).
fn map_values(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    map_of(agent, this)?;
    create_map_iterator(agent, this, MapIterationKind::Value)
}

/// The `get Map.prototype[Symbol.species]` accessor (spec 24.1.2.2): `this`.
fn species_getter(_agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    Ok(this.clone())
}

/// Set.prototype.add (spec 24.2.3.1): append when the value is new.
fn set_add(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = set_of(agent, this)?;
    let value = canonicalize_key(args.first().cloned().unwrap_or(Value::Undefined));
    let mut data = agent.set_data.get(&object.id()).unwrap().borrow_mut();
    if !find_set_index(&data, &value).is_some() {
        data.push(Some(value));
    }
    Ok(this.clone())
}

/// Set.prototype.delete (spec 24.2.3.4).
fn set_delete(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = set_of(agent, this)?;
    let value = canonicalize_key(args.first().cloned().unwrap_or(Value::Undefined));
    let mut data = agent.set_data.get(&object.id()).unwrap().borrow_mut();
    if let Some(index) = find_set_index(&data, &value) {
        data[index] = None;
        Ok(Value::Boolean(true))
    } else {
        Ok(Value::Boolean(false))
    }
}

/// Set.prototype.has (spec 24.2.3.8).
fn set_has(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = set_of(agent, this)?;
    let value = canonicalize_key(args.first().cloned().unwrap_or(Value::Undefined));
    let data = agent.set_data.get(&object.id()).unwrap().borrow();
    Ok(Value::Boolean(find_set_index(&data, &value).is_some()))
}

/// Set.prototype.clear (spec 24.2.3.2).
fn set_clear(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let object = set_of(agent, this)?;
    let mut data = agent.set_data.get(&object.id()).unwrap().borrow_mut();
    for entry in data.iter_mut() {
        *entry = None;
    }
    Ok(Value::Undefined)
}

/// get Set.prototype.size (spec 24.2.3.9).
fn set_size(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let object = set_of(agent, this)?;
    let data = agent.set_data.get(&object.id()).unwrap().borrow();
    let count = data.iter().filter(|entry| entry.is_some()).count();
    Ok(Value::Number(count as f64))
}

/// Set.prototype.forEach (spec 24.2.3.6): callback(value, value, set).
fn set_for_each(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = set_of(agent, this)?;
    let callback = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&callback) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Set.prototype.forEach: callback is not a function".into(),
        ));
    }
    let this_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    let mut index = 0usize;
    loop {
        let (entry, count) = {
            let data = agent.set_data.get(&object.id()).unwrap().borrow();
            (data.get(index).cloned().flatten(), data.len())
        };
        if index >= count {
            break;
        }
        index += 1;
        if let Some(value) = entry {
            crate::function::call(
                agent,
                &callback,
                this_arg.clone(),
                &[value.clone(), value, this.clone()],
            )?;
        }
    }
    Ok(Value::Undefined)
}

/// CreateSetIterator (spec 24.2.6.1).
fn create_set_iterator(
    agent: &mut Agent,
    set: &Value,
    kind: SetIterationKind,
) -> Result<Value, JsError> {
    set_of(agent, set)?;
    let proto = agent
        .current_realm()?
        .intrinsics
        .get(SET_ITERATOR)
        .and_then(|value| as_object(&value))
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "%SetIteratorPrototype% missing".into(),
            )
        })?;
    let iterator = JsObject::ordinary_object_create(Some(proto));
    let kind_code = match kind {
        SetIterationKind::KeyValue => 0u8,
        SetIterationKind::Value => 1u8,
    };
    agent.set_iter_data.insert(
        iterator.id(),
        RefCell::new((Some(set.clone()), 0usize, kind_code)),
    );
    Ok(Value::Object(iterator))
}

/// %SetIteratorPrototype%.next (spec 24.2.6.1).
fn set_iterator_next(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let Value::Object(iterator) = this else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "%SetIteratorPrototype%.next called on an incompatible receiver".into(),
        ));
    };
    let id = iterator.id();
    if !agent.set_iter_data.contains_key(&id) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "%SetIteratorPrototype%.next called on an incompatible receiver".into(),
        ));
    }
    let (set, mut index, kind_code) = {
        let state = agent.set_iter_data.get(&id).unwrap().borrow();
        state.clone()
    };
    let Some(set_value) = set else {
        return iter_result(agent, Value::Undefined, true);
    };
    let set_object = as_object(&set_value).ok_or_else(|| {
        JsError::new(ErrorKind::TypeError, "Iterated Set is not an object".into())
    })?;
    let data = agent
        .set_data
        .get(&set_object.id())
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "Iterated Set is not a Set".into()))?
        .borrow()
        .clone();
    let len = data.len();
    while index < len {
        let entry = data[index].clone();
        index += 1;
        if let Some(value) = entry {
            let result = match kind_code {
                0 => crate::builtins::array::array_from_values(agent, &[value.clone(), value])?,
                _ => value,
            };
            let mut state = agent.set_iter_data.get(&id).unwrap().borrow_mut();
            state.0 = Some(set_value);
            state.1 = index;
            return iter_result(agent, result, false);
        }
    }
    let mut state = agent.set_iter_data.get(&id).unwrap().borrow_mut();
    state.0 = None;
    iter_result(agent, Value::Undefined, true)
}

/// Set.prototype.entries (spec 24.2.3.5).
fn set_entries(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    set_of(agent, this)?;
    create_set_iterator(agent, this, SetIterationKind::KeyValue)
}

/// Set.prototype.values (spec 24.2.3.11); `keys` aliases it.
fn set_values(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    set_of(agent, this)?;
    create_set_iterator(agent, this, SetIterationKind::Value)
}

/// GetSetRecord (spec 24.2.1.1): the `size`/`has`/`keys` surface of `other`.
struct SetRecord {
    object: Value,
    size: usize,
    has: Value,
    keys: Value,
}

fn get_set_record(agent: &mut Agent, other: &Value) -> Result<SetRecord, JsError> {
    if !matches!(other, Value::Object(_) | Value::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Set method argument is not an object".into(),
        ));
    }
    let raw_size = get(agent, other, &JsString::from_utf8("size"))?;
    let number_size = crate::context::to_number(agent, &raw_size)?;
    if number_size.is_nan() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Set-like object has no valid size".into(),
        ));
    }
    let int_size = to_integer_or_infinity(number_size);
    if int_size < 0.0 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "Set size is negative".into(),
        ));
    }
    let has = get(agent, other, &JsString::from_utf8("has"))?;
    if !is_callable(&has) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Set-like object's has is not a function".into(),
        ));
    }
    let keys = get(agent, other, &JsString::from_utf8("keys"))?;
    if !is_callable(&keys) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Set-like object's keys is not a function".into(),
        ));
    }
    Ok(SetRecord {
        object: other.clone(),
        size: int_size as usize,
        has,
        keys,
    })
}

fn get(agent: &mut Agent, value: &Value, name: &JsString) -> Result<Value, JsError> {
    get_property(agent, value, name, value.clone())
}

/// GetIteratorFromMethod (spec 7.4.3): call `method` on `obj` and extract
/// `next`.
fn get_iterator_from_method(
    agent: &mut Agent,
    obj: &Value,
    method: &Value,
) -> Result<IteratorRecord, JsError> {
    let iterator = crate::function::call(agent, method, obj.clone(), &[])?;
    if !matches!(iterator, Value::Object(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator must be an object".into(),
        ));
    }
    let next = get(agent, &iterator, &JsString::from_utf8("next"))?;
    if !is_callable(&next) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator's next method is not callable".into(),
        ));
    }
    Ok(IteratorRecord { iterator, next })
}

/// A fresh Set instance backed by `%Set.prototype%` (spec OrdinaryObjectCreate
/// with « [[SetData]] »): the result of the ES2025 set-methods.
fn new_set_from_data(agent: &mut Agent, data: Vec<Option<Value>>) -> Result<Value, JsError> {
    let proto = agent
        .current_realm()?
        .intrinsics
        .get(SET_PROTO)
        .and_then(|value| as_object(&value))
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "%Set.prototype% missing".into()))?;
    let set = JsObject::ordinary_object_create(Some(proto));
    agent.set_data.insert(set.id(), RefCell::new(data));
    Ok(Value::Object(set))
}

/// Set.prototype.union (spec 24.2.4.9): a copy of `this` plus every element
/// of `other` that is not already present.
fn set_union(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = set_of(agent, this)?;
    let other = args.first().cloned().unwrap_or(Value::Undefined);
    let record = get_set_record(agent, &other)?;
    let mut result = agent.set_data.get(&object.id()).unwrap().borrow().clone();
    let keys = get_iterator_from_method(agent, &record.object, &record.keys)?;
    loop {
        let next = match crate::expr::iterator_step(agent, &keys)? {
            Some(value) => value,
            None => break,
        };
        let value = canonicalize_key(next);
        if !find_set_index(&result, &value).is_some() {
            result.push(Some(value));
        }
    }
    new_set_from_data(agent, result)
}

/// Set.prototype.intersection (spec 24.2.4.3): elements of `this` present in
/// `other` (scanning the smaller side).
fn set_intersection(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = set_of(agent, this)?;
    let other = args.first().cloned().unwrap_or(Value::Undefined);
    let record = get_set_record(agent, &other)?;
    let data = agent.set_data.get(&object.id()).unwrap().borrow().clone();
    let this_size = data.iter().filter(|e| e.is_some()).count();
    let mut result: Vec<Option<Value>> = Vec::new();
    if this_size <= record.size {
        for value in data.iter().flatten() {
            let in_other = to_boolean(&crate::function::call(
                agent,
                &record.has,
                record.object.clone(),
                std::slice::from_ref(value),
            )?);
            if in_other && !find_set_index(&result, value).is_some() {
                result.push(Some(value.clone()));
            }
        }
    } else {
        let keys = get_iterator_from_method(agent, &record.object, &record.keys)?;
        loop {
            let next = match crate::expr::iterator_step(agent, &keys)? {
                Some(value) => value,
                None => break,
            };
            let value = canonicalize_key(next);
            if find_set_index(&data, &value).is_some() && !find_set_index(&result, &value).is_some()
            {
                result.push(Some(value));
            }
        }
    }
    new_set_from_data(agent, result)
}

/// Set.prototype.difference (spec 24.2.4.4): a copy of `this` minus every
/// element present in `other`.
fn set_difference(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = set_of(agent, this)?;
    let other = args.first().cloned().unwrap_or(Value::Undefined);
    let record = get_set_record(agent, &other)?;
    let data = agent.set_data.get(&object.id()).unwrap().borrow().clone();
    let this_size = data.iter().filter(|e| e.is_some()).count();
    let mut result = data.clone();
    if this_size <= record.size {
        let mut index = 0usize;
        while index < result.len() {
            let entry = result[index].clone();
            index += 1;
            if let Some(value) = entry {
                let in_other = to_boolean(&crate::function::call(
                    agent,
                    &record.has,
                    record.object.clone(),
                    &[value],
                )?);
                if in_other {
                    result[index - 1] = None;
                }
            }
        }
    } else {
        let keys = get_iterator_from_method(agent, &record.object, &record.keys)?;
        loop {
            let next = match crate::expr::iterator_step(agent, &keys)? {
                Some(value) => value,
                None => break,
            };
            let value = canonicalize_key(next);
            if let Some(index) = find_set_index(&result, &value) {
                result[index] = None;
            }
        }
    }
    new_set_from_data(agent, result)
}

/// Set.prototype.symmetricDifference (spec 24.2.4.8): elements of either set
/// not in both.
fn set_symmetric_difference(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let object = set_of(agent, this)?;
    let other = args.first().cloned().unwrap_or(Value::Undefined);
    let record = get_set_record(agent, &other)?;
    let id = object.id();
    // The result starts as a copy of [[SetData]]; each key's membership is
    // checked against the *live* set, because the key iterator's `next` can
    // mutate the receiver (set-like-class-mutation keeps values deleted by
    // the mutation and drops values re-added by it).
    let mut result = agent.set_data.get(&id).unwrap().borrow().clone();
    let keys = get_iterator_from_method(agent, &record.object, &record.keys)?;
    loop {
        let next = match crate::expr::iterator_step(agent, &keys)? {
            Some(value) => value,
            None => break,
        };
        let value = canonicalize_key(next);
        if find_set_index(&result, &value).is_none() {
            result.push(Some(value.clone()));
        }
        if set_data_contains(agent, id, &value)
            && let Some(index) = find_set_index(&result, &value)
        {
            result[index] = None;
        }
    }
    new_set_from_data(agent, result)
}

/// Set.prototype.isSubsetOf (spec 24.2.4.6): every element of `this` is in
/// `other`.
fn set_is_subset_of(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = set_of(agent, this)?;
    let other = args.first().cloned().unwrap_or(Value::Undefined);
    let record = get_set_record(agent, &other)?;
    let id = object.id();
    // The size is read once (spec step 3), then the loop walks the live
    // [[SetData]]: `has` can delete from the receiver mid-iteration, and
    // deleted entries must not be visited (set-like-class-mutation).
    if set_data_count(agent, id) > record.size {
        return Ok(Value::Boolean(false));
    }
    let mut index = 0;
    while let Some(value) = set_data_at(agent, id, index) {
        let in_other = to_boolean(&crate::function::call(
            agent,
            &record.has,
            record.object.clone(),
            std::slice::from_ref(&value),
        )?);
        if !in_other {
            return Ok(Value::Boolean(false));
        }
        index += 1;
    }
    Ok(Value::Boolean(true))
}

/// Set.prototype.isSupersetOf (spec 24.2.4.7): every element of `other` is in
/// `this`.
fn set_is_superset_of(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = set_of(agent, this)?;
    let other = args.first().cloned().unwrap_or(Value::Undefined);
    let record = get_set_record(agent, &other)?;
    let data = agent.set_data.get(&object.id()).unwrap().borrow().clone();
    let this_size = data.iter().filter(|e| e.is_some()).count();
    if this_size < record.size {
        return Ok(Value::Boolean(false));
    }
    let keys = get_iterator_from_method(agent, &record.object, &record.keys)?;
    loop {
        let next = match crate::expr::iterator_step(agent, &keys)? {
            Some(value) => value,
            None => break,
        };
        if !find_set_index(&data, &canonicalize_key(next)).is_some() {
            crate::expr::iterator_close(agent, &keys)?;
            return Ok(Value::Boolean(false));
        }
    }
    Ok(Value::Boolean(true))
}

/// Set.prototype.isDisjointFrom (spec 24.2.4.5): no element in both sets.
fn set_is_disjoint_from(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = set_of(agent, this)?;
    let other = args.first().cloned().unwrap_or(Value::Undefined);
    let record = get_set_record(agent, &other)?;
    let id = object.id();
    // The size is read once; the element loop walks the live [[SetData]]
    // (set-like-class-mutation: `has` deletes and re-adds mid-iteration).
    if set_data_count(agent, id) <= record.size {
        let mut index = 0;
        while let Some(value) = set_data_at(agent, id, index) {
            let in_other = to_boolean(&crate::function::call(
                agent,
                &record.has,
                record.object.clone(),
                std::slice::from_ref(&value),
            )?);
            if in_other {
                return Ok(Value::Boolean(false));
            }
            index += 1;
        }
        return Ok(Value::Boolean(true));
    }
    let keys = get_iterator_from_method(agent, &record.object, &record.keys)?;
    loop {
        let next = match crate::expr::iterator_step(agent, &keys)? {
            Some(value) => value,
            None => break,
        };
        if set_data_contains(agent, id, &canonicalize_key(next)) {
            crate::expr::iterator_close(agent, &keys)?;
            return Ok(Value::Boolean(false));
        }
    }
    Ok(Value::Boolean(true))
}

/// WeakMap.prototype.set (spec 26.3.3.7).
fn weak_map_set(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = weak_map_of(agent, this)?;
    let key = args.first().cloned().unwrap_or(Value::Undefined);
    if !can_be_held_weakly(agent, &key) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Invalid value used as weak map key".into(),
        ));
    }
    let value = args.get(1).cloned().unwrap_or(Value::Undefined);
    let mut data = agent.weak_map_data.get(&object.id()).unwrap().borrow_mut();
    if let Some(index) = find_index(&data, &key) {
        if let Some(entry) = &mut data[index] {
            entry.1 = value;
        }
    } else {
        data.push(Some((key, value)));
    }
    Ok(this.clone())
}

/// WeakMap.prototype.get (spec 26.3.3.4): undefined for non-weak keys.
fn weak_map_get(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = weak_map_of(agent, this)?;
    let key = args.first().cloned().unwrap_or(Value::Undefined);
    if !can_be_held_weakly(agent, &key) {
        return Ok(Value::Undefined);
    }
    let data = agent.weak_map_data.get(&object.id()).unwrap().borrow();
    Ok(match find_index(&data, &key) {
        Some(index) => data[index]
            .as_ref()
            .map(|entry| entry.1.clone())
            .unwrap_or(Value::Undefined),
        None => Value::Undefined,
    })
}

/// WeakMap.prototype.has (spec 26.3.3.5): false for non-weak keys.
fn weak_map_has(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = weak_map_of(agent, this)?;
    let key = args.first().cloned().unwrap_or(Value::Undefined);
    if !can_be_held_weakly(agent, &key) {
        return Ok(Value::Boolean(false));
    }
    let data = agent.weak_map_data.get(&object.id()).unwrap().borrow();
    Ok(Value::Boolean(find_index(&data, &key).is_some()))
}

/// WeakMap.prototype.delete (spec 26.3.3.2): false for non-weak keys.
fn weak_map_delete(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = weak_map_of(agent, this)?;
    let key = args.first().cloned().unwrap_or(Value::Undefined);
    if !can_be_held_weakly(agent, &key) {
        return Ok(Value::Boolean(false));
    }
    let mut data = agent.weak_map_data.get(&object.id()).unwrap().borrow_mut();
    if let Some(index) = find_index(&data, &key) {
        data[index] = None;
        Ok(Value::Boolean(true))
    } else {
        Ok(Value::Boolean(false))
    }
}

/// WeakMap.prototype.getOrInsert (spec 26.3.3.4.1).
fn weak_map_get_or_insert(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let object = weak_map_of(agent, this)?;
    let key = args.first().cloned().unwrap_or(Value::Undefined);
    if !can_be_held_weakly(agent, &key) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Invalid value used as weak map key".into(),
        ));
    }
    let value = args.get(1).cloned().unwrap_or(Value::Undefined);
    let mut data = agent.weak_map_data.get(&object.id()).unwrap().borrow_mut();
    if let Some(index) = find_index(&data, &key) {
        let value = data[index]
            .as_ref()
            .map(|entry| entry.1.clone())
            .unwrap_or(Value::Undefined);
        return Ok(value);
    }
    data.push(Some((key, value.clone())));
    Ok(value)
}

/// WeakMap.prototype.getOrInsertComputed (spec 26.3.3.5.1).
fn weak_map_get_or_insert_computed(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let object = weak_map_of(agent, this)?;
    let key = args.first().cloned().unwrap_or(Value::Undefined);
    if !can_be_held_weakly(agent, &key) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Invalid value used as weak map key".into(),
        ));
    }
    let callback = args.get(1).cloned().unwrap_or(Value::Undefined);
    if !is_callable(&callback) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "getOrInsertComputed: callback is not a function".into(),
        ));
    }
    {
        let data = agent.weak_map_data.get(&object.id()).unwrap().borrow();
        if let Some(index) = find_index(&data, &key) {
            let value = data[index]
                .as_ref()
                .map(|entry| entry.1.clone())
                .unwrap_or(Value::Undefined);
            return Ok(value);
        }
    }
    let value = crate::function::call(
        agent,
        &callback,
        Value::Undefined,
        std::slice::from_ref(&key),
    )?;
    let mut data = agent.weak_map_data.get(&object.id()).unwrap().borrow_mut();
    if let Some(index) = find_index(&data, &key) {
        if let Some(entry) = &mut data[index] {
            entry.1 = value.clone();
        }
    } else {
        data.push(Some((key, value.clone())));
    }
    Ok(value)
}

/// WeakSet.prototype.add (spec 26.4.3.1).
fn weak_set_add(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = weak_set_of(agent, this)?;
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    if !can_be_held_weakly(agent, &value) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Invalid value used in weak set".into(),
        ));
    }
    let mut data = agent.weak_set_data.get(&object.id()).unwrap().borrow_mut();
    if !find_set_index(&data, &value).is_some() {
        data.push(Some(value));
    }
    Ok(this.clone())
}

/// WeakSet.prototype.has (spec 26.4.3.3).
fn weak_set_has(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = weak_set_of(agent, this)?;
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    if !can_be_held_weakly(agent, &value) {
        return Ok(Value::Boolean(false));
    }
    let data = agent.weak_set_data.get(&object.id()).unwrap().borrow();
    Ok(Value::Boolean(find_set_index(&data, &value).is_some()))
}

/// WeakSet.prototype.delete (spec 26.4.3.2).
fn weak_set_delete(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let object = weak_set_of(agent, this)?;
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    if !can_be_held_weakly(agent, &value) {
        return Ok(Value::Boolean(false));
    }
    let mut data = agent.weak_set_data.get(&object.id()).unwrap().borrow_mut();
    if let Some(index) = find_set_index(&data, &value) {
        data[index] = None;
        Ok(Value::Boolean(true))
    } else {
        Ok(Value::Boolean(false))
    }
}

/// AddEntriesFromIterable (spec 24.1.1.1): iterate `[key, value]` pairs and
/// call the adder; any abrupt completion closes the iterator.
fn add_entries_from_iterable(
    agent: &mut Agent,
    target: &Value,
    iterable: &Value,
    adder: &Value,
) -> Result<Value, JsError> {
    let iterator = crate::expr::get_iterator(agent, iterable)?;
    loop {
        let next = match crate::expr::iterator_step(agent, &iterator)? {
            Some(value) => value,
            None => return Ok(target.clone()),
        };
        if !matches!(next, Value::Object(_)) {
            let error = JsError::new(
                ErrorKind::TypeError,
                "Iterator value is not an object".into(),
            );
            let _ = crate::expr::iterator_close(agent, &iterator);
            return Err(error);
        }
        // IfAbruptCloseIterator around each step: Get("0"), Get("1"), and
        // the adder call all close the iterator on failure (spec 24.1.1.1).
        let key = match get(agent, &next, &JsString::from_utf8("0")) {
            Ok(key) => key,
            Err(error) => {
                let _ = crate::expr::iterator_close(agent, &iterator);
                return Err(error);
            }
        };
        let value = match get(agent, &next, &JsString::from_utf8("1")) {
            Ok(value) => value,
            Err(error) => {
                let _ = crate::expr::iterator_close(agent, &iterator);
                return Err(error);
            }
        };
        if let Err(error) = crate::function::call(agent, adder, target.clone(), &[key, value]) {
            let _ = crate::expr::iterator_close(agent, &iterator);
            return Err(error);
        }
    }
}

/// GroupBy (spec 7.3.38): iterate `items` with GetIterator, call
/// `callback(value, k)` for each element, and group by the coerced key. The
/// callback and key-coercion completions close the iterator (IfAbruptCloseIterator);
/// a step-count overflow closes it with a TypeError. `coerce` maps a callback
/// result to the group key: ~property~ (ToPropertyKey) for Object.groupBy,
/// ~collection~ (CanonicalizeKeyedCollectionKey) for Map.groupBy.
pub(crate) fn group_by<F>(
    agent: &mut Agent,
    items: &Value,
    callback: &Value,
    coerce: F,
) -> Result<Vec<(Value, Vec<Value>)>, JsError>
where
    F: Fn(&mut Agent, Value) -> Result<Value, JsError>,
{
    require_object_coercible(items)?;
    if !is_callable(callback) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "groupBy: callback is not a function".into(),
        ));
    }
    let iterator = crate::expr::get_iterator(agent, items)?;
    let mut groups: Vec<(Value, Vec<Value>)> = Vec::new();
    let mut k = 0u64;
    loop {
        // spec step 6.a: a step-count overflow closes the iterator with a
        // TypeError.
        if k >= 9007199254740991 {
            let error = JsError::new(ErrorKind::TypeError, "Too many elements to group".into());
            let _ = crate::expr::iterator_close(agent, &iterator);
            return Err(error);
        }
        let next = match crate::expr::iterator_step(agent, &iterator)? {
            Some(value) => value,
            None => break,
        };
        let key = match crate::function::call(
            agent,
            callback,
            Value::Undefined,
            &[next.clone(), Value::Number(k as f64)],
        ) {
            Ok(key) => key,
            Err(error) => {
                let _ = crate::expr::iterator_close(agent, &iterator);
                return Err(error);
            }
        };
        let key = match coerce(agent, key) {
            Ok(key) => key,
            Err(error) => {
                let _ = crate::expr::iterator_close(agent, &iterator);
                return Err(error);
            }
        };
        match groups
            .iter_mut()
            .find(|(existing, _)| same_value(existing, &key))
        {
            Some((_, elements)) => elements.push(next),
            None => groups.push((key, vec![next])),
        }
        k += 1;
    }
    Ok(groups)
}

/// GroupBy (spec 7.3.38) with ~collection~ key coercion: `Map.groupBy`.
fn map_group_by(agent: &mut Agent, _this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let items = args.first().cloned().unwrap_or(Value::Undefined);
    let callback = args.get(1).cloned().unwrap_or(Value::Undefined);
    let groups = group_by(agent, &items, &callback, |_agent, key| {
        Ok(canonicalize_key(key))
    })?;
    let proto = agent
        .current_realm()?
        .intrinsics
        .get(MAP_PROTO)
        .and_then(|value| as_object(&value))
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "%Map.prototype% missing".into()))?;
    let map = JsObject::ordinary_object_create(Some(proto));
    let mut data = Vec::with_capacity(groups.len());
    for (key, elements) in groups {
        let array = crate::builtins::array::array_from_values(agent, &elements)?;
        data.push(Some((key, array)));
    }
    agent.map_data.insert(map.id(), RefCell::new(data));
    Ok(Value::Object(map))
}

/// The Map constructor (spec 24.1.1.1): NewTarget required; fill from the
/// iterable via the `set` adder.
fn map_construct(agent: &mut Agent, args: &[Value], new_target: &Value) -> Result<Value, JsError> {
    let proto = get_prototype_from_constructor(agent, new_target, MAP_PROTO)?;
    let map = JsObject::ordinary_object_create(Some(proto));
    agent.map_data.insert(map.id(), RefCell::new(Vec::new()));
    let map_value = Value::Object(map);
    let iterable = args.first().cloned().unwrap_or(Value::Undefined);
    if matches!(iterable, Value::Undefined | Value::Null) {
        return Ok(map_value);
    }
    let adder = get(agent, &map_value, &JsString::from_utf8("set"))?;
    if !is_callable(&adder) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Map constructor: set is not a function".into(),
        ));
    }
    add_entries_from_iterable(agent, &map_value, &iterable, &adder)
}

/// The Set constructor (spec 24.2.1.1): fill from the iterable via `add`.
fn set_construct(agent: &mut Agent, args: &[Value], new_target: &Value) -> Result<Value, JsError> {
    let proto = get_prototype_from_constructor(agent, new_target, SET_PROTO)?;
    let set = JsObject::ordinary_object_create(Some(proto));
    agent.set_data.insert(set.id(), RefCell::new(Vec::new()));
    let set_value = Value::Object(set);
    let iterable = args.first().cloned().unwrap_or(Value::Undefined);
    if matches!(iterable, Value::Undefined | Value::Null) {
        return Ok(set_value);
    }
    let adder = get(agent, &set_value, &JsString::from_utf8("add"))?;
    if !is_callable(&adder) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Set constructor: add is not a function".into(),
        ));
    }
    let iterator = crate::expr::get_iterator(agent, &iterable)?;
    loop {
        let next = match crate::expr::iterator_step(agent, &iterator)? {
            Some(value) => value,
            None => return Ok(set_value),
        };
        if let Err(error) = crate::function::call(agent, &adder, set_value.clone(), &[next]) {
            let _ = crate::expr::iterator_close(agent, &iterator);
            return Err(error);
        }
    }
}

/// The WeakMap constructor (spec 26.3.1.1).
fn weak_map_construct(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    let proto = get_prototype_from_constructor(agent, new_target, WEAK_MAP_PROTO)?;
    let map = JsObject::ordinary_object_create(Some(proto));
    agent
        .weak_map_data
        .insert(map.id(), RefCell::new(Vec::new()));
    let map_value = Value::Object(map);
    let iterable = args.first().cloned().unwrap_or(Value::Undefined);
    if matches!(iterable, Value::Undefined | Value::Null) {
        return Ok(map_value);
    }
    let adder = get(agent, &map_value, &JsString::from_utf8("set"))?;
    if !is_callable(&adder) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "WeakMap constructor: set is not a function".into(),
        ));
    }
    add_entries_from_iterable(agent, &map_value, &iterable, &adder)
}

/// The WeakSet constructor (spec 26.4.1.1).
fn weak_set_construct(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    let proto = get_prototype_from_constructor(agent, new_target, WEAK_SET_PROTO)?;
    let set = JsObject::ordinary_object_create(Some(proto));
    agent
        .weak_set_data
        .insert(set.id(), RefCell::new(Vec::new()));
    let set_value = Value::Object(set);
    let iterable = args.first().cloned().unwrap_or(Value::Undefined);
    if matches!(iterable, Value::Undefined | Value::Null) {
        return Ok(set_value);
    }
    let adder = get(agent, &set_value, &JsString::from_utf8("add"))?;
    if !is_callable(&adder) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "WeakSet constructor: add is not a function".into(),
        ));
    }
    let iterator = crate::expr::get_iterator(agent, &iterable)?;
    loop {
        let next = match crate::expr::iterator_step(agent, &iterator)? {
            Some(value) => value,
            None => return Ok(set_value),
        };
        if let Err(error) = crate::function::call(agent, &adder, set_value.clone(), &[next]) {
            let _ = crate::expr::iterator_close(agent, &iterator);
            return Err(error);
        }
    }
}

/// Install the Map/Set/WeakMap/WeakSet intrinsics and global bindings during
/// SetDefaultGlobalBindings (spec 24.1, 24.2, 26.3, 26.4).
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));

    // ---- Map ----
    let map_proto = JsObject::ordinary_object_create(object_proto.clone());
    let map_proto_value = Value::Object(map_proto.clone());
    let map_ctor = Function::create_builtin(
        Some(JsString::from_utf8("Map")),
        0,
        Box::new(placeholder("Map")),
        Some(Box::new(placeholder("Map"))),
        None,
    )?;
    let map_ctor_value = Value::Function(map_ctor.clone());
    realm.intrinsics.define(MAP, map_ctor_value.clone());
    realm.intrinsics.define(MAP_PROTO, map_proto_value.clone());
    map_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(map_proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    map_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(map_ctor_value.clone()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // Map statics: groupBy + @@species.
    let group_by = Function::create_builtin(
        Some(JsString::from_utf8("groupBy")),
        2,
        Box::new(placeholder("groupBy")),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(MAP_GROUP_BY, Value::Function(group_by.clone()));
    map_ctor.define_property(
        &JsString::from_utf8("groupBy"),
        &PropertyDescriptor {
            value: Some(Value::Function(group_by)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    let map_species = Function::create_builtin(
        Some(JsString::from_utf8("get [Symbol.species]")),
        0,
        Box::new(placeholder("species")),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(MAP_SPECIES, Value::Function(map_species.clone()));
    map_ctor.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("species").as_ref().clone()),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: Some(Value::Function(map_species)),
            set: Some(Value::Undefined),
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // Map prototype methods.
    let map_methods: [(&str, &str, u64); 13] = [
        ("clear", MAP_CLEAR, 0),
        ("delete", MAP_DELETE, 1),
        ("entries", MAP_ENTRIES, 0),
        ("forEach", MAP_FOR_EACH, 1),
        ("get", MAP_GET, 1),
        ("getOrInsert", MAP_GET_OR_INSERT, 2),
        ("getOrInsertComputed", MAP_GET_OR_INSERT_COMPUTED, 2),
        ("has", MAP_HAS, 1),
        ("keys", MAP_KEYS, 0),
        ("set", MAP_SET, 2),
        ("values", MAP_VALUES, 0),
        ("[Symbol.iterator]", MAP_ENTRIES, 0),
        ("[Symbol.toStringTag]", MAP_PROTO, 0),
    ];
    let mut entries_func = None;
    for (name, intrinsic, length) in map_methods {
        let is_tag = name == "[Symbol.toStringTag]";
        let is_iterator = name == "[Symbol.iterator]";
        if is_tag {
            map_proto.define_property_key(
                &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
                &PropertyDescriptor {
                    value: Some(str("Map")),
                    writable: Some(false),
                    get: None,
                    set: None,
                    enumerable: Some(false),
                    configurable: Some(true),
                },
            )?;
            continue;
        }
        // @@iterator is %Map.prototype.entries%: the same function value.
        if is_iterator && let Some(entries) = entries_func.clone() {
            map_proto.define_property_key(
                &PropertyKey::Symbol(crux::symbol::well_known("iterator").as_ref().clone()),
                &PropertyDescriptor {
                    value: Some(entries),
                    writable: Some(true),
                    get: None,
                    set: None,
                    enumerable: Some(false),
                    configurable: Some(true),
                },
            )?;
            continue;
        }
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name.trim_matches(['[', ']']))),
            length,
            Box::new(placeholder(intrinsic)),
            None,
            None,
        )?;
        realm
            .intrinsics
            .define(intrinsic, Value::Function(func.clone()));
        if name == "entries" {
            entries_func = Some(Value::Function(func.clone()));
        }
        map_proto.define_property(
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
    // The size accessor.
    let size_getter = Function::create_builtin(
        Some(JsString::from_utf8("get size")),
        0,
        Box::new(placeholder("size")),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(MAP_SIZE, Value::Function(size_getter.clone()));
    map_proto.define_property(
        &JsString::from_utf8("size"),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: Some(Value::Function(size_getter)),
            set: Some(Value::Undefined),
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // %MapIteratorPrototype%.
    let map_iterator_proto = JsObject::ordinary_object_create(object_proto.clone());
    let map_iterator_proto_value = Value::Object(map_iterator_proto.clone());
    realm
        .intrinsics
        .define(MAP_ITERATOR, map_iterator_proto_value);
    let map_next = Function::create_builtin(
        Some(JsString::from_utf8("next")),
        0,
        Box::new(placeholder("next")),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(MAP_ITERATOR_NEXT, Value::Function(map_next.clone()));
    map_iterator_proto.define_property(
        &JsString::from_utf8("next"),
        &PropertyDescriptor {
            value: Some(Value::Function(map_next)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    // %Iterator.prototype% (Phase 15) provides @@iterator = a function
    // returning `this`; until then, define it directly so `for..of` and
    // spread work over Map iterators.
    let map_self = Function::create_builtin(
        Some(JsString::from_utf8("[Symbol.iterator]")),
        0,
        Box::new(|this, _| Ok(this.clone())),
        None,
        None,
    )?;
    map_iterator_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("iterator").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::Function(map_self)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    map_iterator_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(str("Map Iterator")),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // ---- Set ----
    let set_proto = JsObject::ordinary_object_create(object_proto.clone());
    let set_proto_value = Value::Object(set_proto.clone());
    let set_ctor = Function::create_builtin(
        Some(JsString::from_utf8("Set")),
        0,
        Box::new(placeholder("Set")),
        Some(Box::new(placeholder("Set"))),
        None,
    )?;
    let set_ctor_value = Value::Function(set_ctor.clone());
    realm.intrinsics.define(SET, set_ctor_value.clone());
    realm.intrinsics.define(SET_PROTO, set_proto_value.clone());
    set_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(set_proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    set_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(set_ctor_value.clone()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    let set_species = Function::create_builtin(
        Some(JsString::from_utf8("get [Symbol.species]")),
        0,
        Box::new(placeholder("species")),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(SET_SPECIES, Value::Function(set_species.clone()));
    set_ctor.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("species").as_ref().clone()),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: Some(Value::Function(set_species)),
            set: Some(Value::Undefined),
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    let set_methods: [(&str, &str, u64); 15] = [
        ("add", SET_ADD, 1),
        ("clear", SET_CLEAR, 0),
        ("delete", SET_DELETE, 1),
        ("difference", SET_DIFFERENCE, 1),
        ("entries", SET_ENTRIES, 0),
        ("forEach", SET_FOR_EACH, 1),
        ("has", SET_HAS, 1),
        ("intersection", SET_INTERSECTION, 1),
        ("isDisjointFrom", SET_IS_DISJOINT_FROM, 1),
        ("isSubsetOf", SET_IS_SUBSET_OF, 1),
        ("isSupersetOf", SET_IS_SUPERSET_OF, 1),
        ("symmetricDifference", SET_SYMMETRIC_DIFFERENCE, 1),
        ("union", SET_UNION, 1),
        ("values", SET_VALUES, 0),
        ("[Symbol.iterator]", SET_VALUES, 0),
    ];
    let mut values_func_opt = None;
    for (name, intrinsic, length) in set_methods {
        let is_iterator = name == "[Symbol.iterator]";
        // @@iterator is %Set.prototype.values%: the same function value.
        if is_iterator && let Some(values) = values_func_opt.clone() {
            set_proto.define_property_key(
                &PropertyKey::Symbol(crux::symbol::well_known("iterator").as_ref().clone()),
                &PropertyDescriptor {
                    value: Some(values),
                    writable: Some(true),
                    get: None,
                    set: None,
                    enumerable: Some(false),
                    configurable: Some(true),
                },
            )?;
            continue;
        }
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name.trim_matches(['[', ']']))),
            length,
            Box::new(placeholder(intrinsic)),
            None,
            None,
        )?;
        realm
            .intrinsics
            .define(intrinsic, Value::Function(func.clone()));
        if name == "values" {
            values_func_opt = Some(Value::Function(func.clone()));
        }
        set_proto.define_property(
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
    // `keys` aliases `values`; @@toStringTag.
    let values_func = realm.intrinsics.get(SET_VALUES).ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "%Set.prototype.values% missing".into(),
        )
    })?;
    realm
        .intrinsics
        .define("%Set.prototype.keys%", values_func.clone());
    set_proto.define_property(
        &JsString::from_utf8("keys"),
        &PropertyDescriptor {
            value: Some(values_func),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    set_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(str("Set")),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    let set_size_getter = Function::create_builtin(
        Some(JsString::from_utf8("get size")),
        0,
        Box::new(placeholder("size")),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(SET_SIZE, Value::Function(set_size_getter.clone()));
    set_proto.define_property(
        &JsString::from_utf8("size"),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: Some(Value::Function(set_size_getter)),
            set: Some(Value::Undefined),
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // %SetIteratorPrototype%.
    let set_iterator_proto = JsObject::ordinary_object_create(object_proto.clone());
    let set_iterator_proto_value = Value::Object(set_iterator_proto.clone());
    realm
        .intrinsics
        .define(SET_ITERATOR, set_iterator_proto_value);
    let set_next = Function::create_builtin(
        Some(JsString::from_utf8("next")),
        0,
        Box::new(placeholder("next")),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(SET_ITERATOR_NEXT, Value::Function(set_next.clone()));
    set_iterator_proto.define_property(
        &JsString::from_utf8("next"),
        &PropertyDescriptor {
            value: Some(Value::Function(set_next)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    let set_self = Function::create_builtin(
        Some(JsString::from_utf8("[Symbol.iterator]")),
        0,
        Box::new(|this, _| Ok(this.clone())),
        None,
        None,
    )?;
    set_iterator_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("iterator").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::Function(set_self)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    set_iterator_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(str("Set Iterator")),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // ---- WeakMap ----
    let weak_map_proto = JsObject::ordinary_object_create(object_proto.clone());
    let weak_map_proto_value = Value::Object(weak_map_proto.clone());
    let weak_map_ctor = Function::create_builtin(
        Some(JsString::from_utf8("WeakMap")),
        0,
        Box::new(placeholder("WeakMap")),
        Some(Box::new(placeholder("WeakMap"))),
        None,
    )?;
    let weak_map_ctor_value = Value::Function(weak_map_ctor.clone());
    realm
        .intrinsics
        .define(WEAK_MAP, weak_map_ctor_value.clone());
    realm
        .intrinsics
        .define(WEAK_MAP_PROTO, weak_map_proto_value.clone());
    weak_map_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(weak_map_proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    weak_map_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(weak_map_ctor_value.clone()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    let weak_map_methods: [(&str, &str, u64); 6] = [
        ("delete", WEAK_MAP_DELETE, 1),
        ("get", WEAK_MAP_GET, 1),
        ("getOrInsert", WEAK_MAP_GET_OR_INSERT, 2),
        ("getOrInsertComputed", WEAK_MAP_GET_OR_INSERT_COMPUTED, 2),
        ("has", WEAK_MAP_HAS, 1),
        ("set", WEAK_MAP_SET, 2),
    ];
    for (name, intrinsic, length) in weak_map_methods {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            Box::new(placeholder(intrinsic)),
            None,
            None,
        )?;
        realm
            .intrinsics
            .define(intrinsic, Value::Function(func.clone()));
        weak_map_proto.define_property(
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
    weak_map_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(str("WeakMap")),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // ---- WeakSet ----
    let weak_set_proto = JsObject::ordinary_object_create(object_proto.clone());
    let weak_set_proto_value = Value::Object(weak_set_proto.clone());
    let weak_set_ctor = Function::create_builtin(
        Some(JsString::from_utf8("WeakSet")),
        0,
        Box::new(placeholder("WeakSet")),
        Some(Box::new(placeholder("WeakSet"))),
        None,
    )?;
    let weak_set_ctor_value = Value::Function(weak_set_ctor.clone());
    realm
        .intrinsics
        .define(WEAK_SET, weak_set_ctor_value.clone());
    realm
        .intrinsics
        .define(WEAK_SET_PROTO, weak_set_proto_value.clone());
    weak_set_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(weak_set_proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    weak_set_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(weak_set_ctor_value.clone()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    let weak_set_methods: [(&str, &str, u64); 3] = [
        ("add", WEAK_SET_ADD, 1),
        ("delete", WEAK_SET_DELETE, 1),
        ("has", WEAK_SET_HAS, 1),
    ];
    for (name, intrinsic, length) in weak_set_methods {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            Box::new(placeholder(intrinsic)),
            None,
            None,
        )?;
        realm
            .intrinsics
            .define(intrinsic, Value::Function(func.clone()));
        weak_set_proto.define_property(
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
    weak_set_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(str("WeakSet")),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // Global bindings.
    for (name, value) in [
        ("Map", map_ctor_value),
        ("Set", set_ctor_value),
        ("WeakMap", weak_map_ctor_value),
        ("WeakSet", weak_set_ctor_value),
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

/// Dispatch by intrinsic identity from `runtime::function::call`.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(MAP).as_ref() == Some(callee) {
        return Some(Err(JsError::new(
            ErrorKind::TypeError,
            "Map constructor cannot be called without 'new'".into(),
        )));
    }
    if intrinsics.get(SET).as_ref() == Some(callee) {
        return Some(Err(JsError::new(
            ErrorKind::TypeError,
            "Set constructor cannot be called without 'new'".into(),
        )));
    }
    if intrinsics.get(WEAK_MAP).as_ref() == Some(callee) {
        return Some(Err(JsError::new(
            ErrorKind::TypeError,
            "WeakMap constructor cannot be called without 'new'".into(),
        )));
    }
    if intrinsics.get(WEAK_SET).as_ref() == Some(callee) {
        return Some(Err(JsError::new(
            ErrorKind::TypeError,
            "WeakSet constructor cannot be called without 'new'".into(),
        )));
    }
    if intrinsics.get(MAP_GROUP_BY).as_ref() == Some(callee) {
        return Some(map_group_by(agent, this, args));
    }
    if intrinsics.get(MAP_CLEAR).as_ref() == Some(callee) {
        return Some(map_clear(agent, this, args));
    }
    if intrinsics.get(MAP_DELETE).as_ref() == Some(callee) {
        return Some(map_delete(agent, this, args));
    }
    if intrinsics.get(MAP_ENTRIES).as_ref() == Some(callee) {
        return Some(map_entries(agent, this, args));
    }
    if intrinsics.get(MAP_FOR_EACH).as_ref() == Some(callee) {
        return Some(map_for_each(agent, this, args));
    }
    if intrinsics.get(MAP_GET).as_ref() == Some(callee) {
        return Some(map_get(agent, this, args));
    }
    if intrinsics.get(MAP_GET_OR_INSERT).as_ref() == Some(callee) {
        return Some(map_get_or_insert(agent, this, args));
    }
    if intrinsics.get(MAP_GET_OR_INSERT_COMPUTED).as_ref() == Some(callee) {
        return Some(map_get_or_insert_computed(agent, this, args));
    }
    if intrinsics.get(MAP_HAS).as_ref() == Some(callee) {
        return Some(map_has(agent, this, args));
    }
    if intrinsics.get(MAP_KEYS).as_ref() == Some(callee) {
        return Some(map_keys(agent, this, args));
    }
    if intrinsics.get(MAP_SET).as_ref() == Some(callee) {
        return Some(map_set(agent, this, args));
    }
    if intrinsics.get(MAP_VALUES).as_ref() == Some(callee) {
        return Some(map_values(agent, this, args));
    }
    if intrinsics.get(MAP_SIZE).as_ref() == Some(callee) {
        return Some(map_size(agent, this, args));
    }
    if intrinsics.get(MAP_SPECIES).as_ref() == Some(callee) {
        return Some(species_getter(agent, this, args));
    }
    if intrinsics.get(MAP_ITERATOR_NEXT).as_ref() == Some(callee) {
        return Some(map_iterator_next(agent, this, args));
    }
    if intrinsics.get(SET_ADD).as_ref() == Some(callee) {
        return Some(set_add(agent, this, args));
    }
    if intrinsics.get(SET_CLEAR).as_ref() == Some(callee) {
        return Some(set_clear(agent, this, args));
    }
    if intrinsics.get(SET_DELETE).as_ref() == Some(callee) {
        return Some(set_delete(agent, this, args));
    }
    if intrinsics.get(SET_DIFFERENCE).as_ref() == Some(callee) {
        return Some(set_difference(agent, this, args));
    }
    if intrinsics.get(SET_ENTRIES).as_ref() == Some(callee) {
        return Some(set_entries(agent, this, args));
    }
    if intrinsics.get(SET_FOR_EACH).as_ref() == Some(callee) {
        return Some(set_for_each(agent, this, args));
    }
    if intrinsics.get(SET_HAS).as_ref() == Some(callee) {
        return Some(set_has(agent, this, args));
    }
    if intrinsics.get(SET_INTERSECTION).as_ref() == Some(callee) {
        return Some(set_intersection(agent, this, args));
    }
    if intrinsics.get(SET_IS_DISJOINT_FROM).as_ref() == Some(callee) {
        return Some(set_is_disjoint_from(agent, this, args));
    }
    if intrinsics.get(SET_IS_SUBSET_OF).as_ref() == Some(callee) {
        return Some(set_is_subset_of(agent, this, args));
    }
    if intrinsics.get(SET_IS_SUPERSET_OF).as_ref() == Some(callee) {
        return Some(set_is_superset_of(agent, this, args));
    }
    if intrinsics.get(SET_SYMMETRIC_DIFFERENCE).as_ref() == Some(callee) {
        return Some(set_symmetric_difference(agent, this, args));
    }
    if intrinsics.get(SET_UNION).as_ref() == Some(callee) {
        return Some(set_union(agent, this, args));
    }
    if intrinsics.get(SET_VALUES).as_ref() == Some(callee) {
        return Some(set_values(agent, this, args));
    }
    if intrinsics.get(SET_SIZE).as_ref() == Some(callee) {
        return Some(set_size(agent, this, args));
    }
    if intrinsics.get(SET_SPECIES).as_ref() == Some(callee) {
        return Some(species_getter(agent, this, args));
    }
    if intrinsics.get(SET_ITERATOR_NEXT).as_ref() == Some(callee) {
        return Some(set_iterator_next(agent, this, args));
    }
    if intrinsics.get(WEAK_MAP_DELETE).as_ref() == Some(callee) {
        return Some(weak_map_delete(agent, this, args));
    }
    if intrinsics.get(WEAK_MAP_GET).as_ref() == Some(callee) {
        return Some(weak_map_get(agent, this, args));
    }
    if intrinsics.get(WEAK_MAP_GET_OR_INSERT).as_ref() == Some(callee) {
        return Some(weak_map_get_or_insert(agent, this, args));
    }
    if intrinsics.get(WEAK_MAP_GET_OR_INSERT_COMPUTED).as_ref() == Some(callee) {
        return Some(weak_map_get_or_insert_computed(agent, this, args));
    }
    if intrinsics.get(WEAK_MAP_HAS).as_ref() == Some(callee) {
        return Some(weak_map_has(agent, this, args));
    }
    if intrinsics.get(WEAK_MAP_SET).as_ref() == Some(callee) {
        return Some(weak_map_set(agent, this, args));
    }
    if intrinsics.get(WEAK_SET_ADD).as_ref() == Some(callee) {
        return Some(weak_set_add(agent, this, args));
    }
    if intrinsics.get(WEAK_SET_DELETE).as_ref() == Some(callee) {
        return Some(weak_set_delete(agent, this, args));
    }
    if intrinsics.get(WEAK_SET_HAS).as_ref() == Some(callee) {
        return Some(weak_set_has(agent, this, args));
    }
    None
}

/// Dispatch by intrinsic identity from `runtime::function::construct`.
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(MAP).as_ref() == Some(callee) {
        return Some(map_construct(agent, args, new_target));
    }
    if intrinsics.get(SET).as_ref() == Some(callee) {
        return Some(set_construct(agent, args, new_target));
    }
    if intrinsics.get(WEAK_MAP).as_ref() == Some(callee) {
        return Some(weak_map_construct(agent, args, new_target));
    }
    if intrinsics.get(WEAK_SET).as_ref() == Some(callee) {
        return Some(weak_set_construct(agent, args, new_target));
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

    #[test]
    fn map_basics_and_key_semantics() {
        assert_eq!(number("new Map().size"), 0.0);
        assert_eq!(
            text(
                "(function(){ var m = new Map(); m.set('a', 1).set('b', 2); return m.get('a') + '|' + m.get('b') + '|' + m.size; })()"
            ),
            "1|2|2"
        );
        // set overwrites in place; delete/clear shrink the live count.
        assert_eq!(
            text(
                "(function(){ var m = new Map([['a', 1]]); m.set('a', 9); var d = m.delete('a'); m.set('b', 2); m.clear(); return d + '|' + m.size; })()"
            ),
            "true|0"
        );
        // NaN and ±0 key behavior (CanonicalizeKeyedCollectionKey).
        assert_eq!(
            text(
                "(function(){ var m = new Map(); m.set(NaN, 1); m.set(-0, 2); return m.get(NaN) + '|' + m.get(0) + '|' + m.has(-0); })()"
            ),
            "1|2|true"
        );
        // Object keys use identity.
        assert_eq!(
            text(
                "(function(){ var k = {}; var m = new Map(); m.set(k, 'v'); return m.get(k) + '|' + m.has({}); })()"
            ),
            "v|false"
        );
    }

    #[test]
    fn map_get_or_insert_and_group_by() {
        assert_eq!(
            text(
                "(function(){ var m = new Map(); var a = m.getOrInsert('k', 1); var b = m.getOrInsert('k', 2); return a + '|' + b + '|' + m.size; })()"
            ),
            "1|1|1"
        );
        assert_eq!(
            text(
                "(function(){ var m = new Map(); var a = m.getOrInsertComputed('k', function(k){ return k + '!'; }); var b = m.getOrInsertComputed('k', function(){ return 'no'; }); return a + '|' + b + '|' + m.get('k'); })()"
            ),
            "k!|k!|k!"
        );
        // groupBy groups by callback key, in first-seen order.
        assert_eq!(
            text(
                "(function(){ var g = Map.groupBy([1, 2, 3, 4], function(n){ return n % 2; }); var out = []; g.forEach(function(v, k){ out.push(k + ':' + v.join('+')); }); return out.join(','); })()"
            ),
            "1:1+3,0:2+4"
        );
    }

    #[test]
    fn map_iterators_and_mutation() {
        // entries/keys/values and for..of.
        assert_eq!(
            text(
                "(function(){ var m = new Map([['a', 1], ['b', 2]]); var out = []; for (var e of m) out.push(e.join(':')); return out.join(','); })()"
            ),
            "a:1,b:2"
        );
        assert_eq!(
            text(
                "(function(){ var m = new Map([['a', 1], ['b', 2]]); var k = m.keys(); var v = m.values(); return k.next().value + '|' + v.next().value; })()"
            ),
            "a|1"
        );
        // Deleted entries are skipped by suspended iterators.
        assert_eq!(
            text(
                "(function(){ var m = new Map([['a', 1], ['b', 2], ['c', 3]]); var it = m.entries(); it.next(); m.delete('b'); var e2 = it.next(); var e3 = it.next(); return (e2.done ? 'd' : e2.value.join(':')) + '|' + (e3.done ? 'd' : e3.value.join(':')); })()"
            ),
            "c:3|d"
        );
        // forEach visits entries added during iteration (spec refreshes the
        // count after each callback).
        assert_eq!(
            text(
                "(function(){ var m = new Map([['a', 1]]); var out = []; m.forEach(function(v, k){ out.push(k); if (k === 'a') m.set('b', 2); }); return out.join(','); })()"
            ),
            "a,b"
        );
        assert_eq!(
            text(
                "Object.prototype.toString.call(new Map()) + '|' + Object.prototype.toString.call(new Map().keys())"
            ),
            "[object Map]|[object Map Iterator]"
        );
    }

    #[test]
    fn set_methods_and_predicates() {
        assert_eq!(
            text(
                "(function(){ var s = new Set([1, 2, 2, 3]); return s.size + '|' + Array.from(s).join(','); })()"
            ),
            "3|1,2,3"
        );
        assert_eq!(
            text(
                "(function(){ var a = new Set([1, 2, 3]); var b = new Set([2, 3, 4]); return a.union(b).size + '|' + a.intersection(b).size + '|' + a.difference(b).size + '|' + a.symmetricDifference(b).size; })()"
            ),
            "4|2|1|2"
        );
        assert_eq!(
            text(
                "(function(){ var a = new Set([1, 2]); var b = new Set([1, 2, 3]); return a.isSubsetOf(b) + '|' + b.isSupersetOf(a) + '|' + a.isDisjointFrom(new Set([4])) + '|' + a.isDisjointFrom(new Set([1])); })()"
            ),
            "true|true|true|false"
        );
        // The set-methods accept array-likes via GetSetRecord.
        assert_eq!(
            number(
                "(function(){ var a = new Set([1, 2]); return a.union({size: 1, has: function(){ return false; }, keys: function(){ return [3][Symbol.iterator](); }}).size; })()"
            ),
            3.0
        );
    }

    #[test]
    fn weak_map_and_weak_set() {
        assert_eq!(
            text(
                "(function(){ var wm = new WeakMap(); var o = {}; wm.set(o, 42); var r = wm.get(o) + '|' + wm.has(o) + '|' + wm.delete(o) + '|' + wm.has(o); try { wm.set(5, 1); return r + '|no'; } catch (e) { return r + '|' + e.name; } })()"
            ),
            "42|true|true|false|TypeError"
        );
        assert_eq!(
            text(
                "(function(){ var wm = new WeakMap(); var o = {}; wm.set(o, 1); var a = wm.getOrInsert(o, 2); var b = wm.getOrInsertComputed(o, function(){ return 'x'; }); return a + '|' + b + '|' + wm.get(o); })()"
            ),
            "1|1|1"
        );
        assert_eq!(
            text(
                "(function(){ var ws = new WeakSet(); var o = {}; ws.add(o); var r = ws.has(o) + '|' + ws.delete(o) + '|' + ws.has(o); try { ws.add(1); return r + '|no'; } catch (e) { return r + '|' + e.name; } })()"
            ),
            "true|true|false|TypeError"
        );
        assert_eq!(
            text(
                "Object.prototype.toString.call(new WeakMap()) + '|' + Object.prototype.toString.call(new WeakSet())"
            ),
            "[object WeakMap]|[object WeakSet]"
        );
    }

    #[test]
    fn constructors_require_new_and_iterables() {
        assert!(run("Map()").is_err());
        assert!(run("Set()").is_err());
        assert!(run("WeakMap()").is_err());
        assert!(run("WeakSet()").is_err());
        assert_eq!(number("new Set([1, 2, 3]).size"), 3.0);
        assert_eq!(
            number(
                "(function(){ var wm = new WeakMap(); var a = {}; wm.set(a, 1); wm.set(a, 9); return wm.get(a); })()"
            ),
            9.0
        );
    }
}
