//! The host-hooks embedding seam (PLAN §2, §8): host-defined operations the
//! execution model invokes, with the spec's default implementations.
//!
//! Designed in Phase 4; later phases fill in module resolution
//! (HostResolveImportedModule), promise-rejection tracking
//! (HostPromiseRejectionTracker), timers, and I/O as hooks here.

use crux::error::JsError;
use crux::string::JsString;

use crate::realm::Realm;

/// Host-defined operations the runtime calls (spec's host-defined abstract
/// operations). Each method's default implementation is the spec's default.
///
/// Attach an implementation to an [`crate::agent::Agent`] via its
/// `host_hooks` field; `None` (the default) uses the spec defaults.
pub trait HostHooks: std::fmt::Debug {
    /// HostEnsureCanCompileStrings (spec 19.2.1.1 step 4): lets hosts block
    /// `eval`/Function-constructor string compilation. `param_strings` are
    /// the Function-constructor parameter texts (empty for `eval`); `direct`
    /// says whether the compilation is a direct eval. The default permits.
    fn ensure_can_compile_strings(
        &self,
        _callee_realm: &Realm,
        _param_strings: &[JsString],
        _body_string: &JsString,
        _direct: bool,
    ) -> Result<(), JsError> {
        Ok(())
    }

    /// HostPromiseRejectionTracker (spec 27.2.1.9): called when a promise is
    /// rejected without a handler (`operation` = Reject) or when a handler is
    /// attached to a rejected promise (`operation` = Handle). The default does
    /// nothing; hosts surface unhandled rejections here.
    fn promise_rejection_tracker(
        &self,
        _promise: &crux::value::Value,
        _operation: bool,
    ) -> Result<(), JsError> {
        Ok(())
    }
}

/// HostPromiseRejectionTracker dispatch: the agent's hooks if present, else
/// the default (no-op). `operation` is `false` for Reject, `true` for Handle.
pub fn promise_rejection_tracker(
    agent: &crate::agent::Agent,
    promise: &crux::value::Value,
    operation: bool,
) -> Result<(), JsError> {
    match &agent.host_hooks {
        Some(hooks) => hooks.promise_rejection_tracker(promise, operation),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::script::perform_eval;

    /// A host that refuses all string compilation.
    #[derive(Debug)]
    struct BlockingHooks;

    impl HostHooks for BlockingHooks {
        fn ensure_can_compile_strings(
            &self,
            _callee_realm: &Realm,
            _param_strings: &[JsString],
            _body_string: &JsString,
            _direct: bool,
        ) -> Result<(), JsError> {
            Err(JsError::new(
                crux::ErrorKind::EvalError,
                "String compilation blocked by the host".into(),
            ))
        }
    }

    #[test]
    fn default_hooks_permit_eval() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let value = perform_eval(&mut agent, "var permitted = 1; permitted", false, true).unwrap();
        assert_eq!(value, crux::Value::Number(1.0));
    }

    #[test]
    fn custom_hooks_block_string_compilation() {
        let mut agent = Agent::new();
        agent.host_hooks = Some(Box::new(BlockingHooks));
        agent.initialize_host_defined_realm().unwrap();
        let err = perform_eval(&mut agent, "1;", false, true).unwrap_err();
        assert_eq!(err.kind, crux::ErrorKind::EvalError);
        // The hook receives the body text; verify it saw the source by
        // recording it through a cell in a second test.
        let observed: std::rc::Rc<std::cell::RefCell<Option<String>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        #[derive(Debug)]
        struct RecordingHooks {
            seen: std::rc::Rc<std::cell::RefCell<Option<String>>>,
        }
        impl HostHooks for RecordingHooks {
            fn ensure_can_compile_strings(
                &self,
                _callee_realm: &Realm,
                _param_strings: &[JsString],
                body_string: &JsString,
                _direct: bool,
            ) -> Result<(), JsError> {
                *self.seen.borrow_mut() = Some(body_string.to_string_lossy());
                Ok(())
            }
        }
        let mut agent = Agent::new();
        agent.host_hooks = Some(Box::new(RecordingHooks {
            seen: observed.clone(),
        }));
        agent.initialize_host_defined_realm().unwrap();
        perform_eval(&mut agent, "var x = 1;", false, false).unwrap();
        assert_eq!(observed.borrow().as_deref(), Some("var x = 1;"));
    }
}
