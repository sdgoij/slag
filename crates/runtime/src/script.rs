//! Script Records (spec 16.1.4) and their algorithms: ParseScript,
//! ScriptEvaluation, and GlobalDeclarationInstantiation, plus the
//! declaration SDOs they consume (LexicallyDeclaredNames, VarDeclaredNames,
//! VarScopedDeclarations, LexicallyScopedDeclarations, BoundNames,
//! IsConstantDeclaration, ScriptIsStrict).

use std::collections::HashSet;

use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::string::{JsString, lookup};
use crux::value::Value;
use syntax::ast::{
    Argument, ArrayBindingElement, ArrayElement, ArrowBody, BindingPattern, Class, ClassElement,
    ClassElementName, Expr, ExprKind, ForBinding, ForInit, ObjectBindingProperty, ObjectProperty,
    Program, PropertyName, Stmt, StmtKind, VarDeclKind,
};

use crate::agent::Agent;
use crate::context::ExecutionContext;
use crate::env::{EnvRecord, EnvRef, new_declarative_environment};
use crate::realm::Realm;
use crux::heap::{GcAny, Trace};

/// A Script Record (spec 16.1.4): the realm the script runs in and its
/// parsed ECMAScript code. [[LoadedModules]] joins with module linking
/// (Phase 7); [[HostDefined]] is host-specific.
#[derive(Debug, Clone)]
pub struct ScriptRecord {
    pub realm: Handle<Realm>,
    pub code: Program,
    /// The exact source text, for `Function.prototype.toString`.
    pub source: JsString,
}

impl Trace for ScriptRecord {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.realm.trace(visit);
        // `code` is the parsed AST (plain data); the source JsString is owned
        // by `code`'s spans.
    }
}

/// ParseScript (spec 16.1.5): parse `source` as a Script and wrap it in a
/// Script Record. Early errors surface here as a SyntaxError.
pub fn parse_script(source: &str, realm: Handle<Realm>) -> Result<Handle<ScriptRecord>, JsError> {
    crate::expr::bump_template_parse_generation();
    let code = parser::parse_script(source)?;
    Ok(Handle::new(ScriptRecord {
        realm,
        code,
        source: JsString::from_utf8(source),
    }))
}

/// ScriptEvaluation (spec 16.1.6): establish a script execution context,
/// instantiate its declarations in the global environment, and evaluate.
pub fn script_evaluation(
    agent: &mut Agent,
    script: &Handle<ScriptRecord>,
) -> Result<Value, JsError> {
    let global_env = script.realm.global_env();
    let context = crate::context::ExecutionContext {
        function: None,
        realm: script.realm,
        script_or_module: Some(crate::context::ScriptOrModule::Script(*script)),
        lexical_environment: global_env,
        variable_environment: global_env,
        private_environment: None,
        source: Some(script.source.clone()),
        annex_b_hoistable: Default::default(),
    };
    agent.execution_context_stack.push(context);

    let strict = script_is_strict(&script.source, &script.code);
    let result = (|| -> Result<Value, JsError> {
        global_declaration_instantiation(agent, &script.code, &global_env, strict)?;
        crate::eval::eval_program(agent, &script.code, strict, true)
    })();

    agent.execution_context_stack.pop();
    result
}

// ---- declaration SDOs (spec 8.1/16.1) ----

/// The bound names of a binding pattern.
pub fn bound_names(pattern: &BindingPattern, out: &mut Vec<JsString>) {
    match pattern {
        BindingPattern::Ident(name) => out.push(lookup(*name)),
        BindingPattern::Object(props) => {
            for prop in props {
                match prop {
                    ObjectBindingProperty::Property { element, .. }
                    | ObjectBindingProperty::Rest(element) => {
                        bound_names(&element.pattern, out);
                    }
                }
            }
        }
        BindingPattern::Array(elements) => {
            for element in elements {
                match element {
                    ArrayBindingElement::Hole => {}
                    ArrayBindingElement::Element(e) | ArrayBindingElement::Rest(e) => {
                        bound_names(&e.pattern, out);
                    }
                }
            }
        }
    }
}

/// spec 8.1.3 IsConstantDeclaration: `const` declarations bind immutably.
pub fn is_constant_declaration(stmt: &Stmt) -> bool {
    matches!(
        &stmt.kind,
        StmtKind::VarDecl { kind, .. } if *kind == VarDeclKind::Const
    )
}

/// ScriptIsStrict (spec 16.1.2): a directive prologue `"use strict"`. The
/// directive must be a genuine one — raw text `use strict` between the quotes
/// (spec 14.1.1) — so `'use str\ict'` and `'use\u0020strict'` do not count.
pub fn script_is_strict(source: &JsString, program: &Program) -> bool {
    for stmt in &program.body {
        let StmtKind::Expr(expr) = &stmt.kind else {
            return false;
        };
        let syntax::ast::ExprKind::Literal(syntax::ast::Literal::Str(value)) = &expr.kind else {
            return false;
        };
        if value.to_string_lossy() != "use strict" {
            continue;
        }
        let units = source.as_slice();
        let (start, end) = (expr.span.start as usize, expr.span.end as usize);
        let inner = if start + 1 < end && end <= units.len() {
            &units[start + 1..end - 1]
        } else {
            return true;
        };
        if inner == "use strict".encode_utf16().collect::<Vec<u16>>().as_slice() {
            return true;
        }
    }
    false
}

/// TopLevelLexicallyDeclaredNames of a statement list (spec 8.1.2): the
/// names bound by direct let/const/using/class declarations. Function
/// declarations are var-scoped at top level and excluded.
pub fn top_level_lexically_declared_names(stmts: &[Stmt]) -> Vec<JsString> {
    let mut names = Vec::new();
    for stmt in stmts {
        match &stmt.kind {
            StmtKind::VarDecl { kind, decls, .. } if *kind != VarDeclKind::Var => {
                for decl in decls {
                    bound_names(&decl.pattern, &mut names);
                }
            }
            StmtKind::UsingDecl { decls, .. } => {
                for decl in decls {
                    bound_names(&decl.pattern, &mut names);
                }
            }
            StmtKind::ClassDecl(class) => {
                if let Some(name) = class.name {
                    names.push(lookup(name));
                }
            }
            _ => {}
        }
    }
    names
}

/// TopLevelLexicallyScopedDeclarations of a statement list (spec 8.1.2):
/// the direct let/const/using/class declarations (not function
/// declarations, which are var-scoped at top level).
pub fn top_level_lexically_scoped_declarations(stmts: &[Stmt]) -> Vec<&Stmt> {
    stmts
        .iter()
        .filter(|stmt| match &stmt.kind {
            StmtKind::ClassDecl(_) | StmtKind::UsingDecl { .. } => true,
            StmtKind::VarDecl { kind, .. } => *kind != VarDeclKind::Var,
            _ => false,
        })
        .collect()
}

/// One var-scoped declaration (spec 8.1.2): a function declaration or the
/// bound names of a var declaration (including `for (var …)` heads and
/// `for (var x in/of …)` bindings).
#[derive(Debug, PartialEq)]
pub enum VarScopedDecl<'a> {
    Function(&'a syntax::ast::Function),
    Variable(Vec<JsString>),
}

/// VarScopedDeclarations of a statement (spec 8.1.2): recurses through
/// block-like statements collecting `var` declarations and for-heads.
/// Function declarations are not var-scoped per the modern spec (they are
/// handled at the top level only).
fn var_scoped_declarations<'a>(stmt: &'a Stmt, out: &mut Vec<VarScopedDecl<'a>>) {
    match &stmt.kind {
        StmtKind::VarDecl { kind, decls, .. } if *kind == VarDeclKind::Var => {
            for decl in decls {
                let mut names = Vec::new();
                bound_names(&decl.pattern, &mut names);
                out.push(VarScopedDecl::Variable(names));
            }
        }
        StmtKind::Block(block) => var_scoped_declarations_in_stmts(&block.stmts, out),
        StmtKind::If {
            consequent,
            alternate,
            ..
        } => {
            var_scoped_declarations(consequent, out);
            if let Some(alt) = alternate {
                var_scoped_declarations(alt, out);
            }
        }
        StmtKind::While { body, .. } | StmtKind::DoWhile { body, .. } => {
            var_scoped_declarations(body, out)
        }
        StmtKind::For { init, body, .. } => {
            if let Some(ForInit::VarDecl { kind, decls, .. }) = init
                && *kind == VarDeclKind::Var
            {
                for decl in decls {
                    let mut names = Vec::new();
                    bound_names(&decl.pattern, &mut names);
                    out.push(VarScopedDecl::Variable(names));
                }
            }
            var_scoped_declarations(body, out);
        }
        StmtKind::ForIn { left, body, .. } | StmtKind::ForOf { left, body, .. } => {
            if let ForBinding::VarDecl { kind, pattern, .. } = left
                && *kind == VarDeclKind::Var
            {
                let mut names = Vec::new();
                bound_names(pattern, &mut names);
                out.push(VarScopedDecl::Variable(names));
            }
            var_scoped_declarations(body, out);
        }
        StmtKind::Try {
            block,
            handler,
            finalizer,
        } => {
            var_scoped_declarations_in_stmts(&block.stmts, out);
            if let Some(handler) = handler {
                var_scoped_declarations_in_stmts(&handler.body.stmts, out);
            }
            if let Some(finalizer) = finalizer {
                var_scoped_declarations_in_stmts(&finalizer.stmts, out);
            }
        }
        StmtKind::Switch { cases, .. } => {
            for case in cases {
                var_scoped_declarations_in_stmts(&case.consequent, out);
            }
        }
        StmtKind::With { body, .. } => var_scoped_declarations(body, out),
        StmtKind::Labeled { body, .. } => var_scoped_declarations(body, out),
        _ => {}
    }
}

fn var_scoped_declarations_in_stmts<'a>(stmts: &'a [Stmt], out: &mut Vec<VarScopedDecl<'a>>) {
    for stmt in stmts {
        var_scoped_declarations(stmt, out);
    }
}

/// TopLevelVarDeclaredNames of a statement list (spec 8.1.2): top-level
/// function declarations plus var declarations at any depth.
pub fn top_level_var_declared_names(stmts: &[Stmt]) -> Vec<JsString> {
    let mut names = Vec::new();
    for stmt in stmts {
        if let StmtKind::FunctionDecl(f) = &stmt.kind
            && let Some(name) = f.name
        {
            names.push(lookup(name));
        }
    }
    for decl in top_level_var_scoped_declarations(stmts) {
        if let VarScopedDecl::Variable(names_in_decl) = decl {
            names.extend(names_in_decl);
        }
    }
    names
}

/// TopLevelVarScopedDeclarations of a statement list (spec 8.1.2): the
/// top-level function declarations followed by the var declarations at any
/// depth, in source order.
pub fn top_level_var_scoped_declarations<'a>(stmts: &'a [Stmt]) -> Vec<VarScopedDecl<'a>> {
    let mut decls = Vec::new();
    for stmt in stmts {
        if let StmtKind::FunctionDecl(f) = &stmt.kind {
            decls.push(VarScopedDecl::Function(f));
        }
    }
    var_scoped_declarations_in_stmts(stmts, &mut decls);
    decls
}

// ---- Annex B block-level function declarations (B.3.2 / B.3.3) ----

/// Annex B (B.3.3.x / B.3.2.1): for each FunctionDeclaration that is not a
/// top-level StatementListItem — block-level (in a Block, switch case, or
/// try/catch/finally list) or statement-position (`if (x) function f(){}`) —
/// in source order, the name, the declaration's span, and whether its Annex B
/// var hoist is applicable. A hoist is suppressed when the name is lexically
/// bound in an enclosing scope: a let/const/class/using declaration, a
/// block-level function declaration, or a non-simple catch parameter.
/// Top-level function declarations are var-scoped and do not suppress.
pub fn annex_b_function_hoists(stmts: &[Stmt]) -> Vec<(JsString, crux::Span, bool)> {
    let mut out = Vec::new();
    let mut stack: Vec<HashSet<JsString>> = Vec::new();
    walk_annex_b_list(stmts, &mut stack, &mut out, false);
    out
}

/// `stack` holds the lexical names of every enclosing statement list.
// JsString hashes are content-stable (a rope's first hash materializes its
// flat cache, which never changes the hash output), so the annex-B name sets
// below are sound.
#[allow(clippy::mutable_key_type)]
fn walk_annex_b_list(
    stmts: &[Stmt],
    stack: &mut Vec<HashSet<JsString>>,
    out: &mut Vec<(JsString, crux::Span, bool)>,
    nested: bool,
) {
    let mut current: HashSet<JsString> = HashSet::new();
    for stmt in stmts {
        match &stmt.kind {
            StmtKind::VarDecl { kind, decls, .. } if *kind != VarDeclKind::Var => {
                for decl in decls {
                    let mut names = Vec::new();
                    bound_names(&decl.pattern, &mut names);
                    current.extend(names);
                }
            }
            StmtKind::UsingDecl { decls, .. } => {
                for decl in decls {
                    let mut names = Vec::new();
                    bound_names(&decl.pattern, &mut names);
                    current.extend(names);
                }
            }
            StmtKind::ClassDecl(class) => {
                if let Some(name) = class.name {
                    current.insert(lookup(name));
                }
            }
            _ => {}
        }
    }
    stack.push(current.clone());
    for stmt in stmts {
        match &stmt.kind {
            StmtKind::FunctionDecl(f) if !f.is_async && !f.is_generator => {
                if let Some(name) = f.name {
                    let name = lookup(name);
                    if nested {
                        let conflict = stack.iter().any(|s| s.contains(&name));
                        out.push((name.clone(), f.span, !conflict));
                        current.insert(name.clone());
                        if let Some(top) = stack.last_mut() {
                            top.insert(name);
                        }
                    }
                }
            }
            _ => walk_annex_b_stmt(stmt, stack, out, &current),
        }
    }
    stack.pop();
}

#[allow(clippy::mutable_key_type)]
fn walk_annex_b_stmt(
    stmt: &Stmt,
    stack: &mut Vec<HashSet<JsString>>,
    out: &mut Vec<(JsString, crux::Span, bool)>,
    _current: &HashSet<JsString>,
) {
    match &stmt.kind {
        StmtKind::FunctionDecl(f) if !f.is_async && !f.is_generator => {
            // Statement-position function: check all enclosing scopes.
            if let Some(name) = f.name {
                let name = lookup(name);
                let conflict = stack.iter().any(|s| s.contains(&name));
                out.push((name, f.span, !conflict));
            }
        }
        StmtKind::Block(block) => walk_annex_b_list(&block.stmts, stack, out, true),
        StmtKind::If {
            consequent,
            alternate,
            ..
        } => {
            walk_annex_b_stmt(consequent, stack, out, _current);
            if let Some(alt) = alternate {
                walk_annex_b_stmt(alt, stack, out, _current);
            }
        }
        StmtKind::While { body, .. }
        | StmtKind::DoWhile { body, .. }
        | StmtKind::With { body, .. }
        | StmtKind::Labeled { body, .. } => walk_annex_b_stmt(body, stack, out, _current),
        StmtKind::For { init, body, .. } => {
            // For-head lexical declarations (`for (let f; …)`) are enclosing
            // names for the body's functions.
            if let Some(ForInit::VarDecl { kind, decls, .. }) = init
                && *kind != VarDeclKind::Var
                && let Some(top) = stack.last_mut()
            {
                for decl in decls {
                    let mut names = Vec::new();
                    bound_names(&decl.pattern, &mut names);
                    top.extend(names);
                }
            }
            walk_annex_b_stmt(body, stack, out, _current);
        }
        StmtKind::ForIn { left, body, .. } | StmtKind::ForOf { left, body, .. } => {
            if let ForBinding::VarDecl { kind, pattern, .. } = left
                && *kind != VarDeclKind::Var
                && let Some(top) = stack.last_mut()
            {
                let mut names = Vec::new();
                bound_names(pattern, &mut names);
                top.extend(names);
            }
            walk_annex_b_stmt(body, stack, out, _current);
        }
        StmtKind::Try {
            block,
            handler,
            finalizer,
        } => {
            walk_annex_b_list(&block.stmts, stack, out, true);
            if let Some(handler) = handler {
                // The catch body is a list whose scope includes the
                // parameter (a non-simple one conflicts with a var).
                stack.push(HashSet::new());
                if let Some(param) = &handler.param
                    && !matches!(param, BindingPattern::Ident(_))
                {
                    let mut names = Vec::new();
                    bound_names(param, &mut names);
                    if let Some(top) = stack.last_mut() {
                        top.extend(names);
                    }
                }
                walk_annex_b_list(&handler.body.stmts, stack, out, true);
                stack.pop();
            }
            if let Some(finalizer) = finalizer {
                walk_annex_b_list(&finalizer.stmts, stack, out, true);
            }
        }
        StmtKind::Switch { cases, .. } => {
            stack.push(HashSet::new());
            for case in cases {
                walk_annex_b_list(&case.consequent, stack, out, true);
            }
            stack.pop();
        }
        _ => {}
    }
}

// ---- GlobalDeclarationInstantiation (spec 16.1.7) ----

/// GlobalDeclarationInstantiation (spec 16.1.7): create the script's global
/// bindings — lexical declarations in the declarative record, functions and
/// vars as properties of the global object — checking for redeclarations and
/// restricted globals first.
pub fn global_declaration_instantiation(
    agent: &mut Agent,
    program: &Program,
    global_env: &EnvRef,
    strict: bool,
) -> Result<(), JsError> {
    let lexical_names = top_level_lexically_declared_names(&program.body);
    let variable_names = top_level_var_declared_names(&program.body);

    for name in &lexical_names {
        if global_env.has_lexical_declaration(name) {
            return Err(JsError::new(
                ErrorKind::SyntaxError,
                format!(
                    "Identifier {:?} has already been declared",
                    name.to_string_lossy()
                ),
            ));
        }
        if global_env.has_restricted_global_property(name)? {
            return Err(JsError::new(
                ErrorKind::SyntaxError,
                format!(
                    "Identifier {:?} has already been declared",
                    name.to_string_lossy()
                ),
            ));
        }
    }
    for name in &variable_names {
        if global_env.has_lexical_declaration(name) {
            return Err(JsError::new(
                ErrorKind::SyntaxError,
                format!(
                    "Identifier {:?} has already been declared",
                    name.to_string_lossy()
                ),
            ));
        }
    }

    let variable_decls = top_level_var_scoped_declarations(&program.body);

    // Function declarations, last one wins; checked in reverse order.
    let mut funcs_to_initialize: Vec<&syntax::ast::Function> = Vec::new();
    let mut declared_func_names: Vec<JsString> = Vec::new();
    for decl in variable_decls.iter().rev() {
        let VarScopedDecl::Function(f) = decl else {
            continue;
        };
        let Some(func_name) = f.name else {
            continue;
        };
        let name = lookup(func_name);
        if declared_func_names.contains(&name) {
            continue;
        }
        if !global_env.can_declare_global_function(&name)? {
            return Err(JsError::new(
                ErrorKind::TypeError,
                format!("Cannot define global function {:?}", name.to_string_lossy()),
            ));
        }
        declared_func_names.push(name);
        funcs_to_initialize.insert(0, *f);
    }

    let mut declared_variable_names: Vec<JsString> = Vec::new();
    for decl in &variable_decls {
        let VarScopedDecl::Variable(names) = decl else {
            continue;
        };
        for name in names {
            if declared_func_names.contains(name) {
                continue;
            }
            if !global_env.can_declare_global_var(name)? {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    format!(
                        "Cannot declare global variable {:?}",
                        name.to_string_lossy()
                    ),
                ));
            }
            if !declared_variable_names.contains(name) {
                declared_variable_names.push(name.clone());
            }
        }
    }

    // Lexical declarations: instantiated here, initialized at evaluation.
    let lexical_decls = top_level_lexically_scoped_declarations(&program.body);
    for decl in lexical_decls {
        let mut names = Vec::new();
        bound_names_of_decl(decl, &mut names);
        for name in names {
            if is_constant_declaration(decl) {
                global_env.create_immutable_binding(&name, true)?;
            } else {
                global_env.create_mutable_binding(&name, false)?;
            }
        }
    }

    for f in funcs_to_initialize {
        let Some(func_name) = f.name else {
            continue;
        };
        let name = lookup(func_name);
        let func_obj = crate::function::instantiate_function(
            agent,
            f,
            *global_env,
            strict,
            Vec::new(),
            Vec::new(),
        )?;
        global_env.create_global_function_binding(&name, func_obj, false)?;
    }

    for name in &declared_variable_names {
        global_env.create_global_var_binding(name, false)?;
    }

    // Annex B.3.3.2: sloppy block-level function declarations hoist a global
    // binding (undefined) when the hoist would not produce an early error.
    if !strict {
        for (name, span, hoistable) in annex_b_function_hoists(&program.body) {
            if !hoistable {
                continue;
            }
            if global_env.has_lexical_declaration(&name) {
                continue;
            }
            if !declared_func_names.contains(&name) && !declared_variable_names.contains(&name) {
                // spec 16.1.7 step 9.e (B.3.3.3): the hoist is a var
                // binding, so a pre-existing own property (e.g. a
                // non-enumerable one) is left in place; only missing
                // bindings are created.
                global_env.create_global_var_binding(&name, false)?;
            }
            agent
                .running_context()?
                .annex_b_hoistable
                .borrow_mut()
                .insert((span.start, span.end));
        }
    }

    Ok(())
}

/// The caller's position for PerformEval's Script early errors (spec
/// 19.2.1.1 steps 5-7): `in_function` when GetThisEnvironment is a function
/// Environment Record, `in_method` when that function has a [[HomeObject]].
fn eval_caller_context(agent: &Agent) -> (bool, bool) {
    let Ok(this_env) = crate::context::get_this_environment(agent) else {
        return (false, false);
    };
    if !matches!(&*this_env, EnvRecord::Function(_)) {
        return (false, false);
    }
    (true, this_env.has_super_binding(agent))
}

/// The caller's private identifiers (without the `#`), walking the running
/// context's PrivateEnvironment chain (spec 9.4.1).
fn collect_private_names(agent: &Agent) -> Vec<crux::AtomId> {
    let Some(private_env) = agent
        .running_context()
        .ok()
        .and_then(|context| context.private_environment)
    else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let mut current: Option<Handle<crate::context::PrivateEnvironment>> = Some(private_env);
    while let Some(env) = current {
        for name in env.names.borrow().iter() {
            let units = name.description.as_slice();
            names.push(crux::intern(units.get(1..).unwrap_or(&[])));
        }
        current = env.outer
    }
    names
}

/// PerformEval (spec 19.2.1.1): parse `source` as a Script and evaluate it
/// in a new execution context nested on the stack.
///
/// `strict_caller` is the strictness of the calling code (direct eval only);
/// `direct` selects the direct-eval wiring: a direct eval's lexical
/// environment extends the caller's, and its vars share the caller's
/// variable environment (unless the eval code is strict).
pub fn perform_eval(
    agent: &mut Agent,
    source: &JsString,
    strict_caller: bool,
    direct: bool,
) -> Result<Value, JsError> {
    let eval_realm = agent.current_realm()?;
    // The caller-context checks of spec 19.2.1.1 steps 5-7: a *direct* eval
    // inherits the caller's function/method position, so the eval'd code may
    // use `new.target`/`super` there, and a caller inside a class makes
    // PrivateIdentifiers parse (they resolve against the inherited private
    // environment). An indirect eval runs in the global scope: all flags stay
    // false. The eval'd `arguments` inside a field initializer is rejected
    // separately below.
    let (in_function, in_method) = if direct {
        eval_caller_context(agent)
    } else {
        (false, false)
    };
    let allow_private = direct && agent.running_context()?.private_environment.is_some();
    let eval_context = parser::EvalContext {
        in_function,
        in_method,
        allow_private,
    };
    // The caller's private identifiers (without the `#`), for validating
    // eval'd `#name` uses against the inherited private environment (spec
    // 19.2.1.1 AllPrivateNamesValid).
    let caller_private_names = if allow_private {
        collect_private_names(agent)
    } else {
        Vec::new()
    };

    // HostEnsureCanCompileStrings (spec 19.2.1.1 step 4).
    let body_string = source.clone();
    if let Some(hooks) = &agent.host_hooks {
        hooks.ensure_can_compile_strings(&eval_realm, &[], &body_string, direct)?;
    }

    let program =
        parser::parse_script_utf16_eval(source.as_slice(), &eval_context, &caller_private_names)?;
    // A fresh eval parse is a distinct site space for the template cache.
    crate::expr::bump_template_parse_generation();
    // A script with no body evaluates to undefined.
    if program.body.is_empty() {
        return Ok(Value::Undefined);
    }
    // spec 19.2.1.1 steps 10-11: strictCaller applies to direct eval only —
    // an indirect eval is always sloppy unless its own directive prologue
    // says otherwise.
    let strict_eval = if direct {
        strict_caller || script_is_strict(source, &program)
    } else {
        script_is_strict(source, &program)
    };
    if strict_eval && !script_is_strict(source, &program) {
        // The caller's strictness subjects the eval'd code to the strict-mode
        // early errors (reserved-word bindings, `with`, octal escapes, …) the
        // first parse could not apply. Re-parse with a synthetic use-strict
        // directive solely to validate; the original program runs.
        let mut units = Vec::with_capacity(source.len() + 14);
        units.extend_from_slice(
            "'use strict';\n"
                .encode_utf16()
                .collect::<Vec<u16>>()
                .as_slice(),
        );
        units.extend_from_slice(source.as_slice());
        parser::parse_script_utf16_eval(&units, &eval_context, &caller_private_names)?;
    }
    // spec 15.14.2 early errors: a `using` declaration at the top level of a
    // Script (the eval goal) is a SyntaxError unless nested in a Block,
    // ForStatement, ForInOfStatement, or function body — which the parser
    // already enforces for those contexts.
    if program
        .body
        .iter()
        .any(|stmt| matches!(stmt.kind, StmtKind::UsingDecl { .. }))
    {
        return Err(JsError::new(
            ErrorKind::SyntaxError,
            "using declarations are not allowed at the top level of eval".into(),
        ));
    }
    // spec 19.2.1.1 step 10: a direct eval inside a class field initializer
    // is a SyntaxError when the eval'd code references `arguments` (arrows
    // count; nested functions have their own `arguments`).
    if direct && agent.field_initializer_depth > 0 && contains_arguments(&program) {
        return Err(JsError::new(
            ErrorKind::SyntaxError,
            "'arguments' is not allowed in direct eval inside a class field initializer".into(),
        ));
    }

    let running = agent.running_context()?;
    let (lexical_env, variable_env, private_env) = if direct {
        // Direct eval: a fresh lexical env over the caller's, sharing the
        // caller's variable environment (spec 19.2.1.1 step 12).
        let lexical_env = new_declarative_environment(Some(running.lexical_environment));
        (
            lexical_env,
            running.variable_environment,
            running.private_environment,
        )
    } else {
        // Indirect eval: fresh lexical env over the global environment.
        let global_env = eval_realm.global_env();
        (
            new_declarative_environment(Some(global_env)),
            global_env,
            None,
        )
    };
    // Strict eval code cannot touch the caller's variable environment.
    let variable_env = if strict_eval {
        lexical_env
    } else {
        variable_env
    };

    let script_or_module = running.script_or_module.clone();
    let eval_source = source.clone();
    let eval_context = ExecutionContext {
        function: None,
        realm: eval_realm,
        script_or_module,
        lexical_environment: lexical_env,
        variable_environment: variable_env,
        private_environment: private_env,
        source: Some(eval_source),
        annex_b_hoistable: Default::default(),
    };
    agent.execution_context_stack.push(eval_context);
    let result = (|| -> Result<Value, JsError> {
        eval_declaration_instantiation(agent, &program, &variable_env, &lexical_env, strict_eval)?;
        crate::eval::eval_program(agent, &program, strict_eval, false)
    })();
    agent.execution_context_stack.pop();
    result
}

/// EvalDeclarationInstantiation (spec 19.2.1.4): instantiate the eval'd
/// script's declarations — vars and functions in `variable_env`, lexical
/// declarations in `lexical_env` — after validating that a sloppy eval's
/// vars do not collide with lexical bindings.
fn eval_declaration_instantiation(
    agent: &mut Agent,
    program: &Program,
    variable_env: &EnvRef,
    lexical_env: &EnvRef,
    strict: bool,
) -> Result<(), JsError> {
    let variable_names = top_level_var_declared_names(&program.body);
    let variable_decls = top_level_var_scoped_declarations(&program.body);
    let variable_env_is_global = matches!(&**variable_env, EnvRecord::Global(_));

    if !strict {
        if variable_env_is_global {
            for name in &variable_names {
                if variable_env.has_lexical_declaration(name) {
                    return Err(duplicate_declaration_error(name));
                }
            }
            // spec 19.2.1.4 step 6: an eval lexical declaration must not
            // collide with a restricted (non-configurable) global var/function
            // binding (script-decl-lex-collision); eval-introduced globals are
            // configurable and do not collide (script-decl-lex-no-collision).
            for name in top_level_lexically_declared_names(&program.body) {
                if variable_env.has_restricted_global_property(&name)? {
                    return Err(duplicate_declaration_error(&name));
                }
            }
        }
        // Walk from the eval's lexical env up to (but not including) the
        // variable environment, rejecting vars that would hoist over a
        // lexical binding (spec 19.2.1.4 steps 3-10). A catch parameter's
        // environment is exempt (Annex B.3.5).
        let mut this_env = Some(*lexical_env);
        while let Some(env) = this_env {
            if Handle::ptr_eq(env, *variable_env) {
                break;
            }
            if !matches!(&*env, EnvRecord::Object(_)) && !env.is_catch_param_env() {
                for name in &variable_names {
                    if env.has_binding(name)? {
                        return Err(duplicate_declaration_error(name));
                    }
                }
            }
            this_env = env.outer();
        }
    }

    // Function declarations, last one wins; checked in reverse order.
    let mut funcs_to_initialize: Vec<&syntax::ast::Function> = Vec::new();
    let mut declared_func_names: Vec<JsString> = Vec::new();
    for decl in variable_decls.iter().rev() {
        let VarScopedDecl::Function(f) = decl else {
            continue;
        };
        let Some(func_name) = f.name else {
            continue;
        };
        let name = lookup(func_name);
        if declared_func_names.contains(&name) {
            continue;
        }
        if variable_env_is_global && !variable_env.can_declare_global_function(&name)? {
            return Err(JsError::new(
                ErrorKind::TypeError,
                format!("Cannot define global function {:?}", name.to_string_lossy()),
            ));
        }
        declared_func_names.push(name);
        funcs_to_initialize.insert(0, *f);
    }

    let mut declared_variable_names: Vec<JsString> = Vec::new();
    for decl in &variable_decls {
        let VarScopedDecl::Variable(names) = decl else {
            continue;
        };
        for name in names {
            if declared_func_names.contains(name) {
                continue;
            }
            if variable_env_is_global && !variable_env.can_declare_global_var(name)? {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    format!(
                        "Cannot declare global variable {:?}",
                        name.to_string_lossy()
                    ),
                ));
            }
            if !declared_variable_names.contains(name) {
                declared_variable_names.push(name.clone());
            }
        }
    }

    // Lexical declarations are instantiated in the eval's lexical env.
    let lexical_decls = top_level_lexically_scoped_declarations(&program.body);
    for decl in lexical_decls {
        let mut names = Vec::new();
        bound_names_of_decl(decl, &mut names);
        for name in names {
            if is_constant_declaration(decl) {
                lexical_env.create_immutable_binding(&name, true)?;
            } else {
                lexical_env.create_mutable_binding(&name, false)?;
            }
        }
    }

    for f in funcs_to_initialize {
        let Some(func_name) = f.name else {
            continue;
        };
        let name = lookup(func_name);
        let env = agent.running_context()?.lexical_environment;
        let func_obj =
            crate::function::instantiate_function(agent, f, env, strict, Vec::new(), Vec::new())?;
        if variable_env_is_global {
            // Eval-created global functions are deletable.
            variable_env.create_global_function_binding(&name, func_obj, true)?;
        } else if !variable_env.has_binding(&name)? {
            variable_env.create_mutable_binding(&name, true)?;
            variable_env.initialize_binding(&name, func_obj)?;
        } else {
            variable_env.set_mutable_binding(&name, func_obj, false)?;
        }
    }

    for name in &declared_variable_names {
        if variable_env_is_global {
            // Eval-created global vars are deletable.
            variable_env.create_global_var_binding(name, true)?;
        } else if !variable_env.has_binding(name)? {
            variable_env.create_mutable_binding(name, true)?;
            variable_env.initialize_binding(name, Value::Undefined)?;
        }
    }

    // Annex B.3.3.3: sloppy block-level function declarations in eval code
    // hoist a var binding in the eval's variable environment (skipped when
    // the hoist would produce an early error).
    if !strict {
        for (name, span, hoistable) in annex_b_function_hoists(&program.body) {
            if !hoistable {
                continue;
            }
            if !declared_func_names.contains(&name)
                && !declared_variable_names.contains(&name)
                && !variable_env.has_binding(&name)?
            {
                if variable_env_is_global {
                    // Eval-created global bindings are deletable.
                    variable_env.create_global_var_binding(&name, true)?;
                } else {
                    variable_env.create_mutable_binding(&name, true)?;
                }
                variable_env.initialize_binding(&name, Value::Undefined)?;
            }
            agent
                .running_context()?
                .annex_b_hoistable
                .borrow_mut()
                .insert((span.start, span.end));
        }
    }

    Ok(())
}

fn duplicate_declaration_error(name: &JsString) -> JsError {
    JsError::new(
        ErrorKind::SyntaxError,
        format!(
            "Identifier {:?} has already been declared",
            name.to_string_lossy()
        ),
    )
}

/// The BoundNames of a top-level lexical declaration.
fn bound_names_of_decl(stmt: &Stmt, out: &mut Vec<JsString>) {
    match &stmt.kind {
        StmtKind::VarDecl { decls, .. } | StmtKind::UsingDecl { decls, .. } => {
            for decl in decls {
                bound_names(&decl.pattern, out);
            }
        }
        StmtKind::ClassDecl(class) => {
            if let Some(name) = class.name {
                out.push(lookup(name));
            }
        }
        _ => {}
    }
}

/// ContainsArguments (spec 15.7.9) over a Script body, for the eval-inside-
/// class-field-initializer early error (spec 19.2.1.1 step 10): an
/// `arguments` IdentifierReference counts unless a nested function or class
/// method body would own its own `arguments` (arrows are transparent).
fn contains_arguments(program: &Program) -> bool {
    let mut found = false;
    walk_stmts(&program.body, &mut |expr| {
        if matches!(expr.kind, ExprKind::Ident(atom) if atom == crux::intern_utf8("arguments")) {
            found = true;
        }
    });
    found
}

/// Whether a function body can observe the `arguments` binding: it
/// references the identifier (through nested arrows, which inherit it;
/// nested functions have their own) or contains a direct `eval` (which
/// could introduce a reference). When neither, the arguments object is
/// unobservable and function instantiation may bind `undefined` instead of
/// building it (the Sputnik decodeURI fixtures call tiny sloppy helpers
/// millions of times).
pub(crate) fn body_observes_arguments(body: &syntax::ast::Block) -> bool {
    let arguments_atom = crux::intern_utf8("arguments");
    let eval_atom = crux::intern_utf8("eval");
    let mut found = false;
    walk_stmts(&body.stmts, &mut |expr| {
        if found {
            return;
        }
        match &expr.kind {
            ExprKind::Ident(atom) if *atom == arguments_atom => found = true,
            ExprKind::Call(call) if matches!(&call.callee.kind, ExprKind::Ident(atom) if *atom == eval_atom) =>
            {
                found = true;
            }
            _ => {}
        }
    });
    found
}

fn walk_stmts(stmts: &[Stmt], visit: &mut impl FnMut(&Expr)) {
    for stmt in stmts {
        match &stmt.kind {
            StmtKind::Block(block) => walk_stmts(&block.stmts, visit),
            StmtKind::Expr(expr) => walk_exprs(expr, visit),
            StmtKind::If {
                test,
                consequent,
                alternate,
            } => {
                walk_exprs(test, visit);
                walk_stmts(std::slice::from_ref(consequent), visit);
                if let Some(alt) = alternate {
                    walk_stmts(std::slice::from_ref(alt), visit);
                }
            }
            StmtKind::VarDecl { decls, .. } | StmtKind::UsingDecl { decls, .. } => {
                for decl in decls {
                    if let Some(init) = &decl.init {
                        walk_exprs(init, visit);
                    }
                }
            }
            StmtKind::Return(Some(expr)) | StmtKind::Throw(expr) => walk_exprs(expr, visit),
            StmtKind::Return(None) => {}
            StmtKind::While { test, body } => {
                walk_exprs(test, visit);
                walk_stmts(std::slice::from_ref(body), visit);
            }
            StmtKind::DoWhile { body, test } => {
                walk_stmts(std::slice::from_ref(body), visit);
                walk_exprs(test, visit);
            }
            StmtKind::For {
                init,
                test,
                update,
                body,
            } => {
                walk_for_init(init, visit);
                if let Some(t) = test {
                    walk_exprs(t, visit);
                }
                if let Some(u) = update {
                    walk_exprs(u, visit);
                }
                walk_stmts(std::slice::from_ref(body), visit);
            }
            StmtKind::ForIn { left, right, body }
            | StmtKind::ForOf {
                left, right, body, ..
            } => {
                walk_for_binding(left, visit);
                walk_exprs(right, visit);
                walk_stmts(std::slice::from_ref(body), visit);
            }
            StmtKind::Try {
                block,
                handler,
                finalizer,
            } => {
                walk_stmts(&block.stmts, visit);
                if let Some(handler) = handler {
                    walk_stmts(&handler.body.stmts, visit);
                }
                if let Some(finalizer) = finalizer {
                    walk_stmts(&finalizer.stmts, visit);
                }
            }
            StmtKind::Switch {
                discriminant,
                cases,
            } => {
                walk_exprs(discriminant, visit);
                for case in cases {
                    if let Some(test) = &case.test {
                        walk_exprs(test, visit);
                    }
                    walk_stmts(&case.consequent, visit);
                }
            }
            StmtKind::With { object, body } => {
                walk_exprs(object, visit);
                walk_stmts(std::slice::from_ref(body), visit);
            }
            StmtKind::Labeled { body, .. } => walk_stmts(std::slice::from_ref(body), visit),
            StmtKind::FunctionDecl(_) => {}
            StmtKind::ClassDecl(class) => walk_class(class, visit),
            StmtKind::Empty | StmtKind::Debugger | StmtKind::Break(_) | StmtKind::Continue(_) => {}
        }
    }
}

fn walk_exprs(expr: &Expr, visit: &mut impl FnMut(&Expr)) {
    visit(expr);
    match &expr.kind {
        ExprKind::Literal(_)
        | ExprKind::Ident(_)
        | ExprKind::This
        | ExprKind::Super
        | ExprKind::MetaProperty { .. }
        | ExprKind::Function(_) => {}
        ExprKind::Class(class) => walk_class(class, visit),
        ExprKind::Arrow { params, body, .. } => {
            for param in params {
                if let Some(init) = &param.init {
                    walk_exprs(init, visit);
                }
            }
            match body {
                ArrowBody::Expr(expr) => walk_exprs(expr, visit),
                ArrowBody::Block(block) => walk_stmts(&block.stmts, visit),
            }
        }
        ExprKind::PrivateIn { object, .. } => walk_exprs(object, visit),
        ExprKind::Array(array) => {
            for element in &array.elements {
                match element {
                    ArrayElement::Expr(e) | ArrayElement::Spread(e) => walk_exprs(e, visit),
                    ArrayElement::Hole => {}
                }
            }
        }
        ExprKind::Object(object) => {
            for prop in &object.props {
                match prop {
                    ObjectProperty::Init { key, value, .. } => {
                        walk_property_name(key, visit);
                        walk_exprs(value, visit);
                    }
                    ObjectProperty::Method { key, .. }
                    | ObjectProperty::Get { key, .. }
                    | ObjectProperty::Set { key, .. } => walk_property_name(key, visit),
                    ObjectProperty::Spread(e) => walk_exprs(e, visit),
                }
            }
        }
        ExprKind::Unary { operand, .. } => walk_exprs(operand, visit),
        ExprKind::Update { target, .. } => walk_exprs(target, visit),
        ExprKind::Binary { left, right, .. } | ExprKind::Logical { left, right, .. } => {
            walk_exprs(left, visit);
            walk_exprs(right, visit);
        }
        ExprKind::Assign { target, value, .. } => {
            walk_exprs(target, visit);
            walk_exprs(value, visit);
        }
        ExprKind::Conditional {
            test,
            consequent,
            alternate,
        } => {
            walk_exprs(test, visit);
            walk_exprs(consequent, visit);
            walk_exprs(alternate, visit);
        }
        ExprKind::Call(call) => {
            walk_exprs(&call.callee, visit);
            for arg in &call.args {
                walk_argument(arg, visit);
            }
        }
        ExprKind::New(new) => {
            walk_exprs(&new.callee, visit);
            for arg in &new.args {
                walk_argument(arg, visit);
            }
        }
        ExprKind::Member(member) => {
            walk_exprs(&member.object, visit);
            if let syntax::MemberProperty::Computed(index) = &member.property {
                walk_exprs(index, visit);
            }
        }
        ExprKind::TaggedTemplate { tag, quasi } => {
            walk_exprs(tag, visit);
            for e in &quasi.exprs {
                walk_exprs(e, visit);
            }
        }
        ExprKind::Template(template) => {
            for e in &template.exprs {
                walk_exprs(e, visit);
            }
        }
        ExprKind::Paren(inner) => walk_exprs(inner, visit),
        ExprKind::Sequence(exprs) => {
            for e in exprs {
                walk_exprs(e, visit);
            }
        }
        ExprKind::Yield { argument, .. } => {
            if let Some(argument) = argument {
                walk_exprs(argument, visit);
            }
        }
        ExprKind::Await(operand) => walk_exprs(operand, visit),
        ExprKind::ImportCall {
            specifier, options, ..
        } => {
            walk_exprs(specifier, visit);
            if let Some(options) = options {
                walk_exprs(options, visit);
            }
        }
    }
}

fn walk_argument(argument: &Argument, visit: &mut impl FnMut(&Expr)) {
    match argument {
        Argument::Expr(e) | Argument::Spread(e) => walk_exprs(e, visit),
    }
}

fn walk_property_name(name: &PropertyName, visit: &mut impl FnMut(&Expr)) {
    if let PropertyName::Computed(expr) = name {
        walk_exprs(expr, visit);
    }
}

/// The parts of a nested class that inherit the enclosing containment:
/// heritage, computed element names, and field initializers. Method bodies,
/// params, and static blocks own their own `arguments`.
fn walk_class(class: &Class, visit: &mut impl FnMut(&Expr)) {
    if let Some(heritage) = &class.heritage {
        walk_exprs(heritage, visit);
    }
    for element in &class.elements {
        match element {
            ClassElement::Method { name, .. }
            | ClassElement::Get { name, .. }
            | ClassElement::Set { name, .. } => walk_class_name(name, visit),
            ClassElement::Field { name, init, .. } => {
                walk_class_name(name, visit);
                if let Some(init) = init {
                    walk_exprs(init, visit);
                }
            }
            ClassElement::StaticBlock(_) => {}
        }
    }
}

fn walk_class_name(name: &ClassElementName, visit: &mut impl FnMut(&Expr)) {
    if let ClassElementName::Property(PropertyName::Computed(expr)) = name {
        walk_exprs(expr, visit);
    }
}

fn walk_for_init(init: &Option<ForInit>, visit: &mut impl FnMut(&Expr)) {
    match init {
        Some(ForInit::Expr(expr)) => walk_exprs(expr, visit),
        Some(ForInit::VarDecl { decls, .. }) => {
            for decl in decls {
                if let Some(init) = &decl.init {
                    walk_exprs(init, visit);
                }
            }
        }
        None => {}
    }
}

fn walk_for_binding(binding: &ForBinding, visit: &mut impl FnMut(&Expr)) {
    match binding {
        ForBinding::Expr(expr) => walk_exprs(expr, visit),
        ForBinding::VarDecl { init, .. } => {
            if let Some(init) = init {
                walk_exprs(init, visit);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crux::string::intern_utf8;
    use crux::value::ValueKind;

    fn parse(source: &str) -> Program {
        parser::parse_script(source).unwrap()
    }

    fn strings(v: &[JsString]) -> Vec<String> {
        v.iter().map(|s| s.to_string_lossy()).collect()
    }

    #[test]
    fn declaration_collectors_split_lexical_and_var() {
        let program = parse("let a; const b = 1; class C {} function f() {} var v; { var w; }");
        let lexical = top_level_lexically_declared_names(&program.body);
        assert_eq!(strings(&lexical), vec!["a", "b", "C"]);
        let vars = top_level_var_declared_names(&program.body);
        assert_eq!(strings(&vars), vec!["f", "v", "w"]);
        // Declarations: top-level functions first, then vars in order.
        let decls = top_level_var_scoped_declarations(&program.body);
        assert_eq!(decls.len(), 3);
        assert!(matches!(decls[0], VarScopedDecl::Function(_)));
        assert_eq!(
            strings(match &decls[1] {
                VarScopedDecl::Variable(names) => names,
                _ => unreachable!(),
            }),
            vec!["v"]
        );
        assert_eq!(
            strings(match &decls[2] {
                VarScopedDecl::Variable(names) => names,
                _ => unreachable!(),
            }),
            vec!["w"]
        );
    }

    #[test]
    fn for_heads_contribute_var_declarations() {
        let program = parse("for (var i = 0; i < 1; i++) {} for (var k in obj) {}");
        let vars = top_level_var_declared_names(&program.body);
        assert_eq!(strings(&vars), vec!["i", "k"]);
    }

    #[test]
    fn script_is_strict_detects_directives() {
        let strict = |src: &str| script_is_strict(&JsString::from_utf8(src), &parse(src));
        assert!(strict("'use strict'; 1;"));
        assert!(!strict("1;"));
        // The directive must be first.
        assert!(!strict("1; 'use strict';"));
        // Escapes and line continuations are not directives (spec 14.1.1).
        assert!(!strict("'use\\u0020strict'; 1;"));
        assert!(!strict("'use str\\\n ict'; 1;"));
    }

    #[test]
    fn bound_names_collect_patterns() {
        let mut out = Vec::new();
        bound_names(&BindingPattern::Ident(intern_utf8("x")), &mut out);
        bound_names(
            &BindingPattern::Array(vec![
                ArrayBindingElement::Element(syntax::ast::BindingElement {
                    pattern: BindingPattern::Ident(intern_utf8("y")),
                    init: None,
                    rest: false,
                    span: crux::Span::new(0, 0),
                }),
                ArrayBindingElement::Hole,
            ]),
            &mut out,
        );
        assert_eq!(strings(&out), vec!["x", "y"]);
    }

    #[test]
    fn constant_declaration_classifies_const_only() {
        let program = parse("let a; const b = 1; var c;");
        assert!(!is_constant_declaration(&program.body[0]));
        assert!(is_constant_declaration(&program.body[1]));
        assert!(!is_constant_declaration(&program.body[2]));
    }

    fn evaluated(source: &str) -> Result<Value, JsError> {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        perform_eval(&mut agent, &JsString::from_utf8(source), false, true)
    }

    #[test]
    fn empty_eval_returns_undefined() {
        assert_eq!(evaluated("").unwrap(), Value::Undefined);
    }

    #[test]
    fn direct_eval_binds_vars_in_the_caller_variable_environment() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let result = perform_eval(
            &mut agent,
            &JsString::from_utf8("var ev = 5; ev"),
            false,
            true,
        )
        .unwrap();
        assert_eq!(result, Value::Number(5.0));
        // The var landed on the global object, deletable (eval-created).
        let global = agent.running_context().unwrap().realm.global_object;
        assert_eq!(
            global.get(&JsString::from_utf8("ev")).unwrap(),
            Value::Number(5.0)
        );
        let prop = global
            .get_own_property(&JsString::from_utf8("ev"))
            .unwrap()
            .unwrap();
        assert!(prop.configurable);
    }

    #[test]
    fn indirect_eval_uses_the_global_environment() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let result = perform_eval(
            &mut agent,
            &JsString::from_utf8("var gv = 7; gv"),
            false,
            false,
        )
        .unwrap();
        assert_eq!(result, Value::Number(7.0));
        let global = agent.running_context().unwrap().realm.global_object;
        assert_eq!(
            global.get(&JsString::from_utf8("gv")).unwrap(),
            Value::Number(7.0)
        );
    }

    #[test]
    fn strict_eval_isolates_var_declarations() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let result = perform_eval(
            &mut agent,
            &JsString::from_utf8("'use strict'; var s = 1; s"),
            false,
            false,
        )
        .unwrap();
        assert_eq!(result, Value::Number(1.0));
        // Strict eval's vars go to the fresh lexical env, not the global.
        let global = agent.running_context().unwrap().realm.global_object;
        assert!(!global.has_own_property(&JsString::from_utf8("s")).unwrap());
    }

    #[test]
    fn eval_lexical_declarations_stay_local() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let result = perform_eval(
            &mut agent,
            &JsString::from_utf8("let lx = 3; lx"),
            false,
            false,
        )
        .unwrap();
        assert_eq!(result, Value::Number(3.0));
        let realm = agent.running_context().unwrap().realm;
        assert!(
            !realm
                .global_env
                .has_lexical_declaration(&JsString::from_utf8("lx"))
        );
    }

    #[test]
    fn sloppy_eval_var_conflicts_with_lexical_bindings() {
        // A global lexical declaration blocks a like-named eval var.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent.run_script("let x;").unwrap();
        assert!(perform_eval(&mut agent, &JsString::from_utf8("var x;"), false, true).is_err());
        // And a like-named var in a *strict* eval is fine (separate env).
        assert!(
            perform_eval(
                &mut agent,
                &JsString::from_utf8("'use strict'; var x;"),
                false,
                true
            )
            .is_ok()
        );
    }

    #[test]
    fn eval_nests_on_the_execution_context_stack() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        assert_eq!(agent.execution_context_stack.len(), 1);
        let result = perform_eval(
            &mut agent,
            &JsString::from_utf8("var nested = 1; nested"),
            false,
            true,
        )
        .unwrap();
        assert_eq!(result, Value::Number(1.0));
        // The eval context was pushed and popped.
        assert_eq!(agent.execution_context_stack.len(), 1);
    }

    #[test]
    fn eval_runs_inside_jobs() {
        let mut agent = Agent::new();
        let realm = agent.initialize_host_defined_realm().unwrap();
        agent.enqueue_generic_job(Some(realm), move |agent| {
            let result = perform_eval(
                agent,
                &JsString::from_utf8("var from_job = 2; from_job"),
                false,
                true,
            )?;
            assert_eq!(result, Value::Number(2.0));
            assert_eq!(agent.execution_context_stack.len(), 1);
            Ok(Value::Undefined)
        });
        agent.run_jobs().unwrap();
        let global = agent.running_context().unwrap().realm.global_object;
        assert_eq!(
            global.get(&JsString::from_utf8("from_job")).unwrap(),
            Value::Number(2.0)
        );
    }

    #[test]
    fn direct_eval_in_field_initializer_rejects_arguments() {
        // spec 19.2.1.1: a direct eval inside a class field initializer is a
        // SyntaxError when the eval'd code references `arguments` — including
        // inside arrows created by the initializer and called later.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let result = agent.run_script(
            "var executed = false; \
             class C { x = eval('executed = true; arguments;'); } \
             var threw = false; \
             try { new C(); } catch (e) { threw = e instanceof SyntaxError; } \
             threw + ',' + executed",
        );
        assert_eq!(
            result.unwrap(),
            Value::String(Handle::new(JsString::from_utf8("true,false")))
        );
        // An arrow created in the initializer and called later still counts.
        let result = agent.run_script(
            "var executed = false; \
             class D { x = () => { eval('executed = true; arguments;'); }; } \
             var threw = false; \
             try { new D().x(); } catch (e) { threw = e instanceof SyntaxError; } \
             threw + ',' + executed",
        );
        assert_eq!(
            result.unwrap(),
            Value::String(Handle::new(JsString::from_utf8("true,false")))
        );
        // A plain function expression has its own `arguments`: no error.
        let result = agent.run_script(
            "var executed = false; \
             class E { x = function() { eval('executed = true; arguments;'); }; } \
             var threw = false; \
             try { new E().x(); } catch (e) { threw = e instanceof SyntaxError; } \
             threw + ',' + executed",
        );
        assert_eq!(
            result.unwrap(),
            Value::String(Handle::new(JsString::from_utf8("false,true")))
        );
    }

    #[test]
    fn eval_function_declarations_bind_to_the_variable_environment() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        perform_eval(
            &mut agent,
            &JsString::from_utf8("function ef() {}"),
            false,
            true,
        )
        .unwrap();
        let global = agent.running_context().unwrap().realm.global_object;
        assert!(matches!(
            global.get(&JsString::from_utf8("ef")).unwrap().kind(),
            ValueKind::Function(_)
        ));
    }

    #[test]
    fn indirect_eval_ignores_the_callers_strictness() {
        // spec 19.2.1.1 step 10: strictCaller applies to direct eval only;
        // an indirect eval in strict caller code is still sloppy.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let result = perform_eval(
            &mut agent,
            &JsString::from_utf8("with ({}) {} var iv = 3; iv"),
            true,
            false,
        )
        .unwrap();
        assert_eq!(result, Value::Number(3.0));
        let global = agent.running_context().unwrap().realm.global_object;
        assert_eq!(
            global.get(&JsString::from_utf8("iv")).unwrap(),
            Value::Number(3.0)
        );
    }

    #[test]
    fn strict_eval_rejects_with_and_reserved_word_bindings() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        assert!(
            perform_eval(
                &mut agent,
                &JsString::from_utf8("var public = 1;"),
                true,
                true,
            )
            .is_err()
        );
        assert!(
            perform_eval(&mut agent, &JsString::from_utf8("with ({}) {}"), true, true,).is_err()
        );
        // The same code in a sloppy caller parses and runs.
        assert_eq!(
            perform_eval(
                &mut agent,
                &JsString::from_utf8("var public = 2; public"),
                false,
                true
            )
            .unwrap(),
            Value::Number(2.0)
        );
    }

    #[test]
    fn top_level_using_in_eval_is_rejected() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        assert!(
            perform_eval(
                &mut agent,
                &JsString::from_utf8("using x = null;"),
                false,
                true
            )
            .is_err()
        );
        // A block-nested `using` is fine.
        assert_eq!(
            perform_eval(
                &mut agent,
                &JsString::from_utf8("{ using x = null; } 5"),
                false,
                true
            )
            .unwrap(),
            Value::Number(5.0)
        );
    }
}
