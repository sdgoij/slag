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
//! the value it refused: a proxy, a module namespace, a host object, an indexed
//! accessor, an array whose `length` is not an index, a `DataView`, a shared or
//! detached buffer, a host pointer the host's external-reference table does not
//! have, a host callback (a built-in that is not an intrinsic) when the host
//! supplies none of its own, a function
//! the engine kept no source text for — a class constructor defined where no
//! frame carries the text its span belongs to, for one — and an environment
//! record a function closed over that is neither declarative nor function: a
//! lookup walks a record's own binding list and nothing else, so an object,
//! module or global record is refused by kind. A **method**, an **accessor**, a
//! **typed array** and the **buffer** it views are no longer on that list: each
//! carries what it is made of now.
//!
//! An Array's element that is **absent** — a hole — travels as the absence it
//! is, the serial `NO_REF` in the element list. So `1 in [1, , 3]` is still
//! false of a restored array where `1 in [1, undefined, 3]` is still true, and
//! an array whose `length` is longer than its elements stays that way.
//! Each entry is a subsystem to carry, and the walk refuses rather than writing
//! something a restore would read back wrong.
//!
//! # Functions
//!
//! A JavaScript function is written as the source text it can be re-parsed
//! from, its [[Strict]], the environment chain it closed over, and its object
//! part, and the restore re-parses that source and instantiates it in that
//! chain. A bind
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
//! # The environment chain
//!
//! A closure's free names are resolved through the environment records it
//! closed over, so those records travel with it. `REC_ENV` carries a
//! **declarative** or **function** record — the outer link and the bindings —
//! and, for a function record, the `this` value and this-binding status that
//! decide whether the record has a `this` binding. What does *not* travel is a
//! record's `[[FunctionObject]]` and its new-target: only an arrow reads those
//! through a chain, and an arrow is rebuilt by **evaluating** its source in the
//! reading realm rather than instantiated in a carried one, so nothing a
//! restored chain is consulted for would read them. The record carries no
//! declaration-time *behaviour* (no Annex B hoist list, no disposable-resource
//! stack, no catch-parameter mark): those are read while a declaration is
//! instantiated, which a restore does not do — it only puts the bindings back.
//!
//! The realm's own global environment writes no record at all (`NO_REF`):
//! `Context::new` rebuilds that one, so the chain ends there and a name it
//! cannot resolve resolves globally, exactly as the chain it came from did. The
//! names a script declared *into* that record — a top-level `let`, `const` or
//! `class` — are the realm's own state rather than a closure's, so they travel
//! beside the carried global rather than as an environment record.
//!
//! Version 1, and the format has not been released: no blob exists outside this
//! tree, so the version stays 1 while the *shape* moves — the binding list this
//! format carries beside a realm's global was added to a context's entry in the
//! table without bumping it, because there is no released reader for the change
//! to be distinguishable from. The version moves the day a released reader would
//! misread what a released writer wrote. The format is ours to version, and its
//! compatibility surface is the *names* it writes: an intrinsic name and a
//! well-known symbol name have to mean the same thing in the tree that reads a
//! blob as in the tree that wrote it. A blob therefore carries the format version
//! and the word size it was written with, and a reader refuses any other.
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
use crux::typed_array::ElementType;
use crux::value::{Value, ValueKind};

use crate::agent::Agent;
use crate::builtins::array_buffer::BufferState;
use crate::env::{Binding, DeclarativeEnv, EnvRecord, EnvRef, FunctionEnv, ThisBindingStatus};
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
const REC_CLASS_METHOD: u8 = 17;
const REC_MODULE: u8 = 18;
const REC_ENV: u8 = 19;
const REC_ARRAY_BUFFER: u8 = 20;
const REC_TYPED_ARRAY: u8 = 21;

/// An array buffer's own flags, in the byte its record carries: whether it is
/// resizable (a `maxByteLength` was given) and whether it is immutable
/// (`transferToImmutable`). A shared or detached buffer is refused by the walk
/// rather than flagged, because neither can be rebuilt as what it was.
const BUFFER_RESIZABLE: u8 = 1;
const BUFFER_IMMUTABLE: u8 = 2;

/// Which environment record an `REC_ENV` record is. Only these two are
/// carried; an object, module or global record is refused by [`visit_env`].
const ENV_DECLARATIVE: u8 = 0;
const ENV_FUNCTION: u8 = 1;

/// The environment's own flags: whether the record is transparent to the
/// static context-chain walk (a per-iteration copy or a named function
/// expression's self-binding scope), which a certified body's capture read
/// skips.
const ENV_FLAG_TRANSPARENT: u8 = 1;

/// A binding's attributes, in the byte a binding carries. These are the
/// binding's own bits, not a property's: the two encodings have the same
/// shape (a flags byte between two serials) and different meanings.
const BIND_MUTABLE: u8 = 1;
const BIND_STRICT: u8 = 2;
const BIND_DELETABLE: u8 = 4;
const BIND_PARAMETER: u8 = 8;

/// A function environment's [[ThisBindingStatus]] (spec 9.2.4), in the byte a
/// function environment carries.
const THIS_LEXICAL: u8 = 0;
const THIS_UNINITIALIZED: u8 = 1;
const THIS_INITIALIZED: u8 = 2;

/// Which grammar a function record's source is read in. A class constructor's
/// `[[SourceText]]` is the **class** it came from (spec 15.7.14 sets it that way
/// for an implicit constructor and an explicit one alike), and an arrow's is the
/// **expression** it was written as (spec 15.3.3), so the record says
/// which grammar its source is in rather than leaving the reader to guess from
/// the text.
const GRAMMAR_FUNCTION: u8 = 0;
const GRAMMAR_CLASS: u8 = 1;
const GRAMMAR_ARROW: u8 = 2;
const GRAMMAR_METHOD: u8 = 3;

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
    /// An environment record this version cannot rebuild: a kind byte that is
    /// not one of this format's, a this-binding status that is not one, or a
    /// function record whose grammar the format does not give an environment.
    UnrebuildableEnvironment(String),
    /// An ArrayBuffer or typed array record this version cannot rebuild: an
    /// element-type name no kind has, a view whose built geometry does not match
    /// the numbers the record carries, or a buffer the engine will not allocate
    /// the way the record describes.
    UnrebuildableBuffer(String),
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
            Self::UnrebuildableEnvironment(reason) => {
                write!(f, "snapshot environment could not be rebuilt: {reason}")
            }
            Self::UnrebuildableBuffer(reason) => {
                write!(f, "snapshot buffer could not be rebuilt: {reason}")
            }
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
    Module(u64),
    Env(u64),
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
    /// is strict, the environment chain it closed over, its [[HomeObject]] when
    /// the grammar is a method, and its object part.
    Function {
        grammar: Grammar,
        source: Vec<u16>,
        strict: bool,
        environment: u32,
        home: u32,
        heritage: u32,
        keys: Vec<u32>,
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
    /// A method that reads a private name, carried as a member of its class: the
    /// class, the member's key, which of the three forms it is, its
    /// [[HomeObject]], and its object part. No source — the class the restore
    /// evaluates already holds the method, under the private name's own brand.
    ClassMethod {
        class: u32,
        key: u32,
        form: Form,
        home: u32,
        proto: u32,
        extensible: bool,
        properties: Vec<StoredProperty>,
    },
    /// A module record: the name it was compiled under, when it had one, and the
    /// source text it is compiled from.
    Module {
        name: Option<Vec<u16>>,
        source: Vec<u16>,
    },
    /// An environment record a closure closed over: which of the two kinds it
    /// is, whether a capture read skips it, its outer link, its bindings, and —
    /// for a function record — the `this` value and this-binding status that
    /// decide whether the record has a `this` binding at all.
    Env {
        kind: u8,
        transparent: bool,
        outer: u32,
        bindings: Vec<CarriedBinding>,
        this_value: u32,
        this_status: u8,
    },
    /// An ArrayBuffer: its geometry, its own flags, the bytes it holds, and its
    /// object part.
    ArrayBuffer {
        byte_length: u32,
        resizable: bool,
        immutable: bool,
        max_byte_length: u32,
        bytes: Vec<u8>,
        proto: u32,
        extensible: bool,
        properties: Vec<StoredProperty>,
    },
    /// A typed array: which of the twelve kinds it is, the buffer it views, its
    /// geometry, and its object part.
    TypedArray {
        element: ElementType,
        buffer: u32,
        byte_offset: u32,
        byte_length: u32,
        array_length: u32,
        auto_length: bool,
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

/// A binding as read from a blob: its name, its attributes, and its value (a
/// serial, or absent while the binding is uninitialized).
///
/// A binding is written in a property's four-field shape — a name serial, a
/// flags byte, a value serial and one reserved — so that the reader above, which
/// already consumes exactly those, is the one that reads it. The reserved field
/// is left to that reader.
#[derive(Clone, Copy)]
struct CarriedBinding {
    name: u32,
    flags: u8,
    value: u32,
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

    /// Told that `function` was rebuilt from the table entry at `pointer`, with
    /// `data` — the write side's counterpart to
    /// [`callback_of`](Self::callback_of).
    ///
    /// A load that can be written again has to be able to say what it restored.
    /// The function is the load's rather than the host's template's, so being
    /// told here is the only thing that makes it identifiable to a later
    /// `callback_of`; a host that never writes a second blob has no use for it,
    /// which is why the default does nothing.
    fn callback_rebuilt(
        &self,
        _function: &Handle<Function>,
        _pointer: usize,
        _data: Option<crate::api::Local>,
    ) {
    }
}

/// One context's slot in a blob: the index the host gave it, the realm its
/// values were built in, and the items themselves.
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
    /// The realm the slot's items belong to.
    pub realm: Handle<Realm>,
    /// The items the host attached to that context, in the order it attached
    /// them: the indices a restore hands them back under.
    pub items: &'a [SnapshotItem],
    /// Whether the slot also carries the realm's **global state** — its global
    /// object *and* the lexical bindings declared into the global environment
    /// record — rather than only the values a host attached, so a restore puts
    /// back what the writing realm installed. A host that wants its own names
    /// (`Deno`, an op table, a bootstrap's helpers) to survive a restart sets
    /// this; `encode`/`decode` do not, so the single-value form is unchanged.
    pub realm_global: bool,
}

/// What a slot holds: an ordinary value, or a module record.
///
/// A module is not a language value — the host-facing handle type says so, with a
/// payload variant of its own — but a host attaches one as context data all the
/// same (the host this format was built for attaches the module records it
/// stores), and a snapshot that simply skipped it would hand every **later**
/// index back under the wrong number. So an item carries either, and the record
/// for a module is what the engine's own compile path rebuilds one from: its
/// source and its name.
#[derive(Clone, Copy)]
pub enum SnapshotItem {
    Value(Value),
    Module(crate::api::Module),
}

impl SnapshotItem {
    /// The value this item is, or `None` when it is a module — which is not a
    /// language value, so there is nothing to hand a caller that asked for one.
    pub fn value(self) -> Option<Value> {
        match self {
            SnapshotItem::Value(value) => Some(value),
            SnapshotItem::Module(_) => None,
        }
    }
}

impl From<Value> for SnapshotItem {
    fn from(value: Value) -> Self {
        SnapshotItem::Value(value)
    }
}

/// What the walk carries for one serial: the value and the realm it belongs to,
/// a module, or an environment record and the realm whose global ends its chain.
#[derive(Clone, Copy)]
enum Entry {
    Value(Value, Handle<Realm>),
    Module(Handle<crate::module::SourceTextModule>),
    Env(EnvRef, Handle<Realm>),
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
    let mut objects: Vec<Entry> = Vec::new();
    let mut serials: HashMap<Identity, u32> = HashMap::new();
    let mut table: Vec<(usize, u32, Vec<u32>, Vec<CarriedBinding>)> =
        Vec::with_capacity(slots.len());
    for slot in slots {
        let mut items = Vec::with_capacity(slot.items.len());
        for item in slot.items {
            items.push(match item {
                SnapshotItem::Value(value) => visit(
                    agent,
                    &slot.realm,
                    externals,
                    host,
                    *value,
                    &mut objects,
                    &mut serials,
                )?,
                SnapshotItem::Module(module) => {
                    visit_module(module.handle(), &mut objects, &mut serials)
                }
            });
        }
        // The realm's global is visited after its items so a value a host
        // attached and a property of the global are **one** record when they are
        // one object: identity is the serial map, so whichever the walk reaches
        // first fixes the serial both are written under.
        let global = if slot.realm_global {
            visit(
                agent,
                &slot.realm,
                externals,
                host,
                Value::Object(slot.realm.global_object),
                &mut objects,
                &mut serials,
            )?
        } else {
            NO_REF
        };
        // The realm's lexical bindings travel with its global, and only a slot
        // that asked for the global is asked for them: they are part of the same
        // "the realm's own state" the flag turns on.
        let mut bindings = Vec::new();
        if slot.realm_global {
            for (name, binding) in carried_global_bindings(&slot.realm)? {
                let name = visit(
                    agent,
                    &slot.realm,
                    externals,
                    host,
                    Value::String(Handle::new(name)),
                    &mut objects,
                    &mut serials,
                )?;
                let value = match &binding.value {
                    Some(value) => visit(
                        agent,
                        &slot.realm,
                        externals,
                        host,
                        *value,
                        &mut objects,
                        &mut serials,
                    )?,
                    None => NO_REF,
                };
                bindings.push(CarriedBinding {
                    name,
                    flags: binding_flags(&binding),
                    value,
                });
            }
        }
        table.push((slot.index, global, items, bindings));
    }

    let mut body = Vec::new();
    for (index, global, items, bindings) in &table {
        write_u32(&mut body, *index as u32);
        write_u32(&mut body, items.len() as u32);
        write_u32(&mut body, *global);
        for item in items {
            write_u32(&mut body, *item);
        }
        write_bindings(&mut body, bindings);
    }
    write_u32(&mut body, objects.len() as u32);
    for entry in &objects {
        write_carried(agent, externals, host, entry, &serials, &mut body)?;
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
    let items = [SnapshotItem::Value(root)];
    encode_slots(
        agent,
        &[Slot {
            index: 0,
            realm: *realm,
            items: &items,
            realm_global: false,
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
) -> Result<Option<Vec<SnapshotItem>>, DecodeError> {
    let (body_len, context_count) = header(bytes)?;
    let mut body = Reader::new(&bytes[HEADER_LEN..HEADER_LEN + body_len]);

    let mut wanted: Option<(u32, Vec<u32>, Vec<CarriedBinding>)> = None;
    for _ in 0..context_count {
        let index = body.u32().ok_or(DecodeError::Truncated)? as usize;
        let count = body.u32().ok_or(DecodeError::Truncated)? as usize;
        if count > body.bytes.len() {
            return Err(DecodeError::Truncated);
        }
        let global = body.u32().ok_or(DecodeError::Truncated)?;
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            items.push(body.u32().ok_or(DecodeError::Truncated)?);
        }
        // Read for every row, not only the wanted one: the binding list is part
        // of a row, so skipping it would leave the reader at the wrong offset
        // for the next context.
        let bindings = read_bindings(&mut body)?;
        if index == slot {
            wanted = Some((global, items, bindings));
        }
    }
    let Some((global, items, bindings)) = wanted else {
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
        class_prototypes: HashMap::new(),
        unfilled_envs: Vec::new(),
    };
    let mut values = Vec::with_capacity(items.len());
    // The realm's own state first: a host's item that the global also holds is
    // then the very object the global was given, because the builder caches by
    // serial either way.
    if global != NO_REF {
        builder.apply_realm_global(global)?;
    }
    if !bindings.is_empty() {
        builder.apply_realm_bindings(&bindings)?;
    }
    for serial in items {
        values.push(builder.materialize_item(serial)?);
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
    match decode_slot(agent, realm, bytes, 0, &[], None)?.and_then(|items| items.first().copied()) {
        Some(SnapshotItem::Value(value)) => Ok(value),
        // A blob whose slot 0 holds a module is not one this single-value form can
        // answer; a host that attached a module reads it through its own slot.
        _ => Err(DecodeError::Truncated),
    }
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
    objects: &mut Vec<Entry>,
    serials: &mut HashMap<Identity, u32>,
) -> Result<u32, Unsupported> {
    let value = canonical(value);
    let key = identity(realm, value);
    if let Some(serial) = serials.get(&key) {
        return Ok(*serial);
    }
    serials.insert(key, objects.len() as u32);
    let serial = objects.len() as u32;
    objects.push(Entry::Value(value, *realm));

    match value.kind() {
        ValueKind::Object(object) => {
            if realm.intrinsics.name_of_value(&value).is_none() {
                if let ObjectKind::External(pointer) = &object.kind {
                    external_index(externals, *pointer)?;
                }
                for child in children(agent, &object)? {
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
                    Callable::Body {
                        home, environment, ..
                    } => {
                        // The chain a closure's free names resolve through, so
                        // the walk owes it every record it is made of — a
                        // binding's name and value, each outer, and a function
                        // record's `this`.
                        if let Some(environment) = environment {
                            visit_env(
                                agent,
                                realm,
                                externals,
                                host,
                                environment,
                                objects,
                                serials,
                            )?;
                        }
                        if let Some(home) = home {
                            visit(agent, realm, externals, host, home, objects, serials)?;
                        }
                    }
                    Callable::Class { heritage, keys, .. } => {
                        // Both are values the record refers to by serial, so both
                        // are walked: a heritage this format cannot carry refuses
                        // here rather than becoming a reference no load could
                        // resolve.
                        if let Some(heritage) = heritage {
                            visit(agent, realm, externals, host, heritage, objects, serials)?;
                        }
                        for key in keys {
                            visit(
                                agent,
                                realm,
                                externals,
                                host,
                                key_value(key),
                                objects,
                                serials,
                            )?;
                        }
                    }
                    Callable::ClassMember {
                        class, key, home, ..
                    } => {
                        // The class is visited because the record resolves the
                        // member *through* it: a class this format cannot carry has
                        // to refuse here rather than leave a record no load could
                        // read.
                        visit(agent, realm, externals, host, class, objects, serials)?;
                        visit(agent, realm, externals, host, key, objects, serials)?;
                        if let Some(home) = home {
                            visit(agent, realm, externals, host, home, objects, serials)?;
                        }
                    }
                }
                for child in children(agent, &function.object)? {
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

/// Give a module its serial: one record, written and read as the source and name
/// the engine's own compile path rebuilds it from.
///
/// A module is deduplicated by its own identity — a host that attached the same
/// record to two contexts gets one — because the walk's books are keyed by
/// identity and a module is a heap box like any other. Nothing descends into it:
/// what a module is *made of* that the language can reach (its namespace, its
/// bindings) is not in the value graph, which is why this needs no children.
fn visit_module(
    module: Handle<crate::module::SourceTextModule>,
    objects: &mut Vec<Entry>,
    serials: &mut HashMap<Identity, u32>,
) -> u32 {
    let key = Identity::Module(crux::handle::Handle::as_ptr(module) as usize as u64);
    if let Some(serial) = serials.get(&key) {
        return *serial;
    }
    let serial = objects.len() as u32;
    serials.insert(key, serial);
    objects.push(Entry::Module(module));
    serial
}

/// What an environment record contributes to a blob: the kind byte it is written
/// under, whether a capture read skips it, and the bindings it holds.
struct CarriedEnvironment {
    kind: u8,
    transparent: bool,
    bindings: Vec<(JsString, Binding)>,
}

/// Read the declarative half of an environment record the format carries, with
/// the kind byte it is written under and the attributes a restore puts back — or
/// the reason this format cannot carry it.
///
/// A declarative record and a function record are the two a closure's names
/// resolve through: a lookup walks the record's own binding list, so an object
/// record (whose names live on a binding object), a module record (whose
/// bindings are import indirections) and a global record (which the reading
/// realm rebuilds) are refused by the kind they are.
fn carried_environment(env: &EnvRef) -> Result<CarriedEnvironment, Unsupported> {
    let (kind, declarative) = match &**env {
        EnvRecord::Declarative(declarative) => (ENV_DECLARATIVE, declarative),
        EnvRecord::Function(function) => (ENV_FUNCTION, &function.declarative),
        EnvRecord::Object(_) => {
            return Err(Unsupported::new(
                "an object environment",
                "its names live on a binding object, and a lookup here walks a record's own binding list",
            ));
        }
        EnvRecord::Module(_) => {
            return Err(Unsupported::new(
                "a module environment",
                "its bindings are import indirections the reading realm would have to have linked already",
            ));
        }
        EnvRecord::Global(_) => {
            return Err(Unsupported::new(
                "a global environment",
                "only the realm's own global environment is left out of a chain; another realm's is not this format's to rebuild",
            ));
        }
    };
    // The borrow is dropped before anything is visited or written: both allocate
    // and can re-enter the agent, and a `RefCell` held across that is a
    // collection the trace skips.
    let bindings = declarative
        .bindings
        .borrow()
        .iter()
        .map(|(name, binding)| {
            if binding.indirect.is_some() {
                return Err(Unsupported::new(
                    "a module import binding",
                    "it resolves through the module that exported it, which the blob does not hold",
                ));
            }
            Ok((name.clone(), binding.clone()))
        })
        .collect::<Result<Vec<_>, Unsupported>>()?;
    Ok(CarriedEnvironment {
        kind,
        transparent: declarative.context_transparent.get(),
        bindings,
    })
}

/// The bindings a realm's own global environment holds **lexically**: a
/// top-level `let`, `const` or `class`, which lives in the global record's
/// declarative half rather than on the global object.
///
/// The realm's global environment writes no environment record of its own —
/// the chain ends in the reading realm's global — but the names a script
/// declared there are the realm's state all the same, so they travel with the
/// carried global instead. A binding is the same [`Binding`] a declarative
/// record holds, so the flags and the TDZ mean what they mean there.
fn carried_global_bindings(realm: &Handle<Realm>) -> Result<Vec<(JsString, Binding)>, Unsupported> {
    let env = realm.global_env;
    let EnvRecord::Global(global) = &*env else {
        return Err(Unsupported::new(
            "a global environment",
            "the realm's global environment is not a global record",
        ));
    };
    // The borrow is dropped before anything is visited or written: both allocate
    // and can re-enter the agent, and a `RefCell` held across that is a
    // collection the trace skips.
    global
        .declarative
        .bindings
        .borrow()
        .iter()
        .map(|(name, binding)| {
            if binding.indirect.is_some() {
                return Err(Unsupported::new(
                    "a module import binding",
                    "it resolves through the module that exported it, which the blob does not hold",
                ));
            }
            Ok((name.clone(), binding.clone()))
        })
        .collect::<Result<Vec<_>, Unsupported>>()
}

/// Give an environment record its serial, and walk everything a restore needs to
/// rebuild it: every binding's name and value, the outer link, and — for a
/// function record — the `this` value a reader of the record's this-binding
/// status could ask for.
///
/// The realm's own global environment is not carried at all: it answers
/// `NO_REF`, which the reader resolves to the *reading* realm's global, so a
/// chain that ended there ends in the realm the restore is rebuilding rather
/// than in a copy of the realm that wrote the blob.
fn visit_env(
    agent: &Agent,
    realm: &Handle<Realm>,
    externals: &[usize],
    host: Option<&dyn HostCallbacks>,
    env: EnvRef,
    objects: &mut Vec<Entry>,
    serials: &mut HashMap<Identity, u32>,
) -> Result<u32, Unsupported> {
    if env.ptr_eq(realm.global_env) {
        return Ok(NO_REF);
    }
    let key = Identity::Env(crux::handle::Handle::as_ptr(env) as usize as u64);
    if let Some(serial) = serials.get(&key) {
        return Ok(*serial);
    }
    // The whole record is read before its serial is registered, so an
    // environment this format cannot carry refuses without leaving an entry the
    // writer would later be asked for. The kind and the flag are the writer's;
    // what the walk owes the graph is every binding's name and value.
    let bindings = carried_environment(&env)?.bindings;
    let serial = objects.len() as u32;
    serials.insert(key, serial);
    objects.push(Entry::Env(env, *realm));
    for (name, binding) in bindings {
        visit(
            agent,
            realm,
            externals,
            host,
            Value::String(Handle::new(name)),
            objects,
            serials,
        )?;
        if let Some(value) = binding.value {
            visit(agent, realm, externals, host, value, objects, serials)?;
        }
    }
    if let Some(outer) = env.outer() {
        visit_env(agent, realm, externals, host, outer, objects, serials)?;
    }
    if let EnvRecord::Function(function) = &*env {
        visit(
            agent,
            realm,
            externals,
            host,
            *function.this_value.borrow(),
            objects,
            serials,
        )?;
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
    /// the source itself, its [[Strict]], its [[Environment]] when the format
    /// carries one for that grammar, and — for the one grammar that has one —
    /// its [[HomeObject]].
    Body {
        grammar: Grammar,
        source: &'a JsString,
        strict: bool,
        home: Option<Value>,
        environment: Option<EnvRef>,
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
    /// A class constructor: the class source, and the definition-time **inputs**
    /// the rebuild must not re-evaluate — the heritage value (`None` for a class
    /// with no `extends` clause) and the computed element keys.
    Class {
        source: &'a JsString,
        heritage: Option<Value>,
        keys: &'a [PropertyKey],
    },
    /// A method that reads a private name: the class whose declaration owns the
    /// name, the member's key, which of the three forms it is, and its
    /// [[HomeObject]]. Carried as a **member of that class** rather than by its
    /// own source, because a `#name` is in scope only inside a class body — so
    /// only the class's own evaluation can rebuild the method.
    ClassMember {
        class: Value,
        key: Value,
        form: Form,
        home: Option<Value>,
    },
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
    /// A method definition's source: `m() {}`, `get x() {}` — the production's
    /// own text, which has no `function` keyword either. The one grammar whose
    /// record also carries a [[HomeObject]].
    Method,
}

impl Grammar {
    fn byte(self) -> u8 {
        match self {
            Grammar::Function => GRAMMAR_FUNCTION,
            Grammar::Class => GRAMMAR_CLASS,
            Grammar::Arrow => GRAMMAR_ARROW,
            Grammar::Method => GRAMMAR_METHOD,
        }
    }

    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            GRAMMAR_FUNCTION => Some(Grammar::Function),
            GRAMMAR_CLASS => Some(Grammar::Class),
            GRAMMAR_ARROW => Some(Grammar::Arrow),
            GRAMMAR_METHOD => Some(Grammar::Method),
            _ => None,
        }
    }

    /// Whether this grammar's record carries a [[HomeObject]] after its strict
    /// byte: true for a method and for nothing else.
    fn carries_home(self) -> bool {
        self == Grammar::Method
    }

    /// Whether this grammar's record carries the [[Environment]] it closed
    /// over: true for a function and a method, the two the restore
    /// instantiates from their source. An arrow and a class constructor are
    /// evaluated instead, so the environment they get is the reading realm's
    /// global.
    fn carries_environment(self) -> bool {
        matches!(self, Grammar::Function | Grammar::Method)
    }
}

/// Which of the three forms a carried class member is.
///
/// A method and an accessor's getter and setter can share one key, so the form
/// is what a class-member record adds to the key to name exactly one member —
/// the descriptor the restore reads holds the member's function under it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Form {
    Method,
    Getter,
    Setter,
}

impl Form {
    fn byte(self) -> u8 {
        match self {
            Form::Method => 0,
            Form::Getter => 1,
            Form::Setter => 2,
        }
    }

    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Form::Method),
            1 => Some(Form::Getter),
            2 => Some(Form::Setter),
            _ => None,
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
            // A method that could resolve a private name cannot be rebuilt from
            // its own text: the object literal a MethodDefinition is valid in has
            // no class body, so a `#name` has no scope in it. It is carried as a
            // member of the class that owns the name instead, when the home object
            // makes that class certain (see `class_member`). A class constructor is
            // excluded: it is a method definition too, and its own `constructor`
            // property would name the class as a member of itself.
            if data.is_method
                && !data.is_class_constructor
                && let Some((class, key, form)) = class_member(agent, function, data)
            {
                return Ok(Callable::ClassMember {
                    class,
                    key,
                    form,
                    home: data.home_object,
                });
            }
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
                // A class constructor's source is the class it came from, and its
                // definition-time **inputs** are carried beside it: the heritage
                // value and the computed element keys were evaluated when the
                // class was defined, and re-evaluating them at restore would
                // resolve a name the reading realm's global may not have. The
                // class grammar reads the source, and the record's own fields
                // carry those inputs.
                return Ok(Callable::Class {
                    source,
                    heritage: carried_heritage(agent, data),
                    keys: &data.computed_keys,
                });
            } else if data.is_method {
                // A method's source is the MethodDefinition it was written as, so
                // it is evaluated where it is being restored rather than parsed —
                // `parse_function` wants a `function` keyword a method has none
                // of. Checked **after** the class constructor, which is a method
                // definition too but whose source is the class.
                Grammar::Method
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
                home: grammar.carries_home().then_some(data.home_object).flatten(),
                // The two grammars the restore *instantiates* keep the chain
                // they closed over. An arrow and a class constructor are rebuilt
                // by evaluating their source in the reading realm instead, so
                // their environment is that realm's global by construction and
                // carrying one would be a field no reader honours.
                environment: grammar.carries_environment().then_some(data.environment),
            })
        }
    }
}

/// The class a method that reads a private name belongs to, with the key and the
/// form that name it there — or `None` when it has no private names, or its class
/// cannot be identified.
///
/// The trigger is the private environment a closure resolves `#name` through: a
/// method that could not resolve one is carried by its own source, which is what
/// every class method with no private elements keeps doing. A `#name` belongs to
/// one class declaration, so a method that reads one has to be rebuilt from
/// *that* class's evaluation; a second evaluation would mint a second brand, and
/// the method would throw when called on an instance of the class the graph
/// actually holds.
///
/// The class is accepted only when the home object makes it certain: a static
/// method's [[HomeObject]] is the class itself, and an instance method's is the
/// class constructor's own `prototype`, compared **by identity** so that an
/// ordinary object which merely carries a class as its `constructor` is not
/// mistaken for a class prototype.
fn class_member(
    agent: &Agent,
    function: &Handle<Function>,
    data: &crate::function::EcmaFunction,
) -> Option<(Value, Value, Form)> {
    let private = data.private_environment?;
    if private.names.borrow().is_empty() {
        return None;
    }
    let home = data.home_object?;
    let (class, holder) = match home.kind() {
        ValueKind::Function(class_function) => {
            if !is_class_constructor(agent, &class_function) {
                return None;
            }
            (home, class_function.object)
        }
        ValueKind::Object(object) => {
            let constructor = object
                .get_own_property(&JsString::from_utf8("constructor"))
                .ok()
                .flatten()
                .and_then(|property| property.value())?;
            let class_function = constructor.as_function()?;
            if !is_class_constructor(agent, &class_function) {
                return None;
            }
            let prototype = class_function
                .object
                .get_own_property(&JsString::from_utf8("prototype"))
                .ok()
                .flatten()
                .and_then(|property| property.value())
                .and_then(|value| value.as_object())?;
            if prototype.id() != object.id() {
                return None;
            }
            (constructor, object)
        }
        _ => return None,
    };
    // The member is found by identity rather than by name: which key a method was
    // defined under is exactly the key whose descriptor holds *this* function, and
    // a computed key is no harder to find than any other.
    let id = function.id();
    for key in holder.own_property_keys().ok()? {
        let Some(property) = holder.get_own_property_key(&key).ok().flatten() else {
            continue;
        };
        let form = match &property.kind {
            PropertyKind::Data { value, .. } => {
                (value.as_function().map(|member| member.id()) == Some(id)).then_some(Form::Method)
            }
            PropertyKind::Accessor { get, set } => [(get, Form::Getter), (set, Form::Setter)]
                .into_iter()
                .find(|(member, _)| {
                    member
                        .as_ref()
                        .and_then(|member| member.as_function())
                        .map(|member| member.id())
                        == Some(id)
                })
                .map(|(_, form)| form),
        };
        if let Some(form) = form {
            return Some((class, key_value(&key), form));
        }
    }
    None
}

/// The single method definition a parsed `(<source>)` or `({<source>})` holds.
///
/// A method record's source is one MethodDefinition — `m() {}`, `get x() {}`,
/// `set x(v) {}` — and the three forms are the object literal's three property
/// kinds. The parse is what tells them apart, which is the point: the AST is what
/// the rebuild instantiates, so the source's key expression is never evaluated.
enum MethodDefinition<'a> {
    Method(&'a syntax::ast::Function),
    Get(&'a syntax::ast::Block),
    Set(
        &'a syntax::ast::BindingPattern,
        Option<&'a syntax::ast::Expr>,
        &'a syntax::ast::Block,
    ),
}

fn method_definition(program: &syntax::ast::Program) -> Option<MethodDefinition<'_>> {
    let [statement] = program.body.as_slice() else {
        return None;
    };
    let syntax::ast::StmtKind::Expr(expression) = &statement.kind else {
        return None;
    };
    let mut expression = expression;
    while let syntax::ast::ExprKind::Paren(inner) = &expression.kind {
        expression = inner;
    }
    let syntax::ast::ExprKind::Object(literal) = &expression.kind else {
        return None;
    };
    let [property] = literal.props.as_slice() else {
        return None;
    };
    match property {
        syntax::ast::ObjectProperty::Method { function, .. } => {
            Some(MethodDefinition::Method(function))
        }
        syntax::ast::ObjectProperty::Get { body, .. } => Some(MethodDefinition::Get(body)),
        syntax::ast::ObjectProperty::Set {
            param, init, body, ..
        } => Some(MethodDefinition::Set(param, init.as_ref(), body)),
        _ => None,
    }
}

/// The class a parsed `(<class source>)` names, if that is what it is: the one
/// expression statement's expression, through the grouping paren the wrapper put
/// there. The parser keeps a paren (it affects `new` binding and
/// `Function.prototype.toString`), so it is one node to step through.
fn class_expression(program: &syntax::ast::Program) -> Option<&syntax::ast::Class> {
    let [statement] = program.body.as_slice() else {
        return None;
    };
    let syntax::ast::StmtKind::Expr(expression) = &statement.kind else {
        return None;
    };
    class_of(expression)
}

fn class_of(expression: &syntax::ast::Expr) -> Option<&syntax::ast::Class> {
    match &expression.kind {
        syntax::ast::ExprKind::Class(class) => Some(class),
        syntax::ast::ExprKind::Paren(inner) => class_of(inner),
        _ => None,
    }
}

/// The heritage a class record carries: the value the class was defined with, or
/// `None` for a class with no `extends` clause.
///
/// `super_constructor` is the *resolved* heritage, and the one case where it is
/// not the value itself is `extends null`, which resolves to
/// `%Function.prototype%` as the super constructor with no proto parent. Carrying
/// the value `null` back is exactly what `resolve_heritage` needs to reproduce it,
/// and no other heritage resolves there: the language rejects
/// `%Function.prototype%` itself as a heritage (it is not a constructor), and a
/// heritage whose `prototype` *is* `%Function.prototype%` keeps itself as the
/// super constructor.
fn carried_heritage(agent: &Agent, data: &crate::function::EcmaFunction) -> Option<Value> {
    let super_constructor = data.super_constructor?;
    let function_prototype = agent
        .current_realm()
        .ok()
        .and_then(|realm| realm.intrinsics.get("%Function.prototype%"));
    if function_prototype == Some(super_constructor) {
        return Some(Value::Null);
    }
    Some(super_constructor)
}

/// Whether a function is a class constructor: what tells a class prototype's
/// `constructor` from an ordinary function a host stored under that name.
fn is_class_constructor(agent: &Agent, function: &Handle<Function>) -> bool {
    agent
        .ecma_functions
        .get(&function.id())
        .is_some_and(|data| data.is_class_constructor)
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

/// The ArrayBuffer state an object's identity names, when the object is one: an
/// ArrayBuffer's data lives in the agent, keyed by the object, rather than in the
/// object itself.
fn array_buffer_state<'a>(
    agent: &'a Agent,
    object: &Handle<JsObject>,
) -> Option<std::cell::Ref<'a, BufferState>> {
    agent
        .buffer_data
        .get(&object.id())
        .map(|state| state.borrow())
}

/// Whether this object is a DataView, whose view state the agent holds the same
/// way it holds a buffer's.
fn is_data_view(agent: &Agent, object: &Handle<JsObject>) -> bool {
    agent.dataview_data.contains_key(&object.id())
}

/// Every value an object reaches: its prototype, its keys, and its values.
///
/// An Array's elements are its values too, and its `length` is neither an
/// element nor an ordinary property, so the two exotics are asked the question
/// their kind makes meaningful — and a typed array is asked it the same way, its
/// buffer being the one value it is made of. A host pointer has no children: what
/// it names is the host's, written as an index into the table the host supplies.
/// Every *other* exotic kind is refused here rather than walked: its own state is
/// not in `properties`, so writing it as the ordinary object its shape would
/// suggest would produce an object that is not the one that was written — a proxy
/// without its traps, a `String` object without its string.
fn children(agent: &Agent, object: &Handle<JsObject>) -> Result<Vec<Value>, Unsupported> {
    let mut children = Vec::new();
    if let Some(prototype) = object
        .get_prototype_of()
        .map_err(|_| Unsupported::new("an object", "its prototype could not be read"))?
    {
        children.push(Value::Object(prototype));
    }
    if is_data_view(agent, object) {
        return Err(Unsupported::new(
            "a data view",
            "a data view's buffer and geometry are not carried yet, and an ordinary object is not what it is",
        ));
    }
    if let Some(state) = array_buffer_state(agent, object) {
        if state.is_shared || state.growable {
            return Err(Unsupported::new(
                "a shared array buffer",
                "a restored copy cannot be the memory another agent holds, and being that memory is the whole of what it is",
            ));
        }
        if state.detached {
            return Err(Unsupported::new(
                "a detached array buffer",
                "it has no bytes to carry, and the engine's own view constructor refuses a view over one",
            ));
        }
    }
    // A view whose data the agent holds rather than the object: a DataView is not
    // carried at all (writing it as the ordinary object it looks like would read
    // back as one, with no view state), and a buffer this format cannot rebuild
    // as what it was is refused here, where the walk names what it refuses.
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
                    // told apart by own-property presence, and the record says
                    // which by the serial it writes (`NO_REF`). A hole has no value
                    // to walk and no property to name.
                    Ok(None) => {}
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
        ObjectKind::IntegerIndexed(slots) => {
            // A view reads its elements out of the block its buffer holds, so the
            // buffer is the one value it is made of. Its indices are virtual and
            // `is_carried_key` skips them, exactly as it skips an Array's.
            children.push(slots.buffer_object);
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
    // An Array's indices live in its slots and a view's are virtual (they read
    // the block its buffer holds), so neither is an own property the record
    // carries — the difference between them is `length`: it is an Array's one
    // own non-index key, and a view's `length` is an accessor on its prototype.
    let is_array = matches!(&object.kind, ObjectKind::Array(_));
    if !is_array && !matches!(&object.kind, ObjectKind::IntegerIndexed(_)) {
        return true;
    }
    let PropertyKey::String(atom) = key else {
        return true;
    };
    let text = string::lookup(*atom);
    if canonical_index(&text).is_some() {
        return false;
    }
    // `length` is an Array's own key; a view's is an accessor on its prototype,
    // so it is an ordinary carried key there.
    !(is_array && text.as_slice() == LENGTH_UNITS)
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
                    home,
                    environment,
                } => {
                    // `carries_home` is what says whether this grammar has a
                    // value beside the source at all — a plain function and an
                    // arrow have none.
                    let carried = if grammar.carries_home() {
                        Carried::Home(home)
                    } else {
                        Carried::None
                    };
                    write_function(
                        realm,
                        &function,
                        grammar,
                        source,
                        strict,
                        environment,
                        carried,
                        serials,
                        body,
                    )?
                }
                Callable::Class {
                    source,
                    heritage,
                    keys,
                } => write_function(
                    realm,
                    &function,
                    Grammar::Class,
                    source,
                    true,
                    None,
                    Carried::Class { heritage, keys },
                    serials,
                    body,
                )?,
                Callable::Bound { .. } => write_bound_function(realm, &function, serials, body)?,
                Callable::HostCallback { pointer, data } => {
                    write_host_callback(realm, &function, pointer, data, externals, serials, body)?
                }
                Callable::ClassMember {
                    class,
                    key,
                    form,
                    home,
                } => write_class_method(realm, &function, class, key, form, home, serials, body)?,
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
            None if matches!(&object.kind, ObjectKind::IntegerIndexed(_)) => {
                write_typed_array(realm, object, serials, body)?
            }
            None => {
                if let ObjectKind::External(pointer) = &object.kind {
                    body.push(REC_EXTERNAL);
                    write_u32(body, external_index(externals, *pointer)?);
                } else if array_buffer_state(agent, &object).is_some() {
                    write_array_buffer(agent, realm, &object, serials, body)?;
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

/// The grammar-conditional values a function record carries beside its source.
///
/// Which of them a record has is exactly what its grammar means: a method's
/// `[[HomeObject]]`, which is the one value `super` resolves through, or a
/// class's definition-time inputs, which are the values the rebuild must not
/// re-evaluate.
enum Carried<'a> {
    None,
    Home(Option<Value>),
    Class {
        heritage: Option<Value>,
        keys: &'a [PropertyKey],
    },
}

/// Write one carried thing: the record a value gets, or a module's own record.
fn write_carried(
    agent: &Agent,
    externals: &[usize],
    host: Option<&dyn HostCallbacks>,
    carried: &Entry,
    serials: &HashMap<Identity, u32>,
    body: &mut Vec<u8>,
) -> Result<(), Unsupported> {
    match carried {
        Entry::Value(value, realm) => {
            write_record(agent, realm, externals, host, *value, serials, body)
        }
        Entry::Module(module) => {
            write_module(module, body);
            Ok(())
        }
        Entry::Env(env, realm) => write_env(realm, *env, serials, body),
    }
}

/// The serial of an environment the walk registered, or `NO_REF` for the realm's
/// own global environment, which the format leaves out of a chain.
///
/// A miss is a walk bug rather than a value the format cannot carry — every
/// environment a record names was visited first — so it refuses loudly instead
/// of writing `NO_REF`, which the reader would take as the realm's global.
fn env_serial(
    realm: &Handle<Realm>,
    env: EnvRef,
    serials: &HashMap<Identity, u32>,
) -> Result<u32, Unsupported> {
    if env.ptr_eq(realm.global_env) {
        return Ok(NO_REF);
    }
    serials
        .get(&Identity::Env(
            crux::handle::Handle::as_ptr(env) as usize as u64
        ))
        .copied()
        .ok_or(Unsupported::new(
            "a function",
            "its environment chain was not walked, so its record names no environment",
        ))
}

/// Write an environment record: its kind, whether a capture read skips it, its
/// outer link and its bindings — then, for a function record, the `this` value
/// and this-binding status.
///
/// A binding is written in the shape a property is (a name serial, a flags byte,
/// a value serial and one reserved) because the four-field reader is the same
/// one: the bytes differ in meaning, not in layout. `NO_REF` where the value
/// goes is the binding's TDZ — an uninitialized `let` or `const` has no value to
/// write, and one that came back initialized would be a binding the writing
/// realm's code would have thrown on.
fn write_env(
    realm: &Handle<Realm>,
    env: EnvRef,
    serials: &HashMap<Identity, u32>,
    body: &mut Vec<u8>,
) -> Result<(), Unsupported> {
    let CarriedEnvironment {
        kind,
        transparent,
        bindings,
    } = carried_environment(&env)?;
    body.push(REC_ENV);
    body.push(kind);
    body.push(if transparent { ENV_FLAG_TRANSPARENT } else { 0 });
    write_u32(
        body,
        match env.outer() {
            Some(outer) => env_serial(realm, outer, serials)?,
            None => NO_REF,
        },
    );
    write_u32(body, bindings.len() as u32);
    for (name, binding) in &bindings {
        write_u32(
            body,
            serial_in(realm, serials, Value::String(Handle::new(name.clone()))),
        );
        body.push(binding_flags(binding));
        write_u32(
            body,
            binding
                .value
                .map_or(NO_REF, |value| serial_in(realm, serials, value)),
        );
        write_u32(body, NO_REF);
    }
    if let EnvRecord::Function(function) = &*env {
        write_u32(
            body,
            serial_in(realm, serials, *function.this_value.borrow()),
        );
        body.push(match function.this_binding_status.get() {
            ThisBindingStatus::Lexical => THIS_LEXICAL,
            ThisBindingStatus::Uninitialized => THIS_UNINITIALIZED,
            ThisBindingStatus::Initialized => THIS_INITIALIZED,
        });
    }
    Ok(())
}

/// Write a binding list in the shape a property list is: the count, then each
/// binding's name serial, its attributes byte, its value serial and one
/// reserved. The four-field reader is the same one, so the bytes differ in
/// meaning, not in layout — `NO_REF` where the value goes is the binding's TDZ,
/// the same spelling an environment record's binding uses.
fn write_bindings(body: &mut Vec<u8>, bindings: &[CarriedBinding]) {
    write_u32(body, bindings.len() as u32);
    for binding in bindings {
        write_u32(body, binding.name);
        body.push(binding.flags);
        write_u32(body, binding.value);
        write_u32(body, NO_REF);
    }
}

/// A binding's attributes byte, spelled once so an environment record's binding
/// and a realm's global lexical one cannot drift apart.
fn binding_flags(binding: &Binding) -> u8 {
    let mut flags = 0u8;
    if binding.mutable {
        flags |= BIND_MUTABLE;
    }
    if binding.strict {
        flags |= BIND_STRICT;
    }
    if binding.deletable {
        flags |= BIND_DELETABLE;
    }
    if binding.parameter {
        flags |= BIND_PARAMETER;
    }
    flags
}

/// Write a module record: the name it was compiled under, when it has one, and
/// the source text it is rebuilt from.
///
/// Those two facts are the whole of what the engine's own compile path needs —
/// `parse_module` takes a specifier, a name and a source — so a restored module is
/// the same module the snapshot carried, as far as anything the blob can hold is
/// concerned. What is not carried is what a module *is* beyond its text: its link
/// status, its bindings and its namespace, none of which is in the value graph
/// (a host that attached a module reaches it as a handle, not as a value).
fn write_module(module: &Handle<crate::module::SourceTextModule>, body: &mut Vec<u8>) {
    body.push(REC_MODULE);
    match &module.name {
        Some(name) => {
            body.push(1);
            write_units(body, name.as_slice());
        }
        None => body.push(0),
    }
    write_units(body, module.source.as_slice());
}

/// Write a JavaScript function as the source text it can be rebuilt from, its
/// [[Strict]], the environment chain it closed over, and its object part — the
/// same prototype/extensible/properties triple an object gets, because a
/// function's own keys are its own keys.
///
/// The source is what makes this the one record a restore re-runs the parser
/// for. The environment is what makes a free name in that source resolve where
/// it did the first time: it is written as the serial of the chain the walk gave
/// the function, and `NO_REF` — the realm's own global environment, or a grammar
/// the restore evaluates rather than instantiates — means the reader's global.
#[allow(clippy::too_many_arguments)]
fn write_function(
    realm: &Handle<Realm>,
    function: &Handle<Function>,
    grammar: Grammar,
    source: &JsString,
    strict: bool,
    environment: Option<EnvRef>,
    carried: Carried<'_>,
    serials: &HashMap<Identity, u32>,
    body: &mut Vec<u8>,
) -> Result<(), Unsupported> {
    body.push(REC_FUNCTION);
    body.push(grammar.byte());
    body.push(u8::from(strict));
    // The environment is written here, before the grammar-conditional values, so
    // a reader consumes it in the one position every grammar has.
    write_u32(
        body,
        match environment {
            Some(environment) => env_serial(realm, environment, serials)?,
            None => NO_REF,
        },
    );
    match carried {
        Carried::None => {}
        Carried::Home(home) => {
            write_u32(
                body,
                home.map_or(NO_REF, |home| serial_in(realm, serials, home)),
            );
        }
        Carried::Class { heritage, keys } => {
            write_u32(
                body,
                heritage.map_or(NO_REF, |heritage| serial_in(realm, serials, heritage)),
            );
            write_u32(body, keys.len() as u32);
            for key in keys {
                write_u32(body, serial_in(realm, serials, key_value(key)));
            }
        }
    }
    write_units(body, source.as_slice());
    write_u32(body, prototype_serial(realm, &function.object, serials)?);
    body.push(u8::from(function.object.extensible.get()));
    write_properties(realm, &function.object, serials, body)?;
    Ok(())
}

/// Write a method that reads a private name as the class member it is: the
/// class, the key and form that name it there, its [[HomeObject]], and its
/// object part.
///
/// The source is deliberately **not** carried. A `#name` belongs to one class
/// declaration, so the only evaluation that can rebuild this method is the one
/// that makes the class — a second evaluation of the class source would mint a
/// second brand, and the method would throw on an instance of the class the
/// graph actually holds. The record therefore names the member and lets the
/// restore take it out of the class it evaluated.
#[allow(clippy::too_many_arguments)]
fn write_class_method(
    realm: &Handle<Realm>,
    function: &Handle<Function>,
    class: Value,
    key: Value,
    form: Form,
    home: Option<Value>,
    serials: &HashMap<Identity, u32>,
    body: &mut Vec<u8>,
) -> Result<(), Unsupported> {
    body.push(REC_CLASS_METHOD);
    write_u32(body, serial_in(realm, serials, class));
    write_u32(body, serial_in(realm, serials, key));
    body.push(form.byte());
    write_u32(
        body,
        home.map_or(NO_REF, |home| serial_in(realm, serials, home)),
    );
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
            // A hole is the **absence** of an element, so the record says so with
            // `NO_REF` — the same sentinel an absent accessor half and a null
            // prototype use. What it must not write is `undefined`, which is an
            // element that is there: `1 in [1, , 3]` is false and
            // `1 in [1, undefined, 3]` is true.
            Ok(None) => write_u32(body, NO_REF),
            Err(_) => return Err(Unsupported::new("an array", "an element could not be read")),
        }
    }
    write_properties(realm, &array, serials, body)?;
    Ok(())
}

/// A length the format writes as a `u32`, or the refusal that names it: every
/// length field in this format is 32-bit, so a longer one refuses rather than
/// silently truncating.
fn length_as_u32(value: usize, type_name: &'static str) -> Result<u32, Unsupported> {
    u32::try_from(value)
        .map_err(|_| Unsupported::new(type_name, "its length is beyond what this format writes"))
}

/// Write an ArrayBuffer as its geometry, its own flags, and the bytes it holds.
///
/// The bytes are the record's substance: an ArrayBuffer *is* its data, and every
/// view over it reads them out of the same block. A buffer whose storage is
/// **host memory** — a v8 backing store over an allocation the host owns — is
/// written the same way, as its bytes, which is what V8's own serializer does
/// with a non-shared ArrayBuffer. What is not carried is the identity of the
/// host's allocation: a host that wants the sharing re-establishes it on its load
/// path, and `deno_core` does exactly that for the three `ContextState`-backed
/// views it creates on every runtime construction.
///
/// A shared, growable or detached buffer is refused by the walk, so it cannot
/// reach here.
fn write_array_buffer(
    agent: &Agent,
    realm: &Handle<Realm>,
    object: &Handle<JsObject>,
    serials: &HashMap<Identity, u32>,
    body: &mut Vec<u8>,
) -> Result<(), Unsupported> {
    let Some(state) = array_buffer_state(agent, object) else {
        return Err(Unsupported::new(
            "an array buffer",
            "its data is not on this agent",
        ));
    };
    let byte_length = length_as_u32(state.byte_length, "an array buffer")?;
    let max_byte_length = length_as_u32(
        state.max_byte_length.unwrap_or(state.byte_length),
        "an array buffer's maximum",
    )?;
    let mut flags = 0u8;
    if state.resizable {
        flags |= BUFFER_RESIZABLE;
    }
    if state.immutable {
        flags |= BUFFER_IMMUTABLE;
    }
    let bytes = state
        .shared
        .read(0, state.byte_length)
        .map_err(|_| Unsupported::new("an array buffer", "its bytes could not be read"))?;
    drop(state);
    body.push(REC_ARRAY_BUFFER);
    body.push(flags);
    write_u32(body, byte_length);
    // Written only for a resizable buffer, and read under the same flag: the
    // engine sets a maximum for a resizable or growable buffer and for no other,
    // and a growable one is refused above.
    if flags & BUFFER_RESIZABLE != 0 {
        write_u32(body, max_byte_length);
    }
    body.extend_from_slice(&bytes);
    write_u32(body, prototype_serial(realm, object, serials)?);
    body.push(u8::from(object.extensible.get()));
    write_properties(realm, object, serials, body)?;
    Ok(())
}

/// Write a typed array as the kind it is, the buffer it views, its geometry, and
/// its object part.
///
/// The elements are not written here: they are the **buffer's** bytes, and a view
/// is a window onto them. So two views over one buffer write one buffer record —
/// the serial map makes the shared object one serial, which is what keeps
/// `a.buffer === b.buffer` true across a round trip — and what a view's own record
/// adds is which elements those bytes are read as and where its window starts.
fn write_typed_array(
    realm: &Handle<Realm>,
    object: Handle<JsObject>,
    serials: &HashMap<Identity, u32>,
    body: &mut Vec<u8>,
) -> Result<(), Unsupported> {
    // The walk refuses every other kind, so the match cannot miss; a miss would
    // be a walk/writer disagreement.
    let ObjectKind::IntegerIndexed(slots) = &object.kind else {
        return Err(Unsupported::new(
            "a typed array",
            "its kind is not the one this record is for",
        ));
    };
    body.push(REC_TYPED_ARRAY);
    write_text(body, slots.element_type.name());
    write_u32(body, serial_in(realm, serials, slots.buffer_object));
    write_u32(
        body,
        length_as_u32(slots.byte_offset, "a typed array's byte offset")?,
    );
    body.push(u8::from(slots.auto_length));
    // A fixed view's length is its own; an auto-length view's is the buffer's,
    // derived on every read and stale in the slots — so writing it would write a
    // number nothing honours, and the restore derives it the same way.
    if !slots.auto_length {
        write_u32(
            body,
            length_as_u32(slots.byte_length, "a typed array's byte length")?,
        );
        write_u32(
            body,
            length_as_u32(slots.array_length, "a typed array's length")?,
        );
    }
    write_u32(body, prototype_serial(realm, &object, serials)?);
    body.push(u8::from(object.extensible.get()));
    write_properties(realm, &object, serials, body)?;
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
            let environment = reader.u32().ok_or(DecodeError::Truncated)?;
            let home = if grammar.carries_home() {
                reader.u32().ok_or(DecodeError::Truncated)?
            } else {
                NO_REF
            };
            let (heritage, keys) = if grammar == Grammar::Class {
                let heritage = reader.u32().ok_or(DecodeError::Truncated)?;
                let count = reader.u32().ok_or(DecodeError::Truncated)? as usize;
                let mut keys = Vec::with_capacity(count.min(1024));
                for _ in 0..count {
                    keys.push(reader.u32().ok_or(DecodeError::Truncated)?);
                }
                (heritage, keys)
            } else {
                (NO_REF, Vec::new())
            };
            let source = reader.units().ok_or(DecodeError::Truncated)?;
            let proto = reader.u32().ok_or(DecodeError::Truncated)?;
            let extensible = reader.u8().ok_or(DecodeError::Truncated)? != 0;
            let properties = read_properties(reader)?;
            Ok(Record::Function {
                grammar,
                source,
                strict,
                environment,
                home,
                heritage,
                keys,
                proto,
                extensible,
                properties,
            })
        }
        REC_ARRAY_BUFFER => {
            let flags = reader.u8().ok_or(DecodeError::Truncated)?;
            let byte_length = reader.u32().ok_or(DecodeError::Truncated)?;
            let max_byte_length = if flags & BUFFER_RESIZABLE != 0 {
                reader.u32().ok_or(DecodeError::Truncated)?
            } else {
                byte_length
            };
            // The bytes are the record's own length, so a blob that declares more
            // than it holds is refused here rather than read past its end.
            let bytes = reader
                .take(byte_length as usize)
                .ok_or(DecodeError::Truncated)?
                .to_vec();
            let proto = reader.u32().ok_or(DecodeError::Truncated)?;
            let extensible = reader.u8().ok_or(DecodeError::Truncated)? != 0;
            let properties = read_properties(reader)?;
            Ok(Record::ArrayBuffer {
                byte_length,
                resizable: flags & BUFFER_RESIZABLE != 0,
                immutable: flags & BUFFER_IMMUTABLE != 0,
                max_byte_length,
                bytes,
                proto,
                extensible,
                properties,
            })
        }
        REC_TYPED_ARRAY => {
            let name = reader.text().ok_or(DecodeError::Truncated)?;
            let element = ElementType::from_name(&name).ok_or_else(|| {
                DecodeError::UnrebuildableBuffer(format!(
                    "`{name}` is not an element type this build has"
                ))
            })?;
            let buffer = reader.u32().ok_or(DecodeError::Truncated)?;
            let byte_offset = reader.u32().ok_or(DecodeError::Truncated)?;
            let auto_length = reader.u8().ok_or(DecodeError::Truncated)? != 0;
            // The two lengths are written only for a fixed view, and read under
            // the same flag: an auto-length view has neither of its own.
            let (byte_length, array_length) = if auto_length {
                (0, 0)
            } else {
                (
                    reader.u32().ok_or(DecodeError::Truncated)?,
                    reader.u32().ok_or(DecodeError::Truncated)?,
                )
            };
            let proto = reader.u32().ok_or(DecodeError::Truncated)?;
            let extensible = reader.u8().ok_or(DecodeError::Truncated)? != 0;
            let properties = read_properties(reader)?;
            Ok(Record::TypedArray {
                element,
                buffer,
                byte_offset,
                byte_length,
                array_length,
                auto_length,
                proto,
                extensible,
                properties,
            })
        }
        REC_ENV => {
            let kind = reader.u8().ok_or(DecodeError::Truncated)?;
            if kind != ENV_DECLARATIVE && kind != ENV_FUNCTION {
                return Err(DecodeError::UnrebuildableEnvironment(format!(
                    "environment kind byte {kind} is not one of this format's"
                )));
            }
            let flags = reader.u8().ok_or(DecodeError::Truncated)?;
            let outer = reader.u32().ok_or(DecodeError::Truncated)?;
            // Which fields follow depends on the kind, so it is checked above
            // rather than left to the reader's next byte.
            let bindings = read_bindings(reader)?;
            let (this_value, this_status) = if kind == ENV_FUNCTION {
                let this_value = reader.u32().ok_or(DecodeError::Truncated)?;
                let this_status = reader.u8().ok_or(DecodeError::Truncated)?;
                if this_status > THIS_INITIALIZED {
                    return Err(DecodeError::UnrebuildableEnvironment(format!(
                        "this-binding status byte {this_status} is not one of this format's"
                    )));
                }
                (this_value, this_status)
            } else {
                (NO_REF, THIS_LEXICAL)
            };
            Ok(Record::Env {
                kind,
                transparent: flags & ENV_FLAG_TRANSPARENT != 0,
                outer,
                bindings,
                this_value,
                this_status,
            })
        }
        REC_MODULE => {
            let name = if reader.u8().ok_or(DecodeError::Truncated)? == 1 {
                Some(reader.units().ok_or(DecodeError::Truncated)?)
            } else {
                None
            };
            let source = reader.units().ok_or(DecodeError::Truncated)?;
            Ok(Record::Module { name, source })
        }
        REC_CLASS_METHOD => {
            let class = reader.u32().ok_or(DecodeError::Truncated)?;
            let key = reader.u32().ok_or(DecodeError::Truncated)?;
            let byte = reader.u8().ok_or(DecodeError::Truncated)?;
            let form = Form::from_byte(byte).ok_or(DecodeError::BadGrammar(byte))?;
            let home = reader.u32().ok_or(DecodeError::Truncated)?;
            let proto = reader.u32().ok_or(DecodeError::Truncated)?;
            let extensible = reader.u8().ok_or(DecodeError::Truncated)? != 0;
            let properties = read_properties(reader)?;
            Ok(Record::ClassMethod {
                class,
                key,
                form,
                home,
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

/// An environment's bindings, read in the shape a property's are: the same four
/// fields, so the reader above is the one that consumes them.
fn read_bindings(reader: &mut Reader<'_>) -> Result<Vec<CarriedBinding>, DecodeError> {
    Ok(read_properties(reader)?
        .into_iter()
        .map(|property| CarriedBinding {
            name: property.key,
            flags: property.flags,
            value: property.first,
        })
        .collect())
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
    /// The `prototype` object each rebuilt class's own evaluation made, by the
    /// class record's serial.
    ///
    /// A class record's property tail replaces that object with the carried
    /// prototype, so by the time a class-member record is read the evaluation's
    /// object is reachable from nowhere else — and it is the only place a member
    /// that reads a private name can come from, because it is the object the
    /// class's private brand belongs to. Kept pinned for the same reason `pins`
    /// is: a member record may be read after its class record is complete.
    class_prototypes: HashMap<u32, Handle<JsObject>>,
    /// Environment records whose shell exists and whose bindings have not been
    /// put back yet, by serial.
    ///
    /// A function record materializes its environment *before* the function can
    /// exist — the instantiation takes it — so the environment has to be a shell
    /// at that point: its own bindings may hold the very function whose build is
    /// in progress. The shell's bindings are filled once that function has been
    /// recorded in `made`, which is what makes a binding that holds it and the
    /// function record resolve to **one** function rather than two.
    unfilled_envs: Vec<u32>,
}

/// A record's value, before it is asked for as a language value: an object's
/// prototype has to be set at its creation, and a prototype is an object rather
/// than a value.
#[derive(Clone, Copy)]
enum Built {
    Value(Value),
    Object(Handle<JsObject>),
    Module(Handle<crate::module::SourceTextModule>),
    Env(EnvRef),
}

impl Builder<'_> {
    fn materialize(&mut self, serial: u32) -> Result<Value, DecodeError> {
        match self.object(serial)? {
            Built::Value(value) => Ok(value),
            Built::Object(object) => Ok(Value::Object(object)),
            // Only a slot's own item list can name a module, and only a function
            // record can name an environment, so a field that does is a blob that
            // disagrees with itself rather than a value to hand back.
            Built::Module(_) => Err(DecodeError::Truncated),
            Built::Env(_) => Err(DecodeError::UnrebuildableEnvironment(format!(
                "serial {serial} names an environment record where a value belongs"
            ))),
        }
    }

    /// The prototype intrinsic of a typed array kind: the object the engine's own
    /// view constructor is handed when the record names no `[[Prototype]]` of its
    /// own, which is the one link a construction cannot express (it takes an
    /// object, and a view whose link is *null* has none). The view's link is set
    /// to null after the construction, so this is only ever its interim one.
    fn typed_array_prototype(
        &mut self,
        element: ElementType,
    ) -> Result<Handle<JsObject>, DecodeError> {
        let name = format!("%{}Array.prototype%", element.name());
        let value = self
            .realm
            .intrinsics
            .get(&name)
            .ok_or_else(|| DecodeError::UnknownIntrinsic(name.clone()))?;
        crate::context::as_object(&value)
            .ok_or_else(|| DecodeError::UnrebuildableBuffer(format!("{name} is not an object")))
    }

    /// The environment record a serial names, for the function record that closes
    /// over it — or the reading realm's global when it names none, which is the
    /// chain every existing blob carries.
    fn function_environment(&mut self, serial: u32) -> Result<EnvRef, DecodeError> {
        match serial {
            NO_REF => Ok(self.realm.global_env),
            serial => self.env_ref(serial),
        }
    }

    /// The environment record a serial names.
    fn env_ref(&mut self, serial: u32) -> Result<EnvRef, DecodeError> {
        match self.object(serial)? {
            Built::Env(env) => Ok(env),
            _ => Err(DecodeError::UnrebuildableEnvironment(format!(
                "serial {serial} does not name an environment record"
            ))),
        }
    }

    /// Put back the bindings of every environment whose shell exists but is not
    /// filled, and of every environment those reach.
    ///
    /// Called once a function record has been recorded in `made`, which is the
    /// moment its own environment may be filled: a binding that holds that very
    /// function then names it, rather than starting a second build of it. An
    /// environment is reachable only from a function record or from another
    /// environment, so the drain below settles the whole chain before the build
    /// returns anything.
    fn settle_envs(&mut self) -> Result<(), DecodeError> {
        while let Some(serial) = self.unfilled_envs.pop() {
            let Some(Built::Env(env)) = self.made.get(serial as usize).copied().flatten() else {
                return Err(DecodeError::UnrebuildableEnvironment(format!(
                    "serial {serial} does not name an environment record"
                )));
            };
            let Some(Record::Env {
                kind,
                bindings,
                this_value,
                this_status,
                ..
            }) = self.records.get(serial as usize)
            else {
                return Err(DecodeError::Truncated);
            };
            // The bindings' values and the function record's own state are
            // materialized **before** the binding list is borrowed: materializing
            // allocates and re-enters the builder, and a `RefCell` held across
            // that is a collection the trace skips. Shell-first is what makes the
            // re-entry safe: an environment named here is already in `made`.
            let mut carried = Vec::with_capacity(bindings.len());
            for binding in bindings {
                let name = self.materialize(binding.name)?;
                let ValueKind::String(name) = name.kind() else {
                    return Err(DecodeError::UnrebuildableEnvironment(
                        "a binding's name is not a string".into(),
                    ));
                };
                let value = match binding.value {
                    NO_REF => None,
                    serial => Some(self.materialize(serial)?),
                };
                carried.push((name, *binding, value));
            }
            let state = if *kind == ENV_FUNCTION {
                let this_value = self.materialize(*this_value)?;
                match *this_status {
                    THIS_LEXICAL => (this_value, ThisBindingStatus::Lexical),
                    THIS_UNINITIALIZED => (this_value, ThisBindingStatus::Uninitialized),
                    THIS_INITIALIZED => (this_value, ThisBindingStatus::Initialized),
                    other => {
                        return Err(DecodeError::UnrebuildableEnvironment(format!(
                            "this-binding status byte {other} is not one of this format's"
                        )));
                    }
                }
            } else {
                (Value::Undefined, ThisBindingStatus::Lexical)
            };
            // The barriers run before the binding list is borrowed, for the same
            // reason the materialization does.
            for (_, _, value) in &carried {
                if let Some(value) = value {
                    crux::heap::write_barrier(&*env, *value);
                }
            }
            let declarative = match &*env {
                EnvRecord::Declarative(declarative) => declarative,
                EnvRecord::Function(function) => &function.declarative,
                _ => {
                    return Err(DecodeError::UnrebuildableEnvironment(format!(
                        "serial {serial} names an environment record this format does not carry"
                    )));
                }
            };
            {
                let mut list = declarative.bindings.borrow_mut();
                for (name, binding, value) in carried {
                    list.push((
                        (*name).clone(),
                        Binding {
                            value,
                            mutable: binding.flags & BIND_MUTABLE != 0,
                            strict: binding.flags & BIND_STRICT != 0,
                            deletable: binding.flags & BIND_DELETABLE != 0,
                            indirect: None,
                            parameter: binding.flags & BIND_PARAMETER != 0,
                        },
                    ));
                }
            }
            if let EnvRecord::Function(function) = &*env {
                *function.this_value.borrow_mut() = state.0;
                function.this_binding_status.set(state.1);
            }
        }
        Ok(())
    }

    /// Put the carried realm global back where it belongs: **onto** the reading
    /// realm's global.
    ///
    /// The record is not a value to hand a host, it is the state of a realm the
    /// host never sees built. So its serial is recorded as the reading realm's
    /// global *before* its properties are defined, which is what makes a
    /// property that refers back to the global — `globalThis` is the ordinary
    /// one — resolve to the real global rather than to a copy of it. Only the
    /// properties are applied: the prototype and the extensibility are the
    /// reading realm's, because `Context::new` rebuilt the realm around the
    /// state and the state is all that travels. A property this realm already
    /// has and refuses to redefine (a non-configurable builtin the writing realm
    /// had replaced) is left as this realm's own, which is the same "the realm is
    /// rebuilt either way" rule the format's header states.
    fn apply_realm_global(&mut self, serial: u32) -> Result<(), DecodeError> {
        let records = self.records;
        let Some(Record::Object { properties, .. }) = records.get(serial as usize) else {
            return Err(DecodeError::Truncated);
        };
        let global = self.realm.global_object;
        self.remember(serial as usize, Built::Object(global));
        define_properties(self, global, properties)
    }

    /// Put the carried global **lexical** bindings back into the reading realm's
    /// global environment record.
    ///
    /// A top-level `let`, `const` or `class` is not a property of the global
    /// object — it lives in the global record's declarative half — so it travels
    /// beside the carried global rather than in it, and this is where it lands.
    /// The bindings are pushed in one step, the way `settle_envs` restores an
    /// environment record's, so a `NO_REF` value stays the TDZ rather than
    /// becoming `undefined`, which a read of the name would answer instead of
    /// throwing.
    fn apply_realm_bindings(&mut self, bindings: &[CarriedBinding]) -> Result<(), DecodeError> {
        // Names and values are materialized **before** the record is borrowed:
        // materializing allocates and re-enters the builder, and a `RefCell` held
        // across that is a collection the trace skips.
        let mut carried = Vec::with_capacity(bindings.len());
        for binding in bindings {
            let name = self.materialize(binding.name)?;
            let ValueKind::String(name) = name.kind() else {
                return Err(DecodeError::UnrebuildableEnvironment(
                    "a binding's name is not a string".into(),
                ));
            };
            let value = match binding.value {
                NO_REF => None,
                serial => Some(self.materialize(serial)?),
            };
            carried.push((name, *binding, value));
        }
        let env = self.realm.global_env;
        for (_, _, value) in &carried {
            if let Some(value) = value {
                crux::heap::write_barrier(&*env, *value);
            }
        }
        let EnvRecord::Global(global) = &*env else {
            return Err(DecodeError::UnrebuildableEnvironment(
                "the realm's global environment is not a global record".into(),
            ));
        };
        {
            let mut list = global.declarative.bindings.borrow_mut();
            for (name, binding, value) in carried {
                // A name this realm already declared lexically is a blob and a
                // realm that disagree; it is refused by name rather than
                // shadowing or replacing what the realm itself built.
                if list.iter().any(|(existing, _)| *existing == *name) {
                    return Err(DecodeError::UnrebuildableEnvironment(format!(
                        "the reading realm already has a lexical binding named {:?}",
                        name.to_string_lossy()
                    )));
                }
                list.push((
                    (*name).clone(),
                    Binding {
                        value,
                        mutable: binding.flags & BIND_MUTABLE != 0,
                        strict: binding.flags & BIND_STRICT != 0,
                        deletable: binding.flags & BIND_DELETABLE != 0,
                        indirect: None,
                        parameter: binding.flags & BIND_PARAMETER != 0,
                    },
                ));
            }
        }
        // A declarative binding changes what a name resolves to without touching
        // the global object, and a compiled global read validates against that
        // object's generation alone (see `GlobalEnv::bump_declarative_generation`)
        // — so the counter is bumped, or a cell warmed in this agent before the
        // restore could keep serving the value it saw.
        self.realm.global_object.bump_generation();
        Ok(())
    }

    /// The item a serial names, which a slot's own list needs: every record is a
    /// value except a module's, which is not one.
    fn materialize_item(&mut self, serial: u32) -> Result<SnapshotItem, DecodeError> {
        Ok(match self.object(serial)? {
            Built::Value(value) => SnapshotItem::Value(value),
            Built::Object(object) => SnapshotItem::Value(Value::Object(object)),
            Built::Module(module) => SnapshotItem::Module(crate::api::Module::from_handle(module)),
            Built::Env(_) => {
                return Err(DecodeError::UnrebuildableEnvironment(format!(
                    "serial {serial} names an environment record, which is not an item"
                )));
            }
        })
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
                // The shell is recorded before the properties are defined, so a
                // property that refers back to this object resolves to it
                // rather than restarting the build.
                self.remember(index, Built::Object(object));
                define_properties(self, object, properties)?;
                // Extensibility **last**: an object's own properties exist
                // before it is made non-extensible, and `[[DefineOwnProperty]]`
                // refuses to add one to an object that already is. Applying the
                // record's flag first therefore drops every property of a
                // non-extensible object — a host's frozen namespace is that
                // case, and `define_properties` answering `Ok(false)` is the
                // only sign of it.
                object.extensible.set(*extensible);
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
                self.remember(index, Built::Object(array));
                for (position, element) in elements.iter().enumerate() {
                    // A hole stays a hole: `array_create` made every index absent,
                    // and writing `undefined` here would give this one a property
                    // the writing realm's array never had.
                    if *element == NO_REF {
                        continue;
                    }
                    let value = self.materialize(*element)?;
                    array
                        .create_data_property_index(position as u64, value)
                        .map_err(|_| DecodeError::Truncated)?;
                }
                define_properties(self, array, extras)?;
                // Extensibility last, for the reason `Record::Object` states.
                array.extensible.set(*extensible);
                Built::Object(array)
            }
            Record::Function {
                grammar,
                source,
                strict,
                environment: environment_serial,
                home,
                heritage,
                keys,
                proto,
                extensible,
                properties,
            } => {
                // An environment is carried for the two grammars the restore
                // *instantiates*: a record that names one for a grammar rebuilt by
                // evaluation would be a field nothing honours, which this format
                // refuses wherever it would otherwise drop it.
                if !grammar.carries_environment() && *environment_serial != NO_REF {
                    return Err(DecodeError::UnrebuildableEnvironment(format!(
                        "a function record in grammar {} names an environment, which its restore does not give it",
                        grammar.byte()
                    )));
                }
                let function = match grammar {
                    Grammar::Function => {
                        self.build_function(source, *strict, *environment_serial, *proto)?
                    }
                    Grammar::Class => self.build_class_function(source, *heritage, keys, *proto)?,
                    Grammar::Method => self.build_method_function(
                        source,
                        *proto,
                        *strict,
                        *home,
                        *environment_serial,
                    )?,
                    Grammar::Arrow => self.build_evaluated_function(source, *proto, *strict)?,
                };
                // A class's own evaluation made a `prototype` object whose members
                // carry the class's private brand; the tail just below replaces it
                // with the carried prototype, so the evaluation's is kept here for
                // the class-member records that read their method out of it.
                if *grammar == Grammar::Class
                    && let Some(prototype) = function
                        .object
                        .get_own_property(&JsString::from_utf8("prototype"))
                        .ok()
                        .flatten()
                        .and_then(|property| property.value())
                        .and_then(|value| value.as_object())
                {
                    self.pins.push(crux::heap::pin_handle(prototype));
                    self.class_prototypes.insert(index as u32, prototype);
                }
                // Recorded before the properties are defined, so an own
                // property that refers back to the function — `prototype`'s
                // `constructor`, for one — resolves to it rather than
                // restarting the build.
                self.remember(index, Built::Value(Value::Function(function)));
                // The environment this record named has been waiting for the
                // function to exist: its bindings may hold this very record, and
                // filling them now is what makes the two one function rather than
                // a function and a copy of it.
                self.settle_envs()?;
                let object = function.object;
                define_properties(self, object, properties)?;
                // Extensibility last, for the reason `Record::Object` states.
                object.extensible.set(*extensible);
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
            Record::Module { name, source } => {
                // Compiled in the reading realm, exactly as the engine's own module
                // path does it: a bootstrap context makes the realm current, and the
                // specifier the record did not keep is the name (a host re-registers
                // a restored module under its own specifier in any case).
                let realm = *self.realm;
                let source = JsString::from_utf16(source);
                let name = name.as_ref().map(|units| JsString::from_utf16(units));
                let specifier = name.clone().unwrap_or_else(|| JsString::from_utf8(""));
                self.agent.push_bootstrap_context(realm);
                let compiled = crate::module::parse_module(
                    self.agent,
                    &specifier,
                    name.as_ref(),
                    &source,
                    &[],
                );
                self.agent.execution_context_stack.pop();
                let module = compiled.map_err(|error| {
                    DecodeError::UnrebuildableFunction(format!(
                        "its module source could not be compiled: {error}"
                    ))
                })?;
                self.remember(index, Built::Module(module));
                Built::Module(module)
            }
            Record::ArrayBuffer {
                byte_length,
                resizable,
                immutable,
                max_byte_length,
                bytes,
                proto,
                extensible,
                properties,
            } => {
                let prototype = self.prototype(*proto)?;
                let object = JsObject::ordinary_object_create(prototype);
                // The engine's own AllocateArrayBuffer: the block, the state entry,
                // and (for a resizable buffer) the capacity a later resize needs
                // plus the flag crux's integer-indexed paths read.
                crate::builtins::array_buffer::allocate_array_buffer(
                    self.agent,
                    &object,
                    *byte_length as usize,
                    *resizable,
                    resizable.then_some(*max_byte_length as usize),
                )
                .map_err(|error| {
                    DecodeError::UnrebuildableBuffer(format!(
                        "the engine would not allocate it: {error}"
                    ))
                })?;
                {
                    let Some(cell) = self.agent.buffer_data.get(&object.id()) else {
                        return Err(DecodeError::UnrebuildableBuffer(
                            "the allocation left no buffer state".into(),
                        ));
                    };
                    let mut state = cell.borrow_mut();
                    state.shared.write(0, bytes).map_err(|_| {
                        DecodeError::UnrebuildableBuffer(
                            "its bytes do not fit the block it was allocated".into(),
                        )
                    })?;
                    if *immutable {
                        state.immutable = true;
                        state.shared.mark_immutable();
                    }
                }
                self.remember(index, Built::Object(object));
                define_properties(self, object, properties)?;
                // Extensibility last, for the reason `Record::Object` states.
                object.extensible.set(*extensible);
                Built::Object(object)
            }
            Record::TypedArray {
                element,
                buffer,
                byte_offset,
                byte_length,
                array_length,
                auto_length,
                proto,
                extensible,
                properties,
            } => {
                // The buffer first: a view is a window onto its block, and the
                // engine's own constructor path reads that block out of the
                // buffer's state rather than out of the view.
                let buffer_value = self.materialize(*buffer)?;
                let Some(buffer_object) = buffer_value.as_object() else {
                    return Err(DecodeError::UnrebuildableBuffer(
                        "the view's buffer is not an object".into(),
                    ));
                };
                if array_buffer_state(self.agent, &buffer_object).is_none() {
                    return Err(DecodeError::UnrebuildableBuffer(
                        "the view's buffer is not an ArrayBuffer".into(),
                    ));
                }
                let prototype = self.typed_array_prototype(*element)?;
                // The construction's `[[Prototype]]`: the record's own link when it
                // has one, and the kind's intrinsic when it has none — a view whose
                // link is *null* has no object to hand the constructor, and gets its
                // null link back below.
                let recorded_prototype = self.prototype(*proto)?;
                let construction_prototype = recorded_prototype.unwrap_or(prototype);
                // The engine's own view constructor: a byte offset, then the
                // element count — which an auto-length view does not have, because
                // its length is whatever the buffer's is.
                let args = if *auto_length {
                    vec![Value::Number(*byte_offset as f64)]
                } else {
                    vec![
                        Value::Number(*byte_offset as f64),
                        Value::Number(*array_length as f64),
                    ]
                };
                let value = crate::builtins::typed_array::typed_array_buffer_path(
                    self.agent,
                    construction_prototype,
                    *element,
                    &buffer_value,
                    &args,
                )
                .map_err(|error| {
                    DecodeError::UnrebuildableBuffer(format!(
                        "the engine would not make the view: {error}"
                    ))
                })?;
                let Some(object) = value.as_object() else {
                    return Err(DecodeError::UnrebuildableBuffer(
                        "the view's construction did not answer an object".into(),
                    ));
                };
                // A null link is the one the construction could not be handed, so
                // it is the one that has to be set afterwards.
                if recorded_prototype.is_none() {
                    object.set_prototype_of(None).map_err(|error| {
                        DecodeError::UnrebuildableBuffer(format!(
                            "its prototype could not be cleared: {error}"
                        ))
                    })?;
                }
                // The engine derived the geometry from the buffer; the record says
                // what it was. The two have to agree, or the blob describes a view
                // this build cannot make and the numbers are what would be wrong.
                let ObjectKind::IntegerIndexed(slots) = &object.kind else {
                    return Err(DecodeError::UnrebuildableBuffer(
                        "the view's construction did not answer a typed array".into(),
                    ));
                };
                let geometry = [
                    ("byteOffset", slots.byte_offset, *byte_offset as usize),
                    ("byteLength", slots.byte_length, *byte_length as usize),
                    ("length", slots.array_length, *array_length as usize),
                ];
                // An auto-length view's two lengths are the buffer's rather than the
                // record's, so the record carries none for it and none is checked.
                let checked = if *auto_length {
                    &geometry[..1]
                } else {
                    &geometry[..]
                };
                for (what, built, recorded) in checked {
                    if built != recorded {
                        return Err(DecodeError::UnrebuildableBuffer(format!(
                            "the view's {what} is {built}, the record says {recorded}"
                        )));
                    }
                }
                if slots.auto_length != *auto_length {
                    return Err(DecodeError::UnrebuildableBuffer(
                        "the view tracks the buffer, the record does not say so".into(),
                    ));
                }
                self.remember(index, Built::Object(object));
                define_properties(self, object, properties)?;
                // Extensibility last, for the reason `Record::Object` states.
                object.extensible.set(*extensible);
                Built::Object(object)
            }
            Record::Env {
                kind,
                transparent,
                outer,
                this_status,
                ..
            } => {
                // The outer link first: an environment is a chain, and the link is
                // materialized — as a shell of its own — before the record that
                // names it exists. `NO_REF` is the realm's own global environment,
                // which the reading realm has already rebuilt.
                let outer = match *outer {
                    NO_REF => None,
                    serial => Some(self.env_ref(serial)?),
                };
                let env = match *kind {
                    ENV_DECLARATIVE => {
                        let declarative = DeclarativeEnv::new(outer);
                        if *transparent {
                            declarative.mark_context_transparent();
                        }
                        Handle::new(EnvRecord::Declarative(declarative))
                    }
                    ENV_FUNCTION => {
                        // The function object and the new-target the record does
                        // not carry stay undefined, which is what a chain is
                        // consulted for nothing of: only an arrow reads those, and
                        // an arrow is rebuilt by evaluation in the reading realm.
                        // The this-binding status is known here because the
                        // constructor uses it to decide whether the record has a
                        // `this` binding at all.
                        let function = FunctionEnv::new(
                            outer,
                            Value::Undefined,
                            Value::Undefined,
                            *this_status == THIS_LEXICAL,
                        );
                        if *transparent {
                            function.declarative.mark_context_transparent();
                        }
                        Handle::new(EnvRecord::Function(function))
                    }
                    other => {
                        return Err(DecodeError::UnrebuildableEnvironment(format!(
                            "environment kind byte {other} is not one of this format's"
                        )));
                    }
                };
                // The shell is recorded before its bindings are put back, so a
                // binding that holds a function whose [[Environment]] is this very
                // record resolves to it rather than restarting the chain. The fill
                // waits until the function that asked for the shell is itself
                // recorded — see `settle_envs`.
                self.remember(index, Built::Env(env));
                self.unfilled_envs.push(index as u32);
                Built::Env(env)
            }
            Record::ClassMethod {
                class,
                key,
                form,
                home,
                proto,
                extensible,
                properties,
            } => {
                // The class is materialized **first**: reading the member out of
                // the class's own evaluation means the class build runs the tail
                // that defines this very member on its prototype, and that nested
                // build is the one the prototype's property gets. If it happened,
                // this record's value is that build rather than a second method.
                self.materialize(*class)?;
                if let Some(made) = self.made[index] {
                    return Ok(made);
                }
                let function = self.build_class_member(*class, *key, *form, *home)?;
                // The class's evaluation derived the member's own prototype link;
                // the record's is what the blob was written with, so it wins — the
                // same rule a function record's prototype follows.
                let prototype = self.prototype(*proto)?.ok_or_else(|| {
                    DecodeError::UnrebuildableFunction("the record names no prototype".into())
                })?;
                function
                    .object
                    .set_prototype_of(Some(prototype))
                    .map_err(|error| {
                        DecodeError::UnrebuildableFunction(format!(
                            "its prototype could not be set: {error}"
                        ))
                    })?;
                self.remember(index, Built::Value(Value::Function(function)));
                let object = function.object;
                define_properties(self, object, properties)?;
                // Extensibility last, for the reason `Record::Object` states.
                object.extensible.set(*extensible);
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
                define_properties(self, object, properties)?;
                // Extensibility last, for the reason `Record::Object` states.
                object.extensible.set(*extensible);
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
                define_properties(self, object, properties)?;
                // Extensibility last, for the reason `Record::Object` states.
                object.extensible.set(*extensible);
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
        // The value the load hands the host twice: once to build the callback
        // from, and once to record the function it rebuilt. `api::Local` is a
        // copy, so this binding is what keeps the two the same value.
        let host_data = data.map(crate::api::Local);
        let callback = self
            .host
            .and_then(|host| host.callback_at(pointer, host_data))
            .ok_or(DecodeError::NoHostCallback(index as usize))?;
        let prototype = self.prototype(proto)?;
        // SAFETY: `api::Isolate` is `repr(C)` with the agent at offset 0, so the
        // address of the agent is the address of the isolate it belongs to. The
        // callback view the host reads carries that pointer, and the host's
        // callback scope is built from it — the same identity
        // `api::Isolate::get_current` relies on.
        let isolate = self.agent as *mut Agent as *mut crate::api::Isolate;
        let function =
            crate::api::host_function(isolate, std::rc::Rc::new(callback), None, prototype)
                .map_err(|error| {
                    DecodeError::UnrebuildableFunction(format!(
                        "the host could not make a function for external reference {index}: {error}"
                    ))
                })?;
        // Tell the host what it just made: this pair is how the write side names
        // a function, so a snapshot written *of this load* can carry what it
        // restored rather than refusing it.
        if let Some(host) = self.host {
            host.callback_rebuilt(&function, pointer, host_data);
        }
        Ok(function)
    }

    /// An evaluated-function record: the source is evaluated as an expression in
    /// the realm being restored, and the function it produces is the value.
    ///
    /// An arrow's record: the source is evaluated as an expression in the realm it
    /// is being restored into, because an arrow *is* an expression and the engine's
    /// own evaluation is what gives a closure its lexical `[[ThisMode]]`, its
    /// deferred absence of a `prototype`, and its strictness.
    ///
    /// The divergence is the module docs': the evaluation is the **reading**
    /// realm's, so a captured `this` or a free name is that realm's global rather
    /// than the scope the original closed over — the arrow record's stated limit.
    fn build_evaluated_function(
        &mut self,
        source: &[u16],
        proto: u32,
        strict: bool,
    ) -> Result<Handle<Function>, DecodeError> {
        let proto = self.prototype(proto)?.ok_or_else(|| {
            DecodeError::UnrebuildableFunction("the record names no prototype".into())
        })?;
        let text = String::from_utf16(source).map_err(|_| {
            DecodeError::UnrebuildableFunction("its arrow source is not valid UTF-16".into())
        })?;
        let realm = *self.realm;
        // An arrow's source is an expression, so it needs the parenthesis wherever
        // it appears. The `strict` byte decides whether the evaluated code is
        // strict — an arrow inherits the strictness of the code around it, and the
        // evaluation has no other way to know. Popped on every path.
        let prefix = if strict { "\"use strict\"; " } else { "" };
        self.agent.push_bootstrap_context(realm);
        let value = self.agent.run_script(&format!("{prefix}({text})"));
        self.agent.execution_context_stack.pop();
        let value = value.map_err(|error| {
            DecodeError::UnrebuildableFunction(format!(
                "its arrow source could not be evaluated: {error}"
            ))
        })?;
        let function = value.as_function().ok_or_else(|| {
            DecodeError::UnrebuildableFunction("its source did not evaluate to a function".into())
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

    /// A method record, rebuilt by **instantiating** the MethodDefinition its source
    /// is rather than by evaluating it.
    ///
    /// Evaluating `({<source>})` would evaluate the source's key expression, and a
    /// computed key may read a name that exists only in the module which defined the
    /// method, which the reading realm's global does not have. The key is not
    /// needed to instantiate a method: a key is only what *installs* a method on an
    /// object, and this record wants the method alone (its `name` arrives with the
    /// record's own properties). So the AST is taken from the parse and the method
    /// instantiated from it — the same `instantiate_method`/`instantiate_accessor`
    /// calls `build_class` makes for a class's elements.
    ///
    /// The parse is a script, so the method's spans index the text the parse was
    /// given; the bootstrap context the instantiation runs under carries that same
    /// text as its `source`, which is what `capture_source` resolves the method's
    /// `[[SourceText]]` from — so `toString`, and a second snapshot of the restored
    /// method, are unaffected. Strictness is handed to the instantiation rather
    /// than written into the source as a directive, which is what keeps the spans
    /// exactly the parsed text's. The environment is the chain the record carried,
    /// for the reason a function record's is.
    fn build_method_function(
        &mut self,
        source: &[u16],
        proto: u32,
        strict: bool,
        home: u32,
        environment: u32,
    ) -> Result<Handle<Function>, DecodeError> {
        let proto = self.prototype(proto)?.ok_or_else(|| {
            DecodeError::UnrebuildableFunction("the record names no prototype".into())
        })?;
        let text = String::from_utf16(source).map_err(|_| {
            DecodeError::UnrebuildableFunction("its method source is not valid UTF-16".into())
        })?;
        // The object literal is the only form a MethodDefinition has.
        let wrapped = format!("({{{text}}})");
        let program = parser::parse_script(&wrapped).map_err(|error| {
            DecodeError::UnrebuildableFunction(format!("its method source does not parse: {error}"))
        })?;
        let definition = method_definition(&program).ok_or_else(|| {
            DecodeError::UnrebuildableFunction("its source is not one method definition".into())
        })?;
        let realm = *self.realm;
        let environment = self.function_environment(environment)?;
        let home = match home {
            NO_REF => None,
            home => Some(self.materialize(home)?),
        };
        self.agent.push_bootstrap_context(realm);
        if let Some(context) = self.agent.execution_context_stack.last_mut() {
            context.source = Some(JsString::from_utf8(&wrapped));
        }
        let value = match definition {
            MethodDefinition::Method(function) => {
                crate::function::instantiate_method(self.agent, function, environment, strict)
            }
            // An accessor's source is the record's own text — the accessor
            // definition, which is what the writer captured — so the rebuild hands
            // it over directly rather than resolving a span against the wrapper
            // it parsed.
            MethodDefinition::Get(body) => crate::function::instantiate_accessor(
                self.agent,
                Vec::new(),
                body,
                environment,
                strict,
                Some(JsString::from_utf8(&text)),
            ),
            // A setter's parameter list is the engine's own one-element form, the
            // same shape `build_class` builds for a class setter.
            MethodDefinition::Set(param, init, body) => crate::function::instantiate_accessor(
                self.agent,
                vec![syntax::ast::BindingElement {
                    pattern: param.clone(),
                    init: init.cloned(),
                    rest: false,
                    span: body.span,
                }],
                body,
                environment,
                strict,
                Some(JsString::from_utf8(&text)),
            ),
        };
        self.agent.execution_context_stack.pop();
        let value = value.map_err(|error| {
            DecodeError::UnrebuildableFunction(format!(
                "its method source could not be instantiated: {error}"
            ))
        })?;
        let function = value.as_function().ok_or_else(|| {
            DecodeError::UnrebuildableFunction("its source did not make a method".into())
        })?;
        if let Some(home) = home {
            crate::function::make_method(self.agent, &Value::Function(function), home).map_err(
                |error| {
                    DecodeError::UnrebuildableFunction(format!(
                        "its [[HomeObject]] could not be set: {error}"
                    ))
                },
            )?;
        }
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

    /// A class record, rebuilt by the engine's own class evaluation with the
    /// definition-time **inputs** the record carries.
    ///
    /// The source is parsed and handed to the evaluation rather than run as a
    /// script, because the keys and the heritage have to be *supplied*: a computed
    /// key's expression and an `extends` expression may read a name that exists
    /// only in the module which defined the class, and a script would evaluate
    /// them against the reading realm's global.
    /// `class_definition_evaluation_with_keys` is the engine's own entry for
    /// exactly that case — its resumable-VM path, where the definition-time
    /// expressions were evaluated before the definition was built — and the class
    /// scope, private environment and strictness it sets up are the ones a script
    /// would have got.
    ///
    /// The parse is a script, so the class's spans index the text the parse was
    /// given; the bootstrap context the evaluation runs under carries that same
    /// text as its `source`, which is what `capture_source` resolves the class's
    /// `[[SourceText]]` from — so `toString`, and a second snapshot of the
    /// restored class, are unaffected by the evaluation having been driven here.
    fn build_class_function(
        &mut self,
        source: &[u16],
        heritage: u32,
        keys: &[u32],
        proto: u32,
    ) -> Result<Handle<Function>, DecodeError> {
        let proto = self.prototype(proto)?.ok_or_else(|| {
            DecodeError::UnrebuildableFunction("the record names no prototype".into())
        })?;
        let text = String::from_utf16(source).map_err(|_| {
            DecodeError::UnrebuildableFunction("its class source is not valid UTF-16".into())
        })?;
        // Both are materialized before the evaluation, which can allocate.
        let heritage = match heritage {
            NO_REF => None,
            heritage => Some(self.materialize(heritage)?),
        };
        let mut resolved = Vec::with_capacity(keys.len());
        for serial in keys {
            let value = self.materialize(*serial)?;
            let key = crate::context::to_property_key(self.agent, &value).map_err(|error| {
                DecodeError::UnrebuildableFunction(format!(
                    "a computed key is not a property key: {error}"
                ))
            })?;
            resolved.push(Some(key));
        }
        let realm = *self.realm;
        // A class source is only valid as an expression, so it is wrapped — and
        // the parser keeps the grouping paren, which `class_expression` steps
        // through.
        let wrapped = format!("({text})");
        let program = parser::parse_script(&wrapped).map_err(|error| {
            DecodeError::UnrebuildableFunction(format!("its class source does not parse: {error}"))
        })?;
        let class = class_expression(&program).ok_or_else(|| {
            DecodeError::UnrebuildableFunction("its class source is not a class".into())
        })?;
        self.agent.push_bootstrap_context(realm);
        if let Some(context) = self.agent.execution_context_stack.last_mut() {
            context.source = Some(JsString::from_utf8(&wrapped));
        }
        let value = crate::class::class_definition_evaluation_with_keys(
            self.agent, class, class.name, heritage, &resolved,
        );
        self.agent.execution_context_stack.pop();
        let value = value.map_err(|error| {
            DecodeError::UnrebuildableFunction(format!(
                "its class source could not be evaluated: {error}"
            ))
        })?;
        let function = value.as_function().ok_or_else(|| {
            DecodeError::UnrebuildableFunction(
                "its class source did not evaluate to a class".into(),
            )
        })?;
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

    /// A class member, read out of the class the evaluation made.
    ///
    /// The class record has already been materialized (see the `Record::ClassMethod`
    /// arm), so its own evaluation prototype is stashed: that object is where a
    /// method reading a private name has to come from, because the private brand
    /// it closes over belongs to that evaluation and to no other. The member is
    /// then re-homed to the record's [[HomeObject]] — the carried prototype — so
    /// `super` resolves through the object the graph holds rather than through the
    /// evaluation's orphan.
    fn build_class_member(
        &mut self,
        class: u32,
        key: u32,
        form: Form,
        home: u32,
    ) -> Result<Handle<Function>, DecodeError> {
        let key = self.materialize(key)?;
        let key = crate::context::to_property_key(self.agent, &key).map_err(|error| {
            DecodeError::UnrebuildableFunction(format!(
                "its member key is not a property key: {error}"
            ))
        })?;
        let home_value = match home {
            NO_REF => None,
            home => Some(self.materialize(home)?),
        };
        // The holder is the object the member was defined on: the class itself for
        // a static, and the class's own evaluation prototype for an instance
        // method — which is why a member whose class's prototype was replaced is
        // not carried this way at all (see `class_member`).
        let holder = match home_value.as_ref().map(|value| value.kind()) {
            Some(ValueKind::Function(class_function)) => class_function.object,
            _ => *self.class_prototypes.get(&class).ok_or_else(|| {
                DecodeError::UnrebuildableFunction(
                    "its class's own evaluation kept no prototype to read it from".into(),
                )
            })?,
        };
        let property = holder
            .get_own_property_key(&key)
            .map_err(|_| DecodeError::Truncated)?
            .ok_or_else(|| {
                DecodeError::UnrebuildableFunction(
                    "its class's own evaluation has no such member".into(),
                )
            })?;
        let member = match (&property.kind, form) {
            (PropertyKind::Data { value, .. }, Form::Method) => value.as_function(),
            (PropertyKind::Accessor { get, .. }, Form::Getter) => {
                get.as_ref().and_then(|value| value.as_function())
            }
            (PropertyKind::Accessor { set, .. }, Form::Setter) => {
                set.as_ref().and_then(|value| value.as_function())
            }
            _ => None,
        }
        .ok_or_else(|| {
            DecodeError::UnrebuildableFunction(
                "its class's own evaluation did not define the member it is read as".into(),
            )
        })?;
        if let Some(home) = home_value {
            crate::function::make_method(self.agent, &Value::Function(member), home).map_err(
                |error| {
                    DecodeError::UnrebuildableFunction(format!(
                        "its [[HomeObject]] could not be set: {error}"
                    ))
                },
            )?;
        }
        Ok(member)
    }

    /// A function record, rebuilt from the source text it carries.
    ///
    /// The parse is what makes this record different from every other one. The
    /// environment it is instantiated in is the chain the record carried, so a
    /// free name in the body resolves where it resolved when the function was
    /// created; a record that carried none — the realm's own global environment —
    /// is instantiated in the reading realm's, which is also every blob written
    /// before the chain was carried. A source the parser cannot read as a
    /// function expression, or a function whose scope the writing realm had no
    /// source for, is refused by name rather than restored wrong.
    fn build_function(
        &mut self,
        source: &[u16],
        strict: bool,
        environment: u32,
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
        let environment = self.function_environment(environment)?;
        // A bootstrap execution context makes the realm current and gives the
        // registration the running context it reads; it is popped on every
        // path, so a failed restore leaves the agent's stack as it found it.
        self.agent.push_bootstrap_context(realm);
        let value = crate::function::instantiate_function_from_source(
            self.agent,
            &parsed,
            environment,
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
            Built::Module(_) | Built::Env(_) => {
                return Err(DecodeError::UnrebuildableEnvironment(format!(
                    "serial {serial} names a record that is not a prototype object"
                )));
            }
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
            Built::Module(module) => crux::heap::pin_handle(*module),
            Built::Env(env) => crux::heap::pin_handle(*env),
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
        // An accessor's **absent** half is the `NO_REF` sentinel, not a record: a
        // getter with no setter is a property with no `[[Set]]`, and materializing
        // the sentinel asks for a record that was never written. The half has to
        // stay absent, which is also what a re-define needs to leave it undefined.
        let half = |builder: &mut Builder<'_>, serial: u32| match serial {
            NO_REF => Ok(None),
            serial => Ok(Some(builder.materialize(serial)?)),
        };
        let get = if accessor {
            half(builder, property.first)?
        } else {
            None
        };
        let set = if accessor {
            half(builder, property.second)?
        } else {
            None
        };
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
            get,
            set,
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

    /// A slot's item list from the values a test attaches — the shape these tests
    /// have always written, now that a slot can hold a module too.
    fn slot_items(values: &[Value]) -> Vec<SnapshotItem> {
        values.iter().copied().map(SnapshotItem::from).collect()
    }

    /// The value an item is, for the tests that read a slot back as values.
    fn item_value(item: SnapshotItem) -> Value {
        item.value().expect("a value item")
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

    /// A non-extensible object's — or function's — properties come back.
    ///
    /// An object's own properties exist **before** it is made non-extensible, so
    /// a record's flag has to be applied after its property list: the reader used
    /// to apply it first, and `[[DefineOwnProperty]]` refuses to add a property to
    /// an object that is already non-extensible — a refusal
    /// (`define_property_key` answering `Ok(false)`) the reader discarded, so
    /// every property of a frozen object was silently lost. A host's frozen
    /// namespace is the case that found it (deno freezes `Deno.core` and
    /// `Deno.core.ops`).
    #[test]
    fn a_non_extensible_values_properties_come_back() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();

        // An object with data properties, frozen.
        let frozen = context
            .try_eval("Object.freeze({ a: 1, b: 2 })")
            .expect("a frozen object")
            .into_value();
        let back = round_trip(&isolate, &realm, frozen);
        let object = back.as_object().expect("an object");
        assert!(
            !object.extensible.get(),
            "the object came back non-extensible"
        );
        let keys = object.own_property_keys().expect("its own keys");
        assert_eq!(
            keys.len(),
            2,
            "its properties came back, not just the shell"
        );
        assert_eq!(number_of_key(&object, "b"), Some(2.0));
        assert_eq!(number_of_key(&object, "a"), Some(1.0));

        // An array with an own extra property, frozen: the array arm applies the
        // flag after its elements and extras for the same reason.
        let frozen = context
            .try_eval("(() => { const a = [1, 2]; a.tag = 7; Object.freeze(a); return a; })()")
            .expect("a frozen array")
            .into_value();
        let back = round_trip(&isolate, &realm, frozen);
        let array = back.as_object().expect("an array");
        assert!(
            !array.extensible.get(),
            "the array came back non-extensible"
        );
        assert_eq!(array_length(&array).expect("a length") as u64, 2);
        assert_eq!(number_of_key(&array, "tag"), Some(7.0));

        // A function with an own property that the fresh function does not
        // already have, made non-extensible: the function, bind and host-callback
        // arms apply the flag after their properties for the same reason, and a
        // property the rebuild must *add* is what tells the order apart (`name`
        // and `length` already exist, so re-defining those is allowed either way).
        let function = context
            .try_eval("(function named() { return 1; })")
            .expect("a function")
            .into_value();
        let object = match function.kind() {
            ValueKind::Function(function) => function.object,
            other => panic!("a function, not {other:?}"),
        };
        object
            .create_data_property_key(
                &PropertyKey::String(string::intern_utf8("tag")),
                Value::Number(7.0),
            )
            .expect("the function's own property");
        object.extensible.set(false);

        let back = round_trip(&isolate, &realm, function);
        let restored = crate::context::as_object(&back).expect("a function's object part");
        assert!(
            !restored.extensible.get(),
            "the function came back non-extensible"
        );
        assert_eq!(
            number_of_key(&restored, "tag"),
            Some(7.0),
            "the function's own property came back"
        );
        assert_eq!(
            string_of_key(&restored, "name").as_deref(),
            Some("named"),
            "and its name came with it"
        );
    }

    /// A named own property of an object, for the assertions above.
    fn own_property(object: &Handle<JsObject>, name: &str) -> Property {
        object
            .get_own_property_key(&PropertyKey::String(string::intern_utf8(name)))
            .expect("an own property read")
            .unwrap_or_else(|| panic!("the own property {name}"))
    }

    fn number_of_key(object: &Handle<JsObject>, name: &str) -> Option<f64> {
        match &own_property(object, name).kind {
            PropertyKind::Data { value, .. } => value.as_number(),
            PropertyKind::Accessor { .. } => None,
        }
    }

    fn string_of_key(object: &Handle<JsObject>, name: &str) -> Option<String> {
        match &own_property(object, name).kind {
            PropertyKind::Data { value, .. } => {
                value.as_string().map(|text| text.to_string_lossy())
            }
            PropertyKind::Accessor { .. } => None,
        }
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

    /// A hole travels as the **absence** it is — `NO_REF` for an element the record
    /// does not have — and it must not come back as `undefined`, which is an
    /// element that is there: `1 in [1, , 3]` is false where
    /// `1 in [1, undefined, 3]` is true. The length is the record's own too, so a
    /// **trailing** hole keeps its index rather than being truncated away.
    #[test]
    fn a_hole_comes_back_a_hole() {
        let (isolate, realm) = fixture();
        let prototype = realm
            .intrinsics
            .array_prototype()
            .and_then(|value| value.as_object());
        let present = |array: &Handle<JsObject>, index: u64| {
            array
                .get_own_property_key(&PropertyKey::String(string::index_atom(index)))
                .expect("a read")
                .is_some()
        };
        // A hole between two elements, a trailing hole, nothing but holes, and —
        // the case that makes the difference testable rather than incidental — a
        // *present* `undefined` beside a hole, where only presence tells them apart.
        for (length, set, expected) in [
            (
                3.0,
                vec![(0, Value::Number(1.0)), (2, Value::Number(3.0))],
                vec![true, false, true],
            ),
            (3.0, vec![(0, Value::Number(1.0))], vec![true, false, false]),
            (2.0, vec![], vec![false, false]),
            (
                3.0,
                vec![(0, Value::Undefined), (2, Value::Undefined)],
                vec![true, false, true],
            ),
        ] {
            let array = JsObject::array_create(prototype, length).expect("an array");
            for (index, value) in &set {
                array
                    .create_data_property_index(*index, *value)
                    .expect("an element");
            }

            let back = round_trip(&isolate, &realm, Value::Object(array));
            let back = back.as_object().expect("an array");
            let found: Vec<bool> = (0..expected.len() as u64)
                .map(|index| present(&back, index))
                .collect();
            assert_eq!(found, expected, "length {length}, elements {set:?}");
            assert_eq!(
                array_length(&back).expect("a length") as f64,
                length,
                "length {length}, elements {set:?}"
            );
        }
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

    /// A slot asked to carry its realm's global restores that state **onto** the
    /// reading realm's global: what the writer installed is back, and a property
    /// that held the writer's global holds the reader's after the restore, which
    /// is what makes `globalThis` a self-reference rather than a copy. A slot that
    /// did not ask for it carries nothing, which is `encode`'s shape.
    #[test]
    fn a_carried_realm_global_lands_on_the_reading_realms_global() {
        let mut writer = api::Isolate::new();
        let writer_context = api::Context::new(&mut writer).expect("a realm");
        let realm = *writer_context.realm();
        writer_context
            .try_eval("globalThis.__carried = { n: 7 }; globalThis.__self = globalThis;")
            .expect("install a global");

        let slots = [Slot {
            index: 0,
            realm,
            items: &[],
            realm_global: true,
        }];
        let blob = encode_slots(agent_of(&writer), &slots, &[], None).expect("a blob");

        let mut reader = api::Isolate::new();
        let reader_context = api::Context::new(&mut reader).expect("a realm");
        let reader_realm = *reader_context.realm();
        let items = decode_slot(agent_mut(&reader), &reader_realm, &blob, 0, &[], None)
            .expect("a blob of this tree")
            .expect("slot 0");
        assert!(items.is_empty(), "the slot attached no items of its own");

        // The carried global **is** this realm's global rather than a copy of it:
        // `globalThis`, and a property that held the writing realm's global, both
        // name this realm's global object. The JavaScript reads below cannot tell
        // a faithful copy from the object itself (they move together), so the
        // identity is asserted against the realm handle.
        let realm_global = Value::Object(reader_realm.global_object);
        let global_id = reader_realm.global_object.id();
        for key in ["globalThis", "__self"] {
            let value = crate::context::get_property_key(
                agent_mut(&reader),
                &realm_global,
                &PropertyKey::from_utf8(key),
                realm_global,
            )
            .expect("a read");
            assert_eq!(
                value.as_object().map(|object| object.id()),
                Some(global_id),
                "{key} names the reading realm's global"
            );
        }

        let back = reader_context
            .try_eval("globalThis.__carried.n + ',' + (globalThis.__self === globalThis)")
            .expect("the state came back")
            .into_value();
        assert_eq!(
            back.as_string()
                .map(|text| text.to_string_lossy())
                .as_deref(),
            Some("7,true")
        );

        // A slot that did not ask for the global carries none of it: the same
        // writer value through `encode` leaves the reading global as it was.
        let plain = encode_value(&writer, &realm, Value::Number(1.0));
        let mut other = api::Isolate::new();
        let other_context = api::Context::new(&mut other).expect("a realm");
        let other_realm = *other_context.realm();
        decode_slot(agent_mut(&other), &other_realm, &plain, 0, &[], None)
            .expect("a blob of this tree")
            .expect("slot 0");
        let absent = other_context
            .try_eval("typeof globalThis.__carried")
            .expect("a read")
            .into_value();
        assert_eq!(
            absent
                .as_string()
                .map(|text| text.to_string_lossy())
                .as_deref(),
            Some("undefined")
        );
    }

    /// A top-level `let`, `const` or `class` is a **global lexical** binding: it
    /// lives in the global environment record's declarative half rather than on
    /// the global object, so carrying the global object alone loses it. This is
    /// `will_snapshot2`'s shape in the crate we stand in for — `let a = 1 + 2`
    /// before the snapshot, `a` read after the restore.
    #[test]
    fn a_global_lexical_binding_travels_with_the_carried_global() {
        let mut writer = api::Isolate::new();
        let writer_context = api::Context::new(&mut writer).expect("a realm");
        let realm = *writer_context.realm();
        writer_context
            .try_eval("let carried = 41 + 1; const fixed = 7;")
            .expect("declare two global lexical bindings");

        let slots = [Slot {
            index: 0,
            realm,
            items: &[],
            realm_global: true,
        }];
        let blob = encode_slots(agent_of(&writer), &slots, &[], None).expect("a blob");

        let mut reader = api::Isolate::new();
        let reader_context = api::Context::new(&mut reader).expect("a realm");
        let reader_realm = *reader_context.realm();
        decode_slot(agent_mut(&reader), &reader_realm, &blob, 0, &[], None)
            .expect("a blob of this tree")
            .expect("slot 0");

        let read = |source: &str| {
            reader_context
                .try_eval(source)
                .expect("a read")
                .into_value()
        };
        assert_eq!(read("carried").as_number(), Some(42.0));
        assert_eq!(read("fixed").as_number(), Some(7.0));

        // The flags are the binding's own rather than a default: a `let` is
        // still writable, and the write is **visible**, which a binding restored
        // immutable — the mutability bit dropped — would not make.
        reader_context
            .try_eval("carried = 5")
            .expect("a `let` is writable");
        assert_eq!(read("carried").as_number(), Some(5.0));

        // A `const` is still immutable: the format carries the attribute the
        // writer's `CreateImmutableBinding` set, so the write throws rather than
        // silently doing nothing.
        match reader_context.try_eval("fixed = 8") {
            Ok(_) => panic!("a restored `const` accepted an assignment"),
            Err(error) => assert_eq!(error.kind, crux::error::ErrorKind::TypeError),
        }
        assert_eq!(read("fixed").as_number(), Some(7.0));
    }

    /// A binding in its temporal dead zone has no value to write, and the format
    /// spells that `NO_REF`. A restore that gave it `undefined` instead would
    /// answer a read where the writing realm's code would have thrown.
    #[test]
    fn a_carried_binding_in_the_temporal_dead_zone_stays_uninitialized() {
        let mut writer = api::Isolate::new();
        let writer_context = api::Context::new(&mut writer).expect("a realm");
        let realm = *writer_context.realm();
        // A script cannot leave this behind — its own `let` is initialized by the
        // time it returns — but the engine's instantiation can: created, never
        // initialized. That is the state the format's `NO_REF` names.
        realm
            .global_env
            .create_mutable_binding(&JsString::from_utf8("pending"), false)
            .expect("a binding in the temporal dead zone");

        let slots = [Slot {
            index: 0,
            realm,
            items: &[],
            realm_global: true,
        }];
        let blob = encode_slots(agent_of(&writer), &slots, &[], None).expect("a blob");

        let mut reader = api::Isolate::new();
        let reader_context = api::Context::new(&mut reader).expect("a realm");
        let reader_realm = *reader_context.realm();
        decode_slot(agent_mut(&reader), &reader_realm, &blob, 0, &[], None)
            .expect("a blob of this tree")
            .expect("slot 0");

        // The name has to exist *and* be uninitialized: a binding that did not
        // travel at all throws the same error kind with a different reason, so
        // the message is what tells "before initialization" from "not defined".
        match reader_context.try_eval("pending") {
            Ok(value) => panic!("a read answered {}", value.into_value()),
            Err(error) => {
                assert_eq!(error.kind, crux::error::ErrorKind::ReferenceError);
                assert!(
                    error.message.contains("before initialization"),
                    "the binding came back uninitialized, not absent: {}",
                    error.message
                );
            }
        }
    }

    /// The bindings are part of the walk, not a side table: a global lexical
    /// binding the format cannot carry refuses the snapshot by name rather than
    /// being dropped and silently missing after the restore.
    #[test]
    fn a_global_lexical_binding_the_format_cannot_carry_refuses_the_snapshot() {
        let mut writer = api::Isolate::new();
        let writer_context = api::Context::new(&mut writer).expect("a realm");
        let realm = *writer_context.realm();
        writer_context
            .try_eval("let trapped = new Proxy({}, {});")
            .expect("a global the walk cannot carry");

        let slots = [Slot {
            index: 0,
            realm,
            items: &[],
            realm_global: true,
        }];
        let error =
            encode_slots(agent_of(&writer), &slots, &[], None).expect_err("the walk refuses");
        assert_eq!(
            error.type_name, "a proxy",
            "the refusal names what could not travel"
        );
    }

    /// The context table is structural: two slots keep their own items, and a
    /// slot the blob does not name answers `None` rather than an empty list.
    #[test]
    fn two_slots_keep_their_own_items() {
        let (isolate, realm) = fixture();
        let first = slot_items(&[Value::Number(1.0), Value::Number(2.0)]);
        let second = slot_items(&[Value::String(Handle::new(JsString::from_utf8("two")))]);
        let slots = [
            Slot {
                index: 0,
                realm,
                items: &first,
                realm_global: false,
            },
            Slot {
                index: 3,
                realm,
                items: &second,
                realm_global: false,
            },
        ];
        let blob = encode_slots(agent_of(&isolate), &slots, &[], None).expect("a blob");

        let read = |slot| {
            decode_slot(agent_mut(&isolate), &realm, &blob, slot, &[], None)
                .expect("a blob of this tree")
        };
        let zero = read(0).expect("slot 0");
        assert_eq!(zero.len(), 2);
        assert_eq!(item_value(zero[1]).as_number(), Some(2.0));
        let three = read(3).expect("slot 3");
        assert_eq!(three.len(), 1);
        assert_eq!(
            item_value(three[0])
                .as_string()
                .map(|text| text.to_string_lossy()),
            Some("two".to_string())
        );
        assert!(read(1).is_none(), "a slot the blob does not name");
    }

    /// A module a host attached is an item a blob carries rather than one it
    /// skips, and the item attached after it keeps its own index.
    ///
    /// A module is not a language value, so there is no `Value` a slot's item list
    /// could spell it with — which is exactly why the list is an item list and not
    /// a value list: a skipped module would hand the host's own `get(index)` the
    /// record that belongs to a later index.
    #[test]
    fn a_module_is_an_item_like_any_other() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let module =
            api::Module::compile(&context, "deno:core", "export const x = 1;").expect("a module");
        let items = [
            SnapshotItem::Module(module),
            SnapshotItem::Value(Value::Number(9.0)),
        ];
        let slots = [Slot {
            index: 0,
            realm,
            items: &items,
            realm_global: false,
        }];
        let blob = encode_slots(agent_of(&isolate), &slots, &[], None).expect("a blob");

        let back = decode_slot(agent_mut(&isolate), &realm, &blob, 0, &[], None)
            .expect("a blob of this tree")
            .expect("slot 0");
        assert_eq!(back.len(), 2, "a module is an item, not a gap");
        match back[0] {
            SnapshotItem::Module(module) => assert_eq!(
                module.source_text().as_deref(),
                Some("export const x = 1;"),
                "the record carries the source the module was compiled from"
            ),
            SnapshotItem::Value(_) => panic!("the module came back as a language value"),
        }
        assert_eq!(item_value(back[1]).as_number(), Some(9.0));
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
        let items = slot_items(&[value]);
        let slots = [Slot {
            index: 1,
            realm: *second.realm(),
            items: &items,
            realm_global: false,
        }];
        let blob = encode_slots(agent_of(&isolate), &slots, &[], None).expect("a blob");

        let back = decode_slot(agent_mut(&isolate), second.realm(), &blob, 1, &[], None)
            .expect("a blob of this tree")
            .expect("slot 1");
        assert_eq!(
            item_value(back[0])
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
        let items = slot_items(&[value]);
        let slots = [Slot {
            index: 0,
            realm: *first.realm(),
            items: &items,
            realm_global: false,
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
        let items = slot_items(&[Value::Object(object), external]);
        let slots = [Slot {
            index: 0,
            realm,
            items: &items,
            realm_global: false,
        }];
        let table = [0x9999usize, pointer as usize, other as usize];
        let blob = encode_slots(agent_of(&isolate), &slots, &table, None).expect("a blob");

        let back = decode_slot(agent_mut(&isolate), &realm, &blob, 0, &table, None)
            .expect("a blob of this tree")
            .expect("slot 0");
        let held = item_value(back[0]).as_object().expect("an object");
        assert_eq!(
            api::External::from(item_value(back[1])).value(),
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
        let items = slot_items(&[external]);
        let slots = [Slot {
            index: 0,
            realm,
            items: &items,
            realm_global: false,
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
        let items = slot_items(&[external]);
        let slots = [Slot {
            index: 0,
            realm,
            items: &items,
            realm_global: false,
        }];
        // The pointer sits at index 1 of a two-entry table, so an index that was
        // ignored would resolve to the wrong entry rather than to nothing.
        let blob =
            encode_slots(agent_of(&isolate), &slots, &[first, pointer], None).expect("a blob");

        assert_eq!(
            decode_slot(agent_mut(&isolate), &realm, &blob, 0, &[], None).err(),
            Some(DecodeError::ExternalIndexOutOfRange { index: 1, count: 0 })
        );
        assert_eq!(
            decode_slot(agent_mut(&isolate), &realm, &blob, 0, &[first], None).err(),
            Some(DecodeError::ExternalIndexOutOfRange { index: 1, count: 1 })
        );
        // And the host's table is what an index resolves against, entry by
        // entry, which is the contract: the blob carries the index, never the
        // address.
        let back = decode_slot(agent_mut(&isolate), &realm, &blob, 0, &[first, other], None)
            .expect("a blob of this tree")
            .expect("slot 0");
        assert_eq!(
            api::External::from(item_value(back[0])).value() as usize,
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
        let items = slot_items(&[Value::Function(callback), Value::Function(bound)]);
        let slots = [Slot {
            index: 0,
            realm,
            items: &items,
            realm_global: false,
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
            call_restored(&isolate, &realm, item_value(back[0]), "").as_number(),
            Some(42.0),
            "the restored function calls the host's callback"
        );
        // The object part is the function's: what the host built it with comes
        // back as written, not recomputed.
        let name = item_value(back[0])
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
            call_restored(&isolate, &realm, item_value(back[1]), "").as_number(),
            Some(42.0),
            "a bind whose target is a host callback round trips too"
        );
        assert_eq!(
            item_value(back[0])
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
        let items = slot_items(&[Value::Function(callback)]);
        let slots = [Slot {
            index: 0,
            realm,
            items: &items,
            realm_global: false,
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
        item_value(back[0]).as_function().expect("a function");
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
        let items = slot_items(&[Value::Function(callback)]);
        let slots = [Slot {
            index: 0,
            realm,
            items: &items,
            realm_global: false,
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
        let items = slot_items(&[Value::Function(callback)]);
        let slots = [Slot {
            index: 0,
            realm,
            items: &items,
            realm_global: false,
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
        let items = slot_items(&[Value::Function(callback)]);
        let slots = [Slot {
            index: 0,
            realm,
            items: &items,
            realm_global: false,
        }];
        let table = [other, pointer];
        let blob = encode_slots(agent_of(&isolate), &slots, &table, Some(&host)).expect("a blob");

        assert_eq!(
            decode_slot(agent_mut(&isolate), &realm, &blob, 0, &table, None).err(),
            Some(DecodeError::NoHostCallback(1)),
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
        // The body is the context table — slot index, item count, the carried
        // global serial, one item, and the slot's binding list (empty here, which
        // is still a count of zero) — then the object count, so the first record's
        // tag follows those 24 bytes (the global serial is `NO_REF` here, which
        // is what `encode` carries, but it is written either way).
        bad_tag[HEADER_LEN + 24] = 200;
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

    /// A builtin function the realm installs is a value a host's graph can hold:
    /// deno's own `00_primordials.js` copies them into an object it attaches to
    /// the realm it snapshots. It comes back as the name the **reading realm**
    /// answers to, which is the realm's own builtin rather than a copy of one —
    /// and the `uncurryThis` shape deno actually holds is a bind over one.
    #[test]
    fn a_builtin_member_round_trips_as_the_realms_own() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let abs = context
            .try_eval("Math.abs")
            .expect("a builtin member")
            .into_value();
        let blob = encode_value(&isolate, &realm, abs);
        assert!(
            blob.windows(10).any(|window| window == b"%Math.abs%"),
            "the builtin is in the blob by the name the realm derives for it"
        );
        let back = decode(agent_mut(&isolate), &realm, &blob).expect("decode");
        assert_eq!(
            back.as_function().map(|function| function.id()),
            abs.as_function().map(|function| function.id()),
            "the restored value is the realm's own Math.abs"
        );

        // `Function.prototype.call.bind(Math.max)` is deno's `uncurryThis`: a bind
        // whose *target* is a builtin, which is the record the name has to serve.
        let bound = context
            .try_eval("Function.prototype.call.bind(Math.max)")
            .expect("a bound builtin")
            .into_value();
        let back = round_trip(&isolate, &realm, bound);
        assert_eq!(
            call_restored(&isolate, &realm, back, "null, 1, 2").as_number(),
            Some(2.0),
            "a bind over a named builtin comes back callable"
        );

        // A static of a builtin *constructor* lives on the function rather than on
        // an object, and is the same kind of member — and one the installs do not
        // name of their own accord, since nothing dispatches through it.
        let is_finite = context
            .try_eval("Number.isFinite")
            .expect("a static")
            .into_value();
        let back = round_trip(&isolate, &realm, is_finite);
        assert_eq!(
            call_restored(&isolate, &realm, back, "1").as_boolean(),
            Some(true),
            "a constructor's static comes back as the realm's own"
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

    /// Run `body` with `value` installed as `restored`, answering `"returned"`
    /// when it completed and the thrown value's constructor name when it did not
    /// — which is how a strictness difference shows up.
    fn outcome_restored(
        isolate: &api::Isolate,
        realm: &Handle<Realm>,
        value: Value,
        body: &str,
    ) -> String {
        run_restored(
            isolate,
            realm,
            value,
            &format!(
                "(function () {{ try {{ {body} return 'returned'; }} \
                 catch (e) {{ return e.constructor.name; }} }})()"
            ),
        )
        .as_string()
        .map(|text| text.to_string_lossy())
        .unwrap_or_default()
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

    /// A function that closed over nothing but the realm's global writes no
    /// environment at all, so the restore instantiates it in the **reading**
    /// realm's global environment and a free name in the body resolves there —
    /// which is also what every blob written before the chain was carried gets.
    /// The realms differ here so that the answer can only be the reading one's
    /// global.
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
            call_restored(&isolate, reader.realm(), item_value(items[0]), "5").as_number(),
            Some(105.0),
            "the free name resolved through the reading realm's global"
        );
    }

    /// A closure's free names resolve through the environment chain it closed
    /// over, and that chain travels with it: a value captured in an outer
    /// function's scope is still there after a restore into a **different**
    /// realm, no global of which has the name. deno's bootstrap installs every
    /// one of its own helpers inside an IIFE, so every function it puts on its
    /// realm is a closure of exactly this shape.
    #[test]
    fn a_closures_captured_binding_comes_back_with_it() {
        let mut isolate = api::Isolate::new();
        let writer = api::Context::new(&mut isolate).expect("a realm");
        let function = writer
            .try_eval(
                "(function () { const captured = 41; return function () { return captured + 1; }; })()",
            )
            .expect("a closure")
            .into_value();
        let blob = encode(agent_of(&isolate), writer.realm(), function).expect("a blob");

        let reader = api::Context::new(&mut isolate).expect("a second realm");
        assert_eq!(
            reader
                .try_eval("typeof captured")
                .expect("a check")
                .into_value()
                .as_string()
                .map(|text| text.to_string_lossy()),
            Some("undefined".to_string()),
            "the name is no global of the reading realm, so only the chain can answer"
        );
        let items = decode_slot(agent_mut(&isolate), reader.realm(), &blob, 0, &[], None)
            .expect("a blob of this tree")
            .expect("slot 0");
        assert_eq!(
            call_restored(&isolate, reader.realm(), item_value(items[0]), "").as_number(),
            Some(42.0),
            "the captured binding travelled with the closure"
        );
    }

    /// The same promise where the enclosing body runs on the **environment**
    /// path: a `with` statement keeps the compiler from certifying that body, so
    /// the closure it returns closes over a function environment record rather
    /// than a certified capture context — the other kind of record the chain
    /// carries, whose own state is written beside the bindings.
    #[test]
    fn a_closures_captured_binding_survives_a_function_environment() {
        let mut isolate = api::Isolate::new();
        let writer = api::Context::new(&mut isolate).expect("a realm");
        // The receiver is a plain object rather than the realm's global, so the
        // enclosing record's `this` is a value the graph carries as itself.
        let function = writer
            .try_eval(
                "(function (scope) { const captured = 41; with (scope) {} return function () { return captured + 1; }; }).call({ tag: 1 }, {})",
            )
            .expect("a closure")
            .into_value();
        let blob = encode(agent_of(&isolate), writer.realm(), function).expect("a blob");

        let reader = api::Context::new(&mut isolate).expect("a second realm");
        let items = decode_slot(agent_mut(&isolate), reader.realm(), &blob, 0, &[], None)
            .expect("a blob of this tree")
            .expect("slot 0");
        assert_eq!(
            call_restored(&isolate, reader.realm(), item_value(items[0]), "").as_number(),
            Some(42.0),
            "the captured binding travelled with the closure"
        );
    }

    /// An environment whose binding holds the very function that closed over it
    /// comes back as **one** function. The chain is a cycle — the function's
    /// environment holds a binding whose value is that function — so the record
    /// has to be a shell before its own bindings are put back, and the function
    /// has to exist before those bindings are filled. A restore that ordered the
    /// two the other way would answer two functions where the writing realm had
    /// one.
    #[test]
    fn a_cycle_through_an_environment_comes_back_one_function() {
        let mut isolate = api::Isolate::new();
        let writer = api::Context::new(&mut isolate).expect("a realm");
        let function = writer
            .try_eval("(function () { const self = function () { return self; }; return self; })()")
            .expect("a closure")
            .into_value();
        let blob = encode(agent_of(&isolate), writer.realm(), function).expect("a blob");
        let items = decode_slot(agent_mut(&isolate), writer.realm(), &blob, 0, &[], None)
            .expect("a blob of this tree")
            .expect("slot 0");
        assert_eq!(
            run_restored(
                &isolate,
                writer.realm(),
                item_value(items[0]),
                "restored() === restored"
            )
            .as_boolean(),
            Some(true),
            "the binding holds the restored function itself, not a second one"
        );
    }

    /// A view whose data the agent holds rather than the object — a `DataView` —
    /// and a buffer this format cannot rebuild as what it was are refused by name.
    /// The first is a *wrong answer* the walk used to give rather than a missing
    /// feature: a DataView was written as the ordinary object it looks like, and
    /// the restore handed back an object with no view state at all. The other two
    /// are buffers whose *identity* is the value — the memory another agent holds,
    /// and the absence a detach leaves behind.
    #[test]
    fn a_view_or_buffer_this_format_cannot_rebuild_is_refused_by_name() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        for (source, expected) in [
            ("new DataView(new ArrayBuffer(8))", "a data view"),
            ("new SharedArrayBuffer(8)", "a shared array buffer"),
            (
                "new SharedArrayBuffer(8, { maxByteLength: 16 })",
                "a shared array buffer",
            ),
            (
                "(function () { const b = new ArrayBuffer(8); b.transfer(); return b; })()",
                "a detached array buffer",
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

    /// A typed array is carried as the kind it is, the buffer it views, its
    /// geometry and — through the buffer — its elements. A view whose elements are
    /// not carried is a view of zeros, and one whose kind is not carried reads the
    /// bytes as the wrong numbers, which is why the kind travels by name.
    #[test]
    fn a_typed_array_round_trips_with_its_bytes_and_geometry() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let view = context
            .try_eval("new Uint8Array([1, 2, 3])")
            .expect("a view")
            .into_value();

        let back = round_trip(&isolate, &realm, view);
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                back,
                "[restored[0], restored[1], restored[2], restored.length, restored.byteLength, \
                 restored.byteOffset, restored.buffer.byteLength, restored.constructor.name, \
                 Array.isArray(restored)].join(',')"
            )
            .as_string()
            .map(|text| text.to_string_lossy()),
            Some("1,2,3,3,3,0,3,Uint8Array,false".to_string()),
            "the elements, the geometry, the kind and the buffer"
        );
        // A view writes into its buffer's block, so a write through the restored
        // view has to reach the restored buffer: a view over a copy of the block
        // would answer the old element here.
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                back,
                "restored[1] = 9; new Uint8Array(restored.buffer)[1]"
            )
            .as_number(),
            Some(9.0),
            "the view and its buffer share one block"
        );
    }

    /// Two views over one buffer come back as two views over **one** buffer. The
    /// buffer is a value in the graph like any other, so the serial map makes it
    /// one record and the restore one object — which is the shape deno's
    /// `callSiteRetBuf`/`callSiteRetBufU8` pair has, two views of one block.
    #[test]
    fn two_views_over_one_buffer_come_back_one_buffer() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let pair = context
            .try_eval(
                "(function () { const bytes = new Uint8Array(8); const words = new Uint32Array(bytes.buffer); \
                 words[0] = 0x04030201; return { bytes, words }; })()",
            )
            .expect("two views")
            .into_value();

        let back = round_trip(&isolate, &realm, pair);
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                back,
                "[restored.bytes.buffer === restored.words.buffer, restored.bytes[0], \
                 restored.words[0], restored.bytes.buffer.byteLength].join(',')"
            )
            .as_string()
            .map(|text| text.to_string_lossy()),
            Some("true,1,67305985,8".to_string()),
            "one buffer, two views of it, and the same bytes read two ways"
        );
    }

    /// A view that neither starts at the beginning of its buffer nor runs to its
    /// end keeps both: they are the record's own geometry, and — because the
    /// elements belong to the buffer — the bytes outside the window come back too.
    #[test]
    fn a_views_offset_and_length_come_back_with_it() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let view = context
            .try_eval(
                "(function () { const buffer = new ArrayBuffer(8); const all = new Uint8Array(buffer); \
                 all[0] = 9; all[7] = 7; return new Uint16Array(buffer, 2, 2); })()",
            )
            .expect("a window")
            .into_value();

        let back = round_trip(&isolate, &realm, view);
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                back,
                "[restored.byteOffset, restored.length, restored.byteLength, \
                 new Uint8Array(restored.buffer)[0], new Uint8Array(restored.buffer)[7]].join(',')"
            )
            .as_string()
            .map(|text| text.to_string_lossy()),
            Some("2,2,4,9,7".to_string()),
            "the window, and the bytes on both sides of it"
        );
    }

    /// A resizable buffer comes back resizable with its maximum, and a view that
    /// tracks it still tracks it: an auto-length view's length is whatever the
    /// buffer's is, so a count cannot describe one and the flag travels instead.
    #[test]
    fn a_resizable_buffer_comes_back_resizable() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let view = context
            .try_eval(
                "(function () { const buffer = new ArrayBuffer(4, { maxByteLength: 8 }); \
                 const view = new Uint8Array(buffer); buffer.resize(6); view[5] = 5; return view; })()",
            )
            .expect("an auto-length view")
            .into_value();

        let back = round_trip(&isolate, &realm, view);
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                back,
                "[restored.buffer.resizable, restored.buffer.maxByteLength, \
                 restored.buffer.byteLength, restored.length, restored[5]].join(',')"
            )
            .as_string()
            .map(|text| text.to_string_lossy()),
            Some("true,8,6,6,5".to_string()),
            "the flags, the maximum, the geometry and the element"
        );
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                back,
                "restored.buffer.resize(8); restored.length"
            )
            .as_number(),
            Some(8.0),
            "the restored view still tracks the buffer it views"
        );
    }

    /// A buffer over the **host's** memory — the shape `deno_core` gives its three
    /// `ContextState`-backed views — is carried as its bytes, which is what V8's
    /// serializer does with a non-shared ArrayBuffer. What is not carried is the
    /// identity of the host's allocation: the restored buffer is engine-owned, and
    /// a host that wants the sharing re-establishes it.
    #[test]
    fn a_buffer_over_host_memory_is_carried_as_its_bytes() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        // The bytes are this test's own allocation, kept alive across the round
        // trip, and the block borrows them the way a v8 backing store does.
        let mut host = vec![1u8, 2, 3, 4];
        let pointer = host.as_mut_ptr();
        let view = {
            let agent = agent_mut(&isolate);
            // SAFETY: the block borrows the test's own `host` bytes, which outlive
            // every clone of it, and it is the only thing that reads them.
            let block = unsafe { crux::typed_array::SharedBuffer::borrowed(pointer, 4, None) };
            let buffer = crate::builtins::array_buffer::array_buffer_from_block(agent, block, 4)
                .expect("a buffer over the host's bytes");
            let prototype = realm
                .intrinsics
                .get("%Uint8Array.prototype%")
                .and_then(|value| crate::context::as_object(&value))
                .expect("%Uint8Array.prototype%");
            crate::builtins::typed_array::typed_array_buffer_path(
                agent,
                prototype,
                ElementType::Uint8,
                &buffer,
                &[],
            )
            .expect("a view of the host's bytes")
        };
        assert_eq!(
            host,
            vec![1, 2, 3, 4],
            "reading for the blob leaves the host's bytes alone"
        );

        let back = round_trip(&isolate, &realm, view);
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                back,
                "[restored[0], restored[3], restored.length, restored.buffer.byteLength].join(',')"
            )
            .as_string()
            .map(|text| text.to_string_lossy()),
            Some("1,4,4,4".to_string()),
            "the host's bytes came back as the restored buffer's"
        );
    }

    /// A view whose `[[Prototype]]` is null — `Object.setPrototypeOf(view, null)`
    /// — comes back with none: the record's link is what the view is built with,
    /// and a null link is the one the construction cannot be handed.
    #[test]
    fn a_null_prototype_view_comes_back_with_no_prototype() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let view = context
            .try_eval("Object.setPrototypeOf(new Uint8Array([1]), null)")
            .expect("a linked-free view")
            .into_value();

        let back = round_trip(&isolate, &realm, view);
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                back,
                "[Object.getPrototypeOf(restored) === null, restored[0]].join(',')"
            )
            .as_string()
            .map(|text| text.to_string_lossy()),
            Some("true,1".to_string()),
            "no prototype, and the element"
        );
    }

    /// Every one of the twelve kinds round-trips with a value in it. The kind
    /// travels by name and the restore derives the intrinsic prototype from that
    /// name, so this pins both name tables — and the derivation between them — for
    /// all twelve rather than for the kinds the other tests happen to use.
    #[test]
    fn every_typed_array_kind_round_trips() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let views = context
            .try_eval(
                "(function () { const views = [Int8Array, Uint8Array, Uint8ClampedArray, Int16Array, \
                 Uint16Array, Int32Array, Uint32Array, Float16Array, Float32Array, Float64Array, \
                 BigInt64Array, BigUint64Array].map((Kind) => new Kind(2)); \
                 for (const view of views) view[0] = view.constructor.name.startsWith('Big') ? 7n : 7; \
                 return views; })()",
            )
            .expect("one view of each kind")
            .into_value();

        let back = round_trip(&isolate, &realm, views);
        let expected = [
            "Int8Array",
            "Uint8Array",
            "Uint8ClampedArray",
            "Int16Array",
            "Uint16Array",
            "Int32Array",
            "Uint32Array",
            "Float16Array",
            "Float32Array",
            "Float64Array",
            "BigInt64Array",
            "BigUint64Array",
        ]
        .map(|name| format!("{name}:2:7"))
        .join(",");
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                back,
                "restored.map((view) => view.constructor.name + ':' + view.length + ':' + \
                 String(view[0])).join(',')"
            )
            .as_string()
            .map(|text| text.to_string_lossy()),
            Some(expected),
            "every kind, its length, and an element of it"
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
        let items = slot_items(&[function, function]);
        let slots = [Slot {
            index: 0,
            realm,
            items: &items,
            realm_global: false,
        }];
        let blob = encode_slots(agent_of(&isolate), &slots, &[], None).expect("a blob");
        let back = decode_slot(agent_mut(&isolate), &realm, &blob, 0, &[], None)
            .expect("a blob of this tree")
            .expect("slot 0");
        assert_eq!(
            item_value(back[0])
                .as_function()
                .expect("the first item is a function")
                .id(),
            item_value(back[1])
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

    /// A method's `[[SourceText]]` is its MethodDefinition — `m() {}`, with no
    /// `function` keyword — so it is carried as the method grammar and the restore
    /// evaluates it as a property of an object literal rather than parsing it.
    #[test]
    fn a_method_round_trips_and_is_callable() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let method = context
            .try_eval("({ m() { return 41; } }).m")
            .expect("a method")
            .into_value();

        let back = round_trip(&isolate, &realm, method);
        let back = back.as_function().expect("a function");
        assert_eq!(
            call_restored(&isolate, &realm, Value::Function(back), "").as_number(),
            Some(41.0),
            "the restored method runs its body"
        );
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                Value::Function(back),
                "restored.toString()"
            )
            .as_string()
            .map(|text| text.to_string_lossy()),
            Some("m() { return 41; }".to_string()),
            "and answers its own source, which the restore's own evaluation captured"
        );
        // A method has no own `prototype` and must not gain one: the deferred
        // MakeConstructor a plain function gets would materialize here, which is
        // what reading the source as a *function* would produce.
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                Value::Function(back),
                "Object.getOwnPropertyDescriptor(restored, 'prototype') === undefined ? 1 : 0"
            )
            .as_number(),
            Some(1.0),
            "a restored method is a method, not a plain function"
        );
    }

    /// A method's `[[HomeObject]]` is what `super` resolves through, and it is the
    /// one value a method record carries beside its source: without it the restore
    /// evaluates the method inside a temporary object literal, and `super` looks
    /// in that instead of in the object the method was written in.
    #[test]
    fn a_method_reaches_super_through_its_carried_home_object() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let method = context
            .try_eval(
                "var proto = { greet() { return 41; } };\n\
                 var holder = { __proto__: proto, m() { return super.greet(); } };\n\
                 holder.m",
            )
            .expect("a method")
            .into_value();

        let back = round_trip(&isolate, &realm, method);
        assert_eq!(
            call_restored(&isolate, &realm, back, "").as_number(),
            Some(41.0),
            "super reached the home object's prototype, which the record carried"
        );
    }

    /// The strict byte is not decoration. A class method is strict whatever its
    /// own body says and an object method inherits the strictness of the code it
    /// was written in, and the evaluation the restore runs is the only place that
    /// can be told which — so the wrapper is strict code exactly when the record
    /// says strict. The class half is also the shape deno's walk reaches next: a
    /// class **with** a method, whose prototype's method is carried through the
    /// class record's own graph.
    #[test]
    fn a_methods_strictness_survives_the_restore() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let class = context
            .try_eval("(class { strict() { classLeak = 1; } ok() { return 41; } })")
            .expect("a class")
            .into_value();
        let method = context
            .try_eval("({ sloppy() { objectLeak = 1; return 41; } }).sloppy")
            .expect("an object method")
            .into_value();

        let back = round_trip(&isolate, &realm, class);
        assert_eq!(
            run_restored(&isolate, &realm, back, "new restored().ok()").as_number(),
            Some(41.0),
            "a class's own method came back through the class's prototype"
        );
        // `strict` throws before it can answer anything, so the strictness is read
        // from what the *call* threw rather than from what it returned.
        assert_eq!(
            outcome_restored(&isolate, &realm, back, "new restored().strict();"),
            "ReferenceError",
            "a class method is strict, and the restore made its code strict"
        );

        let back = round_trip(&isolate, &realm, method);
        assert_eq!(
            outcome_restored(&isolate, &realm, back, "restored();"),
            "returned",
            "an object method written in sloppy code comes back sloppy"
        );
    }

    /// The same wrapper decides an arrow's strictness, which the tenth part had
    /// been losing: an arrow written in strict code was restored as sloppy.
    #[test]
    fn an_arrows_strictness_survives_the_restore() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let strict = context
            .try_eval("\"use strict\"; (() => { arrowLeak = 1; })")
            .expect("a strict arrow")
            .into_value();
        let sloppy = context
            .try_eval("(() => { otherLeak = 1; })")
            .expect("a sloppy arrow")
            .into_value();

        let back = round_trip(&isolate, &realm, strict);
        assert_eq!(
            outcome_restored(&isolate, &realm, back, "restored();"),
            "ReferenceError",
            "an arrow from strict code is still strict after the restore"
        );
        let back = round_trip(&isolate, &realm, sloppy);
        assert_eq!(
            outcome_restored(&isolate, &realm, back, "restored();"),
            "returned",
            "and one from sloppy code is still sloppy"
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

    /// A class's computed element keys are carried, so the restore does not
    /// re-evaluate them: a key expression may read a name that exists only in the
    /// module which defined the class, which the reading realm's global does not
    /// have. The class source's `[[SourceText]]` is the other half of this test —
    /// the frame the evaluation runs under has to carry the parsed text for
    /// `toString`, and a second snapshot, to keep working.
    #[test]
    fn a_class_carries_its_computed_field_key() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let value = context
            .try_eval(
                "(() => { const SymbolIterator = Symbol.iterator; \
                 class C { [SymbolIterator] = 7; } return C; })()",
            )
            .expect("a class with a computed field key")
            .into_value();

        let back = round_trip(&isolate, &realm, value);
        assert_eq!(
            run_restored(&isolate, &realm, back, "new restored()[Symbol.iterator]").as_number(),
            Some(7.0),
            "the field was installed under the key the class was defined with"
        );
        assert_eq!(
            run_restored(&isolate, &realm, back, "restored.toString()")
                .as_string()
                .map(|text| text.to_string_lossy()),
            Some("class C { [SymbolIterator] = 7; }".to_string()),
            "the restored class still answers its class source"
        );
    }

    /// A derived class's heritage is carried as the value the class was defined
    /// with, so the restore does not re-resolve the `extends` expression — which
    /// names a binding of the module that defined the class, not a global. The
    /// observation is the **implicit constructor** calling `super()`: that is
    /// `[[ConstructorKind]]`, which only the heritage decides. (The statics would
    /// survive without it, because a class's own prototype link is carried by its
    /// function record — so they do not prove the heritage.)
    #[test]
    fn a_class_carries_its_heritage() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let value = context
            .try_eval(
                "(() => { const Base = class { constructor() { this.tag = 'base'; } }; \
                 class Derived extends Base {} return Derived; })()",
            )
            .expect("a derived class")
            .into_value();

        let back = round_trip(&isolate, &realm, value);
        assert_eq!(
            run_restored(&isolate, &realm, back, "new restored().tag")
                .as_string()
                .map(|text| text.to_string_lossy()),
            Some("base".to_string()),
            "the restored class is derived, so its implicit constructor calls super()"
        );
    }

    /// `extends null` is the one heritage whose resolved form is not the value it
    /// was written as — it resolves to `%Function.prototype%` as the super
    /// constructor with no proto parent — so the record carries the value `null`
    /// and the class comes back still **derived**, with a null prototype.
    #[test]
    fn a_class_carrying_a_null_heritage_stays_derived() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let value = context
            .try_eval("(class extends null {})")
            .expect("a class extending null")
            .into_value();

        let back = round_trip(&isolate, &realm, value);
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                back,
                "Object.getPrototypeOf(restored.prototype) === null ? 1 : 0",
            )
            .as_number(),
            Some(1.0),
            "the prototype's parent is null"
        );
        assert_eq!(
            outcome_restored(&isolate, &realm, back, "new restored();"),
            "TypeError",
            "a class extending null is still derived, so its constructor reaches super()"
        );
    }

    /// A computed **method** key needs both halves: the class carries the key (so
    /// its own evaluation does not read the module name), and the method is carried
    /// as a member of the class (so the method record's source is never
    /// re-evaluated either).
    #[test]
    fn a_class_carries_a_computed_method_key() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let value = context
            .try_eval(
                "(() => { const SymbolIterator = Symbol.iterator; \
                 class C { #n = 1; [SymbolIterator]() { return this.#n + 1; } } \
                 return C; })()",
            )
            .expect("a class with a computed method key")
            .into_value();

        let back = round_trip(&isolate, &realm, value);
        assert_eq!(
            run_restored(&isolate, &realm, back, "new restored()[Symbol.iterator]()").as_number(),
            Some(2.0),
            "the method came back under the carried key and reads the class's private field"
        );
    }

    /// An accessor's **absent** half is the format's sentinel, not a record: a
    /// getter-only property is one with no `[[Set]]`, and the restore has to leave
    /// it absent rather than ask for a record that was never written. No test
    /// carried an accessor property before this one, which is why the reader kept
    /// the bug until deno's blob reached a getter-only accessor.
    #[test]
    fn an_accessors_absent_half_stays_absent() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let value = context
            .try_eval(
                "(() => { const object = {}; \
                 Object.defineProperty(object, 'onlyGet', { get: Math.abs, configurable: true }); \
                 Object.defineProperty(object, 'onlySet', { set: Math.abs, configurable: true }); \
                 return object; })()",
            )
            .expect("an object with one-sided accessors")
            .into_value();

        let back = round_trip(&isolate, &realm, value);
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                back,
                "Object.getOwnPropertyDescriptor(restored, 'onlyGet').set === undefined ? 1 : 0",
            )
            .as_number(),
            Some(1.0),
            "the getter-only property came back with no setter"
        );
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                back,
                "Object.getOwnPropertyDescriptor(restored, 'onlySet').get === undefined ? 1 : 0",
            )
            .as_number(),
            Some(1.0),
            "and the setter-only property with no getter"
        );
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                back,
                "Object.getOwnPropertyDescriptor(restored, 'onlyGet').get === Math.abs ? 1 : 0",
            )
            .as_number(),
            Some(1.0),
            "the half that is present is the reading realm's own"
        );
    }

    /// The measured deno shape: an **object-literal** method with a computed key
    /// whose expression reads a binding of the module that defined it. Neither
    /// carrying the class (there is none — the home object is a plain object) nor
    /// carrying the key reaches it, because the record does not need a key at all:
    /// the method is *instantiated* from its parsed definition instead of
    /// evaluated, so the key expression is never evaluated.
    #[test]
    fn a_method_with_a_computed_key_round_trips_without_evaluating_it() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let value = context
            .try_eval(
                "(() => { const SymbolIterator = Symbol.iterator; \
                 return { [SymbolIterator]() { return 41; } }; })()",
            )
            .expect("an object with a computed-key method")
            .into_value();

        let back = round_trip(&isolate, &realm, value);
        assert_eq!(
            run_restored(&isolate, &realm, back, "restored[Symbol.iterator]()").as_number(),
            Some(41.0),
            "the method came back under the key the object holds it under"
        );
        // Its own `[[SourceText]]` is re-captured from the parsed text, so a host
        // that reads the method back sees the definition it was written as.
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                back,
                "restored[Symbol.iterator].toString()",
            )
            .as_string()
            .map(|text| text.to_string_lossy()),
            Some("[SymbolIterator]() { return 41; }".to_string()),
            "the restored method answers its own source"
        );
    }

    /// The measured deno shape: a public method whose body calls a **private**
    /// method. It cannot be rebuilt from its own text — the object literal a
    /// MethodDefinition is only valid inside has no class body, so `#bump` has no
    /// scope in it — and it must not be rebuilt by evaluating the class a second
    /// time either: a private name belongs to one class declaration, so a second
    /// evaluation's brand is not the brand the class the graph holds gives its
    /// instances. It is carried as a **member of its class**, and the restore
    /// reads it out of that class's own evaluation.
    #[test]
    fn a_method_that_reads_a_private_name_round_trips_as_its_classs_member() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let value = context
            .try_eval(
                "(() => { class Counter { #count = 3; #bump() { this.#count = this.#count + 1; \
                 return this.#count; } bump() { return this.#bump(); } } \
                 return { ctor: Counter, method: Counter.prototype.bump }; })()",
            )
            .expect("a class and one of its methods")
            .into_value();
        let blob = encode_value(&isolate, &realm, value);
        assert!(
            blob.contains(&REC_CLASS_METHOD),
            "the method is carried as a member of its class, not by its own source"
        );

        let back = decode(agent_mut(&isolate), &realm, &blob).expect("decode");
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                back,
                "(function () { const c = new restored.ctor(); restored.method.call(c); \
                 return restored.method.call(c); })()",
            )
            .as_number(),
            Some(5.0),
            "the restored method reads and mutates the restored class's private field"
        );
    }

    /// A method that reads a private name **and** reaches `super` is re-homed to
    /// the record's [[HomeObject]] — the carried prototype — rather than keeping
    /// the object the class's own evaluation made, which the class record's tail
    /// replaces. The mutation on the carried prototype is what makes the two
    /// distinguishable: only the carried object has it.
    #[test]
    fn a_private_name_method_reaches_super_through_the_carried_prototype() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let value = context
            .try_eval(
                "(() => { class Box { #n = 1; run() { return super.added() + this.#n; } } \
                 Object.setPrototypeOf(Box.prototype, { added() { return 'x'; } }); \
                 return { ctor: Box, run: Box.prototype.run }; })()",
            )
            .expect("a class whose method reads a private name and reaches super")
            .into_value();

        let back = round_trip(&isolate, &realm, value);
        assert_eq!(
            run_restored(
                &isolate,
                &realm,
                back,
                "(function () { const s = new restored.ctor(); \
                 return restored.run.call(s); })()",
            )
            .as_string()
            .map(|text| text.to_string_lossy()),
            Some("x1".to_string()),
            "`super` resolved through the prototype the graph holds"
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

    /// An accessor is carried by the same method record a method uses: the engine
    /// gives a getter or setter the `[[SourceText]]` the spec gives it (15.4.3 —
    /// the accessor instantiation takes the span the parser records for the whole
    /// definition and captures it), so the walk has the source the record needs.
    ///
    /// This test was the **refusal** for that kind until then — the last function
    /// kind with no source text — and its subject is positive coverage now.
    #[test]
    fn an_accessor_round_trips_with_its_source() {
        let mut isolate = api::Isolate::new();
        let context = api::Context::new(&mut isolate).expect("a realm");
        let realm = *context.realm();
        let object = context
            .try_eval(
                "({ backing: 0, get x() { return this.backing; }, \
                 set x(v) { this.backing = v; } })",
            )
            .expect("an object with an accessor")
            .into_value();
        let back = round_trip(&isolate, &realm, object);

        // The restored halves are the writer's: the setter writes and the getter
        // reads the same restored property back.
        assert_eq!(
            run_restored(&isolate, &realm, back, "restored.x = 41; restored.x").as_number(),
            Some(41.0)
        );
        // And each half answers its own definition as its source.
        let getter = run_restored(
            &isolate,
            &realm,
            back,
            "Object.getOwnPropertyDescriptor(restored, 'x').get.toString()",
        );
        assert_eq!(
            getter
                .as_string()
                .map(|text| text.to_string_lossy())
                .as_deref(),
            Some("get x() { return this.backing; }")
        );
        let setter = run_restored(
            &isolate,
            &realm,
            back,
            "Object.getOwnPropertyDescriptor(restored, 'x').set.toString()",
        );
        assert_eq!(
            setter
                .as_string()
                .map(|text| text.to_string_lossy())
                .as_deref(),
            Some("set x(v) { this.backing = v; }")
        );

        // A class's accessor takes the same path with the class arm's span, so a
        // restored class reads through its own getter.
        let class = context
            .try_eval("(class { get c() { return 3; } })")
            .expect("a class with an accessor")
            .into_value();
        let back = round_trip(&isolate, &realm, class);
        assert_eq!(
            run_restored(&isolate, &realm, back, "new restored().c").as_number(),
            Some(3.0)
        );
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
                item_value(back[0])
                    .as_function()
                    .map(|function| function.id()),
                own.as_function().map(|function| function.id()),
                "{name} came back the reading realm's own function"
            );
            assert_ne!(
                item_value(back[0])
                    .as_function()
                    .map(|function| function.id()),
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
