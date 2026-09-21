//! Modules: a source text module record (v8::Module).

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

    /// The specifiers the module imports or re-exports, in source order.
    pub fn requested_specifiers(&self) -> Vec<String> {
        self.module
            .requested_modules
            .iter()
            .map(|request| request.specifier.to_string_lossy())
            .collect()
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
