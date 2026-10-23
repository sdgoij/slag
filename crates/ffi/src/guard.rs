//! Panic containment at the C boundary: unwinding across `extern "C"` is
//! undefined behaviour, so every exported function runs inside [`guard`].

use std::cell::Cell;

thread_local! {
    /// Set when a guarded call panicked on this thread. Every later guarded
    /// call returns `R::default()` without running: after a panic the isolate's
    /// state is unknown, and running more engine code over it risks the very
    /// aliasing the panic interrupted. A context is bound to one thread, so the
    /// thread identifies the suspect isolate.
    static POISONED: Cell<bool> = const { Cell::new(false) };
}

/// Whether an earlier guarded call on this thread panicked.
pub fn is_poisoned() -> bool {
    POISONED.with(Cell::get)
}

/// Clear the poison. A host that has rebuilt its state (or dropped and
/// recreated every context on the thread) calls this; the engine never resets
/// it on its own.
pub fn unpoison() {
    POISONED.with(|poisoned| poisoned.set(false));
}

/// Run `f`, converting a panic into `R::default()` with a stderr note and
/// poisoning later calls on this thread. Every FFI entry point must wrap its
/// body in this.
pub fn guard<R: Default>(f: impl FnOnce() -> R) -> R {
    if is_poisoned() {
        return R::default();
    }
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("unknown panic");
            eprintln!("slag ffi: panic at the C boundary: {message}");
            POISONED.with(|poisoned| poisoned.set(true));
            R::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_contains_panics() {
        unpoison();
        assert_eq!(guard(|| 1_u8 + 1), 2);
        assert_eq!(guard(|| -> u8 { panic!("boom") }), 0);
        unpoison();
    }

    #[test]
    fn a_panic_poisons_later_calls_on_the_thread() {
        unpoison();
        assert_eq!(guard(|| 1_u8), 1);
        assert!(!is_poisoned());
        assert_eq!(guard(|| -> u8 { panic!("boom") }), 0);
        assert!(is_poisoned());
        // The next call short-circuits: its body must not run.
        let mut ran = false;
        assert_eq!(
            guard(|| {
                ran = true;
                7_u8
            }),
            0
        );
        assert!(!ran);
        // An explicit reset restores the calls.
        unpoison();
        assert_eq!(guard(|| 7_u8), 7);
    }
}
