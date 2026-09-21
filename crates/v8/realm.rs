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
use std::rc::Rc;

use runtime::api;

thread_local! {
    static CURRENT: RefCell<Option<Rc<api::Context>>> = const { RefCell::new(None) };
}

/// The realm entered on this thread, if any.
pub(crate) fn current() -> Option<Rc<api::Context>> {
    CURRENT.with(|current| current.borrow().clone())
}

/// Set the entered realm, returning what was there before.
pub(crate) fn enter(realm: Rc<api::Context>) -> Option<Rc<api::Context>> {
    CURRENT.with(|current| current.borrow_mut().replace(realm))
}

/// Restore what [`enter`] displaced.
pub(crate) fn restore(previous: Option<Rc<api::Context>>) {
    CURRENT.with(|current| *current.borrow_mut() = previous);
}
