//! Promises: their state, and the events a rejection callback sees
//! (v8::Promise, v8::PromiseState, v8::PromiseRejectEvent).

use runtime::api;

use crate::data::Promise;
use crate::handle::Local;

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
}
