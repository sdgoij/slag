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
//! the value it refused: a proxy, a typed array, a module namespace, a host
//! object, an array with a hole, an indexed accessor, an array whose `length` is
//! not an index, a host pointer the host's external-reference table does not
//! have, a host callback (a built-in that is not an intrinsic) when the host
//! supplies none of its own, and a function
//! the engine kept no source text for — a method or an accessor, whose source is
//! method-form and needs a parse context of its own, or a function created where
//! there is no source text at all (a `Function`-built body, for one).
//! Each entry is a subsystem to carry, and the walk refuses rather than writing
//! something a restore would read back wrong.
//!
//! # Functions
//!
//! A JavaScript function is written as the source text it can be re-parsed
//! from, its [[Strict]], and its object part, and the restore evaluates that
//! source in the realm's global environment. That is enough for a function
//! whose body is self-contained and not for one that closed over a scope: a
//! blob holds the value graph, not the environment chain a closure was
//! compiled in, so a restored function resolves a free name globally. A bind
//! exotic is written as the three values it is made of — its target, its bound
//! `this` and its bound arguments — so a chain of binds round trips and an
//! intrinsic target is still written by name; a host callback — a Rust closure
//! in this engine — has no source at all, so it is carried through the host
//! instead (see below).
//!
//! A class constructor's source is the **class** it came from: that is the
//! spec's own `[[SourceText]]`, which ClassDefinitionEvaluation sets for an
//! implicit constructor and an explicit one alike. So the record says which
//! grammar its source is in, and the restore evaluates it as a class expression,
//! which brings everything a class is made of back through the engine's own
//! class evaluation — its kind, home object, prototype object, fields and
//! private environment. It is also the one record whose restore **runs
//! definition-time code again**: a computed key, a `static {}` block and a static
//! field initializer execute, where V8's snapshot only restores objects.
//!
//! An **arrow** is carried the same way and for the same reason: its
//! `[[SourceText]]` is the expression it was written as (`(a) => a + 1`), which
//! is not a form `parse_function` can read, so the restore evaluates it in the
//! reading realm instead. What that costs is the scope the arrow closed over: a
//! restored arrow's `this` and its free names are the reading realm's global, the
//! same limit the function record states for free names and a harder one here,
//! because an arrow exists to capture `this`. Its kind survives — an async
//! arrow's `[[Prototype]]` comes from the evaluation and from the record alike.
//!
//! A host callback is the one kind the engine cannot rebuild alone: its body is
//! the host's Rust closure, so the record carries what only the host has — the
//! index of the external-reference table entry the call came from, and the data
//! value the function reads — and the restore asks the host to make the call
//! again. That is the `HostCallbacks` contract: a build that supplies none
//! carries none, which is what the walk refused before this record existed.
//!
//! Version 1. The format is ours to version, and its compatibility surface is
//! the *names* it writes: an intrinsic name and a well-known symbol name have to
//! mean the same thing in the tree that reads a blob as in the tree that wrote
//! it. A blob therefore carries the format version and the word size it was
//! written with, and a reader refuses any other.
//!
//! # The context table
//!
//! A blob holds one table of contexts, each with the index a host gave it —
//! `deno_core`'s is the crate we stand in for's: the default context is 0 and
//! the ones added after it are 1, 2, ... — and each context's items are
//! **written and read against its own realm**. That is not a detail: a realm's
//! objects carry that realm's builtins as their prototypes, so a value has to be
//! recognized by the realm it belongs to, and materialized in the realm that is
//! restoring it. Two realms' `%Array.prototype%` are two objects with one name,
//! which is why the name is what a blob writes and the realm is what resolves
//! it: one record serves both slots, and each restore gives it that realm's own.
//!
//! A value *shared* between two contexts comes back as one value per context,
//! because a restore makes values one slot at a time. A host that needs one
//! object in both realms has a cross-realm reference, which V8's own snapshot
//! would carry and this format does not claim to yet.

use std::collections::HashMap;

use crux::bigint::{self, BigInt};
use crux::function::{Function, FunctionKind};
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
/// body length (4) + context count (4).
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
const REC_EXTERNAL: u8 = 13;
const REC_FUNCTION: u8 = 14;
const REC_BOUND_FUNCTION: u8 = 15;
const REC_HOST_CALLBACK: u8 = 16;

/// Which grammar a function record's source is read in. A class constructor's
/// `[[SourceText]]` is the **class** it came from (spec 15.7.14 sets it that way
/// for an implicit constructor and an explicit one alike), and an arrow's is the
/// **expression** it was written as (spec 15.3.3), so the record says
/// which grammar its source is in rather than leaving the reader to guess from
/// the text.
const GRAMMAR_FUNCTION: u8 = 0;
const GRAMMAR_CLASS: u8 = 1;
const GRAMMAR_ARROW: u8 = 2;

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

    /// The refusal for an empty context table: a blob with no contexts names
    /// nothing, so it is a caller bug rather than a value the format cannot
    /// carry.
    pub(crate) const fn empty_table() -> Self {
        Self::new(
            "the context table",
            "a snapshot carries the data of at least one context",
        )
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
    /// A function record could not be rebuilt into a function in the realm being
    /// restored: a body whose source will not parse or instantiate, a class whose
    /// class source will not evaluate, or a bind exotic whose target is not
    /// callable.
    UnrebuildableFunction(String),
    /// A blob names an intrinsic the reading realm does not have.
    UnknownIntrinsic(String),
    /// A function record's grammar byte is not one this format's.
    BadGrammar(u8),
    /// A blob carries a host callback and the load supplied no callbacks.
    NoHostCallback(usize),
    /// A blob names an entry the host's external-reference table does not have.
    ExternalIndexOutOfRange { index: usize, count: usize },
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
            Self::UnrebuildableFunction(reason) => {
                write!(f, "snapshot function could not be rebuilt: {reason}")
            }
            Self::UnknownIntrinsic(name) => {
                write!(
                    f,
                    "snapshot names the intrinsic {name}, which this realm has not"
                )
            }
            Self::BadGrammar(byte) => {
                write!(
                    f,
                    "snapshot function grammar byte {byte} is not one of this format's"
                )
            }
            Self::NoHostCallback(index) => write!(
                f,
                "snapshot names a host callback at external reference {index}, which the load's host did not supply"
            ),
            Self::ExternalIndexOutOfRange { index, count } => write!(
                f,
                "snapshot names external reference {index}, the host's table has {count}"
            ),
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
    /// A host pointer, by its index in the host's external-reference table.
    External(u32),
    /// A function: which grammar its source is read in, that source, whether it
    /// is strict, and its object part.
    Function {
        grammar: Grammar,
        source: Vec<u16>,
        strict: bool,
        proto: u32,
        extensible: bool,
        properties: Vec<StoredProperty>,
    },
    /// A bind exotic: the target, the bound `this`, and the bound arguments,
    /// plus its object part — which is where its `length` and `name` live, so
    /// they are carried rather than computed back from the target.
    BoundFunction {
        target: u32,
        bound_this: u32,
        bound_args: Vec<u32>,
        proto: u32,
        extensible: bool,
        properties: Vec<StoredProperty>,
    },
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
    /// A host callback: the external-reference table entry it was built from,
    /// the data it reads, and its object part.
    HostCallback {
        index: u32,
        data: u32,
        proto: u32,
        extensible: bool,
        properties: Vec<StoredProperty>,
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

/// A host callback the host built: the table entry its call comes from, and the
/// data it was built with.
///
/// The data is a *value*, so a blob carries it beside the index rather than
/// trusting the load to have the same one — which is what makes a callback that
/// reads `FunctionBuilder::data` carriable rather than quietly broken, and every
/// one of deno's ops is built that way.
pub struct HostCallback {
    /// The external-reference table entry the call comes from.
    pub pointer: usize,
    /// The value the host attached when it built the function, when it did.
    pub data: Option<crate::api::Local>,
}

/// The host half of carrying a callback: which of its own callbacks a function
/// was built from, and the call a table entry names.
///
/// A built-in function in this engine is a Rust closure, so a snapshot cannot
/// rebuild one — the closure is the host's, and only the host can make another.
/// What both ends need is the pointer its external-reference table holds, since
/// the address is a property of the process that loads a blob rather than of the
/// data: the write side is asked what a function was built from, and the read
/// side is asked what call an entry names. A host that supplies neither carries
/// no callbacks, and its builtins refuse as they always have.
pub trait HostCallbacks {
    /// What the host knows about `function`, or `None` for a builtin the host
    /// did not make.
    fn callback_of(&self, function: &Handle<Function>) -> Option<HostCallback>;

    /// The host callback the load's table holds at `pointer`, with the data a
    /// blob carried for it, in the engine's own call shape — or `None` when that
    /// entry is not one a callback can be made from.
    fn callback_at(
        &self,
        pointer: usize,
        data: Option<crate::api::Local>,
    ) -> Option<crate::api::FunctionCallback>;
}

/// One context's slot in a blob: the index the host gave it, the realm its
/// values were built in, and the values themselves.
///
/// The realm is not a hint. A value's builtins are its own realm's, so the walk
/// recognizes an intrinsic — and so writes a reference to it as a name — only
/// through the realm the value belongs to. A slot whose realm is not the one its
/// values were built in fails the walk rather than writing references that
/// resolve to another realm's builtins.
pub struct Slot<'a> {
    /// The index the host gave this context (the crate we stand in for's
    /// convention: the default context is 0, the ones added after it are 1, 2,
    /// ...).
    pub index: usize,
    /// The realm the slot's values belong to.
    pub realm: Handle<Realm>,
    /// The values the host attached to that context, in the order it attached
    /// them: the indices a restore hands them back under.
    pub items: &'a [Value],
}

/// Write the data a host attached to each context, as one blob.
///
/// `agent` answers the `Symbol.for` registry, whose symbols have an identity
/// beyond one blob; every other question the walk asks is asked of the realm the
/// value belongs to. `externals` is the host's external-reference table: a
/// snapshot cannot hold a function or data address, because an address is a
/// property of the process that loads the blob rather than of the data, so a
/// host pointer is written as its *index* into that table and the host rebuilds
/// the table for every load. `host` is the host's own callbacks, when it has
/// any: a built-in is a Rust closure, so only the host can say which table entry
/// one was built from, and a host that supplies none has its builtins refused by
/// name.
pub fn encode_slots(
    agent: &Agent,
    slots: &[Slot<'_>],
    externals: &[usize],
    host: Option<&dyn HostCallbacks>,
) -> Result<Vec<u8>, Unsupported> {
    let mut objects: Vec<(Value, Handle<Realm>)> = Vec::new();
    let mut serials: HashMap<Identity, u32> = HashMap::new();
    let mut table: Vec<(usize, Vec<u32>)> = Vec::with_capacity(slots.len());
    for slot in slots {
        let mut items = Vec::with_capacity(slot.items.len());
        for item in slot.items {
            items.push(visit(
                agent,
                &slot.realm,
                externals,
                host,
                *item,
                &mut objects,
                &mut serials,
            )?);
        }
        table.push((slot.index, items));
    }

    let mut body = Vec::new();
    for (index, items) in &table {
        write_u32(&mut body, *index as u32);
        write_u32(&mut body, items.len() as u32);
        for item in items {
            write_u32(&mut body, *item);
        }
    }
    write_u32(&mut body, objects.len() as u32);
    for (value, realm) in &objects {
        write_record(agent, realm, externals, host, *value, &serials, &mut body)?;
    }

    let mut blob = Vec::with_capacity(HEADER_LEN + body.len() + MAGIC.len());
    blob.extend_from_slice(MAGIC);
    write_u32(&mut blob, FORMAT_VERSION);
    blob.push(std::mem::size_of::<usize>() as u8);
    blob.push(ENDIAN_LITTLE);
    blob.extend_from_slice(&[0, 0]);
    write_u32(&mut blob, body.len() as u32);
    write_u32(&mut blob, table.len() as u32);
    blob.extend_from_slice(&body);
    blob.extend_from_slice(MAGIC);
    Ok(blob)
}

/// Write one value, at slot 0's index 0, against an empty external-reference
/// table.
///
/// A convenience for a caller with one value to carry and no host pointers in
/// it; the table form is [`encode_slots`].
pub fn encode(agent: &Agent, realm: &Handle<Realm>, root: Value) -> Result<Vec<u8>, Unsupported> {
    let items = [root];
    encode_slots(
        agent,
        &[Slot {
            index: 0,
            realm: *realm,
            items: &items,
        }],
        &[],
        None,
    )
}

/// Read the data a blob carried for one context slot, made in `realm`.
///
/// Answers `None` when the blob names no such slot, which is a host asking for a
/// context its own build never recorded rather than a blob that is wrong.
/// Materializing in `realm` is what makes an intrinsic reference meaningful: the
/// name a blob carries resolves against the realm being restored into, not the
/// realm that wrote it. `externals` is the host's table, rebuilt for this load:
/// the index a blob wrote is resolved against it, and an index it does not have
/// is refused rather than read past the end.
///
/// The agent is mutable because carrying a function means re-running the parser
/// and registering a new body: a blob holds source text, not a compiled body.
/// `host` is the host's own callbacks, which is what a host callback record is
/// resolved through — a blob that names one read by a load that supplied none is
/// an error rather than a function that quietly cannot be called.
pub fn decode_slot(
    agent: &mut Agent,
    realm: &Handle<Realm>,
    bytes: &[u8],
    slot: usize,
    externals: &[usize],
    host: Option<&dyn HostCallbacks>,
) -> Result<Option<Vec<Value>>, DecodeError> {
    let (body_len, context_count) = header(bytes)?;
    let mut body = Reader::new(&bytes[HEADER_LEN..HEADER_LEN + body_len]);

    let mut wanted: Option<Vec<u32>> = None;
    for _ in 0..context_count {
        let index = body.u32().ok_or(DecodeError::Truncated)? as usize;
        let count = body.u32().ok_or(DecodeError::Truncated)? as usize;
        if count > body.bytes.len() {
            return Err(DecodeError::Truncated);
        }
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            items.push(body.u32().ok_or(DecodeError::Truncated)?);
        }
        if index == slot {
            wanted = Some(items);
        }
    }
    let Some(items) = wanted else {
        return Ok(None);
    };

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
        externals,
        host,
        records: &records,
        made: vec![None; records.len()],
        pins: Vec::new(),
    };
    let mut values = Vec::with_capacity(items.len());
    for serial in items {
        values.push(builder.materialize(serial)?);
    }
    Ok(Some(values))
}

/// Read a blob, answering the value at slot 0's index 0.
///
/// The single-value form of [`decode_slot`], matching [`encode`].
pub fn decode(
    agent: &mut Agent,
    realm: &Handle<Realm>,
    bytes: &[u8],
) -> Result<Value, DecodeError> {
    decode_slot(agent, realm, bytes, 0, &[], None)?
        .and_then(|items| items.first().copied())
        .ok_or(DecodeError::Truncated)
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

/// Read and check the header, answering the body's length and how many contexts
/// the table holds.
fn header(bytes: &[u8]) -> Result<(usize, usize), DecodeError> {
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
    let context_count = reader.u32().ok_or(DecodeError::Truncated)? as usize;

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
    Ok((body_len, context_count))
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
/// makes a graph a graph rather than a tree. Each record remembers the realm its
/// value was first reached through, because that is the realm whose intrinsics
/// recognize it — the name a reference is written as. `externals` is the host's
/// table, consulted for a host pointer: one the table does not have is refused
/// here, where the walk names what it cannot carry, rather than at emit time
/// where the record is already being written.
fn visit(
    agent: &Agent,
    realm: &Handle<Realm>,
    externals: &[usize],
    host: Option<&dyn HostCallbacks>,
    value: Value,
    objects: &mut Vec<(Value, Handle<Realm>)>,
    serials: &mut HashMap<Identity, u32>,
) -> Result<u32, Unsupported> {
    let value = canonical(value);
    let key = identity(realm, value);
    if let Some(serial) = serials.get(&key) {
        return Ok(*serial);
    }
    serials.insert(key, objects.len() as u32);
    let serial = objects.len() as u32;
    objects.push((value, *realm));

    match value.kind() {
        ValueKind::Object(object) => {
            if realm.intrinsics.name_of_value(&value).is_none() {
                if let ObjectKind::External(pointer) = &object.kind {
                    external_index(externals, *pointer)?;
                }
                for child in children(&object)? {
                    visit(agent, realm, externals, host, child, objects, serials)?;
                }
            }
        }
        ValueKind::Function(function) => {
            if realm.intrinsics.name_of_value(&value).is_none() {
                // A function this realm cannot name may be one *another* realm of
                // this isolate knows, which means the value was built in a
                // different context than the slot it is being written into. That
                // is a host's mistake rather than a missing feature and it has a
                // different fix, so it gets its own message rather than
                // surfacing as "a built-in function" three frames later.
                if named_by_another_realm(agent, realm, &value) {
                    return Err(Unsupported::new(
                        "a value from another realm",
                        "the value was built in another context of this isolate, so the realm it is being written into cannot name its builtins",
                    ));
                }
                // A bound function's own state is three language values, so the
                // walk reaches them the same way it reaches a property's: the
                // object part below is joined by the target, the bound `this`
                // and the bound arguments. A host callback's state is a pointer,
                // so what the walk owes it is the same table check a host
                // pointer's own record gets — a callback the build's table does
                // not hold is named here rather than written as a bad index.
                match callable(agent, &function, host)? {
                    Callable::Bound {
                        target,
                        bound_this,
                        bound_args,
                    } => {
                        visit(agent, realm, externals, host, target, objects, serials)?;
                        visit(agent, realm, externals, host, bound_this, objects, serials)?;
                        for argument in bound_args {
                            visit(agent, realm, externals, host, *argument, objects, serials)?;
                        }
                    }
                    Callable::HostCallback { pointer, data } => {
                        external_index(externals, pointer)?;
                        if let Some(data) = data {
                            visit(agent, realm, externals, host, data, objects, serials)?;
                        }
                    }
                    Callable::Body { .. } => {}
                }
                for child in children(&function.object)? {
                    visit(agent, realm, externals, host, child, objects, serials)?;
                }
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

/// The value a serial names, with a function's object part folded back into the
/// function it belongs to.
///
/// `Object.setPrototypeOf(x, f)` stores `f`'s *object side*, so a prototype link
/// can reach a function as an object — the engine keeps a back-reference on the
/// object part for exactly that reason. The two are one value in the language,
/// and writing the object side as the ordinary object its shape suggests would
/// restore a plain object where a function belongs, so the walk canonicalizes
/// it here: the one place that decides what a serial names.
fn canonical(value: Value) -> Value {
    if let ValueKind::Object(object) = value.kind()
        && let Some(function) = object.function_value()
    {
        return function;
    }
    value
}

/// The values a function is carried by, or the reason this format cannot carry
/// it.
///
/// A built-in that is not an intrinsic is a host callback, and in this engine a
/// host callback is a Rust closure rather than an address or a name a blob could
/// hold. An arrow, a method or an accessor has no standalone source text
/// (`Function.prototype.toString` already answers the native form for it). Both
/// are refused by the kind they are, because the fix differs: a callback wants
/// the external-reference table's `function` field, an arrow wants the scope it
/// closed over.
enum Callable<'a> {
    /// A JavaScript function with a body: which grammar its source is read in,
    /// the source itself, and its [[Strict]].
    Body {
        grammar: Grammar,
        source: &'a JsString,
        strict: bool,
    },
    /// A bind exotic: the target, the bound `this`, and the bound arguments.
    Bound {
        target: Value,
        bound_this: Value,
        bound_args: &'a [Value],
    },
    /// A host callback: the external-reference table pointer it was built from,
    /// and the data it reads when it is called.
    HostCallback { pointer: usize, data: Option<Value> },
}

/// Which grammar a function record's source is read in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Grammar {
    /// A function expression, parsed and instantiated in the reading realm.
    Function,
    /// A class expression: a class constructor's `[[SourceText]]`.
    Class,
    /// An arrow function's source, which is an expression rather than a form with
    /// a `function` keyword — so it is evaluated, not parsed.
    Arrow,
}

impl Grammar {
    fn byte(self) -> u8 {
        match self {
            Grammar::Function => GRAMMAR_FUNCTION,
            Grammar::Class => GRAMMAR_CLASS,
            Grammar::Arrow => GRAMMAR_ARROW,
        }
    }

    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            GRAMMAR_FUNCTION => Some(Grammar::Function),
            GRAMMAR_CLASS => Some(Grammar::Class),
            GRAMMAR_ARROW => Some(Grammar::Arrow),
            _ => None,
        }
    }

    /// How a message names this grammar's source text: `its class source …`.
    fn source_kind(self) -> &'static str {
        match self {
            Grammar::Function => "function",
            Grammar::Class => "class",
            Grammar::Arrow => "arrow",
        }
    }
}

/// What a function is carried by, or why it cannot be carried.
///
/// `host` is the host's own callbacks, when the build supplied them: a built-in
/// is a Rust closure, so the host is the only thing that can say which of its
/// table entries a function came from, and a build with no host refuses them by
/// name exactly as it did before the record existed.
fn callable<'a>(
    agent: &'a Agent,
    function: &'a Handle<Function>,
    host: Option<&dyn HostCallbacks>,
) -> Result<Callable<'a>, Unsupported> {
    match &function.kind {
        FunctionKind::Builtin { construct, .. } => {
            let callback = host.and_then(|host| host.callback_of(function));
            let Some(callback) = callback else {
                return Err(Unsupported::new(
                    "a built-in function",
                    "a host callback is a Rust closure, not a name or an address a snapshot can carry",
                ));
            };
            // The call half can come back because the host can make another;
            // the construct half cannot, because it is the *template* the host
            // built the function with — the instance it creates, the instance
            // template it applies, what a non-object return value means — and a
            // blob carries no template. Writing the call half of something whose
            // construct half would silently vanish is the "read back wrong" this
            // format refuses everywhere else.
            if construct.is_some() {
                return Err(Unsupported::new(
                    "a host constructor",
                    "its [[Construct]] is the template the host built it with, which a blob does not carry",
                ));
            }
            Ok(Callable::HostCallback {
                pointer: callback.pointer,
                data: callback.data.map(|data| *data.value()),
            })
        }
        FunctionKind::Bound {
            target,
            bound_this,
            bound_args,
        } => Ok(Callable::Bound {
            target: *target,
            bound_this: *bound_this,
            bound_args,
        }),
        FunctionKind::EcmaScript => {
            let data = agent
                .ecma_functions
                .get(&function.id())
                .ok_or(Unsupported::new(
                    "a function",
                    "its body is not registered on this agent",
                ))?;
            let Some(source) = data.source.as_ref() else {
                // No source is the primary fact, and it is checked before the
                // method form because a class constructor, an accessor and an
                // arrow all reach here with no source of their own — the engine
                // synthesizes their bodies and `Function.prototype.toString`
                // already answers the native form for them. Blaming the method
                // form for a class constructor would be a false diagnosis.
                let type_name = if data.is_class_constructor {
                    "a class constructor"
                } else {
                    "a function"
                };
                return Err(Unsupported::new(
                    type_name,
                    "the engine kept no source text for it",
                ));
            };
            let grammar = if data.is_class_constructor {
                // A class constructor's source is the class it came from, so the
                // class grammar reads it — checked **before** the method form,
                // because a class constructor is a method definition too and
                // that refusal would be a false diagnosis for it.
                Grammar::Class
            } else if data.is_method {
                return Err(Unsupported::new(
                    "a method",
                    "a method's source has no `function` keyword, so it cannot be re-parsed on its own",
                ));
            } else if data.this_mode == crate::function::ThisMode::Lexical {
                // An arrow: its source is an expression, not a form a function
                // parser can read, so it is carried the way a class is — evaluated
                // where it is being restored. `[[ThisMode]]` is what says so: it
                // is lexical for an arrow and for nothing else in this engine.
                Grammar::Arrow
            } else {
                Grammar::Function
            };
            Ok(Callable::Body {
                grammar,
                source,
                strict: data.strict,
            })
        }
    }
}

/// The index a host pointer has in the external-reference table.
///
/// The refusal names the pointer, not the index: a host that reads the message
/// has to add the pointer to its table, and it has no index to look up yet.
fn external_index(externals: &[usize], pointer: usize) -> Result<u32, Unsupported> {
    externals
        .iter()
        .position(|entry| *entry == pointer)
        .map(|index| index as u32)
        .ok_or(Unsupported::new(
            "a host pointer",
            "the pointer is not in the external-reference table",
        ))
}

/// Whether a realm other than the walking one knows this value as one of its
/// intrinsics.
fn named_by_another_realm(agent: &Agent, realm: &Handle<Realm>, value: &Value) -> bool {
    agent.realms.borrow().iter().any(|other| {
        !Handle::ptr_eq(*other, *realm) && other.intrinsics.name_of_value(value).is_some()
    })
}

/// Every value an object reaches: its prototype, its keys, and its values.
///
/// An Array's elements are its values too, and its `length` is neither an
/// element nor an ordinary property, so the two exotics are asked the question
/// their kind makes meaningful. A host pointer has no children: what it names is
/// the host's, written as an index into the table the host supplies. Every
/// *other* exotic kind is refused here rather than walked: its own state is not
/// in `properties`, so writing it as the ordinary object its shape would suggest
/// would produce an object that is not the one that was written — a proxy
/// without its traps, a typed array without its buffer, a `String` object
/// without its string.
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
        ObjectKind::External(_) => {}
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
    externals: &[usize],
    host: Option<&dyn HostCallbacks>,
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
        ValueKind::Function(function) => match realm.intrinsics.name_of_value(&value) {
            Some(name) => {
                body.push(REC_INTRINSIC);
                write_text(body, &name);
            }
            // The walk refuses a function it cannot carry before this, so
            // reaching here with one means the walk and the writer disagree
            // about a record rather than that a host's value is uncarried.
            None => match callable(agent, &function, host)? {
                Callable::Body {
                    grammar,
                    source,
                    strict,
                } => write_function(realm, &function, grammar, source, strict, serials, body)?,
                Callable::Bound { .. } => write_bound_function(realm, &function, serials, body)?,
                Callable::HostCallback { pointer, data } => {
                    write_host_callback(realm, &function, pointer, data, externals, serials, body)?
                }
            },
        },
        ValueKind::Object(object) => match realm.intrinsics.name_of_value(&value) {
            Some(name) => {
                body.push(REC_INTRINSIC);
                write_text(body, &name);
            }
            None if matches!(&object.kind, ObjectKind::Array(_)) => {
                write_array(realm, object, serials, body)?
            }
            None => {
                if let ObjectKind::External(pointer) = &object.kind {
                    body.push(REC_EXTERNAL);
                    write_u32(body, external_index(externals, *pointer)?);
                } else {
                    write_object(realm, object, serials, body)?;
                }
            }
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

/// Write a JavaScript function as the source text it can be rebuilt from, its
/// [[Strict]], and its object part — the same prototype/extensible/properties
/// triple an object gets, because a function's own keys are its own keys.
///
/// The source is what makes this the one record a restore re-runs the parser
/// for, and it is also the record's limit: what the source cannot say is the
/// [[Environment]] the function closed over, so a restored function resolves a
/// free name through the realm's global environment rather than through the
/// scope it was compiled in. A function whose body is not self-contained is
/// refused upstream.
fn write_function(
    realm: &Handle<Realm>,
    function: &Handle<Function>,
    grammar: Grammar,
    source: &JsString,
    strict: bool,
    serials: &HashMap<Identity, u32>,
    body: &mut Vec<u8>,
) -> Result<(), Unsupported> {
    body.push(REC_FUNCTION);
    body.push(grammar.byte());
    body.push(u8::from(strict));
    write_units(body, source.as_slice());
    write_u32(body, prototype_serial(realm, &function.object, serials)?);
    body.push(u8::from(function.object.extensible.get()));
    write_properties(realm, &function.object, serials, body)?;
    Ok(())
}

/// Write a host callback as the external-reference index it was built from and
/// the data it reads, plus the object part its `length`, `name` and a host's own
/// additions live on.
///
/// The index is the body: an address is a property of the process that loads a
/// blob rather than of the data, so what a load needs to make the call again is
/// which entry of *its* table the host put the same callback in. The data is a
/// language value all the same, so it is carried as one. That is also why the
/// record carries no `[[Construct]]` — see [`callable`].
fn write_host_callback(
    realm: &Handle<Realm>,
    function: &Handle<Function>,
    pointer: usize,
    data: Option<Value>,
    externals: &[usize],
    serials: &HashMap<Identity, u32>,
    body: &mut Vec<u8>,
) -> Result<(), Unsupported> {
    body.push(REC_HOST_CALLBACK);
    write_u32(body, external_index(externals, pointer)?);
    write_u32(
        body,
        match data {
            Some(value) => serial_in(realm, serials, value),
            None => NO_REF,
        },
    );
    write_u32(body, prototype_serial(realm, &function.object, serials)?);
    body.push(u8::from(function.object.extensible.get()));
    write_properties(realm, &function.object, serials, body)?;
    Ok(())
}

/// Write a bind exotic as the three values it is made of, plus the object part
/// its `length` and `name` live on.
///
/// The target is an ordinary value reference, so a chain of binds round trips
/// for as long as its innermost target is a function this format can carry, and
/// an intrinsic target is written by name like any other reference.
fn write_bound_function(
    realm: &Handle<Realm>,
    function: &Handle<Function>,
    serials: &HashMap<Identity, u32>,
    body: &mut Vec<u8>,
) -> Result<(), Unsupported> {
    // The walk refuses a bound function whose state it cannot carry before this,
    // so the match cannot miss; a miss would be a walk/writer disagreement.
    let FunctionKind::Bound {
        target,
        bound_this,
        bound_args,
    } = &function.kind
    else {
        return Err(Unsupported::new(
            "a function",
            "its kind is not the one this record is for",
        ));
    };
    body.push(REC_BOUND_FUNCTION);
    write_u32(body, serial_in(realm, serials, *target));
    write_u32(body, serial_in(realm, serials, *bound_this));
    write_u32(body, bound_args.len() as u32);
    for argument in bound_args {
        write_u32(body, serial_in(realm, serials, *argument));
    }
    write_u32(body, prototype_serial(realm, &function.object, serials)?);
    body.push(u8::from(function.object.extensible.get()));
    write_properties(realm, &function.object, serials, body)?;
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
        .get(&identity(realm, canonical(value)))
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
        REC_INTRINSIC => Ok(Record::Intrinsic(
            reader.text().ok_or(DecodeError::Truncated)?,
        )),
        REC_EXTERNAL => Ok(Record::External(
            reader.u32().ok_or(DecodeError::Truncated)?,
        )),
        REC_FUNCTION => {
            let byte = reader.u8().ok_or(DecodeError::Truncated)?;
            let grammar = Grammar::from_byte(byte).ok_or(DecodeError::BadGrammar(byte))?;
            let strict = reader.u8().ok_or(DecodeError::Truncated)? != 0;
            let source = reader.units().ok_or(DecodeError::Truncated)?;
            let proto = reader.u32().ok_or(DecodeError::Truncated)?;
            let extensible = reader.u8().ok_or(DecodeError::Truncated)? != 0;
            let properties = read_properties(reader)?;
            Ok(Record::Function {
                grammar,
                source,
                strict,
                proto,
                extensible,
                properties,
            })
        }
        REC_HOST_CALLBACK => {
            let index = reader.u32().ok_or(DecodeError::Truncated)?;
            let data = reader.u32().ok_or(DecodeError::Truncated)?;
            let proto = reader.u32().ok_or(DecodeError::Truncated)?;
            let extensible = reader.u8().ok_or(DecodeError::Truncated)? != 0;
            let properties = read_properties(reader)?;
            Ok(Record::HostCallback {
                index,
                data,
                proto,
                extensible,
                properties,
            })
        }
        REC_BOUND_FUNCTION => {
            let target = reader.u32().ok_or(DecodeError::Truncated)?;
            let bound_this = reader.u32().ok_or(DecodeError::Truncated)?;
            let count = reader.u32().ok_or(DecodeError::Truncated)? as usize;
            let mut bound_args = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                bound_args.push(reader.u32().ok_or(DecodeError::Truncated)?);
            }
            let proto = reader.u32().ok_or(DecodeError::Truncated)?;
            let extensible = reader.u8().ok_or(DecodeError::Truncated)? != 0;
            let properties = read_properties(reader)?;
            Ok(Record::BoundFunction {
                target,
                bound_this,
                bound_args,
                proto,
                extensible,
                properties,
            })
        }
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
    agent: &'a mut Agent,
    realm: &'a Handle<Realm>,
    /// The host's external-reference table, which a host pointer record is
    /// resolved against.
    externals: &'a [usize],
    /// The host's own callbacks, which is the only thing that can make one
    /// again: the body is the host's Rust closure, so the record names a table
    /// entry and the host answers the call it holds there.
    host: Option<&'a dyn HostCallbacks>,
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
    fn materialize(&mut self, serial: u32) -> Result<Value, DecodeError> {
        Ok(self.object(serial)?.value())
    }

    /// The value a serial names, made on first ask.
    fn object(&mut self, serial: u32) -> Result<Built, DecodeError> {
        if serial == NO_REF {
            return Err(DecodeError::Truncated);
        }
        let index = serial as usize;
        if let Some(made) = self.made.get(index).and_then(|made| made.as_ref()) {
            return Ok(*made);
        }
        let built = match self.records.get(index).ok_or(DecodeError::Truncated)? {
            Record::Undefined => Built::Value(Value::Undefined),
            Record::Null => Built::Value(Value::Null),
            Record::Boolean(value) => Built::Value(Value::Boolean(*value)),
            Record::Number(value) => Built::Value(Value::Number(*value)),
            Record::String(units) => {
                Built::Value(Value::String(Handle::new(JsString::from_utf16(units))))
            }
            Record::BigInt(text) => Built::Value(Value::BigInt(Handle::new(
                BigInt::parse_str(text, BIGINT_RADIX).ok_or(DecodeError::Truncated)?,
            ))),
            Record::SymbolWellKnown(name) => {
                let name = symbol::WELL_KNOWN_SYMBOLS
                    .get(*name as usize)
                    .ok_or(DecodeError::Truncated)?;
                Built::Value(Value::Symbol(symbol::well_known(name)))
            }
            Record::SymbolRegistry(key) => Built::Value(Value::Symbol(self.registry_symbol(key))),
            Record::Symbol(description) => {
                let description = description
                    .as_ref()
                    .map(|units| JsString::from_utf16(units));
                Built::Value(Value::Symbol(Handle::new(Symbol::new(description))))
            }
            Record::Intrinsic(name) => Built::Value(
                self.realm
                    .intrinsics
                    .get(name)
                    .ok_or_else(|| DecodeError::UnknownIntrinsic(name.clone()))?,
            ),
            Record::External(index) => {
                let pointer = *self.externals.get(*index as usize).ok_or(
                    DecodeError::ExternalIndexOutOfRange {
                        index: *index as usize,
                        count: self.externals.len(),
                    },
                )?;
                Built::Value(Value::Object(JsObject::external_object_create(
                    pointer, None,
                )))
            }
            Record::Object {
                proto,
                extensible,
                properties,
            } => {
                let prototype = self.prototype(*proto)?;
                let object = JsObject::ordinary_object_create(prototype);
                object.extensible.set(*extensible);
                // The shell is recorded before the properties are defined, so a
                // property that refers back to this object resolves to it
                // rather than restarting the build.
                self.remember(index, Built::Object(object));
                define_properties(self, object, properties)?;
                Built::Object(object)
            }
            Record::Array {
                proto,
                extensible,
                length,
                elements,
                extras,
            } => {
                let prototype = self.prototype(*proto)?;
                let array = JsObject::array_create(prototype, *length as f64)
                    .map_err(|_| DecodeError::Truncated)?;
                array.extensible.set(*extensible);
                self.remember(index, Built::Object(array));
                for (position, element) in elements.iter().enumerate() {
                    let value = self.materialize(*element)?;
                    array
                        .create_data_property_index(position as u64, value)
                        .map_err(|_| DecodeError::Truncated)?;
                }
                define_properties(self, array, extras)?;
                Built::Object(array)
            }
            Record::Function {
                grammar,
                source,
                strict,
                proto,
                extensible,
                properties,
            } => {
                let function = match grammar {
                    Grammar::Function => self.build_function(source, *strict, *proto)?,
                    Grammar::Class | Grammar::Arrow => {
                        self.build_evaluated_function(source, *proto, *grammar)?
                    }
                };
                // Recorded before the properties are defined, so an own
                // property that refers back to the function — `prototype`'s
                // `constructor`, for one — resolves to it rather than
                // restarting the build.
                self.remember(index, Built::Value(Value::Function(function)));
                let object = function.object;
                object.extensible.set(*extensible);
                define_properties(self, object, properties)?;
                // An own `prototype` in the record means the writing realm had
                // materialized the deferred one; clearing the flag keeps the
                // first later observation from making a second over it. With no
                // such property the flag stays set and the prototype
                // materializes lazily, exactly as the source function's would.
                if object
                    .get_own_property(&JsString::from_utf8("prototype"))
                    .ok()
                    .flatten()
                    .is_some()
                    && let Some(data) = self.agent.ecma_functions.get_mut(&function.id())
                {
                    data.prototype_pending = false;
                }
                Built::Value(Value::Function(function))
            }
            Record::BoundFunction {
                target,
                bound_this,
                bound_args,
                proto,
                extensible,
                properties,
            } => {
                let function =
                    self.build_bound_function(*target, *bound_this, bound_args, *proto)?;
                // A bind exotic has no registered body, so nothing defers its
                // `prototype`: the record's own properties are the whole object
                // part, and there is no lazy materialization to disarm.
                self.remember(index, Built::Value(Value::Function(function)));
                let object = function.object;
                object.extensible.set(*extensible);
                define_properties(self, object, properties)?;
                Built::Value(Value::Function(function))
            }
            Record::HostCallback {
                index: external,
                data,
                proto,
                extensible,
                properties,
            } => {
                let function = self.build_host_callback(*external, *data, *proto)?;
                // As for a bind exotic: a host callback has no registered body,
                // so nothing defers its `prototype` — the record's own
                // properties are the whole object part.
                self.remember(index, Built::Value(Value::Function(function)));
                let object = function.object;
                object.extensible.set(*extensible);
                define_properties(self, object, properties)?;
                Built::Value(Value::Function(function))
            }
        };
        if self.made[index].is_none() {
            self.remember(index, built);
        }
        Ok(built)
    }

    /// A host callback record: the load's external-reference table names the
    /// call, and the engine's own host-function path builds the function around
    /// it.
    ///
    /// The callback is the host's — this engine cannot make a Rust closure from
    /// an address — so what the record buys is direction: the index is resolved
    /// against the table the *loading* host rebuilt, which is what puts the
    /// address back where it belongs. `api::template::host_function` is the same
    /// constructor a materialized `FunctionTemplate` makes its functions
    /// through, so a restored callback's call view, return-value slot and
    /// pending-exception translation are the engine's existing ones rather than a
    /// second implementation of them.
    fn build_host_callback(
        &mut self,
        index: u32,
        data: u32,
        proto: u32,
    ) -> Result<Handle<Function>, DecodeError> {
        let pointer =
            *self
                .externals
                .get(index as usize)
                .ok_or(DecodeError::ExternalIndexOutOfRange {
                    index: index as usize,
                    count: self.externals.len(),
                })?;
        let data = match data {
            NO_REF => None,
            serial => Some(self.materialize(serial)?),
        };
        let callback = self
            .host
            .and_then(|host| host.callback_at(pointer, data.map(crate::api::Local)))
            .ok_or(DecodeError::NoHostCallback(index as usize))?;
        let prototype = self.prototype(proto)?;
        // SAFETY: `api::Isolate` is `repr(C)` with the agent at offset 0, so the
        // address of the agent is the address of the isolate it belongs to. The
        // callback view the host reads carries that pointer, and the host's
        // callback scope is built from it — the same identity
        // `api::Isolate::get_current` relies on.
        let isolate = self.agent as *mut Agent as *mut crate::api::Isolate;
        crate::api::host_function(isolate, std::rc::Rc::new(callback), None, prototype).map_err(
            |error| {
                DecodeError::UnrebuildableFunction(format!(
                    "the host could not make a function for external reference {index}: {error}"
                ))
            },
        )
    }

    /// An evaluated-function record: the source is evaluated as an expression in
    /// the realm being restored, and the function it produces is the value.
    ///
    /// A class constructor's `[[SourceText]]` is the class and an arrow's is the
    /// arrow — both complete expressions, and neither parseable by
    /// `parse_function`, which expects a `function` keyword. So both are rebuilt
    /// the way they were created: by evaluating them, which is what brings back
    /// what the engine's own creation path gives a closure — for a class its
    /// `[[ConstructorKind]]`, home object, prototype object, fields and private
    /// environment, for an arrow its lexical `[[ThisMode]]` and its deferred
    /// absence of a `prototype` — rather than an approximation assembled from a
    /// record.
    ///
    /// The divergences are the module docs': a class's definition-time code (a
    /// computed key, a `static {}` block, a static field initializer) runs again
    /// here, and the evaluation is the **reading** realm's, so an arrow's captured
    /// `this` and any free name are that realm's global rather than the scope the
    /// original closed over.
    fn build_evaluated_function(
        &mut self,
        source: &[u16],
        proto: u32,
        grammar: Grammar,
    ) -> Result<Handle<Function>, DecodeError> {
        let proto = self.prototype(proto)?.ok_or_else(|| {
            DecodeError::UnrebuildableFunction("the record names no prototype".into())
        })?;
        let kind = grammar.source_kind();
        let text = String::from_utf16(source).map_err(|_| {
            DecodeError::UnrebuildableFunction(format!("its {kind} source is not valid UTF-16"))
        })?;
        let realm = *self.realm;
        // A bootstrap execution context makes the realm current, so the source is
        // evaluated where the restore is materializing it, and it is wrapped in
        // parentheses so it reads as an expression: for a class that keeps the
        // class name bound inside the class rather than in the reading realm's
        // global scope, and for an arrow it is the parenthesis an arrow needs
        // wherever it appears. Popped on every path.
        self.agent.push_bootstrap_context(realm);
        let value = self.agent.run_script(&format!("({text})"));
        self.agent.execution_context_stack.pop();
        let value = value.map_err(|error| {
            DecodeError::UnrebuildableFunction(format!(
                "its {kind} source could not be evaluated: {error}"
            ))
        })?;
        let function = value.as_function().ok_or_else(|| {
            DecodeError::UnrebuildableFunction(format!(
                "its source did not evaluate to {}",
                match grammar {
                    Grammar::Class => "a class",
                    _ => "a function",
                }
            ))
        })?;
        // The evaluation sets the function's own prototype link from what it
        // derived; the record's is what the blob was written with, so it wins.
        function
            .object
            .set_prototype_of(Some(proto))
            .map_err(|error| {
                DecodeError::UnrebuildableFunction(format!(
                    "its prototype could not be set: {error}"
                ))
            })?;
        Ok(function)
    }

    /// A function record, rebuilt from the source text it carries.
    ///
    /// The parse is what makes this record different from every other one, and
    /// the realm's global environment is what it is instantiated in: a blob
    /// holds the value graph, not the scope a closure was compiled in, so a free
    /// name in the body resolves globally here. A source the parser cannot read
    /// as a function expression, or a function whose scope the writing realm
    /// had no source for, is refused by name rather than restored wrong.
    fn build_function(
        &mut self,
        source: &[u16],
        strict: bool,
        proto: u32,
    ) -> Result<Handle<Function>, DecodeError> {
        let proto = self.prototype(proto)?.ok_or_else(|| {
            DecodeError::UnrebuildableFunction("the record names no prototype".into())
        })?;
        let text = String::from_utf16(source).map_err(|_| {
            DecodeError::UnrebuildableFunction("its source is not valid UTF-16".into())
        })?;
        let parsed = match parser::parse_function(&text) {
            Ok(function) => function,
            // An `async function` source has no standalone expression entry in
            // `parse_function`, which expects the `function` keyword first, so
            // the async form is the one retry this needs.
            Err(plain) => parser::parse_function_with_async(&text, true).map_err(|_| {
                DecodeError::UnrebuildableFunction(format!(
                    "its source does not parse as a function expression: {plain}"
                ))
            })?,
        };
        let realm = *self.realm;
        // A bootstrap execution context makes the realm current and gives the
        // registration the running context it reads; it is popped on every
        // path, so a failed restore leaves the agent's stack as it found it.
        self.agent.push_bootstrap_context(realm);
        let value = crate::function::instantiate_function_from_source(
            self.agent,
            &parsed,
            realm.global_env,
            proto,
            Some(JsString::from_utf16(source)),
            strict,
        );
        self.agent.execution_context_stack.pop();
        let value = value.map_err(|error| {
            DecodeError::UnrebuildableFunction(format!(
                "its source could not be instantiated: {error}"
            ))
        })?;
        value.as_function().ok_or_else(|| {
            DecodeError::UnrebuildableFunction("its source did not evaluate to a function".into())
        })
    }

    /// A bind exotic record: its target, its bound `this` and its bound
    /// arguments, rebuilt through the crux constructor the `bind` builtin
    /// itself uses.
    ///
    /// Nothing is recomputed from the target, because nothing has to be: the
    /// bound function's `length` and `name` are own properties, and the record
    /// carries them like any other object part's.
    fn build_bound_function(
        &mut self,
        target: u32,
        bound_this: u32,
        bound_args: &[u32],
        proto: u32,
    ) -> Result<Handle<Function>, DecodeError> {
        let target = self.materialize(target)?;
        let bound_this = self.materialize(bound_this)?;
        let mut arguments = Vec::with_capacity(bound_args.len());
        for argument in bound_args {
            arguments.push(self.materialize(*argument)?);
        }
        let prototype = self.prototype(proto)?;
        Function::bound_function_create(target, bound_this, arguments, prototype).map_err(|error| {
            DecodeError::UnrebuildableFunction(format!("its target is not callable: {error}"))
        })
    }

    fn prototype(&mut self, serial: u32) -> Result<Option<Handle<JsObject>>, DecodeError> {
        if serial == NO_REF {
            return Ok(None);
        }
        Ok(match self.object(serial)? {
            Built::Object(object) => Some(object),
            // An intrinsic's record materializes as a value, not as a built
            // object, because a prototype is not always one: `%Object.prototype%`
            // arrives this way and is exactly the case the format exists for.
            // The engine's coercion, not `Value::as_object`, because
            // `%Function.prototype%` and the resumable kinds' prototypes are
            // callable and report no object side on the value's own accessor.
            Built::Value(value) => crate::context::as_object(&value),
        })
    }

    /// A `Symbol.for` symbol: the registry's own, made when the reading agent
    /// has not seen the key.
    fn registry_symbol(&self, key: &[u16]) -> Handle<Symbol> {
        let key = JsString::from_utf16(key);
        let mut registry = self.agent.global_symbol_registry.borrow_mut();
        if let Some((_, symbol)) = registry.iter().find(|(entry, _)| *entry == key) {
            return Handle::new(symbol.clone());
        }
        let symbol = Handle::new(Symbol::new(Some(key.clone())));
        registry.push((key, (*symbol).clone()));
        symbol
    }

    fn remember(&mut self, index: usize, built: Built) {
        let pin = match &built {
            Built::Value(value) => crux::heap::pin(*value),
            Built::Object(object) => crux::heap::pin_handle(*object),
        };
        self.pins.push(pin);
        self.made[index] = Some(built);
    }
}

/// Define a record's properties on an object, in the order the blob wrote them:
/// that order is the object's own enumeration order, so restoring it in place
/// restores what a host's `for...in` would see.
fn define_properties(
    builder: &mut Builder<'_>,
    object: Handle<JsObject>,
    properties: &[StoredProperty],
) -> Result<(), DecodeError> {
    for property in properties {
        let value = builder.materialize(property.key)?;
        let key = property_key(&value).ok_or(DecodeError::Truncated)?;
        let accessor = property.flags & FLAG_ACCESSOR != 0;
        let descriptor = PropertyDescriptor {
            value: if accessor {
                None
            } else {
                Some(builder.materialize(property.first)?)
            },
            writable: if accessor {
                None
            } else {
                Some(property.flags & FLAG_WRITABLE != 0)
            },
            get: if accessor {
                Some(builder.materialize(property.first)?)
            } else {
                None
            },
            set: if accessor {
                Some(builder.materialize(property.second)?)
            } else {
                None
            },
            enumerable: Some(property.flags & FLAG_ENUMERABLE != 0),
            configurable: Some(property.flags & FLAG_CONFIGURABLE != 0),
        };
        object
            .define_property_key(&key, &descriptor)
            .map_err(|_| DecodeError::Truncated)?;
    }
    Ok(())
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

    /// The reader's accessor. A restore that carries a function re-runs the
    /// parser and registers a new body, so it borrows the agent mutably — which
    /// the isolate's shared reference cannot express, the same reason the api's
    /// own `with_agent` builds its `&mut` from the raw pointer.
    #[allow(clippy::mut_from_ref)]
    fn agent_mut(isolate: &api::Isolate) -> &mut Agent {
        // SAFETY: as `agent_of`; no `&Agent` is live across a call that takes
        // this, because every call site is one statement long.
        unsafe { &mut *isolate.agent_ptr() }
    }

    fn encode_value(isolate: &api::Isolate, realm: &Handle<Realm>, value: Value) -> Vec<u8> {
        encode(agent_of(isolate), realm, value).expect("encode")
    }

    fn round_trip(isolate: &api::Isolate, realm: &Handle<Realm>, value: Value) -> Value {
        let blob = encode_value(isolate, realm, value);
        decode(agent_mut(isolate), realm, &blob).expect("decode")
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
    fn a_reference_that_names_no_record_is_refused() {
        let (isolate, realm) = fixture();
        let mut blob = encode_value(&isolate, &realm, Value::Number(1.0));
        assert!(decode(agent_mut(&isolate), &realm, &blob).is_ok());
        // The body is the context table — slot index, item count, item serial —
        // then the object count and the records. Pointing the item serial past
        // the table is a blob whose graph does not close, which is refused
        // rather than answered with a guess.
        let item = HEADER_LEN + 8;
        blob[item..item + 4].copy_from_slice(&9u32.to_le_bytes());
        assert!(decode(agent_mut(&isolate), &realm, &blob).is_err());
    }

    /// The context table is structural: two slots keep their own items, and a
    /// slot the blob does not name answers `None` rather than an empty list.
    #[test]
    fn two_slots_keep_their_own_items() {
        let (isolate, realm) = fixture();
        let first = [Value::Number(1.0), Value::Number(2.0)];
        let second = [Value::String(Handle::new(JsString::from_utf8("two")))];
        let slots = [
            Slot {
                index: 0,
                realm,
                items: &first,
            },
            Slot {
                index: 3,
                realm,
                items: &second,
            },
        ];
        let blob = encode_slots(agent_of(&isolate), &slots, &[], None).expect("a blob");

        let read = |slot| {
            decode_slot(agent_mut(&isolate), &realm, &blob, slot, &[], None)
                .expect("a blob of this tree")
        };
        let zero = read(0).expect("slot 0");
        assert_eq!(zero.len(), 2);
        assert_eq!(zero[1].as_number(), Some(2.0));
        let three = read(3).expect("slot 3");
        assert_eq!(three.len(), 1);
        assert_eq!(
            three[0].as_string().map(|text| text.to_string_lossy()),
            Some("two".to_string())
        );
        assert!(read(1).is_none(), "a slot the blob does not name");
    }

    /// Each slot is written against its own realm, so two realms' `%Object.prototype%`
    /// — two objects with one name — come back as the realm's own, and a slot's
    /// items keep the realm's intrinsics.
    #[test]
    fn each_slot_is_written_against_its_own_realm() {
        let mut isolate = api::Isolate::new();
        let first = api::Context::new(&mut isolate).expect("a realm");
        let second = api::Context::new(&mut isolate).expect("a second realm");
        let value = second
            .try_eval("({ n: 1 })")
            .expect("an object in the second realm")
            .into_value();
        let second_prototype = second
            .intrinsic("%Object.prototype%")
            .and_then(|value| value.as_object())
            .expect("the second realm's object prototype");
        let items = [value];
        let slots = [Slot {
            index: 1,
            realm: *second.realm(),
            items: &items,
        }];
        let blob = encode_slots(agent_of(&isolate), &slots, &[], None).expect("a blob");

        let back = decode_slot(agent_mut(&isolate), second.realm(), &blob, 1, &[], None)
            .expect("a blob of this tree")
            .expect("slot 1");
        assert_eq!(
            back[0]
                .as_object()
                .expect("an object")
                .get_prototype_of()
                .expect("its prototype")
                .map(|prototype| prototype.id()),
            Some(second_prototype.id()),
            "the object came back linked to its own realm's prototype"
        );
        let _ = first;
    }

    /// A value built in one realm, written into a slot named by another, is
    /// refused as such: its builtins belong to a realm the slot cannot name,
    /// which is a host's mistake about contexts rather than a missing feature.
    #[test]
    fn a_value_from_another_realm_is_refused_as_such() {
        let mut isolate = api::Isolate::new();
        let first = api::Context::new(&mut isolate).expect("a realm");
        let second = api::Context::new(&mut isolate).expect("a second realm");
        let value = second
            .try_eval("({ n: 1 })")
            .expect("an object in the second realm")
            .into_value();
        let items = [value];
        let slots = [Slot {
            index: 0,
            realm: *first.realm(),
            items: &items,
        }];
        let error = encode_slots(agent_of(&isolate), &slots, &[], None).expect_err("refused");
        assert_eq!(error.type_name, "a value from another realm");
    }

    /// A host pointer round trips through the host's table: the blob holds the
    /// index, and the address is the one the table has now — which is why the
    /// table is a compatibility surface rather than part of the data.
    #[test]
    fn a_host_pointer_round_trips_through_the_table() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let pointer = 0x5A5A_0000usize as *mut std::ffi::c_void;
        let other = 0x1234usize as *mut std::ffi::c_void;
        let external = api::External::new(&mut isolate, pointer)
            .expect("an external")
            .as_value()
            .into_value();
        // Nested rather than a root, so the record is reached through a property
        // as well as written as the root of its own slot.
        let object = JsObject::ordinary_object_create(None);
        object
            .create_data_property(&JsString::from_utf8("host"), external)
            .expect("define");
        let items = [Value::Object(object), external];
        let slots = [Slot {
            index: 0,
            realm,
            items: &items,
        }];
        let table = [0x9999usize, pointer as usize, other as usize];
        let blob = encode_slots(agent_of(&isolate), &slots, &table, None).expect("a blob");

        let back = decode_slot(agent_mut(&isolate), &realm, &blob, 0, &table, None)
            .expect("a blob of this tree")
            .expect("slot 0");
        let held = back[0].as_object().expect("an object");
        assert_eq!(
            api::External::from(back[1]).value(),
            pointer,
            "the item came back as the pointer the table has"
        );
        let nested = held
            .get_own_property(&JsString::from_utf8("host"))
            .expect("read")
            .and_then(|property| property.value())
            .expect("the property");
        assert_eq!(
            api::External::from(nested).value(),
            pointer,
            "and so did the one reached through a property"
        );
        let _ = context;
    }

    /// A pointer the host's table does not have is refused where the walk can
    /// name it, rather than written as an index nothing would resolve.
    #[test]
    fn a_host_pointer_not_in_the_table_is_refused_by_name() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let external = api::External::new(&mut isolate, 0x11usize as *mut std::ffi::c_void)
            .expect("an external")
            .as_value()
            .into_value();
        let items = [external];
        let slots = [Slot {
            index: 0,
            realm,
            items: &items,
        }];
        let error = encode_slots(agent_of(&isolate), &slots, &[], None).expect_err("refused");
        assert_eq!(error.type_name, "a host pointer");
        assert!(error.detail.contains("external-reference table"));
        let _ = context;
    }

    /// A blob names an index the table it is loaded against does not have, which
    /// is a read past the end rather than a value to answer with — and the index
    /// is what is resolved, not the position of the value in the graph.
    #[test]
    fn an_index_the_table_does_not_have_is_refused_at_restore() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let first = 0x1111usize;
        let pointer = 0x77usize;
        let other = 0x99usize;
        let external = api::External::new(&mut isolate, pointer as *mut std::ffi::c_void)
            .expect("an external")
            .as_value()
            .into_value();
        let items = [external];
        let slots = [Slot {
            index: 0,
            realm,
            items: &items,
        }];
        // The pointer sits at index 1 of a two-entry table, so an index that was
        // ignored would resolve to the wrong entry rather than to nothing.
        let blob =
            encode_slots(agent_of(&isolate), &slots, &[first, pointer], None).expect("a blob");

        assert_eq!(
            decode_slot(agent_mut(&isolate), &realm, &blob, 0, &[], None),
            Err(DecodeError::ExternalIndexOutOfRange { index: 1, count: 0 })
        );
        assert_eq!(
            decode_slot(agent_mut(&isolate), &realm, &blob, 0, &[first], None),
            Err(DecodeError::ExternalIndexOutOfRange { index: 1, count: 1 })
        );
        // And the host's table is what an index resolves against, entry by
        // entry, which is the contract: the blob carries the index, never the
        // address.
        let back = decode_slot(agent_mut(&isolate), &realm, &blob, 0, &[first, other], None)
            .expect("a blob of this tree")
            .expect("slot 0");
        assert_eq!(
            api::External::from(back[0]).value() as usize,
            other,
            "the address is the table's entry at that index, not the blob's"
        );
        let _ = context;
    }

    /// A host's own callbacks, as the engine sees them: which table entry a
    /// builtin came from, and the call an entry names. A host in a test is a Rust
    /// closure rather than an `extern "C"` pointer, which is all the engine's
    /// side of the contract asks for.
    struct FakeHost {
        pointers: HashMap<u64, usize>,
        asked: std::cell::RefCell<Vec<usize>>,
        /// What the restore handed each call: the data the blob carried.
        handed: std::cell::RefCell<Vec<Option<crate::api::Local>>>,
        answer: f64,
        /// The data the fake host attaches to every callback it made, when it
        /// attaches any: what a `FunctionBuilder::data` callback reads.
        data: Option<crate::api::Local>,
    }

    impl FakeHost {
        fn new(answer: f64) -> Self {
            Self {
                pointers: HashMap::new(),
                asked: std::cell::RefCell::new(Vec::new()),
                handed: std::cell::RefCell::new(Vec::new()),
                answer,
                data: None,
            }
        }

        /// The host made this function, and its table holds `pointer`.
        fn made(&mut self, function: &Handle<Function>, pointer: usize) {
            self.pointers.insert(function.id(), pointer);
        }
    }

    impl HostCallbacks for FakeHost {
        fn callback_of(&self, function: &Handle<Function>) -> Option<HostCallback> {
            let pointer = self.pointers.get(&function.id()).copied()?;
            Some(HostCallback {
                pointer,
                data: self.data,
            })
        }

        fn callback_at(
            &self,
            pointer: usize,
            data: Option<crate::api::Local>,
        ) -> Option<crate::api::FunctionCallback> {
            self.asked.borrow_mut().push(pointer);
            self.handed.borrow_mut().push(data);
            let answer = self.answer;
            Some(Box::new(
                move |info: &crate::api::FunctionCallbackInfo<'_>| {
                    info.get_return_value().set_number(answer);
                },
            ))
        }
    }

    /// A host callback is carried as the table entry the host built it from, and
    /// comes back callable — through the **load's** table, which is the whole
    /// point: the address belongs to the process, not to the blob.
    #[test]
    fn a_host_callback_round_trips_through_the_table() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let function_prototype = context
            .intrinsic("%Function.prototype%")
            .and_then(|value| crate::context::as_object(&value))
            .expect("the realm's function prototype");
        let callback = Function::create_builtin(
            Some(JsString::from_utf8("an_op")),
            0,
            Box::new(|_, _| Ok(Value::Number(1.0))),
            None,
            Some(function_prototype),
        )
        .expect("a builtin");
        let pointer = 0x1111usize;
        let other = 0x2222usize;
        let mut host = FakeHost::new(42.0);
        host.made(&callback, pointer);
        // A bind over the same callback, so the walk reaches one through a bind's
        // target as well as directly — the shape deno's first measurement was.
        let bound = Function::bound_function_create(
            Value::Function(callback),
            Value::Undefined,
            Vec::new(),
            None,
        )
        .expect("a bind");
        let items = [Value::Function(callback), Value::Function(bound)];
        let slots = [Slot {
            index: 0,
            realm,
            items: &items,
        }];
        // The pointer sits at index 1, so an index that was written or read
        // wrongly would resolve to the other entry rather than to nothing.
        let table = [other, pointer];
        let blob = encode_slots(agent_of(&isolate), &slots, &table, Some(&host)).expect("a blob");

        let back = decode_slot(agent_mut(&isolate), &realm, &blob, 0, &table, Some(&host))
            .expect("a blob of this tree")
            .expect("slot 0");
        assert_eq!(
            host.asked.borrow().as_slice(),
            &[pointer],
            "the restore asked the host for the entry's pointer"
        );
        assert_eq!(
            call_restored(&isolate, &realm, back[0], "").as_number(),
            Some(42.0),
            "the restored function calls the host's callback"
        );
        // The object part is the function's: what the host built it with comes
        // back as written, not recomputed.
        let name = back[0]
            .as_function()
            .expect("a function")
            .object
            .get_own_property(&JsString::from_utf8("name"))
            .expect("read")
            .and_then(|property| property.value())
            .and_then(|value| value.as_string())
            .map(|text| text.to_string_lossy());
        assert_eq!(name, Some("an_op".to_string()));
        assert_eq!(
            call_restored(&isolate, &realm, back[1], "").as_number(),
            Some(42.0),
            "a bind whose target is a host callback round trips too"
        );
        assert_eq!(
            back[0]
                .as_function()
                .expect("a function")
                .object
                .get_prototype_of()
                .expect("its prototype")
                .map(|prototype| prototype.id()),
            Some(function_prototype.id()),
            "the record's prototype is the function's [[Prototype]]"
        );
        let _ = context;
    }

    /// The data a host built a callback with rides with it. It is a language
    /// value, so the record carries it as one and the restore hands it back to
    /// the host's call — which is what deno's ops all need, because every op
    /// function is built with its own data.
    #[test]
    fn a_host_callbacks_data_rides_with_it() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let callback = Function::create_builtin(
            Some(JsString::from_utf8("an_op")),
            0,
            Box::new(|_, _| Ok(Value::Undefined)),
            None,
            None,
        )
        .expect("a builtin");
        let data = JsObject::ordinary_object_create(None);
        data.create_data_property(&JsString::from_utf8("marker"), Value::Number(9.0))
            .expect("define");
        let pointer = 0x1111usize;
        let mut host = FakeHost::new(0.0);
        host.made(&callback, pointer);
        host.data = Some(crate::api::Local(Value::Object(data)));
        let items = [Value::Function(callback)];
        let slots = [Slot {
            index: 0,
            realm,
            items: &items,
        }];
        let blob =
            encode_slots(agent_of(&isolate), &slots, &[pointer], Some(&host)).expect("a blob");

        let back = decode_slot(
            agent_mut(&isolate),
            &realm,
            &blob,
            0,
            &[pointer],
            Some(&host),
        )
        .expect("a blob of this tree")
        .expect("slot 0");
        back[0].as_function().expect("a function");
        let handed = host.handed.borrow();
        let handed = handed.last().expect("the host was asked for a call");
        let handed = handed
            .expect("the data came back")
            .value()
            .as_object()
            .expect("the data is an object again");
        assert_eq!(
            handed
                .get_own_property(&JsString::from_utf8("marker"))
                .expect("read")
                .and_then(|property| property.value())
                .and_then(|value| value.as_number()),
            Some(9.0),
            "the data the host attached is the data the restore handed back"
        );
        let _ = context;
    }

    /// A callback the host recognizes but the build's table does not hold is
    /// refused where the walk can name it, with the message a host pointer gets:
    /// the fix is the same one — put it in the table.
    #[test]
    fn a_host_callback_not_in_the_table_is_refused_by_name() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let callback = Function::create_builtin(
            Some(JsString::from_utf8("an_op")),
            0,
            Box::new(|_, _| Ok(Value::Undefined)),
            None,
            None,
        )
        .expect("a builtin");
        let mut host = FakeHost::new(0.0);
        host.made(&callback, 0x1111);
        let items = [Value::Function(callback)];
        let slots = [Slot {
            index: 0,
            realm,
            items: &items,
        }];

        let error =
            encode_slots(agent_of(&isolate), &slots, &[], Some(&host)).expect_err("refused");
        assert_eq!(error.type_name, "a host pointer");
        assert!(error.detail.contains("external-reference table"));
        let _ = context;
    }

    /// A host callback whose `[[Construct]]` the host built is refused rather
    /// than restored without one: a template is what makes a host constructor,
    /// and a blob carries no template.
    #[test]
    fn a_host_constructor_is_refused_rather_than_carried() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let callback = Function::create_builtin(
            Some(JsString::from_utf8("a_host_ctor")),
            1,
            Box::new(|_, _| Ok(Value::Undefined)),
            Some(Box::new(|_, _| Ok(Value::Undefined))),
            None,
        )
        .expect("a builtin");
        let mut host = FakeHost::new(0.0);
        host.made(&callback, 0x1111);
        let items = [Value::Function(callback)];
        let slots = [Slot {
            index: 0,
            realm,
            items: &items,
        }];

        let error =
            encode_slots(agent_of(&isolate), &slots, &[0x1111], Some(&host)).expect_err("refused");
        assert_eq!(error.type_name, "a host constructor");
        assert!(error.detail.contains("[[Construct]]"), "{}", error.detail);
        let _ = context;
    }

    /// A blob that names a host callback, read by a load that supplied none, is
    /// an error rather than a function that quietly cannot be called.
    #[test]
    fn a_host_callback_record_without_a_host_is_refused_at_restore() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let callback = Function::create_builtin(
            Some(JsString::from_utf8("an_op")),
            0,
            Box::new(|_, _| Ok(Value::Undefined)),
            None,
            None,
        )
        .expect("a builtin");
        let pointer = 0x1111usize;
        let other = 0x2222usize;
        let mut host = FakeHost::new(0.0);
        host.made(&callback, pointer);
        let items = [Value::Function(callback)];
        let slots = [Slot {
            index: 0,
            realm,
            items: &items,
        }];
        let table = [other, pointer];
        let blob = encode_slots(agent_of(&isolate), &slots, &table, Some(&host)).expect("a blob");

        assert_eq!(
            decode_slot(agent_mut(&isolate), &realm, &blob, 0, &table, None),
            Err(DecodeError::NoHostCallback(1)),
            "the record's index, not the value's position"
        );
        let _ = context;
    }

    #[test]
    fn a_foreign_blob_is_refused_by_each_of_its_fields() {
        let (isolate, realm) = fixture();
        let blob = encode_value(&isolate, &realm, Value::Number(1.0));
        let decode_it = |bytes: &[u8]| decode(agent_mut(&isolate), &realm, bytes);

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
        // The body is the context table (slot index, item count, one item) then
        // the object count, so the first record's tag follows those 16 bytes.
        bad_tag[HEADER_LEN + 16] = 200;
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
        let back = decode(agent_mut(&isolate), &realm, &blob).expect("decode");
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

    /// Install a restored value on a realm's global object and run a script with
    /// it there, which is the shape a host uses a value it read back in.
    fn run_restored(
        isolate: &api::Isolate,
        realm: &Handle<Realm>,
        value: Value,
        source: &str,
    ) -> Value {
        realm
            .global_object
            .create_data_property(&JsString::from_utf8("restored"), value)
            .expect("install");
        agent_mut(isolate).run_script(source).expect("run")
    }

    /// The same, calling `restored` with a literal argument list.
    fn call_restored(
        isolate: &api::Isolate,
        realm: &Handle<Realm>,
        function: Value,
        arguments: &str,
    ) -> Value {
        run_restored(isolate, realm, function, &format!("restored({arguments})"))
    }

    /// A JavaScript function is carried as the source text it can be re-parsed
    /// from, and the restore makes a callable value of it — which is the whole
    /// of what a host does with one.
    #[test]
    fn a_function_round_trips_and_is_callable() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let function = context
            .try_eval("(function addOne(a) { return a + 1; })")
            .expect("a function")
            .into_value();

        let back = round_trip(&isolate, &realm, function);
        let back = back.as_function().expect("a function");
        assert_eq!(
            call_restored(&isolate, &realm, Value::Function(back), "41").as_number(),
            Some(42.0)
        );
        // The function never had its `prototype` materialized, so the record
        // carries none and the restored body keeps the deferral: the first
        // observation still makes the spec's object, with its back-reference.
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                Value::Function(back),
                "Object.getOwnPropertyDescriptor(restored, 'prototype').value.constructor === restored",
            )
            .as_boolean(),
            Some(true),
            "a deferred prototype still materializes on a restore"
        );
    }

    /// A closure created inside a function the **host** calls from Rust keeps its
    /// own text, and is carried by a snapshot because of it.
    ///
    /// Nothing below that call is a script — the host is the caller — so the
    /// only text the body's spans can be resolved against is the callee's own
    /// frame. Without it `capture_source` finds nothing, the closure answers the
    /// native form for `toString`, and the walk refuses it as a function with no
    /// source: deno's `async_op_0` exactly, which is the shape this pins.
    #[test]
    fn a_closure_created_under_a_host_call_carries_its_source() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let factory = context
            .try_eval("(function () { return function stub() { return 41; }; })")
            .expect("a factory");
        let made = context
            .try_call(&factory, &api::Local::undefined(), &[])
            .expect("the host's call");
        assert_eq!(
            isolate.function_source(&made).as_deref(),
            Some("function stub() { return 41; }"),
            "the closure's own text, captured inside a body no script frame encloses"
        );

        // The same closure through the format: a function is written as the
        // source it can be re-parsed from, so one with no source is refused —
        // which is where deno's `create_blob` stopped.
        let back = round_trip(&isolate, &realm, made.into_value());
        let back = back.as_function().expect("a function");
        assert_eq!(
            call_restored(&isolate, &realm, Value::Function(back), "").as_number(),
            Some(41.0),
            "the restored closure runs the body its text re-parses to"
        );
    }

    /// A function's own properties are its object part's, so they ride with it:
    /// the name and length the engine computed, and anything a host put there.
    #[test]
    fn a_functions_own_properties_ride_with_it() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let function = context
            .try_eval("(function named(a, b) { return a; })")
            .expect("a function")
            .into_value();
        function
            .as_function()
            .expect("a function")
            .object
            .create_data_property(&JsString::from_utf8("tag"), Value::Number(7.0))
            .expect("define");

        let back = round_trip(&isolate, &realm, function);
        let back = back.as_function().expect("a function");
        let own = |name: &str| {
            back.object
                .get_own_property(&JsString::from_utf8(name))
                .expect("read")
                .and_then(|property| property.value())
                .unwrap_or_else(|| panic!("{name} is missing"))
        };
        assert_eq!(
            own("name").as_string().map(|text| text.to_string_lossy()),
            Some("named".to_string())
        );
        assert_eq!(own("length").as_number(), Some(2.0));
        assert_eq!(own("tag").as_number(), Some(7.0));
    }

    /// The restore instantiates in the **reading** realm's global environment,
    /// so a free name in the body resolves there — the one promise a blob can
    /// make, because it carries a value graph rather than the environment chain a
    /// closure closed over. The realms differ here so that the answer can only be
    /// the reading one's global.
    #[test]
    fn a_functions_free_names_resolve_through_the_reading_realms_globals() {
        let mut isolate = api::Isolate::new();
        let writer = api::Context::new(&mut isolate).expect("a realm");
        writer
            .try_eval("globalThis.offset = 10;")
            .expect("a global");
        let function = writer
            .try_eval("(function (a) { return a + offset; })")
            .expect("a function")
            .into_value();
        let blob = encode(agent_of(&isolate), writer.realm(), function).expect("a blob");

        let reader = api::Context::new(&mut isolate).expect("a second realm");
        reader
            .try_eval("globalThis.offset = 100;")
            .expect("a global");
        let items = decode_slot(agent_mut(&isolate), reader.realm(), &blob, 0, &[], None)
            .expect("a blob of this tree")
            .expect("slot 0");
        assert_eq!(
            call_restored(&isolate, reader.realm(), items[0], "5").as_number(),
            Some(105.0),
            "the free name resolved through the reading realm's global"
        );
    }

    /// A function whose `prototype` the writing realm had already materialized
    /// keeps it, and keeps one: the record carries the object, and the restored
    /// function must not make a second one beside it when an observation crosses
    /// the lazy-prototype barrier.
    #[test]
    fn a_materialized_prototype_survives_the_restore() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let function = context
            .try_eval("var C = function () {}; C.prototype.marker = 7; C")
            .expect("a function")
            .into_value();

        let back = round_trip(&isolate, &realm, function);
        // Two of the observations that materialize a deferred `prototype`, so a
        // flag left set would append a second one here.
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                back,
                "Object.getOwnPropertyDescriptor(restored, 'prototype').value.marker",
            )
            .as_number(),
            Some(7.0)
        );
        assert_eq!(
            run_restored(&isolate, &realm, back, "new restored().marker").as_number(),
            Some(7.0)
        );
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                back,
                "Object.getOwnPropertyNames(restored).filter((k) => k === 'prototype').length",
            )
            .as_number(),
            Some(1.0),
            "the record's prototype is the only one the restore left"
        );
    }

    /// Two references to one function come back as one function, because a
    /// function's identity is its serial like any other value's.
    #[test]
    fn a_shared_function_stays_one_function() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let function = context
            .try_eval("(function () { return 1; })")
            .expect("a function")
            .into_value();

        // Two items of one slot, rather than two elements of an array: an
        // array's elements are written from the serial map, which would answer
        // the same serial for both even if the walk had made two records.
        let items = [function, function];
        let slots = [Slot {
            index: 0,
            realm,
            items: &items,
        }];
        let blob = encode_slots(agent_of(&isolate), &slots, &[], None).expect("a blob");
        let back = decode_slot(agent_mut(&isolate), &realm, &blob, 0, &[], None)
            .expect("a blob of this tree")
            .expect("slot 0");
        assert_eq!(
            back[0]
                .as_function()
                .expect("the first item is a function")
                .id(),
            back[1]
                .as_function()
                .expect("the second item is a function")
                .id(),
            "the two references came back one function"
        );
    }

    /// `Object.setPrototypeOf(x, f)` stores a function's *object side*, so the
    /// walk can reach a function as an object — and it has to come back the
    /// function. Writing the object side as the ordinary object its shape
    /// suggests would restore a plain object where a function belongs.
    #[test]
    fn a_function_reached_as_a_prototype_comes_back_a_function() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let function = context
            .try_eval("(function f() { return 1; })")
            .expect("a function")
            .into_value();
        let holder = context.try_eval("({})").expect("an object").into_value();
        holder
            .as_object()
            .expect("an object")
            .set_prototype_of(Some(function.as_function().expect("a function").object))
            .expect("link");

        let pair = array_with(&realm, &[holder, function]);
        let back = round_trip(&isolate, &realm, pair);
        let back = back.as_object().expect("an array");
        let element = |index: &str| {
            back.get_own_property(&JsString::from_utf8(index))
                .expect("read")
                .and_then(|property| property.value())
                .expect("present")
        };
        let prototype = element("0")
            .as_object()
            .expect("the holder")
            .get_prototype_of()
            .expect("its prototype")
            .expect("a prototype");
        let function = element("1").as_function().expect("the function");
        assert_eq!(
            prototype
                .function_value()
                .and_then(|value| value.as_function())
                .map(|function| function.id()),
            Some(function.id()),
            "the prototype came back as the function, not as an object copy of it"
        );
    }

    /// A class constructor is carried by its class's `[[SourceText]]` — the
    /// spec's own slot for it — and the restore evaluates that text as a class
    /// expression, so the constructor it built comes back a constructor: it
    /// constructs, and a bare call still throws because `[[IsClassConstructor]]`
    /// came back with it.
    #[test]
    fn a_class_constructor_round_trips_and_constructs() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let value = context
            .try_eval("class C { x = 7; } C")
            .expect("a class")
            .into_value();

        let back = round_trip(&isolate, &realm, value);
        back.as_function().expect("a class constructor");
        assert_eq!(
            run_restored(&isolate, &realm, back, "new restored().x").as_number(),
            Some(7.0),
            "the restored class constructed, running its field initializer"
        );
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                back,
                "(() => { try { restored(); return false; } catch (e) { return e instanceof TypeError; } })()",
            )
            .as_boolean(),
            Some(true),
            "a bare call still throws, so [[IsClassConstructor]] came back"
        );
        // The record's source is the class's `[[SourceText]]`, so the restored
        // constructor answers the class source rather than the native form.
        assert_eq!(
            run_restored(&isolate, &realm, back, "restored.toString()")
                .as_string()
                .map(|text| text.to_string_lossy()),
            Some("class C { x = 7; }".to_string()),
            "the restore kept the class's source text"
        );
    }

    /// An arrow is carried as its own source text — an expression, so the restore
    /// evaluates it — and comes back callable, still without the `prototype` an
    /// arrow never has.
    #[test]
    fn an_arrow_round_trips_and_is_callable() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let value = context
            .try_eval("((a) => a + 1)")
            .expect("an arrow")
            .into_value();

        let back = round_trip(&isolate, &realm, value);
        back.as_function().expect("an arrow");
        assert_eq!(
            call_restored(&isolate, &realm, back, "41").as_number(),
            Some(42.0),
            "the restored arrow is callable"
        );
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                back,
                "Object.prototype.hasOwnProperty.call(restored, 'prototype') ? 1 : 0",
            )
            .as_number(),
            Some(0.0),
            "an arrow has no `prototype`, and the restore did not give it one"
        );
    }

    /// A fresh arrow answers its own source text. The spec sets `[[SourceText]]`
    /// for an arrow the same way it does for a class constructor — the text
    /// matched by the ArrowFunction production — and `Function.prototype.toString`
    /// answers it, where the engine answered the native form before the capture.
    #[test]
    fn an_arrow_answers_its_source() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let text = context
            .try_eval("((a) => a + 1).toString()")
            .expect("a string")
            .into_value();
        assert_eq!(
            text.as_string().map(|text| text.to_string_lossy()),
            Some("(a) => a + 1".to_string()),
            "the arrow answered its source, not the native form"
        );
    }

    /// An async arrow keeps its kind: the evaluation derives the same
    /// `[[Prototype]]` the record carries (%AsyncFunction.prototype%), and a call
    /// still answers a promise.
    #[test]
    fn an_async_arrow_round_trips_and_keeps_its_kind() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let value = context
            .try_eval("(async (a) => a)")
            .expect("an async arrow")
            .into_value();

        let back = round_trip(&isolate, &realm, value);
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                back,
                "Object.getPrototypeOf(restored) === Object.getPrototypeOf(async () => {}) ? 1 : 0",
            )
            .as_number(),
            Some(1.0),
            "the restored arrow kept the async function prototype"
        );
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                back,
                "restored(1) instanceof Promise ? 1 : 0"
            )
            .as_number(),
            Some(1.0),
            "and calling it still answers a promise"
        );
    }

    /// The class's private environment and its field initializer come back with
    /// it, because the restore re-runs the engine's own class evaluation: a
    /// public field that reads the class's own **private** field is the smallest
    /// observation that proves both were rebuilt.
    #[test]
    fn a_classes_private_environment_comes_back_with_it() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let value = context
            .try_eval("class C { #secret = 41; x = this.#secret + 1; } C")
            .expect("a class")
            .into_value();

        let back = round_trip(&isolate, &realm, value);
        assert_eq!(
            run_restored(&isolate, &realm, back, "new restored().x").as_number(),
            Some(42.0),
            "the private field and the initializer that reads it were both rebuilt"
        );
    }

    /// A class constructor's `[[SourceText]]` is the **class** text, which is
    /// what the spec requires `Function.prototype.toString` to answer when it is
    /// not empty — and which the engine stored nothing for before the record
    /// that carries one existed. This is the half a fresh class pins, with no
    /// snapshot in it.
    #[test]
    fn a_class_constructor_answers_the_class_source() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let text = context
            .try_eval("class C { x = 7; }\nC.toString()")
            .expect("a string")
            .into_value();
        assert_eq!(
            text.as_string().map(|text| text.to_string_lossy()),
            Some("class C { x = 7; }".to_string()),
            "the class constructor answered its class source, not the native form"
        );
    }

    /// A class defined in a body the engine has no *script* frame for still keeps
    /// its class source: `[[SourceText]]` is captured from the text the class's
    /// spans belong to, which is the `Function`-built body's own assembled
    /// string — carried by the frame that body runs under. So this class, which
    /// the walk used to refuse as source-less, round trips.
    #[test]
    fn a_class_from_a_dynamic_function_carries_its_class_source() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let value = context
            .try_eval("(new Function('return class C { x = 7; }'))()")
            .expect("a class")
            .into_value();
        let back = round_trip(&isolate, &realm, value);
        assert_eq!(
            run_restored(&isolate, &realm, back, "new restored().x").as_number(),
            Some(7.0),
            "the class source came from the body it was defined in"
        );
    }

    /// Each function the format cannot carry is refused by the kind it is: the
    /// fix differs, so the message does. The source-less case is now the
    /// **accessor** — a `get`/`set` body, for which the engine keeps no
    /// `[[SourceText]]` at all (its frame carries the text it was parsed from;
    /// its own text is method-form and is part of the method work) — and a
    /// class or arrow defined inside a `Function`-built body is *carried* now,
    /// because that body's frame carries the assembled text its spans belong to.
    #[test]
    fn an_uncarried_function_is_refused_by_kind() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        for (source, expected, detail) in [
            (
                "Object.getOwnPropertyDescriptor({ get x() { return 1; } }, 'x').get",
                "a function",
                "no source text",
            ),
            (
                "({ m() { return 1; } }).m",
                "a method",
                "`function` keyword",
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
            assert!(error.detail.contains(detail), "{source}: {}", error.detail);
        }
    }

    /// A host callback is a Rust closure in this engine rather than an address or
    /// a name, so it refuses as such — the one function kind no source can come
    /// back from.
    #[test]
    fn a_host_callback_is_refused_by_name() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let callback = Function::create_builtin(
            Some(JsString::from_utf8("an_op")),
            0,
            Box::new(|_, _| Ok(Value::Undefined)),
            None,
            None,
        )
        .expect("a builtin");

        let error = encode(agent_of(&isolate), &realm, Value::Function(callback))
            .expect_err("the walk refuses");
        assert_eq!(error.type_name, "a built-in function");
        assert!(error.detail.contains("Rust closure"), "{}", error.detail);
    }

    /// A bind exotic is carried by its target, its bound `this` and its bound
    /// arguments, and comes back callable — the value `deno_core`'s snapshot
    /// reaches through its module map, and the kind that stopped its build.
    #[test]
    fn a_bound_function_round_trips_and_is_callable() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let bound = context
            .try_eval("(function add(a, b) { return a + b; }).bind(null, 1)")
            .expect("a bound function")
            .into_value();

        let back = round_trip(&isolate, &realm, bound);
        let back = back.as_function().expect("a function");
        assert_eq!(
            call_restored(&isolate, &realm, Value::Function(back), "2").as_number(),
            Some(3.0),
            "the bound argument was still bound"
        );
        // `length` and `name` are the bound function's own properties, so they
        // come back as the `bind` builtin left them rather than being computed
        // again from the target.
        let own = |name: &str| {
            back.object
                .get_own_property(&JsString::from_utf8(name))
                .expect("read")
                .and_then(|property| property.value())
                .unwrap_or_else(|| panic!("{name} is missing"))
        };
        assert_eq!(own("length").as_number(), Some(1.0));
        assert_eq!(
            own("name").as_string().map(|text| text.to_string_lossy()),
            Some("bound add".to_string())
        );
    }

    /// The bound `this` and the bound arguments are values in the graph like any
    /// other, so an object bound as a receiver comes back the object.
    #[test]
    fn a_bound_this_and_bound_arguments_ride_with_it() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let bound = context
            .try_eval(
                "var receiver = { tag: 7 }; var argument = { n: 5 }; \
                 (function (a, b) { return this.tag + a.n + b; }).bind(receiver, argument)",
            )
            .expect("a bound function")
            .into_value();

        let back = round_trip(&isolate, &realm, bound);
        assert_eq!(
            call_restored(&isolate, &realm, back, "2").as_number(),
            Some(14.0),
            "the bound receiver and the bound argument came back"
        );
    }

    /// A bind's target is an ordinary value reference, so a chain of binds
    /// round trips — and reached through an object, which is the shape the value
    /// arrives in from a host's own graph.
    #[test]
    fn a_chain_of_bound_functions_round_trips() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let holder = context
            .try_eval(
                "var f = function (a, b, c) { return a + b + c; }; \
                 ({ nested: f.bind(null, 1).bind(null, 2) })",
            )
            .expect("an object")
            .into_value();

        let back = round_trip(&isolate, &realm, holder);
        let nested = back
            .as_object()
            .expect("an object")
            .get_own_property(&JsString::from_utf8("nested"))
            .expect("read")
            .and_then(|property| property.value())
            .expect("the property");
        assert!(
            nested.as_function().is_some(),
            "the nested value came back a function"
        );
        assert_eq!(
            call_restored(&isolate, &realm, nested, "3").as_number(),
            Some(6.0),
            "both binds were still bound"
        );
    }

    /// The realm's global function properties are intrinsics the spec names
    /// (`%isFinite%`, `%parseInt%`, ...), so a reference to one is written by
    /// name rather than refused as a host callback or carried as a copy — and
    /// a restore resolves the name against the realm it is rebuilding.
    #[test]
    fn a_global_function_property_is_written_by_name() {
        let mut isolate = api::Isolate::new();
        // The reading realm is made first, because the api makes the *last*
        // context created the current one and the evals below have to run in
        // the writer's.
        let reader = api::Context::new(&mut isolate).expect("a realm to restore into");
        let writer = api::Context::new(&mut isolate).expect("the realm the value comes from");
        for name in [
            "isFinite",
            "isNaN",
            "parseFloat",
            "parseInt",
            "encodeURI",
            "encodeURIComponent",
            "decodeURI",
            "decodeURIComponent",
            "escape",
            "unescape",
        ] {
            let value = writer
                .try_eval(name)
                .expect("the global function")
                .into_value();
            let blob = encode(agent_of(&isolate), writer.realm(), value).expect("carried by name");
            assert!(
                blob.windows(name.len())
                    .any(|window| window == name.as_bytes()),
                "{name} is in the blob by name, not by structure"
            );
            let back = decode_slot(agent_mut(&isolate), reader.realm(), &blob, 0, &[], None)
                .expect("a blob of this tree")
                .expect("slot 0");
            // The reading realm's own, by its intrinsic table rather than by an
            // eval: the current realm is still the writer's.
            let own = reader
                .intrinsic(&format!("%{name}%"))
                .expect("the reading realm registered it");
            assert_eq!(
                back[0].as_function().map(|function| function.id()),
                own.as_function().map(|function| function.id()),
                "{name} came back the reading realm's own function"
            );
            assert_ne!(
                back[0].as_function().map(|function| function.id()),
                value.as_function().map(|function| function.id()),
                "{name} came back a copy of the writer's"
            );
        }
    }

    /// A bind whose target is a host callback is refused by what cannot be
    /// carried — the target — rather than by the bind.
    #[test]
    fn a_bound_function_over_a_host_callback_is_refused_by_name() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let callback = Function::create_builtin(
            Some(JsString::from_utf8("an_op")),
            0,
            Box::new(|_, _| Ok(Value::Undefined)),
            None,
            None,
        )
        .expect("a builtin");
        let bound = Function::bound_function_create(
            Value::Function(callback),
            Value::Null,
            Vec::new(),
            None,
        )
        .expect("a bound function");

        let error = encode(agent_of(&isolate), &realm, Value::Function(bound))
            .expect_err("the walk refuses");
        assert_eq!(error.type_name, "a built-in function");
    }
}
