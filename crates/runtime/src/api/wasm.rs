//! WebAssembly module objects and the compiled modules behind them
//! (v8::WasmModuleObject, v8::CompiledWasmModule).

use crux::error::{ErrorKind, JsError};
use wasm::Module;

use super::context::Context;
use super::handle::Local;

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
