//! Modules: the handle over a module record, and the vocabulary around loading
//! one (`v8::Module`, `v8::ModuleStatus`, `v8::ModuleImportPhase`).
//!
//! The crate we stand in for holds a module as a C++ heap object and asks the
//! embedder to resolve every request through a callback. The engine parses,
//! links and evaluates module records of its own, and resolves imports itself
//! from the modules the host has registered — so a handle here names one of
//! those records, and instantiation answers the host's callback by asking it for
//! every request and registering the answers.
//!
//! [`ModuleStatus`] and [`ModuleImportPhase`] are the engine's own types: the
//! crate we stand in for's vocabulary is the one the engine's API is written in,
//! so there is nothing to convert.
//!
//! Two divergences the shape cannot hide:
//!
//! - The crate we stand in for resolves a request per referrer; the engine keys
//!   its module namespace by specifier text. Registering what the callback
//!   returns under the specifier it was asked about is faithful only while two
//!   referrers do not write the same text for different modules. A real host's
//!   resolver rewrites specifiers before the engine sees them (`deno_core`'s
//!   `specifier_key`), which is where that belongs.
//! - A source-phase request (`import source`) is answered by the engine from the
//!   module's own record, so `instantiate_module2`'s source callback is never
//!   called.

use crux::error::JsError;
use runtime::api;

use crate::data::{Context, FixedArray, Module, Object, String as JsString, Value};
use crate::handle::Local;
use crate::primitives::undefined;
use crate::scope::PinScope;
use crate::support::{MapFnFrom, MapFnTo, UnitType};

pub use runtime::api::{ModuleImportPhase, ModuleStatus};

/// How a host resolves one module request during
/// [`Local::<Module>::instantiate_module`] (v8::ResolveModuleCallback).
///
/// The crate we stand in for spells this as a C function pointer, with the
/// return value on the stack on Windows. Nothing here crosses a language
/// boundary, so it is a plain function pointer with the same argument list.
pub type ResolveModuleCallback<'s> = fn(
    Local<'s, Context>,
    Local<'s, JsString>,
    Local<'s, FixedArray>,
    Local<'s, Module>,
) -> Option<Local<'s, Module>>;

impl<'s, F> MapFnFrom<F> for ResolveModuleCallback<'s>
where
    F: UnitType
        + Fn(
            Local<'s, Context>,
            Local<'s, JsString>,
            Local<'s, FixedArray>,
            Local<'s, Module>,
        ) -> Option<Local<'s, Module>>,
{
    fn mapping() -> Self {
        resolve_module_adapter::<F>
    }
}

/// The concrete function that stands in for a host's resolver: a generic type
/// parameter cannot coerce to a function pointer, so the host's function item is
/// reconstructed and called from one that can.
fn resolve_module_adapter<'s, F>(
    context: Local<'s, Context>,
    specifier: Local<'s, JsString>,
    attributes: Local<'s, FixedArray>,
    referrer: Local<'s, Module>,
) -> Option<Local<'s, Module>>
where
    F: UnitType
        + Fn(
            Local<'s, Context>,
            Local<'s, JsString>,
            Local<'s, FixedArray>,
            Local<'s, Module>,
        ) -> Option<Local<'s, Module>>,
{
    (F::get())(context, specifier, attributes, referrer)
}

/// How a host would supply the source of a source-phase request
/// (v8::ResolveSourceCallback).
///
/// The engine answers those requests from the record it already has, so this
/// shape exists for a host that passes one, and is never called.
pub type ResolveSourceCallback<'s> = fn(
    Local<'s, Context>,
    Local<'s, JsString>,
    Local<'s, FixedArray>,
    Local<'s, Module>,
) -> Option<Local<'s, Object>>;

impl<'s, F> MapFnFrom<F> for ResolveSourceCallback<'s>
where
    F: UnitType
        + Fn(
            Local<'s, Context>,
            Local<'s, JsString>,
            Local<'s, FixedArray>,
            Local<'s, Module>,
        ) -> Option<Local<'s, Object>>,
{
    fn mapping() -> Self {
        resolve_source_adapter::<F>
    }
}

/// The concrete function that stands in for a host's source resolver; see
/// [`resolve_module_adapter`].
fn resolve_source_adapter<'s, F>(
    context: Local<'s, Context>,
    specifier: Local<'s, JsString>,
    attributes: Local<'s, FixedArray>,
    referrer: Local<'s, Module>,
) -> Option<Local<'s, Object>>
where
    F: UnitType
        + Fn(
            Local<'s, Context>,
            Local<'s, JsString>,
            Local<'s, FixedArray>,
            Local<'s, Module>,
        ) -> Option<Local<'s, Object>>,
{
    (F::get())(context, specifier, attributes, referrer)
}

impl<'s> Local<'s, Module> {
    /// The module's current status (v8::Module::GetStatus).
    pub fn get_status(self) -> ModuleStatus {
        self.module().status()
    }

    /// For a module in `Errored` status, the exception it failed with
    /// (v8::Module::GetException).
    ///
    /// The crate we stand in for returns an empty handle for a module that has
    /// not failed, which its own callers unwrap; *undefined* is the honest value
    /// for the same case here.
    pub fn get_exception(self) -> Local<'s, Value> {
        match self.module().exception() {
            Some(value) => Local::from_engine(value),
            None => undefined(&()).into(),
        }
    }

    /// The module's namespace object (v8::Module::GetModuleNamespace).
    ///
    /// The crate we stand in for takes no scope here because it has the current
    /// one on the stack; the bridge reads the entered context instead.
    pub fn get_module_namespace(self) -> Local<'s, Value> {
        let realm = crate::realm_current();
        match self.module().namespace(&realm) {
            Ok(value) => Local::from_engine(value),
            Err(error) => panic!("bridge: module namespace creation failed: {error}"),
        }
    }

    /// Link the module and everything it imports
    /// (v8::Module::InstantiateModule).
    ///
    /// The crate we stand in for calls `callback` for each request as it links.
    /// The engine resolves from the modules the host has registered, so this
    /// asks `callback` for every request first — and for the requests of every
    /// module it hands back — registers the answers, and then links. `None`
    /// means linking failed, with the exception pending: the host's own if it
    /// refused a request, the engine's otherwise.
    pub fn instantiate_module<'s2, 'i>(
        self,
        scope: &PinScope<'s2, 'i>,
        callback: impl MapFnTo<ResolveModuleCallback<'s2>>,
    ) -> Option<bool> {
        self.instantiate(scope, callback.map_fn_to())
    }

    /// Link the module, taking the source callback a host passes as well
    /// (v8::Module::InstantiateModule with a source resolver).
    pub fn instantiate_module2<'s2, 'i>(
        self,
        scope: &PinScope<'s2, 'i>,
        callback: impl MapFnTo<ResolveModuleCallback<'s2>>,
        source_callback: impl MapFnTo<ResolveSourceCallback<'s2>>,
    ) -> Option<bool> {
        // The engine answers a source-phase request from the module record it
        // already holds, so the callback is converted (a host's spelling of it
        // has to type-check) and dropped.
        let _source: ResolveSourceCallback<'s2> = source_callback.map_fn_to();
        self.instantiate(scope, callback.map_fn_to())
    }

    /// Evaluate the module and its dependencies (v8::Module::Evaluate).
    ///
    /// The engine evaluates a body that may await through a promise capability,
    /// which is exactly what the crate we stand in for hands back, so the value
    /// is that promise. `None` means the module could not be evaluated at all
    /// and a pending exception was set.
    pub fn evaluate(self, scope: &PinScope<'s, '_>) -> Option<Local<'s, Value>> {
        let realm = scope.get_current_context().context();
        match self.module().evaluate(&realm) {
            Ok(value) => Some(Local::from_engine(value)),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// Whether this module is a source text module
    /// (v8::Module::IsSourceTextModule).
    ///
    /// Every module the engine compiles is one, so this is `true` — stated
    /// rather than assumed because a host branches on it.
    pub fn is_source_text_module(self) -> bool {
        let _ = self;
        true
    }

    /// Whether this module is a synthetic module
    /// (v8::Module::IsSyntheticModule).
    pub fn is_synthetic_module(self) -> bool {
        let _ = self;
        false
    }

    /// Resolve every request in the graph, then link.
    fn instantiate<'s2, 'i>(
        self,
        scope: &PinScope<'s2, 'i>,
        resolve: ResolveModuleCallback<'s2>,
    ) -> Option<bool> {
        let realm = scope.get_current_context().context();
        match self.resolve_graph(scope, &realm, resolve) {
            // The host refused a request and set its own exception.
            Ok(false) => None,
            Ok(true) => match self.module().instantiate(&realm) {
                Ok(()) => Some(true),
                Err(error) => {
                    crate::throw(scope, &error);
                    None
                }
            },
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// Walk the graph from this module, asking the host to resolve each request
    /// and registering what it returns. `Ok(false)` means the host refused a
    /// request, whose exception is the pending one.
    ///
    /// Every request edge is asked about, as the crate we stand in for asks
    /// about every edge; each module is walked once, so a cycle terminates.
    fn resolve_graph<'s2>(
        self,
        scope: &PinScope<'s2, '_>,
        realm: &api::Context,
        resolve: ResolveModuleCallback<'s2>,
    ) -> Result<bool, JsError> {
        let context = scope.get_current_context();
        let entry = self.module();
        let mut visited: Vec<api::Module> = vec![entry];
        let mut queue: Vec<api::Module> = vec![entry];
        while let Some(module) = queue.pop() {
            let referrer = Local::<Module>::from_module(module);
            for request in module.requested_modules() {
                let text = api::Local::string(request.specifier.clone());
                let specifier = Local::<JsString>::from_engine(text);
                let attributes =
                    Local::<FixedArray>::from_engine(attributes_of(realm, &request)?).cast();
                let Some(resolved) = resolve(context, specifier, attributes, referrer) else {
                    return Ok(false);
                };
                // Pinned across the registration, which allocates the key: the
                // host's module is only reachable from host memory until then.
                let resolved = resolved.module();
                let pin = resolved.pin();
                resolved.register(realm, &request.specifier)?;
                drop(pin);
                if !visited.contains(&resolved) {
                    visited.push(resolved);
                    queue.push(resolved);
                }
            }
        }
        Ok(true)
    }
}

/// The `FixedArray` of import attributes a resolve callback receives: triples of
/// key, value and source offset, the shape the crate we stand in for builds.
///
/// The offset is the requesting declaration's rather than the attribute's — see
/// [`api::ModuleRequest`](runtime::api::ModuleRequest) for why the engine keeps
/// no finer one.
fn attributes_of(
    realm: &api::Context,
    request: &api::ModuleRequest,
) -> Result<api::Local, JsError> {
    let mut elements = Vec::with_capacity(request.attributes.len() * 3);
    for (key, value) in &request.attributes {
        elements.push(api::Local::string(key.clone()));
        elements.push(api::Local::string(value.clone()));
        elements.push(api::Local::number(request.source_offset as f64));
    }
    api::Array::new(realm, &elements)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Number, Promise};
    use crate::script_compiler::{Source, compile_module};
    use crate::test_support::in_context;

    fn number_of(value: Local<'_, Value>) -> f64 {
        Local::<Number>::try_from(value).expect("number").value()
    }

    /// A module compiled through the compiler runs, reports the status a host
    /// branches on, and exposes its exports through its namespace.
    #[test]
    fn a_compiled_module_evaluates_and_exposes_its_namespace() {
        in_context!(scope, {
            let text = JsString::new(scope, "export const x = 41;").expect("string");
            let mut source = Source::new(text, None);
            let module = compile_module(scope, &mut source).expect("compile");
            assert_eq!(module.get_status(), ModuleStatus::Uninstantiated);
            assert!(module.is_source_text_module());
            assert!(!module.is_synthetic_module());

            let value = module.evaluate(scope).expect("evaluate");
            Local::<Promise>::try_from(value).expect("evaluation promise");
            assert_eq!(module.get_status(), ModuleStatus::Evaluated);

            let namespace =
                Local::<Object>::try_from(module.get_module_namespace()).expect("namespace");
            let key = JsString::new(scope, "x").expect("string").into();
            assert_eq!(number_of(namespace.get(scope, key).expect("get")), 41.0);
        });
    }

    /// A module that throws reports `Errored` and hands the exception back,
    /// which is how a host detects and explains a failed evaluation.
    #[test]
    fn an_errored_module_reports_errored_and_its_exception() {
        in_context!(scope, {
            let text = JsString::new(scope, "throw new Error('nope');").expect("string");
            let mut source = Source::new(text, None);
            let module = compile_module(scope, &mut source).expect("compile");
            assert!(module.get_exception().is_undefined());

            module.evaluate(scope).expect("evaluate");
            assert_eq!(module.get_status(), ModuleStatus::Errored);
            assert!(!module.get_exception().is_undefined());
        });
    }

    /// A host's resolve callback is what supplies every import: the module the
    /// callback returns is registered under the specifier that asked for it, so
    /// the engine's own linking finds it.
    ///
    /// The callback compiles its answer from the context it is handed, which is
    /// what a host's module map does at one remove.
    fn resolve_by_compiling<'s>(
        context: Local<'s, Context>,
        _specifier: Local<'s, JsString>,
        _attributes: Local<'s, FixedArray>,
        _referrer: Local<'s, Module>,
    ) -> Option<Local<'s, Module>> {
        crate::callback_scope!(unsafe scope, context);
        let text = JsString::new(scope, "export const x = 41;").expect("string");
        let mut source = Source::new(text, None);
        let module = compile_module(scope, &mut source)?;
        // The handle is plain data: naming the record under the callback's
        // lifetime is what the surrounding signature asks for, and the record
        // is on this frame's stack (hence a scan root) until it is registered.
        Some(Local::from_module(module.module()))
    }

    #[test]
    fn instantiate_module_resolves_each_request_through_the_host() {
        in_context!(scope, {
            let text = JsString::new(
                scope,
                "import { x } from 'dep';\nexport const doubled = x * 2;",
            )
            .expect("string");
            let mut source = Source::new(text, None);
            let module = compile_module(scope, &mut source).expect("compile");
            assert_eq!(module.get_status(), ModuleStatus::Uninstantiated);

            assert_eq!(
                module.instantiate_module(scope, resolve_by_compiling),
                Some(true)
            );
            assert_eq!(module.get_status(), ModuleStatus::Instantiated);

            module.evaluate(scope).expect("evaluate");
            assert_eq!(module.get_status(), ModuleStatus::Evaluated);

            let namespace =
                Local::<Object>::try_from(module.get_module_namespace()).expect("namespace");
            let key = JsString::new(scope, "doubled").expect("string").into();
            assert_eq!(number_of(namespace.get(scope, key).expect("get")), 82.0);
        });
    }
}
