//! Host clocks. `std::time` panics on `wasm32-unknown-unknown` (its sys time
//! module is `unsupported`), so engine time reads go through this module. On
//! that target the embedding JS supplies two `env` imports:
//! `slag_host_now_ms` (milliseconds since the Unix epoch, e.g. `Date.now()`)
//! and `slag_host_now_monotonic_ms` (a monotonic milliseconds clock, e.g.
//! `performance.now()`); everywhere else std clocks back them.

const MS_TO_NS: u128 = 1_000_000;

#[cfg(not(all(
    target_arch = "wasm32",
    not(any(target_os = "wasi", target_os = "emscripten"))
)))]
fn host_epoch_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(not(all(
    target_arch = "wasm32",
    not(any(target_os = "wasi", target_os = "emscripten"))
)))]
fn host_monotonic_nanos() -> u128 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let start = *START.get_or_init(std::time::Instant::now);
    std::time::Instant::now().duration_since(start).as_nanos()
}

// Imported from the embedding JS (`env.slag_host_now_ms` /
// `env.slag_host_now_monotonic_ms`).
#[cfg(all(
    target_arch = "wasm32",
    not(any(target_os = "wasi", target_os = "emscripten"))
))]
#[link(wasm_import_module = "env")]
unsafe extern "C" {
    fn slag_host_now_ms() -> f64;
    fn slag_host_now_monotonic_ms() -> f64;
}

#[cfg(all(
    target_arch = "wasm32",
    not(any(target_os = "wasi", target_os = "emscripten"))
))]
fn host_epoch_nanos() -> u128 {
    // SAFETY: the imports are required and the embedder supplies them at
    // instantiation; a non-finite or pre-epoch value reads as epoch 0.
    let ms = unsafe { slag_host_now_ms() };
    if ms.is_finite() && ms >= 0.0 {
        (ms as u128).saturating_mul(MS_TO_NS)
    } else {
        0
    }
}

#[cfg(all(
    target_arch = "wasm32",
    not(any(target_os = "wasi", target_os = "emscripten"))
))]
fn host_monotonic_nanos() -> u128 {
    // SAFETY: the imports are required and the embedder supplies them at
    // instantiation; a non-finite value reads as 0.
    let ms = unsafe { slag_host_now_monotonic_ms() };
    if ms.is_finite() && ms >= 0.0 {
        (ms as u128).saturating_mul(MS_TO_NS)
    } else {
        0
    }
}

/// Milliseconds since the Unix epoch (`Date`'s time value, spec 21.4.1.1).
pub(crate) fn epoch_ms() -> f64 {
    (host_epoch_nanos() / MS_TO_NS) as f64
}

/// Nanoseconds since the Unix epoch (`Temporal`, PRNG seeding).
pub(crate) fn epoch_nanos() -> u128 {
    host_epoch_nanos()
}

/// Nanoseconds of a monotonic clock (timeout-job deadlines).
pub(crate) fn monotonic_nanos() -> u64 {
    host_monotonic_nanos() as u64
}
