//! Modules (spec ch. 16): Source Text Module Records, declaration
//! instantiation (linking), evaluation with top-level await, dynamic import,
//! import.meta, and JSON modules.

use std::cell::RefCell;
use std::rc::Rc;

use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::PropertyKey;
use crux::string::JsString;
use crux::value::Value;

use syntax::ast::{
    AssignOp, AttributeKey, ExportDecl, ExportDefault, ExportName, ExportSpecifier, Expr, ExprKind,
    ImportEntry, Module, ModuleItem, Stmt, StmtKind, VarDeclKind,
};

use crate::agent::Agent;
use crate::async_await::AsyncFunctionState;
use crate::context::{ExecutionContext, ScriptOrModule};
use crate::env::{EnvRef, create_import_binding, new_module_environment};
use crate::flow::Completion;
use crate::ir::{Suspension, Vm, VmOutcome};
use crate::realm::Realm;

/// The status of a Source Text Module Record (spec 16.2.1.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleStatus {
    Unlinked,
    Linking,
    Linked,
    Evaluating,
    EvaluatingAsync,
    Evaluated,
}

/// An export entry (spec 16.2.1.18).
#[derive(Debug, Clone)]
pub struct ExportEntry {
    pub export_name: Option<ExportName>,
    pub module_request: Option<JsString>,
    pub import_name: Option<ExportName>,
    pub local_name: Option<crux::AtomId>,
}

/// A Source Text Module Record.
#[derive(Debug)]
pub struct SourceTextModule {
    pub realm: Handle<Realm>,
    pub code: Module,
    /// The exact source text, for `Function.prototype.toString`.
    pub source: JsString,
    pub status: RefCell<ModuleStatus>,
    pub environment: RefCell<Option<EnvRef>>,
    pub namespace: RefCell<Option<Value>>,
    /// (specifier, attributes) of each `import`/`export ... from` clause.
    pub requested_modules: Vec<(JsString, Vec<(AttributeKey, JsString)>)>,
    /// (module specifier, entry) of each import.
    pub import_entries: Vec<(JsString, ImportEntry)>,
    pub local_export_entries: Vec<ExportEntry>,
    pub indirect_export_entries: Vec<ExportEntry>,
    pub star_export_entries: Vec<ExportEntry>,
    pub top_level_capability: RefCell<Option<crate::promise::PromiseCapability>>,
    pub evaluation_error: RefCell<Option<Value>>,
}

/// A host-provided module source (HostResolveImportedModule).
#[derive(Debug, Clone)]
pub struct HostModuleSource {
    pub source: JsString,
    pub json: bool,
}

impl Agent {
    /// Test/CLI hook: register a module the host can resolve by specifier.
    pub fn add_module(&mut self, specifier: &str, source: &str) {
        self.host_modules.borrow_mut().insert(
            JsString::from_utf8(specifier),
            HostModuleSource {
                source: JsString::from_utf8(source),
                json: false,
            },
        );
    }

    /// Test/CLI hook: register a JSON module.
    pub fn add_json_module(&mut self, specifier: &str, json: &str) {
        self.host_modules.borrow_mut().insert(
            JsString::from_utf8(specifier),
            HostModuleSource {
                source: JsString::from_utf8(json),
                json: true,
            },
        );
    }
}

/// The import/export records of a module, collected from its AST
/// (spec 16.2.1.7-16.2.1.10). Built before the module handle is shared so the
/// entries can be populated without interior mutability.
struct ModuleRecords {
    requested_modules: Vec<(JsString, Vec<(AttributeKey, JsString)>)>,
    import_entries: Vec<(JsString, ImportEntry)>,
    local_export_entries: Vec<ExportEntry>,
    indirect_export_entries: Vec<ExportEntry>,
    star_export_entries: Vec<ExportEntry>,
}

/// Parse a module source into a Source Text Module Record.
pub fn parse_module(
    agent: &Agent,
    specifier: &JsString,
    source: &JsString,
) -> Result<Handle<SourceTextModule>, JsError> {
    let realm = agent.current_realm()?;
    let code = {
        let host = agent.host_modules.borrow().get(specifier).cloned();
        match host {
            Some(entry) if entry.json => {
                // A JSON module is a module whose default export is the JSON
                // value: `export default <json>`.
                let wrapped = format!("export default {}", entry.source.to_string_lossy());
                parser::parse_module(&wrapped)?
            }
            _ => parser::parse_module(&source.to_string_lossy())?,
        }
    };
    let records = collect_module_records(&code);
    let module = Handle::new(SourceTextModule {
        realm,
        code,
        source: source.clone(),
        status: RefCell::new(ModuleStatus::Unlinked),
        environment: RefCell::new(None),
        namespace: RefCell::new(None),
        requested_modules: records.requested_modules,
        import_entries: records.import_entries,
        local_export_entries: records.local_export_entries,
        indirect_export_entries: records.indirect_export_entries,
        star_export_entries: records.star_export_entries,
        top_level_capability: RefCell::new(None),
        evaluation_error: RefCell::new(None),
    });
    Ok(module)
}

/// HostResolveImportedModule (spec 16.6.1.1.2): the agent's registered module
/// map, cached in the realm's [[LoadedModules]].
pub fn host_resolve_imported_module(
    agent: &mut Agent,
    specifier: &JsString,
) -> Result<Handle<SourceTextModule>, JsError> {
    let realm = agent.current_realm()?;
    if let Some(module) = realm.loaded_modules.borrow().get(specifier) {
        return Ok(module.clone());
    }
    let source = agent
        .host_modules
        .borrow()
        .get(specifier)
        .cloned()
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                format!("Cannot find module {}", specifier.to_string_lossy()),
            )
        })?;
    let module = parse_module(agent, specifier, &source.source)?;
    realm
        .loaded_modules
        .borrow_mut()
        .insert(specifier.clone(), module.clone());
    Ok(module)
}

/// Collect the import and export entries of a module's AST.
fn collect_module_records(code: &Module) -> ModuleRecords {
    let mut requested_modules = Vec::new();
    let mut import_entries = Vec::new();
    let mut local_export_entries = Vec::new();
    let mut indirect_export_entries = Vec::new();
    let mut star_export_entries = Vec::new();
    for item in &code.body {
        match item {
            ModuleItem::Import(import) => {
                for entry in &import.entries {
                    import_entries.push((import.specifier.clone(), entry.clone()));
                }
                requested_modules.push((import.specifier.clone(), import.attributes.clone()));
            }
            ModuleItem::Export(export) => match export {
                ExportDecl::Named { specifiers, .. } => {
                    for spec in specifiers {
                        let (local, exported) = match spec {
                            ExportSpecifier::Same(name) => (name.clone(), name.clone()),
                            ExportSpecifier::Alias { local, exported } => {
                                (local.clone(), exported.clone())
                            }
                        };
                        let ExportName::Ident(local_id) = local else {
                            continue;
                        };
                        local_export_entries.push(ExportEntry {
                            export_name: Some(exported),
                            module_request: None,
                            import_name: None,
                            local_name: Some(local_id),
                        });
                    }
                }
                ExportDecl::From {
                    specifiers,
                    namespace,
                    specifier,
                    attributes,
                    ..
                } => {
                    if let Some(namespace) = namespace {
                        // `export * as ns from ...`: a local namespace binding
                        // whose value is the imported module's namespace.
                        let local = match namespace {
                            ExportName::Ident(id) => *id,
                            ExportName::Str(_) => continue,
                        };
                        local_export_entries.push(ExportEntry {
                            export_name: Some(ExportName::Ident(local)),
                            module_request: Some(specifier.clone()),
                            import_name: None,
                            local_name: None,
                        });
                    } else if specifiers.is_empty() {
                        // `export * from ...`
                        star_export_entries.push(ExportEntry {
                            export_name: None,
                            module_request: Some(specifier.clone()),
                            import_name: None,
                            local_name: None,
                        });
                    } else {
                        for spec in specifiers {
                            let (local, exported) = match spec {
                                ExportSpecifier::Same(name) => (name.clone(), name.clone()),
                                ExportSpecifier::Alias { local, exported } => {
                                    (local.clone(), exported.clone())
                                }
                            };
                            indirect_export_entries.push(ExportEntry {
                                export_name: Some(exported),
                                module_request: Some(specifier.clone()),
                                import_name: Some(local),
                                local_name: None,
                            });
                        }
                    }
                    requested_modules.push((specifier.clone(), attributes.clone()));
                }
                ExportDecl::Declaration(stmt) => {
                    for name in declared_names(&stmt.kind) {
                        local_export_entries.push(ExportEntry {
                            export_name: Some(ExportName::Ident(name)),
                            module_request: None,
                            import_name: None,
                            local_name: Some(name),
                        });
                    }
                }
                ExportDecl::Default(default) => match &**default {
                    ExportDefault::Function(f) => {
                        // `export default function [name]() {}`: the export
                        // name `default` resolves to the function's local
                        // name (or a synthesized one).
                        let local = f.name.unwrap_or_else(|| {
                            crux::intern(
                                "*default*".encode_utf16().collect::<Vec<u16>>().as_slice(),
                            )
                        });
                        local_export_entries.push(ExportEntry {
                            export_name: Some(ExportName::Ident(crux::intern(
                                "default".encode_utf16().collect::<Vec<u16>>().as_slice(),
                            ))),
                            module_request: None,
                            import_name: None,
                            local_name: Some(local),
                        });
                    }
                    ExportDefault::Class(c) => {
                        let local = c.name.unwrap_or_else(|| {
                            crux::intern(
                                "*default*".encode_utf16().collect::<Vec<u16>>().as_slice(),
                            )
                        });
                        local_export_entries.push(ExportEntry {
                            export_name: Some(ExportName::Ident(crux::intern(
                                "default".encode_utf16().collect::<Vec<u16>>().as_slice(),
                            ))),
                            module_request: None,
                            import_name: None,
                            local_name: Some(local),
                        });
                    }
                    ExportDefault::Expr(_) => {
                        local_export_entries.push(ExportEntry {
                            export_name: Some(ExportName::Ident(crux::intern(
                                "default".encode_utf16().collect::<Vec<u16>>().as_slice(),
                            ))),
                            module_request: None,
                            import_name: None,
                            local_name: Some(crux::intern(
                                "*default*".encode_utf16().collect::<Vec<u16>>().as_slice(),
                            )),
                        });
                    }
                },
            },
            ModuleItem::Stmt(_) => {}
        }
    }
    ModuleRecords {
        requested_modules,
        import_entries,
        local_export_entries,
        indirect_export_entries,
        star_export_entries,
    }
}

/// The declared names of an `export` declaration statement.
fn declared_names(kind: &StmtKind) -> Vec<crux::AtomId> {
    let mut names = Vec::new();
    match kind {
        StmtKind::VarDecl { decls, .. } => {
            for decl in decls {
                let mut out = Vec::new();
                crate::script::bound_names(&decl.pattern, &mut out);
                names.extend(out.into_iter().map(|name| crux::intern(name.as_slice())));
            }
        }
        StmtKind::FunctionDecl(function) => {
            if let Some(name) = function.name {
                names.push(name);
            }
        }
        StmtKind::ClassDecl(class) => {
            if let Some(name) = class.name {
                names.push(name);
            }
        }
        _ => {}
    }
    names
}

/// ModuleDeclarationInstantiation (spec 16.2.2.4): link the module's imports
/// and exports against the module environment, recursively.
pub fn module_declaration_instantiation(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
) -> Result<(), JsError> {
    match *module.status.borrow() {
        // A module already on the link stack (cycle) or past linking returns
        // immediately (spec 16.2.1.6.1.2.1 step 6).
        ModuleStatus::Unlinked => {}
        _ => return Ok(()),
    }
    module.status.replace(ModuleStatus::Linking);
    let env = new_module_environment(None);
    module.environment.replace(Some(env.clone()));

    // Import bindings (live, through the imported module's environment).
    let imports = module.import_entries.clone();
    for (specifier, entry) in imports {
        let imported = host_resolve_imported_module(agent, &specifier)?;
        module_declaration_instantiation(agent, &imported)?;
        let (local, import_name) = match entry {
            ImportEntry::Namespace { local, .. } => {
                // A namespace import binds to the imported module's namespace.
                let namespace = module_namespace(agent, &imported)?;
                let name = crux::lookup(local);
                env.create_immutable_binding(&name, true)?;
                env.initialize_binding(&name, namespace)?;
                continue;
            }
            ImportEntry::Default { local, .. } => {
                (crux::lookup(local), JsString::from_utf8("default"))
            }
            ImportEntry::Named {
                imported, local, ..
            } => {
                let import_name = match imported {
                    ExportName::Ident(id) => crux::lookup(id),
                    ExportName::Str(text) => text,
                };
                (crux::lookup(local), import_name)
            }
        };
        // Resolve the imported name through the target module's exports
        // (spec 16.2.1.7.3.1 step 5): the defining binding may live under a
        // different local name (e.g. `default` is bound as `*default*`) or
        // in a deeper module.
        let mut resolve_set = Vec::new();
        match resolve_export(agent, &imported, &import_name, &mut resolve_set)? {
            Some(ResolvedBinding::Local(target, binding)) => {
                let target_env = target.environment.borrow().clone().ok_or_else(|| {
                    JsError::new(
                        ErrorKind::TypeError,
                        "imported module has no environment".into(),
                    )
                })?;
                create_import_binding(&env, &local, target_env, &binding)?;
            }
            Some(ResolvedBinding::Namespace(target)) => {
                let namespace = module_namespace(agent, &target)?;
                env.create_immutable_binding(&local, true)?;
                env.initialize_binding(&local, namespace)?;
            }
            None => {
                return Err(JsError::new(
                    ErrorKind::SyntaxError,
                    format!(
                        "Module {} does not export {}",
                        specifier.to_string_lossy(),
                        import_name.to_string_lossy()
                    ),
                ));
            }
        }
    }

    // Local export bindings.
    let local_exports = module.local_export_entries.clone();
    for export in &local_exports {
        if let Some(module_request) = &export.module_request {
            // `export * as ns from ...`: bind the imported module's namespace
            // (spec 16.2.2.4 step 28) at instantiation time.
            let imported = host_resolve_imported_module(agent, module_request)?;
            module_declaration_instantiation(agent, &imported)?;
            let namespace = module_namespace(agent, &imported)?;
            let name = export_name_string(export.export_name.as_ref())?;
            if !env.has_binding(&name)? {
                env.create_mutable_binding(&name, false)?;
            }
            env.initialize_binding(&name, namespace)?;
            continue;
        }
        if let Some(local) = export.local_name {
            let name = crux::lookup(local);
            if !env.has_binding(&name)? {
                env.create_mutable_binding(&name, false)?;
            }
        }
    }

    // Instantiate the module's top-level declarations.
    instantiate_module_declarations(agent, module, &env)?;

    // Indirect export bindings (re-exports). InitializeEnvironment only
    // validates that each re-export resolves; the binding itself is read
    // through ResolveExport at namespace access time (spec 16.2.1.7.3.1
    // step 1).
    let indirect_exports = module.indirect_export_entries.clone();
    for export in &indirect_exports {
        let specifier = export.module_request.as_ref().ok_or_else(|| {
            JsError::new(ErrorKind::TypeError, "indirect export has no module".into())
        })?;
        let imported = host_resolve_imported_module(agent, specifier)?;
        module_declaration_instantiation(agent, &imported)?;
        let import_name = export_name_string(export.import_name.as_ref())?;
        let mut resolve_set = Vec::new();
        if resolve_export(agent, &imported, &import_name, &mut resolve_set)?.is_none() {
            return Err(JsError::new(
                ErrorKind::SyntaxError,
                format!(
                    "Module {} does not export {}",
                    specifier.to_string_lossy(),
                    import_name.to_string_lossy()
                ),
            ));
        }
    }

    module.status.replace(ModuleStatus::Linked);
    Ok(())
}

/// The module's top-level var/function/lexical declarations bind into the
/// module environment (spec 16.2.2.4 steps 22-27).
fn instantiate_module_declarations(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
    env: &EnvRef,
) -> Result<(), JsError> {
    let stmts = module_statements(module);
    // Var declarations first (hoisted), then functions, then lexical.
    for stmt in &stmts {
        let StmtKind::VarDecl {
            kind: VarDeclKind::Var,
            decls,
            ..
        } = &stmt.kind
        else {
            continue;
        };
        for decl in decls {
            let mut names = Vec::new();
            crate::script::bound_names(&decl.pattern, &mut names);
            for name in names {
                if !env.has_binding(&name)? {
                    env.create_mutable_binding(&name, false)?;
                }
                // spec 16.2.1.7.3.1 step 6: var bindings start
                // initialized to *undefined* (a pre-existing binding is a
                // declaration the pre-pass created, or a function/import
                // name the parser forbids from colliding with a var).
                env.initialize_binding(&name, Value::Undefined)?;
            }
        }
    }
    for stmt in &stmts {
        let StmtKind::FunctionDecl(function) = &stmt.kind else {
            continue;
        };
        if let Some(name) = function.name {
            let name = crux::lookup(name);
            if !env.has_binding(&name)? {
                env.create_mutable_binding(&name, false)?;
            }
            // The function's own source span, not the whole module text
            // (Function.prototype.toString, spec 20.2.3.5).
            let module_text = module.source.as_slice();
            let (start, end) = (function.span.start as usize, function.span.end as usize);
            let source = (start < end && end <= module_text.len())
                .then(|| JsString::from_utf16(&module_text[start..end]));
            let func = crate::function::instantiate_function_with_source(
                agent,
                function,
                env.clone(),
                true,
                source,
            )?;
            env.initialize_binding(&name, func)?;
        }
    }
    for stmt in &stmts {
        match &stmt.kind {
            StmtKind::VarDecl { kind, decls, .. } if *kind != syntax::ast::VarDeclKind::Var => {
                for decl in decls {
                    let mut names = Vec::new();
                    crate::script::bound_names(&decl.pattern, &mut names);
                    for name in names {
                        if *kind == syntax::ast::VarDeclKind::Const {
                            env.create_immutable_binding(&name, true)?;
                        } else {
                            env.create_mutable_binding(&name, false)?;
                        }
                    }
                }
            }
            StmtKind::ClassDecl(class) => {
                if let Some(name) = class.name {
                    let name = crux::lookup(name);
                    if !env.has_binding(&name)? {
                        env.create_mutable_binding(&name, false)?;
                    }
                }
            }
            _ => {}
        }
    }
    // The synthesized `*default*` binding for `export default expr`. Only an
    // expression default needs the undefined initial value: a default
    // function/class declaration was already instantiated and bound above.
    let default_declared = stmts.iter().any(|stmt| match &stmt.kind {
        StmtKind::FunctionDecl(f) => f
            .name
            .is_some_and(|n| crux::lookup(n) == JsString::from_utf8("*default*")),
        StmtKind::ClassDecl(c) => c
            .name
            .is_some_and(|n| crux::lookup(n) == JsString::from_utf8("*default*")),
        _ => false,
    });
    for export in &module.local_export_entries {
        if let Some(local) = export.local_name
            && crux::lookup(local) == JsString::from_utf8("*default*")
        {
            let name = JsString::from_utf8("*default*");
            if !env.has_binding(&name)? {
                env.create_mutable_binding(&name, false)?;
            }
            if !default_declared {
                env.initialize_binding(&name, Value::Undefined)?;
            }
        }
    }
    Ok(())
}

/// The module's executable statements (declarations of `export ...` included).
fn module_statements(module: &Handle<SourceTextModule>) -> Vec<Stmt> {
    let mut stmts = Vec::new();
    for item in &module.code.body {
        match item {
            ModuleItem::Stmt(stmt) => stmts.push(stmt.clone()),
            ModuleItem::Export(ExportDecl::Declaration(stmt)) => stmts.push(stmt.clone()),
            ModuleItem::Export(ExportDecl::Default(default)) => match &**default {
                ExportDefault::Function(function) => {
                    let mut function = function.clone();
                    if function.name.is_none() {
                        function.name = Some(crux::intern(
                            "*default*".encode_utf16().collect::<Vec<u16>>().as_slice(),
                        ));
                    }
                    stmts.push(Stmt {
                        span: function.span,
                        kind: StmtKind::FunctionDecl(function),
                    });
                }
                ExportDefault::Class(class) => {
                    let mut class = class.clone();
                    if class.name.is_none() {
                        class.name = Some(crux::intern(
                            "*default*".encode_utf16().collect::<Vec<u16>>().as_slice(),
                        ));
                    }
                    stmts.push(Stmt {
                        span: class.span,
                        kind: StmtKind::ClassDecl(class),
                    });
                }
                ExportDefault::Expr(expr) => {
                    // `export default <expr>`: evaluate and bind `*default*`.
                    let atom =
                        crux::intern("*default*".encode_utf16().collect::<Vec<u16>>().as_slice());
                    let target = Expr {
                        span: expr.span,
                        kind: ExprKind::Ident(atom),
                    };
                    let assign = Expr {
                        span: expr.span,
                        kind: ExprKind::Assign {
                            op: AssignOp::Assign,
                            target: Box::new(target),
                            value: Box::new(expr.clone()),
                        },
                    };
                    stmts.push(Stmt {
                        span: expr.span,
                        kind: StmtKind::Expr(assign),
                    });
                }
            },
            _ => {}
        }
    }
    stmts
}

/// ModuleEvaluation (spec 16.2.2.5): create the namespace and execute the
/// body through the resumable VM, returning the (possibly pending) promise
/// that settles when the module finishes. Every module evaluates to a
/// promise — synchronous bodies settle it before returning.
pub fn module_evaluation(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
) -> Result<Value, JsError> {
    let status = *module.status.borrow();
    match status {
        ModuleStatus::Evaluated => {
            return Ok(module
                .top_level_capability
                .borrow()
                .as_ref()
                .map(|c| c.promise.clone())
                .unwrap_or(Value::Undefined));
        }
        ModuleStatus::Evaluating => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Cyclic module evaluation".into(),
            ));
        }
        ModuleStatus::EvaluatingAsync => {
            return Ok(module
                .top_level_capability
                .borrow()
                .as_ref()
                .map(|c| c.promise.clone())
                .unwrap_or(Value::Undefined));
        }
        _ => {}
    }
    // A module reached through a star export may never have been linked
    // (`export *` does not create an import entry); link it on demand.
    if *module.status.borrow() == ModuleStatus::Unlinked {
        module_declaration_instantiation(agent, module)?;
    }
    module.status.replace(ModuleStatus::Evaluating);
    module_namespace(agent, module)?;
    // Evaluate the module's dependencies depth-first (spec 16.2.1.6.1.3.1
    // steps 11-12). A dependency that is already evaluating is a cycle: its
    // body finishes on the current evaluation stack, so it is skipped here.
    let dependencies = module.requested_modules.clone();
    let dependencies_result = (|| -> Result<(), JsError> {
        for (specifier, _) in dependencies {
            let imported = host_resolve_imported_module(agent, &specifier)?;
            let status = *imported.status.borrow();
            match status {
                ModuleStatus::Evaluated
                | ModuleStatus::Evaluating
                | ModuleStatus::EvaluatingAsync => {}
                _ => {
                    module_evaluation(agent, &imported)?;
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = dependencies_result {
        module.status.replace(ModuleStatus::Evaluated);
        return Err(error);
    }
    let env = module
        .environment
        .borrow()
        .clone()
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "module is not linked".into()))?;
    let stmts = module_statements(module);
    let context = ExecutionContext {
        function: None,
        realm: module.realm.clone(),
        script_or_module: Some(ScriptOrModule::Module(module.clone())),
        lexical_environment: env.clone(),
        variable_environment: env.clone(),
        private_environment: None,
        source: Some(module.source.clone()),
        annex_b_hoistable: Default::default(),
    };
    agent.execution_context_stack.push(context.clone());
    let strict = true;
    let body = crate::ir::compile_statements(&stmts, strict)?;
    let promise_ctor = module
        .realm
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let capability = crate::promise::new_promise_capability(agent, &promise_ctor)?;
    module
        .top_level_capability
        .replace(Some(capability.clone()));
    // The body may suspend on top-level await, so it always runs in the
    // resumable driver; synchronous modules complete on the first pass.
    module.status.replace(ModuleStatus::EvaluatingAsync);
    let state = Rc::new(RefCell::new(AsyncFunctionState {
        vm: Vm::new(env, strict),
        body,
        context,
        promise: capability.promise.clone(),
        resolve: capability.resolve.clone(),
        reject: capability.reject.clone(),
    }));
    let mut state_ref = state.borrow_mut();
    let body = state_ref.body.clone();
    let outcome = state_ref.vm.start(agent, &body);
    drop(state_ref);
    match outcome {
        Ok(VmOutcome::Completed(completion)) => {
            agent.execution_context_stack.pop();
            finish_module_evaluation(agent, module, &state, completion)?;
        }
        Ok(VmOutcome::Suspended(Suspension::Await(value))) => {
            agent.execution_context_stack.pop();
            crate::async_await::attach_await(agent, &state, value)?;
        }
        Ok(VmOutcome::Suspended(_)) => {
            agent.execution_context_stack.pop();
            module.status.replace(ModuleStatus::Evaluated);
            return Err(JsError::new(
                ErrorKind::TypeError,
                "module body suspended on a non-await point".into(),
            ));
        }
        Err(error) => {
            agent.execution_context_stack.pop();
            module.status.replace(ModuleStatus::Evaluated);
            return Err(error);
        }
    }
    Ok(capability.promise)
}

/// The tail of an async module evaluation: settle the top-level capability.
fn finish_module_evaluation(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
    state: &Rc<RefCell<AsyncFunctionState>>,
    completion: Completion,
) -> Result<(), JsError> {
    let (resolve, reject) = {
        let state = state.borrow();
        (state.resolve.clone(), state.reject.clone())
    };
    module.status.replace(ModuleStatus::Evaluated);
    match completion {
        Completion::Return(value) | Completion::Normal(value) => {
            crate::function::call(agent, &resolve, Value::Undefined, &[value])?;
        }
        Completion::Empty => {
            crate::function::call(agent, &resolve, Value::Undefined, &[Value::Undefined])?;
        }
        Completion::Throw(value) => {
            module.evaluation_error.replace(Some(value.clone()));
            crate::function::call(agent, &reject, Value::Undefined, &[value])?;
        }
        Completion::Break { .. } | Completion::Continue { .. } => {
            crate::function::call(
                agent,
                &reject,
                Value::Undefined,
                &[Value::String(Handle::new(JsString::from_utf8(
                    "Illegal control flow in a module body",
                )))],
            )?;
        }
    }
    Ok(())
}

/// GetModuleNamespace / the module namespace exotic object (spec 16.2.2.6).
pub fn module_namespace(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
) -> Result<Value, JsError> {
    if let Some(namespace) = module.namespace.borrow().clone() {
        return Ok(namespace);
    }
    // The export names: local + resolved indirect + star-resolved.
    let mut exports: Vec<PropertyKey> = Vec::new();
    for export in &module.local_export_entries {
        if let Ok(name) = export_name_string(export.export_name.as_ref()) {
            exports.push(PropertyKey::from_js_string(&name));
        }
    }
    for export in &module.indirect_export_entries {
        if let Ok(name) = export_name_string(export.export_name.as_ref()) {
            exports.push(PropertyKey::from_js_string(&name));
        }
    }
    for export in &module.star_export_entries {
        let specifier = export.module_request.as_ref().ok_or_else(|| {
            JsError::new(ErrorKind::TypeError, "star export has no module".into())
        })?;
        let imported = host_resolve_imported_module(agent, specifier)?;
        let imported_namespace = module_namespace(agent, &imported)?;
        if let Value::Object(obj) = &imported_namespace {
            for key in obj.own_property_keys()? {
                let PropertyKey::String(_) = key else {
                    continue;
                };
                if !exports.contains(&key) {
                    exports.push(key);
                }
            }
        }
    }
    let namespace = JsObject::module_namespace_object_create(exports)?;
    let namespace_value = Value::Object(namespace.clone());
    agent
        .module_namespaces
        .insert(namespace.id(), module.clone());
    module.namespace.replace(Some(namespace_value.clone()));
    Ok(namespace_value)
}

/// A resolved export binding (spec 16.2.1.7.2.2 ResolveExport).
enum ResolvedBinding {
    /// The binding lives in `module`'s environment under `local_name`.
    Local(Handle<SourceTextModule>, JsString),
    /// The binding is the module namespace object of `module`.
    Namespace(Handle<SourceTextModule>),
}

/// ResolveExport (spec 16.2.1.7.2.2): find the defining module and local
/// binding of an export name, following local, indirect, and star exports.
/// `None` means the name is not exported or is ambiguous (both read as
/// *undefined* through the namespace).
fn resolve_export(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
    name: &JsString,
    resolve_set: &mut Vec<(Handle<SourceTextModule>, JsString)>,
) -> Result<Option<ResolvedBinding>, JsError> {
    if resolve_set
        .iter()
        .any(|(m, n)| Rc::ptr_eq(m, module) && n == name)
    {
        return Ok(None);
    }
    resolve_set.push((module.clone(), name.clone()));
    for export in &module.local_export_entries {
        if export_name_string(export.export_name.as_ref())
            .ok()
            .as_ref()
            == Some(name)
        {
            if let Some(local) = export.local_name {
                return Ok(Some(ResolvedBinding::Local(
                    module.clone(),
                    crux::lookup(local),
                )));
            }
            // `export * as ns from ...`: the binding is a namespace.
            let specifier = export.module_request.as_ref().ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "local export has no module".into())
            })?;
            let imported = host_resolve_imported_module(agent, specifier)?;
            return Ok(Some(ResolvedBinding::Namespace(imported)));
        }
    }
    for export in &module.indirect_export_entries {
        if export_name_string(export.export_name.as_ref())
            .ok()
            .as_ref()
            == Some(name)
        {
            let specifier = export.module_request.as_ref().ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "indirect export has no module".into())
            })?;
            let imported = host_resolve_imported_module(agent, specifier)?;
            let import_name = export_name_string(export.import_name.as_ref())?;
            return resolve_export(agent, &imported, &import_name, resolve_set);
        }
    }
    // A `default` export cannot come from `export *`.
    if name == &JsString::from_utf8("default") {
        return Ok(None);
    }
    let mut star_resolution: Option<ResolvedBinding> = None;
    for export in &module.star_export_entries {
        let specifier = export.module_request.as_ref().ok_or_else(|| {
            JsError::new(ErrorKind::TypeError, "star export has no module".into())
        })?;
        let imported = host_resolve_imported_module(agent, specifier)?;
        let Some(resolution) = resolve_export(agent, &imported, name, resolve_set)? else {
            continue;
        };
        match &star_resolution {
            None => star_resolution = Some(resolution),
            Some(previous) => {
                // Ambiguous unless both resolve to the same binding.
                let same = match (previous, &resolution) {
                    (ResolvedBinding::Local(pm, pn), ResolvedBinding::Local(m, n)) => {
                        Rc::ptr_eq(pm, m) && pn == n
                    }
                    (ResolvedBinding::Namespace(pm), ResolvedBinding::Namespace(m)) => {
                        Rc::ptr_eq(pm, m)
                    }
                    _ => false,
                };
                if !same {
                    return Ok(None);
                }
            }
        }
    }
    Ok(star_resolution)
}

/// Read an exported binding through the module environment (the namespace's
/// [[Get]]). Names that are not exported, or whose export is ambiguous, read
/// as *undefined* (spec 10.4.6.8).
pub fn namespace_get(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
    name: &JsString,
) -> Result<Value, JsError> {
    let mut resolve_set = Vec::new();
    match resolve_export(agent, module, name, &mut resolve_set)? {
        Some(ResolvedBinding::Local(target, local)) => {
            let env =
                target.environment.borrow().clone().ok_or_else(|| {
                    JsError::new(ErrorKind::TypeError, "module is not linked".into())
                })?;
            env.get_binding_value(&local, true)
        }
        Some(ResolvedBinding::Namespace(target)) => module_namespace(agent, &target),
        None => Ok(Value::Undefined),
    }
}

/// `import()` (spec 16.6.1.4): load, link, evaluate, and resolve with the
/// module namespace.
pub fn dynamic_import(
    agent: &mut Agent,
    specifier: &Value,
    options: Option<&Value>,
) -> Result<Value, JsError> {
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let capability = crate::promise::new_promise_capability(agent, &promise_ctor)?;
    let specifier_text = crux::convert::to_string(specifier)?;
    // Import attributes: only `type: "json"` is supported.
    if let Some(options) = options {
        let attributes = import_attributes(options)?;
        for (key, value) in attributes {
            let key = key.to_string_lossy();
            let value = value.to_string_lossy();
            if key != "type" || value != "json" {
                crate::function::call(
                    agent,
                    &capability.reject,
                    Value::Undefined,
                    &[Value::String(Handle::new(JsString::from_utf8(
                        "Unsupported import attribute",
                    )))],
                )?;
                return Ok(capability.promise);
            }
        }
    }
    let resolve = capability.resolve.clone();
    let reject = capability.reject.clone();
    let result = (|| -> Result<(), JsError> {
        let module = host_resolve_imported_module(agent, &specifier_text)?;
        module_declaration_instantiation(agent, &module)?;
        let namespace = module_namespace(agent, &module)?;
        let evaluation = module_evaluation(agent, &module)?;
        if is_promise_value(agent, &evaluation) {
            // Top-level await: resolve when the module finishes.
            let method = crate::context::get_property(
                agent,
                &evaluation,
                &JsString::from_utf8("then"),
                evaluation.clone(),
            )?;
            let then_resolve = make_namespace_resolver(agent, &resolve, &namespace)?;
            crate::function::call(agent, &method, evaluation, &[then_resolve, reject.clone()])?;
        } else {
            crate::function::call(agent, &resolve, Value::Undefined, &[namespace])?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        let rejection = crate::promise::error_value(agent, &error);
        crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
    }
    Ok(capability.promise)
}

fn is_promise_value(agent: &Agent, value: &Value) -> bool {
    let Value::Object(obj) = value else {
        return false;
    };
    agent.promises.contains_key(&obj.id())
}

/// A handler that resolves the import promise with the namespace.
fn make_namespace_resolver(
    agent: &mut Agent,
    resolve: &Value,
    namespace: &Value,
) -> Result<Value, JsError> {
    let closure = crux::function::Function::create_builtin(
        Some(JsString::from_utf8("")),
        1,
        Box::new(|_, _| {
            Err(JsError::new(
                ErrorKind::TypeError,
                "import resolver must be called through the agent".into(),
            ))
        }),
        None,
        None,
    )?;
    agent
        .import_namespace_resolvers
        .insert(closure.id(), (resolve.clone(), namespace.clone()));
    Ok(Value::Function(closure))
}

/// Dispatch the dynamic-import namespace resolvers.
pub fn dispatch_import_resolver(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let Value::Function(function) = callee else {
        return None;
    };
    let (resolve, namespace) = agent
        .import_namespace_resolvers
        .get(&function.id())
        .cloned()?;
    let _ = args;
    crate::function::call(agent, &resolve, Value::Undefined, &[namespace])
        .map(|_| Value::Undefined)
        .into()
}

/// `import.meta` (spec 16.6.2): a fresh host-defined metadata object.
pub fn import_meta(agent: &mut Agent) -> Result<Value, JsError> {
    let proto = agent
        .current_realm()?
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| crate::context::as_object(&value));
    Ok(Value::Object(JsObject::ordinary_object_create(proto)))
}

/// The `with { ... }` attributes of an import.
fn import_attributes(options: &Value) -> Result<Vec<(JsString, JsString)>, JsError> {
    let Value::Object(obj) = options else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "import attributes must be an object".into(),
        ));
    };
    let mut out = Vec::new();
    for key in obj.own_property_keys()? {
        let PropertyKey::String(id) = key else {
            continue;
        };
        let name = crux::lookup(id);
        let value = obj.get(&name)?;
        let value = crux::convert::to_string(&value)?;
        out.push((name, value));
    }
    Ok(out)
}

fn export_name_string(name: Option<&ExportName>) -> Result<JsString, JsError> {
    match name {
        Some(ExportName::Ident(id)) => Ok(crux::lookup(*id)),
        Some(ExportName::Str(text)) => Ok(text.clone()),
        None => Err(JsError::new(
            ErrorKind::TypeError,
            "export has no name".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::promise::PromiseState;

    fn js_str(value: &str) -> Value {
        Value::String(Handle::new(JsString::from_utf8(value)))
    }

    struct Evaluated {
        agent: Agent,
        namespace: Value,
    }

    /// Register `modules`, link and evaluate `entry`, drain jobs, and return
    /// the agent plus the entry module's namespace.
    fn evaluate_modules(modules: &[(&str, &str)], entry: &str) -> Result<Evaluated, JsError> {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm()?;
        for (specifier, source) in modules {
            agent.add_module(specifier, source);
        }
        let module = host_resolve_imported_module(&mut agent, &JsString::from_utf8(entry))?;
        module_declaration_instantiation(&mut agent, &module)?;
        module_evaluation(&mut agent, &module)?;
        agent.run_jobs()?;
        let namespace = module_namespace(&mut agent, &module)?;
        Ok(Evaluated { agent, namespace })
    }

    fn namespace_read(evaluated: &mut Evaluated, name: &str) -> Result<Value, JsError> {
        crate::context::get_property(
            &mut evaluated.agent,
            &evaluated.namespace,
            &JsString::from_utf8(name),
            evaluated.namespace.clone(),
        )
    }

    /// The settled value of a promise (or the value itself if not a promise).
    fn settled(agent: &Agent, value: &Value) -> Result<Value, JsError> {
        let Value::Object(obj) = value else {
            return Ok(value.clone());
        };
        let Some(data) = agent.promises.get(&obj.id()) else {
            return Ok(value.clone());
        };
        match &data.borrow().state {
            PromiseState::Fulfilled(v) | PromiseState::Rejected(v) => Ok(v.clone()),
            PromiseState::Pending { .. } => Err(JsError::new(
                ErrorKind::TypeError,
                "promise still pending".into(),
            )),
        }
    }

    #[test]
    fn exports_are_readable_through_the_namespace() {
        let mut evaluated = evaluate_modules(
            &[(
                "m.js",
                "export var x = 42; export function f() { return 7; }",
            )],
            "m.js",
        )
        .unwrap();
        assert_eq!(
            namespace_read(&mut evaluated, "x").unwrap(),
            Value::Number(42.0)
        );
        assert!(crux::value::is_callable(
            &namespace_read(&mut evaluated, "f").unwrap()
        ));
    }

    #[test]
    fn default_export_expression_binds_the_value() {
        let mut evaluated = evaluate_modules(&[("m.js", "export default 42;")], "m.js").unwrap();
        assert_eq!(
            namespace_read(&mut evaluated, "default").unwrap(),
            Value::Number(42.0)
        );
    }

    #[test]
    fn default_export_anonymous_function_is_exported() {
        let mut evaluated = evaluate_modules(
            &[("m.js", "export default function () { return 1; }")],
            "m.js",
        )
        .unwrap();
        assert!(crux::value::is_callable(
            &namespace_read(&mut evaluated, "default").unwrap()
        ));
    }

    #[test]
    fn imported_bindings_are_live() {
        // m1 exports a mutator; m2 imports the mutator and re-exports the
        // mutated variable. The import binding sees m1's updates.
        let mut evaluated = evaluate_modules(
            &[
                (
                    "m1.js",
                    "export var x = 1; export function bump() { x = 99; }",
                ),
                (
                    "m2.js",
                    "import { bump } from 'm1.js'; import { x } from 'm1.js'; export { x }; export function run() { bump(); return x; }",
                ),
            ],
            "m2.js",
        )
        .unwrap();
        let run = namespace_read(&mut evaluated, "run").unwrap();
        let value =
            crate::function::call(&mut evaluated.agent, &run, Value::Undefined, &[]).unwrap();
        assert_eq!(value, Value::Number(99.0));
    }

    #[test]
    fn namespace_import_is_an_object_with_exports() {
        let mut evaluated = evaluate_modules(
            &[
                ("m1.js", "export var a = 1;"),
                (
                    "m2.js",
                    "import * as ns from 'm1.js'; export var seen = ns.a;",
                ),
            ],
            "m2.js",
        )
        .unwrap();
        assert_eq!(
            namespace_read(&mut evaluated, "seen").unwrap(),
            Value::Number(1.0)
        );
    }

    #[test]
    fn export_star_reexports_except_default() {
        let mut evaluated = evaluate_modules(
            &[
                (
                    "m1.js",
                    "export var a = 1; export var b = 2; export default 'd';",
                ),
                ("m2.js", "export * from 'm1.js'; export var c = 3;"),
            ],
            "m2.js",
        )
        .unwrap();
        assert_eq!(
            namespace_read(&mut evaluated, "a").unwrap(),
            Value::Number(1.0)
        );
        assert_eq!(
            namespace_read(&mut evaluated, "b").unwrap(),
            Value::Number(2.0)
        );
        assert_eq!(
            namespace_read(&mut evaluated, "c").unwrap(),
            Value::Number(3.0)
        );
        // `export *` never re-exports `default`.
        assert_eq!(
            namespace_read(&mut evaluated, "default").unwrap(),
            Value::Undefined
        );
    }

    #[test]
    fn export_star_as_namespace_binds_the_namespace() {
        let mut evaluated = evaluate_modules(
            &[
                ("m1.js", "export var a = 5;"),
                ("m2.js", "export * as m1 from 'm1.js';"),
            ],
            "m2.js",
        )
        .unwrap();
        let m1 = namespace_read(&mut evaluated, "m1").unwrap();
        assert_eq!(
            crate::context::get_property(
                &mut evaluated.agent,
                &m1,
                &JsString::from_utf8("a"),
                m1.clone(),
            )
            .unwrap(),
            Value::Number(5.0)
        );
    }

    #[test]
    fn indirect_reexport_resolves_through_the_source_module() {
        let mut evaluated = evaluate_modules(
            &[
                ("m1.js", "export var x = 10;"),
                ("m2.js", "export { x as y } from 'm1.js';"),
            ],
            "m2.js",
        )
        .unwrap();
        assert_eq!(
            namespace_read(&mut evaluated, "y").unwrap(),
            Value::Number(10.0)
        );
        // The name is not exported under its local name.
        assert_eq!(
            namespace_read(&mut evaluated, "x").unwrap(),
            Value::Undefined
        );
    }

    #[test]
    fn cyclic_imports_link_and_evaluate() {
        let mut evaluated = evaluate_modules(
            &[
                (
                    "a.js",
                    "import { b } from 'b.js'; export function a(n) { return n <= 0 ? 'done' : 'A' + b(n - 1); }",
                ),
                (
                    "b.js",
                    "import { a } from 'a.js'; export function b(n) { return n <= 0 ? 'done' : 'B' + a(n - 1); }",
                ),
            ],
            "a.js",
        )
        .unwrap();
        let a = namespace_read(&mut evaluated, "a").unwrap();
        let value = crate::function::call(
            &mut evaluated.agent,
            &a,
            Value::Undefined,
            &[Value::Number(3.0)],
        )
        .unwrap();
        assert_eq!(value, js_str("ABAdone"));
    }

    #[test]
    fn top_level_await_suspends_then_resumes() {
        let mut evaluated = evaluate_modules(
            &[("m.js", "export var x = await 10; export var y = x + 1;")],
            "m.js",
        )
        .unwrap();
        assert_eq!(
            namespace_read(&mut evaluated, "x").unwrap(),
            Value::Number(10.0)
        );
        assert_eq!(
            namespace_read(&mut evaluated, "y").unwrap(),
            Value::Number(11.0)
        );
    }

    #[test]
    fn json_module_exports_the_parsed_value() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent.add_json_module("./data.json", "{\"a\": 1, \"b\": [2, 3]}");
        agent.add_module(
            "m.js",
            "import data from './data.json' with { type: 'json' }; export var a = data.a; export var len = data.b.length;",
        );
        let module =
            host_resolve_imported_module(&mut agent, &JsString::from_utf8("m.js")).unwrap();
        module_declaration_instantiation(&mut agent, &module).unwrap();
        module_evaluation(&mut agent, &module).unwrap();
        agent.run_jobs().unwrap();
        let namespace = module_namespace(&mut agent, &module).unwrap();
        assert_eq!(
            crate::context::get_property(
                &mut agent,
                &namespace,
                &JsString::from_utf8("a"),
                namespace.clone(),
            )
            .unwrap(),
            Value::Number(1.0)
        );
        assert_eq!(
            crate::context::get_property(
                &mut agent,
                &namespace,
                &JsString::from_utf8("len"),
                namespace.clone(),
            )
            .unwrap(),
            Value::Number(2.0)
        );
    }

    #[test]
    fn dynamic_import_resolves_with_the_namespace() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent.add_module("./m.js", "export var x = 42; export default 'd';");
        let promise = agent
            .run_script("import('./m.js').then(function (ns) { return ns.x + ':' + ns.default; })")
            .unwrap();
        agent.run_jobs().unwrap();
        assert_eq!(settled(&agent, &promise).unwrap(), js_str("42:d"));
    }

    #[test]
    fn import_meta_is_an_object() {
        let mut evaluated =
            evaluate_modules(&[("m.js", "export var t = typeof import.meta;")], "m.js").unwrap();
        assert_eq!(
            namespace_read(&mut evaluated, "t").unwrap(),
            js_str("object")
        );
    }

    #[test]
    fn namespace_has_symbol_to_string_tag() {
        // spec 28.3.1: the namespace's @@toStringTag is "Module".
        let mut evaluated = evaluate_modules(&[("m.js", "export var x = 1;")], "m.js").unwrap();
        let tag = crate::context::get_property_key(
            &mut evaluated.agent,
            &evaluated.namespace,
            &crux::property::PropertyKey::Symbol(
                crux::symbol::well_known("toStringTag").as_ref().clone(),
            ),
            evaluated.namespace.clone(),
        )
        .unwrap();
        assert_eq!(tag, js_str("Module"));
    }

    #[test]
    fn module_functions_render_exact_source() {
        // Functions instantiated during module linking still capture their
        // source text from the module (Function.prototype.toString).
        let mut evaluated = evaluate_modules(
            &[(
                "m.js",
                "export function greet(name) { return 'hi ' + name; }",
            )],
            "m.js",
        )
        .unwrap();
        let greet = namespace_read(&mut evaluated, "greet").unwrap();
        let to_string = crate::context::get_property(
            &mut evaluated.agent,
            &greet,
            &JsString::from_utf8("toString"),
            greet.clone(),
        )
        .unwrap();
        let rendered = crate::function::call(&mut evaluated.agent, &to_string, greet, &[]).unwrap();
        assert_eq!(
            rendered,
            js_str("function greet(name) { return 'hi ' + name; }")
        );
    }
}
