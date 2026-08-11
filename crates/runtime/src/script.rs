//! Script Records (spec 16.1.4) and their algorithms: ParseScript,
//! ScriptEvaluation, and GlobalDeclarationInstantiation, plus the
//! declaration SDOs they consume (LexicallyDeclaredNames, VarDeclaredNames,
//! VarScopedDeclarations, LexicallyScopedDeclarations, BoundNames,
//! IsConstantDeclaration, ScriptIsStrict).

use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::string::{JsString, lookup};
use crux::value::Value;
use syntax::ast::{
    ArrayBindingElement, BindingPattern, ForBinding, ForInit, ObjectBindingProperty, Program, Stmt,
    StmtKind, VarDeclKind,
};

use crate::agent::Agent;
use crate::context::ExecutionContext;
use crate::env::{EnvRecord, EnvRef, new_declarative_environment};
use crate::realm::Realm;

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

/// ParseScript (spec 16.1.5): parse `source` as a Script and wrap it in a
/// Script Record. Early errors surface here as a SyntaxError.
pub fn parse_script(source: &str, realm: Handle<Realm>) -> Result<Handle<ScriptRecord>, JsError> {
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
        realm: script.realm.clone(),
        script_or_module: Some(crate::context::ScriptOrModule::Script(script.clone())),
        lexical_environment: global_env.clone(),
        variable_environment: global_env.clone(),
        private_environment: None,
        source: Some(script.source.clone()),
    };
    agent.execution_context_stack.push(context);

    let strict = script_is_strict(&script.code);
    let result = (|| -> Result<Value, JsError> {
        global_declaration_instantiation(agent, &script.code, &global_env, strict)?;
        crate::eval::eval_program(agent, &script.code, strict)
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

/// ScriptIsStrict (spec 16.1.2): a directive prologue `"use strict"`.
pub fn script_is_strict(program: &Program) -> bool {
    for stmt in &program.body {
        let StmtKind::Expr(expr) = &stmt.kind else {
            return false;
        };
        let syntax::ast::ExprKind::Literal(syntax::ast::Literal::Str(value)) = &expr.kind else {
            return false;
        };
        if value.to_string_lossy() == "use strict" {
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
            if let Some(ForInit::VarDecl { decls, .. }) = init {
                for decl in decls {
                    let mut names = Vec::new();
                    bound_names(&decl.pattern, &mut names);
                    out.push(VarScopedDecl::Variable(names));
                }
            }
            var_scoped_declarations(body, out);
        }
        StmtKind::ForIn { left, body, .. } | StmtKind::ForOf { left, body, .. } => {
            if let ForBinding::VarDecl { pattern, .. } = left {
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
        let func_obj = crate::function::instantiate_function(agent, f, global_env.clone(), strict)?;
        global_env.create_global_function_binding(&name, func_obj, false)?;
    }

    for name in declared_variable_names {
        global_env.create_global_var_binding(&name, false)?;
    }

    Ok(())
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
    source: &str,
    strict_caller: bool,
    direct: bool,
) -> Result<Value, JsError> {
    let eval_realm = agent.current_realm()?;
    // The caller-context checks of spec step 5-7 (inFunc, inMethod,
    // inDerivedCtor, inClassFieldInitializer) are subsumed by the Script
    // early errors the parser applies: Phase 4 code never runs inside a
    // function, and the parser rejects new.target/super/arguments in
    // scripts. Phase 7 wires the function-environment flags.

    // HostEnsureCanCompileStrings (spec 19.2.1.1 step 4).
    let body_string = JsString::from_utf8(source);
    if let Some(hooks) = &agent.host_hooks {
        hooks.ensure_can_compile_strings(&eval_realm, &[], &body_string, direct)?;
    }

    let program = parser::parse_script(source)?;
    // A script with no body evaluates to undefined.
    if program.body.is_empty() {
        return Ok(Value::Undefined);
    }
    let strict_eval = strict_caller || script_is_strict(&program);

    let running = agent.running_context()?;
    let (lexical_env, variable_env, private_env) = if direct {
        // Direct eval: a fresh lexical env over the caller's, sharing the
        // caller's variable environment (spec 19.2.1.1 step 12).
        let lexical_env = new_declarative_environment(Some(running.lexical_environment.clone()));
        (
            lexical_env,
            running.variable_environment.clone(),
            running.private_environment.clone(),
        )
    } else {
        // Indirect eval: fresh lexical env over the global environment.
        let global_env = eval_realm.global_env();
        (
            new_declarative_environment(Some(global_env.clone())),
            global_env,
            None,
        )
    };
    // Strict eval code cannot touch the caller's variable environment.
    let variable_env = if strict_eval {
        lexical_env.clone()
    } else {
        variable_env
    };

    let script_or_module = running.script_or_module.clone();
    let eval_source = JsString::from_utf8(source);
    let eval_context = ExecutionContext {
        function: None,
        realm: eval_realm,
        script_or_module,
        lexical_environment: lexical_env.clone(),
        variable_environment: variable_env.clone(),
        private_environment: private_env,
        source: Some(eval_source),
    };
    agent.execution_context_stack.push(eval_context);
    let result = (|| -> Result<Value, JsError> {
        eval_declaration_instantiation(agent, &program, &variable_env, &lexical_env, strict_eval)?;
        crate::eval::eval_program(agent, &program, strict_eval)
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
        }
        // Walk from the eval's lexical env up to (but not including) the
        // variable environment, rejecting vars that would hoist over a
        // lexical binding (spec 19.2.1.4 steps 3-10).
        let mut this_env = Some(lexical_env.clone());
        while let Some(env) = this_env {
            if Handle::ptr_eq(&env, variable_env) {
                break;
            }
            if !matches!(&*env, EnvRecord::Object(_)) {
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
        let env = agent.running_context()?.lexical_environment.clone();
        let func_obj = crate::function::instantiate_function(agent, f, env, strict)?;
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

    for name in declared_variable_names {
        if variable_env_is_global {
            // Eval-created global vars are deletable.
            variable_env.create_global_var_binding(&name, true)?;
        } else if !variable_env.has_binding(&name)? {
            variable_env.create_mutable_binding(&name, true)?;
            variable_env.initialize_binding(&name, Value::Undefined)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crux::string::intern_utf8;

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
        assert!(script_is_strict(&parse("'use strict'; 1;")));
        assert!(!script_is_strict(&parse("1;")));
        // The directive must be first.
        assert!(!script_is_strict(&parse("1; 'use strict';")));
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
        perform_eval(&mut agent, source, false, true)
    }

    #[test]
    fn empty_eval_returns_undefined() {
        assert_eq!(evaluated("").unwrap(), Value::Undefined);
    }

    #[test]
    fn direct_eval_binds_vars_in_the_caller_variable_environment() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let result = perform_eval(&mut agent, "var ev = 5; ev", false, true).unwrap();
        assert_eq!(result, Value::Number(5.0));
        // The var landed on the global object, deletable (eval-created).
        let global = agent.running_context().unwrap().realm.global_object.clone();
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
        let result = perform_eval(&mut agent, "var gv = 7; gv", false, false).unwrap();
        assert_eq!(result, Value::Number(7.0));
        let global = agent.running_context().unwrap().realm.global_object.clone();
        assert_eq!(
            global.get(&JsString::from_utf8("gv")).unwrap(),
            Value::Number(7.0)
        );
    }

    #[test]
    fn strict_eval_isolates_var_declarations() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let result = perform_eval(&mut agent, "'use strict'; var s = 1; s", false, false).unwrap();
        assert_eq!(result, Value::Number(1.0));
        // Strict eval's vars go to the fresh lexical env, not the global.
        let global = agent.running_context().unwrap().realm.global_object.clone();
        assert!(!global.has_own_property(&JsString::from_utf8("s")).unwrap());
    }

    #[test]
    fn eval_lexical_declarations_stay_local() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let result = perform_eval(&mut agent, "let lx = 3; lx", false, false).unwrap();
        assert_eq!(result, Value::Number(3.0));
        let realm = agent.running_context().unwrap().realm.clone();
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
        assert!(perform_eval(&mut agent, "var x;", false, true).is_err());
        // And a like-named var in a *strict* eval is fine (separate env).
        assert!(perform_eval(&mut agent, "'use strict'; var x;", false, true).is_ok());
    }

    #[test]
    fn eval_nests_on_the_execution_context_stack() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        assert_eq!(agent.execution_context_stack.len(), 1);
        let result = perform_eval(&mut agent, "var nested = 1; nested", false, true).unwrap();
        assert_eq!(result, Value::Number(1.0));
        // The eval context was pushed and popped.
        assert_eq!(agent.execution_context_stack.len(), 1);
    }

    #[test]
    fn eval_runs_inside_jobs() {
        let mut agent = Agent::new();
        let realm = agent.initialize_host_defined_realm().unwrap();
        agent.enqueue_generic_job(Some(realm), move |agent| {
            let result = perform_eval(agent, "var from_job = 2; from_job", false, true)?;
            assert_eq!(result, Value::Number(2.0));
            assert_eq!(agent.execution_context_stack.len(), 1);
            Ok(Value::Undefined)
        });
        agent.run_jobs().unwrap();
        let global = agent.running_context().unwrap().realm.global_object.clone();
        assert_eq!(
            global.get(&JsString::from_utf8("from_job")).unwrap(),
            Value::Number(2.0)
        );
    }

    #[test]
    fn eval_function_declarations_bind_to_the_variable_environment() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        perform_eval(&mut agent, "function ef() {}", false, true).unwrap();
        let global = agent.running_context().unwrap().realm.global_object.clone();
        assert!(matches!(
            global.get(&JsString::from_utf8("ef")).unwrap(),
            Value::Function(_)
        ));
    }
}
