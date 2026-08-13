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

use crux::convert::{to_boolean, to_integer_or_infinity};
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
        /// `undersized` is "allow-partial": a partial final window is yielded.
        allow_partial: bool,
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
    /// The per-value counter passed to callbacks (spec 27.1.3.5 step 5.d).
    pub counter: f64,
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
        // A builtin method's [[Prototype]] is %Function.prototype%; the realm
        // post-pass links null-prototyped intrinsic functions.
        None,
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
    realm.intrinsics.define(
        "%Iterator.prototype.@@iterator%",
        Value::Function(iterator_method.clone()),
    );
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

    // @@toStringTag (spec 27.1.3.10): an accessor returning "Iterator"; the
    // setter ignores prototype properties (SetterThatIgnoresPrototypeProperties).
    let tag_get = Function::create_builtin(
        Some(JsString::from_utf8("get [Symbol.toStringTag]")),
        0,
        Box::new(|_, _| Ok(Value::String(Handle::new(JsString::from_utf8("Iterator"))))),
        None,
        None,
    )?;
    let tag_set = Function::create_builtin(
        Some(JsString::from_utf8("set [Symbol.toStringTag]")),
        1,
        Box::new(placeholder("set [Symbol.toStringTag]".to_string())),
        None,
        None,
    )?;
    realm.intrinsics.define(
        "%Iterator.prototype.@@toStringTag-get%",
        Value::Function(tag_get.clone()),
    );
    realm.intrinsics.define(
        "%Iterator.prototype.@@toStringTag-set%",
        Value::Function(tag_set.clone()),
    );
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: Some(Value::Function(tag_get)),
            set: Some(Value::Function(tag_set)),
            enumerable: Some(false),
            configurable: Some(true),
        },
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
    realm.intrinsics.define(
        "%IteratorHelper.prototype.@@iterator%",
        Value::Function(iterator_method.clone()),
    );
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
    realm.intrinsics.define(
        "%WrapForValidIterator.prototype.@@iterator%",
        Value::Function(iterator_method.clone()),
    );
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
            agent,
            this,
            args,
            proto_obj,
            PropertyKey::from_utf8("constructor"),
        ));
    }
    if intrinsics
        .get("%Iterator.prototype.@@toStringTag-set%")
        .as_ref()
        == Some(callee)
    {
        return Some(setter_that_ignores_prototype_properties(
            agent,
            this,
            args,
            proto_obj,
            PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
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

/// Dispatch a construct: `new Iterator()` throws, while a subclass
/// `super()` call creates an object with the newTarget prototype (spec
/// 27.1.1.1 steps 1-2).
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    _args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    if realm.intrinsics.get(ITERATOR).as_ref() == Some(callee) {
        return Some((|| {
            // spec step 2: NewTarget being the active function object (the
            // %Iterator% constructor itself) throws; only subclass
            // newTargets construct.
            if realm.intrinsics.get(ITERATOR).as_ref() == Some(new_target) {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "The Iterator constructor cannot be called without 'new' on a subclass".into(),
                ));
            }
            // GetPrototypeFromConstructor (spec 10.2.4): newTarget's
            // `prototype`, falling back to %Iterator.prototype%.
            let prototype = match crate::context::get_property(
                agent,
                new_target,
                &JsString::from_utf8("prototype"),
                new_target.clone(),
            ) {
                Ok(value) => as_object(&value),
                Err(e) => return Err(e),
            };
            let prototype = prototype.or_else(|| {
                realm
                    .intrinsics
                    .get(ITERATOR_PROTO)
                    .and_then(|value| as_object(&value))
            });
            Ok(Value::Object(JsObject::ordinary_object_create(prototype)))
        })());
    }
    None
}

/// SetterThatIgnoresPrototypeProperties for the `constructor`/`@@toStringTag`
/// accessors (spec 10.2.2.2).
fn setter_that_ignores_prototype_properties(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    home: &Handle<JsObject>,
    key: PropertyKey,
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
            "Cannot assign to the prototype's property".into(),
        ));
    }
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    if !object.has_own_property_key(&key)? {
        object.create_data_property_or_throw_key(&key, value)?;
        return Ok(Value::Undefined);
    }
    let _ = get_property_key(agent, this, &key, this.clone())?;
    object.set_key(&key, value, true)?;
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
    // The next method is stored as-is; a non-callable one surfaces as a
    // TypeError from the first step (spec: the helpers defer the check).
    let next = get_property(agent, this, &JsString::from_utf8("next"), this.clone())?;
    Ok(IteratorRecord {
        iterator: this.clone(),
        next,
    })
}

/// IteratorStepValue (spec 7.4.5): the next value, or `None` when done.
fn step_value(agent: &mut Agent, record: &IteratorRecord) -> Result<Option<Value>, JsError> {
    iterator_step(agent, record)
}

/// IteratorClose the receiver's `return` method after an argument-validation
/// failure (spec: the helpers validate their argument before GetIteratorDirect
/// and close the receiver on failure). The close error is suppressed — the
/// validation error wins.
fn close_this_on_error(agent: &mut Agent, this: &Value, error: JsError) -> Result<Value, JsError> {
    let return_key = JsString::from_utf8("return");
    if let Ok(return_method) = get_property(agent, this, &return_key, this.clone())
        && is_callable(&return_method)
    {
        let _ = crate::function::call(agent, &return_method, this.clone(), &[]);
    }
    Err(error)
}

/// Close the underlying iterator(s) of a helper, suppressing close errors
/// (the original completion wins, spec IteratorClose step 8).
fn close_helper_iterators(agent: &mut Agent, state: &mut HelperState) {
    if let Some(record) = state.iterator.take() {
        let _ = iterator_close(agent, &record);
    }
    if let HelperMode::FlatMap { inner, .. } = &mut state.mode
        && let Some(inner) = inner.take()
    {
        let _ = iterator_close(agent, &inner);
    }
}

/// IterateUntilCompletion for the eager helpers: step the iterator, running
/// `body` per value with the value's counter; `body` returns `None` to keep
/// iterating or `Some(value)` to stop, closing the iterator (spec 27.1.3.1
/// iterated-until-completion). A body error closes the iterator too, and the
/// error propagates. Returns the stopped value, or `None` when exhausted.
fn iterate_eager(
    agent: &mut Agent,
    record: &IteratorRecord,
    start_counter: f64,
    mut body: impl FnMut(&mut Agent, Value, f64) -> Result<Option<Value>, JsError>,
) -> Result<Option<Value>, JsError> {
    let mut counter = start_counter;
    loop {
        let Some(value) = step_value(agent, record)? else {
            return Ok(None);
        };
        match body(agent, value, counter) {
            Ok(Some(result)) => {
                iterator_close(agent, record)?;
                return Ok(Some(result));
            }
            Ok(None) => {}
            Err(e) => {
                let _ = iterator_close(agent, record);
                return Err(e);
            }
        }
        counter += 1.0;
    }
}

fn call_predicate(
    agent: &mut Agent,
    f: &Value,
    value: &Value,
    counter: f64,
) -> Result<bool, JsError> {
    let result = crate::function::call(
        agent,
        f,
        Value::Undefined,
        &[value.clone(), Value::Number(counter)],
    )?;
    Ok(to_boolean(&result))
}

// ---- the eager helpers ----

fn every_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let predicate = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&predicate) {
        let error = JsError::new(
            ErrorKind::TypeError,
            "Iterator.prototype.every requires a callable predicate".into(),
        );
        return close_this_on_error(agent, this, error);
    }
    let record = get_iterator_direct(agent, this)?;
    let result = iterate_eager(agent, &record, 0.0, |agent, value, counter| {
        if call_predicate(agent, &predicate, &value, counter)? {
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
        let error = JsError::new(
            ErrorKind::TypeError,
            "Iterator.prototype.some requires a callable predicate".into(),
        );
        return close_this_on_error(agent, this, error);
    }
    let record = get_iterator_direct(agent, this)?;
    let result = iterate_eager(agent, &record, 0.0, |agent, value, counter| {
        if call_predicate(agent, &predicate, &value, counter)? {
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
        let error = JsError::new(
            ErrorKind::TypeError,
            "Iterator.prototype.find requires a callable predicate".into(),
        );
        return close_this_on_error(agent, this, error);
    }
    let record = get_iterator_direct(agent, this)?;
    let result = iterate_eager(agent, &record, 0.0, |agent, value, counter| {
        if call_predicate(agent, &predicate, &value, counter)? {
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
        let error = JsError::new(
            ErrorKind::TypeError,
            "Iterator.prototype.forEach requires a callable function".into(),
        );
        return close_this_on_error(agent, this, error);
    }
    let record = get_iterator_direct(agent, this)?;
    iterate_eager(agent, &record, 0.0, |agent, value, counter| {
        crate::function::call(
            agent,
            &f,
            Value::Undefined,
            &[value, Value::Number(counter)],
        )?;
        Ok(None)
    })?;
    Ok(Value::Undefined)
}

fn reduce_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let reducer = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&reducer) {
        let error = JsError::new(
            ErrorKind::TypeError,
            "Iterator.prototype.reduce requires a callable reducer".into(),
        );
        return close_this_on_error(agent, this, error);
    }
    let record = get_iterator_direct(agent, this)?;
    let mut accumulator = args.get(1).cloned();
    // With no initial value the first element seeds the accumulator without a
    // reducer call, so the first reducer call sees counter 1 (spec 27.1.3.?).
    let start_counter = if accumulator.is_none() {
        match step_value(agent, &record)? {
            Some(value) => {
                accumulator = Some(value);
                1.0
            }
            None => {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "Reduce of empty iterator with no initial value".into(),
                ));
            }
        }
    } else {
        0.0
    };
    iterate_eager(agent, &record, start_counter, |agent, value, counter| {
        let acc = accumulator.clone().unwrap_or(Value::Undefined);
        accumulator = Some(crate::function::call(
            agent,
            &reducer,
            Value::Undefined,
            &[acc, value, Value::Number(counter)],
        )?);
        Ok(None)
    })?;
    Ok(accumulator.unwrap_or(Value::Undefined))
}

fn to_array_method(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let record = get_iterator_direct(agent, this)?;
    let mut values = Vec::new();
    iterate_eager(agent, &record, 0.0, |_agent, value, _counter| {
        values.push(value);
        Ok(None)
    })?;
    crate::builtins::array::array_from_values(agent, &values)
}

fn includes_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    // spec 27.1.3.?: an Object this is required (no ToObject coercion).
    if !matches!(this, Value::Object(_) | Value::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator.prototype.includes requires an object this".into(),
        ));
    }
    let search = args.first().cloned().unwrap_or(Value::Undefined);
    // skippedElements: undefined -> 0; otherwise it must be an integral
    // Number (Infinity allowed) with no coercion, else TypeError + close.
    let mut to_skip = match args.get(1).cloned() {
        None | Some(Value::Undefined) => 0.0,
        Some(Value::Number(n)) => {
            let integral = n.is_infinite() || n.fract() == 0.0;
            if !integral {
                let error = JsError::new(
                    ErrorKind::TypeError,
                    "Iterator.prototype.includes requires skippedElements to be an integral Number"
                        .into(),
                );
                return close_this_on_error(agent, this, error);
            }
            if n < 0.0 {
                let error = JsError::new(
                    ErrorKind::RangeError,
                    "Iterator.prototype.includes requires skippedElements to be non-negative"
                        .into(),
                );
                return close_this_on_error(agent, this, error);
            }
            if n.is_finite() && n > 9007199254740991.0 {
                let error = JsError::new(
                    ErrorKind::RangeError,
                    "Iterator.prototype.includes requires skippedElements within 2^53 - 1".into(),
                );
                return close_this_on_error(agent, this, error);
            }
            n
        }
        Some(_) => {
            let error = JsError::new(
                ErrorKind::TypeError,
                "Iterator.prototype.includes requires skippedElements to be an integral Number"
                    .into(),
            );
            return close_this_on_error(agent, this, error);
        }
    };
    let record = get_iterator_direct(agent, this)?;
    let result = iterate_eager(agent, &record, 0.0, |_agent, value, _counter| {
        if to_skip > 0.0 {
            to_skip -= 1.0;
            return Ok(None);
        }
        if crux::ops::same_value_zero(&value, &search) {
            Ok(Some(Value::Boolean(true)))
        } else {
            Ok(None)
        }
    })?;
    Ok(Value::Boolean(result.is_some()))
}

fn join_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    // spec 27.1.3.?: the separator is coerced first, and a coercion error
    // closes the receiver.
    let separator = match args.first() {
        Some(Value::Undefined) | None => JsString::from_utf8(","),
        Some(value) => match context_to_string(agent, value) {
            Ok(text) => text,
            Err(e) => return close_this_on_error(agent, this, e),
        },
    };
    let record = get_iterator_direct(agent, this)?;
    let mut parts = Vec::new();
    iterate_eager(agent, &record, 0.0, |agent, value, _counter| {
        // Nullish contents join as the empty string (like Array.prototype.join).
        let text = match value {
            Value::Null | Value::Undefined => String::new(),
            other => context_to_string(agent, &other)?.to_string_lossy(),
        };
        parts.push(text);
        Ok(None)
    })?;
    Ok(Value::String(Handle::new(JsString::from_utf8(
        &parts.join(&separator.to_string_lossy()),
    ))))
}

/// `Iterator.prototype[Symbol.dispose]`: call the receiver's `return` method
/// (spec 27.1.3.12) — the receiver need not have a `next`.
fn dispose_method(agent: &mut Agent, this: &Value, _args: &[Value]) -> Result<Value, JsError> {
    let return_key = JsString::from_utf8("return");
    if let Ok(return_method) = get_property(agent, this, &return_key, this.clone())
        && is_callable(&return_method)
    {
        crate::function::call(agent, &return_method, this.clone(), &[])?;
    }
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
        let error = JsError::new(
            ErrorKind::TypeError,
            "Iterator.prototype.map requires a callable mapper".into(),
        );
        return close_this_on_error(agent, this, error);
    }
    let record = get_iterator_direct(agent, this)?;
    create_helper(
        agent,
        HelperState {
            iterator: Some(record),
            done: false,
            counter: 0.0,
            mode: HelperMode::Map { mapper },
        },
    )
}

fn filter_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let filterer = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&filterer) {
        let error = JsError::new(
            ErrorKind::TypeError,
            "Iterator.prototype.filter requires a callable filterer".into(),
        );
        return close_this_on_error(agent, this, error);
    }
    let record = get_iterator_direct(agent, this)?;
    create_helper(
        agent,
        HelperState {
            iterator: Some(record),
            done: false,
            counter: 0.0,
            mode: HelperMode::Filter { filterer },
        },
    )
}

fn take_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let limit = take_drop_limit(
        agent,
        this,
        args.first().cloned().unwrap_or(Value::Undefined),
    )?;
    let record = get_iterator_direct(agent, this)?;
    create_helper(
        agent,
        HelperState {
            iterator: Some(record),
            done: false,
            counter: 0.0,
            mode: HelperMode::Take { remaining: limit },
        },
    )
}

fn drop_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let limit = take_drop_limit(
        agent,
        this,
        args.first().cloned().unwrap_or(Value::Undefined),
    )?;
    let record = get_iterator_direct(agent, this)?;
    create_helper(
        agent,
        HelperState {
            iterator: Some(record),
            done: false,
            counter: 0.0,
            mode: HelperMode::Drop { remaining: limit },
        },
    )
}

fn flat_map_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let mapper = args.first().cloned().unwrap_or(Value::Undefined);
    if !is_callable(&mapper) {
        let error = JsError::new(
            ErrorKind::TypeError,
            "Iterator.prototype.flatMap requires a callable mapper".into(),
        );
        return close_this_on_error(agent, this, error);
    }
    let record = get_iterator_direct(agent, this)?;
    create_helper(
        agent,
        HelperState {
            iterator: Some(record),
            done: false,
            counter: 0.0,
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

/// The take/drop limit (spec 27.1.3.3 steps 2-6 / 27.1.3.6): ToNumber through
/// the agent, then a RangeError (with the receiver closed) when the value is
/// NaN, finite and above 2^53 - 1, or truncates below zero.
fn take_drop_limit(agent: &mut Agent, this: &Value, arg: Value) -> Result<f64, JsError> {
    // RequireObjectCoercible (spec 27.1.3.3 step 1): a nullish this fails
    // before the argument is coerced.
    if !matches!(this, Value::Object(_) | Value::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator.prototype.take/drop requires an object this".into(),
        ));
    }
    let num = match crate::context::to_number(agent, &arg) {
        Ok(number) => number,
        Err(e) => return Err(close_this_on_error(agent, this, e).unwrap_err()),
    };
    let limit = to_integer_or_infinity(num);
    let invalid = num.is_nan() || (num.is_finite() && num > 9007199254740991.0) || limit < 0.0;
    if invalid {
        let error = JsError::new(
            ErrorKind::RangeError,
            "Iterator.prototype.take/drop requires a valid limit".into(),
        );
        return Err(close_this_on_error(agent, this, error).unwrap_err());
    }
    Ok(limit)
}

fn chunks_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    // RequireObjectCoercible first (spec 27.1.3.?: step 2).
    if !matches!(this, Value::Object(_) | Value::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator.prototype.chunks requires an object this".into(),
        ));
    }
    let size_arg = args.first().cloned().unwrap_or(Value::Undefined);
    let size = match chunk_window_size(&size_arg) {
        Ok(size) => size,
        Err(e) => return close_this_on_error(agent, this, e),
    };
    let record = get_iterator_direct(agent, this)?;
    create_helper(
        agent,
        HelperState {
            iterator: Some(record),
            done: false,
            counter: 0.0,
            mode: HelperMode::Chunks {
                chunk_size: size,
                buffer: Vec::new(),
            },
        },
    )
}

fn windows_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    // RequireObjectCoercible first.
    if !matches!(this, Value::Object(_) | Value::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Iterator.prototype.windows requires an object this".into(),
        ));
    }
    let size_arg = args.first().cloned().unwrap_or(Value::Undefined);
    let size = match chunk_window_size(&size_arg) {
        Ok(size) => size,
        Err(e) => return close_this_on_error(agent, this, e),
    };
    // undersized defaults to "only-full" and must be one of the two strings.
    let undersized = args.get(1).cloned().unwrap_or(Value::Undefined);
    let mut allow_partial = false;
    let valid_undersized = match &undersized {
        Value::Undefined => true,
        Value::String(text) => {
            let text = text.to_string_lossy();
            allow_partial = text == "allow-partial";
            text == "only-full" || text == "allow-partial"
        }
        _ => false,
    };
    if !valid_undersized {
        let error = JsError::new(
            ErrorKind::TypeError,
            "Iterator.prototype.windows requires undersized to be \"only-full\" or \"allow-partial\""
                .into(),
        );
        return close_this_on_error(agent, this, error);
    }
    let record = get_iterator_direct(agent, this)?;
    create_helper(
        agent,
        HelperState {
            iterator: Some(record),
            done: false,
            counter: 0.0,
            mode: HelperMode::Windows {
                window_size: size,
                buffer: VecDeque::new(),
                allow_partial,
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
            let counter = state.counter;
            state.counter += 1.0;
            let mapped = match crate::function::call(
                agent,
                mapper,
                Value::Undefined,
                &[value, Value::Number(counter)],
            ) {
                Ok(mapped) => mapped,
                Err(e) => {
                    state.done = true;
                    close_helper_iterators(agent, state);
                    return Err(e);
                }
            };
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
                let counter = state.counter;
                state.counter += 1.0;
                let keep = match call_predicate(agent, filterer, &value, counter) {
                    Ok(keep) => keep,
                    Err(e) => {
                        state.done = true;
                        close_helper_iterators(agent, state);
                        return Err(e);
                    }
                };
                if keep {
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
                let counter = state.counter;
                state.counter += 1.0;
                let mapped = match crate::function::call(
                    agent,
                    mapper,
                    Value::Undefined,
                    &[value, Value::Number(counter)],
                ) {
                    Ok(mapped) => mapped,
                    Err(e) => {
                        state.done = true;
                        close_helper_iterators(agent, state);
                        return Err(e);
                    }
                };
                if !matches!(mapped, Value::Object(_) | Value::Function(_)) {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "flatMap mapper must return an iterable object".into(),
                    ));
                }
                // GetIteratorFlattenable (spec 27.1.3.2): an @@iterator when
                // present, otherwise the object's own `next` (a flat iterator).
                let (inner_record, _) = get_iterator_flattenable(agent, &mapped)?;
                *inner = Some(inner_record);
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
                        // Exhausted with an empty buffer: done, no chunk
                        // (spec 27.1.3.? step 8.a.ii.2).
                        return iterator_result(agent, Value::Undefined, true);
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
            allow_partial,
        } => {
            let record = state.iterator.as_ref().ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "no underlying iterator".into())
            })?;
            // Fill the window on the first next().
            while (buffer.len() as f64) < *window_size {
                let Some(value) = step_value(agent, record)? else {
                    // Exhausted: a partial window is yielded only when
                    // undersized is "allow-partial" (spec 27.1.3.? step 8.a.ii).
                    if *allow_partial && !buffer.is_empty() {
                        let partial: Vec<Value> = buffer.iter().cloned().collect();
                        buffer.clear();
                        state.done = true;
                        return iterator_result(
                            agent,
                            crate::builtins::array::array_from_values(agent, &partial)?,
                            false,
                        );
                    }
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
            counter: 0.0,
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
            counter: 0.0,
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
