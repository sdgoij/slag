//! Executed-step census for the interpreter.
//!
//! The interpreter's cost is `steps executed x cost per step`. The dispatch
//! loop's cost per step has been measured and driven down; the step COUNT has
//! not, and a workload that runs two extra stack steps per operation looks the
//! same as one that runs an expensive step when only wall time is measured.
//! This counter separates the two: join it with time from a default build to
//! get `ns per step` for a row.
//!
//! It is a cargo feature rather than an environment switch because the
//! increment sits in the hottest loop of the engine. An atomic per step would
//! dominate the time it exists to explain, so it is compiled in only for a
//! counting run (`--features step-stats`) and disappears entirely otherwise.

#[cfg(feature = "step-stats")]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "step-stats")]
static STEPS: AtomicU64 = AtomicU64::new(0);

/// Record one dispatched interpreter step.
#[inline(always)]
pub fn count_step() {
    #[cfg(feature = "step-stats")]
    STEPS.fetch_add(1, Ordering::Relaxed);
}

/// Steps dispatched since process start (always 0 when the census is off).
#[inline]
pub fn steps() -> u64 {
    #[cfg(feature = "step-stats")]
    {
        STEPS.load(Ordering::Relaxed)
    }
    #[cfg(not(feature = "step-stats"))]
    {
        0
    }
}

/// Whether the census was compiled in.
pub const fn enabled() -> bool {
    cfg!(feature = "step-stats")
}
