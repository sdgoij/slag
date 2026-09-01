//! The `slag` embedding API: the host-facing surface of the Slag engine.
//!
//! One crate to depend on — a [`Context`] per agent/realm, [`JsValue`] and
//! [`JsObject`] handles, host callbacks and hooks, and (with the `jit`
//! feature) the Cranelift JIT hook. Everything else in the workspace is
//! internal; nothing else is part of the embedding contract.
//!
//! ```
//! use slag::{Context, JsValue};
//!
//! let mut context = Context::new().unwrap();
//! let value = context.eval("1 + 2").unwrap();
//! assert_eq!(value.as_number(), Some(3.0));
//! ```

pub use crux::error::JsError;
pub use runtime::HostHooks;
pub use runtime::embed::{Context, HostCallbacks, JsObject, JsValue};
pub use runtime::embed::{OutputFn, RandomFn};

/// Install the Cranelift JIT hook on `context`'s agent (feature `jit`).
///
/// The hook is what makes hot certified bodies run at machine speed; the
/// CLI and conformance sweep enable it by default.
#[cfg(feature = "jit")]
pub fn install_jit(context: &mut Context) -> Result<(), String> {
    jit::install(context.agent_mut())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_works_through_the_facade() {
        let mut context = Context::new().unwrap();
        let value = context.eval("1 + 2").unwrap();
        assert_eq!(value.as_number(), Some(3.0));
    }

    #[cfg(feature = "jit")]
    #[test]
    fn jit_hook_installs_and_runs_a_loop() {
        let mut context = Context::new().unwrap();
        install_jit(&mut context).unwrap();
        let value = context
            .eval("function f(n) { var s = 0; for (var i = 0; i < n; i++) { s += i; } return s; } f(1000)")
            .unwrap();
        assert_eq!(value.as_number(), Some(499500.0));
    }

    #[cfg(feature = "fs")]
    #[test]
    fn fs_globals_install_through_the_feature() {
        let mut context = Context::new().unwrap();
        context.install_fs().unwrap();
        assert!(context.eval("typeof fs").is_ok());
    }
}
