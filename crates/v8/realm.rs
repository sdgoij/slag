//! The realm currently entered on this thread.
//!
//! The engine tracks its own current context per isolate, but a host's
//! operation sometimes has no scope to read it from: `v8::Array::Length` takes
//! no scope at all, while a Slag property read needs the realm. This is the
//! bridge's copy, so those methods keep the signature the crate we stand in for
//! has.
//!
//! It is a single slot rather than a stack, because a host enters contexts
//! through [`ContextScope`](crate::ContextScope), which saves and restores the
//! slot around its own lifetime — so nesting behaves correctly, and the
//! innermost entered context is the one a scope-less call sees.

use std::cell::RefCell;

use runtime::Agent;
use runtime::api;

thread_local! {
    static CURRENT: RefCell<Option<api::Context>> = const { RefCell::new(None) };
}

/// The realm entered on this thread, if any.
pub(crate) fn current() -> Option<api::Context> {
    CURRENT.with(|current| *current.borrow())
}

/// Set the entered realm, returning what was there before.
pub(crate) fn enter(realm: api::Context) -> Option<api::Context> {
    CURRENT.with(|current| current.borrow_mut().replace(realm))
}

/// Restore what [`enter`] displaced.
pub(crate) fn restore(previous: Option<api::Context>) {
    CURRENT.with(|current| *current.borrow_mut() = previous);
}

/// Run `question` against the agent of the realm entered on this thread. With
/// no realm entered there is no agent to ask, and the answer is `None`.
///
/// This is the *unattributed* form, for an operation with no handle to take a
/// realm from (an object reference held as a raw `JsObject`, say). It answers
/// for the innermost entered context, which is ambiguous when two isolates are
/// entered at once — an operation that has a handle should use
/// [`with_agent_for`] with [`crate::handle::LocalHandle::minted_realm`]
/// instead.
pub(crate) fn with_agent<T>(question: impl FnOnce(&mut Agent) -> T) -> Option<T> {
    current().map(|realm| realm.with_agent(question))
}

/// Run `question` against the agent of `realm`, or of the innermost entered
/// realm when the handle recorded none (it was minted before any context was
/// entered, so it cannot be attributed).
pub(crate) fn with_agent_for<T>(
    realm: Option<api::Context>,
    question: impl FnOnce(&mut Agent) -> T,
) -> Option<T> {
    realm
        .or_else(current)
        .map(|realm| realm.with_agent(question))
}
