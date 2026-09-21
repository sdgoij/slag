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
use std::ops::Deref;

use crux::object::ObjectKind;
use crux::typed_array::ElementType;
use runtime::api;

use crate::handle::{Local, Payload};

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
        matches!(payload, Payload::Value(_) | Payload::Context(_))
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

macro_rules! tag_checks {
    ($($tag:ident => $check:expr),* $(,)?) => {
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
    Primitive => |v: &api::Local| !v.is_object(),
    Name => |v: &api::Local| v.is_string() || v.is_symbol(),
    String => |v: &api::Local| v.is_string(),
    Symbol => |v: &api::Local| v.is_symbol(),
    Number => |v: &api::Local| v.is_number(),
    Integer => |v: &api::Local| is_int32(v) || is_uint32(v),
    Int32 => |v: &api::Local| is_int32(v),
    Uint32 => |v: &api::Local| is_uint32(v),
    BigInt => |v: &api::Local| v.is_bigint(),
    Boolean => |v: &api::Local| v.is_boolean(),
    External => |v: &api::Local| object_matches(v, |k| matches!(k, ObjectKind::External(_))),
    Object => |v: &api::Local| v.is_object(),
    Array => |v: &api::Local| object_matches(v, |k| matches!(k, ObjectKind::Array(_))),
    Function => |v: &api::Local| v.is_function(),
    Proxy => |v: &api::Local| object_matches(v, |k| matches!(k, ObjectKind::Proxy(_))),
    StringObject => |v: &api::Local| object_matches(v, |k| matches!(k, ObjectKind::String(_))),
    TypedArray => |v: &api::Local| object_matches(v, |k| matches!(k, ObjectKind::IntegerIndexed(_))),
    Int8Array => |v: &api::Local| typed_array_where(v, |t| matches!(t, ElementType::Int8)),
    Uint8Array => |v: &api::Local| typed_array_where(v, |t| matches!(t, ElementType::Uint8)),
    Uint8ClampedArray => |v: &api::Local| typed_array_where(v, |t| matches!(t, ElementType::Uint8Clamped)),
    Int16Array => |v: &api::Local| typed_array_where(v, |t| matches!(t, ElementType::Int16)),
    Uint16Array => |v: &api::Local| typed_array_where(v, |t| matches!(t, ElementType::Uint16)),
    Int32Array => |v: &api::Local| typed_array_where(v, |t| matches!(t, ElementType::Int32)),
    Uint32Array => |v: &api::Local| typed_array_where(v, |t| matches!(t, ElementType::Uint32)),
    Float16Array => |v: &api::Local| typed_array_where(v, |t| matches!(t, ElementType::Float16)),
    Float32Array => |v: &api::Local| typed_array_where(v, |t| matches!(t, ElementType::Float32)),
    Float64Array => |v: &api::Local| typed_array_where(v, |t| matches!(t, ElementType::Float64)),
    BigInt64Array => |v: &api::Local| typed_array_where(v, |t| matches!(t, ElementType::BigInt64)),
    BigUint64Array => |v: &api::Local| typed_array_where(v, |t| matches!(t, ElementType::BigUint64)),
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

// Brands the engine keeps no internal tag for, read off the prototype chain
// instead. The engine does register the intrinsic prototypes these compare
// against, so the names below are the ones it actually has.
tag_checks! {
    ArrayBuffer => |v: &api::Local| inherits_intrinsic(v, "%ArrayBuffer.prototype%"),
    SharedArrayBuffer => |v: &api::Local| inherits_intrinsic(v, "%SharedArrayBuffer.prototype%"),
    DataView => |v: &api::Local| inherits_intrinsic(v, "%DataView.prototype%"),
    ArrayBufferView => |v: &api::Local| is_array_buffer_view(v),
    Map => |v: &api::Local| inherits_intrinsic(v, "%Map.prototype%"),
    Set => |v: &api::Local| inherits_intrinsic(v, "%Set.prototype%"),
    Date => |v: &api::Local| inherits_intrinsic(v, "%Date.prototype%"),
    RegExp => |v: &api::Local| inherits_intrinsic(v, "%RegExp.prototype%"),
    Promise => |v: &api::Local| inherits_intrinsic(v, "%Promise.prototype%"),
    BigIntObject => |v: &api::Local| inherits_intrinsic(v, "%BigInt.prototype%"),
    BooleanObject => |v: &api::Local| inherits_intrinsic(v, "%Boolean.prototype%"),
    NumberObject => |v: &api::Local| inherits_intrinsic(v, "%Number.prototype%"),
    SymbolObject => |v: &api::Local| inherits_intrinsic(v, "%Symbol.prototype%"),
}

/// A view over an array buffer: a typed array, or a `DataView`.
fn is_array_buffer_view(value: &api::Local) -> bool {
    object_matches(value, |kind| matches!(kind, ObjectKind::IntegerIndexed(_)))
        || inherits_intrinsic(value, "%DataView.prototype%")
}

/// Whether the value is an object whose prototype chain reaches the intrinsic
/// prototype `name` (for example `%ArrayBuffer.prototype%`).
///
/// The engine exposes no brand tags to this crate, so the brand is discovered
/// the way a script would: walk `[[Prototype]]` to the root and compare. That
/// is spoofable — an object can be handed a foreign prototype — and it is not
/// cheap, since every step is a property operation. Both are acceptable while
/// the boundary is being proven; a brand the engine knows is the durable fix.
fn inherits_intrinsic(value: &api::Local, name: &str) -> bool {
    let Some(realm) = crate::realm::current() else {
        return false;
    };
    let Some(brand) = realm.intrinsic(name) else {
        return false;
    };
    let Some(object) = value.value().as_object() else {
        return false;
    };
    let mut current = crux::value::Value::Object(object);
    // The chain is finite and acyclic by construction; the bound guards only
    // against that changing.
    for _ in 0..64 {
        let local = api::Local::from(current);
        let Ok(prototype) = api::Object::get_prototype(&realm, &local) else {
            return false;
        };
        let prototype = *prototype.value();
        if prototype == brand {
            return true;
        }
        if prototype.is_null() {
            return false;
        }
        current = prototype;
    }
    false
}

/// Whether the value is a number that is an integer in `i32` range.
fn is_int32(value: &api::Local) -> bool {
    value
        .as_number()
        .is_some_and(|n| n.fract() == 0.0 && (-2147483648.0..=2147483647.0).contains(&n))
}

/// Whether the value is a number that is an integer in `u32` range.
fn is_uint32(value: &api::Local) -> bool {
    value
        .as_number()
        .is_some_and(|n| n.fract() == 0.0 && (0.0..=4294967295.0).contains(&n))
}

macro_rules! impl_from {
    ($($source:ident => $target:ident),* $(,)?) => {
        $(
            impl<'s> From<Local<'s, $source>> for Local<'s, $target> {
                fn from(local: Local<'s, $source>) -> Self {
                    local.cast()
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
}

macro_rules! impl_try_from {
    ($($source:ident => $target:ident),* $(,)?) => {
        $(
            impl<'s> TryFrom<Local<'s, $source>> for Local<'s, $target> {
                type Error = DataError;

                fn try_from(local: Local<'s, $source>) -> Result<Self, Self::Error> {
                    if <$target as TagCheck>::check(local.payload()) {
                        Ok(local.cast())
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

    #[allow(dead_code)]
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
