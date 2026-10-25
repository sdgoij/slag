//! The JS stack-exhaustion guard: deep (or runaway) JS recursion must surface
//! as a catchable `RangeError` instead of overflowing the native stack and
//! killing the whole process.
//!
//! A non-tail JS->JS call nests native frames (the interpreter's `run_inner`
//! recursion, or the JIT's `run_jit_body`), and in an unoptimized build each
//! level costs ~160 KB of native stack (`eval.rs::run_deep` documents the
//! number), so a handful of levels exhaust the default 1 MiB main-thread
//! stack. The guard turns that into a JS-visible error: every body activation
//! entry compares the current stack pointer against a watermark near the
//! thread's stack bottom and refuses to start an activation that would descend
//! into the reserved margin.
//!
//! The watermark is `low + STACK_GUARD_RESERVE`. The check runs at every
//! activation entry, so the stack can never descend below the watermark by
//! more than one activation's own non-calling frame depth; the reserve only
//! needs to exceed that static depth (not the per-level cost), so the guard
//! does not meaningfully shrink the recursion depth reachable before the old
//! crash point — it converts the crash into a catchable error at the same
//! budget. The reserve is larger in debug builds, whose frames are far deeper.

use crux::error::{ErrorKind, JsError};

/// The bottom margin the guard keeps unused (see the module docs). Debug
/// frames are much deeper than release frames (the interpreter's per-level
/// cost is ~160 KB unoptimized vs ~10 KB optimized), so the reserve scales
/// with the profile.
#[cfg(debug_assertions)]
const STACK_GUARD_RESERVE: usize = 256 * 1024;
#[cfg(not(debug_assertions))]
const STACK_GUARD_RESERVE: usize = 128 * 1024;

/// The current thread's reserved stack bounds as `(low, high)` — the
/// addresses between which the stack may grow (it grows downward from
/// `high`). `None` when the platform cannot report them (the guard stays
/// disabled then). The implementation lives in `stackprobe`, which needs the
/// same answer.
fn stack_bounds() -> Option<(usize, usize)> {
    crux::stack::stack_bounds()
}

/// The current stack pointer, sampled as the address of a local. The local
/// must be address-taken so the compiler keeps it on the stack; its exact
/// slot is irrelevant because only the *relative* position against the
/// watermark matters and every sample is taken the same way.
#[inline]
fn current_sp() -> usize {
    let probe = 0u8;
    (&probe as *const u8) as usize
}

/// The watermark below which a JS activation must not start: `low +
/// STACK_GUARD_RESERVE`. `None` when the platform cannot report the thread's
/// stack bounds (the guard stays disabled on those platforms).
pub(crate) fn stack_guard_limit() -> Option<usize> {
    let (low, high) = stack_bounds()?;
    let limit = low.checked_add(STACK_GUARD_RESERVE)?;
    // A stack smaller than the reserve (a pathological embedder thread)
    // cannot host the guard's margin; leave it disabled rather than failing
    // every activation.
    (limit < high).then_some(limit)
}

thread_local! {
    /// This thread's watermark, sampled once: the stack's geometry is fixed for
    /// the thread's life and sampling it is a syscall. `0` means the platform
    /// could not report it, so the guard is off.
    static THREAD_GUARD_LIMIT: std::cell::OnceCell<usize> =
        const { std::cell::OnceCell::new() };
}

/// This thread's watermark. Sampled at first use rather than at agent
/// creation: an isolate may be entered on any thread (a `SharedIsolate`), so
/// the watermark belongs to the thread running the activation, not to the one
/// that built the agent.
fn thread_guard_limit() -> usize {
    THREAD_GUARD_LIMIT.with(|cell| *cell.get_or_init(|| stack_guard_limit().unwrap_or(0)))
}

pub fn install_probe() {
    crux::stack::install_probe(has_headroom);
}

/// Whether a Rust-level recursion step still has native stack: the probe crux
/// consults ([`crux::stack`]), because the paths that recurse off the JS
/// activation path — trap-less proxy forwarding, the parser's descent — never
/// pass [`enter_js`].
///
/// Kept out of line for the same reason as [`enter_js`]: the probe must measure
/// a frame of its own rather than be folded into the caller's frame.
#[inline(never)]
pub(crate) fn has_headroom() -> bool {
    let limit = thread_guard_limit();
    limit == 0 || current_sp() >= limit
}

/// The per-activation-entry check: refuse to start a new JS activation when
/// the native stack has descended to within [`STACK_GUARD_RESERVE`] of the
/// thread's reserved bottom, surfacing a catchable `RangeError` instead of
/// overflowing the stack.
///
/// Kept out of line: inlining a stack-probe local into the interpreter's
/// giant dispatch frame measurably reshapes it, so the check stays in its
/// own small frame.
#[inline(never)]
pub(crate) fn enter_js() -> Result<(), JsError> {
    let limit = thread_guard_limit();
    if limit != 0 && current_sp() < limit {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "Maximum call stack size exceeded".into(),
        ));
    }
    Ok(())
}
