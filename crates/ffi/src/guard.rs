//! Panic containment at the C boundary: unwinding across `extern "C"` is
//! undefined behaviour, so every exported function runs inside [`guard`].

/// Run `f`, converting a panic into `R::default()` with a stderr note.
/// Every FFI entry point must wrap its body in this.
pub fn guard<R: Default>(f: impl FnOnce() -> R) -> R {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("unknown panic");
            eprintln!("slag ffi: panic at the C boundary: {message}");
            R::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_contains_panics() {
        assert_eq!(guard(|| 1_u8 + 1), 2);
        assert_eq!(guard(|| -> u8 { panic!("boom") }), 0);
    }
}
