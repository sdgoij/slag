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
use std::ptr::NonNull;
use std::rc::Rc;

use runtime::api;

use crate::Isolate;
use crate::data::Context;
use crate::handle::Local;

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
    isolate: NonNull<Isolate>,
    /// The realm this scope operates on. Empty for `C = ()`.
    context: Option<Rc<api::Context>>,
    marker: PhantomData<&'i mut C>,
    _pinned: PhantomPinned,
}

impl<'i, C> HandleScope<'i, C> {
    fn new_in(isolate: NonNull<Isolate>, context: Option<Rc<api::Context>>) -> Self {
        Self {
            isolate,
            context,
            marker: PhantomData,
            _pinned: PhantomPinned,
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
        HandleScope::new_in(NonNull::from(me), context)
    }
}

impl<'s> NewHandleScope<'s> for crate::OwnedIsolate {
    type NewScope = HandleScope<'s, ()>;

    fn make_new_scope(me: &'s mut Self) -> Self::NewScope {
        let context = me.current_context();
        HandleScope::new_in(NonNull::from(&mut **me), context)
    }
}

impl<'s, 'p: 's, C> NewHandleScope<'s> for PinnedRef<'_, HandleScope<'p, C>> {
    type NewScope = HandleScope<'s, C>;

    fn make_new_scope(me: &'s mut Self) -> Self::NewScope {
        HandleScope::new_in(me.0.isolate, me.0.context.clone())
    }
}

impl<'s> HandleScope<'s> {
    #[allow(clippy::new_ret_no_self)]
    pub fn new<P: NewHandleScope<'s>>(scope: &'s mut P) -> ScopeStorage<P::NewScope> {
        ScopeStorage::new(P::make_new_scope(scope))
    }
}

impl<'p, 'i, C> PinnedRef<'p, HandleScope<'i, C>> {
    pub(crate) fn isolate_ptr(&self) -> NonNull<Isolate> {
        self.0.isolate
    }
}

impl<'p, 'i> PinnedRef<'p, HandleScope<'i, Context>> {
    /// The realm this scope operates on.
    pub(crate) fn realm(&self) -> &Rc<api::Context> {
        self.0
            .context
            .as_ref()
            .expect("bridge bug: handle scope without an entered context")
    }

    /// `v8::HandleScope::GetCurrentContext`.
    pub fn get_current_context(&self) -> Local<'p, Context> {
        Local::from_payload(crate::handle::Payload::Context(self.realm().clone()))
    }
}

// ---------------------------------------------------------------------------
// PinnedRef deref chain: context-bearing scope -> plain scope -> isolate.
// ---------------------------------------------------------------------------

fn cast_pinned_ref<'a, 'p, I, O>(pinned: &'a PinnedRef<'p, I>) -> &'a PinnedRef<'p, O> {
    // SAFETY: every scope type stores its fields in the same order and differs
    // only in phantom type parameters, so the two `PinnedRef`s have the same
    // layout.
    unsafe { &*(pinned as *const PinnedRef<'p, I> as *const PinnedRef<'p, O>) }
}

fn cast_pinned_ref_mut<'a, 'p, I, O>(pinned: &'a mut PinnedRef<'p, I>) -> &'a mut PinnedRef<'p, O> {
    // SAFETY: as above.
    unsafe { &mut *(pinned as *mut PinnedRef<'p, I> as *mut PinnedRef<'p, O>) }
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
        // SAFETY: the scope holds a live pointer to an isolate it does not own;
        // the isolate outlives every scope made from it.
        unsafe { self.0.isolate.as_ref() }
    }
}

impl DerefMut for PinnedRef<'_, HandleScope<'_, ()>> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: as above. Scopes are stack-nested, so at most one is live.
        let mut isolate = self.0.isolate;
        unsafe { isolate.as_mut() }
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
    previous: Option<Rc<api::Context>>,
    _pinned: PhantomPinned,
}

impl<'borrow, 'scope, 'i> ContextScope<'borrow, 'scope, HandleScope<'i, Context>> {
    /// Enter `context` for the duration of `scope`.
    ///
    /// The scope handed back has `C = Context` regardless of the parent's `C`,
    /// which is what the crate we stand in for does: entering a context is
    /// exactly the upgrade that type parameter records.
    #[allow(clippy::new_ret_no_self)]
    pub fn new<C>(
        scope: &'borrow mut PinnedRef<'scope, HandleScope<'i, C>>,
        context: Local<'_, Context>,
    ) -> ScopeStorage<Self>
    where
        'scope: 'borrow,
    {
        let realm = context.context().clone();
        // SAFETY: `C` appears only in `PhantomData`, so the two `PinnedRef`s
        // have the same layout; the scope is re-read as context-bearing below.
        let scope: &'borrow mut PinnedRef<'scope, HandleScope<'i, Context>> =
            cast_pinned_ref_mut(scope);
        // SAFETY: the projection only writes a field in place — nothing is
        // moved, so the scope stays pinned where its storage put it.
        let inner = unsafe { scope.0.as_mut().get_unchecked_mut() };
        inner.context = Some(realm.clone());
        // SAFETY: the scope holds a live pointer to the isolate it was made
        // from, which outlives the scope.
        unsafe { inner.isolate.as_ref() }.set_current_context(inner.context.clone());
        let previous = crate::realm::enter(realm);
        ScopeStorage::new(Self {
            scope,
            previous,
            _pinned: PhantomPinned,
        })
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

/// A handle scope for a callback (`v8::CallbackScope`). Slag has no callback
/// handle region to open; the type exists so host code that opens one compiles.
#[repr(C)]
pub struct CallbackScope<'i, C = Context> {
    inner: HandleScope<'i, C>,
}

impl<'i, C> ScopeInit for CallbackScope<'i, C> {
    fn init_stack(me: Pin<&mut Self>) -> Pin<&mut Self> {
        me
    }
}

impl<'p, 'i> Deref for PinnedRef<'p, CallbackScope<'i, Context>> {
    type Target = PinnedRef<'p, HandleScope<'i, Context>>;

    fn deref(&self) -> &Self::Target {
        cast_pinned_ref(self)
    }
}

impl<'p, 'i> DerefMut for PinnedRef<'p, CallbackScope<'i, Context>> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        cast_pinned_ref_mut(self)
    }
}
