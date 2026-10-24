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
//! So the bound is the stack's: the probe compares the current stack pointer
//! against the thread's reserved bottom plus a margin. A host may install its
//! own probe (the runtime does, because it also turns deep JS recursion into a
//! `RangeError`). Without one, the probe asks the operating system for the
//! thread's stack bounds; where the platform cannot report them, the answer is
//! `true` and each caller keeps its own nesting count as the only bound.
//!
//! The bounds are asked for rather than assumed: a fixed "this parse may use N
//! bytes" budget cannot work, because what is available depends on how deep the
//! caller already is. A parse that assumes a megabyte has one on Windows only
//! when it starts at the top of the thread.
//!
//! This is its own crate because the crates that need it are otherwise
//! unrelated — `crux` (the value model), `parser` and `regexp` — and none of
//! them should have to depend on another just for this.

/// How the host answers "is there native stack left for another recursion step?".
type StackProbe = fn() -> bool;

static STACK_PROBE: std::sync::OnceLock<StackProbe> = std::sync::OnceLock::new();

/// Install the host's stack probe (the runtime does this once at startup, with
/// its other hooks). A host probe takes precedence over the operating system's
/// answer.
pub fn install_probe(probe: StackProbe) {
    let _ = STACK_PROBE.set(probe);
}

/// Whether a host installed a probe, so a caller can tell which answer it is
/// getting.
#[inline]
pub fn probe_installed() -> bool {
    STACK_PROBE.get().is_some()
}

/// The bottom margin the probe keeps unused. A recursion step that starts below
/// this has no room left for the frames it will build before it checks again,
/// and debug frames are much deeper than release frames, so the margin scales
/// with the profile.
#[cfg(debug_assertions)]
const STACK_RESERVE: usize = 256 * 1024;
#[cfg(not(debug_assertions))]
const STACK_RESERVE: usize = 128 * 1024;

/// The current thread's reserved stack bounds as `(low, high)` — the addresses
/// between which the stack may grow (it grows downward from `high`). `None`
/// when the platform cannot report them.
pub fn stack_bounds() -> Option<(usize, usize)> {
    #[cfg(target_os = "windows")]
    {
        #[link(name = "Kernel32")]
        unsafe extern "system" {
            fn GetCurrentThreadStackLimits(low_limit: *mut usize, high_limit: *mut usize) -> i32;
        }
        let mut low = 0usize;
        let mut high = 0usize;
        // SAFETY: both pointers reference writable locals the callee does not
        // retain.
        let ok = unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
        if ok != 0 && low != 0 && high > low {
            Some((low, high))
        } else {
            None
        }
    }
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    {
        // SAFETY: `pthread_getattr_np` fills `attr` for the calling thread and
        // `pthread_attr_getstack` reads it back; both operate on locals.
        unsafe {
            let mut attr = std::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
            if libc::pthread_getattr_np(libc::pthread_self(), attr.as_mut_ptr()) != 0 {
                return None;
            }
            let mut attr = attr.assume_init();
            let mut address: *mut libc::c_void = std::ptr::null_mut();
            let mut size: libc::size_t = 0;
            let ok = libc::pthread_attr_getstack(&attr, &mut address, &mut size) == 0;
            let _ = libc::pthread_attr_destroy(&mut attr);
            if ok && !address.is_null() && size > 0 {
                let low = address as usize;
                Some((low, low + size))
            } else {
                None
            }
        }
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        // SAFETY: both functions return the calling thread's own stack geometry.
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

/// The stack pointer, sampled as the address of a local. The local is
/// address-taken so the compiler keeps it on the stack; its exact slot is
/// irrelevant because only the relative position against the limit matters.
#[inline]
fn current_sp() -> usize {
    let probe = 0u8;
    (&probe as *const u8) as usize
}

thread_local! {
    /// This thread's refusal limit, sampled once: the stack's geometry is fixed
    /// for the thread's life and sampling it is a syscall. `0` means the
    /// platform could not report it, so only a caller's own count applies.
    static THREAD_LIMIT: std::cell::OnceCell<usize> = const { std::cell::OnceCell::new() };
}

/// Where this thread's recursion must stop: `low + STACK_RESERVE`, or `0` when
/// the platform cannot report the bounds (or the stack is smaller than the
/// margin, which makes the guard meaningless).
fn default_limit() -> usize {
    THREAD_LIMIT.with(|cell| {
        *cell.get_or_init(|| {
            let Some((low, high)) = stack_bounds() else {
                return 0;
            };
            match low.checked_add(STACK_RESERVE) {
                Some(limit) if limit < high => limit,
                _ => 0,
            }
        })
    })
}

/// Whether the calling thread has native stack left for another recursion step.
///
/// A host probe answers when one is installed. Otherwise the operating system's
/// stack bounds do, and on a platform that cannot report them the answer is
/// `true` — refusing every recursion would be worse than the risk the guard
/// removes, and the callers' own nesting counts still apply there.
///
/// Kept out of line so the sampled local is a frame of its own rather than
/// folded into a caller's.
#[inline(never)]
pub fn has_headroom() -> bool {
    match STACK_PROBE.get() {
        Some(probe) => probe(),
        None => {
            let limit = default_limit();
            limit == 0 || current_sp() >= limit
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fallback is the thread's real stack, not an assumed budget: a thread
    /// given a stack far smaller than the default still reports that it is out
    /// of room, before the recursion reaches the bottom.
    ///
    /// Only platforms that can report the bounds take part; elsewhere the probe
    /// answers `true` by design.
    #[test]
    fn the_default_probe_measures_a_small_thread() {
        if stack_bounds().is_none() {
            return;
        }
        let measured = std::thread::Builder::new()
            .stack_size(1 << 20)
            .spawn(|| {
                // 4 KiB of live locals per level, so a 1 MiB stack is a few
                // hundred levels deep and the margin is reached well before
                // the bottom.
                fn descend(level: usize) -> bool {
                    let pad = [0u8; 4096];
                    std::hint::black_box(&pad);
                    if !has_headroom() {
                        return true;
                    }
                    if level == 0 {
                        return false;
                    }
                    descend(level - 1)
                }
                descend(4096)
            })
            .expect("thread");
        assert!(
            measured.join().expect("no overflow"),
            "the probe must report no headroom before the stack ends"
        );
    }
}
