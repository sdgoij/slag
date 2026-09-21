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
//! - A synthetic module's steps that fail leave the module errored with that
//!   failure as its exception, and its evaluation answers the rejection the
//!   engine made from it; the crate we stand in for answers the empty handle
//!   there instead.

use std::num::NonZeroI32;

use crux::error::JsError;
use runtime::api;

use crate::data::{Context, FixedArray, Module, ModuleRequest, Object, String as JsString, Value};
use crate::handle::{Local, LocalHandle, Payload};
use crate::primitives::undefined;
use crate::scope::PinScope;
use crate::support::{MapFnFrom, MapFnTo, UnitType};

pub use runtime::api::{ModuleImportPhase, ModuleStatus};

/// What a synthetic module's evaluation runs
/// (v8::SyntheticModuleEvaluationSteps).
///
/// The crate we stand in for spells this as two C function pointers — one per
/// platform, because `MaybeLocal<Value>` is returned differently on Windows —
/// and a host's own function is converted into one by `MapFnTo`. Nothing here
/// crosses a language boundary, so it is one plain function pointer with the
/// same argument list, and `None` is the empty `MaybeLocal` (the steps threw).
pub type SyntheticModuleEvaluationSteps<'s> =
    fn(Local<'s, Context>, Local<'s, Module>) -> Option<Local<'s, Value>>;

impl<'s, F> MapFnFrom<F> for SyntheticModuleEvaluationSteps<'s>
where
    F: UnitType + Fn(Local<'s, Context>, Local<'s, Module>) -> Option<Local<'s, Value>>,
{
    fn mapping() -> Self {
        synthetic_module_evaluation_steps::<F>
    }
}

/// The concrete function that stands in for a host's steps: a generic type
/// parameter cannot coerce to a function pointer, so the host's function item is
/// reconstructed and called from one that can.
fn synthetic_module_evaluation_steps<'s, F>(
    context: Local<'s, Context>,
    module: Local<'s, Module>,
) -> Option<Local<'s, Value>>
where
    F: UnitType + Fn(Local<'s, Context>, Local<'s, Module>) -> Option<Local<'s, Value>>,
{
    (F::get())(context, module)
}

/// The function the engine keeps for a synthetic module's evaluation: the
/// host's steps, reached through their *type* rather than through a value.
///
/// The record holds this for as long as the record lives, so it is a plain
/// function pointer with no lifetime of its own. The host's steps cannot be
/// closed over the way `instantiate_module`'s callback is: the engine calls them
/// long after the scopes the host had open when it declared the module are gone,
/// so the handles they receive are made at the call rather than borrowed from a
/// host scope.
fn engine_evaluation_steps<F>(
    context: api::Context,
    module: api::Module,
) -> Result<api::Local, crux::error::JsError>
where
    F: UnitType + for<'s> Fn(Local<'s, Context>, Local<'s, Module>) -> Option<Local<'s, Value>>,
{
    let answered = (F::get())(
        Local::from_payload(Payload::Context(context)),
        Local::from_module(module),
    );
    match answered {
        Some(value) => Ok(value.into_engine()),
        // The empty handle is how the crate's steps report a throw, and the
        // pending exception is what V8 records as the module's error
        // (`SyntheticModule::Evaluate`,
        // `v8/src/objects/synthetic-module.cc:132`). Nothing pending means the
        // steps answered nothing at all, which is their `undefined`.
        None => match crate::scope::isolate_of(context)
            .engine()
            .take_pending_exception()
        {
            Some(thrown) => Err(crux::error::JsError::new(
                crux::error::ErrorKind::TypeError,
                "exception thrown by a synthetic module's evaluation steps".into(),
            )
            .with_value(thrown)),
            None => Ok(api::Local::undefined()),
        },
    }
}

/// A location in JavaScript source (v8::Location).
///
/// The crate we stand in for fills this from V8's `Script::PositionInfo`, whose
/// two numbers are **0-based** — its own callers add one to report a position
/// to a user. The engine's [`SourceLocation`](crux::SourceLocation) is 1-based,
/// so the conversion is here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    line_number: i32,
    column_number: i32,
}

impl Location {
    /// The line, 0-based (v8::Location::GetLineNumber).
    pub fn get_line_number(&self) -> i32 {
        self.line_number
    }

    /// The column, 0-based (v8::Location::GetColumnNumber).
    pub fn get_column_number(&self) -> i32 {
        self.column_number
    }
}

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

impl Module {
    /// Create a synthetic module (v8::Module::CreateSyntheticModule): a record
    /// that carries no source, exposes the exports its host declares, and
    /// evaluates by running `evaluation_steps`.
    ///
    /// `export_names` are the bindings the module exposes and must not repeat;
    /// `module_name` is for logging and changes no behavior. What the steps
    /// answer is the evaluation promise, or the empty handle if they threw.
    ///
    /// The steps are taken by value as there, but only their *type* is used: the
    /// engine keeps a plain function pointer for them, reconstructed from that
    /// type (see [`engine_evaluation_steps`]).
    ///
    /// Linking and evaluating a synthetic module cannot fail except through its
    /// steps, so a failure here is the engine refusing the empty program the
    /// record is built from: it sets the exception and aborts, as the crate we
    /// stand in for does on the same result.
    pub fn create_synthetic_module<'s, 'i, F>(
        scope: &PinScope<'s, 'i>,
        module_name: Local<'s, JsString>,
        export_names: &[Local<'s, JsString>],
        _evaluation_steps: F,
    ) -> Local<'s, Module>
    where
        F: UnitType + for<'a> Fn(Local<'a, Context>, Local<'a, Module>) -> Option<Local<'a, Value>>,
    {
        let realm = scope.get_current_context().context();
        let names: Vec<api::Local> = export_names.iter().map(|name| name.into_engine()).collect();
        let module = api::Module::create_synthetic_module(
            &realm,
            &module_name.into_engine(),
            &names,
            engine_evaluation_steps::<F>,
        );
        match module {
            Ok(module) => Local::from_module(module),
            Err(error) => {
                crate::throw(scope, &error);
                panic!("bridge: creating a synthetic module failed: {error}");
            }
        }
    }
}

impl<'s> LocalHandle<'s, Module> {
    /// The record's identity hash (v8::Module::GetIdentityHash), for a host that
    /// keys a table by module.
    pub fn get_identity_hash(&self) -> NonZeroI32 {
        self.module().get_identity_hash()
    }

    /// The module's current status (v8::Module::GetStatus).
    pub fn get_status(&self) -> ModuleStatus {
        self.module().status()
    }

    /// For a module in `Errored` status, the exception it failed with
    /// (v8::Module::GetException).
    ///
    /// The crate we stand in for returns an empty handle for a module that has
    /// not failed, which its own callers unwrap; *undefined* is the honest value
    /// for the same case here.
    pub fn get_exception(&self) -> Local<'s, Value> {
        match self.module().exception() {
            Some(value) => Local::from_engine(value),
            None => undefined(&()).into(),
        }
    }

    /// The module's namespace object (v8::Module::GetModuleNamespace).
    ///
    /// The crate we stand in for takes no scope here because it has the current
    /// one on the stack; the bridge reads the entered context instead.
    pub fn get_module_namespace(&self) -> Local<'s, Value> {
        let realm = crate::realm_current();
        match self.module().namespace(&realm) {
            Ok(value) => Local::from_engine(value),
            Err(error) => panic!("bridge: module namespace creation failed: {error}"),
        }
    }

    /// The module's namespace object for `phase`
    /// (v8::Module::GetModuleNamespace with an import phase).
    ///
    /// The engine keeps the two namespaces V8 keeps — the eager one and the
    /// deferred one that evaluates on first property access — so this is the
    /// choice between them. An *evaluation* request answers the former and
    /// anything else the latter: V8's own check is that the phase is one of
    /// those two (`DCHECK`, `src/objects/module.cc:340`), and its release
    /// build's fallthrough for anything else is the deferred namespace, which
    /// is what a *source* phase would get here too.
    pub fn get_module_namespace_with_phase(&self, phase: ModuleImportPhase) -> Local<'s, Value> {
        let realm = crate::realm_current();
        let namespace = match phase {
            ModuleImportPhase::kEvaluation => self.module().namespace(&realm),
            ModuleImportPhase::kSource | ModuleImportPhase::kDefer => {
                self.module().deferred_namespace(&realm)
            }
        };
        match namespace {
            Ok(value) => Local::from_engine(value),
            Err(error) => panic!("bridge: module namespace creation failed: {error}"),
        }
    }

    /// Whether any module this one reaches awaits
    /// (v8::Module::IsGraphAsync).
    ///
    /// The engine walks the graph itself, over the modules the host has
    /// registered, so this answers before or after linking alike.
    pub fn is_graph_async(&self) -> bool {
        let realm = crate::realm_current();
        self.module().is_graph_async(&realm).unwrap_or(false)
    }

    /// Where `offset` into this module's source is
    /// (v8::Module::SourceOffsetToLocation).
    ///
    /// For a module request's offset, convert
    /// `ModuleRequest::get_source_offset` with this.
    pub fn source_offset_to_location(&self, offset: i32) -> Location {
        let location = self
            .module()
            .source_offset_to_location(offset.max(0) as u32);
        Location {
            // The engine reports 1-based numbers and V8's Location is 0-based
            // in both.
            line_number: location.line as i32 - 1,
            column_number: location.column as i32 - 1,
        }
    }

    /// The module's requests, in source order
    /// (v8::Module::GetModuleRequests).
    ///
    /// V8 answers a `FixedArray` of request objects; the engine keeps them in
    /// the module's own record, so the array here names the record and each
    /// element names a position in it (see
    /// [`FixedArray`](crate::data::FixedArray)). A host reads it the same way:
    /// a length and `get`.
    pub fn get_module_requests(&self) -> Local<'s, FixedArray> {
        Local::from_payload(Payload::ModuleRequests {
            module: self.module(),
        })
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
        &self,
        scope: &PinScope<'s2, 'i>,
        callback: impl MapFnTo<ResolveModuleCallback<'s2>>,
    ) -> Option<bool> {
        self.instantiate(scope, callback.map_fn_to())
    }

    /// Link the module, taking the source callback a host passes as well
    /// (v8::Module::InstantiateModule with a source resolver).
    pub fn instantiate_module2<'s2, 'i>(
        &self,
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
    pub fn evaluate(&self, scope: &PinScope<'s, '_>) -> Option<Local<'s, Value>> {
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
    pub fn is_source_text_module(&self) -> bool {
        self.module().is_source_text_module()
    }

    /// Whether this module is a synthetic module
    /// (v8::Module::IsSyntheticModule): a record whose exports its host
    /// declared, rather than one parsed from source.
    pub fn is_synthetic_module(&self) -> bool {
        self.module().is_synthetic_module()
    }

    /// Set one of this synthetic module's exports
    /// (v8::Module::SetSyntheticModuleExport).
    ///
    /// The module must be one
    /// [`create_synthetic_module`](Module::create_synthetic_module) made and
    /// must have been instantiated, and `export_name` must be one it declared —
    /// V8 makes the cells the names bind to at instantiation — so anything else
    /// throws and answers `None`, the crate's empty `Maybe<bool>`. An export set
    /// again is the mutable binding V8 makes it, which a namespace read follows.
    pub fn set_synthetic_module_export<'s2>(
        &self,
        scope: &PinScope<'s2, '_>,
        export_name: Local<'s2, JsString>,
        export_value: Local<'s2, Value>,
    ) -> Option<bool> {
        let set = self
            .module()
            .set_synthetic_module_export(&export_name.into_engine(), &export_value.into_engine());
        match set {
            Ok(set) => Some(set),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// Resolve every request in the graph, then link.
    fn instantiate<'s2, 'i>(
        &self,
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
        &self,
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
                    Local::<FixedArray>::from_engine(attributes_of(realm, &request)?).retag();
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

impl LocalHandle<'_, ModuleRequest> {
    /// The module specifier this request names (v8::ModuleRequest::GetSpecifier).
    pub fn get_specifier(&self) -> Local<'static, JsString> {
        Local::from_engine(api::Local::string(self.request().specifier))
    }

    /// What kind of import this is (v8::ModuleRequest::GetPhase).
    pub fn get_phase(&self) -> ModuleImportPhase {
        self.request().phase
    }

    /// The source offset of the declaration that asked for this request
    /// (v8::ModuleRequest::GetSourceOffset).
    ///
    /// Convert it with
    /// [`Module::source_offset_to_location`](LocalHandle::source_offset_to_location).
    pub fn get_source_offset(&self) -> i32 {
        self.request().source_offset as i32
    }

    /// The request's import attributes, as the triples a resolve callback gets:
    /// `[key1, value1, offset1, key2, value2, offset2, ...]`
    /// (v8::ModuleRequest::GetImportAttributes).
    ///
    /// The offsets are the requesting declaration's, the same divergence
    /// [`attributes_of`] records for the callback's array.
    pub fn get_import_attributes(&self) -> Local<'static, FixedArray> {
        // No scope in the crate's signature, so the realm is the one entered on
        // this thread, as `FixedArray::length` also reads it.
        let realm = crate::realm_current();
        match attributes_of(&realm, &self.request()) {
            Ok(elements) => Local::from_engine(elements),
            Err(error) => panic!("bridge: building a request's attributes failed: {error}"),
        }
    }

    /// The engine's own record of the request this handle names.
    ///
    /// # Panics
    ///
    /// On a handle that did not come from
    /// [`Module::get_module_requests`](LocalHandle::get_module_requests), which
    /// the tag check already refuses.
    fn request(&self) -> api::ModuleRequest {
        let (module, index) = self
            .payload()
            .as_module_request()
            .expect("bridge bug: a ModuleRequest handle without a request");
        module
            .request(index as usize)
            .expect("bridge bug: a ModuleRequest handle outlived its list")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Number, Promise, PromiseResolver};
    use crate::handle::Global;
    use crate::promise::PromiseState;
    use crate::scope::GetIsolate;
    use crate::script_compiler::{Source, compile_module};
    use crate::test_support::in_context;

    fn number_of(value: Local<'_, Value>) -> f64 {
        Local::<Number>::try_from(value).expect("number").value()
    }

    /// Compile `text` as a module, in the scope's realm.
    fn compile_text<'s>(scope: &mut PinScope<'s, '_>, text: &str) -> Local<'s, Module> {
        let text = JsString::new(scope, text).expect("string");
        let mut source = Source::new(text, None);
        compile_module(scope, &mut source).expect("compile")
    }

    /// A resolver that answers every request with a module that awaits.
    fn resolve_with_tla<'s>(
        context: Local<'s, Context>,
        _specifier: Local<'s, JsString>,
        _attributes: Local<'s, FixedArray>,
        _referrer: Local<'s, Module>,
    ) -> Option<Local<'s, Module>> {
        crate::callback_scope!(unsafe scope, context);
        let module = compile_text(scope, "await 0;\nexport const x = 1;");
        Some(Local::from_module(module.module()))
    }

    /// A module's requests are read one at a time, in source order, with the
    /// specifier, phase, source offset and attributes each was written with —
    /// which is the whole of what `deno_core` reads out of
    /// `GetModuleRequests`.
    #[test]
    fn a_modules_requests_are_read_one_at_a_time() {
        in_context!(scope, {
            let module = compile_text(
                scope,
                "import { a } from 'one';\n\
                 import * as ns from 'two' with { type: 'json' };\n\
                 import defer * as deferred from 'three';\n",
            );
            let requests = module.get_module_requests();
            assert_eq!(requests.length(), 3);

            let read = |index: usize| {
                let element = requests.get(scope, index).expect("a request");
                Local::<ModuleRequest>::try_from(element).expect("a request")
            };

            assert_eq!(read(0).get_specifier().to_rust_string_lossy(scope), "one");
            assert_eq!(read(0).get_phase(), ModuleImportPhase::kEvaluation);
            assert_eq!(read(0).get_source_offset(), 0);
            assert_eq!(read(0).get_import_attributes().length(), 0);

            assert_eq!(read(1).get_specifier().to_rust_string_lossy(scope), "two");
            assert_eq!(read(1).get_phase(), ModuleImportPhase::kEvaluation);
            let attributes = read(1).get_import_attributes();
            assert_eq!(attributes.length(), 3);
            let attribute = |index: usize| {
                let element = attributes.get(scope, index).expect("an attribute");
                Local::<Value>::try_from(element).expect("a value")
            };
            assert_eq!(attribute(0).to_rust_string_lossy(scope), "type");
            assert_eq!(attribute(1).to_rust_string_lossy(scope), "json");

            assert_eq!(read(2).get_specifier().to_rust_string_lossy(scope), "three");
            assert_eq!(read(2).get_phase(), ModuleImportPhase::kDefer);

            // Past the end there is no element, and neither is there a request.
            assert!(requests.get(scope, 3).is_none());
        });
    }

    /// A request's source offset names the declaration that asked for it, and
    /// `source_offset_to_location` converts it — 0-based, as the crate we stand
    /// in for reports it.
    #[test]
    fn a_requests_offset_names_a_location() {
        in_context!(scope, {
            let module = compile_text(scope, "import 'one';\nimport 'two';");
            let requests = module.get_module_requests();
            let offset = |index: usize| {
                let element = requests.get(scope, index).expect("a request");
                Local::<ModuleRequest>::try_from(element)
                    .expect("a request")
                    .get_source_offset()
            };

            let first = module.source_offset_to_location(offset(0));
            assert_eq!((first.get_line_number(), first.get_column_number()), (0, 0));
            let second = module.source_offset_to_location(offset(1));
            assert_eq!(
                (second.get_line_number(), second.get_column_number()),
                (1, 0)
            );
        });
    }

    /// Only what `get_module_requests` hands out is a request: the element of
    /// any other fixed array is not, which is what a payload-based cast buys
    /// over a test that a host's own array could satisfy.
    #[test]
    fn a_plain_element_is_not_a_module_request() {
        in_context!(scope, {
            let module = compile_text(scope, "import * as ns from 'two' with { type: 'json' };");
            let requests = module.get_module_requests();
            let request = requests.get(scope, 0).expect("a request");
            assert!(Local::<ModuleRequest>::try_from(request).is_ok());

            let element = requests
                .get(scope, 0)
                .and_then(|request| Local::<ModuleRequest>::try_from(request).ok())
                .expect("a request")
                .get_import_attributes()
                .get(scope, 0)
                .expect("an attribute");
            assert!(Local::<ModuleRequest>::try_from(element).is_err());
        });
    }

    /// The graph's async-ness follows the modules a root reaches, not the root
    /// alone, and only a module the host registered is in the graph the engine
    /// walks.
    #[test]
    fn the_graph_is_async_when_a_module_it_reaches_awaits() {
        in_context!(scope, {
            let main = compile_text(scope, "import { x } from 'dep';");
            // Nothing registered yet, and the root itself does not await.
            assert!(!main.is_graph_async());

            assert_eq!(
                main.instantiate_module(scope, resolve_by_compiling),
                Some(true)
            );
            assert!(!main.is_graph_async());

            let awaiting = compile_text(scope, "import { x } from 'dep';");
            assert_eq!(
                awaiting.instantiate_module(scope, resolve_with_tla),
                Some(true)
            );
            assert!(awaiting.is_graph_async());
        });
    }

    /// A module has the two namespaces V8 gives it: the eager one, and a
    /// deferred one that is a different object and evaluates lazily.
    #[test]
    fn the_deferred_namespace_is_not_the_eager_one() {
        in_context!(scope, {
            let module = compile_text(scope, "export const x = 1;");
            assert_eq!(
                module.instantiate_module(scope, resolve_by_compiling),
                Some(true)
            );

            let eager = module.get_module_namespace();
            let by_phase = module.get_module_namespace_with_phase(ModuleImportPhase::kEvaluation);
            assert!(eager == by_phase);

            let deferred = module.get_module_namespace_with_phase(ModuleImportPhase::kDefer);
            assert!(eager != deferred);
            assert!(!deferred.is_undefined());
        });
    }

    /// A handle made inside a callback scope can be returned from it, which is
    /// the shape every resolver in `deno_core` has: the callback opens its own
    /// scope and answers with a handle made there. The scope's *borrow* is not
    /// what a handle is valid for — a handle is the value it carries — so the
    /// answer takes the callback's own lifetime.
    #[test]
    fn a_handle_made_in_a_callback_scope_can_be_returned_from_it() {
        let isolate = &mut crate::Isolate::new(crate::CreateParams::default());
        crate::scope!(let handle_scope, isolate);
        let context = Context::new(handle_scope, Default::default());
        let scope = &mut crate::ContextScope::new(handle_scope, context);
        let value: Local<'_, Value> = JsString::new(scope, "held").expect("string").into();
        let held = Global::<Value>::new(scope, value);

        fn resolver<'s>(context: Local<'s, Context>, held: &Global<Value>) -> Local<'s, Value> {
            crate::callback_scope!(unsafe scope, context);
            Local::new(scope, held.clone())
        }

        let answered = resolver(context, &held);
        assert_eq!(answered.to_rust_string_lossy(scope), "held");
    }

    /// A handle a *method* hands out under a callback scope is as long-lived as
    /// the thing the scope was opened from, not as long-lived as the borrow of
    /// the scope's storage — which is what a helper needs in order to open a
    /// callback scope and a try-catch inside it and still answer its caller's
    /// lifetime. `deno_core`'s synthetic-module steps are exactly that shape: a
    /// resolver's promise, taken under a `tc_scope!` inside a `callback_scope!`.
    #[test]
    fn a_promise_taken_under_a_callback_scopes_try_catch_holds_the_callbacks_lifetime() {
        let isolate = &mut crate::Isolate::new(crate::CreateParams::default());
        crate::scope!(let handle_scope, isolate);
        let context = Context::new(handle_scope, Default::default());
        let _scope = &mut crate::ContextScope::new(handle_scope, context);

        fn settle<'s>(context: Local<'s, Context>) -> Option<Local<'s, Value>> {
            crate::callback_scope!(unsafe scope, context);
            crate::tc_scope!(tc_scope, scope);
            let resolver = PromiseResolver::new(tc_scope).expect("resolver");
            let value = Number::new(tc_scope, 42.0).into();
            assert_eq!(resolver.resolve(tc_scope, value), Some(true));
            let promise = resolver.get_promise(tc_scope);
            // No re-wrapping: this conversion is what the fix buys, and it stops
            // compiling the moment a callback scope's handles go back to being
            // typed with the borrow of its storage.
            Some(promise.into())
        }

        let answered = settle(context).expect("a promise");
        let promise = Local::<Promise>::try_from(answered).expect("promise");
        assert_eq!(promise.state(), PromiseState::Fulfilled);
    }

    /// A module handle keys a host's table the way the crate's does: one record
    /// under two handles is one key, a different record is another, and that
    /// holds for a persistent handle too — which it cannot unless the hash
    /// agrees with `==`.
    #[test]
    fn a_module_handle_keys_a_table() {
        in_context!(scope, {
            let text = JsString::new(scope, "export const x = 1;").expect("string");
            let mut source = Source::new(text, None);
            let module = compile_module(scope, &mut source).expect("compile");
            let same = module;

            let other_text = JsString::new(scope, "export const y = 2;").expect("string");
            let mut other_source = Source::new(other_text, None);
            let other = compile_module(scope, &mut other_source).expect("compile");

            assert_eq!(module, same);
            assert_eq!(module.get_identity_hash(), same.get_identity_hash());
            assert_ne!(module.get_identity_hash(), other.get_identity_hash());

            let mut keys = std::collections::HashSet::new();
            keys.insert(module);
            keys.insert(same);
            assert_eq!(keys.len(), 1, "one record is one key");
            keys.insert(other);
            assert_eq!(keys.len(), 2, "and a different record is a different key");
            assert!(keys.contains(&module));

            let isolate = scope.get_isolate_ptr();
            let mut persistent = std::collections::HashSet::new();
            persistent.insert(Global::new(&isolate, module));
            persistent.insert(Global::new(&isolate, same));
            assert_eq!(persistent.len(), 1);
            assert!(!persistent.contains(&Global::new(&isolate, other)));
        });
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

    /// The steps a synthetic module runs: one export set in a callback scope of
    /// their own, and the promise the crate's steps answer.
    fn synthetic_steps<'s>(
        context: Local<'s, Context>,
        module: Local<'s, Module>,
    ) -> Option<Local<'s, Value>> {
        crate::callback_scope!(unsafe scope, context);
        let name = JsString::new(scope, "answer").expect("string");
        let value = Number::new(scope, 42.0).into();
        assert_eq!(
            module.set_synthetic_module_export(scope, name, value),
            Some(true),
            "the host's own export"
        );
        let resolver = PromiseResolver::new(scope).expect("resolver");
        let promise = resolver.get_promise(scope);
        // A handle is the value it carries, and an engine value outlives the
        // scope it was made in — what a scope's region holds is compiled
        // source, which this names none of.
        Some(Local::from_engine(promise.into_engine()))
    }

    /// Steps that refuse their own module's export, which is how a host's steps
    /// report a throw: the empty handle, with the exception pending.
    fn refusing_steps<'s>(
        context: Local<'s, Context>,
        module: Local<'s, Module>,
    ) -> Option<Local<'s, Value>> {
        crate::callback_scope!(unsafe scope, context);
        let name = JsString::new(scope, "undeclared").expect("string");
        let value = Number::new(scope, 1.0).into();
        assert_eq!(module.set_synthetic_module_export(scope, name, value), None);
        None
    }

    /// A synthetic module carries no source: what it exposes comes from the
    /// steps its host declared, which the engine calls back into with handles
    /// the host's own scopes cannot name.
    #[test]
    fn a_synthetic_module_runs_the_steps_its_host_declared() {
        in_context!(scope, {
            let name = JsString::new(scope, "host").expect("string");
            let names = [JsString::new(scope, "answer").expect("string")];
            let module = Module::create_synthetic_module(scope, name, &names, synthetic_steps);

            assert!(module.is_synthetic_module());
            assert!(!module.is_source_text_module());
            assert_eq!(module.get_status(), ModuleStatus::Uninstantiated);

            let value = module.evaluate(scope).expect("evaluate");
            Local::<Promise>::try_from(value).expect("evaluation promise");
            assert_eq!(module.get_status(), ModuleStatus::Evaluated);

            let namespace =
                Local::<Object>::try_from(module.get_module_namespace()).expect("namespace");
            let key = JsString::new(scope, "answer").expect("string").into();
            assert_eq!(number_of(namespace.get(scope, key).expect("get")), 42.0);
        });
    }

    /// Steps that fail leave the module errored with the failure they threw as
    /// its exception, which is what the crate's evaluation answers the empty
    /// handle for.
    #[test]
    fn a_synthetic_modules_failure_becomes_its_exception() {
        in_context!(scope, {
            let name = JsString::new(scope, "host").expect("string");
            let names = [JsString::new(scope, "answer").expect("string")];
            let module = Module::create_synthetic_module(scope, name, &names, refusing_steps);
            assert!(module.get_exception().is_undefined());

            let value = module.evaluate(scope).expect("evaluate");
            assert_eq!(module.get_status(), ModuleStatus::Errored);
            assert!(!module.get_exception().is_undefined());
            assert_eq!(
                Local::<Promise>::try_from(value)
                    .expect("evaluation promise")
                    .state(),
                PromiseState::Rejected
            );
        });
    }
}
