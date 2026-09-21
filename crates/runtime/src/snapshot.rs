//! The snapshot format: the values a host attached to a context, written to
//! bytes and read back.
//!
//! A host that embeds this engine (Deno's `deno_core` is the reference) builds
//! its realm once, at build time, and wants to skip that work at startup. What
//! it can skip is the part it did itself — the objects, arrays, strings and
//! symbols it installed — because the realm around them is rebuilt
//! deterministically by `Context::new` on every boot. So the format carries a
//! *value graph rooted at what the host chose*, not a heap image: a walk from
//! the root, every value written once and referred to by serial.
//!
//! Two things make the graph small enough to be worth writing:
//!
//! - **A builtin is written by name, not by structure.** A reference to an
//!   intrinsic (`%Object.prototype%`, `%Array%`, ...) is a name; the walk does
//!   not descend into it, and the restore resolves the name in the realm it is
//!   rebuilding. Without this the first prototype would drag in every builtin
//!   the realm has.
//! - **A value is written once.** Identity is carried by serial, so a cycle
//!   round-trips as a cycle and two references to one object come back as one
//!   object — which is what a host's own bookkeeping depends on.
//!
//! What it does not carry yet ends the walk with [`Unsupported`], which names
//! the value it refused: a function body, a proxy, a typed array, a module
//! namespace, a host object, an `External`, an array with a hole, an indexed
//! accessor, an array whose `length` is not an index. Each entry is a subsystem
//! to carry, and the walk refuses rather than writing something a restore would
//! read back wrong.
//!
//! Version 1. The format is ours to version, and its compatibility surface is
//! the *names* it writes: an intrinsic name and a well-known symbol name have to
//! mean the same thing in the tree that reads a blob as in the tree that wrote
//! it. A blob therefore carries the format version and the word size it was
//! written with, and a reader refuses any other.

use std::collections::HashMap;

use crux::bigint::{self, BigInt};
use crux::handle::Handle;
use crux::heap::Pin;
use crux::object::{JsObject, ObjectKind, Property, PropertyKind};
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::{self, JsString};
use crux::symbol::{self, Symbol};
use crux::value::{Value, ValueKind};

use crate::agent::Agent;
use crate::realm::Realm;

/// The blob's first eight bytes, and its last eight: a blob that lost its head
/// or its tail is not one.
pub const MAGIC: &[u8; 8] = b"SLAGSNP\0";

/// The format this tree writes and reads.
pub const FORMAT_VERSION: u32 = 1;

/// magic (8) + version (4) + word size (1) + endianness (1) + reserved (2) +
/// body length (4) + root serial (4).
const HEADER_LEN: usize = 24;

const ENDIAN_LITTLE: u8 = 1;

const REC_UNDEFINED: u8 = 1;
const REC_NULL: u8 = 2;
const REC_BOOLEAN: u8 = 3;
const REC_NUMBER: u8 = 4;
const REC_STRING: u8 = 5;
const REC_BIGINT: u8 = 6;
const REC_SYMBOL_WELL_KNOWN: u8 = 7;
const REC_SYMBOL_REGISTRY: u8 = 8;
const REC_SYMBOL: u8 = 9;
const REC_INTRINSIC: u8 = 10;
const REC_OBJECT: u8 = 11;
const REC_ARRAY: u8 = 12;

const FLAG_ENUMERABLE: u8 = 1;
const FLAG_CONFIGURABLE: u8 = 2;
const FLAG_WRITABLE: u8 = 4;
const FLAG_ACCESSOR: u8 = 8;

/// A serial that names no record: a null prototype, an absent accessor.
const NO_REF: u32 = u32::MAX;

/// The radix bigints are written in: exact, and the sign is part of the text.
const BIGINT_RADIX: u32 = 16;

/// `length`, spelled as code units: the one own key of an Array that is neither
/// an element nor an ordinary property.
const LENGTH_UNITS: &[u16] = &[108, 101, 110, 103, 116, 104];

/// Why a walk gave up.
///
/// The fields are the engine's own names, because the report is read by whoever
/// has to grow the format: "a function" says which subsystem is missing, where
/// "unsupported value" says nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unsupported {
    pub type_name: &'static str,
    pub detail: &'static str,
}

impl Unsupported {
    const fn new(type_name: &'static str, detail: &'static str) -> Self {
        Self { type_name, detail }
    }
}

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.type_name, self.detail)
    }
}

impl std::error::Error for Unsupported {}

/// Why a blob was not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The magic is not this format's, or the trailer does not repeat it.
    NotASnapshot,
    /// The blob was written by another version of the format.
    Version { found: u32, expected: u32 },
    /// The blob was written by a build of another pointer width.
    WordSize { found: u8, expected: u8 },
    /// The blob was written on a host of the other byte order.
    Endianness { found: u8, expected: u8 },
    /// The declared body length is not the number of bytes that follow.
    Length { found: usize, expected: usize },
    /// A field ends past the end of the blob.
    Truncated,
    /// A record tag this version does not write.
    BadTag(u8),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotASnapshot => write!(f, "not a Slag snapshot"),
            Self::Version { found, expected } => write!(
                f,
                "snapshot format version {found}, this engine reads {expected}"
            ),
            Self::WordSize { found, expected } => write!(
                f,
                "snapshot written with a {found}-byte word, this engine has {expected}"
            ),
            Self::Endianness { found, expected } => write!(
                f,
                "snapshot written with endianness {found}, this engine writes {expected}"
            ),
            Self::Length { found, expected } => write!(
                f,
                "snapshot declares {found} bytes of body, the blob has {expected}"
            ),
            Self::Truncated => write!(f, "snapshot ends in the middle of a field"),
            Self::BadTag(tag) => write!(f, "snapshot record tag {tag} is not one of this format's"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// How one value is identified while the walk is in progress.
///
/// Only identity-bearing values *need* an entry — two equal strings are
/// interchangeable in the language — but every value gets one, because a
/// property's value is written as the serial of a record rather than as a value
/// in place: the serial is the only way a graph refers to itself.
#[derive(PartialEq, Eq, Hash)]
enum Identity {
    Intrinsic(String),
    Object(u64),
    Symbol(u64),
    String(Vec<u16>),
    BigInt(String),
    Primitive(u64),
}

/// The identity a value is written once under.
fn identity(realm: &Handle<Realm>, value: Value) -> Identity {
    if let Some(name) = realm.intrinsics.name_of_value(&value) {
        return Identity::Intrinsic(name.to_string());
    }
    match value.kind() {
        ValueKind::Object(object) => Identity::Object(object.id()),
        ValueKind::Function(function) => Identity::Object(function.id()),
        ValueKind::Symbol(symbol) => Identity::Symbol(symbol.id),
        ValueKind::String(text) => Identity::String(text.as_slice().to_vec()),
        ValueKind::BigInt(number) => Identity::BigInt(bigint::to_string(&number, BIGINT_RADIX)),
        // A number's bits are canonical (a NaN is stored as one NaN), so equal
        // numbers share one record.
        _ => Identity::Primitive(value.bits()),
    }
}

/// One record as read from a blob, before any engine value exists for it.
enum Record {
    Undefined,
    Null,
    Boolean(bool),
    Number(f64),
    String(Vec<u16>),
    BigInt(String),
    SymbolWellKnown(u16),
    SymbolRegistry(Vec<u16>),
    Symbol(Option<Vec<u16>>),
    Intrinsic(String),
    Object {
        proto: u32,
        extensible: bool,
        properties: Vec<StoredProperty>,
    },
    Array {
        proto: u32,
        extensible: bool,
        length: u32,
        elements: Vec<u32>,
        extras: Vec<StoredProperty>,
    },
}

/// A property as read from a blob: the key, its attributes, and either one value
/// (data) or a getter and a setter (accessor), each a serial or absent.
struct StoredProperty {
    key: u32,
    flags: u8,
    first: u32,
    second: u32,
}

/// Write `root` and everything it reaches as a snapshot blob.
///
/// `agent` and `realm` are what the walk asks its two identity questions of: a
/// realm names the intrinsics it rebuilt, and the agent holds the `Symbol.for`
/// registry, whose symbols have an identity beyond one blob.
pub fn encode(agent: &Agent, realm: &Handle<Realm>, root: Value) -> Result<Vec<u8>, Unsupported> {
    let mut objects: Vec<Value> = Vec::new();
    let mut serials: HashMap<Identity, u32> = HashMap::new();
    let root_serial = visit(realm, root, &mut objects, &mut serials)?;

    let mut body = Vec::new();
    write_u32(&mut body, objects.len() as u32);
    for value in &objects {
        write_record(agent, realm, *value, &serials, &mut body)?;
    }

    let mut blob = Vec::with_capacity(HEADER_LEN + body.len() + MAGIC.len());
    blob.extend_from_slice(MAGIC);
    write_u32(&mut blob, FORMAT_VERSION);
    blob.push(std::mem::size_of::<usize>() as u8);
    blob.push(ENDIAN_LITTLE);
    blob.extend_from_slice(&[0, 0]);
    write_u32(&mut blob, body.len() as u32);
    write_u32(&mut blob, root_serial);
    blob.extend_from_slice(&body);
    blob.extend_from_slice(MAGIC);
    Ok(blob)
}

/// Write the data a host attached to each context slot.
///
/// The slots are the blob's context table, in V8's convention: slot 0 is the
/// default context, and the ones after it are the contexts added after it. The
/// table is built here rather than by the caller because it belongs to the
/// realm the blob is taken from: an array made through an isolate's *current*
/// realm would carry that realm's `%Array.prototype%` even when the context
/// being written is another one, and the walk below would then descend into the
/// wrong realm's builtins.
pub fn encode_slots(
    agent: &Agent,
    realm: &Handle<Realm>,
    slots: &[Vec<Value>],
) -> Result<Vec<u8>, Unsupported> {
    let prototype = realm
        .intrinsics
        .array_prototype()
        .and_then(|value| value.as_object());
    let root = JsObject::array_create(prototype, slots.len() as f64)
        .map_err(|_| Unsupported::new("the context table", "an array could not be made"))?;
    // The root is pinned while the table is filled: the inner arrays are alive
    // as handles on the stack, but the root is reached only through this local,
    // and a `--gc-stress` collection inside the loop would sweep it otherwise.
    let _root_pin = crux::heap::pin_handle(root);
    for (index, slot) in slots.iter().enumerate() {
        let items = JsObject::array_create(prototype, slot.len() as f64)
            .map_err(|_| Unsupported::new("the context table", "an item list could not be made"))?;
        for (position, value) in slot.iter().enumerate() {
            items
                .create_data_property_index(position as u64, *value)
                .map_err(|_| {
                    Unsupported::new("the context table", "an item could not be listed")
                })?;
        }
        root.create_data_property_index(index as u64, Value::Object(items))
            .map_err(|_| Unsupported::new("the context table", "a slot could not be listed"))?;
    }
    encode(agent, realm, Value::Object(root))
}

/// Read a blob, answering the value it was rooted at.
///
/// The values are made in `realm`, which is what makes an intrinsic reference
/// meaningful: the name a blob carries resolves against the realm that is being
/// restored into, not the realm that wrote it.
pub fn decode(agent: &Agent, realm: &Handle<Realm>, bytes: &[u8]) -> Result<Value, DecodeError> {
    let (body_len, root) = header(bytes)?;

    let mut body = Reader::new(&bytes[HEADER_LEN..HEADER_LEN + body_len]);
    let count = body.u32().ok_or(DecodeError::Truncated)? as usize;
    // One record is at least one byte, so a count larger than the body is a
    // blob that was cut short rather than a graph with many empty records.
    if count > body.bytes.len() {
        return Err(DecodeError::Truncated);
    }
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        records.push(read_record(&mut body)?);
    }

    let mut builder = Builder {
        agent,
        realm,
        records: &records,
        made: vec![None; records.len()],
        pins: Vec::new(),
    };
    builder.materialize(root).ok_or(DecodeError::Truncated)
}

/// Whether `bytes` is a blob of this format, this version and this build's
/// shape: the question a host asks before it hands a blob to an isolate, since
/// a blob that is not one has to boot from source instead.
///
/// The answer is the header's, not the graph's: a blob can pass this and still
/// fail to read, which is the same statement the crate we stand in for makes
/// with `StartupData::IsValid`.
pub fn is_valid(bytes: &[u8]) -> bool {
    header(bytes).is_ok()
}

/// Read and check the header, answering the body's length and the root's
/// serial.
fn header(bytes: &[u8]) -> Result<(usize, u32), DecodeError> {
    let mut reader = Reader::new(bytes);
    if reader.take(MAGIC.len()).ok_or(DecodeError::Truncated)? != MAGIC {
        return Err(DecodeError::NotASnapshot);
    }
    let version = reader.u32().ok_or(DecodeError::Truncated)?;
    if version != FORMAT_VERSION {
        return Err(DecodeError::Version {
            found: version,
            expected: FORMAT_VERSION,
        });
    }
    let word_size = reader.u8().ok_or(DecodeError::Truncated)?;
    let expected_word = std::mem::size_of::<usize>() as u8;
    if word_size != expected_word {
        return Err(DecodeError::WordSize {
            found: word_size,
            expected: expected_word,
        });
    }
    let endianness = reader.u8().ok_or(DecodeError::Truncated)?;
    if endianness != ENDIAN_LITTLE {
        return Err(DecodeError::Endianness {
            found: endianness,
            expected: ENDIAN_LITTLE,
        });
    }
    reader.take(2).ok_or(DecodeError::Truncated)?;
    let body_len = reader.u32().ok_or(DecodeError::Truncated)? as usize;
    let root = reader.u32().ok_or(DecodeError::Truncated)?;

    let available = bytes.len().saturating_sub(HEADER_LEN + MAGIC.len());
    if body_len != available {
        return Err(DecodeError::Length {
            found: body_len,
            expected: available,
        });
    }
    if bytes[bytes.len() - MAGIC.len()..] != *MAGIC {
        return Err(DecodeError::NotASnapshot);
    }
    Ok((body_len, root))
}

fn write_u32(buffer: &mut Vec<u8>, value: u32) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

fn write_f64(buffer: &mut Vec<u8>, value: f64) {
    buffer.extend_from_slice(&value.to_bits().to_le_bytes());
}

fn write_units(buffer: &mut Vec<u8>, units: &[u16]) {
    write_u32(buffer, units.len() as u32);
    for unit in units {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }
}

fn write_text(buffer: &mut Vec<u8>, text: &str) {
    write_u32(buffer, text.len() as u32);
    buffer.extend_from_slice(text.as_bytes());
}

/// A cursor over a blob, answering `None` rather than riding past the end.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(len)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u16(&mut self) -> Option<u16> {
        let bytes = self.take(2)?;
        Some(u16::from_le_bytes(bytes.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        let bytes = self.take(4)?;
        Some(u32::from_le_bytes(bytes.try_into().ok()?))
    }

    fn f64(&mut self) -> Option<f64> {
        let bytes = self.take(8)?;
        Some(f64::from_bits(u64::from_le_bytes(bytes.try_into().ok()?)))
    }

    fn units(&mut self) -> Option<Vec<u16>> {
        let len = self.u32()? as usize;
        let mut units = Vec::with_capacity(len.min(self.bytes.len()));
        for _ in 0..len {
            units.push(self.u16()?);
        }
        Some(units)
    }

    fn text(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        Some(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// Walk `value`, answering the serial of the record that will carry it.
///
/// A value the walk has seen is answered by its existing serial, which is what
/// makes a graph a graph rather than a tree.
fn visit(
    realm: &Handle<Realm>,
    value: Value,
    objects: &mut Vec<Value>,
    serials: &mut HashMap<Identity, u32>,
) -> Result<u32, Unsupported> {
    let key = identity(realm, value);
    if let Some(serial) = serials.get(&key) {
        return Ok(*serial);
    }
    serials.insert(key, objects.len() as u32);
    let serial = objects.len() as u32;
    objects.push(value);

    match value.kind() {
        ValueKind::Object(object) => {
            if realm.intrinsics.name_of_value(&value).is_none() {
                for child in children(&object)? {
                    visit(realm, child, objects, serials)?;
                }
            }
        }
        ValueKind::Function(_) => {
            if realm.intrinsics.name_of_value(&value).is_none() {
                return Err(Unsupported::new(
                    "a function",
                    "a function body is not carried yet",
                ));
            }
        }
        ValueKind::Undefined
        | ValueKind::Null
        | ValueKind::Boolean(_)
        | ValueKind::Number(_)
        | ValueKind::String(_)
        | ValueKind::BigInt(_)
        | ValueKind::Symbol(_) => {}
    }
    Ok(serial)
}

/// Every value an object reaches: its prototype, its keys, and its values.
///
/// An Array's elements are its values too, and its `length` is neither an
/// element nor an ordinary property, so the two exotics are asked the question
/// their kind makes meaningful. Every other exotic kind is refused here rather
/// than walked: its own state is not in `properties`, so writing it as the
/// ordinary object its shape would suggest would produce an object that is not
/// the one that was written — a proxy without its traps, a typed array without
/// its buffer, a `String` object without its string.
fn children(object: &Handle<JsObject>) -> Result<Vec<Value>, Unsupported> {
    let mut children = Vec::new();
    if let Some(prototype) = object
        .get_prototype_of()
        .map_err(|_| Unsupported::new("an object", "its prototype could not be read"))?
    {
        children.push(Value::Object(prototype));
    }
    match &object.kind {
        ObjectKind::Ordinary => {}
        ObjectKind::Array(_) => {
            for index in 0..array_length(object)? as u64 {
                let key = PropertyKey::String(string::index_atom(index));
                match object.get_own_property_key(&key) {
                    Ok(Some(property)) => {
                        if property.is_accessor() {
                            return Err(Unsupported::new(
                                "an array",
                                "an indexed accessor is not carried yet",
                            ));
                        }
                        children.extend(property_values(&property));
                    }
                    // A hole is not an element holding `undefined`: the two are
                    // told apart by own-property presence, and this format
                    // writes one of them until it carries the other.
                    Ok(None) => {
                        return Err(Unsupported::new(
                            "an array",
                            "an array with a hole is not carried yet",
                        ));
                    }
                    Err(_) => {
                        return Err(Unsupported::new("an array", "an element could not be read"));
                    }
                }
            }
        }
        ObjectKind::String(_) => {
            return Err(Unsupported::new(
                "a string object",
                "a primitive wrapper is not carried yet",
            ));
        }
        ObjectKind::Arguments(_) => {
            return Err(Unsupported::new(
                "an arguments object",
                "a mapped parameter binding is not carried yet",
            ));
        }
        ObjectKind::Proxy(_) => {
            return Err(Unsupported::new(
                "a proxy",
                "a proxy's traps and target are not carried yet",
            ));
        }
        ObjectKind::IntegerIndexed(_) => {
            return Err(Unsupported::new(
                "a typed array",
                "a typed array's view and buffer are not carried yet",
            ));
        }
        ObjectKind::ModuleNamespace(_) => {
            return Err(Unsupported::new(
                "a module namespace",
                "a module namespace's bindings are not carried yet",
            ));
        }
        ObjectKind::IsHTMLDDA => {
            return Err(Unsupported::new(
                "the host's document-all object",
                "a callable exotic is not carried yet",
            ));
        }
        ObjectKind::External(_) => {
            return Err(Unsupported::new(
                "a host pointer",
                "an external reference is not carried yet",
            ));
        }
        ObjectKind::Host(_) => {
            return Err(Unsupported::new(
                "a host object",
                "a host object is not carried yet",
            ));
        }
    }
    for key in object
        .own_property_keys()
        .map_err(|_| Unsupported::new("an object", "its own property names could not be read"))?
    {
        if !is_carried_key(object, &key) {
            continue;
        }
        children.push(key_value(&key));
        let property = object
            .get_own_property_key(&key)
            .map_err(|_| Unsupported::new("an object", "an own property could not be read"))?;
        if let Some(property) = property {
            children.extend(property_values(&property));
        }
    }
    Ok(children)
}

/// Whether an own property is written as a property of its own, rather than as
/// one of the slots an Array exotic keeps elsewhere: its indices are its
/// elements, and its `length` is the exotic's own invariant.
fn is_carried_key(object: &Handle<JsObject>, key: &PropertyKey) -> bool {
    if !matches!(&object.kind, ObjectKind::Array(_)) {
        return true;
    }
    let PropertyKey::String(atom) = key else {
        return true;
    };
    let text = string::lookup(*atom);
    canonical_index(&text).is_none() && text.as_slice() != LENGTH_UNITS
}

/// The index a property key spells, when it spells one canonically: the
/// language writes `"1"` for the first element and never `"01"`.
fn canonical_index(key: &JsString) -> Option<u32> {
    let units = key.as_slice();
    if units.is_empty() || (units.len() > 1 && units[0] == b'0' as u16) {
        return None;
    }
    let mut index: u32 = 0;
    for unit in units {
        let digit = char::from_u32(*unit as u32)?.to_digit(10)?;
        index = index.checked_mul(10)?.checked_add(digit)?;
    }
    Some(index)
}

/// The language value a property key is: a key is a string or a symbol.
fn key_value(key: &PropertyKey) -> Value {
    match key {
        PropertyKey::String(atom) => Value::String(Handle::new(string::lookup(*atom))),
        PropertyKey::Symbol(symbol) => Value::Symbol(*symbol),
    }
}

fn property_values(property: &Property) -> Vec<Value> {
    match &property.kind {
        PropertyKind::Data { value, .. } => vec![*value],
        PropertyKind::Accessor { get, set } => get.iter().chain(set.iter()).copied().collect(),
    }
}

/// An array's `length`, refused rather than guessed when it is not an index.
fn array_length(array: &Handle<JsObject>) -> Result<u32, Unsupported> {
    let key = JsString::from_utf16(LENGTH_UNITS);
    let length = array
        .get_own_property(&key)
        .map_err(|_| Unsupported::new("an array", "its length could not be read"))?
        .and_then(|property| property.value())
        .and_then(|value| value.as_number())
        .unwrap_or(0.0);
    if !(0.0..=4294967295.0).contains(&length) || length.trunc() != length {
        return Err(Unsupported::new(
            "an array",
            "its length is not an array index",
        ));
    }
    Ok(length as u32)
}

/// Write the record for a value the walk already gave a serial.
fn write_record(
    agent: &Agent,
    realm: &Handle<Realm>,
    value: Value,
    serials: &HashMap<Identity, u32>,
    body: &mut Vec<u8>,
) -> Result<(), Unsupported> {
    match value.kind() {
        ValueKind::Undefined => body.push(REC_UNDEFINED),
        ValueKind::Null => body.push(REC_NULL),
        ValueKind::Boolean(value) => {
            body.push(REC_BOOLEAN);
            body.push(u8::from(value));
        }
        ValueKind::Number(value) => {
            body.push(REC_NUMBER);
            write_f64(body, value);
        }
        ValueKind::String(text) => {
            body.push(REC_STRING);
            write_units(body, text.as_slice());
        }
        ValueKind::BigInt(number) => {
            body.push(REC_BIGINT);
            write_text(body, &bigint::to_string(&number, BIGINT_RADIX));
        }
        ValueKind::Symbol(symbol) => write_symbol(agent, symbol, body),
        ValueKind::Function(_) => match realm.intrinsics.name_of_value(&value) {
            Some(name) => {
                body.push(REC_INTRINSIC);
                write_text(body, &name);
            }
            None => {
                return Err(Unsupported::new(
                    "a function",
                    "a function body is not carried yet",
                ));
            }
        },
        ValueKind::Object(object) => match realm.intrinsics.name_of_value(&value) {
            Some(name) => {
                body.push(REC_INTRINSIC);
                write_text(body, &name);
            }
            None if matches!(&object.kind, ObjectKind::Array(_)) => {
                write_array(realm, object, serials, body)?
            }
            None => write_object(realm, object, serials, body)?,
        },
    }
    Ok(())
}

fn write_symbol(agent: &Agent, symbol: Handle<Symbol>, body: &mut Vec<u8>) {
    if let Some(index) = symbol::WELL_KNOWN_SYMBOLS
        .iter()
        .position(|name| symbol::well_known(name).id == symbol.id)
    {
        body.push(REC_SYMBOL_WELL_KNOWN);
        body.extend_from_slice(&(index as u16).to_le_bytes());
        return;
    }
    if let Some((key, _)) = agent
        .global_symbol_registry
        .borrow()
        .iter()
        .find(|(_, entry)| entry.id == symbol.id)
    {
        body.push(REC_SYMBOL_REGISTRY);
        write_units(body, key.as_slice());
        return;
    }
    body.push(REC_SYMBOL);
    match &symbol.description {
        Some(description) => {
            body.push(1);
            write_units(body, description.as_slice());
        }
        None => body.push(0),
    }
}

fn write_object(
    realm: &Handle<Realm>,
    object: Handle<JsObject>,
    serials: &HashMap<Identity, u32>,
    body: &mut Vec<u8>,
) -> Result<(), Unsupported> {
    body.push(REC_OBJECT);
    write_u32(body, prototype_serial(realm, &object, serials)?);
    body.push(u8::from(object.extensible.get()));
    write_properties(realm, &object, serials, body)?;
    Ok(())
}

fn write_array(
    realm: &Handle<Realm>,
    array: Handle<JsObject>,
    serials: &HashMap<Identity, u32>,
    body: &mut Vec<u8>,
) -> Result<(), Unsupported> {
    let length = array_length(&array)?;
    body.push(REC_ARRAY);
    write_u32(body, prototype_serial(realm, &array, serials)?);
    body.push(u8::from(array.extensible.get()));
    write_u32(body, length);
    for index in 0..length as u64 {
        let key = PropertyKey::String(string::index_atom(index));
        match array.get_own_property_key(&key) {
            Ok(Some(property)) => {
                let value = property.value().unwrap_or(Value::Undefined);
                write_u32(body, serial_in(realm, serials, value));
            }
            Ok(None) => {
                return Err(Unsupported::new(
                    "an array",
                    "an array with a hole is not carried yet",
                ));
            }
            Err(_) => return Err(Unsupported::new("an array", "an element could not be read")),
        }
    }
    write_properties(realm, &array, serials, body)?;
    Ok(())
}

/// Write an object's own properties: the count, then each one's key, its
/// attributes and the serials of what it holds.
fn write_properties(
    realm: &Handle<Realm>,
    object: &Handle<JsObject>,
    serials: &HashMap<Identity, u32>,
    body: &mut Vec<u8>,
) -> Result<(), Unsupported> {
    let keys = object
        .own_property_keys()
        .map_err(|_| Unsupported::new("an object", "its own property names could not be read"))?;
    let mut properties = Vec::with_capacity(keys.len());
    for key in &keys {
        if !is_carried_key(object, key) {
            continue;
        }
        let property = object
            .get_own_property_key(key)
            .map_err(|_| Unsupported::new("an object", "an own property could not be read"))?;
        let Some(property) = property else {
            continue;
        };
        properties.push(stored_property(realm, key, &property, serials));
    }
    write_u32(body, properties.len() as u32);
    for property in &properties {
        write_u32(body, property.key);
        body.push(property.flags);
        write_u32(body, property.first);
        write_u32(body, property.second);
    }
    Ok(())
}

fn prototype_serial(
    realm: &Handle<Realm>,
    object: &Handle<JsObject>,
    serials: &HashMap<Identity, u32>,
) -> Result<u32, Unsupported> {
    match object.get_prototype_of() {
        Ok(Some(prototype)) => Ok(serial_in(realm, serials, Value::Object(prototype))),
        Ok(None) => Ok(NO_REF),
        Err(_) => Err(Unsupported::new(
            "an object",
            "its prototype could not be read",
        )),
    }
}

fn stored_property(
    realm: &Handle<Realm>,
    key: &PropertyKey,
    property: &Property,
    serials: &HashMap<Identity, u32>,
) -> StoredProperty {
    let mut flags = 0u8;
    if property.enumerable {
        flags |= FLAG_ENUMERABLE;
    }
    if property.configurable {
        flags |= FLAG_CONFIGURABLE;
    }
    let (first, second) = match &property.kind {
        PropertyKind::Data { value, writable } => {
            if *writable {
                flags |= FLAG_WRITABLE;
            }
            (serial_in(realm, serials, *value), 0)
        }
        PropertyKind::Accessor { get, set } => {
            flags |= FLAG_ACCESSOR;
            (
                get.map_or(NO_REF, |value| serial_in(realm, serials, value)),
                set.map_or(NO_REF, |value| serial_in(realm, serials, value)),
            )
        }
    };
    StoredProperty {
        key: serial_in(realm, serials, key_value(key)),
        flags,
        first,
        second,
    }
}

/// The serial of a value the walk has already seen.
///
/// Every value a record refers to is one the walk visited first, so a miss here
/// is a walk bug; `NO_REF` is what a record gets rather than a panic, so a bug
/// shows up as a refused decode instead of a crash in a host's build.
fn serial_in(realm: &Handle<Realm>, serials: &HashMap<Identity, u32>, value: Value) -> u32 {
    serials
        .get(&identity(realm, value))
        .copied()
        .unwrap_or(NO_REF)
}

fn read_record(reader: &mut Reader<'_>) -> Result<Record, DecodeError> {
    let tag = reader.u8().ok_or(DecodeError::Truncated)?;
    match tag {
        REC_UNDEFINED => Ok(Record::Undefined),
        REC_NULL => Ok(Record::Null),
        REC_BOOLEAN => Ok(Record::Boolean(
            reader.u8().ok_or(DecodeError::Truncated)? != 0,
        )),
        REC_NUMBER => Ok(Record::Number(reader.f64().ok_or(DecodeError::Truncated)?)),
        REC_STRING => Ok(Record::String(
            reader.units().ok_or(DecodeError::Truncated)?,
        )),
        REC_BIGINT => Ok(Record::BigInt(reader.text().ok_or(DecodeError::Truncated)?)),
        REC_SYMBOL_WELL_KNOWN => Ok(Record::SymbolWellKnown(
            reader.u16().ok_or(DecodeError::Truncated)?,
        )),
        REC_SYMBOL_REGISTRY => Ok(Record::SymbolRegistry(
            reader.units().ok_or(DecodeError::Truncated)?,
        )),
        REC_SYMBOL => {
            if reader.u8().ok_or(DecodeError::Truncated)? == 0 {
                Ok(Record::Symbol(None))
            } else {
                Ok(Record::Symbol(Some(
                    reader.units().ok_or(DecodeError::Truncated)?,
                )))
            }
        }
        REC_INTRINSIC => Ok(Record::Intrinsic(
            reader.text().ok_or(DecodeError::Truncated)?,
        )),
        REC_OBJECT => {
            let proto = reader.u32().ok_or(DecodeError::Truncated)?;
            let extensible = reader.u8().ok_or(DecodeError::Truncated)? != 0;
            let properties = read_properties(reader)?;
            Ok(Record::Object {
                proto,
                extensible,
                properties,
            })
        }
        REC_ARRAY => {
            let proto = reader.u32().ok_or(DecodeError::Truncated)?;
            let extensible = reader.u8().ok_or(DecodeError::Truncated)? != 0;
            let length = reader.u32().ok_or(DecodeError::Truncated)?;
            let mut elements = Vec::with_capacity(length.min(1024) as usize);
            for _ in 0..length {
                elements.push(reader.u32().ok_or(DecodeError::Truncated)?);
            }
            let extras = read_properties(reader)?;
            Ok(Record::Array {
                proto,
                extensible,
                length,
                elements,
                extras,
            })
        }
        other => Err(DecodeError::BadTag(other)),
    }
}

fn read_properties(reader: &mut Reader<'_>) -> Result<Vec<StoredProperty>, DecodeError> {
    let count = reader.u32().ok_or(DecodeError::Truncated)? as usize;
    let mut properties = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        properties.push(StoredProperty {
            key: reader.u32().ok_or(DecodeError::Truncated)?,
            flags: reader.u8().ok_or(DecodeError::Truncated)?,
            first: reader.u32().ok_or(DecodeError::Truncated)?,
            second: reader.u32().ok_or(DecodeError::Truncated)?,
        });
    }
    Ok(properties)
}

struct Builder<'a> {
    agent: &'a Agent,
    realm: &'a Handle<Realm>,
    records: &'a [Record],
    made: Vec<Option<Built>>,
    /// Every value the build has made, kept alive for the build: the walk
    /// assembles a graph in host memory the collector cannot see, and a
    /// collection in the middle — `--gc-stress` runs one per allocation —
    /// would otherwise sweep a record before the record that names it is
    /// filled.
    pins: Vec<Pin>,
}

/// A record's value, before it is asked for as a language value: an object's
/// prototype has to be set at its creation, and a prototype is an object rather
/// than a value.
#[derive(Clone, Copy)]
enum Built {
    Value(Value),
    Object(Handle<JsObject>),
}

impl Built {
    fn value(&self) -> Value {
        match self {
            Built::Value(value) => *value,
            Built::Object(object) => Value::Object(*object),
        }
    }
}

impl Builder<'_> {
    fn materialize(&mut self, serial: u32) -> Option<Value> {
        self.object(serial).map(|built| built.value())
    }

    fn object(&mut self, serial: u32) -> Option<Built> {
        if serial == NO_REF {
            return None;
        }
        let index = serial as usize;
        if let Some(made) = self.made.get(index).and_then(|made| made.as_ref()) {
            return Some(*made);
        }
        let built = match self.records.get(index)? {
            Record::Undefined => Built::Value(Value::Undefined),
            Record::Null => Built::Value(Value::Null),
            Record::Boolean(value) => Built::Value(Value::Boolean(*value)),
            Record::Number(value) => Built::Value(Value::Number(*value)),
            Record::String(units) => {
                Built::Value(Value::String(Handle::new(JsString::from_utf16(units))))
            }
            Record::BigInt(text) => Built::Value(Value::BigInt(Handle::new(BigInt::parse_str(
                text,
                BIGINT_RADIX,
            )?))),
            Record::SymbolWellKnown(name) => {
                let name = symbol::WELL_KNOWN_SYMBOLS.get(*name as usize)?;
                Built::Value(Value::Symbol(symbol::well_known(name)))
            }
            Record::SymbolRegistry(key) => Built::Value(Value::Symbol(self.registry_symbol(key)?)),
            Record::Symbol(description) => {
                let description = description
                    .as_ref()
                    .map(|units| JsString::from_utf16(units));
                Built::Value(Value::Symbol(Handle::new(Symbol::new(description))))
            }
            Record::Intrinsic(name) => Built::Value(self.realm.intrinsics.get(name)?),
            Record::Object {
                proto,
                extensible,
                properties,
            } => {
                let prototype = self.prototype(*proto);
                let object = JsObject::ordinary_object_create(prototype);
                object.extensible.set(*extensible);
                // The shell is recorded before the properties are defined, so a
                // property that refers back to this object resolves to it
                // rather than restarting the build.
                self.remember(index, Built::Object(object))?;
                define_properties(self, object, properties);
                Built::Object(object)
            }
            Record::Array {
                proto,
                extensible,
                length,
                elements,
                extras,
            } => {
                let prototype = self.prototype(*proto);
                let array = JsObject::array_create(prototype, *length as f64).ok()?;
                array.extensible.set(*extensible);
                self.remember(index, Built::Object(array))?;
                for (position, element) in elements.iter().enumerate() {
                    let value = self.materialize(*element)?;
                    array
                        .create_data_property_index(position as u64, value)
                        .ok()?;
                }
                define_properties(self, array, extras);
                Built::Object(array)
            }
        };
        if self.made[index].is_none() {
            self.remember(index, built)?;
        }
        Some(built)
    }

    fn prototype(&mut self, serial: u32) -> Option<Handle<JsObject>> {
        if serial == NO_REF {
            return None;
        }
        match self.object(serial)? {
            Built::Object(object) => Some(object),
            // An intrinsic's record materializes as a value, not as a built
            // object, because a prototype is not always one: `%Object.prototype%`
            // arrives this way and is exactly the case the format exists for.
            Built::Value(value) => value.as_object(),
        }
    }

    /// A `Symbol.for` symbol: the registry's own, made when the reading agent
    /// has not seen the key.
    fn registry_symbol(&self, key: &[u16]) -> Option<Handle<Symbol>> {
        let key = JsString::from_utf16(key);
        let mut registry = self.agent.global_symbol_registry.borrow_mut();
        if let Some((_, symbol)) = registry.iter().find(|(entry, _)| *entry == key) {
            return Some(Handle::new(symbol.clone()));
        }
        let symbol = Handle::new(Symbol::new(Some(key.clone())));
        registry.push((key, (*symbol).clone()));
        Some(symbol)
    }

    fn remember(&mut self, index: usize, built: Built) -> Option<Built> {
        let pin = match &built {
            Built::Value(value) => crux::heap::pin(*value),
            Built::Object(object) => crux::heap::pin_handle(*object),
        };
        self.pins.push(pin);
        self.made[index] = Some(built);
        Some(built)
    }
}

/// Define a record's properties on an object, in the order the blob wrote them:
/// that order is the object's own enumeration order, so restoring it in place
/// restores what a host's `for...in` would see.
fn define_properties(
    builder: &mut Builder<'_>,
    object: Handle<JsObject>,
    properties: &[StoredProperty],
) {
    for property in properties {
        let Some(value) = builder.materialize(property.key) else {
            return;
        };
        let Some(key) = property_key(&value) else {
            return;
        };
        let accessor = property.flags & FLAG_ACCESSOR != 0;
        let descriptor = PropertyDescriptor {
            value: if accessor {
                None
            } else {
                builder.materialize(property.first)
            },
            writable: if accessor {
                None
            } else {
                Some(property.flags & FLAG_WRITABLE != 0)
            },
            get: if accessor {
                builder.materialize(property.first)
            } else {
                None
            },
            set: if accessor {
                builder.materialize(property.second)
            } else {
                None
            },
            enumerable: Some(property.flags & FLAG_ENUMERABLE != 0),
            configurable: Some(property.flags & FLAG_CONFIGURABLE != 0),
        };
        let _ = object.define_property_key(&key, &descriptor);
    }
}

/// The property key a decoded value names: the writer only writes a string or a
/// symbol where a key goes.
fn property_key(value: &Value) -> Option<PropertyKey> {
    match value.kind() {
        ValueKind::String(text) => Some(PropertyKey::String(string::intern(text.as_slice()))),
        ValueKind::Symbol(symbol) => Some(PropertyKey::Symbol(symbol)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api;

    /// An isolate and the realm its first context made: what a host has in hand
    /// when it takes a snapshot.
    fn fixture() -> (Box<api::Isolate>, Handle<Realm>) {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        (isolate, realm)
    }

    fn agent_of(isolate: &api::Isolate) -> &Agent {
        // SAFETY: the pointer names the isolate's own agent, and the isolate
        // outlives every borrow taken here.
        unsafe { &*isolate.agent_ptr() }
    }

    fn encode_value(isolate: &api::Isolate, realm: &Handle<Realm>, value: Value) -> Vec<u8> {
        encode(agent_of(isolate), realm, value).expect("encode")
    }

    fn round_trip(isolate: &api::Isolate, realm: &Handle<Realm>, value: Value) -> Value {
        let blob = encode_value(isolate, realm, value);
        decode(agent_of(isolate), realm, &blob).expect("decode")
    }

    fn array_with(realm: &Handle<Realm>, values: &[Value]) -> Value {
        let prototype = realm
            .intrinsics
            .array_prototype()
            .and_then(|value| value.as_object());
        let array = JsObject::array_create(prototype, 0.0).expect("an array");
        for (index, value) in values.iter().enumerate() {
            array
                .create_data_property_index(index as u64, *value)
                .expect("element");
        }
        Value::Object(array)
    }

    fn number_of(value: Value) -> Option<f64> {
        value.as_number()
    }

    #[test]
    fn primitives_round_trip() {
        let (isolate, realm) = fixture();
        for value in [
            Value::Undefined,
            Value::Null,
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Number(1.5),
            Value::Number(f64::NAN),
        ] {
            let back = round_trip(&isolate, &realm, value);
            assert!(
                values_identical(value, back),
                "{value:?} did not round trip"
            );
        }
        let negative_zero = round_trip(&isolate, &realm, Value::Number(-0.0));
        assert!(negative_zero.as_number().unwrap().is_sign_negative());
    }

    /// Two values are the same language value: the walk's own notion, which is
    /// what a round trip has to preserve.
    fn values_identical(a: Value, b: Value) -> bool {
        match (a.kind(), b.kind()) {
            (ValueKind::Undefined, ValueKind::Undefined) | (ValueKind::Null, ValueKind::Null) => {
                true
            }
            (ValueKind::Boolean(a), ValueKind::Boolean(b)) => a == b,
            (ValueKind::Number(a), ValueKind::Number(b)) => a.to_bits() == b.to_bits(),
            (ValueKind::String(a), ValueKind::String(b)) => a.as_slice() == b.as_slice(),
            (ValueKind::Symbol(a), ValueKind::Symbol(b)) => a.id == b.id,
            (ValueKind::BigInt(a), ValueKind::BigInt(b)) => {
                bigint::to_string(&a, 10) == bigint::to_string(&b, 10)
            }
            (ValueKind::Object(a), ValueKind::Object(b)) => a.id() == b.id(),
            (ValueKind::Function(a), ValueKind::Function(b)) => a.id() == b.id(),
            _ => false,
        }
    }

    #[test]
    fn a_lone_surrogate_survives() {
        let (isolate, realm) = fixture();
        let text = Handle::new(JsString::from_utf16(&[0x0041, 0xD800, 0x0042]));
        let back = round_trip(&isolate, &realm, Value::String(text));
        let back = back.as_string().expect("a string");
        assert_eq!(back.as_slice(), &[0x0041, 0xD800, 0x0042]);
    }

    #[test]
    fn bigints_round_trip_by_value() {
        let (isolate, realm) = fixture();
        for text in [
            "0",
            "-1",
            "123456789012345678901234567890",
            "340282366920938463463374607431768211456",
        ] {
            let value = Handle::new(BigInt::parse_str(text, 10).expect("a bigint"));
            let back = round_trip(&isolate, &realm, Value::BigInt(value));
            assert_eq!(
                bigint::to_string(&back.as_bigint().expect("a bigint"), 10),
                text
            );
        }
    }

    #[test]
    fn well_known_symbols_keep_their_identity() {
        let (isolate, realm) = fixture();
        let value = Value::Symbol(symbol::well_known("iterator"));
        let back = round_trip(&isolate, &realm, value);
        assert_eq!(
            back.as_symbol().expect("a symbol").id,
            value.as_symbol().unwrap().id
        );
    }

    #[test]
    fn registry_symbols_come_back_as_the_registrys_own() {
        let (isolate, realm) = fixture();
        let key = JsString::from_utf8("shared");
        let symbol = Handle::new(Symbol::new(Some(key.clone())));
        agent_of(&isolate)
            .global_symbol_registry
            .borrow_mut()
            .push((key, (*symbol).clone()));
        let back = round_trip(&isolate, &realm, Value::Symbol(symbol));
        let back = back.as_symbol().expect("a symbol");
        assert!(
            agent_of(&isolate)
                .global_symbol_registry
                .borrow()
                .iter()
                .any(|(_, entry)| entry.id == back.id),
            "the restored symbol is the registry's own"
        );
    }

    #[test]
    fn two_symbols_with_one_description_stay_two() {
        let (isolate, realm) = fixture();
        let text = JsString::from_utf8("x");
        let first = Handle::new(Symbol::new(Some(text.clone())));
        let second = Handle::new(Symbol::new(Some(text)));
        let pair = array_with(&realm, &[Value::Symbol(first), Value::Symbol(second)]);
        let back = round_trip(&isolate, &realm, pair);
        let back = back.as_object().expect("an array");
        let read = |index: &str| {
            back.get_own_property(&JsString::from_utf8(index))
                .expect("read")
                .and_then(|property| property.value())
                .and_then(|value| value.as_symbol())
                .expect("a symbol")
        };
        assert_ne!(read("0").id, read("1").id);
    }

    #[test]
    fn an_object_carries_its_attributes_and_its_intrinsic_prototype() {
        let (isolate, realm) = fixture();
        let prototype = realm
            .intrinsics
            .object_prototype()
            .and_then(|value| value.as_object())
            .expect("%Object.prototype%");
        let object = JsObject::ordinary_object_create(Some(prototype));
        object
            .create_data_property(&JsString::from_utf8("visible"), Value::Number(1.0))
            .expect("define");
        object
            .define_property(
                &JsString::from_utf8("hidden"),
                &PropertyDescriptor {
                    value: Some(Value::Number(2.0)),
                    writable: Some(false),
                    get: None,
                    set: None,
                    enumerable: Some(false),
                    configurable: Some(false),
                },
            )
            .expect("define");

        let back = round_trip(&isolate, &realm, Value::Object(object));
        let back = back.as_object().expect("an object");
        assert_eq!(
            back.get_prototype_of()
                .expect("prototype")
                .map(|prototype| prototype.id()),
            Some(prototype.id()),
            "the prototype came back as the realm's own %Object.prototype%"
        );
        let hidden = back
            .get_own_property(&JsString::from_utf8("hidden"))
            .expect("read")
            .expect("present");
        assert_eq!(hidden.value().and_then(number_of), Some(2.0));
        assert!(!hidden.enumerable);
        assert!(!hidden.configurable);
        assert_eq!(hidden.writable(), Some(false));
        assert!(
            back.get_own_property(&JsString::from_utf8("visible"))
                .expect("read")
                .expect("present")
                .enumerable
        );
    }

    #[test]
    fn a_cycle_comes_back_a_cycle() {
        let (isolate, realm) = fixture();
        let object = JsObject::ordinary_object_create(None);
        object
            .create_data_property(&JsString::from_utf8("self"), Value::Object(object))
            .expect("define");
        object
            .create_data_property(&JsString::from_utf8("again"), Value::Object(object))
            .expect("define");

        let back = round_trip(&isolate, &realm, Value::Object(object));
        let back = back.as_object().expect("an object");
        let read = |name: &str| {
            back.get_own_property(&JsString::from_utf8(name))
                .expect("read")
                .and_then(|property| property.value())
                .and_then(|value| value.as_object())
                .expect("an object")
                .id()
        };
        assert_eq!(read("self"), back.id());
        assert_eq!(read("again"), back.id());
    }

    #[test]
    fn a_null_prototype_stays_null() {
        let (isolate, realm) = fixture();
        let object = JsObject::ordinary_object_create(None);
        let back = round_trip(&isolate, &realm, Value::Object(object));
        assert!(
            back.as_object()
                .expect("an object")
                .get_prototype_of()
                .expect("prototype")
                .is_none()
        );
    }

    #[test]
    fn an_array_round_trips_with_its_length_and_elements() {
        let (isolate, realm) = fixture();
        let array = array_with(
            &realm,
            &[
                Value::Number(1.0),
                Value::String(Handle::new(JsString::from_utf8("two"))),
                Value::Boolean(true),
            ],
        );
        let back = round_trip(&isolate, &realm, array);
        let back = back.as_object().expect("an array");
        assert_eq!(array_length(&back).expect("a length"), 3);
        assert_eq!(
            back.get_own_property(&JsString::from_utf8("1"))
                .expect("read")
                .and_then(|property| property.value())
                .and_then(|value| value.as_string())
                .map(|text| text.to_string_lossy()),
            Some("two".to_string())
        );
        // The prototype is the realm's own, restored by name.
        assert_eq!(
            back.get_prototype_of()
                .expect("prototype")
                .map(|prototype| prototype.id()),
            realm
                .intrinsics
                .array_prototype()
                .and_then(|value| value.as_object())
                .map(|prototype| prototype.id())
        );
    }

    #[test]
    fn a_hole_is_refused_rather_than_written_as_undefined() {
        let (isolate, realm) = fixture();
        let prototype = realm
            .intrinsics
            .array_prototype()
            .and_then(|value| value.as_object());
        let array = JsObject::array_create(prototype, 2.0).expect("an array");
        array
            .create_data_property_index(1, Value::Number(1.0))
            .expect("element");
        let error = encode(agent_of(&isolate), &realm, Value::Object(array)).expect_err("refused");
        assert_eq!(error.type_name, "an array");
        assert!(error.detail.contains("hole"));
    }

    /// Each exotic kind is refused by name rather than written as the ordinary
    /// object its shape would suggest: a proxy has traps in its slots, a typed
    /// array has a buffer, a `String` object has a string, and none of that is
    /// in `properties`.
    #[test]
    fn an_exotic_object_is_refused_by_name() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        for (source, expected) in [
            ("new Proxy({}, {})", "a proxy"),
            ("new Uint8Array(2)", "a typed array"),
            ("new String('x')", "a string object"),
            (
                "(function () { return arguments; })()",
                "an arguments object",
            ),
        ] {
            let value = context
                .try_eval(source)
                .expect("a value to refuse")
                .into_value();
            let error = encode(agent_of(&isolate), &realm, value).expect_err("the walk refuses");
            assert_eq!(
                error.type_name, expected,
                "{source} was refused as something else"
            );
        }
    }

    #[test]
    fn a_root_that_names_no_record_is_refused() {
        let (isolate, realm) = fixture();
        let mut blob = encode_value(&isolate, &realm, Value::Number(1.0));
        assert!(decode(agent_of(&isolate), &realm, &blob).is_ok());
        // The root serial sits at the end of the header; a blob whose graph
        // does not close is refused rather than answered with a guess.
        let root_at = HEADER_LEN - 4;
        blob[root_at..root_at + 4].copy_from_slice(&7u32.to_le_bytes());
        assert!(decode(agent_of(&isolate), &realm, &blob).is_err());
    }

    #[test]
    fn a_foreign_blob_is_refused_by_each_of_its_fields() {
        let (isolate, realm) = fixture();
        let blob = encode_value(&isolate, &realm, Value::Number(1.0));
        let decode_it = |bytes: &[u8]| decode(agent_of(&isolate), &realm, bytes);

        let mut wrong_magic = blob.clone();
        wrong_magic[0] = b'X';
        assert_eq!(decode_it(&wrong_magic), Err(DecodeError::NotASnapshot));

        let mut wrong_version = blob.clone();
        wrong_version[8..12].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        assert!(matches!(
            decode_it(&wrong_version),
            Err(DecodeError::Version { .. })
        ));

        let mut wrong_word = blob.clone();
        wrong_word[12] = 3;
        assert!(matches!(
            decode_it(&wrong_word),
            Err(DecodeError::WordSize { .. })
        ));

        let mut wrong_endian = blob.clone();
        wrong_endian[13] = 2;
        assert!(matches!(
            decode_it(&wrong_endian),
            Err(DecodeError::Endianness { .. })
        ));

        let mut cut_short = blob.clone();
        cut_short.truncate(blob.len() - 1);
        assert!(decode_it(&cut_short).is_err());

        let mut bad_tag = blob.clone();
        bad_tag[HEADER_LEN + 4] = 200;
        assert!(matches!(decode_it(&bad_tag), Err(DecodeError::BadTag(200))));

        assert_eq!(
            decode_it(b"not a snapshot at all"),
            Err(DecodeError::NotASnapshot)
        );
    }

    #[test]
    fn an_intrinsic_is_written_as_its_name() {
        let (isolate, realm) = fixture();
        let prototype = realm
            .intrinsics
            .object_prototype()
            .expect("%Object.prototype%");
        let blob = encode_value(&isolate, &realm, prototype);
        assert!(
            blob.windows(13).any(|window| window == b"%Object.proto"),
            "the builtin is in the blob by name, not by structure"
        );
        let back = decode(agent_of(&isolate), &realm, &blob).expect("decode");
        assert_eq!(
            back.as_object().expect("an object").id(),
            prototype.as_object().unwrap().id()
        );
    }

    /// Two references to one object come back as one object, rather than as two
    /// equal ones — the property a host's own bookkeeping depends on.
    #[test]
    fn a_shared_object_stays_one_object() {
        let (isolate, realm) = fixture();
        let shared = JsObject::ordinary_object_create(None);
        let pair = array_with(&realm, &[Value::Object(shared), Value::Object(shared)]);
        let back = round_trip(&isolate, &realm, pair);
        let back = back.as_object().expect("an array");
        let read = |index: &str| {
            back.get_own_property(&JsString::from_utf8(index))
                .expect("read")
                .and_then(|property| property.value())
                .and_then(|value| value.as_object())
                .expect("an object")
                .id()
        };
        assert_eq!(read("0"), read("1"));
    }
}
