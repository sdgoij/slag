//! Promises: their state, the events a rejection callback sees, and the calls a
//! host makes into one (v8::Promise, v8::PromiseState, v8::PromiseRejectEvent,
//! v8::Promise::Resolver).

use std::cell::RefCell;

use runtime::api;
use runtime::promise::ResolverData;

use crate::data::{Function, Promise, PromiseResolver, Value};
use crate::handle::Local;
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

impl<'s> Local<'s, Promise> {
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
                Some(on_fulfilled.into_engine().into_value()),
                Some(on_rejected.into_engine().into_value()),
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

impl<'s> Local<'s, PromiseResolver> {
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
    use crate::data::Number;
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
