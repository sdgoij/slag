//! Modules (spec ch. 16): Source Text Module Records, declaration
//! instantiation (linking), evaluation with top-level await, dynamic import,
//! import.meta, and JSON modules.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::Ordering;

use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::heap::{GcAny, Trace};
use crux::object::JsObject;
use crux::property::PropertyKey;
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use syntax::ast::{
    Argument, ArrayElement, AttributeKey, BindingPattern, Class, ClassElement, ClassElementName,
    ExportDecl, ExportDefault, ExportName, ExportSpecifier, Expr, ExprKind, ForBinding, ForInit,
    ImportEntry, ImportPhase, MemberProperty, Module, ModuleItem, ObjectProperty, PropertyName,
    Stmt, StmtKind, VarDeclKind, VarDeclarator,
};

use crate::agent::Agent;
use crate::async_await::AsyncFunctionState;
use crate::context::{ExecutionContext, ScriptOrModule};
use crate::env::{EnvRef, create_import_binding, new_module_environment};
use crate::flow::Completion;
use crate::ir::{Suspension, Vm, VmOutcome};
use crate::realm::Realm;

/// The wait state of an `import.defer(...).then(...)` promise: (remaining,
/// capability resolve, capability reject, module).
#[derive(Debug, Clone)]
pub struct DeferredWait(
    pub std::rc::Rc<std::cell::RefCell<u32>>,
    pub Value,
    pub Value,
    pub Handle<crate::module::SourceTextModule>,
    pub Value,
    pub Value,
);

impl Trace for DeferredWait {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        // `.0` is the remaining-dependency counter (a plain u32).
        self.1.trace(visit);
        self.2.trace(visit);
        self.3.trace(visit);
        self.4.trace(visit);
        self.5.trace(visit);
    }
}

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
    /// The module kind (JavaScript/JSON/text/bytes) selected at resolution
    /// by the import attributes.
    pub(crate) kind: ModuleKind,
    pub status: RefCell<ModuleStatus>,
    pub environment: RefCell<Option<EnvRef>>,
    pub namespace: RefCell<Option<Value>>,
    /// The cycle root of this module's strongly connected component (spec
    /// 16.2.2.5 [[CycleRoot]]): set when a dependency cycle is closed during
    /// evaluation; an errored cycle's members re-import through it.
    pub cycle_root: RefCell<Option<Handle<SourceTextModule>>>,
    /// The modules (outside this module's cycle) waiting on this module's
    /// evaluation — [[AsyncParentModules]]: errors and fulfillments
    /// propagate to them.
    pub async_parents: RefCell<Vec<Handle<SourceTextModule>>>,
    /// [[PendingAsyncDependencies]]: the async dependency modules still being
    /// evaluated before this module's body can run.
    pub pending_async: RefCell<u32>,
    pub requested_modules: Vec<ModuleRequest>,
    /// (module specifier, entry, phase) of each import.
    pub import_entries: Vec<ModuleImport>,
    pub local_export_entries: Vec<ExportEntry>,
    pub indirect_export_entries: Vec<ExportEntry>,
    pub star_export_entries: Vec<ExportEntry>,
    pub top_level_capability: RefCell<Option<crate::promise::PromiseCapability>>,
    pub evaluation_error: RefCell<Option<Value>>,
    /// [[ImportMeta]] (spec 16.2.1.8): created on first access and cached.
    pub import_meta: RefCell<Option<Value>>,
    /// [[ModuleSource]] (source-phase-imports): the `%AbstractModuleSource%`
    /// object wrapping this module's source, created on first access.
    pub module_source: RefCell<Option<Value>>,
    /// [[DeferredNamespace]] (import-defer): the deferred module namespace
    /// object, created on first access.
    pub deferred_namespace: RefCell<Option<Value>>,
}

impl Trace for SourceTextModule {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.realm.trace(visit);
        if let Some(environment) = &*self.environment.borrow() {
            environment.trace(visit);
        }
        if let Some(namespace) = &*self.namespace.borrow() {
            namespace.trace(visit);
        }
        if let Some(cycle_root) = &*self.cycle_root.borrow() {
            cycle_root.trace(visit);
        }
        for parent in &*self.async_parents.borrow() {
            parent.trace(visit);
        }
        if let Some(capability) = &*self.top_level_capability.borrow() {
            capability.trace(visit);
        }
        if let Some(error) = &*self.evaluation_error.borrow() {
            error.trace(visit);
        }
        if let Some(meta) = &*self.import_meta.borrow() {
            meta.trace(visit);
        }
        if let Some(source) = &*self.module_source.borrow() {
            source.trace(visit);
        }
        if let Some(namespace) = &*self.deferred_namespace.borrow() {
            namespace.trace(visit);
        }
        // The module source and the module-record strings (specifiers,
        // import attributes, export names) are JsStrings: a rope's children
        // are heap edges. `code` is the parsed AST (plain data) — its
        // strings are parse-produced flats with no heap edges.
        self.source.trace(visit);
        for (specifier, attributes, _) in &self.requested_modules {
            specifier.trace(visit);
            for (key, value) in attributes {
                if let AttributeKey::Str(name) = key {
                    name.trace(visit);
                }
                value.trace(visit);
            }
        }
        for (specifier, entry, _) in &self.import_entries {
            specifier.trace(visit);
            if let ImportEntry::Named { imported, .. } = entry
                && let ExportName::Str(name) = imported
            {
                name.trace(visit);
            }
        }
        for entry in self
            .local_export_entries
            .iter()
            .chain(&self.indirect_export_entries)
            .chain(&self.star_export_entries)
        {
            if let Some(name) = &entry.export_name
                && let ExportName::Str(name) = name
            {
                name.trace(visit);
            }
            if let Some(request) = &entry.module_request {
                request.trace(visit);
            }
            if let Some(name) = &entry.import_name
                && let ExportName::Str(name) = name
            {
                name.trace(visit);
            }
        }
    }
}

/// A host-provided module source (HostResolveImportedModule): the raw
/// bytes of the module file. The module kind (JavaScript, JSON, text, or
/// bytes) is derived from the requested import attributes at resolution,
/// falling back to the registered kind (test262's `<module source>` host
/// artifact is a text module) and then the `.json` extension.
#[derive(Debug, Clone)]
pub struct HostModuleSource {
    pub bytes: Vec<u8>,
    pub(crate) kind: ModuleKind,
}

impl Trace for HostModuleSource {
    fn trace(&self, _visit: &mut dyn FnMut(GcAny)) {}
}

impl Agent {
    /// Test/CLI hook: register a module the host can resolve by specifier.
    pub fn add_module(&mut self, specifier: &str, source: &str) {
        self.host_modules.borrow_mut().insert(
            JsString::from_utf8(specifier),
            HostModuleSource {
                bytes: source.as_bytes().to_vec(),
                kind: ModuleKind::Js,
            },
        );
    }

    /// Test/CLI hook: register a raw-bytes module (text/bytes modules).
    pub fn add_bytes_module(&mut self, specifier: &str, bytes: &[u8]) {
        self.host_modules.borrow_mut().insert(
            JsString::from_utf8(specifier),
            HostModuleSource {
                bytes: bytes.to_vec(),
                kind: ModuleKind::Js,
            },
        );
    }

    /// Test/CLI hook: register a source-capable module (the test262
    /// `<module source>` host artifact): a text module whose source is
    /// available to the source phase.
    pub fn add_source_module(&mut self, specifier: &str, source: &str) {
        self.host_modules.borrow_mut().insert(
            JsString::from_utf8(specifier),
            HostModuleSource {
                bytes: source.as_bytes().to_vec(),
                kind: ModuleKind::Text,
            },
        );
    }

    /// Test/CLI hook: register a JSON module (the `.json` extension selects
    /// the JSON kind at resolution; kept for callers that pass non-`.json`
    /// specifiers).
    pub fn add_json_module(&mut self, specifier: &str, json: &str) {
        self.add_bytes_module(specifier, json.as_bytes());
    }
}

/// The import/export records of a module, collected from its AST
/// (spec 16.2.1.7-16.2.1.10). Built before the module handle is shared so the
/// entries can be populated without interior mutability. Each request carries
/// its phase (the plain `import`, or the source/deferred phases of
/// `import source`/`import defer`).
/// A module request: (specifier, import attributes, import phase).
pub type ModuleRequest = (JsString, Vec<(AttributeKey, JsString)>, ImportPhase);
/// An import entry: (specifier, entry, import phase).
pub type ModuleImport = (JsString, ImportEntry, ImportPhase);
struct ModuleRecords {
    requested_modules: Vec<ModuleRequest>,
    import_entries: Vec<ModuleImport>,
    local_export_entries: Vec<ExportEntry>,
    indirect_export_entries: Vec<ExportEntry>,
    star_export_entries: Vec<ExportEntry>,
}

/// Parse a module source into a Source Text Module Record. The module kind
/// comes from the requested import attributes (`type: json|text|bytes|js`),
/// falling back to the `.json` extension for attribute-less imports.
pub fn parse_module(
    agent: &mut Agent,
    specifier: &JsString,
    source: &JsString,
    attributes: &[(AttributeKey, JsString)],
) -> Result<Handle<SourceTextModule>, JsError> {
    let realm = agent.current_realm()?;
    crate::expr::bump_template_parse_generation();
    let kind = module_kind(agent, specifier, attributes);
    let code = {
        let text = || {
            agent
                .host_modules
                .borrow()
                .get(specifier)
                .map(|entry| String::from_utf8_lossy(&entry.bytes).into_owned())
                .unwrap_or_else(|| source.to_string_lossy())
        };
        match kind {
            ModuleKind::Json => {
                // A JSON module is a module whose default export is the JSON
                // value: `export default <json>`. The source must be
                // well-formed JSON first (spec 16.2.1.7.1 ParseModule): an
                // invalid source is a SyntaxError at resolution, even though
                // the wrapped text would parse as JavaScript.
                let text = text();
                crate::builtins::json::validate_json(agent, &text)?;
                let wrapped = format!("export default {text}");
                parser::parse_module(&wrapped)?
            }
            ModuleKind::Text => {
                // A text module is a Synthetic Module whose default export
                // is the raw source text as a string (CreateTextModule /
                // CreateDefaultExportSyntheticModule); the source is not
                // parsed as JavaScript.
                let text = text();
                let wrapped = format!("export default {:?}", text);
                parser::parse_module(&wrapped)?
            }
            ModuleKind::Bytes => {
                // A bytes module is a Synthetic Module whose default export
                // is an immutable Uint8Array of the raw file bytes
                // (CreateBytesModule / the immutable-arraybuffer proposal):
                // `buffer.immutable` is true and resize/transfer throw. The
                // buffer is created immutable via transferToImmutable.
                let bytes = agent
                    .host_modules
                    .borrow()
                    .get(specifier)
                    .map(|entry| entry.bytes.clone())
                    .unwrap_or_default();
                let values = bytes
                    .iter()
                    .map(|b| b.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                let wrapped = format!(
                    "export default new Uint8Array(Uint8Array.from([{values}]).buffer.transferToImmutable())"
                );
                parser::parse_module(&wrapped)?
            }
            ModuleKind::Js => parser::parse_module(&source.to_string_lossy())?,
        }
    };
    let records = collect_module_records(&code);
    let module = Handle::new(SourceTextModule {
        realm,
        code,
        source: source.clone(),
        kind,
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
        import_meta: RefCell::new(None),
        module_source: RefCell::new(None),
        deferred_namespace: RefCell::new(None),
        cycle_root: RefCell::new(None),
        async_parents: RefCell::new(Vec::new()),
        pending_async: RefCell::new(0),
    });
    Ok(module)
}

/// The module kind requested by import attributes, or the `.json` extension
/// fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModuleKind {
    Js,
    Json,
    Text,
    Bytes,
}

fn module_kind(
    agent: &Agent,
    specifier: &JsString,
    attributes: &[(AttributeKey, JsString)],
) -> ModuleKind {
    if let Some((_, value)) = attributes.iter().find(|(key, _)| key_string(key) == "type") {
        return match value.to_string_lossy().as_str() {
            "json" => ModuleKind::Json,
            "text" => ModuleKind::Text,
            "bytes" => ModuleKind::Bytes,
            _ => ModuleKind::Js,
        };
    }
    if let Some(entry) = agent.host_modules.borrow().get(specifier)
        && entry.kind != ModuleKind::Js
    {
        return entry.kind;
    }
    if specifier.to_string_lossy().ends_with(".json") {
        ModuleKind::Json
    } else {
        ModuleKind::Js
    }
}

fn key_string(key: &AttributeKey) -> String {
    match key {
        AttributeKey::Ident(atom) => crux::lookup(*atom).to_string_lossy(),
        AttributeKey::Str(text) => text.to_string_lossy(),
    }
}

/// The kind of an already-resolved module record.
pub(crate) fn module_kind_of(module: &Handle<SourceTextModule>) -> ModuleKind {
    module.kind
}

/// CreateModuleSourceObject (source-phase-imports): an `%AbstractModuleSource%`
/// instance wrapping the module. Cached on the module record; the source text
/// comes from the host module bytes at `toString` time.
pub(crate) fn module_source_object(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
) -> Result<Value, JsError> {
    if let Some(source) = module.module_source.borrow().clone() {
        return Ok(source);
    }
    let realm = agent.current_realm()?;
    let prototype = realm
        .intrinsics
        .get("%AbstractModuleSource.prototype%")
        .and_then(|value| value.as_object());
    let object = JsObject::ordinary_object_create(prototype);
    let value = Value::Object(object);
    agent.module_sources.insert(object.id(), *module);
    module.module_source.replace(Some(value.clone()));
    Ok(value)
}

/// The raw source text of a module, for the source-phase import of a
/// synthetic (JSON/text/bytes) module.
pub(crate) fn module_source_text(
    agent: &Agent,
    module: &Handle<SourceTextModule>,
) -> Result<Value, JsError> {
    let realm = agent.current_realm()?;
    let specifier = realm
        .loaded_modules
        .borrow()
        .iter()
        .find(|(_, m)| Handle::ptr_eq(**m, *module))
        .map(|(specifier, _)| specifier.clone());
    let bytes = specifier
        .and_then(|specifier| agent.host_modules.borrow().get(&specifier).cloned())
        .map(|entry| entry.bytes)
        .unwrap_or_default();
    Ok(Value::String(Handle::new(JsString::from_utf8(
        &String::from_utf8_lossy(&bytes),
    ))))
}

/// HostResolveImportedModule (spec 16.6.1.1.2): the agent's registered module
/// map, cached in the realm's [[LoadedModules]]. `attributes` are the import
/// attributes of the requesting import (they select the module kind).
pub fn host_resolve_imported_module(
    agent: &mut Agent,
    specifier: &JsString,
    attributes: &[(AttributeKey, JsString)],
) -> Result<Handle<SourceTextModule>, JsError> {
    let realm = agent.current_realm()?;
    if let Some(module) = realm.loaded_modules.borrow().get(specifier) {
        return Ok(*module);
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
    let text = String::from_utf8_lossy(&source.bytes).into_owned();
    let source_text = JsString::from_utf8(&text);
    let module = parse_module(agent, specifier, &source_text, attributes)?;
    realm
        .loaded_modules
        .borrow_mut()
        .insert(specifier.clone(), module);
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
                    import_entries.push((import.specifier.clone(), entry.clone(), import.phase));
                }
                requested_modules.push((
                    import.specifier.clone(),
                    import.attributes.clone(),
                    import.phase,
                ));
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
                        // A re-export of an imported name resolves through the
                        // import (spec 16.2.1.7.1 step 10): a namespace import
                        // becomes an indirect namespace export (both star
                        // resolutions then agree on the namespace), and a
                        // named import an indirect single-name export.
                        if let Some((specifier, entry, phase)) = import_entries
                            .iter()
                            .find(|(_, entry, _)| matches!(entry, ImportEntry::Namespace { local, .. } | ImportEntry::Named { local, .. } | ImportEntry::Default { local, .. } if crux::lookup(*local) == crux::lookup(local_id)))
                        {
                            if *phase == ImportPhase::Source {
                                // A re-export of a source-phase import
                                // (`import source x from …; export { x }`) is
                                // reclassified to an indirect export whose
                                // [[ImportName]] is ~source~: ResolveExport
                                // resolves it to the target module's
                                // ModuleSource object.
                                indirect_export_entries.push(ExportEntry {
                                    export_name: Some(exported),
                                    module_request: Some(specifier.clone()),
                                    import_name: Some(ExportName::Str(source_marker())),
                                    local_name: None,
                                });
                                continue;
                            }
                            if *phase == ImportPhase::Defer
                                && matches!(entry, ImportEntry::Namespace { .. })
                            {
                                // A re-export of a deferred namespace import
                                // (`import defer * as ns from …;
                                // export { ns }`) is reclassified to an
                                // indirect export resolving to the module's
                                // deferred namespace object (import-defer).
                                indirect_export_entries.push(ExportEntry {
                                    export_name: Some(exported),
                                    module_request: Some(specifier.clone()),
                                    import_name: Some(ExportName::Str(defer_marker())),
                                    local_name: None,
                                });
                                continue;
                            }
                            match entry {
                                ImportEntry::Namespace { .. } => {
                                    indirect_export_entries.push(ExportEntry {
                                        export_name: Some(exported),
                                        module_request: Some(specifier.clone()),
                                        import_name: None,
                                        local_name: None,
                                    });
                                }
                                ImportEntry::Named { imported, .. } => {
                                    let import_name = match imported {
                                        ExportName::Ident(id) => ExportName::Ident(*id),
                                        ExportName::Str(text) => ExportName::Str(text.clone()),
                                    };
                                    indirect_export_entries.push(ExportEntry {
                                        export_name: Some(exported),
                                        module_request: Some(specifier.clone()),
                                        import_name: Some(import_name),
                                        local_name: None,
                                    });
                                }
                                ImportEntry::Default { .. } => {
                                    indirect_export_entries.push(ExportEntry {
                                        export_name: Some(exported),
                                        module_request: Some(specifier.clone()),
                                        import_name: Some(ExportName::Ident(crux::intern(
                                            "default".encode_utf16().collect::<Vec<u16>>().as_slice(),
                                        ))),
                                        local_name: None,
                                    });
                                }
                            }
                            continue;
                        }
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
                        // whose value is the imported module's namespace. The
                        // export name may be a string (arbitrary module
                        // namespace names).
                        let local = match namespace {
                            ExportName::Ident(id) => ExportName::Ident(*id),
                            ExportName::Str(text) => ExportName::Str(text.clone()),
                        };
                        local_export_entries.push(ExportEntry {
                            export_name: Some(local),
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
                    requested_modules.push((
                        specifier.clone(),
                        attributes.clone(),
                        ImportPhase::Import,
                    ));
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
    // NewModuleEnvironment(module.[[Realm]].[[GlobalEnv]]) (spec
    // 16.2.1.6.1.2.1 step 4): the module env chains to the global env so
    // module bodies resolve realm globals (URIError, assert, …).
    let env = new_module_environment(Some(module.realm.global_env));
    module.environment.replace(Some(env));

    // spec 16.2.1.6.1.2.1 step 8: every requested module — including those
    // referenced only by `export *`/`export {} from` — must resolve before
    // this module's own bindings are created. Resolving an import can
    // land on a star-reached module, whose environment must exist already;
    // instantiation is idempotent through the status check. All requested
    // modules resolve first (in source order): a resolution failure is a
    // host error that aborts before any export resolution runs, so an
    // unresolvable specifier surfaces ahead of a transitive linking error
    // (`source-phase-import/import-source.js`).
    let requested = module.requested_modules.clone();
    let mut resolved: Vec<Handle<SourceTextModule>> = Vec::with_capacity(requested.len());
    for (specifier, attributes, _) in &requested {
        let imported = host_resolve_imported_module(agent, specifier, attributes)?;
        resolved.push(imported);
    }
    for imported in &resolved {
        module_declaration_instantiation(agent, imported)?;
    }

    // Import bindings (live, through the imported module's environment). A
    // source-phase import binds the imported module's ModuleSource object
    // (spec 16.2.2.4 step 28); a deferred import binds the deferred namespace
    // (lazily evaluated on access).
    let imports = module.import_entries.clone();
    for (specifier, entry, phase) in imports {
        let imported = host_resolve_imported_module(agent, &specifier, &[])?;
        module_declaration_instantiation(agent, &imported)?;
        match phase {
            ImportPhase::Source => {
                let ImportEntry::Default { local, .. } = entry else {
                    return Err(JsError::new(
                        ErrorKind::SyntaxError,
                        "source-phase import must have a single binding".into(),
                    ));
                };
                let source = module_source_object(agent, &imported)?;
                let name = crux::lookup(local);
                env.create_immutable_binding(&name, true)?;
                env.initialize_binding(&name, source)?;
                continue;
            }
            ImportPhase::Defer => {
                // The deferred form is a namespace import only; the binding is
                // the deferred namespace object (module namespace exotic
                // object with [[Deferred]] = true), evaluated lazily on
                // access.
                let ImportEntry::Namespace { local, .. } = entry else {
                    return Err(JsError::new(
                        ErrorKind::SyntaxError,
                        "deferred import must be a namespace import".into(),
                    ));
                };
                let namespace = deferred_namespace(agent, &imported)?;
                let name = crux::lookup(local);
                env.create_immutable_binding(&name, true)?;
                env.initialize_binding(&name, namespace)?;
                continue;
            }
            ImportPhase::Import => {}
        }
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
                let target_env =
                    target
                        .environment
                        .borrow()
                        .as_ref()
                        .copied()
                        .ok_or_else(|| {
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
            // A named import of a re-exported deferred-namespace binding binds
            // the underlying module's deferred namespace object.
            Some(ResolvedBinding::DeferredNamespace(target)) => {
                let namespace = deferred_namespace(agent, &target)?;
                env.create_immutable_binding(&local, true)?;
                env.initialize_binding(&local, namespace)?;
            }
            // A named import of a re-exported source-phase binding binds the
            // underlying module's ModuleSource object (spec
            // InitializeEnvironment step: resolution.[[BindingName]] is
            // ~source~).
            Some(ResolvedBinding::Source(target)) => {
                let source = module_source_object(agent, &target)?;
                env.create_immutable_binding(&local, true)?;
                env.initialize_binding(&local, source)?;
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
    // Instantiate the module's top-level declarations first: the export loop
    // below must not pre-create bindings for names the declarations bind
    // (spec 16.2.1.6.1.2.1 step 7 creates an export binding only when the
    // local name is not declared — `const x; export { x }` collides
    // otherwise).
    instantiate_module_declarations(agent, module, &env)?;

    // Local export bindings.
    let local_exports = module.local_export_entries.clone();
    for export in &local_exports {
        if let Some(module_request) = &export.module_request {
            // `export * as ns from ...`: bind the imported module's namespace
            // (spec 16.2.2.4 step 28) at instantiation time.
            let imported = host_resolve_imported_module(agent, module_request, &[])?;
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

    // Indirect export bindings (re-exports). InitializeEnvironment only
    // validates that each re-export resolves; the binding itself is read
    // through ResolveExport at namespace access time (spec 16.2.1.7.3.1
    // step 1).
    let indirect_exports = module.indirect_export_entries.clone();
    for export in &indirect_exports {
        let specifier = export.module_request.as_ref().ok_or_else(|| {
            JsError::new(ErrorKind::TypeError, "indirect export has no module".into())
        })?;
        let imported = host_resolve_imported_module(agent, specifier, &[])?;
        module_declaration_instantiation(agent, &imported)?;
        // A namespace re-export (`import * as ns; export { ns }`) has no
        // import name: the binding is the imported module's namespace.
        if export.import_name.is_none() {
            module_namespace(agent, &imported)?;
            continue;
        }
        let import_name = export_name_string(export.import_name.as_ref())?;
        // A reclassified source/deferred marker resolves to the imported
        // module itself (the ModuleSource / deferred namespace object), not
        // to an export name — no further validation is needed.
        if import_name == source_marker() || import_name == defer_marker() {
            continue;
        }
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

/// The var-declared names of a statement, descending into statement bodies
/// (VarDeclaredNames semantics, spec 16.2.1.7.1.5): a `var` in a for head, an
/// if branch, or a nested block hoists to the module. Function and class
/// bodies are separate units and contribute nothing.
fn collect_module_var_names(stmt: &Stmt, out: &mut Vec<JsString>) {
    match &stmt.kind {
        StmtKind::VarDecl {
            kind: VarDeclKind::Var,
            decls,
            ..
        } => {
            for decl in decls {
                crate::script::bound_names(&decl.pattern, out);
            }
        }
        StmtKind::Block(block) => {
            for inner in &block.stmts {
                collect_module_var_names(inner, out);
            }
        }
        StmtKind::If {
            consequent,
            alternate,
            ..
        } => {
            collect_module_var_names(consequent, out);
            if let Some(alternate) = alternate {
                collect_module_var_names(alternate, out);
            }
        }
        StmtKind::While { body, .. } | StmtKind::DoWhile { body, .. } => {
            collect_module_var_names(body, out);
        }
        StmtKind::For { init, body, .. } => {
            if let Some(syntax::ForInit::VarDecl {
                kind: VarDeclKind::Var,
                decls,
            }) = init
            {
                for decl in decls {
                    crate::script::bound_names(&decl.pattern, out);
                }
            }
            collect_module_var_names(body, out);
        }
        StmtKind::ForIn { left, body, .. } | StmtKind::ForOf { left, body, .. } => {
            if let syntax::ForBinding::VarDecl {
                kind: VarDeclKind::Var,
                pattern,
                ..
            } = left
            {
                crate::script::bound_names(pattern, out);
            }
            collect_module_var_names(body, out);
        }
        StmtKind::Labeled { body, .. } => collect_module_var_names(body, out),
        StmtKind::With { body, .. } => collect_module_var_names(body, out),
        StmtKind::Switch { cases, .. } => {
            for case in cases {
                for inner in &case.consequent {
                    collect_module_var_names(inner, out);
                }
            }
        }
        StmtKind::Try {
            block,
            handler,
            finalizer,
        } => {
            for inner in &block.stmts {
                collect_module_var_names(inner, out);
            }
            if let Some(handler) = handler {
                for inner in &handler.body.stmts {
                    collect_module_var_names(inner, out);
                }
            }
            if let Some(finalizer) = finalizer {
                for inner in &finalizer.stmts {
                    collect_module_var_names(inner, out);
                }
            }
        }
        _ => {}
    }
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
    // VarDeclaredNames descends into statement bodies (for heads, if
    // branches, blocks), so the hoisted bindings exist for every `var`
    // anywhere in the module body (spec 16.2.2.4 steps 22-27).
    let mut var_names: Vec<JsString> = Vec::new();
    for stmt in &stmts {
        collect_module_var_names(stmt, &mut var_names);
    }
    for name in var_names {
        if !env.has_binding(&name)? {
            env.create_mutable_binding(&name, false)?;
        }
        // spec 16.2.1.7.3.1 step 6: var bindings start initialized to
        // *undefined* (a pre-existing binding is a declaration the pre-pass
        // created, or a function/import name the parser forbids from
        // colliding with a var).
        env.initialize_binding(&name, Value::Undefined)?;
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
                *env,
                true,
                source,
                Vec::new(),
                Vec::new(),
            )?;
            // The function's `import.meta` resolves lexically to this module
            // (spec 13.3.7.1); instantiation runs in the harness context, so
            // record the declaring module explicitly.
            if let ValueKind::Function(handle) = func.kind()
                && let Some(record) = agent.ecma_functions.get_mut(&handle.id())
            {
                record.declaring_module = Some(*module);
            }
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
            StmtKind::UsingDecl { decls, .. } => {
                for decl in decls {
                    let mut names = Vec::new();
                    crate::script::bound_names(&decl.pattern, &mut names);
                    for name in names {
                        env.create_mutable_binding(&name, false)?;
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
    // The synthesized `*default*` binding for `export default expr` is
    // created uninitialized by the lexical-declaration loop above and
    // initialized by the body's `let *default* = <expr>` statement: it is in
    // the temporal dead zone until the module body evaluates it (spec
    // 15.2.3.11 InitializeBoundName). A default function/class declaration
    // was already instantiated and bound by the earlier loops.
    for export in &module.local_export_entries {
        if let Some(local) = export.local_name
            && crux::lookup(local) == JsString::from_utf8("*default*")
        {
            let name = JsString::from_utf8("*default*");
            if !env.has_binding(&name)? {
                env.create_mutable_binding(&name, false)?;
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
                    // `export default <expr>`: a lexical binding that the
                    // body's InitializeBoundName (spec 15.2.3.11 step 5)
                    // initializes at evaluation time — the binding is TDZ
                    // until then, like any other lexical declaration.
                    let atom =
                        crux::intern("*default*".encode_utf16().collect::<Vec<u16>>().as_slice());
                    stmts.push(Stmt {
                        span: expr.span,
                        kind: StmtKind::VarDecl {
                            kind: VarDeclKind::Let,
                            decls: vec![VarDeclarator {
                                pattern: BindingPattern::Ident(atom),
                                init: Some(expr.clone()),
                                span: expr.span,
                            }],
                        },
                    });
                }
            },
            _ => {}
        }
    }
    stmts
}

/// ModuleEvaluation (spec 16.2.2.5): create the namespace and execute the
/// module's body, deferring it until any async dependencies settle and
/// recording the cycle root when a dependency cycle is detected.
pub fn module_evaluation(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
) -> Result<Value, JsError> {
    let promise_ctor = || {
        module
            .realm
            .intrinsics
            .get("%Promise%")
            .unwrap_or(Value::Undefined)
    };
    let status = *module.status.borrow();
    match status {
        ModuleStatus::Evaluated => {
            // spec Evaluate steps 2-3: an errored module — or a fulfilled
            // member of an errored cycle, redirected through its cycle root —
            // rejects a fresh capability with the recorded error.
            let recorded = module.evaluation_error.borrow().clone().or_else(|| {
                module
                    .cycle_root
                    .borrow()
                    .as_ref()
                    .and_then(|root| root.evaluation_error.borrow().clone())
            });
            if let Some(error) = recorded {
                let capability = crate::promise::new_promise_capability(agent, &promise_ctor())?;
                crate::function::call(agent, &capability.reject, Value::Undefined, &[error])?;
                return Ok(capability.promise);
            }
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
    agent.module_eval_stack.push(*module);
    // Evaluate the module's dependencies depth-first (spec 16.2.1.6.1.3.1
    // steps 11-12). A dependency that is already evaluating is a cycle: its
    // body finishes on the current evaluation stack, so it is skipped here.
    let dependencies = module.requested_modules.clone();
    let dependencies_result = (|| -> Result<(), JsError> {
        // A source- or deferred-phase request is not evaluated by the wave
        // (`import source`/`import defer` do not evaluate their target). A
        // deferred request still forces its asynchronous transitive
        // dependencies to evaluate, and this module waits on them
        // (InnerModuleEvaluation step 12 + GatherAsynchronousTransitiveDependencies):
        // `import defer` of a top-level-await module runs that module's
        // async evaluation.
        for (specifier, _, phase) in dependencies {
            let imported = host_resolve_imported_module(agent, &specifier, &[])?;
            if phase == ImportPhase::Defer {
                let async_deps =
                    gather_async_transitive_dependencies(agent, &imported, &mut Vec::new())?;
                for dep in async_deps {
                    let dep_status = *dep.status.borrow();
                    match dep_status {
                        ModuleStatus::Evaluated => {
                            if let Some(error) = dep.evaluation_error.borrow().clone() {
                                return Err(JsError::new(
                                    ErrorKind::TypeError,
                                    "dependency module errored".into(),
                                )
                                .with_value(error));
                            }
                        }
                        ModuleStatus::EvaluatingAsync => {
                            register_async_parent(agent, module, &dep)?;
                        }
                        _ => {
                            module_evaluation(agent, &dep)?;
                            if *dep.status.borrow() == ModuleStatus::EvaluatingAsync {
                                register_async_parent(agent, module, &dep)?;
                            }
                        }
                    }
                }
                continue;
            }
            if phase != ImportPhase::Import {
                continue;
            }
            let status = *imported.status.borrow();
            match status {
                ModuleStatus::Evaluating => {
                    // A dependency cycle: the imported module is the first
                    // member of the cycle on the evaluation stack, hence the
                    // [[CycleRoot]]; record it on every member above it.
                    if let Some(index) = agent
                        .module_eval_stack
                        .iter()
                        .position(|m| Handle::ptr_eq(*m, imported))
                    {
                        let root = imported;
                        for member in &agent.module_eval_stack[index..] {
                            if member.cycle_root.borrow().is_none() {
                                *member.cycle_root.borrow_mut() = Some(root);
                            }
                        }
                    }
                }
                ModuleStatus::Evaluated => {
                    // Already settled: an errored dependency aborts this
                    // module's evaluation with the recorded error (spec
                    // 16.2.2.5 InnerModuleEvaluation step 6 — the abrupt
                    // completion propagates).
                    if let Some(error) = imported.evaluation_error.borrow().clone() {
                        return Err(JsError::new(
                            ErrorKind::TypeError,
                            "dependency module errored".into(),
                        )
                        .with_value(error));
                    }
                }
                ModuleStatus::EvaluatingAsync => {
                    // An async dependency still being evaluated: wait on it
                    // (through its cycle root) before running this body.
                    register_async_parent(agent, module, &imported)?;
                }
                _ => {
                    module_evaluation(agent, &imported)?;
                    // A synchronously-errored dependency aborts this module's
                    // evaluation with the dependency's error (spec 16.2.2.5
                    // InnerModuleEvaluation step 6).
                    if let Some(error) = imported.evaluation_error.borrow().clone() {
                        return Err(JsError::new(
                            ErrorKind::TypeError,
                            "dependency module errored".into(),
                        )
                        .with_value(error));
                    }
                    if *imported.status.borrow() == ModuleStatus::EvaluatingAsync {
                        register_async_parent(agent, module, &imported)?;
                    }
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = dependencies_result {
        module.status.replace(ModuleStatus::Evaluated);
        agent.module_eval_stack.pop();
        return Err(error);
    }
    let capability = crate::promise::new_promise_capability(agent, &promise_ctor())?;
    module
        .top_level_capability
        .replace(Some(capability.clone()));
    // The body may suspend on top-level await, so it always runs in the
    // resumable driver; synchronous modules complete on the first pass.
    module.status.replace(ModuleStatus::EvaluatingAsync);
    if *module.pending_async.borrow() > 0 {
        // spec 16.2.2.5 steps 13-14: the body is deferred until the async
        // dependencies settle (AsyncModuleExecutionFulfilled/Rejected).
        agent.module_eval_stack.pop();
        return Ok(capability.promise);
    }
    let body_result = execute_module_body(agent, module);
    agent.module_eval_stack.pop();
    body_result?;
    Ok(capability.promise)
}

/// Record `module` as waiting on `dep`'s evaluation — spec 16.2.2.5
/// [[AsyncParentModules]]. A non-cycle importer of a cycle member waits on
/// the cycle's root, so its body runs only after the whole cycle finishes
/// (pending-async-dep-from-cycle). A cycle member waits on the dependency
/// itself: its body must not run until the async cycle-member dependency
/// settles.
fn register_async_parent(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
    dep: &Handle<SourceTextModule>,
) -> Result<(), JsError> {
    let _ = agent;
    let target = if module.cycle_root.borrow().is_some() {
        *dep
    } else {
        (*dep.cycle_root.borrow()).unwrap_or(*dep)
    };
    target.async_parents.borrow_mut().push(*module);
    *module.pending_async.borrow_mut() += 1;
    Ok(())
}

/// Run a module's body in the resumable driver (spec 16.2.2.5 steps 12+):
/// the module must already be EVALUATING-ASYNC with a top-level capability
/// installed. Used for immediate execution and when a deferred module's
/// pending async dependencies settle.
fn execute_module_body(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
) -> Result<(), JsError> {
    // An errored cycle may already have settled this module (the rejection
    // propagates before its deferred body runs); the body is a no-op then.
    if module.evaluation_error.borrow().is_some() {
        return Ok(());
    }
    let env = (*module.environment.borrow())
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "module is not linked".into()))?;
    let stmts = module_statements(module);
    let context = ExecutionContext {
        function: None,
        realm: module.realm,
        script_or_module: Some(ScriptOrModule::Module(*module)),
        lexical_environment: env,
        variable_environment: env,
        private_environment: None,
        source: Some(module.source.clone()),
        annex_b_hoistable: Default::default(),
    };
    agent.execution_context_stack.push(context.clone());
    let strict = true;
    let body = crate::ir::compile_statements(&stmts, strict, false)?;
    let (promise, resolve, reject) = {
        let capability = module
            .top_level_capability
            .borrow()
            .clone()
            .ok_or_else(|| {
                JsError::new(
                    ErrorKind::TypeError,
                    "module body executed without a capability".into(),
                )
            })?;
        (capability.promise, capability.resolve, capability.reject)
    };
    let state = Rc::new(RefCell::new(AsyncFunctionState {
        vm: Vm::new(env, strict),
        body: Rc::new(body),
        context,
        promise,
        resolve,
        reject,
        module: Some(*module),
    }));
    let mut state_ref = state.borrow_mut();
    let body = state_ref.body.clone();
    let outcome = state_ref.vm.start(agent, &body);
    drop(state_ref);
    match outcome {
        Ok(VmOutcome::Completed(completion)) => {
            agent.execution_context_stack.pop();
            // The module body's `using` resources are disposed at completion
            // in reverse registration order, like an async body (spec 9.4.3
            // + 16.2.2.5): drain and dispose before settling the capability.
            let resources = state.borrow().vm.lexical_env.drain_disposable_resources();
            if resources.is_empty() {
                finish_module_evaluation(agent, module, &state, completion)?;
            } else {
                crate::builtins::disposable::dispose_async_body_resources(
                    agent,
                    resources,
                    completion,
                    crate::builtins::disposable::AsyncBodySettlement::Module {
                        module: *module,
                        state: state.clone(),
                    },
                )?;
            }
        }
        // `run_inner`'s driver consumes tail calls internally (modules cannot
        // contain return statements); an escaped one is an internal invariant
        // violation.
        Ok(VmOutcome::TailCall(_)) => {
            agent.execution_context_stack.pop();
            return Err(JsError::new(
                ErrorKind::TypeError,
                "tail call escaped the module driver".into(),
            ));
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
            // A synchronous engine error (an unresolved identifier, a failed
            // coercion, ...) carries its thrown value so runtime-phase
            // negative expectations can check the error constructor (the
            // explicit-throw path rejects the capability with the value).
            let value = crate::promise::error_value(agent, &error);
            return Err(error.with_value(value));
        }
    }
    Ok(())
}

/// The tail of an async module evaluation: settle the top-level capability,
/// then propagate the completion to the async parents (spec 16.2.2.5
/// AsyncModuleExecutionFulfilled/Rejected).
pub(crate) fn finish_module_evaluation(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
    state: &Rc<RefCell<AsyncFunctionState>>,
    completion: Completion,
) -> Result<(), JsError> {
    let (resolve, reject) = {
        let state = state.borrow();
        (state.resolve.clone(), state.reject.clone())
    };
    // An errored cycle's rejection may already have settled this module (a
    // deferred body is a no-op after the propagation); the later completion
    // must not re-settle it.
    if module.evaluation_error.borrow().is_some() {
        return Ok(());
    }
    module.status.replace(ModuleStatus::Evaluated);
    match completion {
        Completion::Return(_) | Completion::Normal(_) => {
            // AsyncModuleExecutionFulfilled (spec 16.2.2.5 step 2): the
            // capability resolves with *undefined* — never with the body's
            // completion value, which could be a pending promise (e.g. the
            // value of a trailing dynamic-import statement) and deadlock.
            crate::function::call(agent, &resolve, Value::Undefined, &[Value::Undefined])?;
            notify_async_parents_fulfilled(agent, module)?;
        }
        Completion::Empty => {
            crate::function::call(agent, &resolve, Value::Undefined, &[Value::Undefined])?;
            notify_async_parents_fulfilled(agent, module)?;
        }
        Completion::Throw(value) => {
            module.evaluation_error.replace(Some(value.clone()));
            crate::function::call(
                agent,
                &reject,
                Value::Undefined,
                std::slice::from_ref(&value),
            )?;
            propagate_module_error(agent, module, value)?;
        }
        Completion::Break { .. } | Completion::Continue { .. } => {
            let error = Value::String(Handle::new(JsString::from_utf8(
                "Illegal control flow in a module body",
            )));
            module.evaluation_error.replace(Some(error.clone()));
            crate::function::call(
                agent,
                &reject,
                Value::Undefined,
                std::slice::from_ref(&error),
            )?;
            propagate_module_error(agent, module, error)?;
        }
    }
    Ok(())
}

/// AsyncModuleExecutionRejected (spec 16.2.2.5): record the error on every
/// still-pending async parent and reject its capability with the same error,
/// chaining up the parent tree.
fn propagate_module_error(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
    error: Value,
) -> Result<(), JsError> {
    let parents = std::mem::take(&mut *module.async_parents.borrow_mut());
    for parent in parents {
        if *parent.status.borrow() != ModuleStatus::EvaluatingAsync {
            continue;
        }
        parent.status.replace(ModuleStatus::Evaluated);
        parent.evaluation_error.replace(Some(error.clone()));
        let reject = parent
            .top_level_capability
            .borrow()
            .as_ref()
            .map(|c| c.reject.clone());
        if let Some(reject) = reject {
            crate::function::call(
                agent,
                &reject,
                Value::Undefined,
                std::slice::from_ref(&error),
            )?;
        }
        propagate_module_error(agent, &parent, error.clone())?;
    }
    Ok(())
}

/// AsyncModuleExecutionFulfilled (spec 16.2.2.5): notify the async parents;
/// a parent whose pending count reaches zero runs its deferred body.
fn notify_async_parents_fulfilled(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
) -> Result<(), JsError> {
    let parents = std::mem::take(&mut *module.async_parents.borrow_mut());
    for parent in parents {
        if *parent.status.borrow() != ModuleStatus::EvaluatingAsync {
            continue;
        }
        let pending = {
            let mut pending = parent.pending_async.borrow_mut();
            *pending = pending.saturating_sub(1);
            *pending
        };
        if pending == 0 {
            // Defer the parent's body through the job queue: the parents of
            // a fulfilled module run breadth-first, so a parent's own
            // fulfillment does not cascade into its parents' parents before
            // the remaining siblings execute (spec 16.2.2.5 — the DFS visit
            // order is preserved).
            let realm = agent.current_realm().ok();
            agent.enqueue_promise_job(realm, move |agent| {
                execute_module_body(agent, &parent)?;
                Ok(Value::Undefined)
            });
        }
    }
    Ok(())
}

/// GetModuleNamespace (spec 16.2.2.6): the module namespace exotic object.
pub fn module_namespace(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
) -> Result<Value, JsError> {
    if let Some(namespace) = module.namespace.borrow().clone() {
        return Ok(namespace);
    }
    let value = create_namespace(agent, module, false)?;
    module.namespace.replace(Some(value.clone()));
    Ok(value)
}

/// GetModuleNamespace(module, ~defer~) (import-defer): the deferred namespace
/// — the same exotic object with [[Deferred]] = true, evaluated lazily on
/// property access. Cached per module, distinct from the eager namespace.
pub fn deferred_namespace(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
) -> Result<Value, JsError> {
    if let Some(namespace) = module.deferred_namespace.borrow().clone() {
        return Ok(namespace);
    }
    let value = create_namespace(agent, module, true)?;
    module.deferred_namespace.replace(Some(value.clone()));
    Ok(value)
}

/// `import.defer(...)` (import-defer): the DeferredModule object returned by
/// the dynamic form — an ordinary object whose `.then` method resolves with
/// the module's deferred namespace after its asynchronous transitive
/// dependencies settle. The module itself is not evaluated.
pub(crate) fn deferred_module_object(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
) -> Result<Value, JsError> {
    let realm = agent.current_realm()?;
    let prototype = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| value.as_object());
    let object = JsObject::ordinary_object_create(prototype);
    let then = Function::create_builtin(
        Some(JsString::from_utf8("then")),
        2,
        Box::new(|_, _| {
            Err(JsError::new(
                ErrorKind::TypeError,
                "DeferredModule.then must be called through the agent".into(),
            ))
        }),
        None,
        None,
    )?;
    agent.deferred_module_thens.insert(then.id(), *module);
    object.create_data_property_or_throw(&JsString::from_utf8("then"), Value::Function(then))?;
    Ok(Value::Object(object))
}

/// Dispatch the DeferredModule `.then` method (import-defer): returns a
/// promise that settles with the module's deferred namespace once its
/// asynchronous transitive dependencies have evaluated (the module itself is
/// not evaluated). The user's onFulfilled/onRejected run per the thenable
/// protocol (a `then` getter on the namespace is never consulted).
pub fn dispatch_deferred_module_then(
    agent: &mut Agent,
    callee: &Value,
    _this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let ValueKind::Function(function) = callee.kind() else {
        return None;
    };
    let module = agent.deferred_module_thens.get(&function.id()).cloned()?;
    let on_fulfilled = args.first().cloned().unwrap_or(Value::Undefined);
    let on_rejected = args.get(1).cloned().unwrap_or(Value::Undefined);
    Some(deferred_module_then(
        agent,
        &module,
        on_fulfilled,
        on_rejected,
    ))
}

fn deferred_module_then(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
    on_fulfilled: Value,
    on_rejected: Value,
) -> Result<Value, JsError> {
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let capability = crate::promise::new_promise_capability(agent, &promise_ctor)?;
    let resolve = capability.resolve.clone();
    let reject = capability.reject.clone();
    // The load completes asynchronously (the host's FinishLoadingImportedModule
    // with phase ~defer~): gather and evaluate the module's asynchronous
    // transitive dependencies, then settle with the deferred namespace — the
    // module itself is left unevaluated for lazy access.
    let realm = agent.current_realm()?;
    let module = *module;
    agent.enqueue_generic_job(Some(realm), move |agent| {
        let result = (|| -> Result<(), JsError> {
            let async_deps = gather_async_transitive_dependencies(agent, &module, &mut Vec::new())?;
            if async_deps.is_empty() {
                return settle_deferred_then(
                    agent,
                    &module,
                    &on_fulfilled,
                    &on_rejected,
                    &resolve,
                    &reject,
                );
            }
            // Wait for every async dependency's evaluation promise before
            // settling (spec FinishLoadingImportedModule step 4: Perform
            // PromiseAll over the evaluation promises).
            let remaining = Rc::new(RefCell::new(async_deps.len() as u32));
            let wait_id = NEXT_DEFERRED_WAIT.fetch_add(1, Ordering::Relaxed);
            agent.deferred_module_waits.insert(
                wait_id,
                DeferredWait(
                    remaining.clone(),
                    on_fulfilled.clone(),
                    on_rejected.clone(),
                    module,
                    resolve.clone(),
                    reject.clone(),
                ),
            );
            let fulfill = make_deferred_waiter(agent, wait_id, false)?;
            let on_rejected_wait = make_deferred_waiter(agent, wait_id, true)?;
            for dep in async_deps {
                let evaluation = module_evaluation(agent, &dep)?;
                // SafePerformPromiseAll (spec 16.2.1.6.2.1): the evaluation
                // promises are aggregated with PerformPromiseThen, which
                // attaches reactions directly — a patched
                // `Promise.prototype.then` is never consulted.
                crate::promise::perform_promise_then(
                    agent,
                    &evaluation,
                    Some(fulfill.clone()),
                    Some(on_rejected_wait.clone()),
                    None,
                )?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            let rejection = crate::promise::error_value(agent, &error);
            crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
        }
        Ok(Value::Undefined)
    });
    Ok(capability.promise)
}

/// Settle the DeferredModule `.then` promise: create the deferred namespace,
/// run the user's onFulfilled (per the thenable protocol), and resolve the
/// returned promise with its result.
fn settle_deferred_then(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
    on_fulfilled: &Value,
    on_rejected: &Value,
    resolve: &Value,
    reject: &Value,
) -> Result<(), JsError> {
    let namespace = deferred_namespace(agent, module)?;
    let result = if crux::value::is_callable(on_fulfilled) {
        crate::function::call(
            agent,
            on_fulfilled,
            Value::Undefined,
            std::slice::from_ref(&namespace),
        )
    } else {
        Ok(namespace.clone())
    };
    match result {
        Ok(value) => crate::function::call(agent, resolve, Value::Undefined, &[value]).map(|_| ()),
        Err(error) => {
            if crux::value::is_callable(on_rejected) {
                let rejection = crate::promise::error_value(agent, &error);
                crate::function::call(agent, on_rejected, Value::Undefined, &[rejection])?;
            }
            crate::function::call(agent, reject, Value::Undefined, &[namespace]).map(|_| ())
        }
    }
}

static NEXT_DEFERRED_WAIT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn make_deferred_waiter(
    agent: &mut Agent,
    wait_id: u64,
    is_reject: bool,
) -> Result<Value, JsError> {
    let closure = Function::create_builtin(
        Some(JsString::from_utf8("")),
        1,
        Box::new(|_, _| {
            Err(JsError::new(
                ErrorKind::TypeError,
                "deferred waiter must be called through the agent".into(),
            ))
        }),
        None,
        None,
    )?;
    agent
        .deferred_module_waiter_fns
        .insert(closure.id(), (wait_id, is_reject));
    Ok(Value::Function(closure))
}

/// Dispatch the DeferredModule wait continuations (import-defer): the
/// countdown closures attached to each async dependency's evaluation promise.
pub fn dispatch_deferred_module_wait(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let ValueKind::Function(function) = callee.kind() else {
        return None;
    };
    let (wait_id, is_reject) = agent
        .deferred_module_waiter_fns
        .get(&function.id())
        .copied()?;
    Some((|| -> Result<Value, JsError> {
        let Some(DeferredWait(remaining, on_fulfilled, on_rejected, module, resolve, reject)) =
            agent.deferred_module_waits.get(&wait_id).cloned()
        else {
            return Ok(Value::Undefined);
        };
        if is_reject {
            // The rejection reason is the awaited dependency's error.
            let reason = args.first().cloned().unwrap_or(Value::Undefined);
            if crux::value::is_callable(&on_rejected) {
                crate::function::call(
                    agent,
                    &on_rejected,
                    Value::Undefined,
                    std::slice::from_ref(&reason),
                )?;
            }
            crate::function::call(agent, &reject, Value::Undefined, &[reason])?;
            *remaining.borrow_mut() = 0;
            return Ok(Value::Undefined);
        }
        let mut remaining = remaining.borrow_mut();
        *remaining = remaining.saturating_sub(1);
        if *remaining == 0 {
            // All async dependencies settled; settle with the deferred
            // namespace (created lazily — the module itself is not evaluated
            // yet).
            settle_deferred_then(
                agent,
                &module,
                &on_fulfilled,
                &on_rejected,
                &resolve,
                &reject,
            )?;
        }
        Ok(Value::Undefined)
    })())
}

/// ModuleNamespaceCreate (spec 10.4.6.2): the shared namespace construction.
fn create_namespace(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
    deferred: bool,
) -> Result<Value, JsError> {
    // GetExportedNames (spec 16.2.1.7.1.1): local + indirect names, then the
    // names of every star export, with a cycle guard so a self- or mutually
    // star-exporting module (`export * from` itself) terminates. The namespace
    // object is created only after the full list is known — the guard must
    // not rely on the cached namespace, which is set at the end.
    let mut names: Vec<JsString> = Vec::new();
    let mut stack: Vec<Handle<SourceTextModule>> = Vec::new();
    collect_exported_names(agent, module, &mut stack, &mut names)?;
    // GetModuleNamespace (spec 10.4.6.6 step 4): only names that resolve
    // unambiguously appear on the namespace. A name that does not resolve
    // (or resolves ambiguously through multiple star exports) is omitted.
    names.retain(|name| {
        let mut resolve_set = Vec::new();
        resolve_export(agent, module, name, &mut resolve_set).is_ok_and(|r| r.is_some())
    });
    names.sort_by(|a, b| a.as_slice().cmp(b.as_slice()));
    let exports: Vec<PropertyKey> = names
        .into_iter()
        .map(|name| PropertyKey::from_js_string(&name))
        .collect();
    let namespace = JsObject::module_namespace_object_create(exports, deferred)?;
    let namespace_value = Value::Object(namespace);
    if deferred {
        agent.deferred_namespaces.insert(namespace.id(), *module);
    } else {
        agent.module_namespaces.insert(namespace.id(), *module);
    }
    Ok(namespace_value)
}

/// Whether the object is a deferred module namespace. The runtime dispatches
/// the import-defer evaluation trigger here; crux itself cannot reach the
/// agent.
pub fn deferred_namespace_module(
    agent: &Agent,
    obj: &JsObject,
) -> Option<Handle<SourceTextModule>> {
    if !matches!(
        obj.kind,
        crux::object::ObjectKind::ModuleNamespace(ref slots) if slots.deferred
    ) {
        return None;
    }
    agent.deferred_namespaces.get(&obj.id()).cloned()
}

/// EnsureDeferredNamespaceEvaluation, keyed by the accessed property
/// (IsSymbolLikeNamespaceKey, import-defer): symbols and `then` bypass the
/// evaluation trigger; other keys force it.
pub fn ensure_deferred_namespace_evaluation_key(
    agent: &mut Agent,
    obj: &JsObject,
    key: &crux::property::PropertyKey,
) -> Result<(), JsError> {
    match key {
        crux::property::PropertyKey::Symbol(_) => return Ok(()),
        crux::property::PropertyKey::String(id) => {
            if crux::lookup(*id).to_string_lossy() == "then" {
                return Ok(());
            }
        }
    }
    ensure_deferred_namespace_evaluation(agent, obj)
}

/// [[HasProperty]] with the import-defer evaluation trigger (spec 10.4.6.4):
/// walks the prototype chain from `obj`, dispatching
/// EnsureDeferredNamespaceEvaluation when a deferred namespace is reached, so
/// `key in obj` triggers even when the namespace is only in the chain. A
/// proxy or typed-array base intercepts the walk before the chain is
/// consulted and delegates to the crux [[HasProperty]] (the `has` trap / the
/// integer-indexed interception).
pub fn has_property_with_deferred_trigger(
    agent: &mut Agent,
    object: &crux::object::JsObject,
    key: &crux::property::PropertyKey,
) -> Result<bool, JsError> {
    let mut prototype: Option<crux::handle::Handle<crux::object::JsObject>> = None;
    loop {
        let obj = match &prototype {
            None => object,
            Some(handle) => handle,
        };
        // A proxy's [[HasProperty]] runs its `has` trap (or forwards to the
        // target); the typed-array exotic intercepts canonical index keys.
        // Delegate the rest of the walk to the crux HasProperty, which
        // handles both and continues through the chain.
        if matches!(
            obj.kind,
            crux::object::ObjectKind::Proxy(_) | crux::object::ObjectKind::IntegerIndexed(_)
        ) {
            return obj.has_property_key(key);
        }
        crate::module::ensure_deferred_namespace_evaluation_key(agent, obj, key)?;
        if obj.has_own_property_key(key)? {
            return Ok(true);
        }
        match obj.get_prototype_of()? {
            Some(proto) => prototype = Some(proto),
            None => return Ok(false),
        }
    }
}

/// EnsureDeferredNamespaceEvaluation / GetModuleExportsList (import-defer):
/// accessing a deferred namespace's exports evaluates the module
/// synchronously — unless it is already evaluated, or is not ready for
/// synchronous execution (mid-evaluation, top-level await, or an async
/// dependency), in which case a TypeError is thrown.
pub fn ensure_deferred_namespace_evaluation(
    agent: &mut Agent,
    obj: &JsObject,
) -> Result<(), JsError> {
    let Some(module) = deferred_namespace_module(agent, obj) else {
        return Ok(());
    };
    // EvaluateSync (import-defer): a module whose evaluation already rejected
    // throws the recorded error on every export access — an errored module is
    // evaluated for the purposes of this check.
    let recorded = module.evaluation_error.borrow().clone().or_else(|| {
        module
            .cycle_root
            .borrow()
            .as_ref()
            .and_then(|root| root.evaluation_error.borrow().clone())
    });
    if let Some(error) = recorded {
        return Err(
            JsError::new(ErrorKind::TypeError, "module evaluation failed".into()).with_value(error),
        );
    }
    let status = *module.status.borrow();
    match status {
        ModuleStatus::Evaluated => Ok(()),
        ModuleStatus::Evaluating | ModuleStatus::EvaluatingAsync => Err(JsError::new(
            ErrorKind::TypeError,
            "module is not ready for synchronous evaluation".into(),
        )),
        _ => {
            // ReadyForSyncExecution: a top-level-await module, or one with an
            // async transitive dependency, cannot be evaluated synchronously.
            if module_has_tla(agent, &module)?
                || !module_sync_ready(agent, &module, &mut Vec::new())?
            {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "module is not ready for synchronous evaluation".into(),
                ));
            }
            // EvaluateSync: the DFS evaluation completes synchronously for a
            // sync module; a rejecting evaluation throws its result.
            module_evaluation(agent, &module)?;
            let recorded = module.evaluation_error.borrow().clone().or_else(|| {
                module
                    .cycle_root
                    .borrow()
                    .as_ref()
                    .and_then(|root| root.evaluation_error.borrow().clone())
            });
            if let Some(error) = recorded {
                return Err(
                    JsError::new(ErrorKind::TypeError, "module evaluation failed".into())
                        .with_value(error),
                );
            }
            Ok(())
        }
    }
}

/// ReadyForSyncExecution (import-defer): the module and its (transitive)
/// dependencies are all either evaluated or synchronously evaluable.
fn module_sync_ready(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
    seen: &mut Vec<Handle<SourceTextModule>>,
) -> Result<bool, JsError> {
    if seen.iter().any(|m| Handle::ptr_eq(*m, *module)) {
        return Ok(true);
    }
    seen.push(*module);
    // ReadyForSyncExecution step 5: a fully-evaluated SCC is ready, even
    // when individual members' statuses are EVALUATED (spec
    // 16.2.1.5.2.1). A member of a cycle whose root is still evaluating is
    // not.
    if is_module_scc_evaluated(module) {
        return Ok(true);
    }
    let status = *module.status.borrow();
    match status {
        ModuleStatus::Evaluating | ModuleStatus::EvaluatingAsync => return Ok(false),
        _ => {}
    }
    if module_has_tla(agent, module)? {
        return Ok(false);
    }
    let requested = module.requested_modules.clone();
    for (specifier, _, _) in &requested {
        let imported = host_resolve_imported_module(agent, specifier, &[])?;
        if !module_sync_ready(agent, &imported, seen)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// [[HasTLA]] (spec 16.2.1.5.1): whether the module's top-level code contains
/// a top-level `await` (outside any function body).
fn module_has_tla(agent: &Agent, module: &Handle<SourceTextModule>) -> Result<bool, JsError> {
    let _ = agent;
    let stmts = module_statements(module);
    Ok(stmts.iter().any(stmt_has_top_level_await))
}

fn stmt_has_top_level_await(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Block(block) => block.stmts.iter().any(stmt_has_top_level_await),
        StmtKind::Expr(expr) | StmtKind::Throw(expr) => expr_has_top_level_await(expr),
        StmtKind::Return(Some(expr)) => expr_has_top_level_await(expr),
        StmtKind::If {
            test,
            consequent,
            alternate,
        } => {
            expr_has_top_level_await(test)
                || stmt_has_top_level_await(consequent)
                || alternate.as_deref().is_some_and(stmt_has_top_level_await)
        }
        StmtKind::VarDecl { decls, .. } | StmtKind::UsingDecl { decls, .. } => decls
            .iter()
            .any(|decl| decl.init.as_ref().is_some_and(expr_has_top_level_await)),
        StmtKind::Labeled { body, .. } => stmt_has_top_level_await(body),
        StmtKind::While { test, body } => {
            expr_has_top_level_await(test) || stmt_has_top_level_await(body)
        }
        StmtKind::DoWhile { body, test } => {
            stmt_has_top_level_await(body) || expr_has_top_level_await(test)
        }
        StmtKind::For {
            init,
            test,
            update,
            body,
        } => {
            init.as_ref().is_some_and(for_init_has_top_level_await)
                || test.as_ref().is_some_and(expr_has_top_level_await)
                || update.as_ref().is_some_and(expr_has_top_level_await)
                || stmt_has_top_level_await(body)
        }
        StmtKind::ForIn {
            left, right, body, ..
        }
        | StmtKind::ForOf {
            left, right, body, ..
        } => {
            for_binding_has_top_level_await(left)
                || expr_has_top_level_await(right)
                || stmt_has_top_level_await(body)
        }
        StmtKind::Try {
            block,
            handler,
            finalizer,
        } => {
            block.stmts.iter().any(stmt_has_top_level_await)
                || handler
                    .as_ref()
                    .is_some_and(|h| h.body.stmts.iter().any(stmt_has_top_level_await))
                || finalizer
                    .as_ref()
                    .is_some_and(|f| f.stmts.iter().any(stmt_has_top_level_await))
        }
        StmtKind::Switch {
            discriminant,
            cases,
        } => {
            expr_has_top_level_await(discriminant)
                || cases.iter().any(|case| {
                    case.test.as_ref().is_some_and(expr_has_top_level_await)
                        || case.consequent.iter().any(stmt_has_top_level_await)
                })
        }
        StmtKind::With { object, body } => {
            expr_has_top_level_await(object) || stmt_has_top_level_await(body)
        }
        _ => false,
    }
}

fn for_init_has_top_level_await(init: &ForInit) -> bool {
    match init {
        ForInit::VarDecl { decls, .. } => decls
            .iter()
            .any(|decl| decl.init.as_ref().is_some_and(expr_has_top_level_await)),
        ForInit::Expr(expr) => expr_has_top_level_await(expr),
    }
}

fn for_binding_has_top_level_await(binding: &ForBinding) -> bool {
    match binding {
        ForBinding::Expr(expr) => expr_has_top_level_await(expr),
        ForBinding::VarDecl { init, pattern, .. } => {
            init.as_ref().is_some_and(expr_has_top_level_await)
                || pattern_element_has_await(pattern)
        }
    }
}

fn pattern_element_has_await(element: &BindingPattern) -> bool {
    match element {
        BindingPattern::Ident(_) => false,
        BindingPattern::Object(props) => props.iter().any(|prop| match prop {
            syntax::ast::ObjectBindingProperty::Property { key, element, .. } => {
                matches!(key, PropertyName::Computed(e) if expr_has_top_level_await(e))
                    || element.init.as_ref().is_some_and(expr_has_top_level_await)
                    || pattern_element_has_await(&element.pattern)
            }
            syntax::ast::ObjectBindingProperty::Rest(element) => {
                element.init.as_ref().is_some_and(expr_has_top_level_await)
                    || pattern_element_has_await(&element.pattern)
            }
        }),
        BindingPattern::Array(elements) => elements.iter().any(|element| match element {
            syntax::ast::ArrayBindingElement::Element(element)
            | syntax::ast::ArrayBindingElement::Rest(element) => {
                element.init.as_ref().is_some_and(expr_has_top_level_await)
                    || pattern_element_has_await(&element.pattern)
            }
            syntax::ast::ArrayBindingElement::Hole => false,
        }),
    }
}

fn expr_has_top_level_await(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Await(_) => true,
        ExprKind::Function(_) | ExprKind::Arrow { .. } => false,
        ExprKind::Class(class) => class_has_top_level_await(class),
        ExprKind::Unary { operand, .. } => expr_has_top_level_await(operand),
        ExprKind::Update { target, .. } => expr_has_top_level_await(target),
        ExprKind::Binary { left, right, .. } => {
            expr_has_top_level_await(left) || expr_has_top_level_await(right)
        }
        ExprKind::Logical { left, right, .. } => {
            expr_has_top_level_await(left) || expr_has_top_level_await(right)
        }
        ExprKind::Assign { target, value, .. } => {
            expr_has_top_level_await(target) || expr_has_top_level_await(value)
        }
        ExprKind::Conditional {
            test,
            consequent,
            alternate,
        } => {
            expr_has_top_level_await(test)
                || expr_has_top_level_await(consequent)
                || expr_has_top_level_await(alternate)
        }
        ExprKind::PrivateIn { object, .. } => expr_has_top_level_await(object),
        ExprKind::Call(call) => {
            expr_has_top_level_await(&call.callee)
                || call.args.iter().any(|arg| match arg {
                    Argument::Expr(expr) => expr_has_top_level_await(expr),
                    Argument::Spread(expr) => expr_has_top_level_await(expr),
                })
        }
        ExprKind::New(new) => {
            expr_has_top_level_await(&new.callee)
                || new.args.iter().any(|arg| match arg {
                    Argument::Expr(expr) => expr_has_top_level_await(expr),
                    Argument::Spread(expr) => expr_has_top_level_await(expr),
                })
        }
        ExprKind::Member(member) => {
            expr_has_top_level_await(&member.object)
                || matches!(&member.property, MemberProperty::Computed(e) if expr_has_top_level_await(e))
        }
        ExprKind::TaggedTemplate { tag, quasi } => {
            expr_has_top_level_await(tag) || quasi.exprs.iter().any(expr_has_top_level_await)
        }
        ExprKind::Template(template) => template.exprs.iter().any(expr_has_top_level_await),
        ExprKind::Paren(inner) => expr_has_top_level_await(inner),
        ExprKind::Sequence(exprs) => exprs.iter().any(expr_has_top_level_await),
        ExprKind::Array(literal) => literal.elements.iter().any(|element| match element {
            ArrayElement::Expr(expr) | ArrayElement::Spread(expr) => expr_has_top_level_await(expr),
            ArrayElement::Hole => false,
        }),
        ExprKind::Object(literal) => literal.props.iter().any(|prop| match prop {
            ObjectProperty::Init { key, value, .. } => {
                property_name_has_top_level_await(key) || expr_has_top_level_await(value)
            }
            ObjectProperty::Method { key, .. } => property_name_has_top_level_await(key),
            ObjectProperty::Get { key, body, .. } | ObjectProperty::Set { key, body, .. } => {
                property_name_has_top_level_await(key)
                    || body.stmts.iter().any(stmt_has_top_level_await)
            }
            ObjectProperty::Spread(expr) => expr_has_top_level_await(expr),
        }),
        ExprKind::ImportCall {
            specifier, options, ..
        } => {
            expr_has_top_level_await(specifier)
                || options.as_deref().is_some_and(expr_has_top_level_await)
        }
        ExprKind::MetaProperty { .. }
        | ExprKind::Ident(_)
        | ExprKind::This
        | ExprKind::Literal(_)
        | ExprKind::Yield { .. }
        | ExprKind::Super => false,
    }
}

fn property_name_has_top_level_await(key: &PropertyName) -> bool {
    match key {
        PropertyName::Computed(expr) => expr_has_top_level_await(expr),
        _ => false,
    }
}

fn class_has_top_level_await(class: &Class) -> bool {
    class
        .heritage
        .as_ref()
        .is_some_and(expr_has_top_level_await)
        || class.elements.iter().any(|element| {
            let name = match element {
                ClassElement::Method { name, .. }
                | ClassElement::Get { name, .. }
                | ClassElement::Set { name, .. }
                | ClassElement::Field { name, .. } => name,
                ClassElement::StaticBlock(_) => return false,
            };
            matches!(
                name,
                ClassElementName::Property(PropertyName::Computed(expr))
                    if expr_has_top_level_await(expr)
            )
        })
}

/// IsModuleSCCEvaluated (import-defer, spec 16.2.1.5.4.1): whether the
/// strongly connected component containing the module is fully evaluated — a
/// module whose cycle root is still evaluating is not, even when its own
/// status is EVALUATED.
fn is_module_scc_evaluated(module: &Handle<SourceTextModule>) -> bool {
    if let Some(root) = *module.cycle_root.borrow() {
        return *root.status.borrow() == ModuleStatus::Evaluated;
    }
    *module.status.borrow() == ModuleStatus::Evaluated
}

/// GatherAsynchronousTransitiveDependencies (import-defer): the modules a
/// deferred request still forces to evaluate — the transitive closure of its
/// top-level-await modules (spec 16.2.1.5.3.1 step 12).
fn gather_async_transitive_dependencies(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
    seen: &mut Vec<Handle<SourceTextModule>>,
) -> Result<Vec<Handle<SourceTextModule>>, JsError> {
    let mut result = Vec::new();
    if seen.iter().any(|m| Handle::ptr_eq(*m, *module)) {
        return Ok(result);
    }
    seen.push(*module);
    // spec step 6: an evaluating module — or one whose SCC is already fully
    // evaluated — contributes nothing. A member of a cycle whose root is
    // still evaluating is NOT SCC-evaluated: the walk continues into it so
    // the root's top-level await is gathered.
    if *module.status.borrow() == ModuleStatus::Evaluating {
        return Ok(result);
    }
    if is_module_scc_evaluated(module) {
        return Ok(result);
    }
    if module_has_tla(agent, module)? {
        result.push(*module);
        return Ok(result);
    }
    let requested = module.requested_modules.clone();
    for (specifier, _, _) in &requested {
        let imported = host_resolve_imported_module(agent, specifier, &[])?;
        let additional = gather_async_transitive_dependencies(agent, &imported, seen)?;
        for m in additional {
            if !result.iter().any(|existing| Handle::ptr_eq(*existing, m)) {
                result.push(m);
            }
        }
    }
    Ok(result)
}

/// GetExportedNames (spec 16.2.1.7.1.1): the module's export names, including
/// those re-exported through `export *`. A module already on `stack` is a
/// star-export cycle; its own names were collected by the enclosing frame, so
/// it contributes nothing further.
fn collect_exported_names(
    agent: &mut Agent,
    module: &Handle<SourceTextModule>,
    stack: &mut Vec<Handle<SourceTextModule>>,
    out: &mut Vec<JsString>,
) -> Result<(), JsError> {
    if stack.iter().any(|m| Handle::ptr_eq(*m, *module)) {
        return Ok(());
    }
    stack.push(*module);
    for export in &module.local_export_entries {
        if let Ok(name) = export_name_string(export.export_name.as_ref())
            && !out.contains(&name)
        {
            out.push(name);
        }
    }
    for export in &module.indirect_export_entries {
        if let Ok(name) = export_name_string(export.export_name.as_ref())
            && !out.contains(&name)
        {
            out.push(name);
        }
    }
    for export in &module.star_export_entries {
        let specifier = export.module_request.as_ref().ok_or_else(|| {
            JsError::new(ErrorKind::TypeError, "star export has no module".into())
        })?;
        let imported = host_resolve_imported_module(agent, specifier, &[])?;
        collect_exported_names(agent, &imported, stack, out)?;
    }
    stack.pop();
    Ok(())
}

/// A resolved export binding (spec 16.2.1.7.2.2 ResolveExport).
enum ResolvedBinding {
    /// The binding lives in `module`'s environment under `local_name`.
    Local(Handle<SourceTextModule>, JsString),
    /// The binding is the module namespace object of `module`.
    Namespace(Handle<SourceTextModule>),
    /// The binding is the deferred namespace object of `module` (a
    /// re-exported deferred namespace import, import-defer).
    DeferredNamespace(Handle<SourceTextModule>),
    /// The binding is the ModuleSource object of `module` (a re-exported
    /// source-phase import, spec ResolveExport step: [[BindingName]] is
    /// ~source~).
    Source(Handle<SourceTextModule>),
}

/// The sentinel import name of a reclassified source-phase re-export: an
/// atom that cannot collide with a real identifier (the spec's ~source~
/// marker).
fn source_marker() -> JsString {
    JsString::from_utf8("\u{1}source")
}

/// The sentinel import name of a reclassified deferred-namespace re-export
/// (`import defer * as ns from …; export { ns }`): resolves to the module's
/// deferred namespace object (import-defer).
fn defer_marker() -> JsString {
    JsString::from_utf8("\u{1}defer")
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
        .any(|(m, n)| Handle::ptr_eq(*m, *module) && n == name)
    {
        return Ok(None);
    }
    resolve_set.push((*module, name.clone()));
    for export in &module.local_export_entries {
        if export_name_string(export.export_name.as_ref())
            .ok()
            .as_ref()
            == Some(name)
        {
            if let Some(local) = export.local_name {
                return Ok(Some(ResolvedBinding::Local(*module, crux::lookup(local))));
            }
            // `export * as ns from ...`: the binding is a namespace.
            let specifier = export.module_request.as_ref().ok_or_else(|| {
                JsError::new(ErrorKind::TypeError, "local export has no module".into())
            })?;
            let imported = host_resolve_imported_module(agent, specifier, &[])?;
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
            let imported = host_resolve_imported_module(agent, specifier, &[])?;
            // An `import * as ns; export { ns }` re-export has no import name:
            // the binding is the imported module's namespace itself (spec
            // 16.2.1.7.1 step 10.1.ii.2.b).
            let Some(import_name) = export.import_name.as_ref() else {
                return Ok(Some(ResolvedBinding::Namespace(imported)));
            };
            let import_name = export_name_string(Some(import_name))?;
            // A reclassified source-phase re-export (`import source x from …;
            // export { x }`) resolves to the imported module's ModuleSource
            // object (spec ResolveExport step: [[ImportName]] is ~source~).
            if import_name == source_marker() {
                return Ok(Some(ResolvedBinding::Source(imported)));
            }
            // A reclassified deferred-namespace re-export resolves to the
            // module's deferred namespace object.
            if import_name == defer_marker() {
                return Ok(Some(ResolvedBinding::DeferredNamespace(imported)));
            }
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
        let imported = host_resolve_imported_module(agent, specifier, &[])?;
        let Some(resolution) = resolve_export(agent, &imported, name, resolve_set)? else {
            continue;
        };
        match &star_resolution {
            None => star_resolution = Some(resolution),
            Some(previous) => {
                // Ambiguous unless both resolve to the same binding.
                let same = match (previous, &resolution) {
                    (ResolvedBinding::Local(pm, pn), ResolvedBinding::Local(m, n)) => {
                        Handle::ptr_eq(*pm, *m) && pn == n
                    }
                    (ResolvedBinding::Namespace(pm), ResolvedBinding::Namespace(m)) => {
                        Handle::ptr_eq(*pm, *m)
                    }
                    (
                        ResolvedBinding::DeferredNamespace(pm),
                        ResolvedBinding::DeferredNamespace(m),
                    ) => Handle::ptr_eq(*pm, *m),
                    (ResolvedBinding::Source(pm), ResolvedBinding::Source(m)) => {
                        Handle::ptr_eq(*pm, *m)
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
            let env = target
                .environment
                .borrow()
                .as_ref()
                .copied()
                .ok_or_else(|| JsError::new(ErrorKind::TypeError, "module is not linked".into()))?;
            env.get_binding_value(&local, true)
        }
        Some(ResolvedBinding::Namespace(target)) => module_namespace(agent, &target),
        Some(ResolvedBinding::DeferredNamespace(target)) => deferred_namespace(agent, &target),
        Some(ResolvedBinding::Source(target)) => module_source_object(agent, &target),
        None => Ok(Value::Undefined),
    }
}

/// `import()` (spec 16.6.1.4): load, link, evaluate, and resolve with the
/// module namespace. `phase` selects the `import.source(...)` (source phase)
/// and `import.defer(...)` (deferred phase) forms of the ImportCall.
pub fn dynamic_import(
    agent: &mut Agent,
    specifier: &Value,
    options: Option<&Value>,
    phase: ImportPhase,
) -> Result<Value, JsError> {
    let promise_ctor = agent
        .current_realm()?
        .intrinsics
        .get("%Promise%")
        .unwrap_or(Value::Undefined);
    let capability = crate::promise::new_promise_capability(agent, &promise_ctor)?;
    let resolve = capability.resolve.clone();
    let reject = capability.reject.clone();
    // ToString(specifier) abrupts reject the capability (spec 13.3.10.1
    // steps 6-7) — the import expression must not throw synchronously.
    let specifier_text = match crux::convert::to_string(specifier) {
        Ok(text) => text,
        Err(error) => {
            let rejection = crate::promise::error_value(agent, &error);
            crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
            return Ok(capability.promise);
        }
    };
    // Import attributes: `type: "json" | "text" | "bytes" | "js"` selects
    // the module kind; *undefined* options (including the evaluation of an
    // options expression that yields undefined) skip the attribute
    // validation entirely (spec 13.3.10.2 step 4: "If options is not
    // undefined").
    let mut parsed_attributes: Vec<(AttributeKey, JsString)> = Vec::new();
    if let Some(options) = options
        && !matches!(options.kind(), ValueKind::Undefined)
    {
        let attributes = match import_attributes(agent, options) {
            Ok(attributes) => attributes,
            Err(error) => {
                let rejection = crate::promise::error_value(agent, &error);
                crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
                return Ok(capability.promise);
            }
        };
        for (key, value) in attributes {
            let key_text = key.to_string_lossy();
            let value_text = value.to_string_lossy();
            if key_text != "type"
                || !matches!(value_text.as_str(), "json" | "text" | "bytes" | "js")
            {
                crate::function::call(
                    agent,
                    &reject,
                    Value::Undefined,
                    &[Value::String(Handle::new(JsString::from_utf8(
                        "Unsupported import attribute",
                    )))],
                )?;
                return Ok(capability.promise);
            }
            parsed_attributes.push((AttributeKey::Str(key), value));
        }
    }
    // The load is asynchronous: the host completes it in a job, so the
    // module's evaluation never runs concurrently with the evaluation already
    // in progress (sec-moduleevaluation Evaluate step 1 asserts exactly that;
    // `verify-dfs.js` checks that a dynamic import cannot preempt the DFS
    // evaluation order). By the time this job runs, the current synchronous
    // execution — the ongoing evaluation wave — has finished, and a target
    // that is also a static dependency of the wave is already evaluated.
    let realm = agent.current_realm()?;
    agent.enqueue_generic_job(Some(realm), move |agent| {
        let result = (|| -> Result<(), JsError> {
            let module = host_resolve_imported_module(agent, &specifier_text, &parsed_attributes)?;
            match phase {
                ImportPhase::Source => {
                    // GetModuleSource of a Source Text Module Record always
                    // throws a SyntaxError (spec 16.2.1.7.2): a source-phase
                    // import of a JavaScript module is unavailable. Synthetic
                    // modules (JSON/text/bytes) expose their source text.
                    if module_kind_of(&module) == ModuleKind::Js {
                        let error = JsError::new(
                            ErrorKind::SyntaxError,
                            "source phase import is not available for this module".into(),
                        );
                        let rejection = crate::promise::error_value(agent, &error);
                        crate::function::call(agent, &reject, Value::Undefined, &[rejection])?;
                    } else {
                        let source = module_source_text(agent, &module)?;
                        crate::function::call(agent, &resolve, Value::Undefined, &[source])?;
                    }
                    return Ok(());
                }
                ImportPhase::Defer => {
                    // `import.defer(...)` returns a DeferredModule object (not
                    // a promise): a thenable whose `.then` resolves with the
                    // module's deferred namespace after its asynchronous
                    // transitive dependencies settle — the module itself is
                    // not evaluated until the namespace is accessed.
                    module_declaration_instantiation(agent, &module)?;
                    let deferred_module = deferred_module_object(agent, &module)?;
                    crate::function::call(agent, &resolve, Value::Undefined, &[deferred_module])?;
                    return Ok(());
                }
                ImportPhase::Import => {}
            }
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
        Ok(Value::Undefined)
    });
    Ok(capability.promise)
}

fn is_promise_value(agent: &Agent, value: &Value) -> bool {
    let ValueKind::Object(obj) = value.kind() else {
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
    let ValueKind::Function(function) = callee.kind() else {
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
    // [[ImportMeta]] (spec 16.2.1.8): one ordinary object per module record,
    // created on first access and cached — `import.meta` is the same object
    // for every access within a module and distinct across modules.
    let context = agent.running_context()?;
    if let Some(crate::context::ScriptOrModule::Module(module)) = &context.script_or_module {
        if let Some(meta) = module.import_meta.borrow().clone() {
            return Ok(meta);
        }
        let proto = agent
            .current_realm()?
            .intrinsics
            .get("%Object.prototype%")
            .and_then(|value| crate::context::as_object(&value));
        let meta = Value::Object(JsObject::ordinary_object_create(proto));
        module.import_meta.replace(Some(meta.clone()));
        return Ok(meta);
    }
    let proto = agent
        .current_realm()?
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| crate::context::as_object(&value));
    Ok(Value::Object(JsObject::ordinary_object_create(proto)))
}

/// The `with { ... }` attributes of an import.
fn import_attributes(
    agent: &mut Agent,
    options: &Value,
) -> Result<Vec<(JsString, JsString)>, JsError> {
    // The attributes are the `with` property's own enumerable string keys
    // (spec 13.3.10.2 / import-attributes): the options object itself is the
    // envelope, not the attribute map.
    let ValueKind::Object(_) = options.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "import options must be an object".into(),
        ));
    };
    let with = crate::context::get_property(
        agent,
        options,
        &JsString::from_utf8("with"),
        options.clone(),
    )?;
    if matches!(with.kind(), ValueKind::Undefined) {
        return Ok(Vec::new());
    }
    let ValueKind::Object(with_obj) = with.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "import attributes must be an object".into(),
        ));
    };
    let mut out = Vec::new();
    for key in with_obj.own_property_keys()? {
        let PropertyKey::String(id) = key else {
            continue;
        };
        let Some(prop) = with_obj.get_own_property_key(&PropertyKey::String(id))? else {
            continue;
        };
        if !prop.enumerable {
            continue;
        }
        let name = crux::lookup(id);
        let value = with_obj.get(&name)?;
        // spec: attribute values must already be strings — no coercion.
        let ValueKind::String(text) = value.kind() else {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "import attribute value must be a string".into(),
            ));
        };
        out.push((name, text.as_ref().clone()));
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
        let module = host_resolve_imported_module(&mut agent, &JsString::from_utf8(entry), &[])?;
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
        let ValueKind::Object(obj) = value.kind() else {
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
            host_resolve_imported_module(&mut agent, &JsString::from_utf8("m.js"), &[]).unwrap();
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

    #[test]
    fn errored_async_cycle_rejects_importers_and_members() {
        // A top-level-await cycle {a, b, c} where b throws after its await:
        // the importer (main) rejects with b's error, and re-importing a
        // fulfilled cycle member (c) rejects with the same recorded error
        // (spec 16.2.2.5 Evaluate steps 2-3 / AsyncModuleExecutionRejected).
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent.add_module("./main.js", "import './b.js'; import './x.js';");
        agent.add_module(
            "./b.js",
            "import './c.js'; await Promise.resolve(0); throw new Error('async error in B');",
        );
        agent.add_module("./c.js", "import './a.js'; await Promise.resolve(0);");
        agent.add_module("./a.js", "import './b.js'; await Promise.resolve(0);");
        agent.add_module("./x.js", "import './a.js'; await Promise.resolve(0);");
        let main = host_resolve_imported_module(&mut agent, &JsString::from_utf8("./main.js"), &[])
            .unwrap();
        module_declaration_instantiation(&mut agent, &main).unwrap();
        let main_promise = module_evaluation(&mut agent, &main).unwrap();
        agent.run_jobs().unwrap();
        let main_error = match settled(&agent, &main_promise) {
            Err(_) => panic!("main promise did not reject"),
            Ok(value) => value,
        };
        let message = crate::context::get_property(
            &mut agent,
            &main_error,
            &JsString::from_utf8("message"),
            main_error.clone(),
        )
        .unwrap();
        assert_eq!(message, js_str("async error in B"));
        let c =
            host_resolve_imported_module(&mut agent, &JsString::from_utf8("./c.js"), &[]).unwrap();
        let c_promise = module_evaluation(&mut agent, &c).unwrap();
        agent.run_jobs().unwrap();
        let c_error = match settled(&agent, &c_promise) {
            Err(_) => panic!("c promise did not reject"),
            Ok(value) => value,
        };
        assert_eq!(c_error, main_error);
    }

    #[test]
    fn async_dependency_defers_the_importing_body() {
        // A module with a top-level-await dependency does not run its own
        // body until the dependency settles (spec 16.2.2.5 steps 13-14).
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent.add_module("./m.js", "import './b.js'; export var done = 1;");
        agent.add_module("./b.js", "await Promise.resolve(0); export var x = 1;");
        let m =
            host_resolve_imported_module(&mut agent, &JsString::from_utf8("./m.js"), &[]).unwrap();
        module_declaration_instantiation(&mut agent, &m).unwrap();
        let m_promise = module_evaluation(&mut agent, &m).unwrap();
        agent.run_jobs().unwrap();
        let b =
            host_resolve_imported_module(&mut agent, &JsString::from_utf8("./b.js"), &[]).unwrap();
        assert_eq!(*b.status.borrow(), ModuleStatus::Evaluated);
        assert_eq!(*m.status.borrow(), ModuleStatus::Evaluated);
        let settled = settled(&agent, &m_promise);
        assert!(matches!(settled, Ok(v) if v.is_undefined()));
    }
}
