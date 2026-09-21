//! WebAssembly module objects and the compiled modules behind them
//! (v8::WasmModuleObject, v8::CompiledWasmModule).

use std::cell::RefCell;
use std::rc::Rc;

use crux::error::{ErrorKind, JsError};
use crux::heap::Pin;
use crux::value::Value;
use wasm::Module;

use super::context::Context;
use super::handle::Local;
use crate::promise::PromiseCapability;

/// A compiled WebAssembly module (`v8::CompiledWasmModule`).
///
/// V8's is a handle to a shared allocation that several module objects point at,
/// and it is marked `Send + Sync` so a host can move it to another isolate. Here
/// it is the decoded module itself, which is plain data: it points at nothing in
/// the isolate's arena, so it outlives the object it came from and travels
/// between isolates — the two properties the store a host keeps these in needs.
///
/// What it does not carry is the wire bytes it was decoded from, so
/// `get_wire_bytes_ref` has no answer here rather than a wrong one; the reason is
/// recorded in `.notes/embedding.md` §12 item 10.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledWasmModule {
    module: Module,
}

impl CompiledWasmModule {
    /// The decoded module.
    pub fn module(&self) -> &Module {
        &self.module
    }
}

/// A module object (`v8::WasmModuleObject`).
pub struct WasmModuleObject;

impl WasmModuleObject {
    /// Compile `wire_bytes` into a module object in `context`'s realm
    /// (`v8::WasmModuleObject::Compile`): decode, validate, and answer the object
    /// the JS-API's own constructor answers — the same prototype and the same
    /// [[Module]] slot. A decode or validation failure is the `CompileError` the
    /// JS-API would report.
    pub fn compile(context: &Context, wire_bytes: &[u8]) -> Result<Local, JsError> {
        context
            .with_agent(|agent| crate::builtins::wasm::compile_module_value(agent, wire_bytes))
            .map(Local)
    }

    /// A module object for `compiled` (`v8::WasmModuleObject::FromCompiledModule`):
    /// the module is shared, so several objects can carry one.
    pub fn from_compiled_module(
        context: &Context,
        compiled: &CompiledWasmModule,
    ) -> Result<Local, JsError> {
        context
            .with_agent(|agent| {
                crate::builtins::wasm::module_object(agent, compiled.module.clone())
            })
            .map(Local)
    }

    /// The compiled module a module object carries
    /// (`v8::WasmModuleObject::GetCompiledModule`).
    ///
    /// V8 aborts for a handle that is not a module object — its own `ApiCheck` —
    /// so this answers the reason for one instead.
    pub fn get_compiled_module(
        context: &Context,
        module: &Local,
    ) -> Result<CompiledWasmModule, JsError> {
        context.with_agent(|agent| {
            let object = module.value().as_object().ok_or_else(not_a_module)?;
            agent
                .wasm_modules
                .get(&object.id())
                .cloned()
                .map(|module| CompiledWasmModule { module })
                .ok_or_else(not_a_module)
        })
    }
}

/// The failure both halves of a compiled-module read share.
fn not_a_module() -> JsError {
    JsError::new(
        ErrorKind::TypeError,
        "a module handle that is not a WebAssembly.Module object".into(),
    )
}

/// A streaming compilation in flight (`v8::WasmStreaming`).
///
/// V8 hands this to the embedder's streaming callback; the embedder feeds it
/// bytes and finishes it, which is what settles the promise
/// `WebAssembly.compileStreaming` answered. Here it is a shareable handle over
/// that record: the bytes received so far, the URL the host set, and the promise
/// capability the compile settles — whose three values are **rooted** for as long
/// as a handle exists, because the host holds this while no engine object points
/// at it (the two handlers that do are single-use, and die with the source
/// promise's reactions).
///
/// The tier is stated rather than implied: bytes accumulate and are decoded at
/// [`finish`](Self::finish), not as they arrive. A host sees the same module and
/// the same promise; what it does not get is V8's compile-as-it-streams, which
/// this engine's decoder cannot offer.
#[derive(Clone)]
pub struct WasmStreaming {
    state: Rc<RefCell<StreamingState>>,
}

struct StreamingState {
    /// The realm the promise belongs to, and the way back to the isolate from a
    /// call the engine is not in the middle of.
    context: Context,
    promise: Value,
    resolve: Value,
    reject: Value,
    /// What keeps the three values above alive while the host holds this;
    /// dropped with the state.
    #[allow(dead_code)] // Held for its `Drop`.
    roots: Vec<Pin>,
    bytes: Vec<u8>,
    url: Option<String>,
    settled: bool,
}

impl std::fmt::Debug for WasmStreaming {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.borrow();
        f.debug_struct("WasmStreaming")
            .field("bytes", &state.bytes.len())
            .field("url", &state.url)
            .field("settled", &state.settled)
            .finish_non_exhaustive()
    }
}

impl WasmStreaming {
    /// A stream that will settle `capability`'s promise. Built by
    /// `WebAssembly.compileStreaming`, which is the only thing that makes one.
    pub(crate) fn new(context: Context, capability: &PromiseCapability) -> Self {
        let roots = vec![
            crux::heap::pin(capability.promise),
            crux::heap::pin(capability.resolve),
            crux::heap::pin(capability.reject),
        ];
        Self {
            state: Rc::new(RefCell::new(StreamingState {
                context,
                promise: capability.promise,
                resolve: capability.resolve,
                reject: capability.reject,
                roots,
                bytes: Vec::new(),
                url: None,
                settled: false,
            })),
        }
    }

    /// The promise this stream settles — the one `compileStreaming` answered.
    pub(crate) fn promise(&self) -> Value {
        self.state.borrow().promise
    }

    /// Pass a chunk of bytes to the compilation
    /// (`v8::WasmStreaming::OnBytesReceived`).
    ///
    /// Always consumes the whole chunk, as there: the engine's decoder takes
    /// whole bytes and does its work at [`finish`](Self::finish).
    pub fn on_bytes_received(&mut self, data: &[u8]) {
        self.state.borrow_mut().bytes.extend_from_slice(data);
    }

    /// Set the source URL (`v8::WasmStreaming::SetUrl`).
    ///
    /// Recorded, and nothing else: a `WebAssembly.Module` here carries no URL,
    /// so there is nothing yet for a host's URL to attach to (V8 puts it on the
    /// script the module compiles as). It is recorded rather than dropped because
    /// a host that sets one is describing the module it is streaming, and this is
    /// where a reader would look for it.
    pub fn set_url(&mut self, url: &str) {
        self.state.borrow_mut().url = Some(url.to_owned());
    }

    /// Finish the stream (`v8::WasmStreaming::Finish`): decode and validate what
    /// was received, and settle the promise with the module — or with the
    /// `CompileError` the JS-API's own compile would report.
    ///
    /// A second finish, or one after an abort, does nothing. V8 documents
    /// finishing after an abort as the caller's error, and a no-op is the one
    /// honest answer this shape has for it.
    pub fn finish(&self) -> Result<(), JsError> {
        let (context, resolve, reject, bytes) = {
            let mut state = self.state.borrow_mut();
            if state.settled {
                return Ok(());
            }
            state.settled = true;
            (
                state.context,
                state.resolve,
                state.reject,
                std::mem::take(&mut state.bytes),
            )
        };
        context.with_agent(|agent| {
            let call = |agent: &mut crate::agent::Agent, target: Value, argument: Value| {
                crate::function::call(agent, &target, Value::Undefined, &[argument]).map(|_| ())
            };
            match crate::builtins::wasm::compile_module_value(agent, &bytes) {
                Ok(module) => call(agent, resolve, module),
                Err(error) => {
                    let reason = crate::promise::error_value(agent, &error);
                    call(agent, reject, reason)
                }
            }
        })
    }

    /// Abort the stream (`v8::WasmStreaming::Abort`).
    ///
    /// With a value the promise is rejected with it; with none it is left
    /// unsettled, which is V8's documented behaviour for a stream nobody will
    /// finish (a browser tab being refreshed, say). Either way nothing more can
    /// be fed to it.
    pub fn abort(&self, exception: Option<Local>) -> Result<(), JsError> {
        let (context, reject, exception) = {
            let mut state = self.state.borrow_mut();
            if state.settled {
                return Ok(());
            }
            state.settled = true;
            state.bytes = Vec::new();
            (state.context, state.reject, exception)
        };
        let Some(exception) = exception else {
            return Ok(());
        };
        context.with_agent(|agent| {
            crate::function::call(agent, &reject, Value::Undefined, &[exception.into_value()])
                .map(|_| ())
        })
    }
}

impl crux::heap::Trace for WasmStreaming {
    fn trace(&self, visit: &mut dyn FnMut(crux::heap::GcAny)) {
        // A state borrowed mutably mid-collection is skipped, as the engine's own
        // `RefCell` trace does, and nothing is lost by it: these values are
        // pinned for as long as the state exists.
        if let Ok(state) = self.state.try_borrow() {
            state.promise.trace(visit);
            state.resolve.trace(visit);
            state.reject.trace(visit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Isolate, MaybeLocal, Promise};
    use crate::host::HostHooks;

    /// Magic and version, and nothing else: a module with no sections.
    const EMPTY: [u8; 8] = [0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];

    /// A host that streams `bytes` for every stream it is handed — the fetch loop
    /// a browser or Deno runs, reduced to its last step.
    #[derive(Debug)]
    struct Streams {
        bytes: Vec<u8>,
    }

    impl HostHooks for Streams {
        fn has_wasm_streaming_callback(&self) -> bool {
            true
        }

        fn wasm_streaming(&self, source: &Value, streaming: &WasmStreaming) -> Result<(), JsError> {
            assert!(
                source.as_object().is_some(),
                "the hook is handed what the source resolved to"
            );
            let mut streaming = streaming.clone();
            streaming.on_bytes_received(&self.bytes);
            streaming.finish()
        }
    }

    /// A host that refuses every stream, so the abort path is the one that runs.
    #[derive(Debug)]
    struct Refuses;

    impl HostHooks for Refuses {
        fn has_wasm_streaming_callback(&self) -> bool {
            true
        }
    }

    fn eval(context: &Context, source: &str) -> crate::api::Local {
        match context.eval(source) {
            MaybeLocal::Some(value) => value,
            MaybeLocal::Nothing => panic!("eval failed for {source:?}"),
        }
    }

    /// The JS-API's shape is V8's: the promise is answered immediately, the
    /// source is resolved on a later turn, and the host's hook is what settles
    /// the promise — with the module its bytes compiled to.
    #[test]
    fn compile_streaming_settles_with_the_module_its_host_streams() {
        let mut isolate = Isolate::new();
        isolate.agent.host_hooks = Some(Box::new(Streams {
            bytes: EMPTY.to_vec(),
        }));
        let context = Context::new(&mut isolate).expect("context");

        let promise = eval(
            &context,
            "WebAssembly.compileStreaming({ url: 'streamed' })",
        );
        assert_eq!(
            Promise::state(&context, &promise).expect("state"),
            "pending",
            "the source has not resolved yet"
        );

        context.run_microtasks().expect("microtasks");
        assert_eq!(
            Promise::state(&context, &promise).expect("state"),
            "fulfilled"
        );
        let module = Promise::result(&context, &promise).expect("result");
        assert!(
            WasmModuleObject::get_compiled_module(&context, &module).is_ok(),
            "the resolution is the module object the bytes compiled to"
        );
    }

    /// A source that fails rejects the streaming promise with that failure, which
    /// is what V8's own reject-callback does with it.
    #[test]
    fn a_source_that_rejects_rejects_the_streaming_promise() {
        let mut isolate = Isolate::new();
        isolate.agent.host_hooks = Some(Box::new(Refuses));
        let context = Context::new(&mut isolate).expect("context");

        let promise = eval(
            &context,
            "WebAssembly.compileStreaming(Promise.reject(new Error('no body')))",
        );
        context.run_microtasks().expect("microtasks");

        assert_eq!(
            Promise::state(&context, &promise).expect("state"),
            "rejected"
        );
        let reason = Promise::result(&context, &promise).expect("result");
        assert!(
            reason.as_object().is_some(),
            "the reason the source failed with, not a compile error"
        );
    }

    /// No host hook, no stream: the method is refused rather than answered with a
    /// promise nothing could settle.
    #[test]
    fn compile_streaming_without_a_host_hook_is_refused() {
        let mut isolate = Isolate::new();
        let context = Context::new(&mut isolate).expect("context");

        assert!(
            matches!(
                context.eval("WebAssembly.compileStreaming({})"),
                MaybeLocal::Nothing
            ),
            "a host with no streaming hook refuses the call"
        );
        assert!(isolate.take_pending_exception().is_some());
    }
}
