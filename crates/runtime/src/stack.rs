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
/// disabled then).
fn stack_bounds() -> Option<(usize, usize)> {
    #[cfg(target_os = "windows")]
    {
        #[link(name = "Kernel32")]
        unsafe extern "system" {
            fn GetCurrentThreadStackLimits(low_limit: *mut usize, high_limit: *mut usize) -> i32;
        }
        let mut low = 0usize;
        let mut high = 0usize;
        // SAFETY: both pointers reference writable locals the callee does
        // not retain.
        let ok = unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
        if ok != 0 && low != 0 && high > low {
            Some((low, high))
        } else {
            None
        }
    }
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    {
        // SAFETY: `pthread_getattr_np` fills `attr` for the calling thread
        // and `pthread_attr_getstack` reads it back; both operate on locals.
        unsafe {
            let mut attr = std::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
            if libc::pthread_getattr_np(libc::pthread_self(), attr.as_mut_ptr()) != 0 {
                None
            } else {
                let mut attr = attr.assume_init();
                let mut stackaddr: *mut libc::c_void = std::ptr::null_mut();
                let mut stacksize: libc::size_t = 0;
                let ok =
                    libc::pthread_attr_getstack(&mut attr, &mut stackaddr, &mut stacksize) == 0;
                let _ = libc::pthread_attr_destroy(&mut attr);
                if ok && !stackaddr.is_null() && stacksize > 0 {
                    let low = stackaddr as usize;
                    Some((low, low + stacksize))
                } else {
                    None
                }
            }
        }
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        // SAFETY: both functions return the calling thread's own stack
        // geometry.
        unsafe {
            let high = libc::pthread_get_stackaddr_np(libc::pthread_self()) as usize;
            let size = libc::pthread_get_stacksize_np(libc::pthread_self());
            if high != 0 && size > 0 {
                Some((high - size, high))
            } else {
                None
            }
        }
    }
    #[cfg(not(any(
        target_os = "windows",
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "macos",
        target_os = "ios"
    )))]
    {
        None
    }
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

/// The per-activation-entry check: refuse to start a new JS activation when
/// the native stack has descended to within [`STACK_GUARD_RESERVE`] of the
/// thread's reserved bottom, surfacing a catchable `RangeError` instead of
/// overflowing the stack.
///
/// Kept out of line: inlining a stack-probe local into the interpreter's
/// giant dispatch frame measurably reshapes it, so the check stays in its
/// own small frame.
#[inline(never)]
pub(crate) fn enter_js(agent: &crate::agent::Agent) -> Result<(), JsError> {
    let limit = agent.stack_guard_limit;
    if limit != 0 && current_sp() < limit {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "Maximum call stack size exceeded".into(),
        ));
    }
    Ok(())
}
