//! Modules: a source text module record (v8::Module).

use std::num::NonZeroI32;

use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::string::JsString;

use crate::module::{self, SourceTextModule};

use super::context::Context;
use super::handle::Local;

/// The text a value holds, or a `TypeError` naming what asked for it.
fn string_value(value: &Local, what: &str) -> Result<JsString, JsError> {
    value
        .value()
        .as_string()
        .map(|text| JsString::owned_of(&text))
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, format!("{what} is not a string")))
}

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

/// What a synthetic module's evaluation runs
/// (v8::SyntheticModuleEvaluationSteps).
///
/// The engine hands the callback the module's context and the module during
/// evaluation. What it answers is the evaluation promise — V8's steps return a
/// promise, which becomes the module's [[TopLevelCapability]] — and `Err` is
/// the failure the module records and every later evaluation answers.
pub type SyntheticModuleEvaluationSteps = fn(Context, Module) -> Result<Local, JsError>;

/// A source text module record (v8::Module).
///
/// The record is reachable from the realm for as long as the realm lives, so
/// this handle only names it.
#[derive(Clone, Copy)]
pub struct Module {
    module: Handle<SourceTextModule>,
}

impl Module {
    /// The handle for a record the engine made itself, which is how a
    /// synthetic module's evaluation callback is handed its own module.
    pub(crate) fn from_handle(module: Handle<SourceTextModule>) -> Self {
        Self { module }
    }

    /// Parse `source` as a module in `context`'s realm
    /// (v8::ScriptCompiler::CompileModule).
    pub fn compile(context: &Context, specifier: &str, source: &str) -> Result<Self, JsError> {
        Self::compile_with_name(context, specifier, None, source)
    }

    /// The same, with the name a stack frame reports for this module's code
    /// — the host's own resource name, which a module's specifier is not
    /// always (a host may compile a module for a URL and resolve it under
    /// whatever name it chose).
    ///
    /// `None` in [`compile`](Self::compile), which leaves the frames the module
    /// runs unnamed.
    pub fn compile_with_name(
        context: &Context,
        specifier: &str,
        name: Option<&str>,
        source: &str,
    ) -> Result<Self, JsError> {
        let specifier = JsString::from_utf8(specifier);
        let name = name.map(JsString::from_utf8);
        let source = JsString::from_utf8(source);
        context.with_agent(|agent| {
            module::parse_module(agent, &specifier, name.as_ref(), &source, &[])
                .map(|module| Self { module })
        })
    }

    /// Whether this module is a source text module
    /// (v8::Module::IsSourceTextModule).
    pub fn is_source_text_module(&self) -> bool {
        self.module.synthetic.is_none()
    }

    /// Whether this module is a synthetic one
    /// (v8::Module::IsSyntheticModule): a record a host declared the exports of
    /// and gave an evaluation callback, rather than one parsed from source.
    pub fn is_synthetic_module(&self) -> bool {
        self.module.synthetic.is_some()
    }

    /// Create a synthetic module (spec 16.2.1.5.2 /
    /// v8::Module::CreateSyntheticModule): `export_names` are the bindings it
    /// exposes, and `context` is the realm the evaluation callback is handed.
    ///
    /// The names must be strings and must not repeat, which V8 requires too
    /// ("export_names must not contain duplicates"); a name that is not a
    /// string is a `TypeError` here rather than an unchecked handle.
    #[allow(clippy::new_ret_no_self)] // v8::Module::CreateSyntheticModule returns a module.
    pub fn create_synthetic_module(
        context: &Context,
        name: &Local,
        export_names: &[Local],
        evaluation_steps: SyntheticModuleEvaluationSteps,
    ) -> Result<Self, JsError> {
        let name = string_value(name, "the synthetic module's name")?;
        let mut names = Vec::with_capacity(export_names.len());
        for export in export_names {
            names.push(string_value(export, "an export name")?);
        }
        context.with_agent(|agent| {
            module::synthetic_module_create(agent, *context, &name, names, evaluation_steps)
                .map(|module| Self { module })
        })
    }

    /// Set one of a synthetic module's exports
    /// (v8::Module::SetSyntheticModuleExport).
    ///
    /// The module must have been created by
    /// [`create_synthetic_module`](Self::create_synthetic_module) *and*
    /// instantiated, and the name must be one it declared; anything else is a
    /// `ReferenceError`, which is V8's own check and message.
    pub fn set_synthetic_module_export(
        &self,
        export_name: &Local,
        export_value: &Local,
    ) -> Result<bool, JsError> {
        let name = string_value(export_name, "an export name")?;
        module::set_synthetic_module_export(&self.module, &name, export_value.into_value())
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

    /// The module's source text, when it is a source text module
    /// (`Function.prototype.toString` for the module's functions, and the code
    /// cache a host asks the module's unbound script for).
    ///
    /// `None` for a synthetic module: its body is a host callback, and the
    /// record's source field is the empty program it was built from rather than
    /// anything the host wrote.
    pub fn source_text(&self) -> Option<String> {
        self.is_source_text_module()
            .then(|| self.module.source.to_string_lossy())
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

    /// The modules this one's graph reaches that are stalled on a top-level
    /// await (v8::Module::GetStalledTopLevelAwaitMessages): they are being
    /// evaluated asynchronously with nothing left to wait for, so the await
    /// they are suspended on will not settle. A host reports these before
    /// exiting, to explain why a program is still pending.
    pub fn stalled_top_level_await_modules(&self) -> Vec<Self> {
        module::stalled_top_level_await_modules(&self.module)
            .into_iter()
            .map(Self::from_handle)
            .collect()
    }

    /// Where `offset` into this module's source is
    /// (v8::Module::SourceOffsetToLocation), as a 1-based line and column
    /// (the crate we stand in for's `Location` is 0-based; its callers add
    /// one, and `crates/v8` subtracts it again).
    ///
    /// # Panics
    ///
    /// On a module that is not a source text module, which is the crate's own
    /// check (`Utils::ApiCheck`, `src/api/api.cc:2340`). A synthetic module has
    /// no source, and its offsets are not source offsets.
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

    /// Start evaluating the module's asynchronous transitive dependencies and
    /// answer a promise that settles when they have (v8::Module::
    /// EvaluateForImportDefer). The module itself is not evaluated: its
    /// deferred namespace evaluates on first property access.
    pub fn evaluate_for_import_defer(&self, context: &Context) -> Result<Local, JsError> {
        context
            .with_agent(|agent| module::evaluate_for_import_defer(agent, &self.module).map(Local))
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
    use crate::api::{Context, Isolate, Object, Promise};

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

    /// A synthetic module's exports are linkable: a source module that imports
    /// one binds that module's own export binding, so the value its steps set
    /// is what the importer reads — and a name the host did not declare is the
    /// link error V8 reports.
    #[test]
    fn a_name_a_synthetic_module_declares_can_be_imported() {
        let mut isolate = Isolate::new();
        let context = Context::new(&mut isolate).expect("context");

        let names = [Local::string("a"), Local::string("b")];
        let host = Module::create_synthetic_module(&context, &Local::string("host"), &names, steps)
            .expect("create");
        host.register(&context, "ext:host").expect("register");

        let main = Module::compile(
            &context,
            "main",
            "import { a, b } from 'ext:host';\nexport const sum = a + b;",
        )
        .expect("main");
        main.register(&context, "main").expect("register");
        main.instantiate(&context).expect("instantiate");
        main.evaluate(&context).expect("evaluate");

        let namespace = main.namespace(&context).expect("namespace");
        assert_eq!(
            Object::get(&context, &namespace, "sum")
                .expect("read")
                .as_number(),
            Some(3.0)
        );

        let undeclared =
            Module::compile(&context, "bad", "import { nope } from 'ext:host';").expect("compile");
        undeclared.register(&context, "bad").expect("register");
        let error = undeclared
            .instantiate(&context)
            .expect_err("a name the host did not declare");
        assert_eq!(error.kind, ErrorKind::SyntaxError);
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

    /// The steps a synthetic module evaluates through: set the two exports, and
    /// answer the resolved promise V8's steps must answer.
    fn steps(context: Context, module: Module) -> Result<Local, JsError> {
        for (name, value) in [("a", 1.0), ("b", 2.0)] {
            let name = Local::string(name);
            assert!(
                module
                    .set_synthetic_module_export(&name, &Local::number(value))
                    .expect("set an export")
            );
        }
        Promise::resolve(&context, &Local::undefined())
    }

    /// A synthetic module is one a host declared the exports of: its namespace
    /// answers what the steps set, a name it did not declare or a module that
    /// is not yet instantiated is refused, and a second evaluation answers the
    /// same promise rather than running the steps again.
    #[test]
    fn a_synthetic_module_answers_its_hosts_exports() {
        let mut isolate = Isolate::new();
        let context = Context::new(&mut isolate).expect("context");

        let names = [Local::string("a"), Local::string("b")];
        let module =
            Module::create_synthetic_module(&context, &Local::string("host"), &names, steps)
                .expect("create");
        assert!(module.is_synthetic_module());
        assert!(!module.is_source_text_module());

        // The bindings a name is set into are created at instantiation, so
        // setting one before that is V8's ``Export '%' is not defined``.
        let error = module
            .set_synthetic_module_export(&names[0], &Local::number(1.0))
            .expect_err("a module that is not instantiated refuses");
        assert_eq!(error.kind, ErrorKind::ReferenceError);

        assert_eq!(module.status(), ModuleStatus::Uninstantiated);
        module.instantiate(&context).expect("instantiate");
        assert_eq!(module.status(), ModuleStatus::Instantiated);

        let promise = module.evaluate(&context).expect("evaluate");
        assert_eq!(module.status(), ModuleStatus::Evaluated);
        assert_eq!(
            Promise::state(&context, &promise).expect("state"),
            "fulfilled"
        );

        let namespace = module.namespace(&context).expect("namespace");
        for (name, expected) in [("a", 1.0), ("b", 2.0)] {
            assert_eq!(
                Object::get(&context, &namespace, name)
                    .expect("read")
                    .as_number(),
                Some(expected)
            );
        }
        // A name the host never declared is not on the namespace at all.
        assert!(
            Object::get(&context, &namespace, "c")
                .expect("read")
                .is_undefined()
        );
        let refused = module
            .set_synthetic_module_export(&Local::string("c"), &Local::number(3.0))
            .expect_err("an undeclared name is refused");
        assert_eq!(refused.kind, ErrorKind::ReferenceError);

        // An export set again reads through, which is V8's mutable binding.
        module
            .set_synthetic_module_export(&names[0], &Local::number(7.0))
            .expect("set again");
        assert_eq!(
            Object::get(&context, &namespace, "a")
                .expect("read")
                .as_number(),
            Some(7.0)
        );

        // A second evaluation answers the same promise: the steps ran once.
        let again = module.evaluate(&context).expect("evaluate again");
        assert_eq!(again, promise);
    }

    /// A synthetic module's steps that fail leave the module errored, and both
    /// its exception and every later evaluation carry the failure.
    #[test]
    fn a_failing_synthetic_module_records_its_error() {
        fn failing(_context: Context, _module: Module) -> Result<Local, JsError> {
            Err(JsError::new(
                ErrorKind::TypeError,
                "the steps refused".into(),
            ))
        }

        let mut isolate = Isolate::new();
        let context = Context::new(&mut isolate).expect("context");
        let module = Module::create_synthetic_module(
            &context,
            &Local::string("failing"),
            &[Local::string("a")],
            failing,
        )
        .expect("create");
        module.instantiate(&context).expect("instantiate");

        let promise = module.evaluate(&context).expect("evaluate");
        assert_eq!(module.status(), ModuleStatus::Errored);
        assert_eq!(
            Promise::state(&context, &promise).expect("state"),
            "rejected"
        );
        assert!(module.exception().is_some());
        assert_eq!(
            Object::get(
                &context,
                &module.namespace(&context).expect("namespace"),
                "a"
            )
            .expect("read")
            .as_number(),
            None
        );
    }

    /// The deferred form of a dynamic import: a module with no asynchronous
    /// transitive dependency is answered an already-fulfilled promise carrying
    /// the deferred namespace, and one that has such a dependency is answered a
    /// pending promise having started that dependency's evaluation. The module
    /// itself is not evaluated either way.
    #[test]
    fn a_deferred_import_settles_on_its_async_dependencies() {
        let mut isolate = Isolate::new();
        let context = Context::new(&mut isolate).expect("context");

        // Nothing to gather, so the promise is resolved before it is handed
        // back — which is how a host tells this case from the waiting one.
        let plain = Module::compile(&context, "plain", "export const x = 1;").expect("plain");
        plain.register(&context, "plain").expect("register");
        plain.instantiate(&context).expect("instantiate");
        let promise = plain.evaluate_for_import_defer(&context).expect("promise");
        assert_eq!(
            Promise::state(&context, &promise).expect("state"),
            "fulfilled"
        );
        let settled = Promise::result(&context, &promise).expect("result");
        let deferred = plain.deferred_namespace(&context).expect("deferred");
        assert_eq!(
            settled.value().as_object().map(|object| object.id()),
            deferred.value().as_object().map(|object| object.id())
        );
        assert_eq!(plain.status(), ModuleStatus::Instantiated);

        // A dependency that awaits: gathered, evaluated, and still running, so
        // the promise it settles with is pending.
        let dep = Module::compile(
            &context,
            "dep",
            "await new Promise(() => {});\nexport const x = 1;",
        )
        .expect("dep");
        dep.register(&context, "dep").expect("register");
        let main =
            Module::compile(&context, "main", "import defer * as ns from 'dep';").expect("main");
        main.register(&context, "main").expect("register");
        main.instantiate(&context).expect("instantiate");

        let promise = main.evaluate_for_import_defer(&context).expect("promise");
        assert_eq!(
            Promise::state(&context, &promise).expect("state"),
            "pending"
        );
        assert_eq!(dep.status(), ModuleStatus::Evaluating);
        assert_eq!(main.status(), ModuleStatus::Instantiated);
    }

    /// A module suspended on a top-level await that will not settle is what a
    /// host reports on the way out. A module that finished is not reported, and
    /// one whose dependency is stalled reports that dependency — the walk
    /// looks through the graph, not just at the module it was asked about.
    #[test]
    fn a_module_stalled_on_its_own_await_is_reported() {
        let mut isolate = Isolate::new();
        let context = Context::new(&mut isolate).expect("context");

        let done = Module::compile(&context, "done", "export const x = 1;").expect("done");
        done.register(&context, "done").expect("register");
        done.instantiate(&context).expect("instantiate");
        done.evaluate(&context).expect("evaluate");
        assert!(done.stalled_top_level_await_modules().is_empty());

        let waiting =
            Module::compile(&context, "waiting", "await new Promise(() => {});").expect("waiting");
        waiting.register(&context, "waiting").expect("register");
        waiting.instantiate(&context).expect("instantiate");
        let promise = waiting.evaluate(&context).expect("evaluate");
        assert_eq!(
            Promise::state(&context, &promise).expect("state"),
            "pending"
        );
        let stalled = waiting.stalled_top_level_await_modules();
        assert_eq!(stalled.len(), 1);
        assert!(stalled[0] == waiting);

        let graph = Module::compile(&context, "graph", "import 'waiting';").expect("graph");
        graph.register(&context, "graph").expect("register");
        graph.instantiate(&context).expect("instantiate");
        let stalled = graph.stalled_top_level_await_modules();
        assert_eq!(stalled.len(), 1);
        assert!(stalled[0] == waiting);
    }
}
