//! The JobCallback Record (spec 9.5.1) and its GC edges.
//!
//! The record lives here rather than with the runtime's job queue because the
//! promise reaction lists own it: a promise's record is a payload of the
//! promise object's own [`crate::object::ObjectKind`], so everything it holds
//! has to be traceable from crux.

use crate::heap::{GcAny, Trace};
use crate::value::Value;

/// A JobCallback Record (spec 9.5.1): a function object to invoke when a Job
/// runs, plus a host-defined slot for propagating host context.
///
/// `host_defined` is an opaque word the host owns (its own callback record, a
/// context id, ...). The engine only carries it: it is not a GC reference, so it
/// is not traced, and it is handed back unchanged wherever the record is passed
/// to a host hook (`HostCallJobCallback`).
#[derive(Debug, Clone)]
pub struct JobCallback {
    pub callback: Value,
    pub host_defined: Option<usize>,
}

impl Trace for JobCallback {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.callback.trace(visit);
    }
}
