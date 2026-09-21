//! Modules: a source text module record (v8::Module).

use std::num::NonZeroI32;

use crux::error::JsError;
use crux::handle::Handle;
use crux::string::JsString;

use crate::module::{self, SourceTextModule};

use super::context::Context;
use super::handle::Local;

/// Where a module is in its life (v8::ModuleStatus).
///
/// The engine splits the same span into its own six states (parsing and
/// linking happen in one call, and a body that awaits and a body that does not
/// are distinct), so this is the mapping rather than a re-export: a host reads
/// `Errored` to detect failure, exactly as in the crate we stand in for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModuleStatus {
    /// Its source is in hand, but nothing has been linked.
    Uninstantiated,
    /// Its imports are being resolved.
    Instantiating,
    /// Its imports are resolved, so it is ready to run.
    Instantiated,
    /// It is running, synchronously or awaiting.
    Evaluating,
    /// It has run.
    Evaluated,
    /// It failed, and the exception is the one [`Module::exception`] reports.
    Errored,
}

/// What kind of import a request is (v8::ModuleImportPhase).
///
/// The `k`-prefixed spellings are the ones a host writes, since they come from
/// the crate we stand in for's generated bindings.
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub enum ModuleImportPhase {
    /// An ordinary `import`, whose module is evaluated.
    kEvaluation = 0,
    /// An `import source` request, which wants the source rather than the
    /// evaluation.
    kSource = 1,
    /// An `import defer` request, which is linked but not evaluated yet.
    kDefer = 2,
}

/// One module request (v8::ModuleRequest): a specifier this module imports or
/// re-exports, in source order.
///
/// `source_offset` is the offset of the declaration that asked for it, not of
/// the specifier: the engine's syntax tree records one span per declaration, so
/// a location reported for a request points at its `import`/`export`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleRequest {
    pub specifier: String,
    pub attributes: Vec<(String, String)>,
    pub phase: ModuleImportPhase,
    pub source_offset: u32,
}

/// A source text module record (v8::Module).
///
/// The record is reachable from the realm for as long as the realm lives, so
/// this handle only names it.
#[derive(Clone, Copy)]
pub struct Module {
    module: Handle<SourceTextModule>,
}

impl Module {
    /// Parse `source` as a module in `context`'s realm
    /// (v8::ScriptCompiler::CompileModule).
    pub fn compile(context: &Context, specifier: &str, source: &str) -> Result<Self, JsError> {
        let specifier = JsString::from_utf8(specifier);
        let source = JsString::from_utf8(source);
        context.with_agent(|agent| {
            module::parse_module(agent, &specifier, &source, &[]).map(|module| Self { module })
        })
    }

    /// Name the module, so linking resolves imports of it without asking the
    /// host again — which is what the engine does for every module a host
    /// hands it.
    pub fn register(&self, context: &Context, specifier: &str) -> Result<(), JsError> {
        context.with_agent(|agent| {
            let realm = agent.current_realm()?;
            crux::heap::write_barrier_handle(&*realm, self.module);
            realm
                .loaded_modules
                .borrow_mut()
                .insert(JsString::from_utf8(specifier), self.module);
            Ok(())
        })
    }

    /// The module's status (v8::Module::GetStatus). An errored record reports
    /// `Errored` whatever state its body reached, which is how a host detects a
    /// failed evaluation without the exception value.
    pub fn status(&self) -> ModuleStatus {
        if self.exception().is_some() {
            return ModuleStatus::Errored;
        }
        match *self.module.status.borrow() {
            module::ModuleStatus::Unlinked => ModuleStatus::Uninstantiated,
            module::ModuleStatus::Linking => ModuleStatus::Instantiating,
            module::ModuleStatus::Linked => ModuleStatus::Instantiated,
            module::ModuleStatus::Evaluating | module::ModuleStatus::EvaluatingAsync => {
                ModuleStatus::Evaluating
            }
            module::ModuleStatus::Evaluated => ModuleStatus::Evaluated,
        }
    }

    /// The record's identity hash (v8::Module::GetIdentityHash), for a host that
    /// keys a table by module.
    ///
    /// A module has no identity a language value could carry, so what this
    /// hashes is the address its box lives at — the same thing `PartialEq`
    /// compares, and stable for as long as the record is alive, because the
    /// arena never moves a box. Folded to the width the crate's API answers
    /// with, and forced non-zero so it cannot be mistaken for an absent hash.
    pub fn get_identity_hash(&self) -> NonZeroI32 {
        let address = self.module.as_any().addr();
        // Any odd fold of an address is a usable hash and, being odd, is never
        // zero; the fallback is unreachable and exists to keep this total.
        NonZeroI32::new((address as u32 | 1) as i32).unwrap_or(NonZeroI32::MIN)
    }

    /// Keep the module, and everything it reaches, alive until the returned
    /// pin is dropped.
    ///
    /// The record is not a language value, so no handle scope holds it: a
    /// persistent host handle has to root it itself.
    pub fn pin(&self) -> crux::heap::Pin {
        crux::heap::pin_handle(self.module)
    }

    /// Whether the module's own body awaits ([[HasTLA]]).
    pub fn has_top_level_await(&self, context: &Context) -> Result<bool, JsError> {
        context.with_agent(|agent| module::module_has_tla(agent, &self.module))
    }

    /// Whether any module the graph reaches from this one awaits
    /// (v8::Module::IsGraphAsync).
    ///
    /// V8's own walk (`Module::IsGraphAsync`, `src/objects/module.cc:614`):
    /// this module first, then every `SourceTextModule` its requests reached
    /// once it was linked, until one answers [[HasTLA]]. The engine resolves
    /// from its own link table, so a module whose specifier nothing registered
    /// is not walked — which is what a not-yet-linked request is.
    pub fn is_graph_async(&self, context: &Context) -> Result<bool, JsError> {
        context.with_agent(|agent| module::module_graph_has_tla(agent, &self.module))
    }

    /// Where `offset` into this module's source is
    /// (v8::Module::SourceOffsetToLocation), as a 1-based line and column
    /// (the crate we stand in for's `Location` is 0-based; its callers add
    /// one, and `crates/v8` subtracts it again).
    ///
    /// # Panics
    ///
    /// On a module that is not a source text module, which is the crate's own
    /// check (`Utils::ApiCheck`, `src/api/api.cc:2340`).
    pub fn source_offset_to_location(&self, offset: u32) -> crux::SourceLocation {
        let text = syntax::SourceText::from_utf16(self.module.source.as_slice().to_vec());
        text.line_column(offset)
    }

    /// The module's deferred namespace object (spec 16.2.1.10 with ~defer~),
    /// evaluated lazily on first property access.
    pub fn deferred_namespace(&self, context: &Context) -> Result<Local, JsError> {
        context.with_agent(|agent| module::deferred_namespace(agent, &self.module).map(Local))
    }

    /// The specifiers the module imports or re-exports, in source order.
    pub fn requested_specifiers(&self) -> Vec<String> {
        self.module
            .requested_modules
            .iter()
            .map(|request| request.specifier.to_string_lossy())
            .collect()
    }

    /// How many requests the module has (the length of the array
    /// [`requested_modules`](Self::requested_modules) builds).
    pub fn request_count(&self) -> usize {
        self.module.requested_modules.len()
    }

    /// The request at `index`, without building the whole list
    /// (`v8::Module::GetModuleRequests` reads one element at a time through
    /// its array, which is what this pairs with).
    pub fn request(&self, index: usize) -> Option<ModuleRequest> {
        let request = self.module.requested_modules.get(index)?;
        Some(ModuleRequest {
            specifier: request.specifier.to_string_lossy(),
            attributes: request
                .attributes
                .iter()
                .map(|(key, value)| (module::key_string(key), value.to_string_lossy()))
                .collect(),
            phase: match request.phase {
                syntax::ast::ImportPhase::Import => ModuleImportPhase::kEvaluation,
                syntax::ast::ImportPhase::Source => ModuleImportPhase::kSource,
                syntax::ast::ImportPhase::Defer => ModuleImportPhase::kDefer,
            },
            source_offset: request.span.start,
        })
    }

    /// The module's requests, in source order, with the attributes, phase and
    /// source offset each was written with
    /// (v8::Module::GetModuleRequests).
    pub fn requested_modules(&self) -> Vec<ModuleRequest> {
        self.module
            .requested_modules
            .iter()
            .map(|request| ModuleRequest {
                specifier: request.specifier.to_string_lossy(),
                attributes: request
                    .attributes
                    .iter()
                    .map(|(key, value)| (module::key_string(key), value.to_string_lossy()))
                    .collect(),
                phase: match request.phase {
                    syntax::ast::ImportPhase::Import => ModuleImportPhase::kEvaluation,
                    syntax::ast::ImportPhase::Source => ModuleImportPhase::kSource,
                    syntax::ast::ImportPhase::Defer => ModuleImportPhase::kDefer,
                },
                source_offset: request.span.start,
            })
            .collect()
    }

    /// Link the module and everything it imports (spec 16.2.1.6.1.2).
    pub fn instantiate(&self, context: &Context) -> Result<(), JsError> {
        context.with_agent(|agent| module::module_declaration_instantiation(agent, &self.module))
    }

    /// Evaluate the module (spec 16.2.1.6.2.1); the value is a promise.
    pub fn evaluate(&self, context: &Context) -> Result<Local, JsError> {
        context.with_agent(|agent| module::module_evaluation(agent, &self.module).map(Local))
    }

    /// The module's namespace object (spec 16.2.1.10).
    pub fn namespace(&self, context: &Context) -> Result<Local, JsError> {
        context.with_agent(|agent| module::module_namespace(agent, &self.module).map(Local))
    }

    /// The error the module failed with, if it has failed.
    pub fn exception(&self) -> Option<Local> {
        (*self.module.evaluation_error.borrow()).map(Local)
    }
}

/// Two handles name the same module record when they point at the same box: a
/// module has no identity a language value could carry.
impl PartialEq for Module {
    fn eq(&self, other: &Self) -> bool {
        self.module.ptr_eq(other.module)
    }
}

impl Eq for Module {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Context, Isolate, Object};

    /// A module compiles, links against a registered dependency, evaluates in
    /// order, and exposes its exports through a namespace.
    #[test]
    fn a_module_links_evaluates_and_exposes_a_namespace() {
        let mut isolate = Isolate::new();
        let context = Context::new(&mut isolate).expect("context");

        let dependency =
            Module::compile(&context, "dep", "export const answer = 42;").expect("dep");
        dependency.register(&context, "dep").expect("register");
        let main = Module::compile(
            &context,
            "main",
            "import { answer } from 'dep';\nexport const doubled = answer * 2;",
        )
        .expect("main");

        assert_eq!(main.status(), ModuleStatus::Uninstantiated);
        assert_eq!(main.requested_specifiers(), ["dep"]);
        main.register(&context, "main").expect("register");

        main.instantiate(&context).expect("instantiate");
        assert_eq!(main.status(), ModuleStatus::Instantiated);

        main.evaluate(&context).expect("evaluate");
        assert_eq!(main.status(), ModuleStatus::Evaluated);
        assert!(!main.has_top_level_await(&context).expect("tla"));

        let namespace = main.namespace(&context).expect("namespace");
        assert!(namespace.value().is_object());
        assert_eq!(
            Object::get(&context, &namespace, "doubled")
                .expect("get")
                .as_number(),
            Some(84.0)
        );
    }

    /// A module that throws records the error, and the status says it evaluated
    /// with one — which is what the host reports as `errored`.
    #[test]
    fn a_failing_module_records_its_error() {
        let mut isolate = Isolate::new();
        let context = Context::new(&mut isolate).expect("context");

        let module =
            Module::compile(&context, "boom", "throw new Error('nope');").expect("compile");
        module.register(&context, "boom").expect("register");
        module.instantiate(&context).expect("instantiate");
        module.evaluate(&context).expect("evaluate");
        assert!(module.exception().is_some());
        assert_eq!(module.status(), ModuleStatus::Errored);
    }

    /// A syntax error is reported where the module is compiled.
    #[test]
    fn a_syntax_error_is_reported_at_compile_time() {
        let mut isolate = Isolate::new();
        let context = Context::new(&mut isolate).expect("context");
        assert!(Module::compile(&context, "bad", "export const = 1;").is_err());
    }
}
