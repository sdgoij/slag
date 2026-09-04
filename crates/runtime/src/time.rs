//! The wall clock. `std::time` panics on `wasm32-unknown-unknown` (its sys
//! time module is `unsupported`), so the engine's few epoch-time readers go
//! through this module. On that target the embedding JS must provide the
//! `env.slag_host_now_ms` import (milliseconds since the Unix epoch, e.g.
//! `Date.now()`); everywhere else `std::time::SystemTime` backs it.

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

// Imported from the embedding JS (`env.slag_host_now_ms`).
#[cfg(all(
    target_arch = "wasm32",
    not(any(target_os = "wasi", target_os = "emscripten"))
))]
#[link(wasm_import_module = "env")]
unsafe extern "C" {
    fn slag_host_now_ms() -> f64;
}

#[cfg(all(
    target_arch = "wasm32",
    not(any(target_os = "wasi", target_os = "emscripten"))
))]
fn host_epoch_nanos() -> u128 {
    // SAFETY: `slag_host_now_ms` is a required import the embedder supplies
    // at instantiation; a non-finite or pre-epoch value reads as epoch 0.
    let ms = unsafe { slag_host_now_ms() };
    if ms.is_finite() && ms >= 0.0 {
        (ms as u128).saturating_mul(1_000_000)
    } else {
        0
    }
}

/// Milliseconds since the Unix epoch (`Date`'s time value, spec 21.4.1.1).
pub(crate) fn epoch_ms() -> f64 {
    (host_epoch_nanos() / 1_000_000) as f64
}

/// Nanoseconds since the Unix epoch (`Temporal`, PRNG seeding).
pub(crate) fn epoch_nanos() -> u128 {
    host_epoch_nanos()
}
