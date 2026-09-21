//! Promises: their state, the events a rejection callback sees, and the calls a
//! host makes into one (v8::Promise, v8::PromiseState, v8::PromiseRejectEvent,
//! v8::Promise::Resolver).

use runtime::api;

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

impl<'s> Local<'s, PromiseResolver> {
    /// Resolve the promise the resolver came from
    /// (v8::Promise::Resolver::Resolve).
    ///
    /// The answer is whether this call settled the promise: a resolver that had
    /// already fired — its pair's call, or an earlier one — answers `false`, as
    /// the crate we stand in for's does.
    pub fn resolve(&self, scope: &PinScope<'_, '_>, value: Local<'_, Value>) -> Option<bool> {
        self.settle(scope, value)
    }

    /// Reject the promise the resolver came from
    /// (v8::Promise::Resolver::Reject), with the same answer as
    /// [`resolve`](Self::resolve).
    pub fn reject(&self, scope: &PinScope<'_, '_>, value: Local<'_, Value>) -> Option<bool> {
        self.settle(scope, value)
    }

    /// Call the resolving function, which is what settles the promise: the
    /// engine keeps both functions of a capability in one record whose
    /// `[[AlreadyResolved]]` flag the call sets.
    fn settle(&self, scope: &PinScope<'_, '_>, value: Local<'_, Value>) -> Option<bool> {
        let fired_before = self.already_resolved()?;
        let realm = crate::realm_of(scope);
        let args = [value.into_engine()];
        realm
            .call(self.engine(), &api::Local::undefined(), &args)
            .to_local()?;
        Some(!fired_before)
    }

    /// The `[[AlreadyResolved]]` flag the resolver pair shares.
    fn already_resolved(&self) -> Option<bool> {
        let id = self.engine().value().as_object()?.id();
        crate::realm::with_agent(|agent| {
            agent
                .promise_resolvers
                .get(&id)
                .map(|data| data.borrow().already_resolved.get())
        })
        .flatten()
    }
}
