//! The native-stack probe.
//!
//! Several paths recurse on the native stack without passing a JS activation's
//! guard: a trap-less proxy forwards by re-entering the internal method from
//! Rust, the parser and the RegExp parser are recursive descents. A fixed
//! nesting count cannot bound those, because it says nothing about what a frame
//! costs — the same count that is safe in a release build overflows a debug
//! build's stack, and a process that dies of `STATUS_STACK_OVERFLOW` cannot be
//! caught or reported.
//!
//! So the bound is the host's: the runtime installs a probe that compares the
//! current stack pointer against the thread's watermark, the same one that turns
//! deep JS recursion into a `RangeError`. Crates that recurse consult
//! [`has_headroom`] before descending and reject the input when it returns
//! `false`.
//!
//! Without a probe (a crate-only test, or an embedder that never installs one)
//! [`has_headroom`] answers `true`, and each caller keeps its own count as the
//! fallback bound.
//!
//! This is its own crate because the crates that need it are otherwise
//! unrelated — `crux` (the value model), `parser` and `regexp` — and none of
//! them should have to depend on another just for this.

/// How the host answers "is there native stack left for another recursion step?".
type StackProbe = fn() -> bool;

static STACK_PROBE: std::sync::OnceLock<StackProbe> = std::sync::OnceLock::new();

/// Install the host's stack probe (the runtime does this once at startup, with
/// its other hooks).
pub fn install_probe(probe: StackProbe) {
    let _ = STACK_PROBE.set(probe);
}

/// Whether the calling thread has native stack left for another recursion step.
///
/// `true` when no probe is installed: the callers' own nesting counts are the
/// only bound available then, and refusing every recursion would be worse than
/// the risk this guard removes.
#[inline]
pub fn has_headroom() -> bool {
    match STACK_PROBE.get() {
        Some(probe) => probe(),
        None => true,
    }
}

/// Whether the host installed a probe at all. A caller that has a fallback of
/// its own uses it only when this is `false`: the host's probe measures the real
/// stack, while a fallback can only estimate.
#[inline]
pub fn probe_installed() -> bool {
    STACK_PROBE.get().is_some()
}
