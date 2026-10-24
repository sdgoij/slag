//! The native-stack probe.
//!
//! Several paths recurse on the native stack without passing a JS activation's
//! guard: a trap-less proxy forwards by re-entering the internal method from
//! Rust, and the parser is a recursive-descent one. A fixed nesting count
//! cannot bound those, because it says nothing about what a frame costs — the
//! same count that is safe in a release build overflows a debug build's stack,
//! and a process that dies of `STATUS_STACK_OVERFLOW` cannot be caught or
//! reported.
//!
//! So the bound is the host's: the runtime installs a probe that compares the
//! current stack pointer against the thread's watermark, the same one that turns
//! deep JS recursion into a `RangeError`. Crates that recurse consult
//! [`has_headroom`] before descending and reject the input when it returns
//! `false`.
//!
//! Without a probe (a crate-only test, or an embedder that never installs one)
//! [`has_headroom`] answers `true`; each caller keeps its own count as the
//! fallback bound.

/// How the host answers "is there native stack left for another recursion step?".
type StackProbe = fn() -> bool;

static STACK_PROBE: std::sync::OnceLock<StackProbe> = std::sync::OnceLock::new();

/// Install the host's stack probe (the runtime does this once at startup, with
/// the other crux hooks).
pub fn install_probe(probe: StackProbe) {
    let _ = STACK_PROBE.set(probe);
}

/// Whether the host installed a probe at all. A caller that has a fallback of
/// its own uses it only when this is `false`: the host's probe measures the real
/// stack, while a fallback can only guess.
#[inline]
pub fn probe_installed() -> bool {
    STACK_PROBE.get().is_some()
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
