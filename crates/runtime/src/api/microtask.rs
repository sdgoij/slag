//! When the job queues drain (v8::MicrotasksPolicy).
//!
//! The engine drains a job only when a host asks — `Context::run_microtasks`,
//! or its own loops. `Auto` is the policy that drains *for* the host: after the
//! outermost embedder entry returns, the queues run down, which is what
//! `v8::Isolate` does by default. The entry depth a re-entrant call raises is
//! what keeps a callback that calls back in from draining mid-callback.

/// When the job queues drain (v8::MicrotasksPolicy).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub enum MicrotasksPolicy {
    /// Only when the host asks (v8::MicrotasksPolicy::kExplicit).
    Explicit = 0,
    /// After the outermost embedder entry returns (v8::MicrotasksPolicy::kAuto).
    Auto = 2,
}
