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
    /// attached to a rejected promise (`operation` = Handle). `reason` is the
    /// rejection value when the runtime has one in hand. The default does
    /// nothing; hosts surface unhandled rejections here.
    fn promise_rejection_tracker(
        &self,
        _promise: &crux::value::Value,
        _reason: Option<&crux::value::Value>,
        _operation: bool,
    ) -> Result<(), JsError> {
        Ok(())
    }

    /// The host's error-stack formatter, if it installed one
    /// (v8::Isolate::SetPrepareStackTraceCallback): the error and the frames its
    /// trace was captured from go in, and what `.stack` should read comes out.
    ///
    /// `None` is "no formatter here": the engine renders the trace itself, which
    /// is what a host that installs no callback gets in V8 as well. A host that
    /// answers `Some` owns the text — the frames are the same ones the engine's
    /// own rendering is built from, so a formatter that wants them has them and
    /// one that wants to rewrite them can.
    fn prepare_stack_trace(
        &self,
        _error: &crux::value::Value,
        _frames: &[crate::api::StackFrame],
    ) -> Result<Option<crux::value::Value>, JsError> {
        Ok(None)
    }

    /// HostCreateWorker (spec 9.4.5): start a worker agent running `source`
    /// with access to `shared` byte blocks. The default is unsupported; hosts
    /// attach an implementation (the runtime's is `crate::workers::spawn_worker`,
    /// behind the `workers` feature).
    fn create_worker(
        &self,
        _source: &str,
        _shared: &[crux::typed_array::SharedBuffer],
    ) -> Result<(), JsError> {
        Err(JsError::new(
            crux::ErrorKind::TypeError,
            "HostCreateWorker is not implemented by this host".into(),
        ))
    }

    /// Whether this host handles streaming compilation at all, i.e. whether
    /// [`wasm_streaming`](Self::wasm_streaming) will take a stream.
    ///
    /// `WebAssembly.compileStreaming` needs an embedder that can fetch and feed
    /// bytes — V8 requires one to be installed and checks it — so an engine that
    /// is asked without one refuses the call rather than answering a promise
    /// nothing will ever settle. The default refuses.
    ///
    /// Both halves of the hook exist only with the `wasm` feature: a build
    /// without it has no `WebAssembly` global to stream into, and no
    /// `api::WasmStreaming` for the parameter below, so the trait has no such
    /// method there. A host that implements these without the feature is
    /// implementing a hook its build cannot reach.
    #[cfg(feature = "wasm")]
    fn has_wasm_streaming_callback(&self) -> bool {
        false
    }

    /// The host's streaming hook (`v8::Isolate::SetWasmStreamingCallback`):
    /// called once the source `WebAssembly.compileStreaming` was given has
    /// resolved, with that value and the stream to feed. Finishing the stream
    /// settles the promise the caller got; aborting it rejects that promise.
    ///
    /// The default refuses, which [`has_wasm_streaming_callback`]
    /// (Self::has_wasm_streaming_callback) is what answers for a host that has
    /// not installed one.
    #[cfg(feature = "wasm")]
    fn wasm_streaming(
        &self,
        _source: &crux::value::Value,
        _streaming: &crate::api::WasmStreaming,
    ) -> Result<(), JsError> {
        Err(JsError::new(
            crux::ErrorKind::TypeError,
            "WebAssembly.compileStreaming is not implemented by this host".into(),
        ))
    }

    /// HostInitializeImportMetaObject (spec 16.2.1.8,
    /// `v8::Isolate::SetHostInitializeImportMetaObjectCallback`): lets the host
    /// fill a module's `import.meta` when the engine first makes the object.
    /// `meta` is the ordinary object the engine made and `module` is the record
    /// it belongs to; the host sets on it what its own `import.meta` should
    /// carry (`deno_core`'s callback puts `url`, `main` and `resolve` there).
    ///
    /// Called once per module record, on the first read of `import.meta` in it.
    /// The default does nothing, so a host that installed no callback leaves the
    /// empty object the engine made.
    fn initialize_import_meta_object(
        &self,
        _module: &crate::api::Module,
        _meta: &crux::value::Value,
    ) -> Result<(), JsError> {
        Ok(())
    }

    /// HostImportModuleDynamically (spec 13.3.10.2 step 12,
    /// `v8::Isolate::SetHostImportModuleDynamicallyCallback`): resolve a dynamic
    /// `import()` through the host's own loader, which is the only thing that can
    /// rewrite a relative specifier — it is handed the **referrer's name**, the
    /// name of the module or script the `import()` was written in, to resolve
    /// against.
    ///
    /// `Ok(promise)` is what the `import()` expression answers. `None` means this
    /// host resolves no dynamic imports and the engine falls back to its own
    /// registry ([`crate::module::host_resolve_imported_module`]), which is the
    /// path every host that installs no hook keeps.
    ///
    /// `attributes` is the validated import-attribute list as the host's own
    /// callback takes it: key text and value, in source order. `phase` is the
    /// api-facing phase, which is the vocabulary the host's callback is written
    /// in.
    fn import_module_dynamically(
        &self,
        _specifier: &JsString,
        _referrer_name: Option<&JsString>,
        _phase: crate::api::ModuleImportPhase,
        _attributes: &[(JsString, JsString)],
    ) -> Option<Result<crux::value::Value, JsError>> {
        None
    }
}

/// HostPromiseRejectionTracker dispatch: the agent's hooks if present, else
/// the default (no-op). `operation` is `false` for Reject, `true` for Handle.
pub fn promise_rejection_tracker(
    agent: &crate::agent::Agent,
    promise: &crux::value::Value,
    reason: Option<&crux::value::Value>,
    operation: bool,
) -> Result<(), JsError> {
    match &agent.host_hooks {
        Some(hooks) => hooks.promise_rejection_tracker(promise, reason, operation),
        None => Ok(()),
    }
}

/// The host's error-stack formatter, when it installed one: the answer to use
/// for `.stack`, or `None` for "render it here".
pub fn prepare_stack_trace(
    agent: &crate::agent::Agent,
    error: &crux::value::Value,
    frames: &[crate::api::StackFrame],
) -> Result<Option<crux::value::Value>, JsError> {
    match &agent.host_hooks {
        Some(hooks) => hooks.prepare_stack_trace(error, frames),
        None => Ok(None),
    }
}

/// Whether the agent's hooks handle streaming compilation (see
/// [`HostHooks::has_wasm_streaming_callback`]).
///
/// Only with the `wasm` feature: the hook is part of the JS API, and the
/// `api::WasmStreaming` the dispatch below hands over is exported with it.
#[cfg(feature = "wasm")]
pub fn has_wasm_streaming_callback(agent: &crate::agent::Agent) -> bool {
    match &agent.host_hooks {
        Some(hooks) => hooks.has_wasm_streaming_callback(),
        None => false,
    }
}

/// The streaming hook dispatch: hand `source` and `streaming` to the agent's
/// hooks. Called once the source `WebAssembly.compileStreaming` was given has
/// resolved.
#[cfg(feature = "wasm")]
pub fn wasm_streaming(
    agent: &crate::agent::Agent,
    source: &crux::value::Value,
    streaming: &crate::api::WasmStreaming,
) -> Result<(), JsError> {
    match &agent.host_hooks {
        Some(hooks) => hooks.wasm_streaming(source, streaming),
        None => Err(JsError::new(
            crux::ErrorKind::TypeError,
            "WebAssembly.compileStreaming needs a host streaming hook".into(),
        )),
    }
}

/// The `import.meta` hook dispatch (see
/// [`HostHooks::initialize_import_meta_object`]): the agent's hooks if it has
/// any, else nothing — the object the engine made is then what the module's code
/// sees.
pub fn initialize_import_meta_object(
    agent: &crate::agent::Agent,
    module: &crate::api::Module,
    meta: &crux::value::Value,
) -> Result<(), JsError> {
    match &agent.host_hooks {
        Some(hooks) => hooks.initialize_import_meta_object(module, meta),
        None => Ok(()),
    }
}

/// The dynamic-import hook dispatch (see
/// [`HostHooks::import_module_dynamically`]): the agent's hooks if they resolve
/// dynamic imports, else `None` — the engine's own registry path.
pub fn import_module_dynamically(
    agent: &crate::agent::Agent,
    specifier: &JsString,
    referrer_name: Option<&JsString>,
    phase: crate::api::ModuleImportPhase,
    attributes: &[(JsString, JsString)],
) -> Option<Result<crux::value::Value, JsError>> {
    match &agent.host_hooks {
        Some(hooks) => hooks.import_module_dynamically(specifier, referrer_name, phase, attributes),
        None => None,
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
        let value = perform_eval(
            &mut agent,
            &crux::string::JsString::from_utf8("var permitted = 1; permitted"),
            false,
            true,
        )
        .unwrap();
        assert_eq!(value, crux::Value::Number(1.0));
    }

    #[test]
    fn custom_hooks_block_string_compilation() {
        let mut agent = Agent::new();
        agent.host_hooks = Some(Box::new(BlockingHooks));
        agent.initialize_host_defined_realm().unwrap();
        let err = perform_eval(
            &mut agent,
            &crux::string::JsString::from_utf8("1;"),
            false,
            true,
        )
        .unwrap_err();
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
        perform_eval(
            &mut agent,
            &crux::string::JsString::from_utf8("var x = 1;"),
            false,
            false,
        )
        .unwrap();
        assert_eq!(observed.borrow().as_deref(), Some("var x = 1;"));
    }

    /// A host that formats an error's stack itself
    /// (`Isolate::SetPrepareStackTraceCallback`): it records the frames it is
    /// handed and answers the text `.stack` reads.
    #[derive(Debug)]
    struct FormatsStacks {
        seen: std::rc::Rc<std::cell::RefCell<Vec<crate::api::StackFrame>>>,
    }

    impl HostHooks for FormatsStacks {
        fn prepare_stack_trace(
            &self,
            _error: &crux::value::Value,
            frames: &[crate::api::StackFrame],
        ) -> Result<Option<crux::value::Value>, JsError> {
            *self.seen.borrow_mut() = frames.to_vec();
            Ok(Some(crux::value::Value::String(crux::handle::Handle::new(
                JsString::from_utf8("a host's stack"),
            ))))
        }
    }

    /// A host with a formatter owns what `.stack` reads, and the frames it is
    /// handed are the ones the trace was captured from — the same frames the
    /// engine's own rendering is built from, captured while the stack is still
    /// the one the error was made on.
    #[test]
    fn a_host_formats_the_stack_it_is_handed() {
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut agent = Agent::new();
        agent.host_hooks = Some(Box::new(FormatsStacks { seen: seen.clone() }));
        agent.initialize_host_defined_realm().unwrap();
        // The script reads `.stack` on the error it caught, so the value the
        // accessor answers is what the script hands back.
        let stack = agent
            .run_script_named(
                "function inner() { throw new Error('boom'); }\n\
                 try { inner(); } catch (e) { e.stack; }",
                Some(JsString::from_utf8("file:///host.js")),
            )
            .unwrap();
        assert_eq!(
            stack.as_string().map(|text| text.to_string_lossy()),
            Some("a host's stack".to_string()),
            "the accessor answers what the host's formatter said"
        );
        let seen = seen.borrow();
        assert_eq!(
            seen.iter()
                .map(|frame| frame.function_name.as_deref().unwrap_or(""))
                .collect::<Vec<_>>(),
            vec!["inner", ""],
            "innermost first, and the script's own top level has no function name"
        );
        assert!(
            seen.iter()
                .all(|frame| frame.script_name.as_deref() == Some("file:///host.js")),
            "a script the host named gives that name to its frames: {seen:?}"
        );
        assert_eq!(
            seen[0].column, 26,
            "and the frame is at the call site, not at column zero"
        );
        assert_eq!(seen[0].line, 1, "the frame sits on the line it threw from");
        assert!(seen[0].column > 0, "and at the site, not at column zero");
    }
}
