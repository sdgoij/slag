//! The isolate: the engine's isolate, plus what the bridge records on it.

use std::any::{Any, TypeId};
use std::cell::{RefCell, UnsafeCell};
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
use crate::data::{Array, Context, Data, FixedArray, Object, Promise, PromiseResolver, Value};
use crate::handle::{Global, Local, Payload};
use crate::position::Position;
use crate::promise::{PromiseRejectEvent, PromiseRejectMessage};
use crate::scope::PinScope;
use crate::snapshot::{FunctionCodeHandling, SnapshotCreator, SnapshotRestore, StartupData};
use crate::support::{MapFnFrom, MapFnTo, UniqueRef, UnitType};
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

    /// The snapshot this host asked for, if any (this bridge's own accessor,
    /// for a host that keeps one `CreateParams` around).
    pub fn snapshot(&self) -> Option<&crate::StartupData> {
        self.snapshot_blob.as_ref()
    }
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
    /// Where the errors the bridge threw came from, by object identity, for the
    /// `v8::Message` a host later makes from one of them; see
    /// [`position`](crate::position) for why only a failed compile records one.
    positions: RefCell<HashMap<u64, Position>>,
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
}

const _: () = assert!(std::mem::offset_of!(IsolateInner, engine) == 0);

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
        // The callback is accepted and never run, so there is nothing to fold a
        // host function into: this is the shape the bound asks for, and calling
        // it is the one thing the bridge does not do.
        fn never<'s, 'a>(
            _scope: &mut PinScope<'s, 'a>,
            _error: Local<'s, Value>,
            _sites: Local<'s, Array>,
        ) -> Local<'s, Value> {
            unreachable!("the bridge does not run the prepare-stack-trace callback")
        }

        never
    }
}

/// A reference to an isolate that another thread may keep
/// (v8::IsolateHandle).
///
/// The crate we stand in for hands out a reference-counted handle that reaches
/// the isolate to terminate it or to run a callback at its next safepoint. Slag's
/// isolate is thread-local and its engine has no termination request to set, so
/// this handle reaches nothing: it exists so a host that keeps one across threads
/// — a debugger's waker, a watchdog — can name the type and pass it around, and
/// every request through it answers that it was not made.
#[derive(Clone, Copy, Debug, Default)]
pub struct IsolateHandle;

impl IsolateHandle {
    /// Ask the isolate to stop executing
    /// (v8::IsolateHandle::TerminateExecution).
    ///
    /// Answers `false`: no request was made, because Slag cannot stop a running
    /// execution. The crate we stand in for answers whether the isolate was
    /// still alive to receive the request; here the answer means "not made",
    /// which is the direction a caller can act on — a host that ignores it
    /// behaves as it does there, and a host that checks it learns to stop the
    /// isolate some other way.
    pub fn terminate_execution(&self) -> bool {
        false
    }

    /// Withdraw a termination request
    /// (v8::IsolateHandle::CancelTerminateExecution).
    ///
    /// Answers `false`, for the same reason as
    /// [`terminate_execution`](Self::terminate_execution): there was no request
    /// to withdraw.
    pub fn cancel_terminate_execution(&self) -> bool {
        false
    }

    /// Whether the isolate is terminating an execution
    /// (v8::IsolateHandle::IsExecutionTerminating).
    ///
    /// `false`: nothing can set it.
    pub fn is_execution_terminating(&self) -> bool {
        false
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
#[derive(Clone, Copy)]
pub struct Isolate(NonNull<IsolateInner>);

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
        let engine = *api::Isolate::new();
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
            resolver_rejects: RefCell::new(HashMap::new()),
            cpp_heap,
            templates: RefCell::new(Vec::new()),
            extras_bindings: RefCell::new(std::collections::HashMap::new()),
            positions: RefCell::new(HashMap::new()),
            object_templates: RefCell::new(Vec::new()),
            snapshot_creator: creator,
            restore: RefCell::new(restore),
            externals,
            promise_reject: None,
            wasm_streaming: None,
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

    /// This isolate's address, for a host that keeps one across calls
    /// (v8::Isolate::as_raw_isolate_ptr).
    ///
    /// # Safety
    ///
    /// The handle must be read on the thread the isolate was created on.
    pub unsafe fn as_raw_isolate_ptr(&self) -> UnsafeRawIsolatePtr {
        UnsafeRawIsolatePtr::from_inner_ptr(self.0.as_ptr())
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
    ///
    /// Slag cannot terminate an execution yet, so nothing can be terminating
    /// one and this answers `false`.
    pub fn is_execution_terminating(&self) -> bool {
        false
    }

    /// A handle another thread may keep
    /// (`v8::Isolate::GetThreadSafeHandle`) — see [`IsolateHandle`] for what it
    /// can and cannot do here.
    pub fn thread_safe_handle(&self) -> IsolateHandle {
        IsolateHandle
    }

    /// Ask this isolate to stop executing
    /// (`v8::Isolate::TerminateExecution`).
    ///
    /// Answers `false`: see [`IsolateHandle::terminate_execution`], which is the
    /// same request from another thread.
    pub fn terminate_execution(&self) -> bool {
        false
    }

    /// Withdraw a termination request
    /// (`v8::Isolate::CancelTerminateExecution`).
    ///
    /// Answers `false`, for the same reason as
    /// [`terminate_execution`](Self::terminate_execution).
    pub fn cancel_terminate_execution(&self) -> bool {
        false
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
            return *held.handle().engine();
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

    /// Whether this isolate minted `symbol` as a private name.
    pub(crate) fn owns_private(&self, symbol: Handle<crux::symbol::Symbol>) -> bool {
        self.inner()
            .private_names
            .borrow()
            .values()
            .any(|held| held.handle().engine().value().as_symbol() == Some(symbol))
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
            .map(|held| *held.handle().engine())
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
            .map(|held| *held.handle().engine())
    }

    /// Remember a context's extras binding object, pinned on this isolate.
    pub(crate) fn set_extras_binding(&self, context: u64, value: Local<'_, Object>) {
        self.inner()
            .extras_bindings
            .borrow_mut()
            .insert(context, Global::new(self, value));
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

    /// The callback an error's `stack` property would run
    /// (v8::Isolate::SetPrepareStackTraceCallback).
    ///
    /// Accepted and not run: the engine builds an error's stack itself, with no
    /// host hook in the path, so script-level `Error.prepareStackTrace` is the
    /// only thing that replaces it here.
    pub fn set_prepare_stack_trace_callback<'s>(
        &mut self,
        callback: impl MapFnTo<PrepareStackTraceCallback<'s>>,
    ) {
        let _ = callback;
    }

    /// The callback a module's first read of `import.meta` would run
    /// (v8::Isolate::SetHostInitializeImportMetaObjectCallback).
    ///
    /// Accepted and not run: the engine builds `import.meta` when it loads a
    /// module, so there is no seam for a host to fill.
    pub fn set_host_initialize_import_meta_object_callback(
        &mut self,
        callback: HostInitializeImportMetaObjectCallback,
    ) {
        let _ = callback;
    }

    /// The callback a dynamic `import()` would run
    /// (v8::Isolate::SetHostImportModuleDynamicallyCallback).
    ///
    /// Accepted and not run, and this one is worth reading twice: the engine
    /// resolves a dynamic import itself, from the modules the host registered
    /// with it, so a host's loader is reached by *registration* rather than by
    /// this callback. The header of this bridge's `module` module is where that
    /// is spelled out.
    pub fn set_host_import_module_dynamically_callback(
        &mut self,
        callback: impl HostImportModuleDynamicallyCallback,
    ) {
        let _ = callback;
    }

    /// The same for `import source` and `import defer`
    /// (v8::Isolate::SetHostImportModuleWithPhaseDynamicallyCallback).
    ///
    /// Accepted and not run, for the reason above.
    pub fn set_host_import_module_with_phase_dynamically_callback(
        &mut self,
        callback: impl HostImportModuleWithPhaseDynamicallyCallback,
    ) {
        let _ = callback;
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

    /// The callback a heap close to its limit would run
    /// (v8::Isolate::AddNearHeapLimitCallback).
    ///
    /// Accepted and not run: nothing runs out of heap here — the engine grows
    /// its heap rather than enforcing a limit — so there is no moment at which
    /// this could fire. The host's pointer is dropped with the rest.
    pub fn add_near_heap_limit_callback(
        &mut self,
        callback: NearHeapLimitCallback,
        data: *mut c_void,
    ) {
        let _ = (callback, data);
    }

    /// Withdraw a heap-limit callback and restore the limit
    /// (v8::Isolate::RemoveNearHeapLimitCallback).
    ///
    /// Accepted and not run, for the reason above: there is no registered
    /// callback to withdraw and no limit to restore.
    pub fn remove_near_heap_limit_callback(
        &mut self,
        callback: NearHeapLimitCallback,
        heap_limit: usize,
    ) {
        let _ = (callback, heap_limit);
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

    fn inner(&self) -> &IsolateInner {
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
        let mut creator = handle
            .inner_mut()
            .snapshot_creator
            .take()
            .expect("v8::OwnedIsolate::create_blob: this isolate was not created by Isolate::snapshot_creator");
        Some(creator.create_blob(function_code_handling))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::promise::PromiseState;

    /// The termination requests answer, and the answer is that nothing was
    /// requested: the engine cannot stop a running execution.
    #[test]
    fn termination_requests_report_that_they_were_not_made() {
        let isolate = &mut Isolate::new(CreateParams::default());
        assert!(!isolate.is_execution_terminating());
        assert!(!isolate.terminate_execution());
        assert!(!isolate.cancel_terminate_execution());

        let handle: IsolateHandle = isolate.thread_safe_handle();
        assert!(!handle.terminate_execution());
        assert!(!handle.cancel_terminate_execution());
        assert!(!handle.is_execution_terminating());
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

    /// The promise hooks take a scope as their receiver in the crate we stand in
    /// for — a host calls them on the scope it is running in, not on the isolate
    /// — and a host that installs them keeps running.
    #[test]
    fn the_promise_hooks_are_accepted_from_a_scope() {
        crate::test_support::in_context!(scope, {
            let hook = crate::test_support::eval(scope, "(function () {})");
            let hook = Local::<crate::data::Function>::try_from(hook).expect("a function");
            scope.set_promise_hooks(Some(hook), Some(hook), Some(hook), Some(hook));
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
}
