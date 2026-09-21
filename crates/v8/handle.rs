//! Handles: [`Local`], [`MaybeLocal`], [`Global`], and the payload they carry.

use std::fmt;
use std::marker::PhantomData;
use std::num::NonZeroI32;
use std::ops::Deref;
use std::ptr::NonNull;
use std::rc::Rc;

use crux::value::ValueKind;
use runtime::api;

use crate::scope::PinScope;

/// What a handle points at.
///
/// The `T` in `Local<'s, T>` is a zero-sized tag whose only job is to select
/// methods and impls, so the payload cannot live in the tag — it lives here,
/// and `T` decides how it is read. The engine has four kinds of thing a handle
/// can name: a language value, the realm a `Local<Context>` names, the module
/// record a `Local<Module>` names, and the compiled script a `Local<Script>`
/// names.
///
/// Every variant is `Copy`, which is what lets [`Local`] be: a value, a context
/// and a module are plain data, and a script is a reference into the table its
/// text lives in ([`crate::store`]).
///
/// This is public only because [`Handle`] is; nothing outside the crate should
/// name it.
#[doc(hidden)]
#[derive(Clone, Copy)]
pub enum Payload {
    Value(api::Local),
    Context(api::Context),
    /// A module record. The record is not a language value, so it has no
    /// encoded form a `Value` payload could hold; a handle names the box
    /// directly, and a persistent one has to pin it.
    Module(api::Module),
    /// A compiled script, held as its source. The engine parses at compile time
    /// and evaluates at run time, so there is no compiled-script object to
    /// point at — only text, in this thread's script table.
    Script {
        slot: usize,
        generation: u32,
    },
    /// One of a module's requests (`v8::ModuleRequest`). V8 has a heap object
    /// per request; the engine keeps the requests as part of the module's own
    /// record, so a handle names the record and where in it.
    ModuleRequest {
        module: api::Module,
        index: u32,
    },
    /// A module's whole request list, as `Module::GetModuleRequests` hands it
    /// back: a `FixedArray` whose elements are the requests above.
    ModuleRequests {
        module: api::Module,
    },
    /// One frame of a stack-trace capture (`v8::StackFrame`). A frame is not a
    /// value: it is a position in a capture, so a handle names the capture's
    /// object and where in it.
    StackFrame {
        trace: api::Local,
        index: u32,
    },
    /// A message the bridge minted from a fixed template rather than read off
    /// a thrown value (`v8::Module::GetStalledTopLevelAwaitMessage`). V8 keeps
    /// a message object per template, whose text is the template's; there is
    /// nothing to hold but which template, and the module it reports on, which
    /// is the identity two such messages compare by.
    TemplateMessage {
        module: api::Module,
    },
}

impl Payload {
    pub(crate) fn as_value(&self) -> &api::Local {
        match self {
            Self::Value(value) => value,
            _ => panic!("bridge bug: a non-value handle read as a value"),
        }
    }

    /// The engine value, when this handle names one.
    pub(crate) fn as_value_opt(&self) -> Option<&api::Local> {
        match self {
            Self::Value(value) => Some(value),
            Self::Context(_)
            | Self::Module(_)
            | Self::Script { .. }
            | Self::ModuleRequest { .. }
            | Self::ModuleRequests { .. }
            | Self::StackFrame { .. }
            | Self::TemplateMessage { .. } => None,
        }
    }

    /// The capture and the position in it a stack-frame handle names.
    pub(crate) fn as_stack_frame(&self) -> Option<(api::Local, u32)> {
        match self {
            Self::StackFrame { trace, index } => Some((*trace, *index)),
            _ => None,
        }
    }

    pub(crate) fn as_context(&self) -> api::Context {
        match self {
            Self::Context(context) => *context,
            _ => panic!("bridge bug: a non-Context handle read as a Context"),
        }
    }

    pub(crate) fn as_module(&self) -> api::Module {
        match self {
            Self::Module(module) => *module,
            _ => panic!("bridge bug: a non-Module handle read as a Module"),
        }
    }

    /// The request a `ModuleRequest` handle names, and the module whose list a
    /// `FixedArray` handle is, when the payload is that kind.
    pub(crate) fn as_module_request(&self) -> Option<(api::Module, u32)> {
        match self {
            Self::ModuleRequest { module, index } => Some((*module, *index)),
            _ => None,
        }
    }

    pub(crate) fn as_module_requests(&self) -> Option<api::Module> {
        match self {
            Self::ModuleRequests { module } => Some(*module),
            _ => None,
        }
    }

    /// The text behind a `Script` payload, for a persistent handle that has to
    /// own it: a scoped reference dies with its scope.
    pub(crate) fn as_script_source(&self) -> Option<Rc<str>> {
        match self {
            Self::Script { slot, generation } => Some(
                crate::store::source(*slot, *generation)
                    .expect("bridge bug: a Script handle outlived its handle scope"),
            ),
            _ => None,
        }
    }
}

impl fmt::Debug for Payload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Value(value) => write!(f, "Payload::Value({value:?})"),
            Self::Context(_) => f.write_str("Payload::Context(..)"),
            Self::Module(_) => f.write_str("Payload::Module(..)"),
            Self::Script { slot, generation } => {
                write!(f, "Payload::Script({slot}, {generation})")
            }
            Self::ModuleRequest { module: _, index } => {
                write!(f, "Payload::ModuleRequest({index})")
            }
            Self::ModuleRequests { module: _ } => f.write_str("Payload::ModuleRequests(..)"),
            Self::StackFrame { trace: _, index } => write!(f, "Payload::StackFrame({index})"),
            Self::TemplateMessage { module: _ } => f.write_str("Payload::TemplateMessage(..)"),
        }
    }
}

/// The tag and the handle's lifetime, as one zero-sized marker.
///
/// `fn() -> T` rather than `&'s T` so that the marker is covariant in `T` and
/// does not require `T: 's` — the tag is only ever a type-level selector.
type Tag<'s, T> = PhantomData<(&'s (), fn() -> T)>;

/// A scoped handle over a language value (`v8::Local`).
///
/// The tag `T` decides which methods are in scope, and — as in the crate we
/// stand in for — a tag *reference* is a receiver: `Local<'s, T>` derefs to `T`,
/// which derefs into the [`LocalHandle`] carrying the methods. That chain is what
/// makes `&v8::Value` callable, and the address it hands out is the handle's own
/// payload, so a `&T` and the `Local` it came from name the same thing.
///
/// A handle is `Copy`, as it is in the crate we stand in for.
#[repr(C)]
pub struct Local<'s, T> {
    payload: Payload,
    marker: Tag<'s, T>,
}

/// Where a [`Local`]'s methods live, reachable from the tag it points at.
///
/// The crate we stand in for puts the methods on the tag itself, and the tag is
/// a pointer there; here a tag is zero-sized and has to deref somewhere, so this
/// is the somewhere. `T: Deref<Target = LocalHandle<'static, T>>` puts a `&T`
/// back on exactly the methods a `Local` had, and the inheritance edges
/// (`derefs_to!` in [`data`](crate::data)) move here with them, which keeps the
/// shape's *reachability* while the methods migrate onto the tags themselves.
///
/// It is scaffolding and is deleted with the last of those moves. Until then it
/// is the one deliberate difference from the crate we stand in for: a host that
/// names a method by UFCS on a tag, or implements a trait *for* a tag, sees it
/// (§9). Layout is [`Local`]'s, which is what lets a tag reference be read as one.
#[repr(C)]
pub struct LocalHandle<'s, T> {
    payload: Payload,
    marker: Tag<'s, T>,
}

impl<'s, T> LocalHandle<'s, T> {
    pub(crate) fn payload(&self) -> &Payload {
        &self.payload
    }

    pub(crate) fn engine(&self) -> &api::Local {
        self.payload.as_value()
    }

    pub(crate) fn context(&self) -> api::Context {
        self.payload.as_context()
    }

    pub(crate) fn module(&self) -> api::Module {
        self.payload.as_module()
    }

    pub(crate) fn script_source(&self) -> Rc<str> {
        match self.payload {
            Payload::Script { slot, generation } => crate::store::source(slot, generation)
                .expect("bridge bug: a Script handle outlived its handle scope"),
            _ => panic!("bridge bug: a non-Script handle read as a Script"),
        }
    }

    /// Retag in place: the payload is untouched, so this is a rebuild, not a
    /// reinterpretation. By reference, because the payload is `Copy` and the
    /// handle the tag derefs into is not — the crate we stand in for's tags are
    /// mostly not `Copy` either, which is why a `to_*` method taking `&self` is
    /// its shape and not a lint about the receiver.
    pub(crate) fn retag<U>(&self) -> Local<'s, U> {
        Local::from_payload(self.payload)
    }

    /// The same handle under another tag, by reference.
    ///
    /// # Safety
    ///
    /// Sound because `T` appears only in `PhantomData`: every `LocalHandle<'s,
    /// T>` has the same layout whatever `T` is.
    pub(crate) fn cast_ref<U>(&self) -> &LocalHandle<'s, U> {
        // SAFETY: as documented above — layout does not depend on the tag.
        unsafe { &*(self as *const Self).cast::<LocalHandle<'s, U>>() }
    }
}

impl<T> fmt::Debug for LocalHandle<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.payload, f)
    }
}

/// The tag a handle points at.
///
/// The address is the handle's payload, and a tag is zero-sized, so this
/// reference carries the address and nothing else: it never reaches the tag's
/// (empty) bytes, and the tag's own deref turns it back into the
/// [`LocalHandle`] whose methods it names.
impl<T> Deref for Local<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: every `&T` a host can hold is made here or by the tag-to-tag
        // derefs, so the address is a live `Local`'s payload, which outlives the
        // borrow this reference is tied to.
        unsafe { &*(&self.payload as *const Payload as *const T) }
    }
}

impl<'s, T> Local<'s, T> {
    pub(crate) fn from_payload(payload: Payload) -> Self {
        Self {
            payload,
            marker: PhantomData,
        }
    }

    /// Wrap an engine value under an unchecked tag. The caller knows what it
    /// put in, which is the same trust the crate we stand in for places in its
    /// own casts.
    pub(crate) fn from_engine(value: api::Local) -> Self {
        Self::from_payload(Payload::Value(value))
    }

    /// Wrap an engine module record under the `Module` tag.
    pub(crate) fn from_module(module: api::Module) -> Self {
        Self::from_payload(Payload::Module(module))
    }

    /// Construct a handle from an existing persistent handle (`v8::Local::New`).
    ///
    /// The scope is the shape's, not this bridge's: a handle here is the value
    /// it carries, so nothing about it depends on a scope's lifetime, and
    /// tying the answer to the scope's *borrow* would say otherwise. A host
    /// that opens a callback scope and returns the handle it makes — which the
    /// crate's module resolvers all do — carries the callback's lifetime, not
    /// the body-local scope's, and this is the signature that lets it.
    pub fn new<'i, H: Handle<Data = T>>(_scope: &PinScope<'_, 'i, ()>, handle: H) -> Local<'s, T> {
        Self::from_payload(handle.into_payload())
    }

    /// The engine value behind the handle.
    pub(crate) fn engine(&self) -> &api::Local {
        self.payload.as_value()
    }

    /// The payload, for the tag predicates in [`data`](crate::data).
    pub(crate) fn payload(&self) -> &Payload {
        &self.payload
    }

    /// The realm behind a `Context` handle.
    pub(crate) fn context(&self) -> api::Context {
        self.payload.as_context()
    }

    /// The record behind a `Module` handle.
    pub(crate) fn module(&self) -> api::Module {
        self.payload.as_module()
    }

    /// Retag the handle, with no check that the payload is of the new tag.
    ///
    /// Safe, unlike the crate we stand in for: the payload is untouched and the
    /// tag is phantom, so this is a rebuild rather than a reinterpretation. It is
    /// the bridge's own retag, not the crate's `cast` (which checks and panics) —
    /// kept separate for the callers that know the tag from context and cannot
    /// pay for a check, like the `From`/`TryFrom` tables in [`crate::data`].
    pub(crate) fn retag<U>(self) -> Local<'s, U> {
        let Self { payload, .. } = self;
        Local::from_payload(payload)
    }

    /// Attempts to cast the contained type to another, returning an error if the
    /// conversion fails (`v8::Local::try_cast`).
    pub fn try_cast<A>(self) -> Result<Local<'s, A>, <Self as TryInto<Local<'s, A>>>::Error>
    where
        Self: TryInto<Local<'s, A>>,
    {
        self.try_into()
    }

    /// Attempts to cast the contained type to another, panicking if the
    /// conversion fails (`v8::Local::cast`).
    ///
    /// The panic is the crate we stand in for's contract, not a shortcut: the
    /// return type is a handle, so there is no channel an error could come back
    /// on, and a host that wants the error asks [`try_cast`](Self::try_cast).
    pub fn cast<A>(self) -> Local<'s, A>
    where
        Self: TryInto<Local<'s, A>, Error: std::fmt::Debug>,
    {
        self.try_into().unwrap()
    }

    /// A handle of this tag built from one of its super types
    /// (`v8::Local::cast_unchecked`).
    ///
    /// # Safety
    ///
    /// In the crate we stand in for this reinterprets a pointer, hence the
    /// `unsafe`; here the payload is plain data and the call is a rebuild, so the
    /// signature is reproduced for the shapes. The bound is the crate's own: it
    /// asks that the *other* direction is a conversion that exists at all.
    #[inline(always)]
    pub unsafe fn cast_unchecked<A>(other: Local<'s, A>) -> Self
    where
        Local<'s, A>: TryFrom<Self>,
    {
        let Local { payload, .. } = other;
        Local::from_payload(payload)
    }

    /// Widen the handle's lifetime (v8::Local::extend_lifetime_unchecked).
    ///
    /// # Safety
    ///
    /// A handle is plain data, so nothing here can enforce that what it names
    /// outlives `'o`: the caller must know it does — which is what a host
    /// holding a persistent handle and a scope at once does know.
    #[inline(always)]
    pub unsafe fn extend_lifetime_unchecked<'o, O>(self) -> O
    where
        O: ExtendLifetime<'s, T, Input = Self>,
    {
        // SAFETY: the caller's contract.
        unsafe { O::extend_lifetime_unchecked_from(self) }
    }

    /// The engine value behind the handle, as a `Value`.
    pub(crate) fn into_engine(self) -> api::Local {
        match self.payload {
            Payload::Value(value) => value,
            _ => panic!("bridge bug: a non-value handle read as a value"),
        }
    }
}

impl<T> Clone for Local<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Local<'_, T> {}

impl<T> fmt::Debug for Local<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.payload, f)
    }
}

mod extend_sealed {
    pub trait Sealed {}
}

/// What a handle can be widened into
/// (v8's `ExtendLifetime`).
///
/// The output lifetime is a type parameter rather than a lifetime parameter
/// because a lifetime here would be late-bound, and a caller that knows which
/// lifetime it wants has to be able to say so — the same reason the crate we
/// stand in for gives.
pub trait ExtendLifetime<'s, T>: extend_sealed::Sealed {
    type Input;

    /// # Safety
    ///
    /// The caller must keep whatever the handle names alive for as long as the
    /// returned handle's lifetime claims.
    unsafe fn extend_lifetime_unchecked_from(value: Self::Input) -> Self;
}

impl<T> extend_sealed::Sealed for Local<'_, T> {}

impl<'o, 's, T> ExtendLifetime<'s, T> for Local<'o, T> {
    type Input = Local<'s, T>;

    unsafe fn extend_lifetime_unchecked_from(value: Local<'s, T>) -> Self {
        // The lifetime is a type-level marker over a payload that is already
        // plain data, so widening it rebuilds the handle and nothing else.
        Local::from_payload(value.payload)
    }
}

impl<T, Rhs: Handle> PartialEq<Rhs> for Local<'_, T> {
    /// Identity between two handles, whatever their tags: the payload is what a
    /// handle names, and the tag is a type-level selector.
    fn eq(&self, other: &Rhs) -> bool {
        payload_eq(&self.payload, other.payload_ref())
    }
}

impl<T, Rhs: Handle> PartialEq<Rhs> for Global<T> {
    fn eq(&self, other: &Rhs) -> bool {
        payload_eq(&self.payload, other.payload_ref())
    }
}

/// The identity a handle names, as the crate's `GetIdentityHash` reports it.
///
/// This is the identity `payload_eq` compares, which is the whole contract a
/// host's table needs from it: handles that compare equal hash equal. A handle
/// that names no object — a primitive, a context, a script — has no identity to
/// report, and the casts that can build one are unchecked, so this panics rather
/// than answering a hash that would collide with every other such handle.
pub(crate) fn identity_hash(payload: &Payload) -> NonZeroI32 {
    match payload {
        Payload::Value(value) => fold_identity(value_identity(value)),
        Payload::Module(module) => module.get_identity_hash(),
        Payload::Context(_)
        | Payload::Script { .. }
        | Payload::ModuleRequest { .. }
        | Payload::ModuleRequests { .. }
        | Payload::TemplateMessage { .. }
        | Payload::StackFrame { .. } => {
            panic!("bridge bug: a handle with no identity was hashed")
        }
    }
}

/// The engine's id for the object or function a value names. An object and a
/// function have ids of their own, and a handle of one is never compared with a
/// handle of the other, so one id space is enough here.
fn value_identity(value: &api::Local) -> Option<u64> {
    match value.value().kind() {
        ValueKind::Object(object) => Some(object.id()),
        ValueKind::Function(function) => Some(function.id()),
        _ => None,
    }
}

/// An id, folded to the non-zero `i32` the crate's hashes are.
fn fold_identity(identity: Option<u64>) -> NonZeroI32 {
    let Some(identity) = identity else {
        panic!("bridge bug: a handle with no identity was hashed");
    };
    // The low bits are the ones that differ between live boxes, and forcing the
    // low bit keeps the result non-zero; the fallback is unreachable.
    NonZeroI32::new((identity as u32 | 1) as i32).unwrap_or(NonZeroI32::MIN)
}

/// Whether two payloads name the same thing.
fn payload_eq(left: &Payload, right: &Payload) -> bool {
    match (left, right) {
        (Payload::Value(a), Payload::Value(b)) => a == b,
        // A copy of the same context names the same realm on the same isolate;
        // its global object identifies it, since the realm table is private to
        // the engine.
        (Payload::Context(a), Payload::Context(b)) => {
            a.isolate() == b.isolate() && a.global() == b.global()
        }
        (Payload::Module(a), Payload::Module(b)) => a == b,
        (
            Payload::ModuleRequest {
                module: am,
                index: ai,
            },
            Payload::ModuleRequest {
                module: bm,
                index: bi,
            },
        ) => am == bm && ai == bi,
        (
            Payload::StackFrame {
                trace: at,
                index: ai,
            },
            Payload::StackFrame {
                trace: bt,
                index: bi,
            },
        ) => at == bt && ai == bi,
        (
            Payload::Script {
                slot: a,
                generation: ag,
            },
            Payload::Script {
                slot: b,
                generation: bg,
            },
        ) => a == b && ag == bg,
        // The template a minted message came from is the same in both, so the
        // module it reports on is what tells two of them apart — and V8 mints
        // one message per stalled module, so it tells them apart the same way.
        (Payload::TemplateMessage { module: a }, Payload::TemplateMessage { module: b }) => a == b,
        _ => false,
    }
}

/// A handle that may be absent (`v8::MaybeLocal`).
///
/// Absence means the operation failed and a pending exception was set, which is
/// the same information the crate we stand in for carries in an empty handle.
#[derive(Debug)]
pub struct MaybeLocal<'s, T>(Option<Local<'s, T>>);

impl<'s, T> MaybeLocal<'s, T> {
    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    pub fn to_local(&self) -> Option<Local<'s, T>> {
        self.0
    }

    /// The handle, panicking when the operation failed — the crate we stand in
    /// for's convention: the caller asserts the operation cannot fail.
    pub fn to_local_checked(self) -> Local<'s, T> {
        self.0
            .expect("MaybeLocal::to_local_checked on an empty handle")
    }

    pub fn from_maybe(local: Local<'s, T>) -> Self {
        Self(Some(local))
    }
}

impl<'s, T> Clone for MaybeLocal<'s, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for MaybeLocal<'_, T> {}

impl<'s, T> From<Local<'s, T>> for MaybeLocal<'s, T> {
    fn from(local: Local<'s, T>) -> Self {
        Self(Some(local))
    }
}

impl<'s, T> From<Option<Local<'s, T>>> for MaybeLocal<'s, T> {
    fn from(local: Option<Local<'s, T>>) -> Self {
        Self(local)
    }
}

impl<'s, T> Default for MaybeLocal<'s, T> {
    fn default() -> Self {
        Self(None)
    }
}

/// A persistent handle: keeps a value alive for as long as the handle exists
/// (`v8::Global`).
///
/// "Persistent" has to mean it. A `Global` lives in host memory the
/// conservative stack scan cannot see, so it holds a pin: the value is a root
/// of every collection until the handle is dropped. Without one, a collection
/// could free the box and the handle would silently alias whatever reused the
/// address.
pub struct Global<T> {
    payload: Payload,
    /// Held only for its `Drop`; releasing the pin is the whole of its API.
    #[allow(dead_code)]
    pin: Option<crux::heap::Pin>,
    /// A script's text, owned.
    ///
    /// A scoped reference dies with the region its handle scope opened, so a
    /// handle that outlives that scope has to keep the text itself and hand it
    /// back to the scope it is read in. `None` for every other payload.
    script: Option<Rc<str>>,
    marker: PhantomData<fn() -> T>,
}

impl<T> Global<T> {
    /// A persistent handle over a scoped one.
    pub fn new<H: Handle<Data = T>>(_isolate: &crate::Isolate, handle: H) -> Self {
        Self::from_payload(handle.into_payload())
    }

    fn from_payload(payload: Payload) -> Self {
        // A script's source is owned; a module's box is pinned instead. A
        // request is not a value at all, and what keeps its module alive is the
        // pin on the module's own handle, so a request handle pins nothing.
        let pin = match &payload {
            Payload::Value(value) => Some(crux::heap::pin(*value.value())),
            Payload::Module(module) => Some(module.pin()),
            // A minted message names a module the same way a module handle
            // does, so it is rooted the same way.
            Payload::TemplateMessage { module } => Some(module.pin()),
            Payload::Context(_)
            | Payload::Script { .. }
            | Payload::ModuleRequest { .. }
            | Payload::ModuleRequests { .. }
            | Payload::StackFrame { .. } => None,
        };
        let script = payload.as_script_source();
        Self {
            payload,
            pin,
            script,
            marker: PhantomData,
        }
    }

    /// An empty handle (`v8::Global::Empty`).
    pub fn empty() -> Self {
        Self::from_payload(Payload::Value(api::Local::undefined()))
    }

    pub fn is_empty(&self) -> bool {
        match &self.payload {
            Payload::Value(value) => value.is_undefined(),
            Payload::Context(_)
            | Payload::Module(_)
            | Payload::Script { .. }
            | Payload::ModuleRequest { .. }
            | Payload::ModuleRequests { .. }
            | Payload::StackFrame { .. }
            | Payload::TemplateMessage { .. } => false,
        }
    }

    /// The scoped handle for this persistent one.
    ///
    /// The crate hands back a borrowed `&T` from `open`, which needs the methods
    /// to live on the tag; here they live on `Local`, so what comes back is the
    /// handle itself. An empty handle is not a panic either: the crate's `open`
    /// dereferences the slot it names, and this bridge's empty handle is
    /// *undefined*, so a host that opens one gets a value rather than a crash.
    pub fn open<'s>(&self, scope: &PinScope<'s, '_, ()>) -> Local<'s, T> {
        self.get(scope)
    }

    /// The scoped handle for this persistent one.
    pub fn get<'s>(&self, _scope: &PinScope<'s, '_, ()>) -> Local<'s, T> {
        match &self.script {
            Some(source) => {
                let (slot, generation) = crate::store::store(source.clone());
                Local::from_payload(Payload::Script { slot, generation })
            }
            None => Local::from_payload(self.payload),
        }
    }

    /// The handle this persistent one holds, with no scope to pass.
    ///
    /// [`get`](Self::get) takes a scope because the crate we stand in for needs
    /// one to rebuild a handle; the bridge's handles are already scope-free, so
    /// this is for the isolate's own storage, which has no scope in hand.
    pub(crate) fn handle(&self) -> Local<'_, T> {
        Local::from_payload(self.payload)
    }

    /// The payload by value, for a caller holding a longer lifetime than this
    /// handle: a `Local` is a payload plus a lifetime, so this is what the scope
    /// types rebuild into a handle of their own.
    pub(crate) fn payload_value(&self) -> Payload {
        self.payload
    }

    /// Consume this handle and hand out a raw pointer to it
    /// (`v8::Global::into_raw`).
    ///
    /// The pointer owns the handle: it has to come back through
    /// [`from_raw`](Self::from_raw), or what was rooted stays rooted. That is the
    /// crate we stand in for's contract too, where the V8-side slot stays pinned
    /// until it is taken back; here the handle is one boxed value in host memory,
    /// so "not taken back" is one leaked box rather than a pin the collector
    /// must hold.
    pub fn into_raw(self) -> NonNull<T> {
        // SAFETY: `Box::into_raw` never hands back a null pointer.
        unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(self))) }.cast::<T>()
    }

    /// Take back a handle handed out by [`into_raw`](Self::into_raw)
    /// (`v8::Global::from_raw`).
    ///
    /// The isolate is not read: the crate needs it to re-establish the handle's
    /// liveness against the isolate, and a handle here carries its own pin.
    ///
    /// # Safety
    ///
    /// `data` must come from `into_raw` on a `Global<T>` and be taken back
    /// exactly once — taking it back twice, or reading through it in between,
    /// is what the contract forbids.
    pub unsafe fn from_raw(isolate: &mut crate::Isolate, data: NonNull<T>) -> Self {
        let _ = isolate;
        // SAFETY: the caller's contract: this is the box `into_raw` leaked, and
        // taking it back here is what gives the handle an owner again.
        unsafe { *Box::from_raw(data.as_ptr().cast::<Global<T>>()) }
    }

    pub fn reset(&mut self) {
        *self = Self::from_payload(Payload::Value(api::Local::undefined()));
    }
}

impl<T> Clone for Global<T> {
    fn clone(&self) -> Self {
        Self {
            payload: self.payload,
            pin: match &self.payload {
                Payload::Value(value) => Some(crux::heap::pin(*value.value())),
                Payload::Module(module) => Some(module.pin()),
                Payload::TemplateMessage { module } => Some(module.pin()),
                Payload::Context(_)
                | Payload::Script { .. }
                | Payload::ModuleRequest { .. }
                | Payload::ModuleRequests { .. }
                | Payload::StackFrame { .. } => None,
            },
            script: self.script.clone(),
            marker: PhantomData,
        }
    }
}

impl<T> fmt::Debug for Global<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.payload, f)
    }
}

impl<T> Default for Global<T> {
    fn default() -> Self {
        Self::empty()
    }
}

/// Anything a [`Local`] can be built from.
///
/// The crate we stand in for exposes the same trait so that its own `Local`
/// constructor can accept both scoped and persistent handles.
pub trait Handle: Sized {
    type Data;

    #[doc(hidden)]
    fn into_payload(self) -> Payload;

    /// The payload, borrowed — what a comparison between two handles needs,
    /// since only one of them is being consumed.
    #[doc(hidden)]
    fn payload_ref(&self) -> &Payload;
}

impl<T> Handle for Local<'_, T> {
    type Data = T;

    fn into_payload(self) -> Payload {
        self.payload
    }

    fn payload_ref(&self) -> &Payload {
        &self.payload
    }
}

impl<T> Handle for &Local<'_, T> {
    type Data = T;

    fn into_payload(self) -> Payload {
        self.payload
    }

    fn payload_ref(&self) -> &Payload {
        &self.payload
    }
}

impl<T> Handle for Global<T> {
    type Data = T;

    fn into_payload(self) -> Payload {
        self.payload
    }

    fn payload_ref(&self) -> &Payload {
        &self.payload
    }
}

impl<T> Handle for &Global<T> {
    type Data = T;

    fn into_payload(self) -> Payload {
        self.payload
    }

    fn payload_ref(&self) -> &Payload {
        &self.payload
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Value;
    use crate::scope::GetIsolate;
    use crate::test_support::{eval, in_context};

    /// `open` reads a persistent handle back under the crate's name, and answers
    /// the same handle `get` does — an empty one included, which is *undefined*
    /// here rather than the crate's borrow of a slot it never wrote.
    #[test]
    fn open_reads_back_what_the_handle_holds() {
        in_context!(scope, {
            let isolate = scope.get_isolate_ptr();
            let value = eval(scope, "41 + 1");
            let persistent = Global::new(&isolate, value);

            assert_eq!(persistent.open(scope), persistent.get(scope));
            assert_eq!(persistent.open(scope), value);

            let empty: Global<Value> = Global::empty();
            assert!(empty.is_empty());
            assert!(empty.open(scope).is_undefined());
        });
    }

    /// The raw-pointer trip the host this stands in for makes: a persistent
    /// handle is handed out as a pointer, kept somewhere with no scope in reach,
    /// and taken back. The value is still the one it named in between.
    #[test]
    fn a_persistent_handle_survives_a_trip_through_a_raw_pointer() {
        in_context!(scope, {
            let isolate = scope.get_isolate_ptr();
            let value = eval(scope, "[1, 2, 3]");
            let persistent = Global::new(&isolate, value);

            let raw = persistent.into_raw();
            let mut isolate = isolate;
            // SAFETY: `raw` is the pointer `into_raw` handed out just above, and
            // it is taken back here — once.
            let back = unsafe { Global::<Value>::from_raw(&mut isolate, raw) };

            let opened = back.get(scope);
            crate::test_support::bind(scope, "arr", opened);
            assert_eq!(crate::test_support::eval_number(scope, "arr.length"), 3.0);
            assert_eq!(crate::test_support::eval_number(scope, "arr[2]"), 3.0);
        });
    }
}
