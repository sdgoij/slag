//! Promises: their state, the events a rejection callback sees, and the calls a
//! host makes into one (v8::Promise, v8::PromiseState, v8::PromiseRejectEvent,
//! v8::Promise::Resolver).

use std::cell::RefCell;
use std::marker::PhantomData;

use runtime::api;
use runtime::promise::ResolverData;

use crate::data::{Function, Promise, PromiseResolver, Value};
use crate::handle::{Local, LocalHandle};
use crate::scope::PinScope;

/// A promise's state (v8::PromiseState).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromiseState {
    Pending,
    Fulfilled,
    Rejected,
}

/// Why a rejection callback ran (v8::PromiseRejectEvent).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromiseRejectEvent {
    /// A promise was rejected and nothing handled it before the microtask
    /// checkpoint.
    PromiseRejectWithNoHandler,
    /// A handler arrived for a rejection that was already reported.
    PromiseHandlerAddedAfterReject,
    /// A rejected promise was resolved again.
    PromiseRejectAfterResolved,
    /// A resolved promise was rejected again.
    PromiseResolveAfterResolved,
}

impl<'s> LocalHandle<'s, Promise> {
    /// The promise's state (v8::Promise::State).
    pub fn state(&self) -> PromiseState {
        let realm = crate::realm_current();
        match api::Promise::state(&realm, self.engine()) {
            Ok("pending") => PromiseState::Pending,
            Ok("fulfilled") => PromiseState::Fulfilled,
            Ok("rejected") => PromiseState::Rejected,
            // The handle is tagged as a promise and the realm is in reach, so
            // the engine refusing it is the bridge's own inconsistency.
            _ => panic!("bridge bug: a Promise handle the engine does not know"),
        }
    }

    /// The promise's result value (v8::Promise::Result).
    ///
    /// The crate we stand in for documents that the promise must have settled;
    /// what a pending one answers is unspecified, and here — as there — it is
    /// *undefined* rather than a failure.
    pub fn result<'a>(&self, scope: &PinScope<'a, '_>) -> Local<'a, Value> {
        let realm = crate::realm_of(scope);
        match api::Promise::result(&realm, self.engine()) {
            Ok(value) => Local::from_engine(value),
            Err(error) => {
                crate::throw(scope, &error);
                Local::from_engine(api::Local::undefined())
            }
        }
    }

    /// Mark the promise as handled (v8::Promise::MarkAsHandled): the
    /// bookkeeping a host does when it takes responsibility for a rejection it
    /// is not reacting to.
    pub fn mark_as_handled(&self) {
        let realm = crate::realm_current();
        realm.with_agent(|agent| {
            let Some(id) = self.engine().value().as_object().map(|object| object.id()) else {
                return;
            };
            if let Some(data) = agent.promises.get(&id) {
                data.borrow_mut().is_handled = true;
            }
        });
    }

    /// `promise.then(on_fulfilled, on_rejected)`, answering the derived promise
    /// (v8::Promise::Then2).
    ///
    /// This is the engine's own PerformPromiseThen rather than a call to a `then`
    /// property, which is what the crate we stand in for does: a script that
    /// replaced `Promise.prototype.then` cannot change what a host's call here
    /// does.
    pub fn then2<'a>(
        &self,
        scope: &PinScope<'a, '_>,
        on_fulfilled: Local<'_, Function>,
        on_rejected: Local<'_, Function>,
    ) -> Option<Local<'a, Promise>> {
        self.then_with(scope, Some(on_fulfilled), Some(on_rejected))
    }

    /// `promise.catch(on_rejected)` (v8::Promise::Catch).
    ///
    /// The fulfilling side is *undefined*, which is what the crate we stand in
    /// for passes V8 — a promise that fulfils runs no handler of its own through
    /// this call, so its result passes through.
    pub fn catch<'a>(
        &self,
        scope: &PinScope<'a, '_>,
        on_rejected: Local<'_, Function>,
    ) -> Option<Local<'a, Promise>> {
        self.then_with(scope, None, Some(on_rejected))
    }

    /// The engine's PerformPromiseThen with either side optional, which is how
    /// the engine takes it: a side the host did not give is *undefined*.
    fn then_with<'a>(
        &self,
        scope: &PinScope<'a, '_>,
        on_fulfilled: Option<Local<'_, Function>>,
        on_rejected: Option<Local<'_, Function>>,
    ) -> Option<Local<'a, Promise>> {
        let realm = crate::realm_of(scope);
        let promise = *self.engine().value();
        let constructor = realm.intrinsic("%Promise%")?;
        let capability =
            realm.with_agent(|agent| runtime::promise::new_promise_capability(agent, &constructor));
        let capability = match capability {
            Ok(capability) => capability,
            Err(error) => {
                crate::throw(scope, &error);
                return None;
            }
        };
        let performed = realm.with_agent(|agent| {
            runtime::promise::perform_promise_then(
                agent,
                &promise,
                on_fulfilled.map(|handler| handler.into_engine().into_value()),
                on_rejected.map(|handler| handler.into_engine().into_value()),
                Some(capability),
            )
        });
        match performed {
            Ok(value) => Some(Local::from_engine(api::Local::from(value))),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }
}

/// What a rejection callback is handed (v8::PromiseRejectMessage).
///
/// One deliberate difference from the crate we stand in for: there it wraps a
/// pointer to V8's own struct, and here it carries the engine's values, because
/// nothing crosses an ABI on the way to the callback. `get_promise`, `get_event`
/// and `get_value` answer the same three things either way.
///
/// The event is one of the four [`PromiseRejectEvent`] variants, but only two can
/// arrive: the engine reports a rejection that has no handler and a handler
/// arriving for one that was already reported, and has no event for the two
/// "already settled" cases.
pub struct PromiseRejectMessage<'msg> {
    isolate: crate::Isolate,
    promise: crux::value::Value,
    event: PromiseRejectEvent,
    value: Option<crux::value::Value>,
    marker: PhantomData<&'msg ()>,
}

impl<'msg> PromiseRejectMessage<'msg> {
    pub(crate) fn new(
        isolate: crate::Isolate,
        promise: crux::value::Value,
        event: PromiseRejectEvent,
        value: Option<crux::value::Value>,
    ) -> Self {
        Self {
            isolate,
            promise,
            event,
            value,
            marker: PhantomData,
        }
    }

    /// The isolate the rejection happened on, which is what a callback scope is
    /// opened from.
    pub(crate) fn isolate(&self) -> crate::Isolate {
        self.isolate
    }

    /// The rejected promise (v8::PromiseRejectMessage::GetPromise).
    pub fn get_promise(&self) -> Local<'msg, Promise> {
        Local::from_engine(api::Local::from(self.promise))
    }

    /// Why the callback ran (v8::PromiseRejectMessage::GetEvent).
    pub fn get_event(&self) -> PromiseRejectEvent {
        self.event
    }

    /// The rejection value, when the engine had one in hand
    /// (v8::PromiseRejectMessage::GetValue).
    pub fn get_value(&self) -> Option<Local<'msg, Value>> {
        self.value
            .map(|value| Local::from_engine(api::Local::from(value)))
    }
}

impl std::fmt::Debug for PromiseRejectMessage<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromiseRejectMessage")
            .field("event", &self.event)
            .finish_non_exhaustive()
    }
}

impl PromiseResolver {
    /// A resolver with a fresh promise in the pending state
    /// (v8::Promise::Resolver::New).
    ///
    /// A resolver *is* the capability's resolving function, which is what the
    /// bridge's own `resolve`/`reject` call: the engine keys the pair's
    /// `[[AlreadyResolved]]` flag by the function's identity.
    pub fn new<'a>(scope: &PinScope<'a, '_>) -> Option<Local<'a, PromiseResolver>> {
        let realm = crate::realm_of(scope);
        let constructor = realm.intrinsic("%Promise%")?;
        let capability =
            realm.with_agent(|agent| runtime::promise::new_promise_capability(agent, &constructor));
        match capability {
            Ok(capability) => {
                let resolve: Local<'a, PromiseResolver> =
                    Local::from_engine(api::Local::from(capability.resolve));
                // The engine's record for the pair names each function by
                // identity, so the rejecting one is kept here, where a handle
                // can name it back: a host holds only the resolve function.
                if let Some(id) = resolve.engine().value().as_function().map(|f| f.id()) {
                    scope
                        .isolate_ptr()
                        .add_resolver_reject(id, api::Local::from(capability.reject));
                }
                Some(resolve)
            }
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }
}

impl<'s> LocalHandle<'s, PromiseResolver> {
    /// The promise this resolver settles (v8::Promise::Resolver::GetPromise).
    pub fn get_promise<'a>(&self, _scope: &PinScope<'a, '_>) -> Local<'a, Promise> {
        let promise = self.resolver_data(|data| data.borrow().promise);
        match promise {
            Some(promise) => Local::from_engine(api::Local::from(promise)),
            // The record is keyed by the resolving function, so a handle that
            // names something else has no promise behind it. The crate answers
            // an empty handle there and its callers unwrap it; this says what
            // went wrong instead of handing back one.
            None => panic!("bridge: a PromiseResolver handle with no promise behind it"),
        }
    }

    /// Resolve the promise the resolver came from
    /// (v8::Promise::Resolver::Resolve).
    ///
    /// The answer is whether this call settled the promise: a resolver that had
    /// already fired — its pair's call, or an earlier one — answers `false`, as
    /// the crate we stand in for's does.
    pub fn resolve(&self, scope: &PinScope<'_, '_>, value: Local<'_, Value>) -> Option<bool> {
        self.settle(scope, value, false)
    }

    /// Reject the promise the resolver came from
    /// (v8::Promise::Resolver::Reject), with the same answer as
    /// [`resolve`](Self::resolve).
    pub fn reject(&self, scope: &PinScope<'_, '_>, value: Local<'_, Value>) -> Option<bool> {
        self.settle(scope, value, true)
    }

    /// Call the pair's resolving or rejecting function, which is what settles
    /// the promise: the engine keeps both functions of one capability in a single
    /// record whose `[[AlreadyResolved]]` flag the call sets.
    ///
    /// Both functions have to be called, and the engine hands back only the
    /// identifier it keyed a record by — so the rejecting one is the value the
    /// isolate kept for the pair when the resolver was made.
    fn settle(
        &self,
        scope: &PinScope<'_, '_>,
        value: Local<'_, Value>,
        reject: bool,
    ) -> Option<bool> {
        let fired_before = self.already_resolved()?;
        let function = if reject {
            self.reject_function()?
        } else {
            *self.engine()
        };
        let realm = crate::realm_of(scope);
        let args = [value.into_engine()];
        realm
            .call(&function, &api::Local::undefined(), &args)
            .to_local()?;
        Some(!fired_before)
    }

    /// The pair's rejecting function, which the isolate holds for the resolve
    /// function this handle names.
    fn reject_function(&self) -> Option<api::Local> {
        let id = self.engine().value().as_function()?.id();
        let realm = crate::realm::current()?;
        // SAFETY: as elsewhere: a realm lives in the agent of a live isolate, and
        // the engine isolate is the first field of `IsolateInner`.
        let isolate = unsafe { crate::Isolate::from_engine_ptr(realm.isolate()) };
        isolate.resolver_reject(id)
    }

    /// The `[[AlreadyResolved]]` flag the resolver pair shares.
    fn already_resolved(&self) -> Option<bool> {
        self.resolver_data(|data| data.borrow().already_resolved.get())
    }

    /// The engine's record for the resolving function this handle names: the one
    /// place the pair's promise and the flag they share live. The engine keys it
    /// by the *function's* identity — a function value has no object side here —
    /// so a handle that names anything else answers `None`.
    fn resolver_data<T>(&self, read: impl FnOnce(&RefCell<ResolverData>) -> T) -> Option<T> {
        let id = self.engine().value().as_function()?.id();
        crate::realm::with_agent(|agent| agent.promise_resolvers.get(&id).map(|data| read(data)))
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Function, Number};
    use crate::test_support::{eval, in_context};

    /// The number a promise settled with.
    fn settled_number(scope: &PinScope<'_, '_>, promise: Local<'_, Promise>) -> f64 {
        Local::<Number>::try_from(promise.result(scope))
            .expect("number")
            .value()
    }

    /// A resolver made by `new` carries its own promise, pending until the
    /// resolver fires — and a second call answers `false`, because the pair's
    /// flag is set by the first.
    #[test]
    fn a_resolver_settles_the_promise_it_made() {
        in_context!(scope, {
            let resolver = PromiseResolver::new(scope).expect("resolver");
            let promise = resolver.get_promise(scope);
            assert_eq!(promise.state(), PromiseState::Pending);

            let seven = eval(scope, "7");
            assert_eq!(resolver.resolve(scope, seven), Some(true));
            assert_eq!(promise.state(), PromiseState::Fulfilled);
            assert_eq!(settled_number(scope, promise), 7.0);

            assert_eq!(resolver.resolve(scope, seven), Some(false));
        });
    }

    /// `catch` sends a rejection to its handler. The fulfilling side is
    /// *undefined* there, so a rejection is the only way the handler can run —
    /// which is what tells this apart from `then2` with a fulfilling handler.
    #[test]
    fn catch_runs_the_rejection_handler() {
        in_context!(scope, {
            scope.set_microtasks_policy(crate::MicrotasksPolicy::Explicit);

            let promise = eval(scope, "Promise.reject(new Error('boom'))");
            let promise = Local::<Promise>::try_from(promise).expect("promise");
            let handler = eval(scope, "(function () { globalThis.caught = 7; })");
            let handler = Local::<Function>::try_from(handler).expect("function");

            assert!(promise.catch(scope, handler).is_some());
            assert_eq!(
                crate::test_support::eval_number(
                    scope,
                    "globalThis.caught === undefined ? -1 : globalThis.caught"
                ),
                -1.0,
                "nothing runs a queued reaction until a checkpoint"
            );

            scope.perform_microtask_checkpoint();
            assert_eq!(
                crate::test_support::eval_number(scope, "globalThis.caught"),
                7.0,
                "the rejection reached the handler"
            );
        });
    }

    /// Two resolvers are two promises: one settling says nothing about the
    /// other, which is what the record lookup in `resolver_data` is for.
    #[test]
    fn one_resolver_does_not_settle_anothers_promise() {
        in_context!(scope, {
            let first = PromiseResolver::new(scope).expect("resolver");
            let second = PromiseResolver::new(scope).expect("resolver");
            assert_ne!(first.get_promise(scope), second.get_promise(scope));

            let reason = eval(scope, "new Error('no')");
            assert_eq!(second.reject(scope, reason), Some(true));
            assert_eq!(second.get_promise(scope).state(), PromiseState::Rejected);
            assert_eq!(first.get_promise(scope).state(), PromiseState::Pending);
        });
    }
}
