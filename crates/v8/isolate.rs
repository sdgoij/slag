//! The isolate: the engine's isolate, plus what the bridge records on it.

use std::any::{Any, TypeId};
use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::rc::Rc;

use crux::handle::Handle;
use crux::string::JsString;
use runtime::api;

use crate::ExternalReference;
use crate::cppgc::Heap;
use crate::data::Private;
use crate::data::{
    Array, Context, Data, FixedArray, Function, Object, Promise, PromiseResolver, Value,
};
use crate::function::{FunctionCallbackArguments, ReturnValue};
use crate::handle::{Global, Local, Payload};
use crate::position::Position;
use crate::promise::{PromiseRejectEvent, PromiseRejectMessage};
use crate::scope::PinScope;
use crate::snapshot::{FunctionCodeHandling, SnapshotCreator, SnapshotRestore, StartupData};
use crate::support::{MapFnFrom, UniqueRef, UnitType};
use crate::wasm::WasmStreaming;

/// Create parameters (`v8::CreateParams`).
///
/// Two of the crate we stand in for's settings are carried rather than acted
/// on:
///
/// - `snapshot_blob` is consumed when it is a blob of this engine's own format
///   (see [`StartupData`](crate::StartupData)), and booted from source when it
///   is not — which is the answer `is_valid` gives the host before it hands one
///   over.
/// - `external_references` is the table a blob's indices resolve against, and a
///   restored isolate keeps it for that (see
///   [`ExternalReference`](crate::ExternalReference)): an address belongs to the
///   process, so a snapshot names an index and the host rebuilds the table for
///   every load.
///
/// Everything else there configures V8's heap, its allocator or its sandbox,
/// none of which Slag exposed as an embedding setting. The one setting besides
/// the snapshot that does take effect is `cpp_heap`, which becomes the
/// isolate's own heap for host objects.
///
/// Not `Clone`, as there: a heap is one of the settings, and a heap is not
/// copyable.
#[derive(Debug, Default)]
pub struct CreateParams {
    snapshot_blob: Option<crate::StartupData>,
    external_references: Option<std::borrow::Cow<'static, [crate::ExternalReference]>>,
    /// The heap the isolate takes ownership of for host objects
    /// (`CreateParams::cpp_heap`).
    cpp_heap: Option<Heap>,
    /// The heap sizes a host asked for (`CreateParams::heap_limits`): the
    /// maximum is enforced against the heap's live-box footprint and fires the
    /// near-heap-limit callback, and the initial value is what that callback is
    /// handed as V8's `initial_heap_limit`. A maximum of 0 is V8's own default
    /// — no limit — and leaves the engine's heap unbounded, as it was.
    heap_limits: Option<(usize, usize)>,
    /// The generation and code-range sizes a host asked for
    /// (`CreateParams::set_max_old_generation_size_in_bytes` and the two beside
    /// it). **Recorded and reported, and nothing else**: a generation is a part
    /// of a *V8* heap, and this engine's arena is not one. A host that reads
    /// them back gets what it set, or `0` for "the engine's own policy" — not
    /// V8's default, which would be a number invented here. What *does*
    /// constrain the collector is [`CreateParams::heap_limits`].
    max_old_generation_size: usize,
    max_young_generation_size: usize,
    code_range_size: usize,
}

impl CreateParams {
    /// Start from a snapshot this host carries
    /// (v8::CreateParams::snapshot_blob).
    pub fn snapshot_blob(mut self, data: crate::StartupData) -> Self {
        self.snapshot_blob = Some(data);
        self
    }

    /// The external references a snapshot indexes into
    /// (v8::CreateParams::external_references).
    pub fn external_references(
        mut self,
        external_references: std::borrow::Cow<'static, [crate::ExternalReference]>,
    ) -> Self {
        self.external_references = Some(external_references);
        self
    }

    /// The heap for host objects, which the isolate takes ownership of
    /// (v8::CreateParams::cpp_heap).
    ///
    /// An isolate without one gets an empty heap of its own, because
    /// [`Isolate::get_cpp_heap`] always answers `Some` here — see its own
    /// documentation.
    pub fn cpp_heap(mut self, heap: UniqueRef<Heap>) -> Self {
        self.cpp_heap = Some(heap.into_inner());
        self
    }

    /// The heap's initial and maximum size in bytes
    /// (v8::CreateParams::heap_limits): a maximum above zero is enforced, and
    /// the callback a heap that reaches it runs answers the limit to continue
    /// with.
    pub fn heap_limits(mut self, initial: usize, maximum: usize) -> Self {
        self.heap_limits = Some((initial, maximum));
        self
    }

    /// The limits a host asked for, if any (this bridge's own accessor, for a
    /// host that keeps one `CreateParams` around).
    pub fn get_heap_limits(&self) -> Option<(usize, usize)> {
        self.heap_limits
    }

    /// The heap's maximum old-generation size
    /// (`CreateParams::set_max_old_generation_size_in_bytes`). Recorded and
    /// reported: see the field's own note.
    pub fn set_max_old_generation_size_in_bytes(mut self, size: usize) -> Self {
        self.max_old_generation_size = size;
        self
    }

    /// The heap's maximum young-generation size
    /// (`CreateParams::set_max_young_generation_size_in_bytes`). Recorded and
    /// reported, as the old-generation one is.
    pub fn set_max_young_generation_size_in_bytes(mut self, size: usize) -> Self {
        self.max_young_generation_size = size;
        self
    }

    /// The heap's code range size
    /// (`CreateParams::set_code_range_size_in_bytes`). Recorded and reported,
    /// as the generation sizes are.
    pub fn set_code_range_size_in_bytes(mut self, size: usize) -> Self {
        self.code_range_size = size;
        self
    }

    /// The maximum old-generation size a host set, or `0` for one that set none
    /// (`CreateParams::max_old_generation_size_in_bytes`).
    pub fn max_old_generation_size_in_bytes(&self) -> usize {
        self.max_old_generation_size
    }

    /// The maximum young-generation size a host set, or `0` for one that set
    /// none (`CreateParams::max_young_generation_size_in_bytes`).
    pub fn max_young_generation_size_in_bytes(&self) -> usize {
        self.max_young_generation_size
    }

    /// The code range size a host set, or `0` for one that set none
    /// (`CreateParams::code_range_size_in_bytes`).
    pub fn code_range_size_in_bytes(&self) -> usize {
        self.code_range_size
    }

    /// Derive heap limits from the memory a system has
    /// (`CreateParams::heap_limits_from_system_memory`): half of `total` for the
    /// old generation and a sixteenth for the young one, leaving the code range
    /// unset. The numbers are this bridge's own derivation, and — as with the
    /// setters above — the engine's collector reads none of them; they exist so
    /// that a host asking what a V8 heap would have been told gets a plausible
    /// answer rather than zeros.
    pub fn heap_limits_from_system_memory(mut self, total: u64, _limit: u64) -> Self {
        self.max_old_generation_size = (total / 2) as usize;
        self.max_young_generation_size = (total / 16) as usize;
        self
    }

    /// The snapshot this host asked for, if any (this bridge's own accessor,
    /// for a host that keeps one `CreateParams` around).
    pub fn snapshot(&self) -> Option<&crate::StartupData> {
        self.snapshot_blob.as_ref()
    }
}

/// What the bridge built a host function from, and what it has to answer for
/// when a snapshot carries one.
///
/// Both halves are the host's: the callback is an address in this process, so a
/// blob names the entry of the host's table it came from instead, and the data
/// is a *value* the callback reads, so a blob carries it as a value. The data is
/// held pinned because the snapshot walk reads it from here rather than through
/// the closure the function calls through.
#[derive(Clone)]
pub(crate) struct BuiltCallback {
    /// The template's `FunctionCallback`, as the address its table entry holds.
    pub(crate) callback: usize,
    /// The value the template attached (`v8::FunctionBuilder::data`).
    pub(crate) data: Option<Global<Value>>,
}

/// What an [`Isolate`] handle points at.
///
/// The engine isolate is the first field, which is what lets an engine pointer
/// and the handle wrapping it name the same address; the assertion below keeps
/// it that way.
#[repr(C)]
pub struct IsolateInner {
    pub(crate) engine: api::Isolate,
    /// The realm most recently entered through a `ContextScope`. The engine
    /// tracks its own current realm; this is the bridge's copy of it, so a
    /// scope can hand object operations the context they need.
    context: RefCell<Option<api::Context>>,
    /// The host's value kept across a suspension
    /// (`SetContinuationPreservedEmbedderData`). Persistent, so the collector
    /// keeps it: a plain value in host memory would be swept.
    continuation_data: Option<Global<Value>>,
    /// Host data keyed by type. `SetData`/`GetData` is the slot-*number* version
    /// of the same idea, and both exist in the crate we stand in for.
    ///
    /// An `UnsafeCell` rather than a `RefCell` because `get_slot` hands out a
    /// reference into the map, which no borrow guard could outlive; the
    /// contract on that method is the crate we stand in for's.
    slots: UnsafeCell<HashMap<TypeId, Box<dyn Any>>>,
    /// The slots a host keeps on a context
    /// (`v8::Context::SetAlignedPointerInEmbedderData`). The engine's contexts
    /// have no such slots, so the bridge owns them, keyed by the context's
    /// global object — the identity the bridge already tells two contexts
    /// apart by — and the index the host chose.
    context_slots: RefCell<HashMap<(u64, i32), usize>>,
    /// The private names the bridge minted here (`v8::Private::ForApi`), by
    /// description. A private name is a symbol the bridge mints and keeps: the
    /// engine has no private-name kind, and a symbol-keyed property is invisible
    /// to the walks a private name has to be invisible to. Keeping the symbol is
    /// what makes one description one name, and what keeps it off the
    /// collector's list — the crate promises a private name is never collected.
    ///
    /// The key is code units, because that is what tells two names apart; the
    /// engine's `JsString` has no hash to be a map key.
    private_names: RefCell<HashMap<Vec<u16>, Global<Value>>>,
    /// The private names the bridge minted *fresh* (`v8::Private::new`), in mint
    /// order. A fresh name is deliberately not in the registry above — two calls
    /// with one description are two names — but it is still a private name, so it
    /// is kept here: keeping it is what `owns_private` (and through it
    /// `is_private` and the `Data => Private` cast) recognizes, and what keeps it
    /// off the collector's list as the crate promises.
    fresh_privates: RefCell<Vec<Global<Value>>>,
    /// The rejecting function of every resolver pair the bridge made
    /// (`v8::Promise::Resolver::Reject`), by the resolve function's identity.
    ///
    /// The engine's record for a pair names each function by identity, and the
    /// value of the rejecting one is nowhere else once the capability has been
    /// built — a host only ever holds the resolve function — so the isolate keeps
    /// it for a handle to name back.
    resolver_rejects: RefCell<HashMap<u64, Global<Value>>>,
    /// The heap for host objects, which this isolate owns for its whole life.
    /// Not behind a `RefCell` because `get_cpp_heap` hands out a reference to it;
    /// the heap's own allocation list is the interior-mutable part.
    cpp_heap: Heap,
    /// The function templates the host created on this isolate. A template has
    /// no engine object of its own, so the isolate owns it and a handle names
    /// its address; letting go of one is deferred to the isolate's `Drop`,
    /// which is later than the collector would reap an unused template, and
    /// never earlier.
    templates: RefCell<Vec<Rc<api::FunctionTemplate>>>,
    /// The extras binding object a context asked for, held the way a template is:
    /// the isolate owns it, because a handle to it outlives the scope it was made
    /// in.
    extras_bindings: RefCell<std::collections::HashMap<u64, Global<Object>>>,
    /// The security token each context was given
    /// (`v8::Context::SetSecurityToken`), by the context identity the rest of
    /// this bridge's per-context state is keyed by.
    ///
    /// Kept rather than consulted: V8's token decides whether one context's
    /// properties may be read and written from another, and this engine has no
    /// cross-realm access check for a token to decide. See
    /// [`LocalHandle::set_security_token`](crate::Context) for what a host can
    /// and cannot observe.
    security_tokens: RefCell<std::collections::HashMap<u64, Global<Value>>>,
    /// The token a context answers with before it is given one
    /// (`v8::Context::UseDefaultSecurityToken`, and the default every context
    /// starts on). One per isolate, minted on the first ask, so two contexts on
    /// one isolate compare equal the way V8's default makes them.
    default_security_token: RefCell<Option<Global<Value>>>,
    /// Where the errors the bridge threw came from, by object identity, for the
    /// `v8::Message` a host later makes from one of them; see
    /// [`position`](crate::position) for why only a failed compile records one.
    positions: RefCell<HashMap<u64, Position>>,
    /// The host callback each materialized function was built from, by the
    /// function's identity — the one thing a snapshot cannot rebuild and the
    /// host therefore has to answer for: the record names a table entry, and the
    /// load's table names the call again. Populated from
    /// [`callback_templates`](Self::callback_templates) when a function is
    /// materialized, and it holds the template's data value as a `Global`,
    /// because the engine's walk reads it and the collector has to keep it until
    /// the blob is written.
    callbacks: RefCell<HashMap<u64, BuiltCallback>>,
    /// The same, by the address a template's handle names, for the moment
    /// between a template being built and the function it materializes: a
    /// template has no engine object of its own, so the callback it was built
    /// from is recorded against its address and *copied* onto each function
    /// `get_function` makes (a copy rather than a move, so a template a host is
    /// serializing can still be read back by its own address).
    callback_templates: RefCell<HashMap<usize, BuiltCallback>>,
    /// The record a snapshot carries a template as, by the context whose realm
    /// the record was built in and the template's address.
    ///
    /// A snapshot item that is a template is written as a record object, and two
    /// templates that inherit from one parent have to name *one* record for it —
    /// the engine writes a value once, so sharing the object is what makes the
    /// load rebuild one parent rather than one per child. Keyed by the context as
    /// well because the record holds that realm's `%Object.prototype%`: a record
    /// built in one realm is not a value another realm's slot can carry.
    template_records: RefCell<HashMap<(u64, usize), Global<Data>>>,
    /// The object templates the host created on this isolate, held the same way
    /// and for the same reason as the function templates above.
    object_templates: RefCell<Vec<Rc<api::ObjectTemplate>>>,
    /// What a snapshot would carry, when this isolate is one a host is
    /// serializing. `None` for every isolate that is not, which is what makes
    /// the creator-only methods refuse on those.
    pub(crate) snapshot_creator: Option<SnapshotCreator>,
    /// The blob this isolate booted from, once its header checked out, and what
    /// has been read out of it. `None` for an isolate that booted from source —
    /// which is every isolate that was not handed a valid blob, since a blob
    /// that is not one of this engine's is not consumed.
    pub(crate) restore: RefCell<Option<SnapshotRestore>>,
    /// The host's external-reference table: the addresses a snapshot may name by
    /// index instead of holding. The host rebuilds it for every load, which is
    /// why it is the host's argument rather than part of a blob.
    pub(crate) externals: Vec<ExternalReference>,
    /// The host's promise-rejection callback
    /// (`v8::Isolate::SetPromiseRejectCallback`), once it installed one.
    ///
    /// Held on the isolate rather than inside the host-hook seam's struct because
    /// the seam is one `HostHooks` implementation for every callback a host
    /// installs, and installing one must not drop another; the seam reads each
    /// back by finding the isolate it was installed on. See
    /// [`Isolate::install_hooks`].
    promise_reject: Option<PromiseRejectCallback>,
    /// The host's streaming-compilation callback
    /// (`v8::Isolate::SetWasmStreamingCallback`), once it installed one.
    wasm_streaming: Option<StreamingCallback>,
    /// The host's wasm code-generation policy
    /// (`v8::Isolate::SetAllowWasmCodeGenerationCallback`), once it installed
    /// one. The engine reaches it through this bridge's `HostHooks` before it
    /// compiles a wasm module.
    allow_wasm_code_generation: Option<WasmCodeGenerationCallback>,
    /// The host's `import.meta` callback
    /// (`v8::Isolate::SetHostInitializeImportMetaObjectCallback`), once it
    /// installed one. The engine reaches it through this bridge's `HostHooks`
    /// when it makes a module's `import.meta`.
    import_meta: Option<HostInitializeImportMetaObjectCallback>,
    /// The host's dynamic-import callbacks
    /// (`v8::Isolate::SetHostImportModuleDynamicallyCallback`, and the
    /// with-phase one), once it installed them. The engine reaches them through
    /// this bridge's `HostHooks` when code evaluates `import(...)`.
    import_dynamically: Option<ImportModuleDynamicallyCallback>,
    import_dynamically_with_phase: Option<ImportModuleWithPhaseDynamicallyCallback>,
    /// The host's error-stack formatter
    /// (`v8::Isolate::SetPrepareStackTraceCallback`), once it installed one. The
    /// engine reaches it through this bridge's `HostHooks` when an error's
    /// `stack` is read.
    prepare_stack_trace: Option<PrepareStackTraceCallbackOwned>,
    /// The call-site prototype this isolate hands a host's formatter, built on
    /// the first read of a `stack` and kept so one isolate has one set of method
    /// functions rather than one per read.
    callsite_prototype: RefCell<Option<Global<Object>>>,
    /// The host's collection callbacks, in installation order
    /// (`v8::Isolate::AddGCPrologueCallback` and its epilogue sibling).
    pub(crate) gc_callbacks: RefCell<Vec<crate::heap::GcCallbackEntry>>,
    /// Whether the engine has this isolate's collection observer, which is
    /// installed with the first callback rather than once per callback.
    pub(crate) gc_observer_registered: Cell<bool>,
}

const _: () = assert!(std::mem::offset_of!(IsolateInner, engine) == 0);

/// The isolate's default security token: an ordinary object with no prototype.
///
/// Opaque by construction, which is all a token is — the only thing a host can
/// do with one is compare it against another (`v8::Context::GetSecurityToken`
/// with no token of the context's own, which is where V8 allocates one of these
/// per isolate too).
fn minted_security_token() -> Local<'static, Value> {
    Local::from_engine(api::Local::from(crux::value::Value::Object(
        crux::object::JsObject::ordinary_object_create(None),
    )))
}

/// Host objects are dropped before the engine is, which their destructors need.
///
/// A `cppgc` value's destructor may reach this isolate: dropping a
/// [`MicrotaskQueue`](crate::MicrotaskQueue) token releases its engine queue
/// there, and deno's `vm` is what does it — the token lives in the
/// `ContextifyContext` wrapper, so its destructor runs when this heap frees that
/// value. `engine` is the struct's first field, so a field-by-field drop would
/// take it away before the heap and the values in it go. Terminating the heap
/// here runs those destructors while the engine is still alive; `Heap`'s own
/// `Drop` then finds nothing left to free.
impl Drop for IsolateInner {
    fn drop(&mut self) {
        self.cpp_heap.terminate();
    }
}

/// The callback an interrupt request would run (v8::InterruptCallback).
pub type InterruptCallback =
    unsafe extern "C" fn(isolate: UnsafeRawIsolatePtr, data: *mut std::ffi::c_void);

/// The callback a promise rejection runs
/// (v8::Isolate::SetPromiseRejectCallback).
///
/// `extern "C"` here and in the three types below is the crate we stand in
/// for's shape, not a boundary this bridge has: a handle is `Rc`-backed and so
/// not FFI-safe, and the warning that says so is about a function pointer that is
/// only ever called from Rust.
#[allow(improper_ctypes_definitions)]
pub type PromiseRejectCallback = unsafe extern "C" fn(PromiseRejectMessage);

/// The host's streaming-compilation callback
/// (v8::Isolate::SetWasmStreamingCallback), stored on the isolate that installed
/// it.
///
/// The bound is higher-ranked rather than the crate's `MapFnTo` spelling for the
/// same reason a synthetic module's steps are: the engine calls this from a job,
/// where no scope of the host's is open, so the handles it hands over are made at
/// the call. See [`host_streaming_callback`].
type StreamingCallback =
    for<'a, 'b, 'c> fn(&'c mut PinScope<'a, 'b>, Local<'a, Value>, WasmStreaming<false>);

/// The host's wasm code-generation policy
/// (`v8::Isolate::SetAllowWasmCodeGenerationCallback`).
///
/// Asked before a wasm module is compiled, with the context the compile runs in;
/// `false` refuses it. The crate we stand in for declares this a plain
/// `extern "C"` function rather than a mapped one, so it is stored as the type
/// the host wrote and called directly.
pub type WasmCodeGenerationCallback =
    extern "C" fn(Local<Context>, Local<crate::data::String>) -> bool;

/// The function the engine's hook seam calls for a streaming compile: the host's
/// callback, reached through its type.
///
/// A mapped function pointer names the scope's lifetime and cannot be stored past
/// it; this fn item is lifetime-parameterized instead, and its parameters share
/// the lifetime of the source handle — the one the host's callback is required to
/// give the scope as well, so the call below satisfies that requirement by
/// construction rather than by inference.
fn host_streaming_callback<'a, 'i, 's, F>(
    scope: &'s mut PinScope<'a, 'i>,
    source: Local<'a, Value>,
    streaming: WasmStreaming<false>,
) where
    F: UnitType
        + for<'x, 'y, 'z> Fn(&'z mut PinScope<'x, 'y>, Local<'x, Value>, WasmStreaming<false>),
{
    (F::get())(scope, source, streaming)
}

/// The host's error-stack formatter, as the isolate keeps it
/// (v8::Isolate::SetPrepareStackTraceCallback).
///
/// Lifetime-parameterized for the same reason [`host_streaming_callback`] is: a
/// mapped function pointer names the scope's lifetime and cannot be stored past
/// it, so the host function is folded into this item and the item is what the
/// isolate holds.
type PrepareStackTraceCallbackOwned = for<'a, 'i, 's> fn(
    &'s mut PinScope<'a, 'i>,
    Local<'a, Value>,
    Local<'a, Array>,
) -> Local<'a, Value>;

fn host_prepare_stack_trace<'a, 'i, 's, F>(
    scope: &'s mut PinScope<'a, 'i>,
    error: Local<'a, Value>,
    sites: Local<'a, Array>,
) -> Local<'a, Value>
where
    F: UnitType
        + for<'x, 'y, 'z> Fn(
            &'z mut PinScope<'x, 'y>,
            Local<'x, Value>,
            Local<'x, Array>,
        ) -> Local<'x, Value>,
{
    (F::get())(scope, error, sites)
}

/// The callback a dynamic `import()` runs, as the isolate keeps it
/// (v8::Isolate::SetHostImportModuleDynamicallyCallback).
type ImportModuleDynamicallyCallback = for<'a, 'i, 's> fn(
    &'s mut PinScope<'a, 'i>,
    Local<'a, Data>,
    Local<'a, Value>,
    Local<'a, crate::data::String>,
    Local<'a, FixedArray>,
) -> Option<Local<'a, Promise>>;

/// The phase-aware one
/// (v8::Isolate::SetHostImportModuleWithPhaseDynamicallyCallback).
type ImportModuleWithPhaseDynamicallyCallback = for<'a, 'i, 's> fn(
    &'s mut PinScope<'a, 'i>,
    Local<'a, Data>,
    Local<'a, Value>,
    Local<'a, crate::data::String>,
    crate::ModuleImportPhase,
    Local<'a, FixedArray>,
) -> Option<Local<'a, Promise>>;

/// The concrete function that stands in for a host's dynamic-import callback: a
/// generic type parameter cannot coerce to a function pointer, so the host's
/// function item is reconstructed and called from one that can.
fn host_import_module_dynamically<'a, 'i, 's, F>(
    scope: &'s mut PinScope<'a, 'i>,
    options: Local<'a, Data>,
    resource_name: Local<'a, Value>,
    specifier: Local<'a, crate::data::String>,
    attributes: Local<'a, FixedArray>,
) -> Option<Local<'a, Promise>>
where
    F: UnitType
        + for<'x, 'y, 'z> FnOnce(
            &'z mut PinScope<'x, 'y>,
            Local<'x, Data>,
            Local<'x, Value>,
            Local<'x, crate::data::String>,
            Local<'x, FixedArray>,
        ) -> Option<Local<'x, Promise>>,
{
    (F::get())(scope, options, resource_name, specifier, attributes)
}

/// The same for the phase-aware callback.
fn host_import_module_with_phase_dynamically<'a, 'i, 's, F>(
    scope: &'s mut PinScope<'a, 'i>,
    options: Local<'a, Data>,
    resource_name: Local<'a, Value>,
    specifier: Local<'a, crate::data::String>,
    phase: crate::ModuleImportPhase,
    attributes: Local<'a, FixedArray>,
) -> Option<Local<'a, Promise>>
where
    F: UnitType
        + for<'x, 'y, 'z> FnOnce(
            &'z mut PinScope<'x, 'y>,
            Local<'x, Data>,
            Local<'x, Value>,
            Local<'x, crate::data::String>,
            crate::ModuleImportPhase,
            Local<'x, FixedArray>,
        ) -> Option<Local<'x, Promise>>,
{
    (F::get())(scope, options, resource_name, specifier, phase, attributes)
}

/// The callback an error's `stack` property would run
/// (v8::Isolate::SetPrepareStackTraceCallback).
///
/// Shaped after the *host function* rather than after the crate we stand in
/// for's C pointer: there the value travels back through the platform's calling
/// convention — a hidden return pointer on Windows, a register elsewhere — and
/// nothing crosses an ABI here, so the convention has nothing to carry.
pub type PrepareStackTraceCallback<'s> =
    fn(&mut PinScope<'s, '_>, Local<'s, Value>, Local<'s, Array>) -> Local<'s, Value>;

/// The callback `import.meta` would run the first time it is read
/// (v8::Isolate::SetHostInitializeImportMetaObjectCallback).
#[allow(improper_ctypes_definitions)] // as `PromiseRejectCallback` says.
pub type HostInitializeImportMetaObjectCallback =
    unsafe extern "C" fn(Local<Context>, Local<crate::data::Module>, Local<crate::data::Object>);

/// The callback a dynamic `import()` would run
/// (v8::Isolate::SetHostImportModuleDynamicallyCallback).
///
/// A trait rather than a function pointer, as there: the host's function is
/// generic over the scope's and the isolate's lifetimes, so the bound a caller
/// has to satisfy is higher-ranked.
pub trait HostImportModuleDynamicallyCallback:
    UnitType
    + for<'s, 'i> FnOnce(
        &mut PinScope<'s, 'i>,
        Local<'s, Data>,
        Local<'s, Value>,
        Local<'s, crate::data::String>,
        Local<'s, FixedArray>,
    ) -> Option<Local<'s, Promise>>
{
}

impl<F> HostImportModuleDynamicallyCallback for F where
    F: UnitType
        + for<'s, 'i> FnOnce(
            &mut PinScope<'s, 'i>,
            Local<'s, Data>,
            Local<'s, Value>,
            Local<'s, crate::data::String>,
            Local<'s, FixedArray>,
        ) -> Option<Local<'s, Promise>>
{
}

/// The same, for `import source`
/// (v8::Isolate::SetHostImportModuleWithPhaseDynamicallyCallback).
pub trait HostImportModuleWithPhaseDynamicallyCallback:
    UnitType
    + for<'s, 'i> FnOnce(
        &mut PinScope<'s, 'i>,
        Local<'s, Data>,
        Local<'s, Value>,
        Local<'s, crate::data::String>,
        crate::ModuleImportPhase,
        Local<'s, FixedArray>,
    ) -> Option<Local<'s, Promise>>
{
}

impl<F> HostImportModuleWithPhaseDynamicallyCallback for F where
    F: UnitType
        + for<'s, 'i> FnOnce(
            &mut PinScope<'s, 'i>,
            Local<'s, Data>,
            Local<'s, Value>,
            Local<'s, crate::data::String>,
            crate::ModuleImportPhase,
            Local<'s, FixedArray>,
        ) -> Option<Local<'s, Promise>>
{
}

/// Whether an asynchronous WebAssembly compilation succeeded
/// (v8::WasmAsyncSuccess).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub enum WasmAsyncSuccess {
    Success,
    Fail,
}

/// The callback an asynchronous WebAssembly compilation would settle
/// (v8::Isolate::SetWasmAsyncResolvePromiseCallback).
#[allow(improper_ctypes_definitions)] // as `PromiseRejectCallback` says.
pub type WasmAsyncResolvePromiseCallback = unsafe extern "C" fn(
    UnsafeRawIsolatePtr,
    Local<Context>,
    Local<PromiseResolver>,
    Local<Value>,
    WasmAsyncSuccess,
);

/// The callback a heap that is close to its limit would run
/// (v8::Isolate::AddNearHeapLimitCallback).
pub type NearHeapLimitCallback = unsafe extern "C" fn(
    data: *mut std::ffi::c_void,
    current_heap_limit: usize,
    initial_heap_limit: usize,
) -> usize;

impl<'s, F> MapFnFrom<F> for PrepareStackTraceCallback<'s>
where
    F: UnitType
        + for<'a> Fn(&mut PinScope<'s, 'a>, Local<'s, Value>, Local<'s, Array>) -> Local<'s, Value>,
{
    fn mapping() -> Self {
        // The mapped shape names the caller's scope lifetime, which cannot be
        // stored past it; a host's callback is taken as the fn item that erases
        // it instead — see [`Isolate::set_prepare_stack_trace_callback`], which
        // is where a host function is folded into one.
        fn run<'s, 'a, F>(
            scope: &mut PinScope<'s, 'a>,
            error: Local<'s, Value>,
            sites: Local<'s, Array>,
        ) -> Local<'s, Value>
        where
            F: UnitType
                + for<'b> Fn(
                    &mut PinScope<'s, 'b>,
                    Local<'s, Value>,
                    Local<'s, Array>,
                ) -> Local<'s, Value>,
        {
            (F::get())(scope, error, sites)
        }

        run::<F>
    }
}

/// A reference to an isolate that another thread may keep
/// (v8::IsolateHandle).
///
/// The crate we stand in for hands out a reference-counted handle that reaches
/// the isolate to terminate it or to run a callback at its next safepoint. Slag's
/// isolate is thread-local and cannot leave its own thread, so this handle carries
/// the one piece of isolate state that can: the termination request. A host that
/// keeps one across threads — a watchdog — can stop a running execution with it;
/// the interrupt request remains one it cannot make, because the engine runs to
/// completion on the calling thread.
#[derive(Clone, Debug)]
pub struct IsolateHandle {
    termination: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl IsolateHandle {
    /// Ask the isolate to stop executing
    /// (v8::IsolateHandle::TerminateExecution).
    ///
    /// Answers `true` when the request was made. The running execution observes
    /// it at its next check point — a loop's back edge, a compiled loop's probe,
    /// or a call — and there throws `Error("execution terminated")`.
    pub fn terminate_execution(&self) -> bool {
        self.termination
            .store(true, std::sync::atomic::Ordering::Relaxed);
        true
    }

    /// Withdraw a termination request
    /// (v8::IsolateHandle::CancelTerminateExecution).
    ///
    /// Answers `true`: the request is withdrawn from whatever state it is in, and
    /// the isolate runs again either way.
    pub fn cancel_terminate_execution(&self) -> bool {
        self.termination
            .store(false, std::sync::atomic::Ordering::Relaxed);
        true
    }

    /// Whether the isolate is terminating an execution
    /// (v8::IsolateHandle::IsExecutionTerminating).
    pub fn is_execution_terminating(&self) -> bool {
        self.termination.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Ask the isolate to run `callback` when it next reaches a safepoint
    /// (v8::IsolateHandle::RequestInterrupt).
    ///
    /// Answers `false` and does not call `callback`: the engine runs to
    /// completion on the calling thread, so there is no safepoint at which it
    /// could.
    pub fn request_interrupt(
        &self,
        callback: InterruptCallback,
        data: *mut std::ffi::c_void,
    ) -> bool {
        let _ = (callback, data);
        false
    }
}

/// A raw isolate pointer (v8::UnsafeRawIsolatePtr).
///
/// Transparent over a pointer, so a host may transmute it to a `usize` and key a
/// map by it, which is what the crate we stand in for allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct UnsafeRawIsolatePtr(*mut IsolateInner);

impl UnsafeRawIsolatePtr {
    /// The null pointer.
    pub fn null() -> Self {
        Self(std::ptr::null_mut())
    }

    pub fn is_null(&self) -> bool {
        self.0.is_null()
    }

    pub(crate) fn from_inner_ptr(ptr: *mut IsolateInner) -> Self {
        Self(ptr)
    }
}

/// A heap and execution state (v8::Isolate).
///
/// A handle, not the state: [`OwnedIsolate`] owns an [`IsolateInner`] and every
/// scope carries a copy of this handle. That is why it is `Copy`, as it is in
/// the crate we stand in for, and why a scope can be opened from a raw pointer.
///
/// Handles alias the same state, so a method taking `&mut self` only means the
/// caller has no other handle *at hand* — the same convention the rest of this
/// bridge follows for the isolate.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct Isolate(NonNull<IsolateInner>);

/// How a host identified a date-time configuration change
/// (`v8::TimeZoneDetection`).
///
/// `Redetect` is the only level the crate we stand in for names; it asks the
/// engine to re-read the host's timezone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeZoneDetection {
    Redetect,
}

impl Isolate {
    /// Create parameters with the defaults (`v8::Isolate::CreateParams`).
    pub fn create_params() -> CreateParams {
        CreateParams::default()
    }

    /// A fresh isolate, booting from source rather than from `params`' snapshot —
    /// see [`CreateParams`] for what that means for a host.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(params: CreateParams) -> OwnedIsolate {
        Self::with(params, None)
    }

    /// An isolate set up for serialization
    /// (v8::SnapshotCreator, and `v8::Isolate::SnapshotCreator`).
    ///
    /// The isolate is a real one and the creator records what a blob would
    /// carry; the blob itself cannot be produced — see
    /// [`OwnedIsolate::create_blob`].
    #[allow(clippy::new_ret_no_self)]
    pub fn snapshot_creator(
        external_references: Option<std::borrow::Cow<'static, [crate::ExternalReference]>>,
        params: Option<CreateParams>,
    ) -> OwnedIsolate {
        SnapshotCreator::new(external_references, params)
    }

    /// The same, continuing from a snapshot the host carries.
    #[allow(clippy::new_ret_no_self)]
    pub fn snapshot_creator_from_existing_snapshot(
        existing_snapshot_blob: StartupData,
        external_references: Option<std::borrow::Cow<'static, [crate::ExternalReference]>>,
        params: Option<CreateParams>,
    ) -> OwnedIsolate {
        SnapshotCreator::from_existing_snapshot(existing_snapshot_blob, external_references, params)
    }

    /// An isolate with a creator attached, or without one.
    ///
    /// `params`' snapshot and external references go the way they always do:
    /// carried and not consumed (see [`CreateParams`]). Its heap does not — the
    /// isolate takes ownership of one, because every isolate here has one.
    pub(crate) fn with(params: CreateParams, creator: Option<SnapshotCreator>) -> OwnedIsolate {
        let mut engine = *api::Isolate::new();
        if let Some((initial, maximum)) = params.heap_limits {
            engine.set_heap_limits(initial, maximum);
        }
        let cpp_heap = match params.cpp_heap {
            Some(heap) => heap,
            None => Heap::new(),
        };
        // A blob this engine cannot read is carried and not consumed, the same
        // statement `StartupData::is_valid` makes: the isolate boots from source
        // and the host finds out by asking rather than by being refused.
        let restore = params
            .snapshot_blob
            .and_then(crate::snapshot::SnapshotRestore::new);
        let externals = params
            .external_references
            .map(|refs| refs.into_owned())
            .unwrap_or_default();
        let mut inner = Box::new(IsolateInner {
            engine,
            context: RefCell::new(None),
            continuation_data: None,
            slots: UnsafeCell::new(HashMap::new()),
            context_slots: RefCell::new(HashMap::new()),
            private_names: RefCell::new(HashMap::new()),
            fresh_privates: RefCell::new(Vec::new()),
            resolver_rejects: RefCell::new(HashMap::new()),
            cpp_heap,
            templates: RefCell::new(Vec::new()),
            extras_bindings: RefCell::new(std::collections::HashMap::new()),
            security_tokens: RefCell::new(std::collections::HashMap::new()),
            default_security_token: RefCell::new(None),
            positions: RefCell::new(HashMap::new()),
            callbacks: RefCell::new(HashMap::new()),
            callback_templates: RefCell::new(HashMap::new()),
            template_records: RefCell::new(HashMap::new()),
            object_templates: RefCell::new(Vec::new()),
            snapshot_creator: creator,
            restore: RefCell::new(restore),
            externals,
            promise_reject: None,
            wasm_streaming: None,
            allow_wasm_code_generation: None,
            import_meta: None,
            import_dynamically: None,
            import_dynamically_with_phase: None,
            prepare_stack_trace: None,
            callsite_prototype: RefCell::new(None),
            gc_callbacks: RefCell::new(Vec::new()),
            gc_observer_registered: Cell::new(false),
        });
        // SAFETY: the box's allocation is where the state lives and it outlives
        // every handle to it — `OwnedIsolate` keeps it alive.
        let handle = unsafe { Self::from_inner_ptr(&mut *inner) };
        OwnedIsolate { inner, handle }
    }

    /// Take the restore out of this isolate, so the caller can make handles
    /// from it without holding a borrow of the isolate that they need.
    pub(crate) fn take_restore(&mut self) -> Option<SnapshotRestore> {
        self.inner_mut().restore.borrow_mut().take()
    }

    pub(crate) fn put_restore(&mut self, restore: SnapshotRestore) {
        *self.inner_mut().restore.borrow_mut() = Some(restore);
    }

    /// The external-reference table this isolate was built with: the addresses a
    /// blob's indices resolve against.
    pub(crate) fn externals(&self) -> &[ExternalReference] {
        &self.inner().externals
    }

    /// The handle for the state at `ptr`.
    ///
    /// # Safety
    ///
    /// `ptr` must point at a live `IsolateInner`.
    pub(crate) unsafe fn from_inner_ptr(ptr: *mut IsolateInner) -> Self {
        // SAFETY: the caller's contract.
        unsafe { Self(NonNull::new_unchecked(ptr)) }
    }

    /// The handle for the isolate an engine pointer belongs to.
    ///
    /// # Safety
    ///
    /// `ptr` must be the engine isolate of a live bridge isolate; the engine
    /// isolate is the first field of [`IsolateInner`], which is what makes the
    /// two addresses the same.
    pub(crate) unsafe fn from_engine_ptr(ptr: *mut api::Isolate) -> Self {
        // SAFETY: the caller's contract, and the assertion above.
        unsafe { Self::from_inner_ptr(ptr.cast()) }
    }

    /// The handle for a pointer a host kept
    /// (v8::Isolate::from_raw_isolate_ptr).
    ///
    /// # Safety
    ///
    /// `ptr` must have come from [`as_raw_isolate_ptr`](Self::as_raw_isolate_ptr)
    /// on an isolate that is still alive.
    pub unsafe fn from_raw_isolate_ptr(ptr: UnsafeRawIsolatePtr) -> Self {
        // SAFETY: the caller's contract.
        unsafe { Self(NonNull::new_unchecked(ptr.0)) }
    }

    /// The handle for a pointer a host kept, under the name a collection
    /// callback uses (v8::Isolate::from_raw_isolate_ptr_unchecked).
    ///
    /// The crate separates this from [`from_raw_isolate_ptr`](Self::from_raw_isolate_ptr)
    /// by a thread-affinity assertion, not by the conversion: this bridge makes no
    /// such assertion anywhere (its isolate handle is a plain pointer, and the
    /// engine's own thread rules are what a host obeys), so the two do the same
    /// thing and this carries the name the callback sites read. It answers the
    /// handle by value where there is a `&'static mut` borrow, because there is no
    /// `Isolate` in host memory to borrow — the pointer *is* the handle.
    ///
    /// # Safety
    ///
    /// `ptr` must name an isolate that is still alive.
    pub unsafe fn from_raw_isolate_ptr_unchecked(ptr: UnsafeRawIsolatePtr) -> Self {
        // SAFETY: the caller's contract.
        unsafe { Self(NonNull::new_unchecked(ptr.0)) }
    }

    /// This isolate's inner address, for the bridge's own modules that have to
    /// hand an isolate back to a host (see `heap`'s collection observer).
    pub(crate) fn as_inner_ptr(&self) -> *mut IsolateInner {
        self.0.as_ptr()
    }

    /// This isolate's address, for a host that keeps one across calls
    /// (v8::Isolate::as_raw_isolate_ptr).
    ///
    /// # Safety
    ///
    /// The handle must be read on the thread the isolate was created on.
    pub unsafe fn as_raw_isolate_ptr(&self) -> UnsafeRawIsolatePtr {
        UnsafeRawIsolatePtr::from_inner_ptr(self.0.as_ptr())
    }

    /// The handle for a host's stored raw pointer, borrowed rather than copied
    /// (v8::Isolate::ref_from_raw_isolate_ptr_mut_unchecked).
    ///
    /// # Safety
    ///
    /// `ptr` must point at an [`UnsafeRawIsolatePtr`] naming an isolate that is
    /// still alive, and the borrow must not outlive that storage. The crate's
    /// contract also has the caller check the thread the isolate belongs to;
    /// this bridge makes no such check anywhere, as its other raw-pointer
    /// conversions state.
    pub unsafe fn ref_from_raw_isolate_ptr_mut_unchecked<'a>(
        ptr: *mut UnsafeRawIsolatePtr,
    ) -> &'a mut Isolate {
        // SAFETY: an `Isolate` is this bridge's handle for the same
        // `*mut IsolateInner` an `UnsafeRawIsolatePtr` wraps, and it is
        // `repr(transparent)` over that pointer, so the host's storage for the
        // raw pointer is a valid handle for the isolate the caller names there.
        unsafe { &mut *ptr.cast::<Isolate>() }
    }

    /// `v8::Isolate::SetData`.
    ///
    /// A host pointer, as there; the engine keeps slots of its own and this is
    /// the same number seen as an address. The receiver is `&self` where the
    /// crate's is `&mut self`, which is the permissive direction: every call
    /// that compiles there compiles here.
    pub fn set_data(&self, slot: u32, data: *mut c_void) {
        self.inner().engine.set_data(slot, data as usize);
    }

    /// `v8::Isolate::GetData`, null for a slot that was never set.
    pub fn get_data(&self, slot: u32) -> *mut c_void {
        self.inner().engine.get_data(slot).unwrap_or(0) as *mut c_void
    }

    /// The heap this isolate owns for host objects
    /// (`v8::Isolate::GetCppHeap`).
    ///
    /// Always `Some`, where the crate we stand in for answers null unless
    /// `CreateParams` was handed a heap. The host this stands in for unwraps the
    /// answer unconditionally, so an isolate here always owns one — the host's
    /// own when it passed one through [`CreateParams::cpp_heap`], and an empty
    /// one otherwise.
    pub fn get_cpp_heap(&self) -> Option<&Heap> {
        Some(&self.inner().cpp_heap)
    }

    /// Keep the value a scope's `SetContinuationPreservedEmbedderData` stores.
    ///
    /// The receiver is the *scope* there, as it is for the getter below: the
    /// value survives a suspension, and the scope is what a host has in hand
    /// while one is in flight.
    pub(crate) fn set_continuation_data(&mut self, data: Local<Value>) {
        self.inner_mut().continuation_data = Some(Global::new(self, data));
    }

    /// The payload a scope turns into its own-lifetime handle for
    /// `GetContinuationPreservedEmbedderData`, *undefined* when nothing was
    /// stored.
    ///
    /// A payload rather than a handle: the crate ties the answer to the scope's
    /// lifetime, so the scope rebuilds the handle and this only names what it
    /// holds.
    pub(crate) fn continuation_data_payload(&self) -> Payload {
        self.inner().continuation_data.as_ref().map_or(
            Payload::Value(api::Local::undefined()),
            Global::payload_value,
        )
    }

    /// A reference to the host data of type `T` (v8::Isolate::GetData's
    /// type-keyed counterpart).
    ///
    /// The caller must not hold the reference across a
    /// [`set_slot`](Self::set_slot) or [`remove_slot`](Self::remove_slot) of the
    /// same type: those replace and drop it, which is the contract the crate we
    /// stand in for documents for this method too.
    pub fn get_slot<T: 'static>(&self) -> Option<&T> {
        // SAFETY: the slot map is only replaced through `&mut self`, and the
        // caller's contract covers the hold-across-write case.
        let slots = unsafe { &*self.inner().slots.get() };
        slots.get(&TypeId::of::<T>())?.downcast_ref::<T>()
    }

    /// A mutable reference to the host data of type `T`
    /// (v8::Isolate::GetDataMut).
    pub fn get_slot_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.slots_mut()
            .get_mut(&TypeId::of::<T>())
            .and_then(|value| value.downcast_mut::<T>())
    }

    /// Give the isolate ownership of `value` (v8::Isolate::SetData): one value
    /// per type, replacing an earlier one. Answers whether it was set without
    /// replacing, which is the convention there.
    pub fn set_slot<T: 'static>(&mut self, value: T) -> bool {
        self.slots_mut()
            .insert(TypeId::of::<T>(), Box::new(value))
            .is_none()
    }

    /// Take back the host data of type `T` (v8::Isolate::RemoveData).
    pub fn remove_slot<T: 'static>(&mut self) -> Option<T> {
        let value = self.slots_mut().remove(&TypeId::of::<T>())?;
        value.downcast::<T>().ok().map(|value| *value)
    }

    /// `v8::Isolate::ThrowException`.
    pub fn throw_exception(&self, exception: Local<'_, Value>) {
        self.inner()
            .engine
            .throw_exception(exception.into_engine().into_value());
    }

    /// Drain the microtask and job queues (`v8::Isolate::PerformMicrotaskCheckpoint`).
    pub fn run_microtasks(&mut self) -> Result<(), crux::error::JsError> {
        self.engine_mut().run_microtasks()
    }

    /// Add a microtask that calls `callback` with no arguments when the queues
    /// drain (v8::Isolate::EnqueueMicrotask).
    ///
    /// The engine's microtasks are its promise jobs, so the callback runs with
    /// them, in the realm this isolate has entered.
    pub fn enqueue_microtask(&mut self, callback: Local<'_, Function>) {
        let context = self.current_context().unwrap_or_else(crate::realm_current);
        context.enqueue_microtask(callback.into_engine());
    }

    /// Run the queues until they are empty
    /// (`v8::Isolate::PerformMicrotaskCheckpoint`), which is how a host drains
    /// them while the policy is explicit.
    ///
    /// One divergence, and it is the one this bridge already records for the
    /// `Auto` policy: the crate swallows what a job throws, and this makes it the
    /// pending exception instead, so a host that wants to see it can.
    pub fn perform_microtask_checkpoint(&mut self) {
        if let Err(error) = self.run_microtasks() {
            crate::throw(self, &error);
        }
    }

    /// When the queues drain without the host asking
    /// (v8::Isolate::GetMicrotasksPolicy).
    ///
    /// The engine defaults to `Explicit` where the crate we stand in for
    /// defaults to `Auto`: a host inherits no draining it did not ask for, and
    /// says so when it wants one.
    pub fn get_microtasks_policy(&self) -> crate::MicrotasksPolicy {
        self.engine().get_microtasks_policy()
    }

    /// When the queues drain without the host asking
    /// (v8::Isolate::SetMicrotasksPolicy).
    pub fn set_microtasks_policy(&mut self, policy: crate::MicrotasksPolicy) {
        self.engine_mut().set_microtasks_policy(policy);
    }

    /// Set the context a deserialized isolate would start in
    /// (v8::Isolate::SetDefaultContext).
    ///
    /// # Panics
    ///
    /// Panics if the isolate did not come from
    /// [`snapshot_creator`](Self::snapshot_creator), as the crate we stand in
    /// for does.
    pub fn set_default_context(&mut self, context: Local<Context>) {
        self.creator().set_default_context(context.context());
    }

    /// Add another context to the snapshot, answering its index
    /// (v8::Isolate::AddContext).
    ///
    /// # Panics
    ///
    /// Panics if the isolate did not come from
    /// [`snapshot_creator`](Self::snapshot_creator).
    pub fn add_context(&mut self, context: Local<Context>) -> usize {
        self.creator().add_context(context.context())
    }

    /// Attach `data` to the context snapshot, answering its index
    /// (v8::Isolate::AddContextData).
    ///
    /// # Panics
    ///
    /// Panics if the isolate did not come from
    /// [`snapshot_creator`](Self::snapshot_creator), as the crate we stand in
    /// for does, or if the context is not one the snapshot carries.
    pub fn add_context_data<T>(&mut self, context: Local<Context>, data: Local<'_, T>) -> usize
    where
        for<'l> Local<'l, T>: Into<Local<'l, Data>>,
    {
        let data: Local<'_, Data> = data.into();
        // A template handle is not a value the engine can carry: its payload is an
        // address into *this* process. The item is written as the record a load
        // rebuilds a template from, when it is one of this isolate's templates.
        let data = match data.payload() {
            Payload::Value(_) => {
                crate::snapshot::template_record(self, context.context(), data).unwrap_or(data)
            }
            _ => data,
        };
        let held = Global::new(self, data);
        self.creator().add_context_data(context.context(), held)
    }

    /// The creator this isolate was made with, or a panic saying it has none.
    fn creator(&mut self) -> &mut SnapshotCreator {
        self.inner_mut()
            .snapshot_creator
            .as_mut()
            .expect("v8::Isolate: this isolate was not created by Isolate::snapshot_creator")
    }

    /// Give an isolate its creator (the snapshot constructors' entry).
    pub(crate) fn with_snapshot_creator(
        params: CreateParams,
        creator: SnapshotCreator,
    ) -> OwnedIsolate {
        Self::with(params, Some(creator))
    }

    /// Whether the isolate is terminating an execution
    /// (`v8::Isolate::IsExecutionTerminating`).
    pub fn is_execution_terminating(&self) -> bool {
        self.engine().is_execution_terminating()
    }

    /// A handle another thread may keep
    /// (`v8::Isolate::GetThreadSafeHandle`), which reaches this isolate's
    /// termination request — see [`IsolateHandle`].
    pub fn thread_safe_handle(&self) -> IsolateHandle {
        IsolateHandle {
            termination: self.engine().termination_flag(),
        }
    }

    /// Ask this isolate to stop executing
    /// (`v8::Isolate::TerminateExecution`).
    ///
    /// Answers `true`: the request is made, and the execution that is running
    /// observes it at its next check point.
    pub fn terminate_execution(&self) -> bool {
        self.engine().terminate_execution();
        true
    }

    /// Withdraw a termination request
    /// (`v8::Isolate::CancelTerminateExecution`).
    pub fn cancel_terminate_execution(&self) -> bool {
        self.engine().cancel_terminate_execution();
        true
    }

    /// V8's `Isolate::DateTimeConfigurationChangeNotification`: a host's signal
    /// that the date-time configuration changed and cached local-time state must
    /// be re-read.
    ///
    /// Nothing happens, and that is the whole answer: this engine fixes the
    /// local-time offset at UTC (`builtins::date`), so it caches no date-time
    /// configuration for the notification to invalidate. A host that changes the
    /// process timezone sees the same dates either way — the divergence
    /// `.notes/embedding.md` §9 records, not a gap in this method.
    pub fn date_time_configuration_change_notification(&mut self, _detection: TimeZoneDetection) {}

    /// Add `change` bytes to the isolate's external-memory account and answer the
    /// new total (`v8::Isolate::AdjustAmountOfExternalAllocatedMemory`).
    ///
    /// This is the one lever a host has to tell the engine about memory it holds
    /// outside the heap — a GPU device's backing, a natively allocated buffer —
    /// which the engine cannot see for itself. The total it answers is what
    /// [`HeapStatistics::external_memory`](crate::HeapStatistics::external_memory)
    /// reports beside the buffers the agent holds, and it floors at zero, so an
    /// over-correcting host is told it holds no external memory rather than a
    /// negative number of bytes.
    ///
    /// One divergence, recorded in `.notes/embedding.md` §9: V8 feeds this number
    /// to its GC heuristics, so a host's `+16 MiB` nudges a collection; this
    /// engine's collections are its own growth policy, so the account is reported
    /// rather than consulted.
    pub fn adjust_amount_of_external_allocated_memory(&mut self, change: i64) -> i64 {
        self.engine_mut().adjust_external_memory(change)
    }

    pub(crate) fn engine(&self) -> &api::Isolate {
        &self.inner().engine
    }

    pub(crate) fn engine_mut(&mut self) -> &mut api::Isolate {
        &mut self.inner_mut().engine
    }

    pub(crate) fn current_context(&self) -> Option<api::Context> {
        *self.inner().context.borrow()
    }

    pub(crate) fn set_current_context(&self, context: Option<api::Context>) {
        *self.inner().context.borrow_mut() = context;
    }

    /// Take ownership of a function template, so a handle can name its address
    /// for as long as the isolate lives.
    pub(crate) fn add_template(&self, template: Rc<api::FunctionTemplate>) {
        self.inner().templates.borrow_mut().push(template);
    }

    /// Record what a template was built from, by the address its handle names —
    /// the value [`materialized`](Self::materialized) moves when the template
    /// finally makes a function.
    pub(crate) fn template_callback(
        &self,
        address: usize,
        callback: usize,
        data: Option<Global<Value>>,
    ) {
        self.inner()
            .callback_templates
            .borrow_mut()
            .insert(address, BuiltCallback { callback, data });
    }

    /// What a template was built from, by the address its handle names: the
    /// callback address and the data it was built with, as
    /// [`template_callback`](Self::template_callback) recorded them. `None` for
    /// an address this isolate holds no template under.
    ///
    /// It stays readable after the template has materialized a function, which is
    /// what a snapshot of the template needs: [`materialized`](Self::materialized)
    /// *copies* the record onto the function rather than moving it, so both
    /// identities — the address a template handle names, and the id of the function
    /// it made — answer for the same callback.
    pub(crate) fn template_parts(&self, address: usize) -> Option<(usize, Option<Global<Value>>)> {
        self.inner()
            .callback_templates
            .borrow()
            .get(&address)
            .map(|recorded| (recorded.callback, recorded.data.clone()))
    }

    /// The record a snapshot already built for a template in `context`'s realm, if
    /// it has one: what makes two templates that share a parent name *one* record
    /// for it.
    pub(crate) fn template_record_for(&self, context: u64, address: usize) -> Option<Global<Data>> {
        self.inner()
            .template_records
            .borrow()
            .get(&(context, address))
            .cloned()
    }

    /// Remember the record a snapshot built for a template, before its properties
    /// are filled: a template that inherits from one already being written (or from
    /// itself) then names the record under construction rather than starting a
    /// second one.
    pub(crate) fn remember_template_record(
        &self,
        context: u64,
        address: usize,
        record: Global<Data>,
    ) {
        self.inner()
            .template_records
            .borrow_mut()
            .insert((context, address), record);
    }

    /// Copy a template's callback and data onto the function it just materialized,
    /// which is the identity the engine's snapshot walk asks a *function* with.
    ///
    /// A copy rather than a move, so the template's own address keeps answering:
    /// a host's template item is written from what the template was built from, and
    /// by the time a snapshot is taken the template has usually materialized the
    /// function already (a host that registers a template materializes it — deno
    /// does, for every op class it registers).
    pub(crate) fn materialized(&self, address: usize, function: u64) {
        let recorded = self
            .inner()
            .callback_templates
            .borrow()
            .get(&address)
            .cloned();
        if let Some(recorded) = recorded {
            self.inner()
                .callbacks
                .borrow_mut()
                .insert(function, recorded);
        }
    }

    /// Record that a *load* rebuilt the function identified by `function` from
    /// the table entry at `pointer`, with `data` — the same entry shape
    /// [`materialized`](Self::materialized) keeps for a template's function, so
    /// the write side finds a restored function the way it finds a built one.
    pub(crate) fn record_rebuilt_callback(
        &self,
        function: u64,
        pointer: usize,
        data: Option<api::Local>,
    ) {
        let data = data.map(|value| Global::new(self, Local::from_engine(value)));
        self.inner().callbacks.borrow_mut().insert(
            function,
            BuiltCallback {
                callback: pointer,
                data,
            },
        );
    }

    /// Every callback the host built a function from, by that function's
    /// identity, taken: what `create_blob` writes as indices into the host's
    /// table. Taken rather than cloned because the values are pins, and because
    /// the isolate that calls this is the one being consumed.
    pub(crate) fn take_callbacks(&self) -> HashMap<u64, BuiltCallback> {
        std::mem::take(&mut self.inner().callbacks.borrow_mut())
    }

    /// Write the host's pointer into one of a context's slots
    /// (`v8::Context::SetAlignedPointerInEmbedderData`).
    pub(crate) fn set_context_slot(&self, context: u64, index: i32, pointer: usize) {
        self.inner()
            .context_slots
            .borrow_mut()
            .insert((context, index), pointer);
    }

    /// The pointer in one of a context's slots, or null when the host never
    /// wrote one there.
    pub(crate) fn context_slot(&self, context: u64, index: i32) -> usize {
        self.inner()
            .context_slots
            .borrow()
            .get(&(context, index))
            .copied()
            .unwrap_or(0)
    }

    /// Forget every slot written for one context
    /// (`v8::Context::ClearAllSlots`), leaving other contexts' slots alone.
    pub(crate) fn clear_context_slots(&self, context: u64) {
        self.inner()
            .context_slots
            .borrow_mut()
            .retain(|(owner, _), _| *owner != context);
    }

    /// Record where a thrown error came from, for the `v8::Message` a host makes
    /// from it later ([`position`](crate::position)).
    ///
    /// A primitive has no identity to key by, and no message made from one can
    /// carry a position either, so it records nothing.
    pub(crate) fn record_position(&self, thrown: &api::Local, position: Position) {
        let Some(object) = thrown.as_object() else {
            return;
        };
        self.inner()
            .positions
            .borrow_mut()
            .insert(object.id(), position);
    }

    /// The position recorded for `thrown`, if the bridge recorded one.
    ///
    /// Nothing removes an entry: the object's identity keys it, and only a
    /// collection knows an object is gone. The engine's own per-object tables
    /// (`error_data`, `error_stack`) hold their entries the same way.
    pub(crate) fn position_of(&self, thrown: &api::Local) -> Option<Position> {
        let object = thrown.as_object()?;
        self.inner().positions.borrow().get(&object.id()).cloned()
    }

    /// The private name that goes with `description`, minting and keeping one
    /// the first time it is asked for (`v8::Private::ForApi`).
    ///
    /// The returned value is a symbol, which is how a private name exists here
    /// at all — see [`private`](crate::private) for what that does and does not
    /// hide.
    pub(crate) fn private_symbol(&self, description: Option<&[u16]>) -> api::Local {
        let key = description.unwrap_or_default().to_vec();
        if let Some(held) = self.inner().private_names.borrow().get(&key) {
            return held.engine_value();
        }
        let symbol = crux::symbol::Symbol::new(description.map(JsString::from_utf16));
        let value = api::Local::from(crux::value::Value::Symbol(crux::handle::Handle::new(
            symbol,
        )));
        self.inner()
            .private_names
            .borrow_mut()
            .insert(key, Global::new(self, Local::from_engine(value)));
        value
    }

    /// A private name minted fresh for `description` (`v8::Private::new`): unlike
    /// [`private_symbol`](Self::private_symbol) it consults no registry, so two
    /// calls with one description are two names, and it keeps what it minted so
    /// the name is still recognized as a private name.
    pub(crate) fn private_symbol_fresh(&self, description: Option<&[u16]>) -> api::Local {
        let symbol = crux::symbol::Symbol::new(description.map(JsString::from_utf16));
        let value = api::Local::from(crux::value::Value::Symbol(crux::handle::Handle::new(
            symbol,
        )));
        self.inner()
            .fresh_privates
            .borrow_mut()
            .push(Global::new(self, Local::from_engine(value)));
        value
    }

    /// Whether this isolate minted `symbol` as a private name, registered for a
    /// description or minted fresh.
    pub(crate) fn owns_private(&self, symbol: Handle<crux::symbol::Symbol>) -> bool {
        let inner = self.inner();
        let matched =
            |held: &Global<Value>| held.engine_value().value().as_symbol() == Some(symbol);
        inner.private_names.borrow().values().any(matched)
            || inner.fresh_privates.borrow().iter().any(matched)
    }

    /// Keep a resolver pair's rejecting function (`v8::Promise::Resolver`).
    pub(crate) fn add_resolver_reject(&self, resolve: u64, reject: api::Local) {
        self.inner()
            .resolver_rejects
            .borrow_mut()
            .insert(resolve, Global::new(self, Local::from_engine(reject)));
    }

    /// The rejecting function of the pair whose resolve function has this
    /// identity, when the bridge made the pair.
    pub(crate) fn resolver_reject(&self, resolve: u64) -> Option<api::Local> {
        self.inner()
            .resolver_rejects
            .borrow()
            .get(&resolve)
            .map(|held| held.engine_value())
    }

    /// Whether this isolate stores a function template at `pointer`.
    ///
    /// This is what a cast to [`FunctionTemplate`](crate::FunctionTemplate)
    /// asks: a template handle carries an `External` naming the address the
    /// isolate took ownership of, and a host pointer the host wrapped itself is
    /// some other address.
    pub(crate) fn owns_template(&self, pointer: *mut c_void) -> bool {
        self.inner()
            .templates
            .borrow()
            .iter()
            .any(|template| Rc::as_ptr(template) as *mut c_void == pointer)
    }

    /// Take ownership of an object template, so a handle can name its address
    /// for as long as the isolate lives.
    ///
    /// The sibling of [`add_template`](Self::add_template) rather than the same
    /// list: a handle names the address it was registered under, and the two
    /// kinds are then told apart by which list holds it.
    pub(crate) fn add_object_template(&self, template: Rc<api::ObjectTemplate>) {
        self.inner().object_templates.borrow_mut().push(template);
    }

    /// The extras binding object a context was given, if its host asked for one
    /// (`v8::Context::GetExtrasBindingObject`).
    pub(crate) fn extras_binding(&self, context: u64) -> Option<api::Local> {
        self.inner()
            .extras_bindings
            .borrow()
            .get(&context)
            .map(|held| held.engine_value())
    }

    /// Remember a context's extras binding object, pinned on this isolate.
    pub(crate) fn set_extras_binding(&self, context: u64, value: Local<'_, Object>) {
        self.inner()
            .extras_bindings
            .borrow_mut()
            .insert(context, Global::new(self, value));
    }

    /// The token `context` was given, or `None` when it is still on the
    /// isolate's default (`v8::Context::GetSecurityToken`).
    pub(crate) fn security_token(&self, context: u64) -> Option<api::Local> {
        let tokens = self.inner().security_tokens.borrow();
        tokens.get(&context).map(|held| held.engine_value())
    }

    /// Give `context` a token (`v8::Context::SetSecurityToken`).
    pub(crate) fn set_security_token(&self, context: u64, value: Local<'_, Value>) {
        self.inner()
            .security_tokens
            .borrow_mut()
            .insert(context, Global::new(self, value));
    }

    /// Forget `context`'s token, so it answers the isolate's default again
    /// (`v8::Context::UseDefaultSecurityToken`).
    pub(crate) fn use_default_security_token(&self, context: u64) {
        self.inner().security_tokens.borrow_mut().remove(&context);
    }

    /// The isolate's default token, minted on the first ask
    /// (`v8::Context::GetSecurityToken` with no token of its own).
    ///
    /// An ordinary object with no prototype, which is what it is there: the
    /// engine allocates one opaque object per isolate and every context without
    /// a token of its own answers it, so the only thing a host can do with one
    /// is compare it against another.
    pub(crate) fn default_security_token(&self) -> api::Local {
        let mut held = self.inner().default_security_token.borrow_mut();
        if let Some(token) = held.as_ref() {
            return token.engine_value();
        }
        let token = Global::new(self, minted_security_token());
        let value = token.engine_value();
        *held = Some(token);
        value
    }

    /// Whether the isolate has background work pending
    /// (v8::Isolate::HasPendingBackgroundTasks).
    ///
    /// `false`, and honestly so: a background task is work the isolate posted to
    /// another thread, and this engine posts none — [`Platform`](crate::Platform)
    /// is the shape for that same gap. Work the host could *make progress on* is
    /// a different question, and the job queues answer it: those are drained by
    /// [`run_microtasks`](Self::run_microtasks), and a queued job is not a
    /// background task.
    pub fn has_pending_background_tasks(&self) -> bool {
        false
    }

    /// Tells the engine to capture a stack trace when an exception goes
    /// uncaught
    /// (v8::Isolate::SetCaptureStackTraceForUncaughtExceptions).
    ///
    /// Accepted and carried no further: the trace has nowhere to go here. The
    /// crate we stand in for hands it to the listener registered by
    /// `add_message_listener`, which this bridge has no producer for, so a host
    /// sees the same thing whether it asks for traces or not.
    pub fn set_capture_stack_trace_for_uncaught_exceptions(
        &mut self,
        capture: bool,
        frame_limit: i32,
    ) {
        let _ = (capture, frame_limit);
    }

    /// Send promise rejections to `callback`
    /// (v8::Isolate::SetPromiseRejectCallback).
    ///
    /// This is the one isolate-level callback the engine fires, because it has
    /// the event: `HostPromiseRejectionTracker` reports a promise rejected with
    /// no handler, and a handler attached to a rejection it has already
    /// reported. Setting one claims the isolate's host-hook seam for it — the
    /// seam is one trait for every host-defined operation, so this bridge owns
    /// it for the isolate and an isolate whose host never calls this keeps the
    /// defaults.
    ///
    /// Two things the callback sees differently from V8's:
    ///
    /// - The engine reports a rejection as it happens, where V8 waits for the
    ///   microtask checkpoint. A host that tracks unhandled rejections still
    ///   sees each one, and still sees the matching "handler arrived" event.
    /// - The rejection value is present only when the engine had it in hand;
    ///   V8 always has it.
    pub fn set_promise_reject_callback(&mut self, callback: PromiseRejectCallback) {
        self.inner_mut().promise_reject = Some(callback);
        self.install_hooks();
    }

    /// Embedder injection point for `WebAssembly.compileStreaming(source)`
    /// (v8::Isolate::SetWasmStreamingCallback).
    ///
    /// The callback receives the value the source argument resolved to and the
    /// stream to feed; the stream outlives the call and is what settles the
    /// promise `compileStreaming` answered. See `crate::wasm`.
    ///
    /// Where the crate we stand in for installs this on the engine directly, the
    /// engine here takes it as a host hook and asks whether one is installed
    /// before it answers `compileStreaming` at all. Installing one is what makes
    /// that method exist (`runtime::host::has_wasm_streaming_callback`), and an
    /// isolate whose host never calls this keeps the engine's default refusal.
    pub fn set_wasm_streaming_callback<F>(&mut self, _: F)
    where
        F: UnitType
            + for<'a, 'b, 'c> Fn(&'c mut PinScope<'a, 'b>, Local<'a, Value>, WasmStreaming<false>),
    {
        self.inner_mut().wasm_streaming = Some(host_streaming_callback::<F>);
        self.install_hooks();
    }

    /// Ask the host whether a wasm module may be compiled in a context
    /// (`v8::Isolate::SetAllowWasmCodeGenerationCallback`).
    ///
    /// The callback is handed the context the compile runs in and a source
    /// string, and answers whether to allow it; a host that installs none
    /// permits every compile, which is this engine's own default. Where the
    /// crate we stand in for names a `source`, this engine's compiles are byte
    /// compiles and it keeps no text source for one, so what a callback here is
    /// handed is the empty string — informational either way, and the one host
    /// that reads it (`deno/ext/node/ops/vm.rs`) ignores it.
    pub fn set_allow_wasm_code_generation_callback(
        &mut self,
        callback: WasmCodeGenerationCallback,
    ) {
        self.inner_mut().allow_wasm_code_generation = Some(callback);
        self.install_hooks();
    }

    /// The callback an error's `stack` property runs
    /// (v8::Isolate::SetPrepareStackTraceCallback).
    ///
    /// Run: the engine asks this bridge's `HostHooks` when an error's `stack` is
    /// read, hands it the error and the frames its trace was captured from, and
    /// the bridge builds the call-site objects this callback takes and stores its
    /// answer as the value `.stack` reads. A host that installs none reads the
    /// engine's own rendering, which is what V8 gives one that installs none as
    /// well.
    pub fn set_prepare_stack_trace_callback<F>(&mut self, _: F)
    where
        F: UnitType
            + for<'x, 'y, 'z> Fn(
                &'z mut PinScope<'x, 'y>,
                Local<'x, Value>,
                Local<'x, Array>,
            ) -> Local<'x, Value>,
    {
        self.inner_mut().prepare_stack_trace = Some(host_prepare_stack_trace::<F>);
        self.install_hooks();
    }

    /// The callback a module's first read of `import.meta` runs
    /// (v8::Isolate::SetHostInitializeImportMetaObjectCallback).
    ///
    /// Run, unlike the two dynamic-import setters below: the engine makes the
    /// `import.meta` object and asks this bridge's `HostHooks` to fill it the
    /// first time a module reads it, which is the moment V8 calls this callback.
    /// A host that installs none gets the empty object the engine made, which is
    /// what V8 hands out to one that installs none as well.
    pub fn set_host_initialize_import_meta_object_callback(
        &mut self,
        callback: HostInitializeImportMetaObjectCallback,
    ) {
        self.inner_mut().import_meta = Some(callback);
        self.install_hooks();
    }

    /// The callback a dynamic `import()` runs
    /// (v8::Isolate::SetHostImportModuleDynamicallyCallback).
    ///
    /// Run: the engine asks this bridge's `HostHooks` when a module or a script
    /// evaluates `import(...)`, and the bridge hands the host the four things
    /// this crate's callback takes — the referrer's name (an empty string for
    /// code written with no origin name), the specifier, the import attributes as
    /// key/value pairs, and a host-defined options `PrimitiveArray` (empty, this
    /// engine keeping none) — and returns the promise the host answers with. A
    /// host that installs no callback leaves the engine's own registry resolution
    /// in place, which is keyed by specifier text and so cannot rewrite a
    /// relative specifier against its referrer.
    pub fn set_host_import_module_dynamically_callback<F>(&mut self, _: F)
    where
        F: HostImportModuleDynamicallyCallback,
    {
        self.inner_mut().import_dynamically = Some(host_import_module_dynamically::<F>);
        self.install_hooks();
    }

    /// The same for `import source` and `import defer`
    /// (v8::Isolate::SetHostImportModuleWithPhaseDynamicallyCallback).
    ///
    /// When a host installs both, this is the one that runs, which is the
    /// precedence the crate has.
    pub fn set_host_import_module_with_phase_dynamically_callback<F>(&mut self, _: F)
    where
        F: HostImportModuleWithPhaseDynamicallyCallback,
    {
        self.inner_mut().import_dynamically_with_phase =
            Some(host_import_module_with_phase_dynamically::<F>);
        self.install_hooks();
    }

    /// The callback an asynchronous WebAssembly compilation would settle
    /// (v8::Isolate::SetWasmAsyncResolvePromiseCallback).
    ///
    /// Accepted and not run: `WebAssembly.compile` is synchronous here, so
    /// there is no asynchronous compilation whose promise would need resolving —
    /// and no `WasmStreaming` type either, which is the same gap seen from the
    /// other side.
    pub fn set_wasm_async_resolve_promise_callback(
        &mut self,
        callback: WasmAsyncResolvePromiseCallback,
    ) {
        let _ = callback;
    }

    /// The callback a heap close to its limit runs
    /// (v8::Isolate::AddNearHeapLimitCallback).
    ///
    /// The engine fires it when the heap's live-box footprint reaches the limit
    /// `CreateParams::heap_limits` set, and continues with the limit the
    /// callback answers — so a host that terminates the execution inside it
    /// stops the run, and one that raises the limit keeps it going. The `data`
    /// pointer is the host's own, kept as it was registered and not owned here:
    /// V8's contract is that the host keeps it alive until it removes the
    /// callback. The callback gets no isolate, here as there, so it cannot
    /// re-enter the engine.
    pub fn add_near_heap_limit_callback(
        &mut self,
        callback: NearHeapLimitCallback,
        data: *mut c_void,
    ) {
        // Kept as a `usize` so the boxed closure carries nothing but plain data
        // (the pointer is the host's; see the contract above).
        let data = data as usize;
        let boxed: api::NearHeapLimitCallback = Box::new(move |current, initial| {
            // SAFETY: the host's contract for `AddNearHeapLimitCallback`: the
            // callback is live for as long as it is registered and `data` is the
            // pointer it was registered with.
            unsafe { callback(data as *mut c_void, current, initial) }
        });
        self.inner_mut()
            .engine
            .set_near_heap_limit_callback(Some(boxed));
    }

    /// Withdraw a heap-limit callback and restore the limit
    /// (v8::Isolate::RemoveNearHeapLimitCallback).
    ///
    /// The limit is only replaced when the host names one above zero, which is
    /// what `deno_core` passes when it swaps callbacks: V8's own 0 keeps the
    /// limit in force. The callback argument is not compared against the one
    /// registered, because the engine keeps one slot — V8 keeps one callback
    /// too, so a host that passes a different function would be asking to remove
    /// something that was never installed.
    pub fn remove_near_heap_limit_callback(
        &mut self,
        _callback: NearHeapLimitCallback,
        heap_limit: usize,
    ) {
        self.inner_mut()
            .engine
            .remove_near_heap_limit_callback(heap_limit);
    }

    /// Tell the engine that the isolate is about to wait for work
    /// (v8::Isolate::SetIdle).
    ///
    /// Accepted and not acted on: this is the CPU profiler's attribution hint,
    /// and the engine has no profiler for it to reach — see
    /// [`inspector`](crate::inspector) for what exists in that area and what
    /// does not. A host's event loop calls it on every turn, and calling it here
    /// costs that branch and nothing else.
    pub fn set_idle(&mut self, is_idle: bool) {
        let _ = is_idle;
    }

    /// The inner state, for the bridge's own modules — the GC observer reads the
    /// host's collection callbacks from it.
    pub(crate) fn inner(&self) -> &IsolateInner {
        // SAFETY: the handle only exists for a live inner, which the
        // `OwnedIsolate` that made it keeps alive.
        unsafe { self.0.as_ref() }
    }

    fn inner_mut(&mut self) -> &mut IsolateInner {
        // SAFETY: as `inner`; `&mut self` is exclusive over this handle.
        unsafe { self.0.as_mut() }
    }

    /// Make the engine's host-hook seam this bridge's.
    ///
    /// One `HostHooks` implementation serves every host-defined operation, so
    /// both callbacks a host can install go through here, and neither may
    /// replace the other: the hooks struct is stateless, reading each callback
    /// off the isolate that owns it, so replacing the seam is what installing
    /// the second of them looks like. This isolate is one the bridge made, and
    /// the bridge is the only thing that sets its hooks.
    fn install_hooks(&mut self) {
        self.engine_mut().agent().host_hooks = Some(Box::new(BridgeHooks));
    }

    fn slots_mut(&mut self) -> &mut HashMap<TypeId, Box<dyn Any>> {
        // SAFETY: as `get_slot` — the caller's contract covers a live borrow.
        unsafe { &mut *self.inner_mut().slots.get() }
    }
}

/// The isolate's implementation of the engine's host-hook seam: the events this
/// bridge is the producer for.
///
/// The seam is a single trait for every host-defined operation the engine has,
/// so this type is the bridge's whole implementation of it and a later hook the
/// engine grows joins it here. The methods not written below keep the engine's
/// defaults, which is exactly what an isolate with no hooks at all gets.
///
/// It carries no state: each callback a host installs is stored on the isolate it
/// was installed on, and this reads it back by finding that isolate at call time
/// (`api::Isolate::get_current`). That is what lets a host install the
/// promise-rejection callback and the streaming callback in either order without
/// one replacing the other — see [`Isolate::install_hooks`].
#[derive(Debug)]
struct BridgeHooks;

impl runtime::HostHooks for BridgeHooks {
    fn promise_rejection_tracker(
        &self,
        promise: &crux::value::Value,
        reason: Option<&crux::value::Value>,
        operation: bool,
    ) -> Result<(), crux::error::JsError> {
        // A rejection is reported from inside an engine operation, so the
        // isolate running it is the one the message belongs to; outside one
        // there is neither an isolate nor a rejection.
        let Some(engine) = api::Isolate::get_current() else {
            return Ok(());
        };
        // SAFETY: the engine isolate is the first field of `IsolateInner` — the
        // assertion that keeps it there is next to the type — so the pointer the
        // engine hands back names the bridge's own isolate.
        let isolate = unsafe { Isolate::from_engine_ptr(engine) };
        let Some(callback) = isolate.inner().promise_reject else {
            return Ok(());
        };
        let event = if operation {
            PromiseRejectEvent::PromiseHandlerAddedAfterReject
        } else {
            PromiseRejectEvent::PromiseRejectWithNoHandler
        };
        let message = PromiseRejectMessage::new(isolate, *promise, event, reason.copied());
        // SAFETY: the host installed this callback to be called with a
        // rejection, and this is that call, on the thread owning the isolate.
        unsafe { (callback)(message) };
        Ok(())
    }

    fn prepare_stack_trace(
        &self,
        error: &crux::value::Value,
        frames: &[api::StackFrame],
    ) -> Result<Option<crux::value::Value>, crux::error::JsError> {
        // SAFETY: as `promise_rejection_tracker` — the engine hands back the
        // isolate whose error is being read, and that address is the bridge's.
        let Some(engine) = api::Isolate::get_current() else {
            return Ok(None);
        };
        let isolate = unsafe { Isolate::from_engine_ptr(engine) };
        let Some(callback) = isolate.inner().prepare_stack_trace else {
            return Ok(None);
        };
        // The realm the error is being read in: the engine calls this from the
        // `stack` accessor, and a realm is in reach for the whole of an
        // isolate's life (one context per isolate, its bootstrap execution
        // context never popped).
        let Some(context) = isolate.current_context() else {
            return Ok(None);
        };
        let context_local = Local::<Context>::from_payload(Payload::Context(context));
        crate::callback_scope!(unsafe scope, context_local);
        let error = Local::<Value>::from_engine(api::Local::from(*error));
        let sites = call_sites(scope, &isolate, frames)?;
        let value = callback(scope, error, sites);
        Ok(Some(value.into_engine().into_value()))
    }

    fn has_wasm_streaming_callback(&self) -> bool {
        // SAFETY: as `promise_rejection_tracker` — the engine hands back the
        // isolate whose agent is running, and that address is the bridge's.
        match api::Isolate::get_current() {
            Some(engine) => {
                let isolate = unsafe { Isolate::from_engine_ptr(engine) };
                isolate.inner().wasm_streaming.is_some()
            }
            None => false,
        }
    }

    fn allow_wasm_code_generation(&self, context: &api::Context) -> bool {
        // SAFETY: as `wasm_streaming` — the engine hands back the isolate whose
        // wasm compile is running, and that address is the bridge's.
        let Some(engine) = api::Isolate::get_current() else {
            return true;
        };
        let isolate = unsafe { Isolate::from_engine_ptr(engine) };
        let Some(callback) = isolate.inner().allow_wasm_code_generation else {
            // No host policy: every compile is allowed, which is the engine's
            // own default.
            return true;
        };
        let context_local = Local::<Context>::from_payload(Payload::Context(*context));
        crate::callback_scope!(unsafe _scope, context_local);
        // The source argument: this engine compiles bytes and keeps no text
        // source for one, so the callback is handed the empty string. The crate
        // we stand in for's own argument is informational, and the one host that
        // installs this ignores it.
        let source = Local::<crate::data::String>::from_engine(api::Local::string(""));
        callback(context_local, source)
    }

    fn wasm_streaming(
        &self,
        source: &crux::value::Value,
        streaming: &api::WasmStreaming,
    ) -> Result<(), crux::error::JsError> {
        let Some(engine) = api::Isolate::get_current() else {
            return Ok(());
        };
        // SAFETY: as above.
        let isolate = unsafe { Isolate::from_engine_ptr(engine) };
        // The realm the engine is running in — it calls this from a job, and a
        // realm is in reach for the whole of an isolate's life (one context per
        // isolate, its bootstrap execution context never popped).
        let Some(context) = isolate.current_context() else {
            return Err(crux::error::JsError::new(
                crux::ErrorKind::TypeError,
                "a streaming compile needs a realm in reach".into(),
            ));
        };
        let context_local = Local::<Context>::from_payload(Payload::Context(context));
        crate::callback_scope!(unsafe scope, context_local);
        let Some(callback) = isolate.inner().wasm_streaming.as_ref() else {
            return Ok(());
        };
        let source = Local::<Value>::from_engine(api::Local::from(*source));
        callback(scope, source, WasmStreaming(streaming.clone()));
        Ok(())
    }

    fn initialize_import_meta_object(
        &self,
        module: &api::Module,
        meta: &crux::value::Value,
    ) -> Result<(), crux::error::JsError> {
        let Some(engine) = api::Isolate::get_current() else {
            return Ok(());
        };
        // SAFETY: as `wasm_streaming` — the engine hands back the isolate whose
        // agent is running, and that address is the bridge's.
        let isolate = unsafe { Isolate::from_engine_ptr(engine) };
        let Some(callback) = isolate.inner().import_meta else {
            return Ok(());
        };
        // The realm the engine is running in. `import.meta` is only made while a
        // module's own code runs, so a realm is in reach; a module whose realm is
        // somehow gone is a bridge bug rather than a state a host can act on.
        let Some(context) = isolate.current_context() else {
            return Err(crux::error::JsError::new(
                crux::ErrorKind::TypeError,
                "an import.meta object needs a realm in reach".into(),
            ));
        };
        let context_local = Local::<Context>::from_payload(Payload::Context(context));
        // The scope is what enters the context for the call, which is how the
        // crate we stand in for invokes this callback; the callback itself is
        // handed the context handle rather than a scope.
        crate::callback_scope!(unsafe _scope, context_local);
        let meta = Local::<Value>::from_engine(api::Local::from(*meta));
        let Ok(meta) = Local::<Object>::try_from(meta) else {
            // The engine makes this object itself, so anything else is a bridge
            // bug and saying so beats handing the host something it cannot use.
            return Err(crux::error::JsError::new(
                crux::ErrorKind::TypeError,
                "the import.meta the engine made is not an object".into(),
            ));
        };
        // SAFETY: the host installed this callback for exactly this call, and
        // the crate we stand in for declares it `unsafe extern "C"` because it
        // receives handles rather than because it may do anything the host's own
        // code could not.
        unsafe { callback(context_local, Local::from_module(*module), meta) };
        Ok(())
    }

    fn import_module_dynamically(
        &self,
        specifier: &crux::string::JsString,
        referrer_name: Option<&crux::string::JsString>,
        phase: crate::ModuleImportPhase,
        attributes: &[(crux::string::JsString, crux::string::JsString)],
    ) -> Option<Result<crux::value::Value, crux::error::JsError>> {
        let engine = api::Isolate::get_current()?;
        // SAFETY: as `wasm_streaming` — the engine hands back the isolate whose
        // agent is running, and that address is the bridge's.
        let isolate = unsafe { Isolate::from_engine_ptr(engine) };
        let plain = isolate.inner().import_dynamically;
        let with_phase = isolate.inner().import_dynamically_with_phase;
        if plain.is_none() && with_phase.is_none() {
            // No callback: `None` leaves the engine's own registry resolution in
            // place, which is what a host that installs none expects.
            return None;
        }
        let context = isolate.current_context()?;
        let context_local = Local::<Context>::from_payload(Payload::Context(context));
        crate::callback_scope!(unsafe scope, context_local);
        // The four arguments this crate's callback takes. The options array is
        // always a `PrimitiveArray` — empty, because this engine keeps no
        // host-defined options — which is what the host's unchecked cast to one
        // relies on.
        let options: Local<'_, Data> = crate::data::PrimitiveArray::new(scope, 0).into();
        // An empty name is what the crate sends for code written with no origin
        // name, and what the host's "(no referrer)" branch is written to read.
        let referrer = referrer_name
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        let resource_name: Local<'_, Value> =
            Local::from(crate::data::String::new(scope, &referrer)?);
        let specifier: Local<'_, crate::data::String> =
            crate::data::String::new(scope, &specifier.to_string_lossy())?;
        // Key/value pairs, which is the shape a *dynamic* import's attributes
        // arrive in — a static request's carry a source offset as well.
        let realm = crate::realm_of(scope);
        let mut elements = Vec::with_capacity(attributes.len() * 2);
        for (key, value) in attributes {
            elements.push(api::Local::string(key.to_string_lossy()));
            elements.push(api::Local::string(value.to_string_lossy()));
        }
        let attributes =
            Local::<FixedArray>::from_engine(api::Array::new(&realm, &elements).ok()?).retag();
        let promise = match (plain, with_phase) {
            (_, Some(callback)) => {
                callback(scope, options, resource_name, specifier, phase, attributes)
            }
            (Some(callback), None) => {
                callback(scope, options, resource_name, specifier, attributes)
            }
            (None, None) => return None,
        };
        let promise = promise?;
        Some(Ok(Local::<Value>::from(promise).into_engine().into_value()))
    }
}

/// The methods a call site answers, each reading the frame data stored under the
/// name it is paired with (v8::CallSite).
///
/// The method names are V8's, including the `isToplevel` spelling its API has.
/// `getScriptNameOrSourceURL` answers the file name, which is what V8 answers
/// for code that was named rather than carrying a `//# sourceURL`.
const CALL_SITE_METHODS: [(&str, &str); 15] = [
    ("getTypeName", "typeName"),
    ("getFunctionName", "functionName"),
    ("getMethodName", "methodName"),
    ("getFileName", "fileName"),
    ("getLineNumber", "lineNumber"),
    ("getColumnNumber", "columnNumber"),
    ("getEvalOrigin", "evalOrigin"),
    ("isToplevel", "isToplevel"),
    ("isEval", "isEval"),
    ("isNative", "isNative"),
    ("isConstructor", "isConstructor"),
    ("isAsync", "isAsync"),
    ("isPromiseAll", "isPromiseAll"),
    ("getPromiseIndex", "promiseIndex"),
    ("getScriptNameOrSourceURL", "fileName"),
];

/// The error a bridge-internal build step reports. The engine's own steps
/// refuse these only when something is deeply wrong, and a host being handed a
/// stack is better served by the failure than by an empty one.
fn bridge_failure(what: &str) -> crux::error::JsError {
    crux::error::JsError::new(
        crux::ErrorKind::Error,
        format!("bridge: building {what} failed"),
    )
}

/// The call-site objects a host's formatter is handed, one per frame and in the
/// order the frames came in (v8::CallSite).
///
/// Each is an ordinary object carrying its frame's data in private names, with
/// the methods on a prototype the isolate keeps: a formatter reads a field by
/// *calling* a method on the object (`getFileName`, `isEval`, ...), which is
/// what a JS object with methods is for and what no amount of plain data would
/// let it do.
fn call_sites<'s>(
    scope: &mut PinScope<'s, '_>,
    isolate: &Isolate,
    frames: &[api::StackFrame],
) -> Result<Local<'s, Array>, crux::error::JsError> {
    let prototype = call_site_prototype(scope, isolate)?;
    let mut sites: Vec<Local<'s, Value>> = Vec::with_capacity(frames.len());
    for frame in frames {
        let site = Object::with_prototype_and_properties(scope, prototype.into(), &[], &[]);
        set_call_site_fields(scope, site, frame)?;
        sites.push(site.into());
    }
    Ok(Array::new_with_elements(scope, &sites))
}

/// The prototype every call site of this isolate shares: one function per
/// method, each answering the frame data that call site holds. Built on the
/// first read of an error's `stack` and kept on the isolate, so a host that
/// reads many stacks builds the methods once.
fn call_site_prototype<'s>(
    scope: &mut PinScope<'s, '_>,
    isolate: &Isolate,
) -> Result<Local<'s, Object>, crux::error::JsError> {
    if let Some(prototype) = &*isolate.inner().callsite_prototype.borrow() {
        return Ok(Local::new(scope, prototype));
    }
    let prototype = Object::new(scope);
    for (method, key) in CALL_SITE_METHODS {
        let key = crate::data::String::new(scope, key)
            .ok_or_else(|| bridge_failure("a call site's data key"))?;
        let function = Function::builder(read_call_site_field)
            .data(key.into())
            .build(scope)
            .ok_or_else(|| bridge_failure("a call-site method"))?;
        let name = crate::data::String::new(scope, method)
            .ok_or_else(|| bridge_failure("a call-site method's name"))?;
        if prototype.set(scope, name.into(), function.into()) != Some(true) {
            return Err(bridge_failure("a call-site method's property"));
        }
    }
    *isolate.inner().callsite_prototype.borrow_mut() = Some(Global::new(scope, prototype));
    Ok(prototype)
}

/// One call-site method: the frame data stored on its call site under the key
/// the builder was given as its data.
fn read_call_site_field<'s, 'i>(
    scope: &mut PinScope<'s, 'i>,
    args: FunctionCallbackArguments<'s>,
    rv: ReturnValue<'_>,
) {
    let Some(key) = Local::<crate::data::String>::try_from(args.data()).ok() else {
        return;
    };
    let private = Private::for_api(scope, Some(key));
    let this = args.this();
    if let Some(value) = this.get_private(scope, private) {
        rv.set(value);
    }
}

/// Fill one call site with its frame's data, under the private names the
/// prototype's methods read. A frame answers *undefined* for what is not
/// recorded (a script's file name among them) and a boolean for every flag,
/// because a formatter's reads are typed: an absent boolean would be a
/// rejection rather than a `false`.
fn set_call_site_fields<'s>(
    scope: &mut PinScope<'s, '_>,
    site: Local<'s, Object>,
    frame: &api::StackFrame,
) -> Result<(), crux::error::JsError> {
    let unnamed = frame.function_name.is_none();
    let file = match &frame.script_name {
        Some(name) => call_site_text(scope, name)?,
        None => crate::undefined(scope).into(),
    };
    set_call_site_field(scope, site, "fileName", file)?;
    let function = match &frame.function_name {
        Some(name) => call_site_text(scope, name)?,
        None => crate::undefined(scope).into(),
    };
    set_call_site_field(scope, site, "functionName", function)?;
    set_call_site_field(scope, site, "typeName", crate::undefined(scope).into())?;
    set_call_site_field(scope, site, "methodName", crate::undefined(scope).into())?;
    set_call_site_field(scope, site, "evalOrigin", crate::undefined(scope).into())?;
    let line = call_site_position(scope, frame.line)?;
    set_call_site_field(scope, site, "lineNumber", line)?;
    let column = call_site_position(scope, frame.column)?;
    set_call_site_field(scope, site, "columnNumber", column)?;
    // A frame with no function of its own is at the top level of the code it
    // runs — a module's body among them — which is the reading V8's `isToplevel`
    // has and the one its formatting turns on.
    let top_level = crate::data::Boolean::new(scope, unnamed);
    set_call_site_field(scope, site, "isToplevel", top_level.into())?;
    let is_eval = crate::data::Boolean::new(scope, frame.is_eval);
    set_call_site_field(scope, site, "isEval", is_eval.into())?;
    let is_constructor = crate::data::Boolean::new(scope, frame.is_constructor);
    set_call_site_field(scope, site, "isConstructor", is_constructor.into())?;
    // Nothing in an execution context records these two, so a frame is never
    // native and never a `Promise.all` entry; V8 answers the same for code that
    // is none of them. `isAsync` is the frame's own flag, which the engine sets
    // only on the awaiting frames it appends for a suspended async body.
    let is_native = crate::data::Boolean::new(scope, false);
    set_call_site_field(scope, site, "isNative", is_native.into())?;
    let is_async = crate::data::Boolean::new(scope, frame.is_async);
    set_call_site_field(scope, site, "isAsync", is_async.into())?;
    let is_promise_all = crate::data::Boolean::new(scope, false);
    set_call_site_field(scope, site, "isPromiseAll", is_promise_all.into())?;
    set_call_site_field(scope, site, "promiseIndex", crate::undefined(scope).into())?;
    Ok(())
}

/// A call site's text, as a string a formatter reads.
fn call_site_text<'s>(
    scope: &mut PinScope<'s, '_>,
    value: &str,
) -> Result<Local<'s, Value>, crux::error::JsError> {
    crate::data::String::new(scope, value)
        .map(Local::<Value>::from)
        .ok_or_else(|| bridge_failure("a call site's text"))
}

/// A frame's line or column: the number, or *undefined* for "no information" —
/// which is what V8 answers for a frame with no position.
fn call_site_position<'s>(
    scope: &mut PinScope<'s, '_>,
    value: usize,
) -> Result<Local<'s, Value>, crux::error::JsError> {
    if value == 0 {
        return Ok(crate::undefined(scope).into());
    }
    Ok(crate::data::Number::new(scope, value as f64).into())
}

/// Store one field of a call site under its private name.
fn set_call_site_field<'s>(
    scope: &mut PinScope<'s, '_>,
    site: Local<'s, Object>,
    key: &str,
    value: Local<'s, Value>,
) -> Result<(), crux::error::JsError> {
    let name = crate::data::String::new(scope, key)
        .ok_or_else(|| bridge_failure("a call site's data key"))?;
    let private = Private::for_api(scope, Some(name));
    if site.set_private(scope, private, value) != Some(true) {
        return Err(bridge_failure("a call site's data"));
    }
    Ok(())
}

/// An owned isolate (`v8::OwnedIsolate`), as returned by [`Isolate::new`].
///
/// The state lives in the box; the handle beside it is what `Deref` hands out,
/// so a host never holds the state itself.
pub struct OwnedIsolate {
    /// Held for its `Drop`: releasing it drops the engine isolate and the slot
    /// map with it.
    #[allow(dead_code)]
    inner: Box<IsolateInner>,
    handle: Isolate,
}

impl Deref for OwnedIsolate {
    type Target = Isolate;

    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

impl DerefMut for OwnedIsolate {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.handle
    }
}

impl AsMut<Isolate> for OwnedIsolate {
    fn as_mut(&mut self) -> &mut Isolate {
        self
    }
}

impl AsMut<Isolate> for Isolate {
    fn as_mut(&mut self) -> &mut Isolate {
        self
    }
}

impl OwnedIsolate {
    /// Create the snapshot blob (v8::OwnedIsolate::CreateBlob).
    ///
    /// # Panics
    ///
    /// When the isolate did not come from
    /// [`Isolate::snapshot_creator`](Isolate::snapshot_creator), as the crate we
    /// stand in for does; when the creator took no context, since a blob
    /// carries what a context holds; and when a value in the graph is one the
    /// format cannot carry yet — see [`SnapshotCreator`](crate::SnapshotCreator),
    /// whose message names the value. The crate's `Option` is unwrapped by its
    /// own callers, so the loudest available message is the honest one: a blob
    /// that quietly lost part of a host's state would move the failure to where
    /// the host cannot see it.
    pub fn create_blob(self, function_code_handling: FunctionCodeHandling) -> Option<StartupData> {
        let mut handle = self.handle;
        // The callbacks the host built functions from, read before the creator is
        // taken: the blob records each one as an index into the external-reference
        // table, so the load can put the call back.
        let callbacks = handle.take_callbacks();
        let mut creator = handle
            .inner_mut()
            .snapshot_creator
            .take()
            .expect("v8::OwnedIsolate::create_blob: this isolate was not created by Isolate::snapshot_creator");
        Some(creator.create_blob(function_code_handling, callbacks))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::promise::PromiseState;

    /// The generation and code-range sizes a host sets are *reported back*, and
    /// nothing else: a generation is a part of a V8 heap, and this engine's arena
    /// is not one. What a host that reads them must not get is a number V8 would
    /// have had — that would be invented here — so the answer is what the host
    /// set, `0` when it set none, and a derivation from the system memory the
    /// host hands in when it asks for one.
    #[test]
    fn heap_size_settings_are_reported_back() {
        let params = CreateParams::default();
        assert_eq!(params.max_old_generation_size_in_bytes(), 0);
        assert_eq!(params.max_young_generation_size_in_bytes(), 0);
        assert_eq!(params.code_range_size_in_bytes(), 0);

        let params = params
            .set_max_old_generation_size_in_bytes(1024 * 1024 * 512)
            .set_max_young_generation_size_in_bytes(1024 * 1024 * 8)
            .set_code_range_size_in_bytes(1024 * 1024 * 256);
        assert_eq!(params.max_old_generation_size_in_bytes(), 1024 * 1024 * 512);
        assert_eq!(params.max_young_generation_size_in_bytes(), 1024 * 1024 * 8);
        assert_eq!(params.code_range_size_in_bytes(), 1024 * 1024 * 256);

        let derived = CreateParams::default().heap_limits_from_system_memory(16 << 30, 0);
        assert_eq!(derived.max_old_generation_size_in_bytes(), 8 << 30);
        assert_eq!(derived.max_young_generation_size_in_bytes(), 1 << 30);
        assert_eq!(derived.code_range_size_in_bytes(), 0);
    }

    /// A host that kept the raw pointer can borrow the handle back out of its own
    /// storage, which is the shape `ext/napi`'s `Env` uses to reach the isolate its
    /// callbacks run on.
    #[test]
    fn a_host_borrows_its_stored_raw_isolate_pointer_as_the_handle() {
        let isolate = &mut Isolate::new(CreateParams::default());
        // SAFETY: the isolate is alive for the borrow below.
        let mut raw = unsafe { isolate.as_raw_isolate_ptr() };
        // SAFETY: `raw` names that live isolate and outlives the borrow.
        let borrowed = unsafe { Isolate::ref_from_raw_isolate_ptr_mut_unchecked(&mut raw) };
        // The reinterpreted handle names the same isolate, not a copy of an
        // address: its own raw pointer round-trips, and a scope over it enters a
        // context a script runs in.
        // SAFETY: `borrowed` names the live isolate.
        assert_eq!(unsafe { borrowed.as_raw_isolate_ptr() }, raw);
        crate::scope!(let scope, borrowed);
        let context = crate::Context::new(scope, Default::default());
        let scope = &mut crate::ContextScope::new(scope, context);
        assert_eq!(crate::test_support::eval_number(scope, "6 * 7"), 42.0);
    }

    /// A termination request crosses threads and is observed at a check point,
    /// and the isolate and its handle see the same one.
    #[test]
    fn termination_requests_reach_the_isolate_from_either_side() {
        let isolate = &mut Isolate::new(CreateParams::default());
        assert!(!isolate.is_execution_terminating());

        assert!(isolate.terminate_execution());
        assert!(isolate.is_execution_terminating());

        let handle: IsolateHandle = isolate.thread_safe_handle();
        assert!(handle.is_execution_terminating());
        assert!(handle.cancel_terminate_execution());
        assert!(!isolate.is_execution_terminating());
    }

    /// The heap limits a host sets are recorded and readable — and, as the field
    /// says, they bound nothing: an isolate made from params carrying them is an
    /// ordinary isolate.
    #[test]
    fn heap_limits_are_recorded_and_readable() {
        assert_eq!(Isolate::create_params().get_heap_limits(), None);
        let params = Isolate::create_params().heap_limits(0, 5 * 1024 * 1024);
        assert_eq!(params.get_heap_limits(), Some((0, 5 * 1024 * 1024)));
        let isolate = &mut Isolate::new(params);
        assert!(!isolate.is_execution_terminating());
    }

    /// A microtask added from Rust runs when the queues drain, in the realm the
    /// isolate entered, and it does not run before then.
    #[test]
    fn a_queued_microtask_runs_on_the_next_checkpoint() {
        crate::test_support::in_context!(scope, {
            crate::test_support::eval(
                scope,
                "globalThis.ran = 0; globalThis.microtask = function () { globalThis.ran = 7; };",
            );
            let microtask: Local<'_, Function> =
                crate::test_support::eval(scope, "globalThis.microtask").cast();
            let mut isolate = scope.isolate_ptr();
            isolate.enqueue_microtask(microtask);
            assert_eq!(
                crate::test_support::eval_number(scope, "globalThis.ran"),
                0.0,
                "the microtask does not run before the queues drain"
            );
            isolate.perform_microtask_checkpoint();
            assert_eq!(
                crate::test_support::eval_number(scope, "globalThis.ran"),
                7.0,
                "the enqueued callback ran when the queues drained"
            );
        });
    }

    /// An interrupt is not scheduled, and the callback is not run: there is no
    /// safepoint to run it at.
    #[test]
    fn an_interrupt_request_does_not_call_its_callback() {
        static CALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

        unsafe extern "C" fn on_interrupt(
            _isolate: UnsafeRawIsolatePtr,
            _data: *mut std::ffi::c_void,
        ) {
            CALLED.store(true, std::sync::atomic::Ordering::SeqCst);
        }

        let isolate = &mut Isolate::new(CreateParams::default());
        let handle = isolate.thread_safe_handle();
        assert!(!handle.request_interrupt(on_interrupt, std::ptr::null_mut()));
        assert!(!CALLED.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// A host with a stack formatter is handed an array of call sites, and what
    /// it answers is what `.stack` reads. The formatter here is deno's shape:
    /// it reads each frame by *calling* a method on its call site rather than by
    /// reading a property.
    #[test]
    fn a_host_formatter_is_handed_call_sites() {
        fn formatter<'s, 'i>(
            scope: &mut PinScope<'s, 'i>,
            _error: Local<'s, Value>,
            sites: Local<'s, Array>,
        ) -> Local<'s, Value> {
            // `globalThis.readSites` is installed by the test below: a formatter
            // that reaches into its realm is what a host's own formatter does.
            let global = scope.get_current_context().global(scope);
            let key = crate::data::String::new(scope, "readSites").expect("a string");
            let read = global
                .get(scope, key.into())
                .and_then(|value| Local::<Function>::try_from(value).ok());
            let Some(read) = read else {
                return crate::undefined(scope).into();
            };
            read.call(scope, global.into(), &[sites.into()])
                .unwrap_or_else(|| crate::undefined(scope).into())
        }

        crate::test_support::in_context!(scope, {
            scope.set_prepare_stack_trace_callback(formatter);
            let _ = crate::test_support::eval(
                scope,
                "globalThis.readSites = (sites) => sites.map((s) => \
                 [s.getFunctionName(), s.getFileName(), s.isToplevel(), s.isEval(), \
                 s.getLineNumber(), s.isAsync()].map(String).join('|')).join(';');",
            );
            let stack = crate::test_support::eval(
                scope,
                "function inner() { throw new Error('boom'); }\n\
                 try { inner(); } catch (e) { e.stack; }",
            );
            let text = stack.to_rust_string_lossy(scope);
            assert!(
                text.starts_with("inner|undefined|false|false|1|false;"),
                "the innermost frame answers each method for itself: {text}"
            );
            assert!(
                text.ends_with("undefined|undefined|true|false|2|false"),
                "and the script's own top level is the outermost frame, unnamed \
                 and top level: {text}"
            );
            // The awaiting frame the engine appends for a suspended async body is
            // the one frame marked async, which is what a formatter spells
            // `at async <file>` from.
            let _ = crate::test_support::eval(
                scope,
                "globalThis.asyncStack = '';\n\
                 async function body() {\n\
                 await Promise.resolve(1).then(() => { globalThis.asyncStack = new Error('boom').stack; });\n\
                 }\n\
                 body();",
            );
            scope.perform_microtask_checkpoint();
            let sites = crate::test_support::eval(scope, "globalThis.asyncStack");
            let text = sites.to_rust_string_lossy(scope);
            assert!(
                text.ends_with("body|undefined|false|false|3|true"),
                "the awaiting body is the marked frame: {text}"
            );
        });
    }

    /// The one host callback of the isolate-level set that the engine fires: a
    /// promise rejected with no handler reaches it, and so does a handler
    /// attached to that rejection afterwards.
    #[test]
    fn a_rejected_promise_reaches_the_host_callback() {
        thread_local! {
            static EVENTS: std::cell::RefCell<Vec<(PromiseRejectEvent, bool)>> =
                const { std::cell::RefCell::new(Vec::new()) };
        }

        #[allow(improper_ctypes_definitions)] // as the callback type says.
        unsafe extern "C" fn record(message: PromiseRejectMessage) {
            EVENTS.with(|events| {
                events
                    .borrow_mut()
                    .push((message.get_event(), message.get_value().is_some()));
            });
        }

        crate::test_support::in_context!(scope, {
            scope.set_promise_reject_callback(record);
            crate::test_support::eval(
                scope,
                "const rejected = Promise.reject(new Error('boom'));\n\
                 rejected.catch(function () {});",
            );
        });

        EVENTS.with(|events| {
            assert_eq!(
                events.borrow().as_slice(),
                &[
                    (PromiseRejectEvent::PromiseRejectWithNoHandler, true),
                    (PromiseRejectEvent::PromiseHandlerAddedAfterReject, true),
                ],
                "the rejection and the late handler are both reported, in order"
            );
        });
    }

    /// The other isolate-level callback a host installs
    /// (`v8::Isolate::SetAllowWasmCodeGenerationCallback`): the engine asks it
    /// before a wasm compile, and a host that says no turns the compile into
    /// V8's CompileError — while one that installs no callback at all leaves
    /// every compile working.
    #[test]
    fn a_host_can_refuse_wasm_code_generation() {
        thread_local! {
            static ASKED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
            static ANSWER: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
        }

        #[allow(improper_ctypes_definitions)] // as the callback type says.
        extern "C" fn policy(
            _context: Local<crate::data::Context>,
            _source: Local<crate::data::String>,
        ) -> bool {
            ASKED.with(|asked| asked.set(asked.get() + 1));
            ANSWER.with(|answer| answer.get())
        }

        // The bytes `compile_module_value` answers a Module object for, as the
        // Uint8Array the JS API takes. 1 is "compiled", 2 is "refused with a
        // CompileError", 0 anything else.
        let bytes = crate::test_support::EXPORTS_A_MEMORY
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let source = format!(
            "(() => {{ try {{ new WebAssembly.Module(new Uint8Array([{bytes}])); return 1; }} \
             catch (e) {{ return e.name === 'CompileError' ? 2 : 0; }} }})()"
        );

        let isolate = &mut Isolate::new(CreateParams::default());
        isolate.set_allow_wasm_code_generation_callback(policy);
        crate::scope!(let handle_scope, isolate);
        let context = crate::Context::new(handle_scope, Default::default());
        let scope = &mut crate::ContextScope::new(handle_scope, context);

        // The callback permits, so the module compiles and the host was asked.
        assert_eq!(crate::test_support::eval_number(scope, &source), 1.0);
        assert!(
            ASKED.with(|asked| asked.get()) >= 1,
            "the host was never asked about the compile"
        );

        ANSWER.with(|answer| answer.set(false));
        assert_eq!(
            crate::test_support::eval_number(scope, &source),
            2.0,
            "the refusal is not V8's CompileError"
        );

        ANSWER.with(|answer| answer.set(true));
        assert_eq!(crate::test_support::eval_number(scope, &source), 1.0);
    }

    /// A host callback that streams one module for every source it is handed —
    /// the last step of the fetch loop a browser or Deno runs, reduced to it.
    ///
    /// A fn item rather than a closure because that is the shape a host installs
    /// (`UnitType`), and its lifetimes are spelled as Deno spells its own
    /// callback's: the source handle and the scope's first parameter share one
    /// lifetime, which is what the installed callback type requires of them.
    fn host_streams_a_module<'a>(
        _scope: &mut PinScope<'a, '_>,
        _source: Local<'a, Value>,
        mut streaming: WasmStreaming<false>,
    ) {
        streaming.on_bytes_received(&crate::test_support::EXPORTS_A_MEMORY);
        streaming.set_url("app.wasm");
        streaming.finish();
    }

    /// A host that installs a streaming callback can leave the fetching to its
    /// own code and still get V8's shape: the engine answers the promise at once,
    /// the source resolves on a later turn, and the callback's stream is what
    /// settles the promise — with the module its bytes compiled to.
    #[test]
    fn a_host_streaming_callback_settles_compile_streaming() {
        crate::test_support::in_context!(scope, {
            scope.set_wasm_streaming_callback(host_streams_a_module);
            let promise = Local::<Promise>::try_from(crate::test_support::eval(
                scope,
                "WebAssembly.compileStreaming({ url: 'app.wasm' })",
            ))
            .expect("a promise");
            assert_eq!(
                promise.state(),
                PromiseState::Pending,
                "the source has not resolved yet"
            );

            scope.run_microtasks().expect("microtasks");
            assert_eq!(promise.state(), PromiseState::Fulfilled);

            // The resolution is read through the engine's own accessor: the
            // bridge's cast table only widens a module object to `Object` and
            // `Value`, which is all a host's own code ever asks for.
            let compiled = api::WasmModuleObject::get_compiled_module(
                &crate::realm_current(),
                promise.result(scope).engine(),
            )
            .expect("the resolution is a module object");
            assert_eq!(
                compiled.module().exports[0].name,
                "m",
                "the module the host streamed, not some other one"
            );
        });
    }

    /// The isolate's callbacks share one host-hook implementation, so installing
    /// the second of them must not drop the first, in either direction. Both are
    /// exercised after both installs, in the order a host makes them.
    #[test]
    fn installing_one_isolate_callback_keeps_the_other() {
        thread_local! {
            static REJECTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        }

        #[allow(improper_ctypes_definitions)] // as the callback type says.
        unsafe extern "C" fn record(_message: PromiseRejectMessage) {
            REJECTED.with(|seen| seen.set(true));
        }

        crate::test_support::in_context!(scope, {
            scope.set_promise_reject_callback(record);
            scope.set_wasm_streaming_callback(host_streams_a_module);

            let promise = Local::<Promise>::try_from(crate::test_support::eval(
                scope,
                "WebAssembly.compileStreaming({ url: 'app.wasm' })",
            ))
            .expect("a promise");
            crate::test_support::eval(scope, "Promise.reject(new Error('boom'));");
            scope.run_microtasks().expect("microtasks");
            assert_eq!(
                promise.state(),
                PromiseState::Fulfilled,
                "the streaming callback installed second still runs"
            );
        });

        REJECTED.with(|seen| {
            assert!(
                seen.get(),
                "the rejection callback installed first still runs"
            )
        });
    }

    /// The isolate-level callbacks the engine never fires are still *shaped* like
    /// a host's functions, which is the whole of their contract here: the host's
    /// callback compiles, and nothing beyond that is promised.
    #[test]
    fn the_recorded_setters_accept_a_hosts_callbacks() {
        #[allow(improper_ctypes_definitions)] // as the callback type says.
        extern "C" fn initialize_import_meta(
            _context: Local<Context>,
            _module: Local<crate::data::Module>,
            _meta: Local<crate::data::Object>,
        ) {
        }

        fn dynamic_import<'s, 'i>(
            _scope: &mut PinScope<'s, 'i>,
            _options: Local<'s, Data>,
            _resource_name: Local<'s, Value>,
            _specifier: Local<'s, crate::data::String>,
            _attributes: Local<'s, FixedArray>,
        ) -> Option<Local<'s, Promise>> {
            None
        }

        fn import_with_phase<'s, 'i>(
            _scope: &mut PinScope<'s, 'i>,
            _options: Local<'s, Data>,
            _resource_name: Local<'s, Value>,
            _specifier: Local<'s, crate::data::String>,
            _phase: crate::ModuleImportPhase,
            _attributes: Local<'s, FixedArray>,
        ) -> Option<Local<'s, Promise>> {
            None
        }

        fn prepare_stack_trace<'s, 'i>(
            _scope: &mut PinScope<'s, 'i>,
            error: Local<'s, Value>,
            _sites: Local<'s, Array>,
        ) -> Local<'s, Value> {
            error
        }

        #[allow(improper_ctypes_definitions)] // as the callback type says.
        extern "C" fn async_resolve(
            _isolate: UnsafeRawIsolatePtr,
            _context: Local<Context>,
            _resolver: Local<PromiseResolver>,
            _result: Local<Value>,
            _success: WasmAsyncSuccess,
        ) {
        }

        extern "C" fn heap_limit(
            _data: *mut std::ffi::c_void,
            current_heap_limit: usize,
            _initial_heap_limit: usize,
        ) -> usize {
            current_heap_limit
        }

        let isolate = &mut Isolate::new(CreateParams::default());
        isolate.set_capture_stack_trace_for_uncaught_exceptions(true, 10);
        isolate.set_prepare_stack_trace_callback(prepare_stack_trace);
        isolate.set_host_initialize_import_meta_object_callback(initialize_import_meta);
        isolate.set_host_import_module_dynamically_callback(dynamic_import);
        isolate.set_host_import_module_with_phase_dynamically_callback(import_with_phase);
        isolate.set_wasm_async_resolve_promise_callback(async_resolve);
        isolate.add_near_heap_limit_callback(heap_limit, std::ptr::null_mut());
        isolate.remove_near_heap_limit_callback(heap_limit, 0);
        isolate.set_idle(true);
    }

    /// The promise hooks a scope installs are run by the engine
    /// (`v8::Context::SetPromiseHooks`), with V8's call shape: an init is handed
    /// the promise and its parent, every other kind the promise alone. This is
    /// the shape `deno_core`'s two timer tests use — a hook installed through
    /// the scope and armed the first time it runs — and a host that installs
    /// them keeps running.
    #[test]
    fn a_promise_hook_installed_through_the_scope_runs() {
        crate::test_support::in_context!(scope, {
            crate::test_support::eval(
                scope,
                "globalThis.hooks = [];\n\
                 globalThis.record = (kind) => function () {\n\
                   globalThis.hooks.push(kind + ':' + arguments.length);\n\
                 };",
            );
            let hook = |kind: &str| {
                let value = crate::test_support::eval(scope, &format!("record('{kind}')"));
                Local::<crate::data::Function>::try_from(value).expect("a function")
            };
            scope.set_promise_hooks(
                Some(hook("init")),
                Some(hook("before")),
                Some(hook("after")),
                Some(hook("resolve")),
            );

            crate::test_support::eval(scope, "Promise.resolve(1).then(function () {})");
            // The reaction job runs when the host drains its queue, which is
            // where deno's event loop drains it: before and after are that job's
            // pair.
            scope.perform_microtask_checkpoint();
            let hooks = crate::test_support::eval(scope, "globalThis.hooks.join(',')")
                .to_rust_string_lossy(scope);
            for expected in ["init:2", "resolve:1", "before:1", "after:1"] {
                assert!(
                    hooks.split(',').any(|call| call == expected),
                    "{expected} was not among {hooks}"
                );
            }
            assert_eq!(crate::test_support::eval_number(scope, "1 + 1"), 2.0);
        });
    }

    /// Nothing is ever pending in the background, and that is not the same
    /// statement as "nothing is pending": the job below is pending, and running
    /// it is what this assertion sits next to.
    #[test]
    fn nothing_is_pending_in_the_background() {
        crate::test_support::in_context!(scope, {
            scope.set_microtasks_policy(crate::MicrotasksPolicy::Explicit);
            crate::test_support::eval(
                scope,
                "Promise.resolve().then(function () { globalThis.ran = 7; })",
            );
            assert!(
                !scope.has_pending_background_tasks(),
                "a queued job is drained by a checkpoint, not by another thread"
            );

            scope.perform_microtask_checkpoint();
            assert_eq!(
                crate::test_support::eval_number(scope, "globalThis.ran"),
                7.0,
                "work really was pending, and the checkpoint is what ran it"
            );
            assert!(!scope.has_pending_background_tasks());
        });
    }

    /// A checkpoint runs what is queued, which under the explicit policy is the
    /// only thing that runs it — and the queue is empty afterwards, so a second
    /// checkpoint has nothing to do.
    #[test]
    fn a_checkpoint_runs_the_queued_jobs() {
        crate::test_support::in_context!(scope, {
            scope.set_microtasks_policy(crate::MicrotasksPolicy::Explicit);
            crate::test_support::eval(
                scope,
                "Promise.resolve().then(function () { globalThis.ran = 7; })",
            );
            assert_eq!(
                crate::test_support::eval_number(
                    scope,
                    "globalThis.ran === undefined ? -1 : globalThis.ran"
                ),
                -1.0,
                "nothing runs the queue but the checkpoint"
            );

            scope.perform_microtask_checkpoint();
            assert_eq!(
                crate::test_support::eval_number(scope, "globalThis.ran"),
                7.0
            );
        });
    }

    /// V8's date-time notification is absorbed: the engine fixes local time at
    /// UTC, so there is no cached configuration for a `Redetect` to invalidate
    /// and the clock reads the same before and after.
    #[test]
    fn a_date_time_notification_leaves_the_clock_where_it_is() {
        crate::test_support::in_context!(scope, {
            let before = crate::test_support::eval(scope, "new Date(0).toISOString()")
                .to_rust_string_lossy(scope);
            scope.date_time_configuration_change_notification(crate::TimeZoneDetection::Redetect);
            let after = crate::test_support::eval(scope, "new Date(0).toISOString()")
                .to_rust_string_lossy(scope);
            assert_eq!(before, after, "the notification changes nothing");
            assert_eq!(before, "1970-01-01T00:00:00.000Z");
        });
    }

    /// The host's external-memory account is what the heap statistics report, and
    /// it floors at zero. Both calls go through a scope, which is the shape the
    /// tree's webgpu sites call it in.
    #[test]
    fn the_external_memory_account_is_the_statistic_and_floors_at_zero() {
        crate::test_support::in_context!(scope, {
            assert_eq!(
                scope.adjust_amount_of_external_allocated_memory(1 << 20),
                1 << 20
            );
            assert_eq!(scope.get_heap_statistics().external_memory(), 1 << 20);

            assert_eq!(
                scope.adjust_amount_of_external_allocated_memory(-(2 << 20)),
                0,
                "a change past zero floors there"
            );
            assert_eq!(scope.get_heap_statistics().external_memory(), 0);
        });
    }
}
