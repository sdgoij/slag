//! Weak persistent handles: [`Weak`] and [`TracedReference`], over the engine's
//! own weak registry (`runtime::api::Weak`).
//!
//! Both read the same way to a host: a handle that does *not* keep its value
//! alive, and answers `None` once the collector has taken it. The crate we stand
//! in for separates them because its `TracedReference` belongs to the cppgc
//! machinery; here the difference is which payloads they accept — see the
//! divergence note on [`TracedReference`].

use std::fmt;
use std::marker::PhantomData;

use runtime::api;

use crate::handle::{Global, Handle, Local, Payload};
use crate::scope::PinScope;

/// What a weak handle is holding.
///
/// A *value* is the case the engine can watch: it is a box in the arena, and the
/// sweep that frees the box is what empties the handle. The other payloads this
/// bridge has — a realm, a module record, a script table entry — are not heap
/// values, so there is no box for the collector to take, and the handle holds
/// them the way [`Global`] does (pinned where `Global` pins).
enum Inner<T> {
    Weak(api::Weak),
    Strong(Global<T>),
}

fn inner_from_payload<T>(payload: Payload) -> Inner<T> {
    match payload {
        // The one payload the engine can watch.
        Payload::Value(value) => Inner::Weak(api::Weak::new(value)),
        // One `Global`'s worth of ownership: the pin rule for every non-value
        // payload lives in one place, and this reuses it rather than repeating it.
        other => Inner::Strong(Global::from_payload(other)),
    }
}

/// The context a weak death callback is handed (`v8::WeakCallbackInfo`).
///
/// The crate we stand in for fills this with the isolate, the value and the
/// parameter a `SetWeak` call carried. The call sites in this tree write `|_|`,
/// so what is here is the type; its accessors arrive when a call site asks for
/// one (§9's demand rule) rather than existing unused.
pub struct WeakCallbackInfo<T> {
    marker: PhantomData<fn() -> T>,
}

impl<T> fmt::Debug for WeakCallbackInfo<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WeakCallbackInfo")
    }
}

/// A weak persistent handle (`v8::Weak<T>`).
///
/// The value is not kept alive by this handle: it dies on its own terms, and the
/// handle answers `None` afterwards. `with_finalizer` adds the crate's other half
/// — a callback for the death — which runs after the collection that took it,
/// once, and with the handle already empty.
pub struct Weak<T> {
    inner: Inner<T>,
}

impl<T> Weak<T> {
    /// A weak handle over `handle` (`v8::Weak::new`).
    pub fn new<H: Handle<Data = T>>(_scope: &PinScope<'_, '_, ()>, handle: H) -> Self {
        Self {
            inner: inner_from_payload(handle.into_payload()),
        }
    }

    /// A weak handle with a callback for the value's death
    /// (`v8::Weak::with_finalizer`).
    ///
    /// The callback is deferred like a host finalizer: it may allocate and may
    /// run JS, the sweep may not, so the engine queues it and the next drain runs
    /// it — with this handle already empty, so it cannot resurrect what it is
    /// told about. A *non-value* payload has nothing for the engine to watch, so
    /// its finalizer never runs; §9 states that divergence rather than this type
    /// hiding it.
    pub fn with_finalizer<H: Handle<Data = T>>(
        scope: &PinScope<'_, '_, ()>,
        handle: H,
        finalizer: Box<dyn FnOnce(WeakCallbackInfo<T>)>,
    ) -> Self
    where
        T: 'static,
    {
        let weak = Self::new(scope, handle);
        if let Inner::Weak(inner) = &weak.inner {
            inner.set_callback(Box::new(move || {
                finalizer(WeakCallbackInfo {
                    marker: PhantomData,
                })
            }));
        }
        weak
    }

    /// The value, while the collector has not taken it (`v8::Weak::to_local`).
    pub fn to_local<'s>(&self, scope: &PinScope<'s, '_, ()>) -> Option<Local<'s, T>> {
        match &self.inner {
            Inner::Weak(inner) => inner.to_local().map(Local::from_engine),
            // Nothing to watch, so it is there while the handle is.
            Inner::Strong(strong) => Some(strong.get(scope)),
        }
    }

    /// Whether the value is gone (`v8::Weak::is_empty`).
    pub fn is_empty(&self) -> bool {
        match &self.inner {
            Inner::Weak(inner) => inner.is_empty(),
            Inner::Strong(_) => false,
        }
    }

    /// Release the handle whatever the value does from here on
    /// (`v8::Weak::clear`), which is also what a weak callback does with the
    /// handle it is handed.
    pub fn clear(&mut self) {
        if let Inner::Weak(inner) = &mut self.inner {
            inner.clear();
        }
    }
}

impl<T> fmt::Debug for Weak<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Weak")
            .field("empty", &self.is_empty())
            .finish()
    }
}

/// A weak reference to a value a host keeps in its own structures
/// (`v8::TracedReference<T>`, `v8::TracedGlobal<T>`).
///
/// The read is the same as [`Weak`]'s — `get` answers `None` once the collector
/// has taken the value — which is the shape the call sites in this tree rely on:
/// they `unwrap` or `match` it.
///
/// **One divergence, stated** (§9): a `TracedReference` of a *value* is a real
/// weak handle, while one of a `Context`, a `Module` or a script — payloads this
/// bridge does not keep as heap values — holds the payload the way [`Global`]
/// does and therefore answers `Some` for its lifetime. The engine has no weak
/// handle over a realm or a module record; a host that needs one needs the engine
/// to grow it, not this type to pretend.
pub struct TracedReference<T> {
    inner: Inner<T>,
}

impl<T> TracedReference<T> {
    /// A weak reference to `value` (`v8::TracedReference::new`).
    pub fn new<H: Handle<Data = T>>(_scope: &PinScope<'_, '_, ()>, value: H) -> Self {
        Self {
            inner: inner_from_payload(value.into_payload()),
        }
    }

    /// The value, while the collector has not taken it
    /// (`v8::TracedReference::Get`).
    pub fn get<'s>(&self, scope: &PinScope<'s, '_, ()>) -> Option<Local<'s, T>> {
        match &self.inner {
            Inner::Weak(inner) => inner.to_local().map(Local::from_engine),
            Inner::Strong(strong) => Some(strong.get(scope)),
        }
    }

    /// Whether the value is gone (`v8::TracedReference::IsEmpty`).
    pub fn is_empty(&self) -> bool {
        match &self.inner {
            Inner::Weak(inner) => inner.is_empty(),
            Inner::Strong(_) => false,
        }
    }
}

impl<T> fmt::Debug for TracedReference<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TracedReference")
            .field("empty", &self.is_empty())
            .finish()
    }
}

impl<T> crate::cppgc::Traced for TracedReference<T> {
    /// Trace **nothing**, which is the whole contract of a weak reference: a
    /// `Visitor` walking a host's structures must not reach the value through
    /// this handle, or the handle would be strong. The crate we stand in for's
    /// `TracedReference` is the same shape for the same reason — its traced-ness
    /// is about the *handle* living in a managed object, not about keeping the
    /// value alive.
    fn trace(&self, _visitor: &mut crate::cppgc::Visitor) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Context, Number, Value};

    /// The reading both types share: a live value answers, and releasing the
    /// handle is what makes it empty. The collector-driven emptying is the
    /// engine's own tests (`crux`'s three and `runtime`'s one), so what is pinned
    /// here is the bridge's reading of it.
    #[test]
    fn a_weak_handle_reads_its_value_and_releases_it() {
        crate::test_support::in_context!(scope, {
            let value = Local::<Value>::from_engine(api::Local::string("weakly held"));
            let mut weak = Weak::<Value>::new(scope, value);
            assert!(!weak.is_empty(), "the value is there to begin with");
            assert!(weak.to_local(scope).is_some());

            weak.clear();
            assert!(weak.is_empty(), "a released handle answers nothing");
            assert!(weak.to_local(scope).is_none());
        });
    }

    /// A traced reference over a value the host also holds reads it back, and one
    /// over a realm — a payload this bridge cannot watch — answers while it lives
    /// rather than pretending to be weak.
    #[test]
    fn a_traced_reference_reads_a_held_value_and_a_realm() {
        crate::test_support::in_context!(scope, {
            let held = api::Global::new(api::Local::number(7.0));
            let reference = TracedReference::<Number>::new(scope, Local::from_engine(held.get()));
            assert!(!reference.is_empty());
            assert_eq!(reference.get(scope).map(|local| local.value()), Some(7.0));

            let context = scope.get_current_context();
            let traced = TracedReference::<Context>::new(scope, context);
            assert!(!traced.is_empty());
            assert!(traced.get(scope).is_some(), "a realm has no box to lose");
        });
    }

    /// `with_finalizer` is the shape the node_sqlite and webgpu state uses: it
    /// builds, and the callback it is handed is registered with the engine rather
    /// than dropped.
    #[test]
    fn a_finalizer_can_be_registered_over_a_value() {
        crate::test_support::in_context!(scope, {
            let value = Local::<Value>::from_engine(api::Local::string("watched"));
            let weak = Weak::<Value>::with_finalizer(scope, value, Box::new(|_| {}));
            assert!(!weak.is_empty());
            let called = std::rc::Rc::new(std::cell::Cell::new(false));
            let flag = std::rc::Rc::clone(&called);
            let other = Local::<Value>::from_engine(api::Local::string("also watched"));
            let watched =
                Weak::<Value>::with_finalizer(scope, other, Box::new(move |_| flag.set(true)));
            assert!(!watched.is_empty());
            assert!(!called.get(), "the callback waits for the collector");
        });
    }
}
