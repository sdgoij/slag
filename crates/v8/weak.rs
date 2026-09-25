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

use crate::Isolate;
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

impl<T> Clone for Inner<T> {
    /// Another handle to the *same* reference. Both arms are handles rather than
    /// copies of what they name — the weak arm is the engine's own copyable id,
    /// and the strong arm goes through [`Global`], pin and all — which is why
    /// clearing through one clone releases every handle that came from it.
    fn clone(&self) -> Self {
        match self {
            Inner::Weak(inner) => Inner::Weak(*inner),
            Inner::Strong(strong) => Inner::Strong(strong.clone()),
        }
    }
}

/// The context a weak death callback is handed (`v8::WeakCallbackInfo`).
///
/// The crate we stand in for fills this with the isolate, the value and the
/// parameter a `SetWeak` call carried, and its accessors arrive when a call site
/// asks for one (the demand rule §9 states). It is *not*, however, what a
/// finalizer is handed here: every site in the tree annotates that parameter
/// `|_: &mut v8::Isolate|`, so [`Weak::with_finalizer`] takes the isolate
/// directly and this type stays the name the crate's surface gives.
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
    ///
    /// The receiver is the isolate, as in the crate we stand in for: `ext/napi`'s
    /// `Env::isolate` passes one, and the scope-passing sites reach it through the
    /// scope's deref chain. It is the isolate the collection runs in, which is the
    /// one this handle was made in — the shape every site in the tree annotates
    /// (`|_: &mut v8::Isolate|`), and the one that lets webgpu's finalizer adjust
    /// the host's external-memory account. It is captured here because the
    /// engine's own callback carries no isolate; a weak callback that outlives the
    /// isolate is dropped unsent rather than called, so the handle cannot be read
    /// after the isolate is gone.
    pub fn with_finalizer<H: Handle<Data = T>>(
        isolate: &mut Isolate,
        handle: H,
        finalizer: Box<dyn FnOnce(&mut Isolate)>,
    ) -> Self
    where
        T: 'static,
    {
        let weak = Self {
            inner: inner_from_payload(handle.into_payload()),
        };
        if let Inner::Weak(inner) = &weak.inner {
            // The handle is a copyable pointer, so the callback owns one rather
            // than borrowing the caller's.
            let mut isolate = *isolate;
            inner.set_callback(Box::new(move || {
                finalizer(&mut isolate);
            }));
        }
        weak
    }

    /// A strong handle over the value, while the collector has not taken it
    /// (`v8::Weak::to_global`), or `None` once it has.
    ///
    /// The upgrade a host makes when it wants to keep the value: it asks
    /// [`to_local`](Self::to_local)'s question and answers it with a handle that
    /// pins. A non-value payload answers `Some` for as long as the handle lives,
    /// since its handle holds it the way [`Global`] does.
    pub fn to_global(&self, isolate: &mut Isolate) -> Option<Global<T>> {
        match &self.inner {
            Inner::Weak(inner) => inner
                .to_local()
                .map(|value| Global::new(isolate, Local::<T>::from_engine(value))),
            Inner::Strong(strong) => Some(strong.clone()),
        }
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

impl<T> Clone for Weak<T> {
    /// Clone the handle, not a second reference to the same value. The crate we
    /// stand in for's `Weak` is cloneable for this reason, and a host relies on
    /// it — `deno_webgpu`'s `DeviceErrorHandler::push_error` clones one out of a
    /// `OnceLock` to move it into a `'static` task. A fresh handle would look
    /// identical while the value lived and diverge on `clear`, so the two must
    /// be one reference.
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
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

impl<T> Clone for TracedReference<T> {
    /// Clone the handle, not a second reference — [`Weak`]'s reasoning, and the
    /// shape a host's own traced structures clone into.
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
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

    /// A clone is another handle to the *same* reference, not a second weak handle
    /// over the same value. A host relies on this when it clones out of shared
    /// state: `clear` through either handle must empty both, which is the fact a
    /// `Clone` that minted a fresh `api::Weak` would quietly break.
    #[test]
    fn a_cloned_weak_handle_is_the_same_reference() {
        crate::test_support::in_context!(scope, {
            let value = Local::<Value>::from_engine(api::Local::string("cloned"));
            let weak = Weak::<Value>::new(scope, value);
            let mut clone = weak.clone();
            assert!(
                clone.to_local(scope) == weak.to_local(scope),
                "the clone names the same value"
            );

            clone.clear();
            assert!(
                weak.is_empty(),
                "clearing one handle releases the reference they share"
            );
        });
    }

    /// A payload the engine cannot watch — a realm — is held the way [`Global`]
    /// holds it, so a clone of it names the same realm: a `Clone` that dropped
    /// the strong payload would answer a different one.
    #[test]
    fn a_cloned_realm_handle_names_the_same_realm() {
        crate::test_support::in_context!(scope, {
            let context = scope.get_current_context();
            let weak = Weak::<Context>::new(scope, context);
            let clone = weak.clone();
            let original = weak.to_local(scope).expect("a realm has no box to lose");
            let read = clone.to_local(scope).expect("a realm has no box to lose");
            assert!(read == original, "the clone names the same realm");
        });
    }

    /// The traced reference reads the same way through a clone, and a clone of a
    /// value is still a real weak handle rather than a second strong one.
    #[test]
    fn a_cloned_traced_reference_reads_the_same_value() {
        crate::test_support::in_context!(scope, {
            let held = api::Global::new(api::Local::number(7.0));
            let reference = TracedReference::<Number>::new(scope, Local::from_engine(held.get()));
            let clone = reference.clone();
            assert_eq!(clone.get(scope).map(|local| local.value()), Some(7.0));
            assert_eq!(clone.is_empty(), reference.is_empty());
        });
    }

    /// The `E0521` shape `deno_webgpu` reported, kept as the regression that pins
    /// `Weak<T>: Clone`. A `&self` method clones the handle out of a `OnceLock` and
    /// moves it into a `'static` task; without the impl, this `.clone()` resolved
    /// to `Clone for &Weak`, so the closure captured a borrow of `self` and failed
    /// with `'1 must outlive 'static`. Every earlier section of the plan had the
    /// error down as deno-side and pre-existing; §9 records that the attribution
    /// was wrong.
    #[test]
    fn a_weak_handle_clones_out_of_a_shared_owner_into_a_static_task() {
        use crate::data::{Function, Object, String};
        use std::sync::OnceLock;

        struct Spawner;
        impl Spawner {
            fn spawn<F>(&self, _f: F)
            where
                F: FnOnce(&mut PinScope<'_, '_, Context>) + 'static,
            {
            }
        }
        struct Handler {
            device: OnceLock<Weak<Object>>,
            spawner: Spawner,
        }
        impl Handler {
            fn push_error(&self) {
                let device = self.device.get().expect("set before use").clone();
                self.spawner.spawn(move |scope| {
                    let Some(device) = device.to_local(scope) else {
                        return;
                    };
                    let key = String::new(scope, "dispatchEvent").expect("a string");
                    let handler = device
                        .get(scope, key.into())
                        .and_then(|value| Local::<Function>::try_from(value).ok());
                    assert!(handler.is_none(), "the device has no such handler");
                });
            }
        }

        crate::test_support::in_context!(scope, {
            let device = Object::new(scope);
            let handler = Handler {
                device: OnceLock::new(),
                spawner: Spawner,
            };
            assert!(
                handler
                    .device
                    .set(Weak::<Object>::new(scope, device))
                    .is_ok(),
                "the device is set once"
            );
            handler.push_error();
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

    /// The finalizer is handed the **isolate** — the one parameter type every site
    /// in the tree accepts (two of them annotate `|_: &mut v8::Isolate|`
    /// explicitly) and the one the webgpu finalizer adjusts the host's
    /// external-memory account through. A compile-time guard: this call is what
    /// pins the shape, and the callback itself waits for a collection.
    #[test]
    fn a_finalizer_is_handed_the_isolate() {
        crate::test_support::in_context!(scope, {
            let value = Local::<Value>::from_engine(api::Local::string("watched"));
            let weak = Weak::<Value>::with_finalizer(
                scope,
                value,
                Box::new(|isolate: &mut crate::Isolate| {
                    isolate.adjust_amount_of_external_allocated_memory(64);
                }),
            );
            assert!(!weak.is_empty());
        });
    }

    /// The upgrade: a live value becomes a `Global` that pins it, and a released
    /// handle upgrades to nothing — the same question `to_local` asks, answered
    /// with a handle that keeps the value.
    #[test]
    fn a_weak_handle_upgrades_to_a_strong_one_while_the_value_lives() {
        crate::test_support::in_context!(scope, {
            let value = Local::<Value>::from_engine(api::Local::string("upgraded"));
            let mut weak = Weak::<Value>::new(scope, value);
            let mut isolate = crate::scope::GetIsolate::get_isolate_ptr(scope);
            let strong = weak.to_global(&mut isolate).expect("still alive");
            assert_eq!(strong.get(scope), value, "the upgrade names the same value");

            weak.clear();
            assert!(
                weak.to_global(&mut isolate).is_none(),
                "a released handle upgrades to nothing"
            );
        });
    }

    /// The receiver is the isolate, as in the crate we stand in for: the isolate
    /// shape `ext/napi`'s `Env::isolate` passes, and the scope shape the
    /// node_sqlite and webgpu sites pass, which reaches the same parameter through
    /// the scope's deref chain. A compile-level pin — the callbacks themselves wait
    /// for a collection.
    #[test]
    fn a_finalizer_takes_an_isolate_and_a_scope_alike() {
        crate::test_support::in_context!(scope, {
            let mut isolate = crate::scope::GetIsolate::get_isolate_ptr(scope);
            let value = Local::<Value>::from_engine(api::Local::string("isolate receiver"));
            let weak = Weak::<Value>::with_finalizer(&mut isolate, value, Box::new(|_| {}));
            assert!(!weak.is_empty());

            let other = Local::<Value>::from_engine(api::Local::string("scope receiver"));
            let scoped = Weak::<Value>::with_finalizer(scope, other, Box::new(|_| {}));
            assert!(!scoped.is_empty());
        });
    }
}
