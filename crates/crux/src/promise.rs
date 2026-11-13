//! The promise records (spec 27.2): [[PromiseState]] with its reaction lists,
//! the PromiseReaction Record, the PromiseCapability Record, and the
//! [[AlreadyResolved]] slot.
//!
//! A promise's record is a payload of the promise object's own
//! [`crate::object::ObjectKind`] (`ObjectKind::Promise`), not an agent-side
//! table keyed by object identity: the collector traces the reactions with the
//! object that owns them, the record needs no reap when the object dies, and a
//! lookup is a match on the kind instead of a hash of the object id.

use std::cell::Cell;

use crate::heap::{GcAny, Trace};
use crate::job::JobCallback;
use crate::value::Value;

/// [[PromiseState]] plus the reaction lists (spec 27.2.1.3).
pub enum PromiseState {
    Pending {
        fulfill_reactions: Vec<PromiseReaction>,
        reject_reactions: Vec<PromiseReaction>,
    },
    Fulfilled(Value),
    Rejected(Value),
}

impl std::fmt::Debug for PromiseState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromiseState::Pending { .. } => f.write_str("pending"),
            PromiseState::Fulfilled(_) => f.write_str("fulfilled"),
            PromiseState::Rejected(_) => f.write_str("rejected"),
        }
    }
}

impl Trace for PromiseState {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        match self {
            PromiseState::Pending {
                fulfill_reactions,
                reject_reactions,
            } => {
                fulfill_reactions.trace(visit);
                reject_reactions.trace(visit);
            }
            PromiseState::Fulfilled(value) | PromiseState::Rejected(value) => value.trace(visit),
        }
    }
}

/// The Promise Record: the [[PromiseState]], the [[IsHandled]] flag, and the
/// promise the resolvers settle.
///
/// [[AlreadyResolved]] is *not* here: it belongs to a resolving-function
/// pair, and one promise can outlive several pairs (NewPromiseResolveThenableJob
/// makes a second one), so it lives on the flag box the pair shares
/// (`crate::function::ResolverFlag`).
#[derive(Debug)]
pub struct PromiseData {
    pub state: PromiseState,
    /// [[IsHandled]]: whether any reaction was attached (unhandled-rejection
    /// tracking).
    pub is_handled: Cell<bool>,
    /// The promise object these resolving functions settle. It names the
    /// object that owns this record, which is why a resolver dispatch needs no
    /// other lookup to reach it. Filled by `JsObject::promise_object_create`
    /// (the record and the object have to name each other).
    pub promise: Value,
}

impl PromiseData {
    /// A pending record whose promise back edge is not yet filled.
    pub fn pending() -> Self {
        Self {
            state: PromiseState::Pending {
                fulfill_reactions: Vec::new(),
                reject_reactions: Vec::new(),
            },
            is_handled: Cell::new(false),
            promise: Value::Undefined,
        }
    }
}

impl Trace for PromiseData {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.state.trace(visit);
        self.promise.trace(visit);
    }
}

/// A PromiseReaction Record (spec 27.2.1.5).
#[derive(Clone)]
pub struct PromiseReaction {
    /// The result capability of the `then` that created this reaction; `None`
    /// for internal reactions (async/await resumptions) that only run the
    /// handler.
    pub capability: Option<PromiseCapability>,
    /// [[Type]]: which reaction list the reaction came from.
    pub kind: ReactionKind,
    /// [[Handler]]; empty for non-callable arguments (the default behavior).
    pub handler: Option<JobCallback>,
}

impl std::fmt::Debug for PromiseReaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromiseReaction")
            .field("capability", &self.capability.is_some())
            .field("kind", &self.kind)
            .finish()
    }
}

impl Trace for PromiseReaction {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.capability.trace(visit);
        self.handler.trace(visit);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionKind {
    Fulfill,
    Reject,
}

/// A PromiseCapability Record (spec 27.2.1.1): the promise plus its
/// resolving functions.
#[derive(Debug, Clone)]
pub struct PromiseCapability {
    pub promise: Value,
    /// The `[[Resolve]]`/`[[Reject]]` functions. Both `undefined` for an
    /// INTERNAL capability: the realm's own `%Promise%` cannot hand a
    /// resolving function to user code (its executor is
    /// `GetCapabilitiesExecutor`, engine code), so an internal caller settles
    /// through `promise::settle_capability`/`promise::reject_capability`
    /// instead of building two closures it would only call once.
    pub resolve: Value,
    pub reject: Value,
}

impl Trace for PromiseCapability {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.promise.trace(visit);
        self.resolve.trace(visit);
        self.reject.trace(visit);
    }
}
