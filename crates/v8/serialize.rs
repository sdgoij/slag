//! Value serialization and deserialization (`v8::ValueSerializer`,
//! `v8::ValueDeserializer`).
//!
//! # The wire format is this bridge's own
//!
//! Version 1, and not V8's. V8's format is internal — `value-serializer.cc` is
//! three thousand lines of C++ carrying sixteen versions of history — and
//! matching it would buy exactly one thing: exchanging blobs with a real V8
//! process. Every use `deno_core` makes of this API is in-process
//! (`postMessage`, `structuredClone`, `BroadcastChannel`, `node:v8`), so the
//! format here is the smallest one that carries what the walk below covers. The
//! snapshot and the platform in this crate diverge the same way, and say so in
//! their own headers.
//!
//! # What it carries
//!
//! `undefined`, `null`, booleans; numbers as the `f64` itself, so `-0` and
//! every NaN survive; strings as UTF-16 code units, so a lone surrogate
//! survives; BigInts by sign and 64-bit words; arrays, with a hole written as a
//! hole; ordinary objects by their own enumerable string keys; `Map`, `Set`,
//! `Date`, `RegExp`, `ArrayBuffer`, typed arrays and `DataView`; and object
//! identity, so a cycle round-trips as a cycle and two references to one object
//! come back as one object.
//!
//! A host object is written and read through the delegate's hooks, and a shared
//! array buffer through its transfer-id hook. The wasm module transfer-id hook
//! is part of the delegate surface but unreachable here: the engine hands out
//! no wasm module value for the walk to recognize.
//!
//! Anything else — a function, a symbol, a promise, a proxy, a weak collection,
//! an iterator, an `Error`, a primitive wrapper — fails through
//! `throw_data_clone_error`, which is how V8 reports a value it will not clone.
//!
//! # Two gaps, both narrow
//!
//! A property *key* that is not UTF-8 clean is rendered as U+FFFD on the way
//! out, because the bridge's own property enumeration spells keys as a `str`; a
//! *value* string is exact. And a malformed stream sets a pending `Error` where
//! V8 raises a `DOMException` `DataCloneError`, which the engine has no type
//! for.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::pin::pin;

use crux::object::ObjectKind;
use crux::typed_array::ElementType;
use crux::value::Value as EngineValue;
use crux::value::ValueKind;

use runtime::api;

use crate::data::{
    Array, ArrayBuffer, ArrayBufferView, BigInt, Context, Map, Name, Number, Object, Set,
    SharedArrayBuffer, String as V8String, Value,
};
use crate::handle::Local;
use crate::property_descriptor::PropertyDescriptor;
use crate::scope::{CallbackScope, ContextScope, GetIsolate, PinScope};
use crate::support::{BackingStore, SharedRef};
use crate::{Isolate, NewStringType};

/// The format version this bridge writes (`write_header`) and reads.
const WIRE_FORMAT_VERSION: u8 = 1;

/// The byte that starts a header, the way V8's `kVersionTag` does.
const HEADER_TAG: u8 = 0xFF;

// What a value's bytes start with. The layout of each is in the walk that
// writes it, which is the only place both ends have to agree.
const TAG_HOLE: u8 = 0x00;
const TAG_UNDEFINED: u8 = 0x01;
const TAG_NULL: u8 = 0x02;
const TAG_TRUE: u8 = 0x03;
const TAG_FALSE: u8 = 0x04;
const TAG_NUMBER: u8 = 0x05;
const TAG_STRING: u8 = 0x06;
const TAG_BIG_INT: u8 = 0x07;
const TAG_REFERENCE: u8 = 0x08;
const TAG_ARRAY: u8 = 0x09;
const TAG_OBJECT: u8 = 0x0A;
const TAG_MAP: u8 = 0x0B;
const TAG_SET: u8 = 0x0C;
const TAG_DATE: u8 = 0x0D;
const TAG_REG_EXP: u8 = 0x0E;
const TAG_ARRAY_BUFFER: u8 = 0x0F;
const TAG_TRANSFER_ARRAY_BUFFER: u8 = 0x10;
const TAG_SHARED_ARRAY_BUFFER: u8 = 0x11;
const TAG_ARRAY_BUFFER_VIEW: u8 = 0x12;
const TAG_HOST_OBJECT: u8 = 0x13;

/// The element kind of a `DataView`, which is not a typed array and so has no
/// element type in the tag.
const VIEW_DATA_VIEW: u8 = 0;

/// A walk that has already reported why it stopped: the delegate was handed a
/// `throw_data_clone_error`, or an operation on the scope already threw.
struct Failed;

/// The wire tag for a typed array's element type.
fn element_kind(element_type: ElementType) -> u8 {
    match element_type {
        ElementType::Int8 => 1,
        ElementType::Uint8 => 2,
        ElementType::Uint8Clamped => 3,
        ElementType::Int16 => 4,
        ElementType::Uint16 => 5,
        ElementType::Int32 => 6,
        ElementType::Uint32 => 7,
        ElementType::Float16 => 8,
        ElementType::Float32 => 9,
        ElementType::Float64 => 10,
        ElementType::BigInt64 => 11,
        ElementType::BigUint64 => 12,
    }
}

/// The `%XArray%` constructor for one of [`element_kind`]'s tags.
fn element_constructor(kind: u8) -> Option<&'static str> {
    match kind {
        1 => Some("%Int8Array%"),
        2 => Some("%Uint8Array%"),
        3 => Some("%Uint8ClampedArray%"),
        4 => Some("%Int16Array%"),
        5 => Some("%Uint16Array%"),
        6 => Some("%Int32Array%"),
        7 => Some("%Uint32Array%"),
        8 => Some("%Float16Array%"),
        9 => Some("%Float32Array%"),
        10 => Some("%Float64Array%"),
        11 => Some("%BigInt64Array%"),
        12 => Some("%BigUint64Array%"),
        _ => None,
    }
}

/// The element type of a typed array handle, or `None` for anything else.
fn element_type_of(value: &Local<'_, Value>) -> Option<ElementType> {
    match value.engine().value().as_object()?.kind {
        ObjectKind::IntegerIndexed(slots) => Some(slots.element_type),
        _ => None,
    }
}

/// The engine value behind a handle.
fn engine_value(value: &Local<'_, Value>) -> EngineValue {
    *value.engine().value()
}

/// The handle for an engine value, at whatever lifetime the caller wants: a
/// handle is plain data and does not borrow from a scope.
fn local_of<'s>(value: EngineValue) -> Local<'s, Value> {
    Local::from_engine(api::Local::from(value))
}

/// The bytes behind a store, borrowed from it.
///
/// # Safety
///
/// Not `unsafe` to call, but the slice points into the block the store owns:
/// it is valid only while the caller holds the store and does not resize or
/// detach the buffer underneath it.
fn store_bytes(store: &BackingStore) -> Option<&[u8]> {
    let length = store.byte_length();
    if length == 0 {
        return Some(&[]);
    }
    let pointer = store.data()?;
    // SAFETY: the pointer is the block's live base and the store is borrowed
    // for the slice's whole life, so the range is readable for that long.
    Some(unsafe { std::slice::from_raw_parts(pointer.as_ptr().cast::<u8>(), length) })
}

/// A descriptor that defines a new own data property with every attribute set,
/// which is `CreateDataProperty` (spec 7.3.5) — the define a deserialize wants,
/// since `[[Set]]` would run a setter the object inherited (`__proto__` is one
/// on `Object.prototype`) instead of creating the property that was written.
fn data_property(value: Local<'_, Value>) -> PropertyDescriptor {
    let mut descriptor = PropertyDescriptor::new_from_value_writable(value, true);
    descriptor.set_enumerable(true);
    descriptor.set_configurable(true);
    descriptor
}

/// A `Local<Name>` for a string name.
fn name_of<'s>(scope: &PinScope<'s, '_, ()>, text: &str) -> Option<Local<'s, Name>> {
    let string = V8String::new(scope, text)?;
    Local::<Name>::try_from(Local::<Value>::from(string)).ok()
}

/// A `Local<Value>` holding a string.
fn string_value<'s>(scope: &PinScope<'s, '_, ()>, text: &str) -> Option<Local<'s, Value>> {
    Some(Local::<Value>::from(V8String::new(scope, text)?))
}

/// Construct through a realm intrinsic (`%Map%`, `%Date%`, …).
fn construct(scope: &PinScope<'_, '_>, name: &str, args: &[EngineValue]) -> Option<EngineValue> {
    let realm = crate::realm_of(scope);
    let constructor = api::Local::from(realm.intrinsic(name)?);
    let args: Vec<api::Local> = args.iter().copied().map(api::Local::from).collect();
    let value = realm.construct(&constructor, &args).to_local()?;
    Some(*value.value())
}

/// Call a realm intrinsic with `this` (`%Map.prototype.set%`,
/// `%Set.prototype.add%`).
///
/// The intrinsic, not the global: a script that replaces `Map.prototype.set`
/// cannot change what a deserialize does, which is what keeps the read side
/// from running host code.
fn call_intrinsic(
    scope: &PinScope<'_, '_>,
    name: &str,
    this: EngineValue,
    args: &[EngineValue],
) -> Option<EngineValue> {
    let realm = crate::realm_of(scope);
    let function = api::Local::from(realm.intrinsic(name)?);
    let this = api::Local::from(this);
    let args: Vec<api::Local> = args.iter().copied().map(api::Local::from).collect();
    let value = realm.call(&function, &this, &args).to_local()?;
    Some(*value.value())
}

/// The `[[DateValue]]` the agent records for a Date instance.
fn date_time_value(value: &Local<'_, Value>) -> Option<f64> {
    let id = value.engine().value().as_object()?.id();
    crate::realm::with_agent(|agent| agent.date_data.get(&id).copied()).flatten()
}

/// The source and flags the agent records for a RegExp instance.
fn reg_exp_text(value: &Local<'_, Value>) -> Option<(Vec<u16>, String)> {
    let id = value.engine().value().as_object()?.id();
    crate::realm::with_agent(|agent| {
        let state = agent.regexp_data.get(&id)?;
        Some((state.source.as_slice().to_vec(), state.flags_text.clone()))
    })
    .flatten()
}

/// A brand the engine records in an agent table that the crate we stand in for
/// does not clone, and the message that says so.
///
/// The list is explicit rather than a fallback so that a branded object is
/// never written as an empty plain object: the brands below are the ones the
/// bridge has a predicate for.
fn uncloneable_brand(value: &Local<'_, Value>) -> Option<&'static str> {
    if value.is_promise() {
        return Some("A promise could not be cloned.");
    }
    if value.is_weak_map() || value.is_weak_set() {
        return Some("A weak collection could not be cloned.");
    }
    if value.is_map_iterator() || value.is_set_iterator() || value.is_generator_object() {
        return Some("An iterator could not be cloned.");
    }
    if value.is_native_error() {
        return Some("An Error could not be cloned.");
    }
    if value.is_string_object()
        || value.is_number_object()
        || value.is_boolean_object()
        || value.is_symbol_object()
        || value.is_big_int_object()
    {
        return Some("A primitive wrapper could not be cloned.");
    }
    None
}

/// The serializer's side of the API: the delegate, the buffer the walk writes
/// into, and the identity table that makes a cycle a cycle.
///
/// The crate we stand in for pins this because C++ holds a pointer into it;
/// nothing here needs the address to stay put, so it is an ordinary box.
pub struct ValueSerializerHeap<'a> {
    delegate: Box<dyn ValueSerializerImpl + 'a>,
    isolate: Isolate,
    out: RefCell<Vec<u8>>,
    /// Object identity (the engine's object id) to the id written for it.
    memo: RefCell<HashMap<u64, u32>>,
    /// Buffer identity to the transfer id the host registered it under.
    transfers: RefCell<HashMap<u64, u32>>,
    treat_array_buffer_views_as_host_objects: Cell<bool>,
}

impl<'a> ValueSerializerHeap<'a> {
    fn new(delegate: Box<dyn ValueSerializerImpl + 'a>, isolate: Isolate) -> Self {
        Self {
            delegate,
            isolate,
            out: RefCell::new(Vec::new()),
            memo: RefCell::new(HashMap::new()),
            transfers: RefCell::new(HashMap::new()),
            treat_array_buffer_views_as_host_objects: Cell::new(false),
        }
    }

    fn put_u8(&self, byte: u8) {
        self.out.borrow_mut().push(byte);
    }

    fn put_u16(&self, value: u16) {
        self.out
            .borrow_mut()
            .extend_from_slice(&value.to_le_bytes());
    }

    fn put_u32(&self, value: u32) {
        self.out
            .borrow_mut()
            .extend_from_slice(&value.to_le_bytes());
    }

    fn put_u64(&self, value: u64) {
        self.out
            .borrow_mut()
            .extend_from_slice(&value.to_le_bytes());
    }

    fn put_f64(&self, value: f64) {
        self.put_u64(value.to_bits());
    }

    fn put_bytes(&self, bytes: &[u8]) {
        self.out.borrow_mut().extend_from_slice(bytes);
    }

    /// The identity the host registered this buffer under, if it did.
    fn transfer_id_of(&self, identity: u64) -> Option<u32> {
        self.transfers.borrow().get(&identity).copied()
    }

    /// Report a value that cannot be cloned through the delegate, and answer
    /// the marker a walk failure carries.
    fn clone_error(&self, scope: &mut PinScope<'_, '_>, message: &str) -> Failed {
        if let Some(text) = V8String::new(scope, message) {
            self.delegate.throw_data_clone_error(scope, text);
        }
        Failed
    }

    /// Write `value` under `context`'s realm, answering whether it was written;
    /// `None` means the delegate reported a value it could not clone.
    fn serialize(&self, context: Local<'_, Context>, value: Local<'_, Value>) -> Option<bool> {
        let mut isolate = self.isolate;
        // SAFETY: the scope opens no state of its own here; the signature is
        // reproduced for the shapes (see `crate::scope`).
        let storage = unsafe { CallbackScope::new(&mut isolate) };
        let storage = pin!(storage);
        let mut plain = storage.init();
        let current = Local::new(&plain, &context);
        let mut entered = ContextScope::new(&mut plain, current);
        match self.write_value_inner(&mut entered, value) {
            Ok(()) => Some(true),
            Err(Failed) => None,
        }
    }

    fn write_value_inner<'s>(
        &self,
        scope: &mut PinScope<'s, '_>,
        value: Local<'s, Value>,
    ) -> Result<(), Failed> {
        match value.engine().value().kind() {
            ValueKind::Undefined => {
                self.put_u8(TAG_UNDEFINED);
                Ok(())
            }
            ValueKind::Null => {
                self.put_u8(TAG_NULL);
                Ok(())
            }
            ValueKind::Boolean(true) => {
                self.put_u8(TAG_TRUE);
                Ok(())
            }
            ValueKind::Boolean(false) => {
                self.put_u8(TAG_FALSE);
                Ok(())
            }
            ValueKind::Number(number) => {
                self.put_u8(TAG_NUMBER);
                self.put_f64(number);
                Ok(())
            }
            ValueKind::String(_) => self.write_string(scope, value),
            ValueKind::BigInt(_) => self.write_big_int(scope, value),
            ValueKind::Symbol(_) => Err(self.clone_error(scope, "A symbol could not be cloned.")),
            ValueKind::Function(_) => {
                Err(self.clone_error(scope, "A function could not be cloned."))
            }
            ValueKind::Object(_) => self.write_object(scope, value),
        }
    }

    fn write_string(
        &self,
        scope: &mut PinScope<'_, '_>,
        value: Local<'_, Value>,
    ) -> Result<(), Failed> {
        let text = Local::<V8String>::try_from(value)
            .map_err(|_| self.clone_error(scope, "A string could not be cloned."))?;
        self.put_string(scope, &text.to_utf16())
    }

    /// A string value: its code units, little-endian, after the count.
    fn put_string(&self, scope: &mut PinScope<'_, '_>, units: &[u16]) -> Result<(), Failed> {
        let length = u32::try_from(units.len())
            .map_err(|_| self.clone_error(scope, "A string is too long to clone."))?;
        self.put_u8(TAG_STRING);
        self.put_u32(length);
        for unit in units {
            self.put_u16(*unit);
        }
        Ok(())
    }

    fn write_big_int(
        &self,
        scope: &mut PinScope<'_, '_>,
        value: Local<'_, Value>,
    ) -> Result<(), Failed> {
        let big = Local::<BigInt>::try_from(value)
            .map_err(|_| self.clone_error(scope, "A BigInt could not be cloned."))?;
        let mut words = vec![0u64; big.word_count()];
        let (negative, written) = big.to_words_array(&mut words);
        let count = u32::try_from(written.len())
            .map_err(|_| self.clone_error(scope, "A BigInt could not be cloned."))?;
        self.put_u8(TAG_BIG_INT);
        self.put_u8(u8::from(negative));
        self.put_u32(count);
        for word in written.iter() {
            self.put_u64(*word);
        }
        Ok(())
    }

    fn write_object<'s>(
        &self,
        scope: &mut PinScope<'s, '_>,
        value: Local<'s, Value>,
    ) -> Result<(), Failed> {
        let Some(object) = value.engine().value().as_object() else {
            return Err(self.clone_error(scope, "An object could not be cloned."));
        };
        // The identity is recorded before anything is written, so a reference
        // from inside the value to the value itself has an id to name — which
        // is what makes a cycle round-trip.
        let identity = object.id();
        if let Some(id) = self.memo.borrow().get(&identity).copied() {
            self.put_u8(TAG_REFERENCE);
            self.put_u32(id);
            return Ok(());
        }
        let id = match u32::try_from(self.memo.borrow().len()) {
            Ok(id) => id,
            Err(_) => return Err(self.clone_error(scope, "Too many objects to clone.")),
        };
        self.memo.borrow_mut().insert(identity, id);

        match &object.kind {
            ObjectKind::Array(_) => self.write_array(scope, value),
            ObjectKind::IntegerIndexed(_) => self.write_view(scope, value),
            ObjectKind::Ordinary => self.write_ordinary(scope, value),
            // The exotic receivers the crate we stand in for refuses before it
            // looks at the type (`IsSpecialReceiverInstanceType`): a proxy,
            // a module namespace, an arguments object, a boxed string, a host
            // object, an `External`, `$262.IsHTMLDDA`.
            ObjectKind::String(_)
            | ObjectKind::Arguments(_)
            | ObjectKind::Proxy(_)
            | ObjectKind::ModuleNamespace(_)
            | ObjectKind::IsHTMLDDA
            | ObjectKind::External(_)
            | ObjectKind::Host(_) => Err(self.clone_error(scope, "An object could not be cloned.")),
        }
    }

    /// An ordinary object: a branded one by its brand, otherwise a plain object
    /// by its own enumerable string keys.
    fn write_ordinary<'s>(
        &self,
        scope: &mut PinScope<'s, '_>,
        value: Local<'s, Value>,
    ) -> Result<(), Failed> {
        if value.is_array_buffer() {
            return self.write_array_buffer(scope, value);
        }
        if value.is_shared_array_buffer() {
            return self.write_shared_array_buffer(scope, value);
        }
        if value.is_data_view() {
            return self.write_view(scope, value);
        }
        if value.is_map() {
            return self.write_map(scope, value);
        }
        if value.is_set() {
            return self.write_set(scope, value);
        }
        if value.is_date() {
            let Some(time) = date_time_value(&value) else {
                return Err(self.clone_error(scope, "A Date could not be cloned."));
            };
            self.put_u8(TAG_DATE);
            self.put_f64(time);
            return Ok(());
        }
        if value.is_reg_exp() {
            let Some((source, flags)) = reg_exp_text(&value) else {
                return Err(self.clone_error(scope, "A RegExp could not be cloned."));
            };
            self.put_u8(TAG_REG_EXP);
            self.put_string(scope, &source)?;
            return self.put_string(scope, &flags.encode_utf16().collect::<Vec<u16>>());
        }
        if let Some(message) = uncloneable_brand(&value) {
            return Err(self.clone_error(scope, message));
        }
        self.write_plain_object(scope, value)
    }

    fn write_plain_object<'s>(
        &self,
        scope: &mut PinScope<'s, '_>,
        value: Local<'s, Value>,
    ) -> Result<(), Failed> {
        let object = value.cast::<Object>();
        if self.delegate.has_custom_host_object(&self.isolate) {
            match self.delegate.is_host_object(scope, object) {
                Some(true) => return self.write_host_object(scope, object),
                Some(false) => {}
                None => return Err(Failed),
            }
        }
        let Some(names) = object.get_own_property_names(
            scope,
            crate::property::GetPropertyNamesArgs {
                mode: crate::property::KeyCollectionMode::OwnOnly,
                property_filter: crate::property::PropertyFilter::ONLY_ENUMERABLE
                    | crate::property::PropertyFilter::SKIP_SYMBOLS,
                index_filter: crate::property::IndexFilter::IncludeIndices,
                key_conversion: crate::property::KeyConversionMode::ConvertToString,
            },
        ) else {
            return Err(Failed);
        };
        self.put_u8(TAG_OBJECT);
        self.put_u32(names.length());
        for index in 0..names.length() {
            let Some(key) = names.get_index(scope, index) else {
                return Err(Failed);
            };
            self.write_value_inner(scope, key)?;
            let Some(element) = object.get(scope, key) else {
                return Err(Failed);
            };
            self.write_value_inner(scope, element)?;
        }
        Ok(())
    }

    fn write_array(
        &self,
        scope: &mut PinScope<'_, '_>,
        value: Local<'_, Value>,
    ) -> Result<(), Failed> {
        let array = value.cast::<Array>();
        let length = array.length();
        self.put_u8(TAG_ARRAY);
        self.put_u32(length);
        for index in 0..length {
            let Some(key) = string_value(scope, &index.to_string()) else {
                return Err(Failed);
            };
            // `get_index` is a `[[Get]]`, which cannot tell a hole from an
            // element holding `undefined`; the own-property question is the one
            // that can, and it is the one a hole is.
            let present = match array.has_own_property(scope, key) {
                Some(present) => present,
                None => return Err(Failed),
            };
            if !present {
                self.put_u8(TAG_HOLE);
                continue;
            }
            let Some(element) = array.get_index(scope, index) else {
                return Err(Failed);
            };
            self.write_value_inner(scope, element)?;
        }
        Ok(())
    }

    fn write_map(
        &self,
        scope: &mut PinScope<'_, '_>,
        value: Local<'_, Value>,
    ) -> Result<(), Failed> {
        let map = match Local::<Map>::try_from(value) {
            Ok(map) => map,
            Err(_) => return Err(self.clone_error(scope, "A Map could not be cloned.")),
        };
        let entries = map.as_array(scope);
        let count = entries.length() / 2;
        self.put_u8(TAG_MAP);
        self.put_u32(count);
        for index in 0..count {
            for slot in [index * 2, index * 2 + 1] {
                let Some(entry) = entries.get_index(scope, slot) else {
                    return Err(Failed);
                };
                self.write_value_inner(scope, entry)?;
            }
        }
        Ok(())
    }

    fn write_set(
        &self,
        scope: &mut PinScope<'_, '_>,
        value: Local<'_, Value>,
    ) -> Result<(), Failed> {
        let set = match Local::<Set>::try_from(value) {
            Ok(set) => set,
            Err(_) => return Err(self.clone_error(scope, "A Set could not be cloned.")),
        };
        // A set's elements come back doubled — each as its own key and value —
        // so every second one is the element.
        let entries = set.as_array(scope);
        let count = entries.length() / 2;
        self.put_u8(TAG_SET);
        self.put_u32(count);
        for index in 0..count {
            let Some(element) = entries.get_index(scope, index * 2) else {
                return Err(Failed);
            };
            self.write_value_inner(scope, element)?;
        }
        Ok(())
    }

    fn write_view<'s>(
        &self,
        scope: &mut PinScope<'s, '_>,
        value: Local<'s, Value>,
    ) -> Result<(), Failed> {
        if self.treat_array_buffer_views_as_host_objects.get() {
            return self.write_host_object(scope, value.cast::<Object>());
        }
        let Ok(view) = Local::<ArrayBufferView>::try_from(value) else {
            return Err(self.clone_error(scope, "A view could not be cloned."));
        };
        let Some(buffer) = view.buffer(scope) else {
            return Err(self.clone_error(scope, "A view could not be cloned."));
        };
        let (kind, length) = if value.is_data_view() {
            (VIEW_DATA_VIEW, view.byte_length())
        } else {
            let Some(element_type) = element_type_of(&value) else {
                return Err(self.clone_error(scope, "A view could not be cloned."));
            };
            (
                element_kind(element_type),
                view.byte_length() / element_type.size(),
            )
        };
        let (offset, length) = match (u32::try_from(view.byte_offset()), u32::try_from(length)) {
            (Ok(offset), Ok(length)) => (offset, length),
            _ => return Err(self.clone_error(scope, "A view could not be cloned.")),
        };
        self.put_u8(TAG_ARRAY_BUFFER_VIEW);
        self.put_u8(kind);
        self.put_u32(offset);
        self.put_u32(length);
        // The buffer goes last and by identity, so two views over one buffer
        // are two references to one copy.
        self.write_value_inner(scope, Local::<Value>::from(buffer))
    }

    fn write_array_buffer(
        &self,
        scope: &mut PinScope<'_, '_>,
        value: Local<'_, Value>,
    ) -> Result<(), Failed> {
        let Ok(buffer) = Local::<ArrayBuffer>::try_from(value) else {
            return Err(self.clone_error(scope, "An ArrayBuffer could not be cloned."));
        };
        let Some(identity) = value.engine().value().as_object().map(|object| object.id()) else {
            return Err(self.clone_error(scope, "An ArrayBuffer could not be cloned."));
        };
        if let Some(transfer_id) = self.transfer_id_of(identity) {
            self.put_u8(TAG_TRANSFER_ARRAY_BUFFER);
            self.put_u32(transfer_id);
            return Ok(());
        }
        if buffer.was_detached() {
            return Err(
                self.clone_error(scope, "An ArrayBuffer is detached and could not be cloned.")
            );
        }
        let store = buffer.get_backing_store();
        let Some(bytes) = store_bytes(&store) else {
            return Err(self.clone_error(scope, "An ArrayBuffer could not be cloned."));
        };
        let Ok(length) = u32::try_from(bytes.len()) else {
            return Err(self.clone_error(scope, "An ArrayBuffer is too long to clone."));
        };
        self.put_u8(TAG_ARRAY_BUFFER);
        self.put_u32(length);
        self.put_bytes(bytes);
        Ok(())
    }

    fn write_shared_array_buffer<'s>(
        &self,
        scope: &mut PinScope<'s, '_>,
        value: Local<'s, Value>,
    ) -> Result<(), Failed> {
        let Ok(buffer) = Local::<SharedArrayBuffer>::try_from(value) else {
            return Err(self.clone_error(scope, "A SharedArrayBuffer could not be cloned."));
        };
        // A shared buffer has no clone form: it is either transferred (the
        // delegate answers with an id for it) or not cloneable at all.
        match self.delegate.get_shared_array_buffer_id(scope, buffer) {
            Some(transfer_id) => {
                self.put_u8(TAG_SHARED_ARRAY_BUFFER);
                self.put_u32(transfer_id);
                Ok(())
            }
            None => Err(self.clone_error(
                scope,
                "A SharedArrayBuffer is not transferable and could not be cloned.",
            )),
        }
    }

    fn write_host_object<'s>(
        &self,
        scope: &mut PinScope<'s, '_>,
        object: Local<'s, Object>,
    ) -> Result<(), Failed> {
        self.put_u8(TAG_HOST_OBJECT);
        match self.delegate.write_host_object(scope, object, self) {
            Some(true) => Ok(()),
            Some(false) => Err(self.clone_error(scope, "An object could not be cloned.")),
            None => Err(Failed),
        }
    }
}

impl ValueSerializerHelper for ValueSerializerHeap<'_> {
    fn value_serializer_heap(&self) -> &ValueSerializerHeap<'_> {
        self
    }
}

/// A stack object over an owned and pinned [`ValueSerializerHeap`]
/// (`v8::ValueSerializer`). The `'a` is the lifetime of the delegate.
pub struct ValueSerializer<'a> {
    heap: Box<ValueSerializerHeap<'a>>,
}

impl<'a> ValueSerializer<'a> {
    pub fn new<'s, 'i, D: ValueSerializerImpl + 'a>(
        scope: &PinScope<'s, 'i>,
        delegate: Box<D>,
    ) -> Self {
        Self {
            heap: Box::new(ValueSerializerHeap::new(delegate, scope.get_isolate_ptr())),
        }
    }
}

impl ValueSerializer<'_> {
    /// The bytes written, taking them away (`v8::ValueSerializer::Release`).
    pub fn release(&self) -> Vec<u8> {
        std::mem::take(&mut self.heap.out.borrow_mut())
    }

    pub fn write_value(
        &self,
        context: Local<'_, Context>,
        value: Local<'_, Value>,
    ) -> Option<bool> {
        self.heap.serialize(context, value)
    }
}

/// The delegate a serializer calls back into: what a host object is, how to
/// write it, and what to do with a value that cannot be cloned.
pub trait ValueSerializerImpl {
    fn throw_data_clone_error<'s>(
        &self,
        scope: &mut PinScope<'s, '_>,
        message: Local<'s, V8String>,
    );

    fn has_custom_host_object(&self, _isolate: &Isolate) -> bool {
        false
    }

    fn is_host_object<'s>(
        &self,
        scope: &mut PinScope<'s, '_>,
        _object: Local<'s, Object>,
    ) -> Option<bool> {
        let message = V8String::new(scope, "Deno serializer: is_host_object not implemented")?;
        let exception = crate::Exception::error(scope, message);
        scope.throw_exception(exception);
        None
    }

    fn write_host_object<'s>(
        &self,
        scope: &mut PinScope<'s, '_>,
        _object: Local<'s, Object>,
        _value_serializer: &dyn ValueSerializerHelper,
    ) -> Option<bool> {
        let message = V8String::new(scope, "Deno serializer: write_host_object not implemented")?;
        let exception = crate::Exception::error(scope, message);
        scope.throw_exception(exception);
        None
    }

    fn get_shared_array_buffer_id<'s>(
        &self,
        _scope: &mut PinScope<'s, '_>,
        _shared_array_buffer: Local<'s, SharedArrayBuffer>,
    ) -> Option<u32> {
        None
    }

    fn get_wasm_module_transfer_id(
        &self,
        scope: &mut PinScope<'_, '_>,
        _module: Local<'_, crate::data::WasmModuleObject>,
    ) -> Option<u32> {
        let message = V8String::new(
            scope,
            "Deno serializer: get_wasm_module_transfer_id not implemented",
        )?;
        let exception = crate::Exception::error(scope, message);
        scope.throw_exception(exception);
        None
    }
}

/// What a delegate writes nested data with (`v8::ValueSerializerHelper`).
///
/// The crate we stand in for hangs these off an FFI object it calls
/// `get_cxx_value_serializer`; there is no C++ here, so the one required method
/// hands out the heap the methods write through.
pub trait ValueSerializerHelper {
    #[doc(hidden)]
    fn value_serializer_heap(&self) -> &ValueSerializerHeap<'_>;

    /// Start the stream: the tag and the format version
    /// (`v8::ValueSerializer::WriteHeader`).
    fn write_header(&self) {
        let heap = self.value_serializer_heap();
        heap.put_u8(HEADER_TAG);
        heap.put_u8(WIRE_FORMAT_VERSION);
    }

    fn write_value(&self, context: Local<Context>, value: Local<Value>) -> Option<bool> {
        self.value_serializer_heap().serialize(context, value)
    }

    fn write_uint32(&self, value: u32) {
        self.value_serializer_heap().put_u32(value);
    }

    fn write_uint64(&self, value: u64) {
        self.value_serializer_heap().put_u64(value);
    }

    fn write_double(&self, value: f64) {
        self.value_serializer_heap().put_f64(value);
    }

    fn write_raw_bytes(&self, source: &[u8]) {
        self.value_serializer_heap().put_bytes(source);
    }

    /// Register `array_buffer` under `transfer_id`, so the walk writes the id
    /// instead of the bytes (`v8::ValueSerializer::TransferArrayBuffer`).
    fn transfer_array_buffer(&self, transfer_id: u32, array_buffer: Local<ArrayBuffer>) {
        let Some(identity) = array_buffer
            .engine()
            .value()
            .as_object()
            .map(|object| object.id())
        else {
            return;
        };
        self.value_serializer_heap()
            .transfers
            .borrow_mut()
            .insert(identity, transfer_id);
    }

    fn set_treat_array_buffer_views_as_host_objects(&self, mode: bool) {
        self.value_serializer_heap()
            .treat_array_buffer_views_as_host_objects
            .set(mode);
    }
}

impl ValueSerializerHelper for ValueSerializer<'_> {
    fn value_serializer_heap(&self) -> &ValueSerializerHeap<'_> {
        &self.heap
    }
}

/// The deserializer's side of the API: the delegate, the bytes it reads from,
/// and the tables that give a reference back its identity.
pub struct ValueDeserializerHeap<'a> {
    delegate: Box<dyn ValueDeserializerImpl + 'a>,
    isolate: Isolate,
    /// A copy of the host's bytes. The crate we stand in for points into the
    /// host's buffer; owning them removes a lifetime and a dangling read, and
    /// a message is small enough that the copy does not matter.
    input: Vec<u8>,
    position: Cell<usize>,
    version: Cell<u32>,
    header_read: Cell<bool>,
    supports_legacy_wire_format: Cell<bool>,
    /// The id of each object read so far, in the order it was read. `None`
    /// while an object is being read, so a reference from inside it to itself
    /// finds the object that is already being built.
    memo: RefCell<Vec<Option<EngineValue>>>,
    /// Transfer id to the buffer the host registered for it.
    transfers: RefCell<HashMap<u32, EngineValue>>,
    /// Transfer id to a shared array buffer the host registered for it.
    shared_transfers: RefCell<HashMap<u32, EngineValue>>,
}

impl<'a> ValueDeserializerHeap<'a> {
    fn new(delegate: Box<dyn ValueDeserializerImpl + 'a>, isolate: Isolate, data: &[u8]) -> Self {
        Self {
            delegate,
            isolate,
            input: data.to_vec(),
            position: Cell::new(0),
            version: Cell::new(0),
            header_read: Cell::new(false),
            supports_legacy_wire_format: Cell::new(false),
            memo: RefCell::new(Vec::new()),
            transfers: RefCell::new(HashMap::new()),
            shared_transfers: RefCell::new(HashMap::new()),
        }
    }

    /// `length` bytes from the stream, and past them.
    fn take(&self, length: usize) -> Option<&[u8]> {
        let start = self.position.get();
        let end = start.checked_add(length)?;
        let bytes = self.input.get(start..end)?;
        self.position.set(end);
        Some(bytes)
    }

    fn take_u8(&self) -> Option<u8> {
        self.take(1)?.first().copied()
    }

    fn take_u32(&self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn take_u64(&self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn take_f64(&self) -> Option<f64> {
        Some(f64::from_bits(self.take_u64()?))
    }

    fn take_units(&self, count: usize) -> Option<Vec<u16>> {
        let mut units = Vec::new();
        for _ in 0..count {
            units.push(u16::from_le_bytes(self.take(2)?.try_into().ok()?));
        }
        Some(units)
    }

    /// Report a stream this bridge cannot read, and answer `None`.
    fn malformed<T>(&self, scope: &mut PinScope<'_, '_>, message: &str) -> Option<T> {
        if let Some(text) = V8String::new(scope, message) {
            let exception = crate::Exception::error(scope, text);
            scope.throw_exception(exception);
        }
        None
    }

    /// An id for an object about to be read, so that a reference to it from
    /// inside it resolves.
    fn reserve(&self) -> Option<u32> {
        let mut memo = self.memo.borrow_mut();
        let id = u32::try_from(memo.len()).ok()?;
        memo.push(None);
        Some(id)
    }

    fn fill(&self, id: u32, value: EngineValue) {
        let mut memo = self.memo.borrow_mut();
        if let Some(slot) = memo.get_mut(id as usize) {
            *slot = Some(value);
        }
    }

    /// The object an id was reserved for (`ReadObjectReference`).
    fn referenced(&self, scope: &mut PinScope<'_, '_>, id: u32) -> Option<EngineValue> {
        let value = self.memo.borrow().get(id as usize).copied().flatten();
        match value {
            Some(value) => Some(value),
            None => self.malformed(scope, "a reference to an object that was not read yet"),
        }
    }

    /// Read the stream's header (`v8::ValueDeserializer::ReadHeader`).
    ///
    /// `Some(true)` when the stream is this bridge's format (or was accepted as
    /// the version-0 shape), `Some(false)` when it names a format this bridge
    /// cannot read, and `None` when the bytes do not carry a header at all.
    /// Idempotent: a second call answers the first one's result.
    fn read_header_within(&self, scope: &mut PinScope<'_, '_>) -> Option<bool> {
        if self.header_read.get() {
            return Some(true);
        }
        let tag = self.take_u8()?;
        if tag != HEADER_TAG {
            // Anything else is the shape a version-0 stream had, which a host
            // can accept; this bridge never writes one, so reading a value from
            // it fails on the tag.
            if !self.supports_legacy_wire_format.get() {
                return self.malformed(scope, "the bytes are not a serialized value");
            }
            self.position.set(0);
            self.version.set(0);
            self.header_read.set(true);
            return Some(true);
        }
        let Some(version) = self.take_u8() else {
            return self.malformed(scope, "the header is truncated");
        };
        if version > WIRE_FORMAT_VERSION {
            return Some(false);
        }
        self.version.set(u32::from(version));
        self.header_read.set(true);
        Some(true)
    }

    /// [`read_header_within`](Self::read_header_within) in a scope of its own,
    /// which is what the helper trait and the first `read` both need.
    fn read_header_at(&self, context: Local<'_, Context>) -> Option<bool> {
        let mut isolate = self.isolate;
        let storage = unsafe { CallbackScope::new(&mut isolate) };
        let storage = pin!(storage);
        let mut plain = storage.init();
        let current = Local::new(&plain, &context);
        let mut entered = ContextScope::new(&mut plain, current);
        self.read_header_within(&mut entered)
    }

    fn read<'t>(&self, context: Local<'t, Context>) -> Option<Local<'t, Value>> {
        let mut isolate = self.isolate;
        // SAFETY: as `ValueSerializerHeap::serialize`.
        let storage = unsafe { CallbackScope::new(&mut isolate) };
        let storage = pin!(storage);
        let mut plain = storage.init();
        let current = Local::new(&plain, &context);
        let mut entered = ContextScope::new(&mut plain, current);
        let scope = &mut entered;
        // The crate we stand in for reads the header on the first value when
        // the host did not, which is what makes a bare `read_value` work.
        match self.read_header_within(scope) {
            Some(true) => {}
            Some(false) | None => return None,
        }
        let value = self.read_value_inner(scope)?;
        Some(local_of(value))
    }

    fn read_value_inner(&self, scope: &mut PinScope<'_, '_>) -> Option<EngineValue> {
        let tag = self.take_u8()?;
        self.read_with_tag(scope, tag)
    }

    fn read_with_tag(&self, scope: &mut PinScope<'_, '_>, tag: u8) -> Option<EngineValue> {
        match tag {
            TAG_UNDEFINED => Some(EngineValue::Undefined),
            TAG_NULL => Some(EngineValue::Null),
            TAG_TRUE => Some(EngineValue::Boolean(true)),
            TAG_FALSE => Some(EngineValue::Boolean(false)),
            TAG_NUMBER => Some(EngineValue::Number(self.take_f64()?)),
            TAG_STRING => self.read_string(scope),
            TAG_BIG_INT => self.read_big_int(scope),
            TAG_REFERENCE => {
                let id = self.take_u32()?;
                self.referenced(scope, id)
            }
            TAG_ARRAY => self.read_array(scope),
            TAG_OBJECT => self.read_object(scope),
            TAG_MAP => self.read_map(scope),
            TAG_SET => self.read_set(scope),
            TAG_DATE => self.read_date(scope),
            TAG_REG_EXP => self.read_reg_exp(scope),
            TAG_ARRAY_BUFFER => self.read_array_buffer(scope),
            TAG_TRANSFER_ARRAY_BUFFER => self.read_transferred_buffer(scope),
            TAG_SHARED_ARRAY_BUFFER => self.read_shared_array_buffer(scope),
            TAG_ARRAY_BUFFER_VIEW => self.read_view(scope),
            TAG_HOST_OBJECT => self.read_host_object(scope),
            TAG_HOLE => self.malformed(scope, "a hole outside an array"),
            _ => self.malformed(scope, "a tag this bridge does not know"),
        }
    }

    /// An array element: `Some(None)` for a hole, which is defined by not being
    /// defined.
    fn read_element(&self, scope: &mut PinScope<'_, '_>) -> Option<Option<EngineValue>> {
        let tag = self.take_u8()?;
        if tag == TAG_HOLE {
            return Some(None);
        }
        Some(self.read_with_tag(scope, tag))
    }

    fn read_string(&self, scope: &mut PinScope<'_, '_>) -> Option<EngineValue> {
        let length = self.take_u32()? as usize;
        let units = self.take_units(length)?;
        let text = V8String::new_from_two_byte(scope, &units, NewStringType::Normal)?;
        Some(engine_value(&Local::<Value>::from(text)))
    }

    fn read_big_int(&self, scope: &mut PinScope<'_, '_>) -> Option<EngineValue> {
        let negative = self.take_u8()? != 0;
        let count = self.take_u32()? as usize;
        let mut words = Vec::new();
        for _ in 0..count {
            words.push(self.take_u64()?);
        }
        let big = BigInt::new_from_words(scope, negative, &words)?;
        Some(engine_value(&Local::<Value>::from(big)))
    }

    fn read_array(&self, scope: &mut PinScope<'_, '_>) -> Option<EngineValue> {
        let length = self.take_u32()?;
        let id = self.reserve()?;
        let array = Array::new(scope, 0);
        let value = engine_value(&Local::<Value>::from(array));
        self.fill(id, value);
        // The length first, so the indices that follow fill a holey array
        // rather than being its only elements. A length is a plain data
        // property: it is neither enumerable nor configurable, so the
        // descriptor must not claim otherwise — the define would be refused and
        // the array would keep the length it was made with.
        let length_key = name_of(scope, "length")?;
        let length_value = Local::<Value>::from(Number::new(scope, f64::from(length)));
        let length_descriptor = PropertyDescriptor::new_from_value_writable(length_value, true);
        self.define(scope, &array, length_key, &length_descriptor)?;
        for index in 0..length {
            let Some(element) = self.read_element(scope)? else {
                continue;
            };
            let key = name_of(scope, &index.to_string())?;
            self.define(scope, &array, key, &data_property(local_of(element)))?;
        }
        Some(value)
    }

    fn read_object(&self, scope: &mut PinScope<'_, '_>) -> Option<EngineValue> {
        let count = self.take_u32()?;
        let id = self.reserve()?;
        let object = Object::new(scope);
        let value = engine_value(&Local::<Value>::from(object));
        self.fill(id, value);
        for _ in 0..count {
            let key = self.read_value_inner(scope)?;
            let entry = self.read_value_inner(scope)?;
            let name = match Local::<Name>::try_from(local_of(key)) {
                Ok(name) => name,
                Err(_) => return self.malformed(scope, "an object key that is not a string"),
            };
            self.define(scope, &object, name, &data_property(local_of(entry)))?;
        }
        Some(value)
    }

    fn read_map(&self, scope: &mut PinScope<'_, '_>) -> Option<EngineValue> {
        let count = self.take_u32()?;
        let id = self.reserve()?;
        let map = construct(scope, "%Map%", &[])?;
        self.fill(id, map);
        for _ in 0..count {
            let key = self.read_value_inner(scope)?;
            let entry = self.read_value_inner(scope)?;
            call_intrinsic(scope, "%Map.prototype.set%", map, &[key, entry])?;
        }
        Some(map)
    }

    fn read_set(&self, scope: &mut PinScope<'_, '_>) -> Option<EngineValue> {
        let count = self.take_u32()?;
        let id = self.reserve()?;
        let set = construct(scope, "%Set%", &[])?;
        self.fill(id, set);
        for _ in 0..count {
            let element = self.read_value_inner(scope)?;
            call_intrinsic(scope, "%Set.prototype.add%", set, &[element])?;
        }
        Some(set)
    }

    fn read_date(&self, scope: &mut PinScope<'_, '_>) -> Option<EngineValue> {
        let id = self.reserve()?;
        let time = self.take_f64()?;
        let value = construct(scope, "%Date%", &[EngineValue::Number(time)])?;
        self.fill(id, value);
        Some(value)
    }

    fn read_reg_exp(&self, scope: &mut PinScope<'_, '_>) -> Option<EngineValue> {
        let id = self.reserve()?;
        // The source and the flags are written as string values, so each
        // carries its own tag.
        let source = self.read_value_inner(scope)?;
        let flags = self.read_value_inner(scope)?;
        let value = construct(scope, "%RegExp%", &[source, flags])?;
        self.fill(id, value);
        Some(value)
    }

    fn read_array_buffer(&self, scope: &mut PinScope<'_, '_>) -> Option<EngineValue> {
        let id = self.reserve()?;
        let length = self.take_u32()? as usize;
        let bytes = self.take(length)?.to_vec();
        // SAFETY-free path: the store owns the bytes, and the buffer takes the
        // store's block, so nothing points at the Rust allocation afterwards.
        let store = SharedRef::new(BackingStore::from_bytes(&bytes));
        let buffer = ArrayBuffer::with_backing_store(scope, &store);
        let value = engine_value(&Local::<Value>::from(buffer));
        self.fill(id, value);
        Some(value)
    }

    fn read_transferred_buffer(&self, scope: &mut PinScope<'_, '_>) -> Option<EngineValue> {
        let id = self.reserve()?;
        let transfer_id = self.take_u32()?;
        let Some(value) = self.transfers.borrow().get(&transfer_id).copied() else {
            return self.malformed(scope, "an array buffer that was not transferred");
        };
        self.fill(id, value);
        Some(value)
    }

    fn read_shared_array_buffer(&self, scope: &mut PinScope<'_, '_>) -> Option<EngineValue> {
        let id = self.reserve()?;
        let transfer_id = self.take_u32()?;
        let registered = self.shared_transfers.borrow().get(&transfer_id).copied();
        let value = match registered {
            Some(value) => value,
            // The delegate owns the failure channel here: its default throws,
            // and a host that answers `None` without throwing gets a `None`
            // from `read_value`.
            None => {
                let buffer = self
                    .delegate
                    .get_shared_array_buffer_from_id(scope, transfer_id)?;
                engine_value(&Local::<Value>::from(buffer))
            }
        };
        self.fill(id, value);
        Some(value)
    }

    fn read_view(&self, scope: &mut PinScope<'_, '_>) -> Option<EngineValue> {
        let id = self.reserve()?;
        let kind = self.take_u8()?;
        let offset = f64::from(self.take_u32()?);
        let length = f64::from(self.take_u32()?);
        let buffer = self.read_value_inner(scope)?;
        let buffer = match Local::<ArrayBuffer>::try_from(local_of(buffer)) {
            Ok(buffer) => buffer,
            Err(_) => return self.malformed(scope, "a view whose buffer is not an ArrayBuffer"),
        };
        let name = if kind == VIEW_DATA_VIEW {
            "%DataView%"
        } else {
            element_constructor(kind)?
        };
        let args = [
            engine_value(&Local::<Value>::from(buffer)),
            EngineValue::Number(offset),
            EngineValue::Number(length),
        ];
        // A range that does not fit the buffer is what a truncated or tampered
        // stream looks like here, and the constructor throws for it.
        let value = construct(scope, name, &args)?;
        self.fill(id, value);
        Some(value)
    }

    fn read_host_object(&self, scope: &mut PinScope<'_, '_>) -> Option<EngineValue> {
        let id = self.reserve()?;
        let object = self.delegate.read_host_object(scope, self)?;
        let value = engine_value(&Local::<Value>::from(object));
        self.fill(id, value);
        Some(value)
    }

    /// Define an own property, or report why it could not be.
    fn define(
        &self,
        scope: &mut PinScope<'_, '_>,
        object: &Local<'_, Object>,
        key: Local<'_, Name>,
        descriptor: &PropertyDescriptor,
    ) -> Option<()> {
        match object.define_property(scope, key, descriptor) {
            Some(true) => Some(()),
            Some(false) => self.malformed(scope, "a property could not be defined"),
            None => None,
        }
    }
}

impl ValueDeserializerHelper for ValueDeserializerHeap<'_> {
    fn value_deserializer_heap(&self) -> &ValueDeserializerHeap<'_> {
        self
    }
}

/// A stack object over an owned [`ValueDeserializerHeap`]
/// (`v8::ValueDeserializer`).
pub struct ValueDeserializer<'a> {
    heap: Box<ValueDeserializerHeap<'a>>,
}

impl<'a> ValueDeserializer<'a> {
    pub fn new<'s, 'i, D: ValueDeserializerImpl + 'a>(
        scope: &PinScope<'s, 'i>,
        delegate: Box<D>,
        data: &[u8],
    ) -> Self {
        Self {
            heap: Box::new(ValueDeserializerHeap::new(
                delegate,
                scope.get_isolate_ptr(),
                data,
            )),
        }
    }
}

impl ValueDeserializer<'_> {
    /// Accept a stream that does not start with this format's header as the
    /// version-0 shape
    /// (`v8::ValueDeserializer::SetSupportsLegacyWireFormat`).
    pub fn set_supports_legacy_wire_format(&self, supports_legacy_wire_format: bool) {
        self.heap
            .supports_legacy_wire_format
            .set(supports_legacy_wire_format);
    }

    pub fn read_value<'t>(&self, context: Local<'t, Context>) -> Option<Local<'t, Value>> {
        self.heap.read(context)
    }
}

/// The delegate a deserializer calls back into: how to read a host object, and
/// where a transfer id's shared array buffer or wasm module is.
pub trait ValueDeserializerImpl {
    fn read_host_object<'s>(
        &self,
        scope: &mut PinScope<'s, '_>,
        _value_deserializer: &dyn ValueDeserializerHelper,
    ) -> Option<Local<'s, Object>> {
        let message = V8String::new(scope, "Deno deserializer: read_host_object not implemented")?;
        let exception = crate::Exception::error(scope, message);
        scope.throw_exception(exception);
        None
    }

    fn get_shared_array_buffer_from_id<'s>(
        &self,
        scope: &mut PinScope<'s, '_>,
        _transfer_id: u32,
    ) -> Option<Local<'s, SharedArrayBuffer>> {
        let message = V8String::new(
            scope,
            "Deno deserializer: get_shared_array_buffer_from_id not implemented",
        )?;
        let exception = crate::Exception::error(scope, message);
        scope.throw_exception(exception);
        None
    }

    fn get_wasm_module_from_id<'s>(
        &self,
        scope: &mut PinScope<'s, '_>,
        _clone_id: u32,
    ) -> Option<Local<'s, crate::data::WasmModuleObject>> {
        let message = V8String::new(
            scope,
            "Deno deserializer: get_wasm_module_from_id not implemented",
        )?;
        let exception = crate::Exception::error(scope, message);
        scope.throw_exception(exception);
        None
    }
}

/// What a delegate reads nested data with (`v8::ValueDeserializerHelper`).
pub trait ValueDeserializerHelper {
    #[doc(hidden)]
    fn value_deserializer_heap(&self) -> &ValueDeserializerHeap<'_>;

    fn read_header(&self, context: Local<Context>) -> Option<bool> {
        self.value_deserializer_heap().read_header_at(context)
    }

    fn read_value<'s>(&self, context: Local<'s, Context>) -> Option<Local<'s, Value>> {
        self.value_deserializer_heap().read(context)
    }

    fn read_uint32(&self, value: &mut u32) -> bool {
        match self.value_deserializer_heap().take_u32() {
            Some(read) => {
                *value = read;
                true
            }
            None => false,
        }
    }

    fn read_uint64(&self, value: &mut u64) -> bool {
        match self.value_deserializer_heap().take_u64() {
            Some(read) => {
                *value = read;
                true
            }
            None => false,
        }
    }

    fn read_double(&self, value: &mut f64) -> bool {
        match self.value_deserializer_heap().take_f64() {
            Some(read) => {
                *value = read;
                true
            }
            None => false,
        }
    }

    fn read_raw_bytes(&self, length: usize) -> Option<&[u8]> {
        self.value_deserializer_heap().take(length)
    }

    /// Register the buffer a transfer id in the stream names
    /// (`v8::ValueDeserializer::TransferArrayBuffer`).
    fn transfer_array_buffer(&self, transfer_id: u32, array_buffer: Local<ArrayBuffer>) {
        let value = engine_value(&Local::<Value>::from(array_buffer));
        self.value_deserializer_heap()
            .transfers
            .borrow_mut()
            .insert(transfer_id, value);
    }

    fn transfer_shared_array_buffer(
        &self,
        transfer_id: u32,
        shared_array_buffer: Local<SharedArrayBuffer>,
    ) {
        let value = engine_value(&Local::<Value>::from(shared_array_buffer));
        self.value_deserializer_heap()
            .shared_transfers
            .borrow_mut()
            .insert(transfer_id, value);
    }

    fn get_wire_format_version(&self) -> u32 {
        self.value_deserializer_heap().version.get()
    }
}

impl ValueDeserializerHelper for ValueDeserializer<'_> {
    fn value_deserializer_heap(&self) -> &ValueDeserializerHeap<'_> {
        &self.heap
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{bind, eval, eval_number, in_context};
    use std::rc::Rc;

    /// The marker a host object's payload starts with, so the read side can
    /// tell it is reading what the write side wrote.
    const HOST_MARKER: u32 = 42;

    /// A delegate with no host objects: it records what it was asked to
    /// refuse, and reports a refusal the way a host does — by throwing.
    #[derive(Default)]
    struct RefusingDelegate {
        clone_errors: Rc<RefCell<Vec<String>>>,
    }

    impl RefusingDelegate {
        fn new() -> (Self, Rc<RefCell<Vec<String>>>) {
            let clone_errors = Rc::new(RefCell::new(Vec::new()));
            let delegate = Self {
                clone_errors: Rc::clone(&clone_errors),
            };
            (delegate, clone_errors)
        }
    }

    impl ValueSerializerImpl for RefusingDelegate {
        fn throw_data_clone_error<'s>(
            &self,
            scope: &mut PinScope<'s, '_>,
            message: Local<'s, V8String>,
        ) {
            self.clone_errors
                .borrow_mut()
                .push(message.to_rust_string_lossy(scope));
            let exception = crate::Exception::error(scope, message);
            scope.throw_exception(exception);
        }
    }

    impl ValueDeserializerImpl for RefusingDelegate {}

    /// A delegate whose host objects are the objects carrying an own `host`
    /// property: it writes their `data` property as the payload, and reads a
    /// fresh `{ data }` object back.
    #[derive(Default)]
    struct HostDelegate {
        clone_errors: Rc<RefCell<Vec<String>>>,
    }

    impl HostDelegate {
        fn new() -> (Self, Rc<RefCell<Vec<String>>>) {
            let clone_errors = Rc::new(RefCell::new(Vec::new()));
            let delegate = Self {
                clone_errors: Rc::clone(&clone_errors),
            };
            (delegate, clone_errors)
        }
    }

    impl ValueSerializerImpl for HostDelegate {
        fn throw_data_clone_error<'s>(
            &self,
            scope: &mut PinScope<'s, '_>,
            message: Local<'s, V8String>,
        ) {
            self.clone_errors
                .borrow_mut()
                .push(message.to_rust_string_lossy(scope));
            let exception = crate::Exception::error(scope, message);
            scope.throw_exception(exception);
        }

        fn has_custom_host_object(&self, _isolate: &Isolate) -> bool {
            true
        }

        fn is_host_object<'s>(
            &self,
            scope: &mut PinScope<'s, '_>,
            object: Local<'s, Object>,
        ) -> Option<bool> {
            object.has_own_property(scope, string_value(scope, "host")?)
        }

        fn write_host_object<'s>(
            &self,
            scope: &mut PinScope<'s, '_>,
            object: Local<'s, Object>,
            value_serializer: &dyn ValueSerializerHelper,
        ) -> Option<bool> {
            value_serializer.write_uint32(HOST_MARKER);
            let data = object.get(scope, string_value(scope, "data")?)?;
            value_serializer.write_value(scope.get_current_context(), data)?;
            Some(true)
        }
    }

    impl ValueDeserializerImpl for HostDelegate {
        fn read_host_object<'s>(
            &self,
            scope: &mut PinScope<'s, '_>,
            value_deserializer: &dyn ValueDeserializerHelper,
        ) -> Option<Local<'s, Object>> {
            let mut marker = 0;
            assert!(value_deserializer.read_uint32(&mut marker));
            assert_eq!(marker, HOST_MARKER);
            let data = value_deserializer.read_value(scope.get_current_context())?;
            let object = Object::new(scope);
            let key = name_of(scope, "data")?;
            match object.define_property(scope, key, &data_property(data)) {
                Some(true) => Some(object),
                Some(false) | None => None,
            }
        }
    }

    /// A value through both ends of the round trip.
    fn round_trip<'s>(scope: &PinScope<'s, '_>, value: Local<'s, Value>) -> Local<'s, Value> {
        let context = scope.get_current_context();
        let serializer = ValueSerializer::new(scope, Box::new(RefusingDelegate::new().0));
        serializer.write_header();
        assert_eq!(serializer.write_value(context, value), Some(true));
        let bytes = serializer.release();

        let deserializer =
            ValueDeserializer::new(scope, Box::new(RefusingDelegate::new().0), &bytes);
        assert_eq!(deserializer.read_header(context), Some(true));
        let value = deserializer.read_value(context).expect("a value");
        Local::from_engine(value.into_engine())
    }

    /// `Object.is` between a value and its copy. `Object.is` rather than `===`
    /// so that `-0` and every NaN are distinguished.
    fn same_value(source: &str) -> bool {
        let same;
        in_context!(scope, {
            let original = eval(scope, source);
            let copy = round_trip(scope, original);
            bind(scope, "original", original);
            bind(scope, "copy", copy);
            same = eval(scope, "Object.is(copy, original)").is_true();
        });
        same
    }

    #[test]
    fn the_primitives_round_trip() {
        for source in [
            "undefined",
            "null",
            "true",
            "false",
            "0",
            "-0",
            "1.5",
            "-1.5",
            "NaN",
            "Infinity",
            "-Infinity",
            "1e308",
            "5e-324",
        ] {
            assert!(same_value(source), "{source} did not survive");
        }
    }

    #[test]
    fn strings_round_trip_as_code_units() {
        for source in ["''", "'hello'", r"'\uD800'", r"'a\uDC00b'", r"'\u{1F600}'"] {
            assert!(same_value(source), "{source} did not survive");
        }
        in_context!(scope, {
            // A lone surrogate is one code unit, not a replacement character.
            let copy = round_trip(scope, eval(scope, r"'\uD800'"));
            bind(scope, "copy", copy);
            assert_eq!(eval_number(scope, "copy.length"), 1.0);
            assert_eq!(eval_number(scope, "copy.charCodeAt(0)"), 0xD800 as f64);
        });
    }

    #[test]
    fn big_ints_round_trip() {
        for source in [
            "0n",
            "1n",
            "-1n",
            "2n ** 200n",
            "-(2n ** 200n)",
            "12345678901234567890123n",
            "-(2n ** 64n)",
        ] {
            assert!(same_value(source), "{source} did not survive");
        }
    }

    #[test]
    fn an_array_keeps_its_holes() {
        in_context!(scope, {
            let copy = round_trip(scope, eval(scope, "[, 1, , 2]"));
            bind(scope, "copy", copy);
            assert_eq!(eval_number(scope, "copy.length"), 4.0);
            assert!(eval(scope, "1 in copy").is_true());
            assert!(eval(scope, "3 in copy").is_true());
            assert!(eval(scope, "0 in copy").is_false());
            assert!(eval(scope, "2 in copy").is_false());
            assert_eq!(eval_number(scope, "copy[3]"), 2.0);
        });
    }

    #[test]
    fn an_object_round_trips_by_its_own_enumerable_string_keys() {
        in_context!(scope, {
            let copy = round_trip(
                scope,
                eval(
                    scope,
                    "(() => { const o = { a: 1, b: { c: [2] }, d: undefined }; \
                     Object.defineProperty(o, 'hidden', { value: 3, enumerable: false }); \
                     return o })()",
                ),
            );
            bind(scope, "copy", copy);
            assert_eq!(eval_number(scope, "copy.a"), 1.0);
            assert_eq!(eval_number(scope, "copy.b.c[0]"), 2.0);
            assert!(eval(scope, "'d' in copy").is_true());
            assert!(eval(scope, "copy.d === undefined").is_true());
            assert!(eval(scope, "Object.keys(copy).join() === 'a,b,d'").is_true());
            assert!(eval(scope, "!Object.is(Object.getPrototypeOf(copy), null)").is_true());
        });
    }

    #[test]
    fn an_own_proto_key_is_defined_rather_than_set() {
        in_context!(scope, {
            // `__proto__` is an accessor on `Object.prototype`: a `[[Set]]`
            // would change the prototype instead of creating the property the
            // stream names.
            let copy = round_trip(
                scope,
                eval(
                    scope,
                    "(() => { const o = {}; Object.defineProperty(o, '__proto__', \
                     { value: 4, enumerable: true, writable: true, configurable: true }); \
                     return o })()",
                ),
            );
            bind(scope, "copy", copy);
            assert!(eval(scope, "Object.getPrototypeOf(copy) === Object.prototype").is_true());
            assert_eq!(
                eval_number(
                    scope,
                    "Object.getOwnPropertyDescriptor(copy, '__proto__').value"
                ),
                4.0
            );
        });
    }

    #[test]
    fn keyed_collections_round_trip() {
        in_context!(scope, {
            let copy = round_trip(scope, eval(scope, "new Map([['a', 1], ['b', { c: 2 }]])"));
            bind(scope, "copy", copy);
            assert!(eval(scope, "copy instanceof Map").is_true());
            assert_eq!(eval_number(scope, "copy.size"), 2.0);
            assert_eq!(eval_number(scope, "copy.get('a')"), 1.0);
            assert_eq!(eval_number(scope, "copy.get('b').c"), 2.0);
        });
        in_context!(scope, {
            let copy = round_trip(scope, eval(scope, "new Set([1, 2, 3])"));
            bind(scope, "copy", copy);
            assert!(eval(scope, "copy instanceof Set").is_true());
            assert_eq!(eval_number(scope, "copy.size"), 3.0);
            assert!(eval(scope, "copy.has(2)").is_true());
        });
        in_context!(scope, {
            // A map key whose identity matters: the same object comes back.
            let copy = round_trip(
                scope,
                eval(
                    scope,
                    "(() => { const k = {}; return new Map([[k, k]]) })()",
                ),
            );
            bind(scope, "copy", copy);
            assert!(
                eval(
                    scope,
                    "(() => { const k = copy.keys().next().value; return copy.get(k) === k })()"
                )
                .is_true()
            );
        });
    }

    #[test]
    fn a_date_keeps_its_time_value() {
        in_context!(scope, {
            let copy = round_trip(scope, eval(scope, "new Date(1234567890123)"));
            bind(scope, "copy", copy);
            assert!(eval(scope, "copy instanceof Date").is_true());
            assert_eq!(eval_number(scope, "copy.getTime()"), 1234567890123.0);
        });
        in_context!(scope, {
            let copy = round_trip(scope, eval(scope, "new Date(NaN)"));
            bind(scope, "copy", copy);
            assert!(eval(scope, "Number.isNaN(copy.getTime())").is_true());
        });
    }

    #[test]
    fn a_reg_exp_keeps_its_source_and_flags() {
        in_context!(scope, {
            let copy = round_trip(scope, eval(scope, "/ab+c/gi"));
            bind(scope, "copy", copy);
            assert!(eval(scope, "copy instanceof RegExp").is_true());
            assert!(eval(scope, "copy.source === 'ab+c'").is_true());
            assert!(eval(scope, "copy.flags === 'gi'").is_true());
            assert!(eval(scope, "copy.test('ABBBC')").is_true());
        });
    }

    #[test]
    fn a_buffer_and_its_views_round_trip_together() {
        in_context!(scope, {
            let copy = round_trip(
                scope,
                eval(
                    scope,
                    "(() => { const b = new ArrayBuffer(4); \
                     new Uint8Array(b).set([1, 2, 3, 4]); \
                     return [b, new Uint8Array(b)] })()",
                ),
            );
            bind(scope, "copy", copy);
            assert!(eval(scope, "copy[0] instanceof ArrayBuffer").is_true());
            assert_eq!(eval_number(scope, "copy[0].byteLength"), 4.0);
            assert!(eval(scope, "copy[1] instanceof Uint8Array").is_true());
            assert_eq!(eval_number(scope, "copy[1].length"), 4.0);
            assert_eq!(eval_number(scope, "copy[1][3]"), 4.0);
            // One buffer, not two: the byte stream named it once.
            assert!(eval(scope, "copy[1].buffer === copy[0]").is_true());
        });
        in_context!(scope, {
            let copy = round_trip(scope, eval(scope, "new Float64Array([1.5, -2.5])"));
            bind(scope, "copy", copy);
            assert!(eval(scope, "copy instanceof Float64Array").is_true());
            assert_eq!(eval_number(scope, "copy.length"), 2.0);
            assert_eq!(eval_number(scope, "copy[0]"), 1.5);
            assert_eq!(eval_number(scope, "copy[1]"), -2.5);
        });
        in_context!(scope, {
            let copy = round_trip(scope, eval(scope, "new DataView(new ArrayBuffer(8), 2, 4)"));
            bind(scope, "copy", copy);
            assert!(eval(scope, "copy instanceof DataView").is_true());
            assert_eq!(eval_number(scope, "copy.byteOffset"), 2.0);
            assert_eq!(eval_number(scope, "copy.byteLength"), 4.0);
        });
    }

    #[test]
    fn a_cycle_and_a_shared_reference_survive() {
        in_context!(scope, {
            let copy = round_trip(
                scope,
                eval(scope, "(() => { const o = {}; o.self = o; return o })()"),
            );
            bind(scope, "copy", copy);
            assert!(eval(scope, "copy.self === copy").is_true());
        });
        in_context!(scope, {
            let copy = round_trip(
                scope,
                eval(scope, "(() => { const o = {}; return [o, o] })()"),
            );
            bind(scope, "copy", copy);
            assert!(eval(scope, "copy[0] === copy[1]").is_true());
        });
        in_context!(scope, {
            // A cycle through a keyed collection, which is the case a reader
            // that creates the collection after its contents cannot do.
            let copy = round_trip(
                scope,
                eval(
                    scope,
                    "(() => { const m = new Map(); m.set('self', m); return m })()",
                ),
            );
            bind(scope, "copy", copy);
            assert!(eval(scope, "copy.get('self') === copy").is_true());
        });
    }

    #[test]
    fn a_host_object_goes_through_the_delegates_hooks() {
        in_context!(scope, {
            let original = eval(scope, "({ host: true, data: { n: 5 } })");
            let context = scope.get_current_context();
            let (delegate, errors) = HostDelegate::new();
            let serializer = ValueSerializer::new(scope, Box::new(delegate));
            serializer.write_header();
            assert_eq!(serializer.write_value(context, original), Some(true));
            let bytes = serializer.release();
            assert!(errors.borrow().is_empty());

            let deserializer =
                ValueDeserializer::new(scope, Box::new(HostDelegate::new().0), &bytes);
            assert_eq!(deserializer.read_header(context), Some(true));
            let copy = deserializer.read_value(context).expect("a value");
            bind(scope, "copy", Local::from_engine(copy.into_engine()));
            // The payload came back through the delegate's own read hook, not
            // from the object's keys, which is what the hook is for.
            assert_eq!(eval_number(scope, "copy.data.n"), 5.0);
            assert!(eval(scope, "!('host' in copy)").is_true());
        });
    }

    #[test]
    fn a_value_that_cannot_be_cloned_is_reported() {
        for source in [
            "(function () {})",
            "(() => {})",
            "Symbol('s')",
            "Promise.resolve()",
            "new WeakMap()",
            "new WeakSet()",
            "new Map()[Symbol.iterator]()",
            "(function* () {})()",
            "new Error('x')",
            "new Number(5)",
            "(() => { const o = {}; return new Proxy(o, {}) })()",
            "(() => { const b = new ArrayBuffer(4); const o = { b }; b.transfer(); return o })()",
        ] {
            in_context!(scope, {
                let (delegate, errors) = RefusingDelegate::new();
                let serializer = ValueSerializer::new(scope, Box::new(delegate));
                serializer.write_header();
                let value = eval(scope, source);
                assert_eq!(
                    serializer.write_value(scope.get_current_context(), value),
                    None,
                    "{source} was written"
                );
                let errors = errors.borrow();
                assert_eq!(errors.len(), 1, "{source} reported {errors:?}");
                assert!(errors.first().is_some_and(|message| !message.is_empty()));
            });
        }
    }

    #[test]
    fn a_header_is_required_and_versioned() {
        in_context!(scope, {
            let context = scope.get_current_context();

            let deserializer =
                ValueDeserializer::new(scope, Box::new(RefusingDelegate::new().0), &[]);
            assert_eq!(deserializer.read_header(context), None);

            let newer = [HEADER_TAG, WIRE_FORMAT_VERSION + 1];
            let deserializer =
                ValueDeserializer::new(scope, Box::new(RefusingDelegate::new().0), &newer);
            assert_eq!(deserializer.read_header(context), Some(false));

            // A stream that does not start with the tag is not this format, and
            // is read as the version-0 shape only when a host asks for it.
            let deserializer =
                ValueDeserializer::new(scope, Box::new(RefusingDelegate::new().0), &[0x7F]);
            assert_eq!(deserializer.read_header(context), None);

            let legacy =
                ValueDeserializer::new(scope, Box::new(RefusingDelegate::new().0), &[0x7F]);
            legacy.set_supports_legacy_wire_format(true);
            assert_eq!(legacy.read_header(context), Some(true));
            assert_eq!(legacy.get_wire_format_version(), 0);
            // This bridge writes no version-0 stream, so the tag is unknown.
            assert!(legacy.read_value(context).is_none());
        });
    }

    #[test]
    fn a_truncated_value_is_not_a_value() {
        in_context!(scope, {
            let context = scope.get_current_context();
            // An array whose element count promises eleven elements that are
            // not there.
            let bytes = [HEADER_TAG, WIRE_FORMAT_VERSION, TAG_ARRAY, 11, 0, 0, 0];
            let deserializer =
                ValueDeserializer::new(scope, Box::new(RefusingDelegate::new().0), &bytes);
            assert_eq!(deserializer.read_header(context), Some(true));
            assert!(deserializer.read_value(context).is_none());
        });
    }

    #[test]
    fn a_number_is_the_f64_bits_it_started_as() {
        in_context!(scope, {
            // The wire form of a number is its `f64`, so a NaN is the same NaN
            // and a negative zero is still negative.
            let number = crate::data::Number::new(scope, -0.0);
            let copy = round_trip(scope, Local::<Value>::from(number));
            bind(scope, "copy", copy);
            assert!(eval(scope, "Object.is(copy, -0)").is_true());
        });
    }

    #[test]
    fn an_unread_header_is_read_by_the_first_value() {
        in_context!(scope, {
            let context = scope.get_current_context();
            let serializer = ValueSerializer::new(scope, Box::new(RefusingDelegate::new().0));
            serializer.write_header();
            let value = Local::<Value>::from(crate::data::Number::new(scope, 7.0));
            assert_eq!(serializer.write_value(context, value), Some(true));
            let bytes = serializer.release();

            let deserializer =
                ValueDeserializer::new(scope, Box::new(RefusingDelegate::new().0), &bytes);
            let copy = deserializer.read_value(context).expect("a value");
            let copy: Local<'_, Value> = Local::from_engine(copy.into_engine());
            let copy = Local::<Number>::try_from(copy).expect("number");
            assert_eq!(copy.value(), 7.0);
        });
    }
}
