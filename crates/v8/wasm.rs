//! WebAssembly module objects and the compiled modules behind them
//! (`v8::WasmModuleObject`, `v8::CompiledWasmModule`).
//!
//! The crate we stand in for compiles wasm through V8's own compiler and keeps
//! the result in a shared C++ allocation; here a module object is the engine's
//! `WebAssembly.Module` object — an ordinary object whose decoded module sits in
//! the engine's table under the object's id — and the compiled form is that
//! decoded module.
//!
//! Two divergences the shape cannot hide:
//!
//! - A compiled module here is plain data rather than a handle to a shared
//!   allocation, which is what makes it usable across isolates (the property a
//!   host's store needs) and also why it cannot answer `get_wire_bytes_ref`: the
//!   engine decodes into structures and keeps no wire bytes.
//! - A module object the bridge compiles carries the module *prototype*, so it is
//!   the object `WebAssembly.Module` would answer, prototype and [[Module]] slot
//!   alike. A host that compares against its own `WebAssembly.Module.prototype`
//!   sees them agree.

use runtime::api;

use crate::data::{Value, WasmModuleObject};
use crate::handle::{Local, LocalHandle};
use crate::scope::PinScope;

/// A compiled WebAssembly module (`v8::CompiledWasmModule`).
///
/// The crate we stand in for holds a shared allocation and marks it `Send + Sync`
/// so a host can move it between isolates. Here it is the decoded module itself:
/// nothing in it points into the isolate's arena, so it outlives the object it
/// came from and travels between isolates on its own. `get_wire_bytes_ref` is
/// absent rather than wrong — see the module header.
#[derive(Debug, Clone)]
pub struct CompiledWasmModule(pub(crate) api::CompiledWasmModule);

impl WasmModuleObject {
    /// Compile `wire_bytes` into a module object
    /// (v8::WasmModuleObject::Compile).
    ///
    /// The object is the one the JS-API's own constructor answers, so a host's
    /// compiled module and a script's `WebAssembly.Module` are the same kind of
    /// thing. `None` is a decode or validation failure, with the `CompileError`
    /// the JS-API would report left pending.
    pub fn compile<'s>(
        scope: &PinScope<'s, '_>,
        wire_bytes: &[u8],
    ) -> Option<Local<'s, WasmModuleObject>> {
        let realm = crate::realm_of(scope);
        match api::WasmModuleObject::compile(&realm, wire_bytes) {
            Ok(value) => Some(Local::from_engine(value)),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// A module object for `compiled`
    /// (v8::WasmModuleObject::FromCompiledModule).
    ///
    /// The module is shared rather than copied, so several objects can carry one
    /// compiled module — which is what a host reads a module out, sends it
    /// somewhere, and reads it back in for.
    pub fn from_compiled_module<'s>(
        scope: &PinScope<'s, '_>,
        compiled: &CompiledWasmModule,
    ) -> Option<Local<'s, WasmModuleObject>> {
        let realm = crate::realm_of(scope);
        match api::WasmModuleObject::from_compiled_module(&realm, &compiled.0) {
            Ok(value) => Some(Local::from_engine(value)),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }
}

impl<'s> LocalHandle<'s, WasmModuleObject> {
    /// The compiled module this object carries
    /// (v8::WasmModuleObject::GetCompiledModule).
    ///
    /// The crate we stand in for aborts for a handle that is not a module object
    /// — its own `ApiCheck` — so this reports the reason instead of pretending:
    /// the engine keeps the module under the object's identity, and a handle
    /// without one is a bridge bug rather than a host's mistake.
    pub fn get_compiled_module(&self) -> CompiledWasmModule {
        let realm = crate::realm_current();
        match api::WasmModuleObject::get_compiled_module(&realm, self.engine()) {
            Ok(compiled) => CompiledWasmModule(compiled),
            Err(error) => panic!("bridge: a handle that is not a module object: {error}"),
        }
    }
}

/// A streaming compilation in flight (`v8::WasmStreaming`).
///
/// The crate we stand in for spells this as a shared pointer with a const
/// parameter selecting the cached-module-bytes protocol; here it is the engine's
/// stream handle, and the parameter stays because a host's own code names the
/// type with it. What that parameter selects there —
/// `set_has_compiled_module_bytes` and the caching callback `finish` takes on
/// `WasmStreaming<true>` — is absent here rather than wrong: this engine has no
/// compiled-module cache to hand bytes back to.
///
/// It is moved rather than copied, as there: a host takes it out of the callback
/// it was handed and gives it to whatever reads the source, and dropping it
/// without finishing leaves the promise unsettled (nothing can feed the stream
/// any more), which is what V8's own last `shared_ptr` dropping amounts to.
pub struct WasmStreaming<const HAS_COMPILED_MODULE_BYTES: bool>(pub(crate) api::WasmStreaming);

impl<const HAS_COMPILED_MODULE_BYTES: bool> WasmStreaming<HAS_COMPILED_MODULE_BYTES> {
    /// Pass a new chunk of bytes to the compilation
    /// (v8::WasmStreaming::OnBytesReceived).
    pub fn on_bytes_received(&mut self, data: &[u8]) {
        self.0.on_bytes_received(data);
    }

    /// Set the UTF-8 encoded source URL for the compilation
    /// (v8::WasmStreaming::SetUrl). Must be called before `finish`.
    ///
    /// Recorded and nothing else, as the engine's own documentation says: a
    /// module here carries no URL, so a host's URL describes the module it is
    /// streaming without anywhere to attach to yet.
    pub fn set_url(&mut self, url: &str) {
        self.0.set_url(url);
    }

    /// Abort streaming compilation (v8::WasmStreaming::Abort): the promise is
    /// rejected with `exception`, or left unsettled when there is none — V8's
    /// own behaviour for a stream nobody will finish.
    ///
    /// An engine refusal aborts the caller rather than being reported, because
    /// there is nowhere to report it: the crate we stand in for's `abort`
    /// answers nothing either, and the one way this can fail — a stream whose
    /// realm is already gone — is a bridge bug rather than a host's mistake.
    pub fn abort(self, exception: Option<Local<'_, Value>>) {
        if let Err(error) = self.0.abort(exception.map(|value| value.into_engine())) {
            panic!("bridge: aborting a wasm stream failed: {error}");
        }
    }
}

impl WasmStreaming<false> {
    /// Finish the stream (v8::WasmStreaming::Finish): what was received is
    /// compiled and the promise settles with the module — or with the
    /// `CompileError` its bytes deserved.
    ///
    /// Must not be called after `abort`; a call that lands after the stream was
    /// settled either way does nothing.
    ///
    /// An engine refusal aborts the caller for the same reason `abort`'s does:
    /// the crate we stand in for's `finish` answers nothing, and a failure here
    /// means the stream's realm is gone, which no host can bring about while it
    /// holds the stream.
    pub fn finish(self) {
        if let Err(error) = self.0.finish() {
            panic!("bridge: finishing a wasm stream failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Object, String as JsString};
    use crate::handle::Local as BridgeLocal;
    use crate::test_support::{EXPORTS_A_MEMORY, eval, in_context};

    /// A compiled module is the decoded module itself, so it survives leaving
    /// the object it came from — the property a host's store needs when it moves
    /// one between isolates — and a module object rebuilt from it carries the
    /// same module rather than a copy of its bytes.
    #[test]
    fn a_wasm_module_round_trips_through_its_compiled_form() {
        in_context!(scope, {
            let module = WasmModuleObject::compile(scope, &EXPORTS_A_MEMORY).expect("compile");
            let compiled = module.get_compiled_module();
            assert_eq!(compiled.0.module().exports.len(), 1);
            assert_eq!(compiled.0.module().exports[0].name, "m");

            // The object a host compiles is the object the JS-API would answer:
            // same prototype, so a host's own `instanceof WebAssembly.Module`
            // agrees with the engine's.
            let prototype = BridgeLocal::<Object>::from(module)
                .get_prototype(scope)
                .expect("a prototype");
            assert_eq!(prototype, eval(scope, "WebAssembly.Module.prototype"));

            let again = WasmModuleObject::from_compiled_module(scope, &compiled).expect("again");
            assert_ne!(again, module, "a second object, not the same handle");
            assert_eq!(
                again.get_compiled_module().0.module(),
                compiled.0.module(),
                "the module one object carries is the one the other does"
            );
        });
    }

    /// Bytes that are not a module answer `None` with the JS-API's own
    /// `CompileError` pending, not a bridge error: a host's `compile` sees what
    /// a script's `new WebAssembly.Module` would.
    #[test]
    fn compiling_bytes_that_are_not_a_module_leaves_a_compile_error() {
        in_context!(scope, {
            crate::tc_scope!(tc_scope, scope);
            assert!(WasmModuleObject::compile(tc_scope, &[0x00, 0x61, 0x73]).is_none());
            assert!(tc_scope.has_caught());

            let exception = tc_scope.exception().expect("a pending exception");
            let object = BridgeLocal::<Object>::try_from(exception).expect("an error object");
            let name = JsString::new(tc_scope, "name").expect("string").into();
            assert_eq!(
                object
                    .get(tc_scope, name)
                    .expect("name")
                    .to_rust_string_lossy(tc_scope),
                "CompileError"
            );
        });
    }
}
