//! The tag types of the V8 API, and the inheritance between them.
//!
//! In the crate this stands in for, `Local<'s, T>` is a `NonNull<T>` and `T` is
//! a real heap object; method availability comes from C++ inheritance, which
//! the crate mirrors by making each tag `Deref` to its base class. Here the tag
//! is zero-sized and the payload lives in [`Local`](crate::Local), so the same
//! inheritance is spelled as a `Deref` from one `Local` instantiation to the
//! next. Method resolution is identical either way — that is what the shapes
//! have to buy, because a consumer's signatures and trait impls have to keep
//! resolving.
//!
//! The hierarchy below is transcribed from the crate we stand in for, not
//! invented: every edge matches an `impl_deref!` there.

use std::any::type_name;
use std::convert::TryFrom;
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::hash::{Hash, Hasher};
use std::num::NonZeroI32;
use std::ops::Deref;

use crux::object::ObjectKind;
use crux::typed_array::ElementType;
use crux::value::ValueKind;
use runtime::Agent;
use runtime::api;
use runtime::function::EcmaFunction;

use crate::handle::{Global, Local, Payload};

/// Every tag in the API. Zero-sized by design: a tag selects methods and
/// comparison/cast impls, and holds nothing.
macro_rules! tags {
    ($($(#[$attr:meta])* $name:ident),* $(,)?) => {
        $(
            $(#[$attr])*
            #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
            pub struct $name(());
        )*
    };
}

tags! {
    // v8::Data and its direct subclasses.
    Data,
    Value,
    Context,
    Module,
    ModuleRequest,
    Private,
    FixedArray,
    PrimitiveArray,
    AccessorSignature,
    Signature,
    UnboundScript,
    UnboundModuleScript,

    // v8::Value and below.
    Primitive,
    Name,
    String,
    Symbol,
    Number,
    Integer,
    Int32,
    Uint32,
    BigInt,
    Boolean,
    External,

    Object,
    Array,
    Function,
    Promise,
    PromiseResolver,
    Proxy,
    RegExp,
    Date,
    Map,
    Set,
    StringObject,
    NumberObject,
    BooleanObject,
    SymbolObject,
    BigIntObject,
    WasmMemoryObject,
    WasmModuleObject,

    ArrayBuffer,
    SharedArrayBuffer,
    ArrayBufferView,
    DataView,
    TypedArray,
    Uint8Array,
    Uint8ClampedArray,
    Int8Array,
    Uint16Array,
    Int16Array,
    Uint32Array,
    Int32Array,
    Float16Array,
    Float32Array,
    Float64Array,
    BigInt64Array,
    BigUint64Array,

    // Templates.
    Template,
    FunctionTemplate,
    ObjectTemplate,

    // Not part of the `Data` hierarchy in the crate we stand in for.
    Script,
    Message,
    StackTrace,
}

macro_rules! derefs_to {
    ($($sub:ident => $base:ident),* $(,)?) => {
        $(
            impl<'s> Deref for Local<'s, $sub> {
                type Target = Local<'s, $base>;

                fn deref(&self) -> &Self::Target {
                    self.cast_ref()
                }
            }
        )*
    };
}

derefs_to! {
    // v8::Data subclasses.
    Value => Data,
    Context => Data,
    Module => Data,
    ModuleRequest => Data,
    Private => Data,
    FixedArray => Data,
    PrimitiveArray => Data,
    AccessorSignature => Data,
    Signature => Data,
    UnboundScript => Data,
    UnboundModuleScript => Data,
    Template => Data,

    // v8::Value subclasses.
    Primitive => Value,
    External => Value,

    // v8::Primitive subclasses.
    Name => Primitive,
    Number => Primitive,
    BigInt => Primitive,
    Boolean => Primitive,

    // v8::Name subclasses.
    String => Name,
    Symbol => Name,

    // v8::Number subclasses.
    Integer => Number,
    Int32 => Integer,
    Uint32 => Integer,

    // v8::Object subclasses.
    Object => Value,
    Array => Object,
    Function => Object,
    Promise => Object,
    PromiseResolver => Object,
    Proxy => Object,
    RegExp => Object,
    Date => Object,
    Map => Object,
    Set => Object,
    StringObject => Object,
    NumberObject => Object,
    BooleanObject => Object,
    SymbolObject => Object,
    BigIntObject => Object,
    WasmMemoryObject => Object,
    WasmModuleObject => Object,
    ArrayBuffer => Object,
    SharedArrayBuffer => Object,
    ArrayBufferView => Object,

    // Array buffers and views.
    DataView => ArrayBufferView,
    TypedArray => ArrayBufferView,
    Uint8Array => TypedArray,
    Uint8ClampedArray => TypedArray,
    Int8Array => TypedArray,
    Uint16Array => TypedArray,
    Int16Array => TypedArray,
    Uint32Array => TypedArray,
    Int32Array => TypedArray,
    Float16Array => TypedArray,
    Float32Array => TypedArray,
    Float64Array => TypedArray,
    BigInt64Array => TypedArray,
    BigUint64Array => TypedArray,

    // Templates.
    FunctionTemplate => Template,
    ObjectTemplate => Template,
}

/// Whether a payload is of the type a tag names.
///
/// One predicate per tag is enough here, unlike the crate we stand in for,
/// where a cast's check depends on its source as well: every `Local<'s, T>` in
/// this bridge carries the same payload, so the target alone decides.
///
/// Tags with no honest predicate are simply absent, so a cast to one is a
/// compile error rather than a cast that always fails. The engine does not
/// distinguish them yet — `ArrayBuffer`, `Map`, `Set`, `Date`, `RegExp`,
/// `Promise`, the wrapper objects and the template types are all ordinary
/// objects to it, and telling them apart needs a property lookup that no
/// payload-only predicate can do.
pub(crate) trait TagCheck {
    fn check(payload: &Payload) -> bool;
}

impl TagCheck for Data {
    fn check(payload: &Payload) -> bool {
        matches!(
            payload,
            Payload::Value(_) | Payload::Context(_) | Payload::Module(_)
        )
    }
}

impl TagCheck for Value {
    fn check(payload: &Payload) -> bool {
        matches!(payload, Payload::Value(_))
    }
}

impl TagCheck for Context {
    fn check(payload: &Payload) -> bool {
        matches!(payload, Payload::Context(_))
    }
}

/// A module record, which has a payload of its own rather than an encoded
/// language value — see `Payload::Module`.
impl TagCheck for Module {
    fn check(payload: &Payload) -> bool {
        matches!(payload, Payload::Module(_))
    }
}

macro_rules! tag_checks {
    ($($tag:ident => $check:path),* $(,)?) => {
        $(
            impl TagCheck for $tag {
                fn check(payload: &Payload) -> bool {
                    payload.as_value_opt().is_some_and($check)
                }
            }
        )*
    };
}

tag_checks! {
    Primitive => is_primitive,
    Name => is_name,
    String => is_string,
    Symbol => is_symbol,
    Number => is_number,
    Integer => is_integer,
    Int32 => is_int32,
    Uint32 => is_uint32,
    BigInt => is_big_int,
    Boolean => is_boolean,
    External => is_external,
    Object => is_object,
    Array => is_array,
    Function => is_function,
    Proxy => is_proxy,
    StringObject => is_string_object,
    TypedArray => is_typed_array,
    Int8Array => is_int8_array,
    Uint8Array => is_uint8_array,
    Uint8ClampedArray => is_uint8_clamped_array,
    Int16Array => is_int16_array,
    Uint16Array => is_uint16_array,
    Int32Array => is_int32_array,
    Uint32Array => is_uint32_array,
    Float16Array => is_float16_array,
    Float32Array => is_float32_array,
    Float64Array => is_float64_array,
    BigInt64Array => is_big_int64_array,
    BigUint64Array => is_big_uint64_array,
}

// The brands the engine keeps no internal tag for. It does keep a table of
// them, one entry per instance keyed by object identity, which answers the
// stricter question the crate we stand in for asks: an object merely handed a
// foreign prototype is not a `Map`, and a walk of its chain would say it is.
tag_checks! {
    ArrayBuffer => is_array_buffer,
    SharedArrayBuffer => is_shared_array_buffer,
    DataView => is_data_view,
    ArrayBufferView => is_array_buffer_view,
    Map => is_map,
    Set => is_set,
    Date => is_date,
    RegExp => is_reg_exp,
    Promise => is_promise,
    BigIntObject => is_big_int_object,
    BooleanObject => is_boolean_object,
    NumberObject => is_number_object,
    SymbolObject => is_symbol_object,
}

// A template has no engine object of its own: a handle is an `External` naming
// the address the isolate took the template under, so the question is whether
// the entered realm's isolate has that address. The only kind this bridge mints
// is a function one, which is why `Template` — the base both kinds share — asks
// the same question; an object template would have to widen both.
tag_checks! {
    Template => is_function_template,
    FunctionTemplate => is_function_template,
    Private => is_private,
}

// The predicates below are what both callers share: the cast tables above, and
// the public `Value` surface in [`crate::value`]. They are named after the
// methods of the crate we stand in for and take its handle, since that is what
// both have in hand.

/// Not an object.
pub(crate) fn is_primitive(value: &api::Local) -> bool {
    !is_object(value)
}

/// A string or a symbol.
pub(crate) fn is_name(value: &api::Local) -> bool {
    is_string(value) || is_symbol(value)
}

pub(crate) fn is_string(value: &api::Local) -> bool {
    value.is_string()
}

pub(crate) fn is_symbol(value: &api::Local) -> bool {
    value.is_symbol()
}

pub(crate) fn is_number(value: &api::Local) -> bool {
    value.is_number()
}

/// An integer in `i32` or `u32` range.
pub(crate) fn is_integer(value: &api::Local) -> bool {
    is_int32(value) || is_uint32(value)
}

pub(crate) fn is_big_int(value: &api::Local) -> bool {
    value.is_bigint()
}

pub(crate) fn is_boolean(value: &api::Local) -> bool {
    value.is_boolean()
}

/// A receiver (`v8::Value::IsObject`), so a function counts: in the crate we
/// stand in for this is `IsJSReceiver`, and a function is one. The engine's own
/// `Value::is_object` is narrower — it asks about the tag — which is why this
/// does not delegate to it.
pub(crate) fn is_object(value: &api::Local) -> bool {
    matches!(
        value.value().kind(),
        ValueKind::Object(_) | ValueKind::Function(_)
    )
}

pub(crate) fn is_function(value: &api::Local) -> bool {
    value.is_function()
}

pub(crate) fn is_external(value: &api::Local) -> bool {
    object_matches(value, |kind| matches!(kind, ObjectKind::External(_)))
}

/// A function template this isolate minted.
///
/// The handle carries an `External` naming the template's address in the
/// isolate, so the address has to be on the isolate's list of the templates it
/// took ownership of. Without a realm entered there is no isolate to ask, which
/// is the answer the other table predicates give too; an `External` the host
/// wrapped around a pointer of its own is never an answer of this one.
pub(crate) fn is_function_template(value: &api::Local) -> bool {
    let Some(pointer) = external_pointer(value) else {
        return false;
    };
    let Some(realm) = crate::realm::current() else {
        return false;
    };
    // SAFETY: a realm lives in the agent of a live isolate, and the engine
    // isolate is the first field of `IsolateInner`, so the pointer the realm
    // carries names a live inner — see `Isolate::from_engine_ptr`.
    let isolate = unsafe { crate::Isolate::from_engine_ptr(realm.isolate()) };
    isolate.owns_template(pointer)
}

/// A private name: a symbol this isolate minted as one for
/// [`Private::for_api`](crate::Private::for_api), and holds. The engine has no
/// private-name kind, so "is this a private name" is the same shape of question
/// as "is this a template" — whether the isolate is holding it.
pub(crate) fn is_private(value: &api::Local) -> bool {
    let Some(symbol) = value.value().as_symbol() else {
        return false;
    };
    let Some(realm) = crate::realm::current() else {
        return false;
    };
    // SAFETY: as in `is_function_template`: a realm lives in the agent of a live
    // isolate, and the engine isolate is the first field of `IsolateInner`.
    let isolate = unsafe { crate::Isolate::from_engine_ptr(realm.isolate()) };
    isolate.owns_private(symbol)
}

/// The host pointer an `External` carries, for a value that is one.
fn external_pointer(value: &api::Local) -> Option<*mut std::ffi::c_void> {
    match &value.value().as_object()?.kind {
        ObjectKind::External(pointer) => Some(*pointer as *mut std::ffi::c_void),
        _ => None,
    }
}

pub(crate) fn is_array(value: &api::Local) -> bool {
    object_matches(value, |kind| matches!(kind, ObjectKind::Array(_)))
}

pub(crate) fn is_proxy(value: &api::Local) -> bool {
    object_matches(value, |kind| matches!(kind, ObjectKind::Proxy(_)))
}

/// A `String` object, not a string primitive.
pub(crate) fn is_string_object(value: &api::Local) -> bool {
    object_matches(value, |kind| matches!(kind, ObjectKind::String(_)))
}

/// An arguments object (spec 10.4.4).
pub(crate) fn is_arguments_object(value: &api::Local) -> bool {
    object_matches(value, |kind| matches!(kind, ObjectKind::Arguments(_)))
}

/// A module namespace object (spec 10.4.6).
pub(crate) fn is_module_namespace_object(value: &api::Local) -> bool {
    object_matches(value, |kind| matches!(kind, ObjectKind::ModuleNamespace(_)))
}

pub(crate) fn is_typed_array(value: &api::Local) -> bool {
    object_matches(value, |kind| matches!(kind, ObjectKind::IntegerIndexed(_)))
}

/// A view over an array buffer: a typed array, or a `DataView`.
pub(crate) fn is_array_buffer_view(value: &api::Local) -> bool {
    is_typed_array(value) || is_data_view(value)
}

/// A generator instance. The engine keys each one's suspended state by object
/// identity, so that table is what marks it out; async generators have a table
/// of their own, and are not generator objects in the crate we stand in for
/// either.
pub(crate) fn is_generator_object(value: &api::Local) -> bool {
    in_table(value, |agent, id| agent.generators.contains_key(&id))
}

/// A native error. The engine's notion is `[[ErrorData]]`, which is the one the
/// crate we stand in for uses here: an instance of a subclass of `Error`
/// answers true, while an object merely given `Error.prototype` answers false.
pub(crate) fn is_native_error(value: &api::Local) -> bool {
    in_table(value, |agent, id| agent.error_data.contains(&id))
}

/// One predicate per brand the agent records as a table of instances keyed by
/// object identity.
macro_rules! table_predicates {
    ($($name:ident => $table:ident),* $(,)?) => {
        $(
            pub(crate) fn $name(value: &api::Local) -> bool {
                in_table(value, |agent, id| agent.$table.contains_key(&id))
            }
        )*
    };
}

table_predicates! {
    is_data_view => dataview_data,
    is_map => map_data,
    is_set => set_data,
    is_weak_map => weak_map_data,
    is_weak_set => weak_set_data,
    is_map_iterator => map_iter_data,
    is_set_iterator => set_iter_data,
    is_date => date_data,
    is_reg_exp => regexp_data,
    is_promise => promises,
    is_boolean_object => boolean_data,
    is_number_object => number_data,
    is_symbol_object => symbol_data,
    is_big_int_object => bigint_data,
}

/// The two buffer kinds share one table, so the `SharedArrayBuffer` flag in the
/// entry is what separates them.
pub(crate) fn is_array_buffer(value: &api::Local) -> bool {
    in_table(value, |agent, id| {
        agent
            .buffer_data
            .get(&id)
            .is_some_and(|state| !state.borrow().is_shared)
    })
}

pub(crate) fn is_shared_array_buffer(value: &api::Local) -> bool {
    in_table(value, |agent, id| {
        agent
            .buffer_data
            .get(&id)
            .is_some_and(|state| state.borrow().is_shared)
    })
}

/// Whether the function's own record answers `question`. The kind is a property
/// of the function, not of its prototype chain — which is why a bound function
/// an async one was bound into answers false here, as it does in the crate we
/// stand in for.
fn function_record(value: &api::Local, question: impl Fn(&EcmaFunction) -> bool) -> bool {
    let Some(function) = value.value().as_function() else {
        return false;
    };
    let id = function.id();
    ask(|agent| agent.ecma_functions.get(&id).is_some_and(&question)).unwrap_or(false)
}

pub(crate) fn is_async_function(value: &api::Local) -> bool {
    // An async generator function is neither kind here, which is also how the
    // crate we stand in for reads it.
    function_record(value, |data| data.is_async && !data.is_generator)
}

pub(crate) fn is_generator_function(value: &api::Local) -> bool {
    function_record(value, |data| data.is_generator && !data.is_async)
}

/// Whether one of the agent's identity-keyed tables holds the object the value
/// names.
fn in_table(value: &api::Local, holds: impl Fn(&Agent, u64) -> bool) -> bool {
    let Some(object) = value.value().as_object() else {
        return false;
    };
    let id = object.id();
    ask(|agent| holds(agent, id)).unwrap_or(false)
}

/// Run `question` against the agent, when a context is entered. With none there
/// is no agent to ask, and every table question answers false.
fn ask<T>(question: impl FnOnce(&mut Agent) -> T) -> Option<T> {
    crate::realm::with_agent(question)
}

/// Whether the value is an ordinary object whose internal kind satisfies
/// `predicate`.
fn object_matches(value: &api::Local, predicate: impl Fn(&ObjectKind) -> bool) -> bool {
    value
        .value()
        .as_object()
        .is_some_and(|object| predicate(&object.kind))
}

/// Whether the value is an integer-indexed object viewing the given element
/// type.
fn typed_array_where(value: &api::Local, predicate: impl Fn(ElementType) -> bool) -> bool {
    object_matches(value, |kind| match kind {
        ObjectKind::IntegerIndexed(slots) => predicate(slots.element_type),
        _ => false,
    })
}

/// Whether the value is a number that is an integer in `i32` range.
pub(crate) fn is_int32(value: &api::Local) -> bool {
    value
        .as_number()
        .is_some_and(|n| n.fract() == 0.0 && (-2147483648.0..=2147483647.0).contains(&n))
}

/// Whether the value is a number that is an integer in `u32` range.
pub(crate) fn is_uint32(value: &api::Local) -> bool {
    value
        .as_number()
        .is_some_and(|n| n.fract() == 0.0 && (0.0..=4294967295.0).contains(&n))
}

/// The typed-array predicates, one per element type.
macro_rules! typed_array_predicates {
    ($($name:ident => $element:ident),* $(,)?) => {
        $(
            pub(crate) fn $name(value: &api::Local) -> bool {
                typed_array_where(value, |kind| matches!(kind, ElementType::$element))
            }
        )*
    };
}

typed_array_predicates! {
    is_int8_array => Int8,
    is_uint8_array => Uint8,
    is_uint8_clamped_array => Uint8Clamped,
    is_int16_array => Int16,
    is_uint16_array => Uint16,
    is_int32_array => Int32,
    is_uint32_array => Uint32,
    is_float16_array => Float16,
    is_float32_array => Float32,
    is_float64_array => Float64,
    is_big_int64_array => BigInt64,
    is_big_uint64_array => BigUint64,
}

macro_rules! impl_from {
    ($($source:ident => $target:ident),* $(,)?) => {
        $(
            impl<'s> From<Local<'s, $source>> for Local<'s, $target> {
                fn from(local: Local<'s, $source>) -> Self {
                    local.retag()
                }
            }
        )*
    };
}

// Every upcast the crate we stand in for declares: each tag to the bases it is
// reachable from by `Deref`, which is what makes `.into()` work across the
// hierarchy rather than only one step at a time.
impl_from! {
    Value => Data,
    Context => Data,
    Module => Data,
    ModuleRequest => Data,
    Private => Data,
    FixedArray => Data,
    PrimitiveArray => Data,
    AccessorSignature => Data,
    Signature => Data,
    UnboundScript => Data,
    UnboundModuleScript => Data,
    Template => Data,
    FunctionTemplate => Data,
    ObjectTemplate => Data,

    External => Value,
    Object => Value,
    Array => Value,
    ArrayBuffer => Value,
    ArrayBufferView => Value,
    DataView => Value,
    TypedArray => Value,
    BigInt64Array => Value,
    BigUint64Array => Value,
    Float16Array => Value,
    Float32Array => Value,
    Float64Array => Value,
    Int16Array => Value,
    Int32Array => Value,
    Int8Array => Value,
    Uint16Array => Value,
    Uint32Array => Value,
    Uint8Array => Value,
    Uint8ClampedArray => Value,
    BigIntObject => Value,
    BooleanObject => Value,
    Date => Value,
    Function => Value,
    Map => Value,
    NumberObject => Value,
    Promise => Value,
    PromiseResolver => Value,
    Proxy => Value,
    RegExp => Value,
    Set => Value,
    SharedArrayBuffer => Value,
    StringObject => Value,
    SymbolObject => Value,
    WasmMemoryObject => Value,
    WasmModuleObject => Value,
    Primitive => Value,
    BigInt => Value,
    Boolean => Value,
    Name => Value,
    String => Value,
    Symbol => Value,
    Number => Value,
    Integer => Value,
    Int32 => Value,
    Uint32 => Value,

    Array => Object,
    ArrayBuffer => Object,
    ArrayBufferView => Object,
    DataView => Object,
    TypedArray => Object,
    BigInt64Array => Object,
    BigUint64Array => Object,
    Float16Array => Object,
    Float32Array => Object,
    Float64Array => Object,
    Int16Array => Object,
    Int32Array => Object,
    Int8Array => Object,
    Uint16Array => Object,
    Uint32Array => Object,
    Uint8Array => Object,
    Uint8ClampedArray => Object,
    BigIntObject => Object,
    BooleanObject => Object,
    Date => Object,
    Function => Object,
    Map => Object,
    NumberObject => Object,
    Promise => Object,
    PromiseResolver => Object,
    Proxy => Object,
    RegExp => Object,
    Set => Object,
    SharedArrayBuffer => Object,
    StringObject => Object,
    SymbolObject => Object,
    WasmMemoryObject => Object,
    WasmModuleObject => Object,

    BigInt => Primitive,
    Boolean => Primitive,
    Name => Primitive,
    String => Primitive,
    Symbol => Primitive,
    Number => Primitive,
    Integer => Primitive,
    Int32 => Primitive,
    Uint32 => Primitive,

    String => Name,
    Symbol => Name,

    Integer => Number,
    Int32 => Number,
    Uint32 => Number,

    Int32 => Integer,
    Uint32 => Integer,

    DataView => ArrayBufferView,
    TypedArray => ArrayBufferView,
    BigInt64Array => ArrayBufferView,
    BigUint64Array => ArrayBufferView,
    Float16Array => ArrayBufferView,
    Float32Array => ArrayBufferView,
    Float64Array => ArrayBufferView,
    Int16Array => ArrayBufferView,
    Int32Array => ArrayBufferView,
    Int8Array => ArrayBufferView,
    Uint16Array => ArrayBufferView,
    Uint32Array => ArrayBufferView,
    Uint8Array => ArrayBufferView,
    Uint8ClampedArray => ArrayBufferView,

    BigInt64Array => TypedArray,
    BigUint64Array => TypedArray,
    Float16Array => TypedArray,
    Float32Array => TypedArray,
    Float64Array => TypedArray,
    Int16Array => TypedArray,
    Int32Array => TypedArray,
    Int8Array => TypedArray,
    Uint16Array => TypedArray,
    Uint32Array => TypedArray,
    Uint8Array => TypedArray,
    Uint8ClampedArray => TypedArray,

    FunctionTemplate => Template,
    ObjectTemplate => Template,

    // Every tag to `Data`, which is the transitive half of the list above: the
    // crate we stand in for declares each descendant against *every* base it is
    // reachable from by `Deref`, and a host that upcasts two steps at once
    // (`Local<Function>` to `Local<Data>`, say) needs the pair to exist.
    Array => Data,
    ArrayBuffer => Data,
    ArrayBufferView => Data,
    BigInt => Data,
    BigInt64Array => Data,
    BigIntObject => Data,
    BigUint64Array => Data,
    Boolean => Data,
    BooleanObject => Data,
    DataView => Data,
    Date => Data,
    External => Data,
    Float32Array => Data,
    Float64Array => Data,
    Function => Data,
    Int16Array => Data,
    Int32 => Data,
    Int32Array => Data,
    Int8Array => Data,
    Integer => Data,
    Map => Data,
    Name => Data,
    Number => Data,
    NumberObject => Data,
    Object => Data,
    Primitive => Data,
    Promise => Data,
    PromiseResolver => Data,
    Proxy => Data,
    RegExp => Data,
    Set => Data,
    SharedArrayBuffer => Data,
    String => Data,
    StringObject => Data,
    Symbol => Data,
    SymbolObject => Data,
    TypedArray => Data,
    Uint16Array => Data,
    Uint32 => Data,
    Uint32Array => Data,
    Uint8Array => Data,
    Uint8ClampedArray => Data,
    WasmMemoryObject => Data,
    WasmModuleObject => Data,
}

macro_rules! impl_try_from {
    ($($source:ident => $target:ident),* $(,)?) => {
        $(
            impl<'s> TryFrom<Local<'s, $source>> for Local<'s, $target> {
                type Error = DataError;

                fn try_from(local: Local<'s, $source>) -> Result<Self, Self::Error> {
                    if <$target as TagCheck>::check(local.payload()) {
                        Ok(local.retag())
                    } else {
                        Err(DataError::bad_type::<$target, $source>())
                    }
                }
            }
        )*
    };
}

// Downcasts, always from a base to something below it. A pair is absent when
// the tag below has no honest predicate (see `TagCheck`): the compiler then
// reports the missing cast instead of the cast silently failing.
impl_try_from! {
    Data => Context,
    Data => Module,
    Data => Private,
    Data => Value,
    Data => Primitive,
    Data => Name,
    Data => String,
    Data => Symbol,
    Data => Number,
    Data => Integer,
    Data => Int32,
    Data => Uint32,
    Data => BigInt,
    Data => Boolean,
    Data => External,
    Data => Object,
    Data => Array,
    Data => Function,
    Data => Proxy,
    Data => StringObject,
    Data => TypedArray,
    Data => Int8Array,
    Data => Uint8Array,
    Data => Uint8ClampedArray,
    Data => Int16Array,
    Data => Uint16Array,
    Data => Int32Array,
    Data => Uint32Array,
    Data => Float16Array,
    Data => Float32Array,
    Data => Float64Array,
    Data => BigInt64Array,
    Data => BigUint64Array,
    Data => ArrayBuffer,
    Data => SharedArrayBuffer,
    Data => DataView,
    Data => ArrayBufferView,
    Data => Map,
    Data => Set,
    Data => Date,
    Data => RegExp,
    Data => Promise,
    Data => BigIntObject,
    Data => BooleanObject,
    Data => NumberObject,
    Data => SymbolObject,
    Data => Template,
    Data => FunctionTemplate,

    Value => Primitive,
    Value => Name,
    Value => String,
    Value => Symbol,
    Value => Number,
    Value => Integer,
    Value => Int32,
    Value => Uint32,
    Value => BigInt,
    Value => Boolean,
    Value => External,
    Value => Object,
    Value => Array,
    Value => Function,
    Value => Proxy,
    Value => StringObject,
    Value => TypedArray,
    Value => Int8Array,
    Value => Uint8Array,
    Value => Uint8ClampedArray,
    Value => Int16Array,
    Value => Uint16Array,
    Value => Int32Array,
    Value => Uint32Array,
    Value => Float16Array,
    Value => Float32Array,
    Value => Float64Array,
    Value => BigInt64Array,
    Value => BigUint64Array,
    Value => ArrayBuffer,
    Value => SharedArrayBuffer,
    Value => DataView,
    Value => ArrayBufferView,
    Value => Map,
    Value => Set,
    Value => Date,
    Value => RegExp,
    Value => Promise,
    Value => BigIntObject,
    Value => BooleanObject,
    Value => NumberObject,
    Value => SymbolObject,

    Object => Array,
    Object => Function,
    Object => Proxy,
    Object => StringObject,
    Object => TypedArray,
    Object => Int8Array,
    Object => Uint8Array,
    Object => Uint8ClampedArray,
    Object => Int16Array,
    Object => Uint16Array,
    Object => Int32Array,
    Object => Uint32Array,
    Object => Float16Array,
    Object => Float32Array,
    Object => Float64Array,
    Object => BigInt64Array,
    Object => BigUint64Array,
    Object => ArrayBuffer,
    Object => SharedArrayBuffer,
    Object => DataView,
    Object => ArrayBufferView,
    Object => Map,
    Object => Set,
    Object => Date,
    Object => RegExp,
    Object => Promise,
    Object => BigIntObject,
    Object => BooleanObject,
    Object => NumberObject,
    Object => SymbolObject,

    Primitive => Name,
    Primitive => String,
    Primitive => Symbol,
    Primitive => Number,
    Primitive => Integer,
    Primitive => Int32,
    Primitive => Uint32,
    Primitive => BigInt,
    Primitive => Boolean,

    Name => String,
    Name => Symbol,

    Number => Integer,
    Number => Int32,
    Number => Uint32,

    Integer => Int32,
    Integer => Uint32,

    ArrayBufferView => TypedArray,
    ArrayBufferView => Int8Array,
    ArrayBufferView => Uint8Array,
    ArrayBufferView => Uint8ClampedArray,
    ArrayBufferView => Int16Array,
    ArrayBufferView => Uint16Array,
    ArrayBufferView => Int32Array,
    ArrayBufferView => Uint32Array,
    ArrayBufferView => Float16Array,
    ArrayBufferView => Float32Array,
    ArrayBufferView => Float64Array,
    ArrayBufferView => BigInt64Array,
    ArrayBufferView => BigUint64Array,
    ArrayBufferView => DataView,

    TypedArray => Int8Array,
    TypedArray => Uint8Array,
    TypedArray => Uint8ClampedArray,
    TypedArray => Int16Array,
    TypedArray => Uint16Array,
    TypedArray => Int32Array,
    TypedArray => Uint32Array,
    TypedArray => Float16Array,
    TypedArray => Float32Array,
    TypedArray => Float64Array,
    TypedArray => BigInt64Array,
    TypedArray => BigUint64Array,

    Template => FunctionTemplate,
}

/// The tags the crate we stand in for hashes by identity, which is the half of
/// its hash surface this bridge can answer: every one of them names an object (or
/// a module record), so the identity `PartialEq` already compares — the object's
/// id in the arena — is the hash. The value tags (`Value`, `Name`, `String`,
/// `Symbol` and the primitives) are the other half and are absent: their
/// equality here is the value's, and a hash that agrees with it needs the
/// same-value work `Local`'s `PartialEq` does not do yet.
macro_rules! identity_hashes {
    ($($tag:ident),* $(,)?) => {
        $(
            impl Eq for Local<'_, $tag> {}

            impl Eq for Global<$tag> {}

            impl Hash for Local<'_, $tag> {
                fn hash<H: Hasher>(&self, state: &mut H) {
                    self.identity_hash().hash(state);
                }
            }

            impl Hash for Global<$tag> {
                fn hash<H: Hasher>(&self, state: &mut H) {
                    self.handle().identity_hash().hash(state);
                }
            }
        )*
    };
}

impl<'s, T> Local<'s, T> {
    /// The identity hash of the thing this handle names, for the tagged impls
    /// above; a crate-private door onto [`identity_hash`](crate::handle::identity_hash).
    pub(crate) fn identity_hash(&self) -> NonZeroI32 {
        crate::handle::identity_hash(self.payload())
    }
}

identity_hashes! {
    Module,
    Object,
    Array,
    Function,
    Promise,
    PromiseResolver,
    Proxy,
    RegExp,
    Date,
    Map,
    Set,
    StringObject,
    NumberObject,
    BooleanObject,
    SymbolObject,
    BigIntObject,
    ArrayBuffer,
    SharedArrayBuffer,
    ArrayBufferView,
    DataView,
    TypedArray,
    Uint8Array,
    Uint8ClampedArray,
    Int8Array,
    Uint16Array,
    Int16Array,
    Uint32Array,
    Int32Array,
    Float16Array,
    Float32Array,
    Float64Array,
    BigInt64Array,
    BigUint64Array,
}

/// The error of a failed [`Local`] cast: the value was not of the type the tag
/// named.
#[derive(Clone, Copy, Debug)]
pub enum DataError {
    BadType {
        actual: &'static str,
        expected: &'static str,
    },
    NoData {
        expected: &'static str,
    },
}

impl DataError {
    /// Built by a failed `TryFrom` cast between tags.
    pub(crate) fn bad_type<E: 'static, A: 'static>() -> Self {
        Self::BadType {
            expected: type_name::<E>(),
            actual: type_name::<A>(),
        }
    }

    pub(crate) fn no_data<E: 'static>() -> Self {
        Self::NoData {
            expected: type_name::<E>(),
        }
    }
}

impl Display for DataError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadType { expected, actual } => {
                write!(f, "expected type `{expected}`, got `{actual}`")
            }
            Self::NoData { expected } => {
                write!(f, "expected `Some({expected})`, found `None`")
            }
        }
    }
}

impl Error for DataError {}
