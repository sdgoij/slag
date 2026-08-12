//! The `%Iterator%` intrinsic (spec 27.1): the constructor, the prototype
//! helpers (eager `every`/`some`/`find`/`forEach`/`reduce`/`toArray`/
//! `includes`/`join`, lazy `map`/`filter`/`take`/`drop`/`flatMap`/`chunks`/
//! `windows`), `toAsync`, `Symbol.dispose`, and the statics `from`/`concat`/
//! `zip`/`zipKeyed`. Lazy helpers return an iterator-helper object
//! (`%IteratorHelper.prototype%`) whose `next`/`return` are dispatched by
//! intrinsic identity (the %eval% pattern).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crux::convert::{to_boolean, to_integer_or_infinity, to_number};
use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, is_callable};

use crate::agent::Agent;
use crate::context::{as_object, get_property, get_property_key, to_string as context_to_string};
use crate::expr::{IteratorRecord, get_iterator, get_method, iterator_close, iterator_step};
use crate::realm::Realm;

const ITERATOR: &str = "%Iterator%";
const ITERATOR_PROTO: &str = "%Iterator.prototype%";
const ITERATOR_HELPER_PROTO: &str = "%IteratorHelper.prototype%";
const WRAP_PROTO: &str = "%WrapForValidIterator.prototype%";

const DROP: &str = "drop";
const EVERY: &str = "every";
const FILTER: &str = "filter";
const FIND: &str = "find";
const FLAT_MAP: &str = "flatMap";
const FOR_EACH: &str = "forEach";
const MAP: &str = "map";
const REDUCE: &str = "reduce";
const SOME: &str = "some";
const TAKE: &str = "take";
const TO_ARRAY: &str = "toArray";
const TO_ASYNC: &str = "toAsync";
const CHUNKS: &str = "chunks";
const WINDOWS: &str = "windows";
const INCLUDES: &str = "includes";
const JOIN: &str = "join";
const FROM: &str = "from";
const CONCAT: &str = "concat";
const ZIP: &str = "zip";
const ZIP_KEYED: &str = "zipKeyed";
const NEXT: &str = "next";
const RETURN: &str = "return";
const THROW: &str = "throw";

/// The mode of an iterator-helper object (spec 27.1.3.1 table).
#[derive(Debug)]
pub enum HelperMode {
    Map {
        mapper: Value,
    },
    Filter {
        filterer: Value,
    },
    Take {
        remaining: f64,
    },
    Drop {
        remaining: f64,
    },
    FlatMap {
        mapper: Value,
        inner: Option<IteratorRecord>,
    },
    Chunks {
        chunk_size: f64,
        buffer: Vec<Value>,
    },
    Windows {
        window_size: f64,
        buffer: VecDeque<Value>,
    },
    Concat {
        iterators: Vec<IteratorRecord>,
        index: usize,
    },
    Zip {
        iterators: Vec<IteratorRecord>,
        /// The own keys of each yielded object (zipKeyed); empty for zip.
        keys: Vec<Value>,
        longest: bool,
        remainder: Value,
    },
}

/// The state of an iterator-helper object, keyed by object identity.
#[derive(Debug)]
pub struct HelperState {
    pub iterator: Option<IteratorRecord>,
    pub done: bool,
    pub mode: HelperMode,
}

/// The state of a `%WrapForValidIterator%` object (`Iterator.from` on a flat
/// iterable), keyed by object identity.
#[derive(Debug)]
pub struct WrappedIteratorState {
    pub record: IteratorRecord,
}

pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));

    // %Iterator.prototype%: an ordinary object with proto %Object.prototype%.
    let iterator_proto = JsObject::ordinary_object_create(object_proto.clone());
    let iterator_proto_value = Value::Object(iterator_proto.clone());
    realm
        .intrinsics
        .define(ITERATOR_PROTO, iterator_proto_value.clone());

    // %Iterator%: a function object with proto %Function.prototype%; both
    // call and construct throw.
    let iterator_ctor = Function::create_builtin(
        Some(JsString::from_utf8("Iterator")),
        0,
        Box::new(placeholder("Iterator".to_string())),
        Some(Box::new(placeholder("Iterator".to_string()))),
        None,
    )?;
    let iterator_ctor_value = Value::Function(iterator_ctor.clone());
    realm
        .intrinsics
        .define(ITERATOR, iterator_ctor_value.clone());
    if let Some(function_proto) = realm
        .intrinsics
        .get("%Function.prototype%")
        .and_then(|value| as_object(&value))
    {
        iterator_ctor
            .object
            .set_prototype_of(Some(function_proto))?;
    }
    iterator_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(iterator_proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;

    install_prototype_methods(realm, &iterator_proto)?;
    install_statics(realm, &iterator_ctor)?;
    install_helper_prototype(realm)?;
    install_wrap_prototype(realm)?;

    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("Iterator"),
        &PropertyDescriptor {
            value: Some(iterator_ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

fn placeholder(name: String) -> crux::function::NativeFn {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

fn define_method(
    realm: &Handle<Realm>,
    proto: &Handle<JsObject>,
    key: &str,
    name: &str,
    length: u64,
) -> Result<Handle<Function>, JsError> {
    let method = Function::create_builtin(
        Some(JsString::from_utf8(name)),
        length,
        Box::new(placeholder(name.to_string())),
        None,
        Some(proto.clone()),
    )?;
    realm
        .intrinsics
        .define(key, Value::Function(method.clone()));
    proto.define_property(
        &JsString::from_utf8(name),
        &PropertyDescriptor {
            value: Some(Value::Function(method.clone())),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(method)
}

fn install_prototype_methods(
    realm: &Handle<Realm>,
    proto: &Handle<JsObject>,
) -> Result<(), JsError> {
    // The lazy helpers: each returns an iterator-helper object.
    for (name, length) in [
        (DROP, 1),
        (FILTER, 1),
        (MAP, 1),
        (TAKE, 1),
        (FLAT_MAP, 1),
        (CHUNKS, 1),
        (WINDOWS, 1),
    ] {
        define_method(
            realm,
            proto,
            &format!("%Iterator.prototype.{name}%"),
            name,
            length,
        )?;
    }
    // The eager helpers: each consumes the iterator and returns a plain value.
    for (name, length) in [
        (EVERY, 1),
        (SOME, 1),
        (FIND, 1),
        (FOR_EACH, 1),
        (REDUCE, 1),
        (TO_ARRAY, 0),
        (INCLUDES, 1),
        (JOIN, 1),
    ] {
        define_method(
            realm,
            proto,
            &format!("%Iterator.prototype.{name}%"),
            name,
            length,
        )?;
    }
    define_method(
        realm,
        proto,
        &format!("%Iterator.prototype.{TO_ASYNC}%"),
        TO_ASYNC,
        0,
    )?;
    // %Iterator.prototype%[@@dispose] closes the iterator (spec 27.1.3.12).
    let dispose_method = Function::create_builtin(
        Some(JsString::from_utf8("[Symbol.dispose]")),
        0,
        Box::new(placeholder("[Symbol.dispose]".to_string())),
        None,
        None,
    )?;
    realm.intrinsics.define(
        "%Iterator.prototype.@@dispose%",
        Value::Function(dispose_method.clone()),
    );
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("dispose").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::Function(dispose_method)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // %Iterator.prototype%[@@iterator] returns `this` (spec 27.1.3.9).
    let iterator_method = Function::create_builtin(
        Some(JsString::from_utf8("[Symbol.iterator]")),
        0,
        Box::new(|this, _| Ok(this.clone())),
        None,
        None,
    )?;
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("iterator").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::Function(iterator_method)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // @@toStringTag (spec 27.1.3.10).
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor::none(Value::String(Handle::new(JsString::from_utf8("Iterator")))),
    )?;

    // %Iterator.prototype%.constructor is an accessor (spec 27.1.3.1); the
    // getter returns %Iterator%, the setter ignores prototype properties.
    let get = Function::create_builtin(
        Some(JsString::from_utf8("get constructor")),
        0,
        Box::new(placeholder("get constructor".to_string())),
        None,
        None,
    )?;
    let set = Function::create_builtin(
        Some(JsString::from_utf8("set constructor")),
        1,
        Box::new(placeholder("set constructor".to_string())),
        None,
        None,
    )?;
    realm.intrinsics.define(
        "%Iterator.prototype.constructor-get%",
        Value::Function(get.clone()),
    );
    realm.intrinsics.define(
        "%Iterator.prototype.constructor-set%",
        Value::Function(set.clone()),
    );
    proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: Some(Value::Function(get)),
            set: Some(Value::Function(set)),
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

fn install_statics(realm: &Handle<Realm>, ctor: &Handle<Function>) -> Result<(), JsError> {
    for (name, length) in [(FROM, 1), (CONCAT, 0), (ZIP, 1), (ZIP_KEYED, 1)] {
        let method = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            Box::new(placeholder(name.to_string())),
            None,
            None,
        )?;
        realm.intrinsics.define(
            &format!("%Iterator.{name}%"),
            Value::Function(method.clone()),
        );
        ctor.define_property(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: Some(Value::Function(method)),
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

/// `%IteratorHelper.prototype%`: `next`, `return`, `@@iterator`, and
/// @@toStringTag = "Iterator Helper" (spec 27.1.3.4).
fn install_helper_prototype(realm: &Handle<Realm>) -> Result<(), JsError> {
    let iterator_proto = realm
        .intrinsics
        .get(ITERATOR_PROTO)
        .and_then(|value| as_object(&value));
    let proto = JsObject::ordinary_object_create(iterator_proto);
    let proto_value = Value::Object(proto.clone());
    realm.intrinsics.define(ITERATOR_HELPER_PROTO, proto_value);
    for (name, length) in [(NEXT, 0), (RETURN, 0)] {
        let method = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            Box::new(placeholder(name.to_string())),
            None,
            None,
        )?;
        realm.intrinsics.define(
            &format!("%IteratorHelper.prototype.{name}%"),
            Value::Function(method.clone()),
        );
        proto.define_property(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: Some(Value::Function(method)),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
    }
    let iterator_method = Function::create_builtin(
        Some(JsString::from_utf8("[Symbol.iterator]")),
        0,
        Box::new(|this, _| Ok(this.clone())),
        None,
        None,
    )?;
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("iterator").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::Function(iterator_method)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor::none(Value::String(Handle::new(JsString::from_utf8(
            "Iterator Helper",
        )))),
    )?;
    Ok(())
}

/// `%WrapForValidIterator.prototype%`: `next`, `return`, `throw`, `@@iterator`
/// (spec 27.1.3.2).
fn install_wrap_prototype(realm: &Handle<Realm>) -> Result<(), JsError> {
    let iterator_proto = realm
        .intrinsics
        .get(ITERATOR_PROTO)
        .and_then(|value| as_object(&value));
    let proto = JsObject::ordinary_object_create(iterator_proto);
    let proto_value = Value::Object(proto.clone());
    realm.intrinsics.define(WRAP_PROTO, proto_value);
    for name in [NEXT, RETURN, THROW] {
        let method = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            1,
            Box::new(placeholder(name.to_string())),
            None,
            None,
        )?;
        realm.intrinsics.define(
            &format!("%WrapForValidIterator.prototype.{name}%"),
            Value::Function(method.clone()),
        );
        proto.define_property(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: Some(Value::Function(method)),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
    }
    let iterator_method = Function::create_builtin(
        Some(JsString::from_utf8("[Symbol.iterator]")),
        0,
        Box::new(|this, _| Ok(this.clone())),
        None,
        None,
    )?;
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("iterator").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::Function(iterator_method)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// A prototype-method handler, dispatched by intrinsic identity.
type MethodHandler = fn(&mut Agent, &Value, &[Value]) -> Result<Value, JsError>;

/// Route a call to an Iterator builtin by identity.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let Value::Function(_function) = callee else {
        return None;
    };
    // The iterator-helper `next`/`return` operate on `this`.
    for name in [NEXT, RETURN] {
        let key = format!("%IteratorHelper.prototype.{name}%");
        if agent.current_realm().ok()?.intrinsics.get(&key).as_ref() == Some(callee) {
            return Some(helper_method(agent, name == RETURN, this, args));
        }
    }
    // The wrap-for-valid-iterator methods operate on `this`.
    for name in [NEXT, RETURN, THROW] {
        let key = format!("%WrapForValidIterator.prototype.{name}%");
        if agent.current_realm().ok()?.intrinsics.get(&key).as_ref() == Some(callee) {
            return Some(wrap_method(agent, name, this, args));
        }
    }
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    let proto_value = intrinsics.get(ITERATOR_PROTO)?;
    let Value::Object(proto_obj) = &proto_value else {
        return None;
    };
    let methods: &[(&str, MethodHandler)] = &[
        (DROP, drop_method),
        (EVERY, every_method),
        (FILTER, filter_method),
        (FIND, find_method),
        (FLAT_MAP, flat_map_method),
        (FOR_EACH, for_each_method),
        (MAP, map_method),
        (REDUCE, reduce_method),
        (SOME, some_method),
        (TAKE, take_method),
        (TO_ARRAY, to_array_method),
        (TO_ASYNC, to_async_method),
        (CHUNKS, chunks_method),
        (WINDOWS, windows_method),
        (INCLUDES, includes_method),
        (JOIN, join_method),
    ];
    for (name, handler) in methods {
        let key = format!("%Iterator.prototype.{name}%");
        if intrinsics.get(&key).as_ref() == Some(callee) {
            return Some(handler(agent, this, args));
        }
    }
    if intrinsics.get("%Iterator.prototype.@@dispose%").as_ref() == Some(callee) {
        return Some(dispose_method(agent, this, args));
    }
    if intrinsics
        .get("%Iterator.prototype.constructor-get%")
        .as_ref()
        == Some(callee)
    {
        let iterator = intrinsics.get(ITERATOR).unwrap_or(Value::Undefined);
        return Some(Ok(iterator));
    }
    if intrinsics
        .get("%Iterator.prototype.constructor-set%")
        .as_ref()
        == Some(callee)
    {
        return Some(setter_that_ignores_prototype_properties(
            agent, this, args, proto_obj,
        ));
    }
    if intrinsics.get(ITERATOR).as_ref() == Some(callee) {
        // The Iterator constructor throws when called (spec 27.1.1).
        return Some(Err(JsError::new(
            ErrorKind::TypeError,
            "The Iterator constructor cannot be called".into(),
        )));
    }
    for name in [FROM, CONCAT, ZIP, ZIP_KEYED] {
        let key = format!("%Iterator.{name}%");
        if intrinsics.get(&key).as_ref() == Some(callee) {
            return Some(match name {
                FROM => iterator_from(agent, this, args),
                CONCAT => iterator_concat(agent, this, args),
                ZIP => iterator_zip(agent, this, args, false),
                ZIP_KEYED => iterator_zip(agent, this, args, true),
                _ => unreachable!(),
            });
        }
    }
    None
}

/// Dispatch a construct: the Iterator constructor throws (spec 27.1.1).
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    _args: &[Value],
    _new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    if realm.intrinsics.get(ITERATOR).as_ref() == Some(callee) {
        return Some(Err(JsError::new(
            ErrorKind::TypeError,
            "The Iterator constructor cannot be constructed".into(),
        )));
    }
    None
}

/// SetterThatIgnoresPrototypeProperties for the `constructor` accessor
/// (spec 10.2.2.2).
fn setter_that_ignores_prototype_properties(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    home: &Handle<JsObject>,
) -> Result<Value, JsError> {
    let Some(object) = as_object(this) else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "constructor setter requires an object this".into(),
        ));
    };
    if object.id() == home.id() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot assign to the prototype's constructor".into(),
        ));
    }
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    let key = PropertyKey::from_utf8("constructor");
    if !object.has_own_property_key(&key)? {
        object.create_data_property(&JsString::from_utf8("constructor"), value)?;
        return Ok(Value::Undefined);
    }
    let _ = get_property_key(agent, this, &key, this.clone())?;
    object.set(&JsString::from_utf8("constructor"), value, true)?;
    Ok(Value::Undefined)
}

/// CreateIterResultObject (spec 8.4.11).
fn iterator_result(agent: &Agent, value: Value, done: bool) -> Result<Value, JsError> {
    let object_proto = agent
        .current_realm()
        .ok()
        .and_then(|realm| realm.intrinsics.get("%Object.prototype%"))
        .and_then(|value| as_object(&value));
    let object = JsObject::ordinary_object_create(object_proto);
    object.create_data_property(&JsString::from_utf8("value"), value)?;
    object.create_data_property(&JsString::from_utf8("done"), Value::Boolean(done))?;
    Ok(Value::Object(object))
}

/// GetIteratorDirect (spec 7.4.1): `this` must be an object with a callable
/// `next` method.
fn get_iterator_direct(agent: &mut Agent, this: &Value) -> Result<IteratorRecord, JsError> {
    let (Value::Object(_) | Value::Function(_)) = this else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator method called on a non-object".into(),
        ));
    };
    let next = get_property(agent, this, &JsString::from_utf8("next"), this.clone())?;
    if !is_callable(&next) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator's next method is not callable".into(),
        ));
    }
    Ok(IteratorRecord {
        iterator: this.clone(),
        next,
    })
}

/// IteratorStepValue (spec 7.4.5): the next value, or `None` when done.
fn step_value(agent: &mut Agent, record: &IteratorRecord) -> Result<Option<Value>, JsError> {
    iterator_step(agent, record)
}

/// IterateUntilCompletion for the eager helpers: step the iterator, running
/// `body` per value; `body` returns `None` to keep iterating or `Some(value)`
/// to stop, closing the iterator (spec 27.1.3.1 iterated-until-completion).
/// Returns the stopped value, or `None` when the iterator was exhausted.
fn iterate_eager(
    agent: &mut Agent,
    record: &IteratorRecord,
    mut body: impl FnMut(&mut Agent, Value) -> Result<Option<Value>, JsError>,
) -> Result<Option<Value>, JsError> {
    loop {
        let Some(value) = step_value(agent, record)? else {
            return Ok(None);
        };
        if let Some(result) = body(agent, value)? {
            iterator_close(agent, record)?;
            return Ok(Some(result));
        }
    }
}

fn call_predicate(agent: &mut Agent, f: &Value, value: &Value) -> Result<bool, JsError> {
    let result = crate::function::call(agent, f, Value::Undefined, std::slice::from_ref(value))?;
    Ok(to_boolean(&result))
}

// ---- the eager helpers ----

fn every_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let predicate = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&predicate) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator.prototype.every requires a callable predicate".into(),
        ));
    }
    let record = get_iterator_direct(agent, this)?;
    let result = iterate_eager(agent, &record, |agent, value| {
        if call_predicate(agent, &predicate, &value)? {
            Ok(None)
        } else {
            Ok(Some(Value::Boolean(false)))
        }
    })?;
    Ok(Value::Boolean(result.is_none()))
}

fn some_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let predicate = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&predicate) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator.prototype.some requires a callable predicate".into(),
        ));
    }
    let record = get_iterator_direct(agent, this)?;
    let result = iterate_eager(agent, &record, |agent, value| {
        if call_predicate(agent, &predicate, &value)? {
            Ok(Some(Value::Boolean(true)))
        } else {
            Ok(None)
        }
    })?;
    Ok(Value::Boolean(result.is_some()))
}

fn find_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let predicate = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&predicate) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator.prototype.find requires a callable predicate".into(),
        ));
    }
    let record = get_iterator_direct(agent, this)?;
    let result = iterate_eager(agent, &record, |agent, value| {
        if call_predicate(agent, &predicate, &value)? {
            Ok(Some(value))
        } else {
            Ok(None)
        }
    })?;
    Ok(result.unwrap_or(Value::Undefined))
}

fn for_each_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let f = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&f) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator.prototype.forEach requires a callable function".into(),
        ));
    }
    let record = get_iterator_direct(agent, this)?;
    iterate_eager(agent, &record, |agent, value| {
        crate::function::call(agent, &f, Value::Undefined, &[value])?;
        Ok(None)
    })?;
    Ok(Value::Undefined)
}

fn reduce_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let reducer = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&reducer) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator.prototype.reduce requires a callable reducer".into(),
        ));
    }
    let record = get_iterator_direct(agent, this)?;
    let mut accumulator = args.get(1).cloned();
    if accumulator.is_none() {
        match step_value(agent, &record)? {
            Some(value) => accumulator = Some(value),
            None => {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "Reduce of empty iterator with no initial value".into(),
                ));
            }
        }
    }
    iterate_eager(agent, &record, |agent, value| {
        let acc = accumulator.clone().unwrap_or(Value::Undefined);
        accumulator = Some(crate::function::call(
            agent,
            &reducer,
            Value::Undefined,
            &[acc, value],
        )?);
        Ok(None)
    })?;
    Ok(accumulator.unwrap_or(Value::Undefined))
}

fn to_array_method(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let record = get_iterator_direct(agent, this)?;
    let mut values = Vec::new();
    iterate_eager(agent, &record, |_agent, value| {
        values.push(value);
        Ok(None)
    })?;
    crate::builtins::array::array_from_values(agent, &values)
}

fn includes_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let search = args.first().cloned().unwrap_or(Value::Undefined);
    let record = get_iterator_direct(agent, this)?;
    let result = iterate_eager(agent, &record, |_agent, value| {
        if crux::ops::same_value_zero(&value, &search) {
            Ok(Some(Value::Boolean(true)))
        } else {
            Ok(None)
        }
    })?;
    Ok(Value::Boolean(result.is_some()))
}

fn join_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let record = get_iterator_direct(agent, this)?;
    let separator = match args.first() {
        Some(Value::Undefined) | None => JsString::from_utf8(","),
        Some(value) => context_to_string(agent, value)?,
    };
    let mut parts = Vec::new();
    iterate_eager(agent, &record, |agent, value| {
        parts.push(context_to_string(agent, &value)?.to_string_lossy());
        Ok(None)
    })?;
    Ok(Value::String(Handle::new(JsString::from_utf8(
        &parts.join(&separator.to_string_lossy()),
    ))))
}

/// `Iterator.prototype[Symbol.dispose]`: close the iterator (spec 27.1.3.12).
fn dispose_method(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let record = get_iterator_direct(agent, this)?;
    iterator_close(agent, &record)?;
    Ok(Value::Undefined)
}

// ---- the lazy helpers ----

fn create_helper(agent: &mut Agent, state: HelperState) -> Result<Value, JsError> {
    let proto = agent
        .current_realm()?
        .intrinsics
        .get(ITERATOR_HELPER_PROTO)
        .and_then(|value| as_object(&value));
    let object = JsObject::ordinary_object_create(proto);
    agent
        .iterator_helpers
        .insert(object.id(), Rc::new(RefCell::new(state)));
    Ok(Value::Object(object))
}

fn map_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let mapper = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&mapper) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator.prototype.map requires a callable mapper".into(),
        ));
    }
    let record = get_iterator_direct(agent, this)?;
    create_helper(
        agent,
        HelperState {
            iterator: Some(record),
            done: false,
            mode: HelperMode::Map { mapper },
        },
    )
}

fn filter_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let filterer = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&filterer) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator.prototype.filter requires a callable filterer".into(),
        ));
    }
    let record = get_iterator_direct(agent, this)?;
    create_helper(
        agent,
        HelperState {
            iterator: Some(record),
            done: false,
            mode: HelperMode::Filter { filterer },
        },
    )
}

fn take_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let limit_arg = args.first().cloned().unwrap_or(Value::Undefined);
    let limit = to_integer_or_infinity(to_number(&limit_arg)?);
    if limit < 0.0 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "Iterator.prototype.take requires a non-negative limit".into(),
        ));
    }
    let record = get_iterator_direct(agent, this)?;
    create_helper(
        agent,
        HelperState {
            iterator: Some(record),
            done: false,
            mode: HelperMode::Take { remaining: limit },
        },
    )
}

fn drop_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let limit_arg = args.first().cloned().unwrap_or(Value::Undefined);
    let limit = to_integer_or_infinity(to_number(&limit_arg)?);
    if limit < 0.0 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "Iterator.prototype.drop requires a non-negative limit".into(),
        ));
    }
    let record = get_iterator_direct(agent, this)?;
    create_helper(
        agent,
        HelperState {
            iterator: Some(record),
            done: false,
            mode: HelperMode::Drop { remaining: limit },
        },
    )
}

fn flat_map_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let mapper = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&mapper) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator.prototype.flatMap requires a callable mapper".into(),
        ));
    }
    let record = get_iterator_direct(agent, this)?;
    create_helper(
        agent,
        HelperState {
            iterator: Some(record),
            done: false,
            mode: HelperMode::FlatMap {
                mapper,
                inner: None,
            },
        },
    )
}

/// Validate a chunk/window size per the chunking proposal: it must already
/// be an integral Number in [1, 2^32 - 1]. Unlike take/drop, no ToNumber
/// coercion happens, so user-defined valueOf/toString are never called.
fn chunk_window_size(arg: &Value) -> Result<f64, JsError> {
    let Value::Number(size) = arg else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "size must be a Number".into(),
        ));
    };
    if !size.is_finite() || size.fract() != 0.0 {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "size must be an integral Number".into(),
        ));
    }
    if *size < 1.0 || *size > 4294967295.0 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "size must be in the range 1 to 2^32 - 1".into(),
        ));
    }
    Ok(*size)
}

fn chunks_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let size_arg = args.first().cloned().unwrap_or(Value::Undefined);
    let size = chunk_window_size(&size_arg)?;
    let record = get_iterator_direct(agent, this)?;
    create_helper(
        agent,
        HelperState {
            iterator: Some(record),
            done: false,
            mode: HelperMode::Chunks {
                chunk_size: size,
                buffer: Vec::new(),
            },
        },
    )
}

fn windows_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let size_arg = args.first().cloned().unwrap_or(Value::Undefined);
    let size = chunk_window_size(&size_arg)?;
    // undersized defaults to "only-full" and must be one of the two strings.
    let undersized = args.get(1).cloned().unwrap_or(Value::Undefined);
    let valid_undersized = match &undersized {
        Value::Undefined => true,
        Value::String(text) => {
            let text = text.to_string_lossy();
            text == "only-full" || text == "allow-partial"
        }
        _ => false,
    };
    if !valid_undersized {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator.prototype.windows requires undersized to be \"only-full\" or \"allow-partial\""
                .into(),
        ));
    }
    let record = get_iterator_direct(agent, this)?;
    create_helper(
        agent,
        HelperState {
            iterator: Some(record),
            done: false,
            mode: HelperMode::Windows {
                window_size: size,
                buffer: VecDeque::new(),
            },
        },
    )
}

fn to_async_method(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    // toAsync wraps the sync iterator obtained from `this`; for plain
    // iterables this means going through @@iterator (matching Array's
    // [Symbol.asyncIterator]).
    let record = crate::expr::get_iterator(agent, this)?;
    let object = crate::async_await::async_from_sync_iterator(agent, &record)?;
    Ok(Value::Object(object))
}

/// The `%IteratorHelper.prototype%` `next`/`return` dispatch (spec 27.1.3.4).
fn helper_method(
    agent: &mut Agent,
    is_return: bool,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let Value::Object(obj) = this else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator helper method called on a non-object".into(),
        ));
    };
    let state = agent
        .iterator_helpers
        .remove(&obj.id())
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not an iterator helper".into()))?;
    let result = if is_return {
        helper_return(agent, &state, args)
    } else {
        helper_next(agent, &state, args)
    };
    agent.iterator_helpers.insert(obj.id(), state);
    result
}

fn helper_return(
    agent: &mut Agent,
    state: &Rc<RefCell<HelperState>>,
    args: &[Value],
) -> Result<Value, JsError> {
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    let mut state = state.borrow_mut();
    if state.done {
        return iterator_result(agent, Value::Undefined, true);
    }
    state.done = true;
    let mut records: Vec<IteratorRecord> = Vec::new();
    if let Some(record) = state.iterator.take() {
        records.push(record);
    }
    if let HelperMode::FlatMap { inner, .. } = &mut state.mode
        && let Some(inner) = inner.take()
    {
        records.push(inner);
    }
    if let HelperMode::Concat { iterators, .. } = &mut state.mode {
        records.append(iterators);
    }
    if let HelperMode::Zip { iterators, .. } = &mut state.mode {
        records.append(iterators);
    }
    for record in &records {
        iterator_close(agent, record)?;
    }
    iterator_result(agent, value, true)
}

fn helper_next(
    agent: &mut Agent,
    state: &Rc<RefCell<HelperState>>,
    _args: &[Value],
) -> Result<Value, JsError> {
    let mut state = state.borrow_mut();
    if state.done {
        return iterator_result(agent, Value::Undefined, true);
    }
    let result = step_helper(agent, &mut state);
    if state.done {
        // Natural exhaustion: the underlying iterators were already consumed.
        state.iterator = None;
        state.done = true;
    }
    result
}

/// Drive one `next()` of the helper: pull from the underlying iterator(s)
/// according to the mode and produce the next iteration result.
fn step_helper(agent: &mut Agent, state: &mut HelperState) -> Result<Value, JsError> {
    let done_result = |agent: &Agent| iterator_result(agent, Value::Undefined, true);
    match &mut state.mode {
        HelperMode::Map { mapper } => {
            let record = state.iterator.as_ref().ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "no underlying iterator".into())
            })?;
            let Some(value) = step_value(agent, record)? else {
                state.done = true;
                return done_result(agent);
            };
            let mapped = crate::function::call(agent, mapper, Value::Undefined, &[value])?;
            iterator_result(agent, mapped, false)
        }
        HelperMode::Filter { filterer } => {
            let record = state.iterator.as_ref().ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "no underlying iterator".into())
            })?;
            loop {
                let Some(value) = step_value(agent, record)? else {
                    state.done = true;
                    return done_result(agent);
                };
                if call_predicate(agent, filterer, &value)? {
                    return iterator_result(agent, value, false);
                }
            }
        }
        HelperMode::Take { remaining } => {
            let record = state.iterator.as_ref().ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "no underlying iterator".into())
            })?;
            if *remaining <= 0.0 {
                // spec: IteratorClose on the underlying when the limit hits 0.
                state.done = true;
                if let Some(record) = state.iterator.take() {
                    iterator_close(agent, &record)?;
                }
                return done_result(agent);
            }
            let Some(value) = step_value(agent, record)? else {
                state.done = true;
                return done_result(agent);
            };
            *remaining -= 1.0;
            iterator_result(agent, value, false)
        }
        HelperMode::Drop { remaining } => {
            let record = state.iterator.as_ref().ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "no underlying iterator".into())
            })?;
            while *remaining > 0.0 {
                let Some(_) = step_value(agent, record)? else {
                    state.done = true;
                    return done_result(agent);
                };
                *remaining -= 1.0;
            }
            let Some(value) = step_value(agent, record)? else {
                state.done = true;
                return done_result(agent);
            };
            iterator_result(agent, value, false)
        }
        HelperMode::FlatMap { mapper, inner } => {
            loop {
                if let Some(inner_record) = inner {
                    let Some(value) = step_value(agent, inner_record)? else {
                        *inner = None;
                        continue;
                    };
                    return iterator_result(agent, value, false);
                }
                let record = state.iterator.as_ref().ok_or_else(|| {
                    JsError::new(ErrorKind::TypeError, "no underlying iterator".into())
                })?;
                let Some(value) = step_value(agent, record)? else {
                    state.done = true;
                    return done_result(agent);
                };
                let mapped = crate::function::call(agent, mapper, Value::Undefined, &[value])?;
                if !matches!(mapped, Value::Object(_) | Value::Function(_)) {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "flatMap mapper must return an iterable object".into(),
                    ));
                }
                // Strings are rejected by flatMap (GetIteratorFlattenable with
                // hint ~reject-strings~).
                let mapped_iterable = get_method(agent, &mapped, "@@iterator")?;
                let Some(method) = mapped_iterable else {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "flatMap mapper must return an iterable object".into(),
                    ));
                };
                let inner_value = crate::function::call(agent, &method, mapped.clone(), &[])?;
                if !matches!(inner_value, Value::Object(_)) {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "flatMap inner iterator must be an object".into(),
                    ));
                }
                let next = get_property(
                    agent,
                    &inner_value,
                    &JsString::from_utf8("next"),
                    inner_value.clone(),
                )?;
                if !is_callable(&next) {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "flatMap inner iterator has no callable next".into(),
                    ));
                }
                *inner = Some(IteratorRecord {
                    iterator: inner_value,
                    next,
                });
            }
        }
        HelperMode::Chunks { chunk_size, buffer } => {
            let record = state.iterator.as_ref().ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "no underlying iterator".into())
            })?;
            buffer.clear();
            while (buffer.len() as f64) < *chunk_size {
                let Some(value) = step_value(agent, record)? else {
                    state.done = true;
                    if buffer.is_empty() {
                        let array = crate::builtins::array::array_from_values(agent, &[])?;
                        return iterator_result(agent, array, true);
                    }
                    let array = crate::builtins::array::array_from_values(agent, buffer)?;
                    return iterator_result(agent, array, false);
                };
                buffer.push(value);
            }
            let array = crate::builtins::array::array_from_values(agent, buffer)?;
            iterator_result(agent, array, false)
        }
        HelperMode::Windows {
            window_size,
            buffer,
        } => {
            let record = state.iterator.as_ref().ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "no underlying iterator".into())
            })?;
            // Fill the window on the first next().
            while (buffer.len() as f64) < *window_size {
                let Some(value) = step_value(agent, record)? else {
                    state.done = true;
                    return iterator_result(agent, Value::Undefined, true);
                };
                buffer.push_back(value);
            }
            let window: Vec<Value> = buffer.iter().cloned().collect();
            buffer.pop_front();
            // The window slides by one: the next call pulls one more value.
            iterator_result(
                agent,
                crate::builtins::array::array_from_values(agent, &window)?,
                false,
            )
        }
        HelperMode::Concat { iterators, index } => {
            loop {
                if *index >= iterators.len() {
                    state.done = true;
                    // spec: closing semantics — close every iterable on completion.
                    let records = std::mem::take(iterators);
                    for record in &records {
                        iterator_close(agent, record)?;
                    }
                    return done_result(agent);
                }
                let record = &iterators[*index];
                let Some(value) = step_value(agent, record)? else {
                    *index += 1;
                    continue;
                };
                return iterator_result(agent, value, false);
            }
        }
        HelperMode::Zip {
            iterators,
            keys,
            longest,
            remainder,
            ..
        } => {
            if iterators.is_empty() {
                state.done = true;
                return done_result(agent);
            }
            let mut values = Vec::with_capacity(iterators.len());
            let mut any_done = false;
            let mut all_done = true;
            for record in iterators.iter_mut() {
                match step_value(agent, record)? {
                    Some(value) => {
                        values.push(value);
                        all_done = false;
                    }
                    None => {
                        any_done = true;
                        values.push(remainder.clone());
                    }
                }
            }
            if (!*longest && any_done) || (all_done && any_done) {
                // shortest mode ends at the first exhausted column; longest
                // mode ends once every column is exhausted.
                state.done = true;
                let records = std::mem::take(iterators);
                for record in &records {
                    iterator_close(agent, record)?;
                }
                return done_result(agent);
            }
            if keys.is_empty() {
                iterator_result(
                    agent,
                    crate::builtins::array::array_from_values(agent, &values)?,
                    false,
                )
            } else {
                // zipKeyed: an object with the collected keys.
                let object_proto = agent
                    .current_realm()
                    .ok()
                    .and_then(|realm| realm.intrinsics.get("%Object.prototype%"))
                    .and_then(|value| as_object(&value));
                let object = JsObject::ordinary_object_create(object_proto);
                for (key, value) in keys.iter().zip(values.iter()) {
                    let key = crate::context::to_property_key(agent, key)?;
                    match key {
                        PropertyKey::String(name) => {
                            object.create_data_property(&crux::lookup(name), value.clone())?;
                        }
                        PropertyKey::Symbol(symbol) => {
                            object.define_property_key(
                                &PropertyKey::Symbol(symbol),
                                &PropertyDescriptor {
                                    value: Some(value.clone()),
                                    writable: Some(true),
                                    get: None,
                                    set: None,
                                    enumerable: Some(true),
                                    configurable: Some(true),
                                },
                            )?;
                        }
                    }
                }
                iterator_result(agent, Value::Object(object), false)
            }
        }
    }
}

// ---- the statics ----

/// GetIteratorFlattenable (spec 7.4.3): a value is either an iterable (has
/// @@iterator) or a flat iterator (wrapped). Returns the record and whether a
/// wrapper object was created.
fn get_iterator_flattenable(
    agent: &mut Agent,
    value: &Value,
) -> Result<(IteratorRecord, bool), JsError> {
    if matches!(value, Value::Undefined | Value::Null) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator.from requires an iterable value".into(),
        ));
    }
    let method = get_method(agent, value, "@@iterator")?;
    let Some(method) = method else {
        // A flat iterable: wrap the value itself.
        let next = get_property(agent, value, &JsString::from_utf8("next"), value.clone())?;
        if !is_callable(&next) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Flat iterable has no callable next method".into(),
            ));
        }
        return Ok((
            IteratorRecord {
                iterator: value.clone(),
                next,
            },
            true,
        ));
    };
    let iterator = crate::function::call(agent, &method, value.clone(), &[])?;
    if !matches!(iterator, Value::Object(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator must be an object".into(),
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
    Ok((IteratorRecord { iterator, next }, false))
}

fn iterator_from(agent: &mut Agent, _this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    let (record, wrapped) = get_iterator_flattenable(agent, &value)?;
    if !wrapped {
        return Ok(record.iterator);
    }
    let proto = agent
        .current_realm()?
        .intrinsics
        .get(WRAP_PROTO)
        .and_then(|value| as_object(&value));
    let object = JsObject::ordinary_object_create(proto);
    agent.wrapped_iterators.insert(
        object.id(),
        Rc::new(RefCell::new(WrappedIteratorState { record })),
    );
    Ok(Value::Object(object))
}

/// The `%WrapForValidIterator.prototype%` method bodies (spec 27.1.3.2.1-3).
fn wrap_method(
    agent: &mut Agent,
    name: &str,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let Value::Object(obj) = this else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "WrapForValidIterator method called on a non-object".into(),
        ));
    };
    let state = agent
        .wrapped_iterators
        .get(&obj.id())
        .cloned()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "not a wrapped iterator".into()))?;
    let record = state.borrow().record.clone();
    match name {
        NEXT => {
            let result = crate::function::call(agent, &record.next, record.iterator.clone(), args)?;
            if !matches!(result, Value::Object(_)) {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "Iterator result is not an object".into(),
                ));
            }
            Ok(result)
        }
        RETURN => {
            let return_method = get_property(
                agent,
                &record.iterator,
                &JsString::from_utf8("return"),
                record.iterator.clone(),
            )?;
            if is_callable(&return_method) {
                crate::function::call(agent, &return_method, record.iterator.clone(), args)
            } else {
                let value = args.first().cloned().unwrap_or(Value::Undefined);
                let object = JsObject::ordinary_object_create(None);
                object.create_data_property(&JsString::from_utf8("value"), value)?;
                object.create_data_property(&JsString::from_utf8("done"), Value::Boolean(true))?;
                Ok(Value::Object(object))
            }
        }
        THROW => {
            let throw_method = get_property(
                agent,
                &record.iterator,
                &JsString::from_utf8("throw"),
                record.iterator.clone(),
            )?;
            if is_callable(&throw_method) {
                crate::function::call(agent, &throw_method, record.iterator.clone(), args)
            } else {
                let reason = args.first().cloned().unwrap_or(Value::Undefined);
                Err(
                    JsError::new(ErrorKind::TypeError, "iterator has no throw method".into())
                        .with_value(reason),
                )
            }
        }
        _ => unreachable!(),
    }
}

fn iterator_concat(agent: &mut Agent, _this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let mut iterators = Vec::new();
    for value in args {
        let (record, wrapped) = get_iterator_flattenable(agent, value)?;
        let record = if wrapped {
            // The flat wrap's record has the value's own next; keep it.
            record
        } else {
            record
        };
        iterators.push(record);
    }
    create_helper(
        agent,
        HelperState {
            iterator: None,
            done: false,
            mode: HelperMode::Concat {
                iterators,
                index: 0,
            },
        },
    )
}

fn iterator_zip(
    agent: &mut Agent,
    _this: &Value,
    args: &[Value],
    keyed: bool,
) -> Result<Value, JsError> {
    let iterables = args.first().cloned().unwrap_or(Value::Undefined);
    let options = args.get(1).cloned();
    let (mut longest, mut remainder) = (false, Value::Undefined);
    if let Some(options) = options
        && let Value::Object(_) | Value::Function(_) = options
    {
        let length = get_property(
            agent,
            &options,
            &JsString::from_utf8("length"),
            options.clone(),
        )?;
        if let Value::String(text) = length {
            longest = text.to_string_lossy() == "longest";
        }
        let rem = get_property(
            agent,
            &options,
            &JsString::from_utf8("remainder"),
            options.clone(),
        )?;
        remainder = rem;
    }
    let mut iterators = Vec::new();
    let mut keys = Vec::new();
    let record = get_iterator(agent, &iterables)?;
    while let Some(element) = iterator_step(agent, &record)? {
        if keyed {
            // zipKeyed: each element is a pair [key, iterable].
            let pair_record = get_iterator(agent, &element)?;
            let key = match iterator_step(agent, &pair_record)? {
                Some(key) => key,
                None => Value::Undefined,
            };
            let value = match iterator_step(agent, &pair_record)? {
                Some(value) => value,
                None => Value::Undefined,
            };
            let (inner, _) = get_iterator_flattenable(agent, &value)?;
            iterators.push(inner);
            keys.push(key);
        } else {
            let (inner, _) = get_iterator_flattenable(agent, &element)?;
            iterators.push(inner);
        }
    }
    create_helper(
        agent,
        HelperState {
            iterator: None,
            done: false,
            mode: HelperMode::Zip {
                iterators,
                keys,
                longest,
                remainder,
            },
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::evaluate;

    fn run(source: &str) -> Result<Value, JsError> {
        evaluate(source)
    }

    #[test]
    fn lazy_helpers_chain_lazily() {
        assert_eq!(
            run("JSON.stringify([1, 2, 3, 4].values().map(x => x * 2).toArray())").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("[2,4,6,8]")))
        );
        assert_eq!(
            run("JSON.stringify([1, 2, 3, 4, 5].values().filter(x => x % 2).toArray())").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("[1,3,5]")))
        );
        assert_eq!(
            run("JSON.stringify([1, 2, 3].values().flatMap(x => [x, x * 10]).toArray())").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("[1,10,2,20,3,30]")))
        );
    }

    #[test]
    fn take_and_drop_respect_limits() {
        assert_eq!(
            run("JSON.stringify([1, 2, 3, 4, 5].values().take(2).toArray())").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("[1,2]")))
        );
        assert_eq!(
            run("JSON.stringify([1, 2, 3, 4, 5].values().drop(2).toArray())").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("[3,4,5]")))
        );
    }

    #[test]
    fn take_closes_the_underlying_iterator() {
        assert_eq!(
            run(concat!(
                "let closed = false;",
                "const it = [1, 2, 3][Symbol.iterator]();",
                "it.return = () => { closed = true; return { done: true }; };",
                "const t = it.take(1);",
                "t.next(); t.next();",
                "closed;"
            ))
            .unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn eager_helpers_return_plain_values() {
        assert_eq!(
            run("[1, 2, 3].values().reduce((a, b) => a + b, 0)").unwrap(),
            Value::Number(6.0)
        );
        assert_eq!(
            run("[1, 2, 3].values().some(x => x > 2)").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("[1, 2, 3].values().every(x => x > 0)").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("[5, 6, 7].values().join('-')").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("5-6-7")))
        );
        assert_eq!(
            run("[1, 2, 3].values().find(x => x === 2)").unwrap(),
            Value::Number(2.0)
        );
    }

    #[test]
    fn statics_from_concat_zip_and_zip_keyed() {
        assert_eq!(
            run("JSON.stringify(Iterator.from([10, 20]).toArray())").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("[10,20]")))
        );
        assert_eq!(
            run("JSON.stringify(Iterator.concat([1, 2], [3, 4]).toArray())").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("[1,2,3,4]")))
        );
        assert_eq!(
            run("JSON.stringify(Iterator.zip([[1, 2], ['a', 'b']]).toArray())").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("[[1,\"a\"],[2,\"b\"]]")))
        );
        assert_eq!(
            run(concat!(
                "JSON.stringify(Iterator.zip([['a', 'b'], [1, 2, 3]],",
                "{ length: 'longest', remainder: 'R' }).toArray())"
            ))
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8(
                "[[\"a\",1],[\"b\",2],[\"R\",3]]"
            )))
        );
        assert_eq!(
            run(
                "JSON.stringify(Iterator.zipKeyed([['k1', [1, 2]], ['k2', ['a', 'b']]]).toArray())"
            )
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8(
                "[{\"k1\":1,\"k2\":\"a\"},{\"k1\":2,\"k2\":\"b\"}]"
            )))
        );
    }

    #[test]
    fn iterator_prototype_shapes() {
        assert_eq!(
            run("Object.getPrototypeOf(Iterator) === Function.prototype").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("[][Symbol.iterator]().take(0) instanceof Iterator").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("Iterator.prototype[Symbol.toStringTag]").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("Iterator")))
        );
        assert_eq!(
            run("Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]())) === Iterator.prototype").unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn symbol_dispose_closes_the_iterator() {
        assert_eq!(
            run(concat!(
                "let closed = false;",
                "const it = [1, 2][Symbol.iterator]();",
                "it.return = () => { closed = true; return { done: true }; };",
                "it[Symbol.dispose]();",
                "closed;"
            ))
            .unwrap(),
            Value::Boolean(true)
        );
    }
}
