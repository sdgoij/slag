//! Scopes: `HandleScope`, `ContextScope`, `CallbackScope`, `TryCatch`, and the
//! `PinnedRef` type they share.
//!
//! The crate we stands in for needs its scope types for two reasons: handles
//! are allocated in a per-scope region, and its collector *moves* objects, so
//! the scope is what rewrites the handles pointing at them. Slag's arena keeps
//! stable addresses and its handles are `Rc`-backed, so neither reason applies
//! and the scopes here are inert.
//!
//! What is *not* inert is the type state. A host writes `scope!`, passes
//! `&mut PinScope` around, and relies on `ContextScope` upgrading a scope from
//! "no context" to "has a context". So the shapes are reproduced exactly —
//! including the `C = Context` type parameter that only ever appears in a
//! `PhantomData` — and only the bodies are trivial.

use std::marker::{PhantomData, PhantomPinned};
use std::ops::{Deref, DerefMut};
use std::pin::Pin;

use runtime::api;

use crate::Isolate;
use crate::data::{Context, Data, DataError, Function, Message, Value};
use crate::handle::{Local, Payload};
use crate::promise::PromiseRejectMessage;
use crate::store;

/// A scope pinned to its storage: `PinnedRef<'s, HandleScope<'i, C>>` is what a
/// host's `scope` binding is.
pub type PinScope<'s, 'i, C = Context> = PinnedRef<'s, HandleScope<'i, C>>;

/// The callback-scope form of [`PinScope`].
pub type PinCallbackScope<'s, 'i, C = Context> = PinnedRef<'s, CallbackScope<'i, C>>;

/// Storage for a scope, keeping it pinned for as long as the binding lives.
#[repr(C)]
pub struct ScopeStorage<T: ScopeInit> {
    scope: T,
    _pinned: PhantomPinned,
}

impl<T: ScopeInit> ScopeStorage<T> {
    pub fn new(scope: T) -> Self {
        Self {
            scope,
            _pinned: PhantomPinned,
        }
    }

    pub fn init(self: Pin<&mut Self>) -> PinnedRef<'_, T> {
        // SAFETY: the projection only re-borrows a field, so the scope stays
        // pinned for as long as the storage does — which is why `ScopeStorage`
        // is `!Unpin`.
        let scope = unsafe { self.map_unchecked_mut(|storage| &mut storage.scope) };
        PinnedRef(T::init_stack(scope))
    }
}

impl<T: ScopeInit> Deref for ScopeStorage<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.scope
    }
}

impl<T: ScopeInit> DerefMut for ScopeStorage<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.scope
    }
}

/// A scope that can be stored in [`ScopeStorage`].
pub trait ScopeInit: Sized {
    /// Hook for whatever a scope has to do when it becomes active. Slag's
    /// scopes have nothing to do; the hook exists so the shapes match.
    fn init_stack(me: Pin<&mut Self>) -> Pin<&mut Self>;
}

/// A scope pinned in place ([`ScopeStorage::init`]'s result).
#[repr(transparent)]
pub struct PinnedRef<'p, T>(Pin<&'p mut T>);

impl<'p, T> PinnedRef<'p, T> {
    /// Reborrow, as the crate we stand in for does, so a scope can be handed to
    /// a nested scope without giving up the binding.
    pub fn as_mut_ref(&mut self) -> PinnedRef<'_, T> {
        PinnedRef(self.0.as_mut())
    }
}

/// A handle scope: what a host creates first, and what it passes to everything
/// that makes a handle.
///
/// `C` is `Context` for a scope that has entered one and `()` for a scope that
/// has not; it appears only in a `PhantomData`, so both forms have the same
/// layout and a `ContextScope` can add the context by reinterpretation, exactly
/// as the crate we stand in for does.
#[repr(C)]
pub struct HandleScope<'i, C = Context> {
    isolate: Isolate,
    /// The realm this scope operates on. Empty for `C = ()`.
    context: Option<api::Context>,
    /// Whether this scope opened the region its handles live in, and so is the
    /// one that closes it. `true` for a `HandleScope`; `false` for a
    /// [`CallbackScope`] and an [`EscapableHandleScope`], which borrow the
    /// enclosing region so that a handle they hand out can carry the enclosing
    /// lifetime instead of their own borrow (see [`CallbackScope`]).
    owns_region: bool,
    marker: PhantomData<&'i mut C>,
    _pinned: PhantomPinned,
}

impl<'i, C> HandleScope<'i, C> {
    fn new_in(isolate: Isolate, context: Option<api::Context>) -> Self {
        store::open_region();
        Self::with_region(isolate, context, true)
    }

    /// A scope whose handles live in the enclosing region rather than one it
    /// opens: a [`CallbackScope`] or an [`EscapableHandleScope`]. Its `Drop`
    /// closes nothing, and the enclosing scope closes what it opened.
    fn new_inherited(isolate: Isolate, context: Option<api::Context>) -> Self {
        Self::with_region(isolate, context, false)
    }

    fn with_region(isolate: Isolate, context: Option<api::Context>, owns_region: bool) -> Self {
        Self {
            isolate,
            context,
            owns_region,
            marker: PhantomData,
            _pinned: PhantomPinned,
        }
    }
}

impl<C> Drop for HandleScope<'_, C> {
    fn drop(&mut self) {
        if self.owns_region {
            store::close_region();
        }
    }
}

impl<'i, C> ScopeInit for HandleScope<'i, C> {
    fn init_stack(me: Pin<&mut Self>) -> Pin<&mut Self> {
        me
    }
}

/// What [`HandleScope::new`] accepts: an isolate, or an enclosing scope.
pub trait NewHandleScope<'s> {
    type NewScope: ScopeInit;

    fn make_new_scope(me: &'s mut Self) -> Self::NewScope;
}

impl<'s> NewHandleScope<'s> for Isolate {
    type NewScope = HandleScope<'s, ()>;

    fn make_new_scope(me: &'s mut Self) -> Self::NewScope {
        let context = me.current_context();
        HandleScope::new_in(*me, context)
    }
}

impl<'s> NewHandleScope<'s> for crate::OwnedIsolate {
    type NewScope = HandleScope<'s, ()>;

    fn make_new_scope(me: &'s mut Self) -> Self::NewScope {
        let context = me.current_context();
        HandleScope::new_in(**me, context)
    }
}

impl<'s, 'p: 's, C> NewHandleScope<'s> for PinnedRef<'_, HandleScope<'p, C>> {
    type NewScope = HandleScope<'s, C>;

    fn make_new_scope(me: &'s mut Self) -> Self::NewScope {
        HandleScope::new_in(me.0.isolate, me.0.context)
    }
}

/// The same, over a callback scope: a `CallbackScope` *is* a `HandleScope` with a
/// name, so a host that has opened one can open an ordinary scope inside it — the
/// prologue every callback in this tree writes
/// (`v8::callback_scope!(unsafe cb_scope, context); v8::scope!(scope, cb_scope);`)
/// — and inherit its isolate and context unchanged.
impl<'s, 'p: 's, C> NewHandleScope<'s> for PinnedRef<'_, CallbackScope<'p, C>> {
    type NewScope = HandleScope<'s, C>;

    fn make_new_scope(me: &'s mut Self) -> Self::NewScope {
        HandleScope::new_in(me.0.inner.isolate, me.0.inner.context)
    }
}

impl<'s> HandleScope<'s> {
    #[allow(clippy::new_ret_no_self)]
    pub fn new<P: NewHandleScope<'s>>(scope: &'s mut P) -> ScopeStorage<P::NewScope> {
        ScopeStorage::new(P::make_new_scope(scope))
    }
}

impl<'p, 'i, C> PinnedRef<'p, HandleScope<'i, C>> {
    pub(crate) fn isolate_ptr(&self) -> Isolate {
        self.0.isolate
    }
}

impl<'p, 'i> PinnedRef<'p, HandleScope<'i, Context>> {
    /// The realm this scope operates on.
    pub(crate) fn realm(&self) -> api::Context {
        self.0
            .context
            .expect("bridge bug: handle scope without an entered context")
    }

    /// `v8::HandleScope::GetCurrentContext`.
    pub fn get_current_context(&self) -> Local<'p, Context> {
        Local::from_payload(crate::handle::Payload::Context(self.realm()))
    }

    /// Store the host's value that survives a suspension
    /// (`v8::HandleScope::SetContinuationPreservedEmbedderData`).
    ///
    /// The receiver is the scope, as it is in the crate we stand in for: the
    /// value is read back mid-suspension, where a host has a scope rather than
    /// the isolate it came from. The isolate keeps it.
    pub fn set_continuation_preserved_embedder_data(&self, data: Local<Value>) {
        let mut isolate = self.0.isolate;
        isolate.set_continuation_data(data);
    }

    /// The value [`set_continuation_preserved_embedder_data`] stored, or
    /// *undefined* (`v8::HandleScope::GetContinuationPreservedEmbedderData`).
    ///
    /// The handle carries the *scope's* lifetime rather than the borrow of it,
    /// as there: a host holds this value across the scope manipulations a
    /// suspension goes through, and a handle tied to a borrow could not survive
    /// one.
    ///
    /// [`set_continuation_preserved_embedder_data`]: Self::set_continuation_preserved_embedder_data
    pub fn get_continuation_preserved_embedder_data(&self) -> Local<'p, Value> {
        Local::from_payload(self.0.isolate.continuation_data_payload())
    }

    /// The hooks the engine runs around a promise's life
    /// (v8::Context::SetPromiseHooks), installed on the context this scope is
    /// in.
    ///
    /// The four run where V8 runs them: `init` when a promise is created, and
    /// `before`, `after` and `resolve` around a reaction job's handler and on
    /// settlement. An init is handed the promise and its parent, the other three
    /// the promise alone; `None` leaves a slot unset, and an unset slot runs
    /// nothing — which is how a host installs only the kinds it wants.
    ///
    /// The parent an init sees is `undefined`: V8 passes the promise whose
    /// reaction created this one where it knows it, and the engine does not track
    /// that (`.notes/embedding.md` §9).
    pub fn set_promise_hooks(
        &self,
        init_hook: Option<Local<Function>>,
        before_hook: Option<Local<Function>>,
        after_hook: Option<Local<Function>>,
        resolve_hook: Option<Local<Function>>,
    ) {
        let engine = |hook: Option<Local<Function>>| hook.map(|hook| hook.into_engine());
        self.realm().set_promise_hooks(
            engine(init_hook),
            engine(before_hook),
            engine(after_hook),
            engine(resolve_hook),
        );
    }
}

impl<'p, 'i, C> PinnedRef<'p, HandleScope<'i, C>> {
    /// The data a snapshot carried for the isolate at `index`
    /// (v8::HandleScope::GetIsolateDataFromSnapshotOnce).
    ///
    /// Always [`DataError::NoData`], and truthfully so: the format carries what
    /// a *context* was given, because that is what a host attaches and what a
    /// restorable realm is rebuilt around. The crate we stand in for answers
    /// this for the *second* read of an index; here it is every read.
    pub fn get_isolate_data_from_snapshot_once<T>(
        &self,
        index: usize,
    ) -> Result<Local<'p, T>, DataError>
    where
        T: 'static,
        for<'l> <Local<'l, Data> as TryInto<Local<'l, T>>>::Error: get_data_sealed::ToDataError,
        for<'l> Local<'l, Data>: TryInto<Local<'l, T>>,
    {
        let _ = index;
        Err(DataError::no_data::<T>())
    }

    /// The data a snapshot carried for the context at `index`
    /// (v8::HandleScope::GetContextDataFromSnapshotOnce).
    ///
    /// The item the creator attached there, once: the crate we stand in for
    /// hands each index out a single time, and so does this, because what a
    /// restore holds is the one persistent handle the blob's graph rooted. An
    /// index that was never attached, or has already been read, answers
    /// [`DataError::NoData`] — including on an isolate that booted from source,
    /// where every index does.
    pub fn get_context_data_from_snapshot_once<T>(
        &self,
        index: usize,
    ) -> Result<Local<'p, T>, DataError>
    where
        T: 'static,
        for<'l> <Local<'l, Data> as TryInto<Local<'l, T>>>::Error: get_data_sealed::ToDataError,
        for<'l> Local<'l, Data>: TryInto<Local<'l, T>>,
    {
        let Some(context) = self.0.context else {
            return Err(DataError::no_data::<T>());
        };
        let mut isolate = self.0.isolate;
        let identity = crate::snapshot::context_identity(context);
        let Some(item) = crate::snapshot::take_context_data(&mut isolate, identity, index) else {
            return Err(DataError::no_data::<T>());
        };
        let data: Local<'p, Data> = Local::from_payload(item.payload_value());
        data.try_into()
            .map_err(get_data_sealed::ToDataError::to_data_error)
    }
}

/// Seals what a failed cast can be converted into, so the bounds above name a
/// trait no host can implement — the same seal the crate we stand in for
/// carries, for the same reason.
mod get_data_sealed {
    use crate::DataError;
    use std::convert::Infallible;

    pub trait ToDataError {
        fn to_data_error(self) -> DataError;
    }

    impl ToDataError for DataError {
        fn to_data_error(self) -> DataError {
            self
        }
    }

    impl ToDataError for Infallible {
        fn to_data_error(self) -> DataError {
            unreachable!("an infallible cast cannot fail")
        }
    }
}

// ---------------------------------------------------------------------------
// PinnedRef deref chain: context-bearing scope -> plain scope -> isolate.
// ---------------------------------------------------------------------------

fn cast_pinned_ref<'a, 'p, I, O>(pinned: &'a PinnedRef<'p, I>) -> &'a PinnedRef<'p, O> {
    // SAFETY: the scope types that cast into each other store the scope they
    // wrap first and differ otherwise only in phantom type parameters, so the
    // two `PinnedRef`s name the same address with the same contents.
    unsafe { &*(pinned as *const PinnedRef<'p, I> as *const PinnedRef<'p, O>) }
}

fn cast_pinned_ref_mut<'a, 'p, I, O>(pinned: &'a mut PinnedRef<'p, I>) -> &'a mut PinnedRef<'p, O> {
    // SAFETY: as above.
    unsafe { &mut *(pinned as *mut PinnedRef<'p, I> as *mut PinnedRef<'p, O>) }
}

/// A cast that takes on the scope's *own* lifetime instead of the borrow of its
/// storage, which is what a callback scope's `Deref` is (see [`CallbackScope`]).
///
/// The casts above preserve the scope's lifetime, so they say nothing beyond the
/// layout; this one is a claim its caller makes, and the callers are the deref
/// pair that hands out a callback scope's handles plus the try-catch opened over
/// one.
fn cast_pinned_ref_widening<'a, 'p, 'q, I, O>(
    pinned: &'a PinnedRef<'p, I>,
) -> &'a PinnedRef<'q, O> {
    // SAFETY: the scope types that cast into each other store the scope they
    // wrap first and differ otherwise only in phantom type parameters, so the
    // two `PinnedRef`s name the same address with the same contents. The
    // lifetime is the scope's own parameter, which its constructor tied to what
    // the scope was opened from — not a reborrow, and that is the point.
    unsafe { &*(pinned as *const PinnedRef<'p, I> as *const PinnedRef<'q, O>) }
}

/// The mutable form of [`cast_pinned_ref_widening`].
fn cast_pinned_ref_widening_mut<'a, 'p, 'q, I, O>(
    pinned: &'a mut PinnedRef<'p, I>,
) -> &'a mut PinnedRef<'q, O> {
    // SAFETY: as above.
    unsafe { &mut *(pinned as *mut PinnedRef<'p, I> as *mut PinnedRef<'q, O>) }
}

impl<'p, 'i> Deref for PinnedRef<'p, HandleScope<'i, Context>> {
    type Target = PinnedRef<'p, HandleScope<'i, ()>>;

    fn deref(&self) -> &Self::Target {
        cast_pinned_ref(self)
    }
}

impl<'p, 'i> DerefMut for PinnedRef<'p, HandleScope<'i, Context>> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        cast_pinned_ref_mut(self)
    }
}

impl Deref for PinnedRef<'_, HandleScope<'_, ()>> {
    type Target = Isolate;

    fn deref(&self) -> &Self::Target {
        &self.0.isolate
    }
}

impl DerefMut for PinnedRef<'_, HandleScope<'_, ()>> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: the isolate handle is not structurally pinned — it is a
        // copyable pointer — so projecting it out of the pinned scope is fine.
        let scope = unsafe { self.0.as_mut().get_unchecked_mut() };
        &mut scope.isolate
    }
}

/// Makes a `HandleScope` and binds `&mut PinScope` to the given name:
/// `v8::scope!(let scope, isolate)`.
#[macro_export]
macro_rules! scope {
    ($scope:ident, $param:expr $(,)?) => {
        let mut $scope = $crate::HandleScope::new($param);
        let mut $scope = {
            let scope_pinned = unsafe { ::std::pin::Pin::new_unchecked(&mut $scope) };
            scope_pinned.init()
        };
        let $scope = &mut $scope;
    };
    (let $scope:ident, $param:expr $(,)?) => {
        $crate::scope!($scope, $param);
    };
}

/// As [`scope!`], but enters a context as well.
#[macro_export]
macro_rules! scope_with_context {
    ($scope:ident, $param:expr, $context:expr $(,)?) => {
        let mut $scope = $crate::HandleScope::new($param);
        let mut $scope = {
            let scope_pinned = unsafe { ::std::pin::Pin::new_unchecked(&mut $scope) };
            scope_pinned.init()
        };
        let $scope = &mut $scope;
        let context = $crate::Local::new($scope, $context);
        let $scope = &mut $crate::ContextScope::new($scope, context);
    };
    (let $scope:ident, $param:expr, $context:expr $(,)?) => {
        $crate::scope_with_context!($scope, $param, $context);
    };
}

/// A handle scope with a context entered (`v8::ContextScope`).
#[repr(C)]
pub struct ContextScope<'borrow, 'scope, P: ScopeInit> {
    scope: &'borrow mut PinnedRef<'scope, P>,
    /// What the thread's entered realm was before this scope, restored when it
    /// ends so nesting behaves like a stack.
    previous: Option<api::Context>,
    _pinned: PhantomPinned,
}

impl<'borrow, 'scope, 'i> ContextScope<'borrow, 'scope, HandleScope<'i, Context>> {
    /// Enter `context` for the duration of `scope`.
    ///
    /// The scope handed back has `C = Context` regardless of the parent's `C`,
    /// which is what the crate we stand in for does: entering a context is
    /// exactly the upgrade that type parameter records.
    ///
    /// Handed back as the scope itself rather than through [`ScopeStorage`], as
    /// there: a context scope is not address-sensitive, because it borrows the
    /// scope it wraps instead of holding one, so a host keeps it by reference.
    /// Storage here would make `&mut ContextScope::new(..)` a
    /// `&mut ScopeStorage<..>`, which is not a receiver the crate's
    /// `TryCatch::new` takes.
    pub fn new<C>(
        scope: &'borrow mut PinnedRef<'scope, HandleScope<'i, C>>,
        context: Local<'_, Context>,
    ) -> Self
    where
        'scope: 'borrow,
    {
        let realm = context.context();
        // SAFETY: `C` appears only in `PhantomData`, so the two `PinnedRef`s
        // have the same layout; the scope is re-read as context-bearing below.
        let scope: &'borrow mut PinnedRef<'scope, HandleScope<'i, Context>> =
            cast_pinned_ref_mut(scope);
        // SAFETY: the projection only writes a field in place — nothing is
        // moved, so the scope stays pinned where its storage put it.
        let inner = unsafe { scope.0.as_mut().get_unchecked_mut() };
        inner.context = Some(realm);
        // SAFETY: the scope holds a live pointer to the isolate it was made
        // from, which outlives the scope.
        inner.isolate.set_current_context(inner.context);
        let previous = crate::realm::enter(realm);
        Self {
            scope,
            previous,
            _pinned: PhantomPinned,
        }
    }
}

impl<'p, P: ScopeInit> Deref for ContextScope<'_, 'p, P> {
    type Target = PinnedRef<'p, P>;

    fn deref(&self) -> &Self::Target {
        self.scope
    }
}

impl<P: ScopeInit> DerefMut for ContextScope<'_, '_, P> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.scope
    }
}

impl<P: ScopeInit> ScopeInit for ContextScope<'_, '_, P> {
    fn init_stack(me: Pin<&mut Self>) -> Pin<&mut Self> {
        me
    }
}

impl<P: ScopeInit> Drop for ContextScope<'_, '_, P> {
    fn drop(&mut self) {
        crate::realm::restore(self.previous.take());
    }
}

/// A handle scope for a callback (`v8::CallbackScope`).
///
/// It opens no region of its own: it borrows the enclosing one, so a handle it
/// hands out lives as long as the scope it was made from — which is what its
/// `Deref` widening to `PinnedRef<'i, HandleScope<'i, C>>` promises, and what a
/// host's helper returning a handle it built in a callback scope relies on. A
/// callback scope that opens a region would invalidate exactly those handles at
/// its own `Drop`, one scope before the lifetime its `Deref` handed out. Deno's
/// `callback_scope!(cb, context); scope!(scope, cb);` prologue is unaffected: the
/// nested `scope!` is an ordinary `HandleScope` and opens its own region.
///
/// Its `Deref` is not the reborrow the other scopes' are: the handles it hands
/// out live as long as the thing it was made from — the context, which is the
/// scope's own parameter — rather than as long as the borrow of its storage.
/// That is the crate we stand in for's own choice, and the reason a host's helper
/// can return a handle it built in a callback scope at all. The obligation it
/// places on a host is real here too, and what it costs is stated in
/// [`crate::store`]: a handle that outlives the enclosing scope reads a poisoned
/// cell, and a *script* handle among them a released slot, either of which panics
/// rather than reading whatever took the place.
#[repr(C)]
pub struct CallbackScope<'i, C = Context> {
    inner: HandleScope<'i, C>,
}

impl<'s> CallbackScope<'s> {
    /// Open a callback scope (v8::CallbackScope::new).
    ///
    /// # Safety
    ///
    /// The crate we stand in for marks this unsafe because the scope it opens
    /// lives in the callback's stack frame. Slag's scopes keep no such frame
    /// state, so the signature is reproduced for the shapes, not for a hazard.
    #[allow(clippy::new_ret_no_self)]
    pub unsafe fn new<P: NewCallbackScope<'s>>(param: P) -> ScopeStorage<P::NewScope> {
        ScopeStorage::new(P::make_new_scope(param))
    }
}

impl<'i, C> ScopeInit for CallbackScope<'i, C> {
    fn init_stack(me: Pin<&mut Self>) -> Pin<&mut Self> {
        me
    }
}

impl<'p, 'i, C> Deref for PinnedRef<'p, CallbackScope<'i, C>> {
    type Target = PinnedRef<'i, HandleScope<'i, C>>;

    fn deref(&self) -> &Self::Target {
        cast_pinned_ref_widening(self)
    }
}

impl<'p, 'i, C> DerefMut for PinnedRef<'p, CallbackScope<'i, C>> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        cast_pinned_ref_widening_mut(self)
    }
}

/// The isolate behind anything a scope can be opened from.
///
/// The crate we stand in for names this trait and uses it as the bound that
/// lets a scope be opened from a context, a callback's info or an isolate. It
/// answers with the handle, which is a pointer, so the name matches there.
pub trait GetIsolate {
    fn get_isolate_ptr(&self) -> Isolate;
}

// The engine isolate is the first field of `IsolateInner`, so an engine pointer
// and the handle that wraps it are the same address. That is what lets a
// `Local<Context>` — which carries an engine context, not a bridge isolate —
// answer this trait at all.
const _: () = assert!(std::mem::offset_of!(crate::isolate::IsolateInner, engine) == 0);

fn bridge_isolate(engine: *mut api::Isolate) -> Isolate {
    // SAFETY: the engine isolate lives inside the inner, which is where the
    // handle points; see the assertion above.
    unsafe { Isolate::from_inner_ptr(engine.cast()) }
}

/// The bridge isolate a context belongs to, for an operation the crate we stand
/// in for declares without a scope (it has no scope to take one from).
pub(crate) fn isolate_of(context: api::Context) -> Isolate {
    bridge_isolate(context.isolate())
}

impl GetIsolate for Isolate {
    fn get_isolate_ptr(&self) -> Isolate {
        *self
    }
}

impl GetIsolate for crate::OwnedIsolate {
    fn get_isolate_ptr(&self) -> Isolate {
        (**self).get_isolate_ptr()
    }
}

impl<T: GetIsolate + ?Sized> GetIsolate for &T {
    fn get_isolate_ptr(&self) -> Isolate {
        (**self).get_isolate_ptr()
    }
}

impl<T: GetIsolate + ?Sized> GetIsolate for &mut T {
    fn get_isolate_ptr(&self) -> Isolate {
        (**self).get_isolate_ptr()
    }
}

impl<T: GetIsolate> GetIsolate for PinnedRef<'_, T> {
    fn get_isolate_ptr(&self) -> Isolate {
        self.0.as_ref().get_isolate_ptr()
    }
}

impl<C> GetIsolate for HandleScope<'_, C> {
    fn get_isolate_ptr(&self) -> Isolate {
        self.isolate
    }
}

impl<C> GetIsolate for CallbackScope<'_, C> {
    fn get_isolate_ptr(&self) -> Isolate {
        self.inner.isolate
    }
}

impl<P: GetIsolate + ScopeInit> GetIsolate for ContextScope<'_, '_, P> {
    fn get_isolate_ptr(&self) -> Isolate {
        self.scope.get_isolate_ptr()
    }
}

impl<C> GetIsolate for EscapableHandleScope<'_, '_, C> {
    fn get_isolate_ptr(&self) -> Isolate {
        self.inner.isolate
    }
}

impl<P: GetIsolate> GetIsolate for TryCatch<'_, '_, P> {
    fn get_isolate_ptr(&self) -> Isolate {
        self.scope.get_isolate_ptr()
    }
}

impl GetIsolate for Local<'_, Context> {
    fn get_isolate_ptr(&self) -> Isolate {
        isolate_of(self.context())
    }
}

impl GetIsolate for PromiseRejectMessage<'_> {
    fn get_isolate_ptr(&self) -> Isolate {
        self.isolate()
    }
}

impl GetIsolate for crate::fast_api::FastApiCallbackOptions<'_> {
    fn get_isolate_ptr(&self) -> Isolate {
        self.isolate
    }
}

impl GetIsolate for crate::function::FunctionCallbackInfo {
    fn get_isolate_ptr(&self) -> Isolate {
        self.isolate()
    }
}

/// A scope a callback scope can be opened from (v8::CallbackScope::new).
pub trait NewCallbackScope<'s>: Sized + GetIsolate {
    type NewScope: ScopeInit;

    fn make_new_scope(me: Self) -> Self::NewScope;
}

fn callback_scope_from<'s, C>(
    isolate: Isolate,
    context: Option<api::Context>,
) -> CallbackScope<'s, C> {
    CallbackScope {
        inner: HandleScope::new_inherited(isolate, context),
    }
}

impl<'s> NewCallbackScope<'s> for &'s mut Isolate {
    type NewScope = CallbackScope<'s, ()>;

    fn make_new_scope(me: Self) -> Self::NewScope {
        callback_scope_from(me.get_isolate_ptr(), None)
    }
}

impl<'s> NewCallbackScope<'s> for &'s mut crate::OwnedIsolate {
    type NewScope = CallbackScope<'s, ()>;

    fn make_new_scope(me: Self) -> Self::NewScope {
        callback_scope_from(me.get_isolate_ptr(), None)
    }
}

impl<'s> NewCallbackScope<'s> for Local<'s, Context> {
    type NewScope = CallbackScope<'s>;

    fn make_new_scope(me: Self) -> Self::NewScope {
        callback_scope_from(me.get_isolate_ptr(), Some(me.context()))
    }
}

impl<'s> NewCallbackScope<'s> for &'s crate::fast_api::FastApiCallbackOptions<'s> {
    type NewScope = CallbackScope<'s>;

    fn make_new_scope(me: Self) -> Self::NewScope {
        // A fast call has no context of its own, so the scope takes the one
        // entered on this thread — which is the realm its op was called from.
        callback_scope_from(me.get_isolate_ptr(), crate::realm::current())
    }
}

impl<'s> NewCallbackScope<'s> for &'s crate::function::FunctionCallbackInfo {
    type NewScope = CallbackScope<'s>;

    fn make_new_scope(me: Self) -> Self::NewScope {
        // The call's realm is the one entered on this thread: the engine made
        // it current before it called in.
        callback_scope_from(me.get_isolate_ptr(), crate::realm::current())
    }
}

impl<'s> NewCallbackScope<'s> for &'s PromiseRejectMessage<'s> {
    type NewScope = CallbackScope<'s>;

    fn make_new_scope(me: Self) -> Self::NewScope {
        // A rejection is reported while the realm it happened in is entered —
        // the engine reports it from inside a promise operation — so the scope
        // takes that realm, as the other context-less callbacks do.
        callback_scope_from(me.get_isolate_ptr(), crate::realm::current())
    }
}

/// A scope a `TryCatch` can be opened from (v8::TryCatch::new).
pub trait NewTryCatch<'scope>: GetIsolate {
    type NewScope: ScopeInit;

    fn make_new_scope(me: &'scope mut Self) -> Self::NewScope;
}

impl<'scope, 'obj: 'scope, 'i, C> NewTryCatch<'scope> for PinnedRef<'obj, HandleScope<'i, C>> {
    type NewScope = TryCatch<'scope, 'obj, HandleScope<'i, C>>;

    fn make_new_scope(me: &'scope mut Self) -> Self::NewScope {
        TryCatch {
            scope: me,
            catch: None,
            _pinned: PhantomPinned,
        }
    }
}

impl<'scope, 'obj: 'scope, 'i, C> NewTryCatch<'scope> for PinnedRef<'obj, CallbackScope<'i, C>> {
    type NewScope = TryCatch<'scope, 'i, HandleScope<'i, C>>;

    fn make_new_scope(me: &'scope mut Self) -> Self::NewScope {
        TryCatch {
            // A callback scope *is* a handle scope: the two `PinnedRef`s name
            // the same address, which is the bridge's own `Deref` between them.
            // The handler takes on that `Deref`'s lifetime rather than the
            // borrow, so a host's `tc_scope!` opened over a callback scope hands
            // out handles as long-lived as the callback scope's own.
            scope: cast_pinned_ref_widening_mut(me),
            catch: None,
            _pinned: PhantomPinned,
        }
    }
}

impl<'scope, 'obj: 'scope, 's, 'esc, C> NewTryCatch<'scope>
    for PinnedRef<'obj, EscapableHandleScope<'s, 'esc, C>>
{
    type NewScope = TryCatch<'scope, 'obj, EscapableHandleScope<'s, 'esc, C>>;

    fn make_new_scope(me: &'scope mut Self) -> Self::NewScope {
        TryCatch {
            scope: me,
            catch: None,
            _pinned: PhantomPinned,
        }
    }
}

impl<'scope, 'obj: 'scope, T: GetIsolate + ScopeInit> NewTryCatch<'scope>
    for ContextScope<'_, 'obj, T>
{
    type NewScope = TryCatch<'scope, 'obj, T>;

    fn make_new_scope(me: &'scope mut Self) -> Self::NewScope {
        TryCatch {
            // A context scope *is* a pinned reference to the scope it wraps —
            // its own `Deref` — so the handler borrows that inner scope.
            scope: me,
            catch: None,
            _pinned: PhantomPinned,
        }
    }
}

/// An external exception handler (v8::TryCatch).
///
/// Slag keeps one pending exception on its isolate, so this scope observes that
/// slot rather than opening a region of its own: opening it takes aside
/// whatever was already pending, and closing it swallows what it caught. The
/// engine's [`api::TryCatch`] holds that state; what is added here is the scope
/// chain a host dereferences through.
#[repr(C)]
pub struct TryCatch<'scope, 'obj, P> {
    scope: &'scope mut PinnedRef<'obj, P>,
    catch: Option<api::TryCatch>,
    _pinned: PhantomPinned,
}

impl<'scope, P: NewTryCatch<'scope>> TryCatch<'scope, '_, P> {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(param: &'scope mut P) -> ScopeStorage<P::NewScope> {
        ScopeStorage::new(P::make_new_scope(param))
    }
}

impl<P: GetIsolate> ScopeInit for TryCatch<'_, '_, P> {
    fn init_stack(me: Pin<&mut Self>) -> Pin<&mut Self> {
        // SAFETY: the projection writes a field in place — nothing moves, so
        // the scope stays pinned where its storage put it — and it runs once,
        // before anything can have borrowed the scope.
        let me = unsafe { me.get_unchecked_mut() };
        let mut isolate = me.scope.get_isolate_ptr();
        me.catch = Some(api::TryCatch::new(isolate.engine_mut()));
        // SAFETY: as above.
        unsafe { Pin::new_unchecked(me) }
    }
}

impl<'p, 'obj, P> PinnedRef<'p, TryCatch<'_, 'obj, P>> {
    /// Whether an exception was caught (v8::TryCatch::HasCaught).
    pub fn has_caught(&self) -> bool {
        self.catch().has_caught()
    }

    /// Whether the caught exception is an execution termination
    /// (v8::TryCatch::HasTerminated).
    ///
    /// A termination is a *request* here rather than a mark on the exception: a
    /// check point throws the same `Error` deno looks for while the request
    /// stands, and a host that has cancelled it sees `false`. The two differ only
    /// for a host that holds a caught termination past its own cancel.
    pub fn has_terminated(&self) -> bool {
        self.catch().has_terminated()
    }

    /// The caught exception (v8::TryCatch::Exception).
    pub fn exception(&self) -> Option<Local<'obj, Value>> {
        self.catch().exception().map(Local::from_engine)
    }

    /// The message the caught exception carries (v8::TryCatch::Message).
    ///
    /// V8 records a `Message` when it throws and answers it here; this engine
    /// keeps no such record, so the message is minted from the caught value —
    /// which is where every answer the message gives is read from, the same way
    /// [`Exception::create_message`](crate::Exception::create_message) mints
    /// one. `None` when nothing was caught.
    pub fn message(&self) -> Option<Local<'obj, Message>> {
        let exception = self.catch().exception()?;
        Some(Local::from_payload(Payload::Value(exception)))
    }

    /// Clear the caught exception (v8::TryCatch::Reset).
    pub fn reset(&mut self) {
        self.catch_mut().reset();
    }

    /// Re-throw what this handler caught (v8::TryCatch::ReThrow), answering the
    /// value that will now propagate to the handler around it.
    ///
    /// The engine keeps one pending-exception slot and the handler clears it on
    /// drop unless it was rethrown — which is what calling this records — so the
    /// value comes back and stays pending together.
    pub fn rethrow(&mut self) -> Option<Local<'obj, Value>> {
        let exception = self.catch().exception().map(Local::from_engine);
        self.catch_mut().rethrow();
        exception
    }

    fn catch(&self) -> &api::TryCatch {
        self.0
            .catch
            .as_ref()
            .expect("bridge bug: a TryCatch scope read before its storage was initialized")
    }

    fn catch_mut(&mut self) -> &mut api::TryCatch {
        // SAFETY: the field is borrowed in place; nothing moves.
        unsafe { self.0.as_mut().get_unchecked_mut() }
            .catch
            .as_mut()
            .expect("bridge bug: a TryCatch scope read before its storage was initialized")
    }
}

impl<'p, 'obj, P> Deref for PinnedRef<'p, TryCatch<'_, 'obj, P>> {
    type Target = PinnedRef<'obj, P>;

    fn deref(&self) -> &Self::Target {
        // The try-catch was opened from this scope, so a borrow of the
        // try-catch is a borrow of it.
        &*self.0.scope
    }
}

impl<'p, 'obj, P> DerefMut for PinnedRef<'p, TryCatch<'_, 'obj, P>> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: the projection reaches the scope the try-catch was opened
        // from, borrowed mutably for as long as the try-catch exists; nothing
        // is moved.
        let try_catch = unsafe { self.0.as_mut().get_unchecked_mut() };
        &mut *try_catch.scope
    }
}

/// A scope an escapable handle scope can be opened from.
pub trait NewEscapableHandleScope<'s> {
    type NewScope: ScopeInit;

    fn make_new_scope(me: &'s mut Self) -> Self::NewScope;
}

/// A handle scope one handle can escape from (v8::EscapableHandleScope).
///
/// Nothing has to be promoted here: a [`Local`] names a payload cell, and this
/// scope borrows the *enclosing* region rather than opening one, so a handle it
/// hands out — and the one [`escape`](PinnedRef::escape) returns — already lives
/// in a region that outlives it. That is the same rule a [`CallbackScope`]
/// follows, and for the same reason: `Deref` ties an ordinary handle to the
/// escapable scope's storage borrow and `escape` ties its answer to the scope it
/// was opened over, and a region this scope opened itself would die at its
/// `Drop`, before both. The cost is that a handle made under an escapable scope is
/// reclaimed when the enclosing scope closes rather than here — looser than V8's
/// invalidation, never tighter.
///
/// A nested `HandleScope::new(escapable)` borrows the escapable scope, which is
/// what keeps an escape from being called while such a scope is open.
#[repr(C)]
pub struct EscapableHandleScope<'s, 'esc, C = Context> {
    inner: HandleScope<'s, C>,
    marker: PhantomData<&'esc mut C>,
    _pinned: PhantomPinned,
}

impl<'s, 'esc, C> EscapableHandleScope<'s, 'esc, C> {
    /// The storage for a scope opened over `scope`
    /// (v8::EscapableHandleScope::New).
    ///
    /// The `where` clause is what makes the call inferable, and it is not
    /// decoration: this type names three parameters and the argument names none
    /// of them, so without the equality on `NewScope` the compiler has nothing to
    /// infer `'esc` (and, for a scope over an isolate, `C`) from — `cannot infer
    /// type` at the call, which is how the crate's own macro failed to compile.
    /// Tying it to the associated type of the trait every constructor
    /// implements is what makes `escapable_handle_scope!` work, and the
    /// `escapable_handle_scope!`-shaped test in `crate::stack_trace` is what
    /// holds it.
    #[allow(clippy::new_ret_no_self)]
    pub fn new<P>(scope: &'s mut P) -> ScopeStorage<P::NewScope>
    where
        P: NewEscapableHandleScope<'s, NewScope = EscapableHandleScope<'s, 'esc, C>>,
    {
        ScopeStorage::new(P::make_new_scope(scope))
    }
}

impl<C> ScopeInit for EscapableHandleScope<'_, '_, C> {
    fn init_stack(me: Pin<&mut Self>) -> Pin<&mut Self> {
        me
    }
}

impl<'s> NewEscapableHandleScope<'s> for Isolate {
    type NewScope = EscapableHandleScope<'s, 's, ()>;

    fn make_new_scope(me: &'s mut Self) -> Self::NewScope {
        let context = me.current_context();
        EscapableHandleScope {
            inner: HandleScope::new_inherited(*me, context),
            marker: PhantomData,
            _pinned: PhantomPinned,
        }
    }
}

impl<'s, 'obj: 's, 'i, C> NewEscapableHandleScope<'s> for PinnedRef<'obj, HandleScope<'i, C>> {
    type NewScope = EscapableHandleScope<'s, 'obj, C>;

    fn make_new_scope(me: &'s mut Self) -> Self::NewScope {
        EscapableHandleScope {
            inner: HandleScope::new_inherited(me.0.isolate, me.0.context),
            marker: PhantomData,
            _pinned: PhantomPinned,
        }
    }
}

/// A context scope can have an escapable one opened over it, which is the shape
/// `vm.rs` writes: `EscapableHandleScope::new(&mut ContextScope::new(..))`, so a
/// handle built while the context is entered still names its payload after the
/// escapable scope closes. The escapable scope borrows the *inner* handle scope
/// (a [`ContextScope`] already derefs to it), so the context stays entered for
/// as long as the escapable scope is open.
///
/// What an escape promotes to is the enclosing scope — the `'scope` of the
/// context scope, the parent `PinnedRef`'s own lifetime — and not the borrow of
/// that parent: `vm.rs`'s `eval_machine` returns the escaped handle under the
/// lifetime its own `PinScope` parameter names, and pinning the target to the
/// borrow instead is what made that return unprovable.
impl<'s, 'borrow, 'scope, 'i, C> NewEscapableHandleScope<'s>
    for ContextScope<'borrow, 'scope, HandleScope<'i, C>>
where
    C: 's + 'scope,
{
    type NewScope = EscapableHandleScope<'s, 'scope, C>;

    fn make_new_scope(me: &'s mut Self) -> Self::NewScope {
        EscapableHandleScope {
            inner: HandleScope::new_inherited(me.scope.0.isolate, me.scope.0.context),
            marker: PhantomData,
            _pinned: PhantomPinned,
        }
    }
}

impl<'p, 's, 'esc, C> Deref for PinnedRef<'p, EscapableHandleScope<'s, 'esc, C>> {
    type Target = PinnedRef<'p, HandleScope<'s, C>>;

    fn deref(&self) -> &Self::Target {
        cast_pinned_ref(self)
    }
}

impl<'p, 's, 'esc, C> DerefMut for PinnedRef<'p, EscapableHandleScope<'s, 'esc, C>> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        cast_pinned_ref_mut(self)
    }
}

impl<'p, 's, 'esc, C> PinnedRef<'p, EscapableHandleScope<'s, 'esc, C>> {
    /// Promote a handle to the enclosing scope
    /// (v8::EscapableHandleScope::Escape).
    pub fn escape<T>(&mut self, value: Local<'_, T>) -> Local<'esc, T> {
        Local::from_payload(*value.payload())
    }
}

/// A scope inside which JavaScript may run
/// (v8::AllowJavascriptExecutionScope).
///
/// The crate we stand in for pairs this with a *disallow* scope that stops
/// scripts running while a host is inside native code, and this one is what
/// re-permits them. The engine has no such gate — nothing in it refuses to run
/// a script — so there is nothing to restore, and the scope exists so host code
/// that brackets native work with it compiles and reads the same.
pub struct AllowJavascriptExecutionScope<'s, 'obj, P> {
    scope: &'s mut PinnedRef<'obj, P>,
    _pinned: PhantomPinned,
}

impl<'s, P: NewAllowJavascriptExecutionScope<'s>> AllowJavascriptExecutionScope<'s, '_, P> {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(param: &'s mut P) -> ScopeStorage<P::NewScope> {
        ScopeStorage::new(P::make_new_scope(param))
    }
}

/// What a scope hands a [`AllowJavascriptExecutionScope`]: the crate we stand in
/// for has the same trait, so a host's own scope types can be wrapped.
pub trait NewAllowJavascriptExecutionScope<'s> {
    type NewScope: ScopeInit;

    fn make_new_scope(me: &'s mut Self) -> Self::NewScope;
}

impl<'s, 'obj: 's, P> NewAllowJavascriptExecutionScope<'s> for PinnedRef<'obj, P> {
    type NewScope = AllowJavascriptExecutionScope<'s, 'obj, P>;

    fn make_new_scope(me: &'s mut Self) -> Self::NewScope {
        AllowJavascriptExecutionScope {
            scope: me,
            _pinned: PhantomPinned,
        }
    }
}

impl<P> ScopeInit for AllowJavascriptExecutionScope<'_, '_, P> {
    fn init_stack(me: Pin<&mut Self>) -> Pin<&mut Self> {
        me
    }
}

impl<'p, 's, 'obj, P> Deref for PinnedRef<'p, AllowJavascriptExecutionScope<'s, 'obj, P>> {
    type Target = PinnedRef<'obj, P>;

    fn deref(&self) -> &Self::Target {
        // The wrapped scope is a reference field, so this is a reborrow rather
        // than the layout cast the other scope derefs use.
        self.0.scope
    }
}

impl<'p, 's, 'obj, P> DerefMut for PinnedRef<'p, AllowJavascriptExecutionScope<'s, 'obj, P>> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: the wrapped scope is behind a reference, which is not
        // structurally pinned, so projecting it out of the pinned scope is
        // fine — the same reasoning as the isolate handle's projection.
        let scope = unsafe { self.0.as_mut().get_unchecked_mut() };
        scope.scope
    }
}

/// Makes a `TryCatch` and binds `&mut PinnedRef<TryCatch<..>>` to the given
/// name: `v8::tc_scope!(let scope, scope)`.
#[macro_export]
macro_rules! tc_scope {
    ($scope:ident, $param:expr $(,)?) => {
        let mut $scope = $crate::TryCatch::new($param);
        let mut $scope = {
            let scope_pinned = unsafe { ::std::pin::Pin::new_unchecked(&mut $scope) };
            scope_pinned.init()
        };
        let $scope = &mut $scope;
    };
    (let $scope:ident, $param:expr $(,)?) => {
        $crate::tc_scope!($scope, $param);
    };
}

/// Opens a callback scope and binds `&mut PinnedRef<CallbackScope<..>>` to the
/// given name: `v8::callback_scope!(unsafe scope, context)`.
#[macro_export]
macro_rules! callback_scope {
    (unsafe $scope:ident, $param:expr $(,)?) => {
        #[allow(clippy::macro_metavars_in_unsafe)]
        let mut $scope = {
            let param = $param;
            unsafe { $crate::CallbackScope::new(param) }
        };
        let mut $scope = {
            let scope_pinned = unsafe { ::std::pin::Pin::new_unchecked(&mut $scope) };
            scope_pinned.init()
        };
        let $scope = &mut $scope;
    };
    (unsafe let $scope:ident, $param:expr $(,)?) => {
        $crate::callback_scope!(unsafe $scope, $param);
    };
}

/// Opens an escapable handle scope and binds `&mut PinnedRef<EscapableHandleScope>`
/// to the given name: `v8::escapable_handle_scope!(let scope, scope)`.
#[macro_export]
macro_rules! escapable_handle_scope {
    ($scope:ident, $param:expr $(,)?) => {
        let mut $scope = $crate::EscapableHandleScope::new($param);
        let mut $scope = {
            let scope_pinned = unsafe { ::std::pin::Pin::new_unchecked(&mut $scope) };
            scope_pinned.init()
        };
        let $scope = &mut $scope;
    };
    (let $scope:ident, $param:expr $(,)?) => {
        $crate::escapable_handle_scope!($scope, $param);
    };
}

#[cfg(test)]
mod tests {
    use crate::DataError;

    /// The three lines `vm.rs:227` writes, which are a compile-time guard as much
    /// as a test: an escapable scope opened over a context scope, and a try-catch
    /// over *that*, which is what `tc_scope!` does. Without the `ContextScope`
    /// impls of `NewEscapableHandleScope` and `NewTryCatch` this does not build.
    /// What it also pins is that the context stays entered, so code evaluated
    /// under the try-catch still runs in this realm rather than a bare one.
    #[test]
    fn an_escapable_scope_can_be_opened_over_a_context_scope() {
        let isolate = &mut crate::Isolate::new(crate::CreateParams::default());
        crate::scope!(let handle_scope, isolate);
        let context = crate::Context::new(handle_scope, Default::default());
        let context_scope = &mut crate::ContextScope::new(handle_scope, context);

        let storage = std::pin::pin!(crate::EscapableHandleScope::new(context_scope));
        let scope = &mut storage.init();
        crate::tc_scope!(scope, scope);

        let value = crate::test_support::eval(&*scope, "6 * 7");
        let escaped = scope.escape(value);
        assert_eq!(
            crate::Local::<crate::data::Number>::try_from(escaped)
                .expect("a number")
                .value(),
            42.0
        );
        assert_eq!(
            crate::test_support::eval_number(&*scope, "6 * 7"),
            42.0,
            "the context is still the entered one"
        );
    }

    /// The prologue every callback in this tree writes: a callback scope, then an
    /// ordinary handle scope opened *over* it. The second inherits the first's
    /// isolate and context, which is the whole of what `NewHandleScope` for a
    /// pinned `CallbackScope` has to do — and what 12 call sites in the tree wait
    /// on. This is a compile-time guard as much as a test: without that impl the
    /// `scope!` below does not build.
    #[test]
    fn a_handle_scope_can_be_opened_over_a_callback_scope() {
        crate::test_support::in_context!(scope, {
            let context = scope.get_current_context();
            crate::callback_scope!(unsafe cb_scope, context);
            crate::scope!(inner, cb_scope);
            // The scope is an ordinary one, and it is the callback scope's: code
            // evaluates in the realm the callback scope was opened over, which is
            // what the two inherited fields are for.
            assert_eq!(crate::test_support::eval_number(inner, "6 * 7"), 42.0);
        });
    }

    /// The continuation-preserved value is held across the scope manipulations a
    /// suspension goes through: the handle carries the scope's lifetime, not a
    /// borrow of it, so a host can keep reading the value while the scope is
    /// mutated underneath. That shape is the whole of what the host this stands
    /// in for needs it for, and getting it wrong is a borrow error there rather
    /// than a wrong answer here.
    #[test]
    fn the_continuation_value_survives_a_mutation_of_its_scope() {
        crate::test_support::in_context!(scope, {
            let value = crate::test_support::eval(scope, "40 + 2");
            scope.set_continuation_preserved_embedder_data(value);
            let held = scope.get_continuation_preserved_embedder_data();

            // A `&mut` use of the same scope, with `held` still live.
            scope.set_microtasks_policy(crate::MicrotasksPolicy::Explicit);

            crate::test_support::bind(scope, "cped", held);
            assert_eq!(crate::test_support::eval_number(scope, "cped"), 42.0);
        });
    }

    /// Run a script that throws, leaving its exception pending.
    fn throw_a_test_error(scope: &crate::scope::PinScope<'_, '_>) {
        let text = crate::String::new(scope, "throw new Error('boom')").expect("string");
        let script = crate::Script::compile(scope, text, None).expect("compile");
        assert!(script.run(scope).is_none(), "the script throws");
    }

    /// A handler answers the message of what it caught
    /// (`v8::TryCatch::Message`), and a compile error's message carries the line
    /// the bridge recorded for it.
    #[test]
    fn a_handler_answers_the_message_of_what_it_caught() {
        crate::test_support::in_context!(scope, {
            crate::tc_scope!(let caught, scope);
            // A failed compile leaves its exception pending where the handler is
            // watching, and that is the one case the bridge records a position
            // for — so its message has a line to answer with.
            let text = crate::String::new(caught, "let a = 1;\n  )\n").expect("string");
            assert!(
                crate::Script::compile(caught, text, None).is_none(),
                "the source compiles, so there is no error to ask about"
            );
            assert!(caught.has_caught());

            let message = caught.message().expect("the caught error has a message");
            assert_eq!(message.get_line_number(caught), Some(2));
            let line = message.get_source_line(caught).expect("the recorded line");
            assert_eq!(line.to_rust_string_lossy(caught), "  )");
        });
    }

    /// A handler that caught nothing answers no message, which is what
    /// distinguishes it from one whose caught value has no position.
    #[test]
    fn a_handler_that_caught_nothing_answers_no_message() {
        crate::test_support::in_context!(scope, {
            crate::tc_scope!(let caught, scope);
            assert!(!caught.has_caught());
            assert!(caught.message().is_none());
        });
    }

    /// A handler that rethrows leaves the exception pending past itself — which
    /// is what a handler opened around it sees — and hands back the value that
    /// is doing the propagating.
    ///
    /// The throwing call has to happen *inside* the handler: opening one takes
    /// the pending exception aside, which is what lets the handler see what it
    /// caught rather than what was already there.
    #[test]
    fn a_rethrown_exception_stays_pending() {
        crate::test_support::in_context!(scope, {
            {
                crate::tc_scope!(let caught, scope);
                throw_a_test_error(caught);
                assert!(caught.has_caught());
                let thrown = caught.rethrow().expect("the caught exception comes back");
                assert!(thrown.is_object());
                assert!(scope.engine().has_pending_exception());
            }

            assert!(
                scope.engine().has_pending_exception(),
                "the rethrow left it pending for the handler around this one"
            );
        });
    }

    /// The handler that does *not* rethrow swallows what it caught, which is what
    /// makes the test above say something.
    #[test]
    fn an_exception_a_handler_did_not_rethrow_does_not_survive_it() {
        crate::test_support::in_context!(scope, {
            {
                crate::tc_scope!(let caught, scope);
                throw_a_test_error(caught);
                assert!(caught.has_caught());
            }

            assert!(
                !scope.engine().has_pending_exception(),
                "the handler swallowed it on the way out"
            );
        });
    }

    /// The shape deno's error path runs: an exception is caught and *read*, and
    /// the host then calls a JS function whose body calls a host callback — the
    /// realm's format-exception callback is an arrow that calls an op. Reading
    /// the exception hands it to the handler, so the call that follows is not
    /// made with one still pending, and the callback's string reaches the host.
    /// A callback call made while the exception is still pending is reported as
    /// the callback's own throw, which is why this is one test and not two.
    #[test]
    fn a_host_call_after_a_handler_read_the_exception_reaches_the_callbacks_result() {
        use crate::Function;
        use crate::Local;
        use crate::data::Value;
        use crate::function::{FunctionCallbackArguments, ReturnValue};
        use crate::test_support::{bind, eval, in_context};

        fn is_native_error(
            scope: &mut crate::scope::PinScope<'_, '_>,
            _args: FunctionCallbackArguments,
            rv: ReturnValue,
        ) {
            rv.set(crate::data::Boolean::new(scope, true).into());
        }

        in_context!(scope, {
            let op = Function::builder(is_native_error).build(scope).expect("op");
            bind(scope, "isNativeError", op.cast::<Value>());
            let callback = eval(
                scope,
                "(error) => { isNativeError(error); return `realm / ${error}`; }",
            );
            let callback: Local<crate::data::Function> = callback.try_cast().expect("function");
            let this: Local<Value> = Local::from_engine(runtime::api::Local::undefined());

            crate::tc_scope!(let caught, scope);
            throw_a_test_error(caught);
            assert!(caught.has_caught(), "the throw was caught");
            let exception = caught.exception().expect("the caught exception");
            let formatted = callback.call(caught, this, &[exception]);
            assert_eq!(
                formatted.map(|value| value.to_rust_string_lossy(caught)),
                Some("realm / Error: boom".to_string()),
                "the callback ran and its string came back"
            );
        });
    }

    /// The pattern a host writes: pin the scope, init it, and hand it wherever a
    /// scope goes. It wraps the scope it was made from rather than replacing it,
    /// so everything reachable through the outer one stays reachable.
    #[test]
    fn an_allow_scope_is_the_scope_it_wraps() {
        let isolate = &mut crate::Isolate::new(crate::CreateParams::default());
        crate::scope!(let handle_scope, isolate);
        let scope: &mut crate::PinScope<'_, '_, ()> = handle_scope;

        let allow = std::pin::pin!(crate::AllowJavascriptExecutionScope::new(scope));
        let scope = &mut allow.init();
        let text = crate::String::new(scope, "'bracketed'").expect("string");
        assert_eq!(text.to_rust_string_lossy(scope), "'bracketed'");
    }

    /// The snapshot restore side answers for an isolate that booted from source,
    /// and says so through the error channel the shape has rather than a panic.
    #[test]
    fn reading_snapshot_data_reports_that_there_is_none() {
        crate::test_support::in_context!(scope, {
            match scope.get_context_data_from_snapshot_once::<crate::data::Data>(0) {
                Err(DataError::NoData { expected }) => assert!(expected.contains("Data")),
                other => panic!("expected no data, got {other:?}"),
            }
            assert!(matches!(
                scope.get_isolate_data_from_snapshot_once::<crate::data::Data>(3),
                Err(DataError::NoData { .. })
            ));
            // The cast bound is part of the shape: asking for a narrower tag is
            // what a host does when it attached one.
            assert!(matches!(
                scope.get_context_data_from_snapshot_once::<crate::data::Object>(0),
                Err(DataError::NoData { .. })
            ));
        });
    }
}
