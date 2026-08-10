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
use crate::env::EnvRef;
use crate::realm::Realm;

/// A Script Record (spec 16.1.4): the realm the script runs in and its
/// parsed ECMAScript code. [[LoadedModules]] joins with module linking
/// (Phase 7); [[HostDefined]] is host-specific.
#[derive(Debug, Clone)]
pub struct ScriptRecord {
    pub realm: Handle<Realm>,
    pub code: Program,
}

/// ParseScript (spec 16.1.5): parse `source` as a Script and wrap it in a
/// Script Record. Early errors surface here as a SyntaxError.
pub fn parse_script(source: &str, realm: Handle<Realm>) -> Result<Handle<ScriptRecord>, JsError> {
    let code = parser::parse_script(source)?;
    Ok(Handle::new(ScriptRecord { realm, code }))
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
        script_or_module: Some(script.clone()),
        lexical_environment: global_env.clone(),
        variable_environment: global_env.clone(),
        private_environment: None,
    };
    agent.execution_context_stack.push(context);

    let strict = script_is_strict(&script.code);
    let result = (|| -> Result<Value, JsError> {
        global_declaration_instantiation(&script.code, &global_env)?;
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

/// Instantiate a top-level function declaration: Phase 4 creates a function
/// value carrying its name; [[Environment]]/[[ECMAScriptCode]] join in
/// Phase 7.
pub fn instantiate_function_object(f: &syntax::ast::Function) -> Value {
    Value::Function(Handle::new(crux::Function::new(f.name.map(lookup))))
}

/// GlobalDeclarationInstantiation (spec 16.1.7): create the script's global
/// bindings — lexical declarations in the declarative record, functions and
/// vars as properties of the global object — checking for redeclarations and
/// restricted globals first.
pub fn global_declaration_instantiation(
    program: &Program,
    global_env: &EnvRef,
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
        let func_obj = instantiate_function_object(f);
        global_env.create_global_function_binding(&name, func_obj, false)?;
    }

    for name in declared_variable_names {
        global_env.create_global_var_binding(&name, false)?;
    }

    Ok(())
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
}
