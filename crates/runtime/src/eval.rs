//! The minimal evaluator for Phase 4 (spec ch. 13-14 evaluation surface).
//!
//! Phase 4 needs just enough evaluation for its exit criteria and binding
//! tests: literals, identifiers, expression statements, `var`/`let`/`const`
//! declarations, function/class declarations, and blocks. The full
//! expression and statement evaluation is Phase 6.

use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::value::Value;
use syntax::ast::{BindingPattern, Expr, ExprKind, Literal, Program, Stmt, StmtKind, VarDeclKind};

use crate::agent::Agent;
use crate::context::{get_value, initialize_referenced_binding, put_value, resolve_binding};
use crate::env::{EnvRef, new_declarative_environment};
use crate::script::{bound_names, instantiate_function_object};

/// Evaluate a whole program (the Evaluation of |Script|, spec 16.1.6 step
/// 11). The completion value is the value of the last evaluated statement;
/// scripts whose statements produce ~empty~ completions yield *undefined*.
pub fn eval_program(agent: &mut Agent, program: &Program, strict: bool) -> Result<Value, JsError> {
    eval_statement_list(agent, &program.body, strict)
}

fn eval_statement_list(agent: &mut Agent, stmts: &[Stmt], strict: bool) -> Result<Value, JsError> {
    let mut value = Value::Undefined;
    for stmt in stmts {
        value = eval_statement(agent, stmt, strict)?;
    }
    Ok(value)
}

fn eval_statement(agent: &mut Agent, stmt: &Stmt, strict: bool) -> Result<Value, JsError> {
    match &stmt.kind {
        StmtKind::Empty | StmtKind::Debugger => Ok(Value::Undefined),
        StmtKind::Expr(expr) => eval_expr(agent, expr, strict),
        StmtKind::VarDecl { kind, decls } => match kind {
            VarDeclKind::Var => eval_var_declarations(agent, decls, strict),
            VarDeclKind::Let | VarDeclKind::Const => {
                eval_lexical_declarations(agent, decls, strict)
            }
            // `using` evaluation (disposal semantics) is Phase 15; the
            // bindings themselves are instantiated like `let`.
            VarDeclKind::Using | VarDeclKind::AwaitUsing => {
                eval_lexical_declarations(agent, decls, strict)
            }
        },
        StmtKind::UsingDecl { decls, .. } => eval_lexical_declarations(agent, decls, strict),
        StmtKind::FunctionDecl(f) => eval_function_declaration(agent, f, strict),
        StmtKind::ClassDecl(class) => eval_class_declaration(agent, class, strict),
        StmtKind::Block(block) => eval_block(agent, block, strict),
        _ => Err(not_implemented("statement evaluation")),
    }
}

/// VariableStatement evaluation (spec 14.3.2): resolve each binding and
/// PutValue the initializer. The bindings themselves were created by
/// declaration instantiation.
fn eval_var_declarations(
    agent: &mut Agent,
    decls: &[syntax::ast::VarDeclarator],
    strict: bool,
) -> Result<Value, JsError> {
    for decl in decls {
        if let Some(init) = &decl.init {
            let BindingPattern::Ident(name) = &decl.pattern else {
                return Err(not_implemented("destructuring variable declarations"));
            };
            let value = eval_expr(agent, init, strict)?;
            let name = crux::lookup(*name);
            let reference = resolve_binding(agent, &name, strict)?;
            put_value(agent, &reference, value)?;
        }
    }
    Ok(Value::Undefined)
}

/// LexicalDeclaration evaluation (spec 14.2.2): evaluate each initializer
/// and InitializeReferencedBinding; bindings without initializers become
/// *undefined*.
fn eval_lexical_declarations(
    agent: &mut Agent,
    decls: &[syntax::ast::VarDeclarator],
    strict: bool,
) -> Result<Value, JsError> {
    for decl in decls {
        let BindingPattern::Ident(name) = &decl.pattern else {
            return Err(not_implemented("destructuring lexical declarations"));
        };
        let value = match &decl.init {
            Some(init) => eval_expr(agent, init, strict)?,
            None => Value::Undefined,
        };
        let name = crux::lookup(*name);
        let reference = resolve_binding(agent, &name, strict)?;
        initialize_referenced_binding(&reference, value)?;
    }
    Ok(Value::Undefined)
}

/// FunctionDeclaration evaluation (spec 15.2.6): instantiate the function
/// object and bind it in the VariableEnvironment.
fn eval_function_declaration(
    agent: &mut Agent,
    f: &syntax::ast::Function,
    _strict: bool,
) -> Result<Value, JsError> {
    let Some(name) = f.name else {
        return Ok(Value::Undefined);
    };
    let func_obj = instantiate_function_object(f);
    let env = agent.running_context()?.variable_environment.clone();
    let name = crux::lookup(name);
    env.set_mutable_binding(&name, func_obj, false)?;
    Ok(Value::Undefined)
}

/// ClassDeclaration evaluation: the binding was created (uninitialized) by
/// declaration instantiation; Phase 4 binds a function-shaped value, and
/// Phase 7 performs ClassDefinitionEvaluation.
fn eval_class_declaration(
    agent: &mut Agent,
    class: &syntax::ast::Class,
    strict: bool,
) -> Result<Value, JsError> {
    let Some(name) = class.name else {
        return Err(JsError::new(
            ErrorKind::SyntaxError,
            "Class declarations require a name".into(),
        ));
    };
    let name = crux::lookup(name);
    let class_value = Value::Function(Handle::new(crux::Function::new(Some(name.clone()))));
    let reference = resolve_binding(agent, &name, strict)?;
    initialize_referenced_binding(&reference, class_value)?;
    Ok(Value::Undefined)
}

/// Block evaluation (spec 14.2.3): a fresh declarative lexical environment
/// for the block's declarations, instantiated before the statements run.
fn eval_block(
    agent: &mut Agent,
    block: &syntax::ast::Block,
    strict: bool,
) -> Result<Value, JsError> {
    let old_env = agent.running_context()?.lexical_environment.clone();
    let block_env = new_declarative_environment(Some(old_env.clone()));
    block_declaration_instantiation(&block.stmts, &block_env)?;
    agent.running_context_mut()?.lexical_environment = block_env.clone();
    let result = eval_statement_list(agent, &block.stmts, strict);
    agent.running_context_mut()?.lexical_environment = old_env;
    result
}

/// BlockDeclarationInstantiation (spec 14.2.4): create the block's lexical
/// bindings uninitialized; block-level function declarations are instantiated
/// and initialized immediately.
fn block_declaration_instantiation(stmts: &[Stmt], block_env: &EnvRef) -> Result<(), JsError> {
    for stmt in stmts {
        match &stmt.kind {
            StmtKind::VarDecl { kind, decls, .. } if *kind != VarDeclKind::Var => {
                for decl in decls {
                    let mut names = Vec::new();
                    bound_names(&decl.pattern, &mut names);
                    for name in names {
                        block_env.create_mutable_binding(&name, false)?;
                    }
                }
            }
            StmtKind::UsingDecl { decls, .. } => {
                for decl in decls {
                    let mut names = Vec::new();
                    bound_names(&decl.pattern, &mut names);
                    for name in names {
                        block_env.create_mutable_binding(&name, false)?;
                    }
                }
            }
            StmtKind::ClassDecl(class) => {
                if let Some(name) = class.name {
                    let name = crux::lookup(name);
                    block_env.create_mutable_binding(&name, false)?;
                }
            }
            StmtKind::FunctionDecl(f) => {
                if let Some(name) = f.name {
                    let name = crux::lookup(name);
                    block_env.create_mutable_binding(&name, false)?;
                    let func_obj = instantiate_function_object(f);
                    block_env.initialize_binding(&name, func_obj)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn eval_expr(agent: &mut Agent, expr: &Expr, strict: bool) -> Result<Value, JsError> {
    match &expr.kind {
        ExprKind::Literal(literal) => eval_literal(literal),
        ExprKind::Ident(name) => {
            let name = crux::lookup(*name);
            let reference = resolve_binding(agent, &name, strict)?;
            get_value(&reference)
        }
        ExprKind::Assign {
            op: syntax::ast::AssignOp::Assign,
            target,
            value,
        } => {
            // AssignmentExpression : LeftHandSideExpression = AssignmentExpression
            // (spec 13.15.1) restricted to identifier targets; member and
            // pattern targets join with Phase 6.
            let ExprKind::Ident(name) = &target.kind else {
                return Err(not_implemented("assignment targets"));
            };
            let value = eval_expr(agent, value, strict)?;
            let name = crux::lookup(*name);
            let reference = resolve_binding(agent, &name, strict)?;
            put_value(agent, &reference, value.clone())?;
            Ok(value)
        }
        ExprKind::Paren(inner) => eval_expr(agent, inner, strict),
        _ => Err(not_implemented("expression evaluation")),
    }
}

fn eval_literal(literal: &Literal) -> Result<Value, JsError> {
    match literal {
        Literal::Null => Ok(Value::Null),
        Literal::Boolean(b) => Ok(Value::Boolean(*b)),
        Literal::Number(n) => Ok(Value::Number(*n)),
        Literal::BigInt(n) => Ok(Value::BigInt(Handle::new(n.clone()))),
        Literal::Str(s) => Ok(Value::String(Handle::new(s.clone()))),
        Literal::RegExp { .. } => Err(JsError::new(
            ErrorKind::TypeError,
            "Regular expression literals are not implemented until Phase 11".into(),
        )),
    }
}

fn not_implemented(what: &str) -> JsError {
    JsError::new(
        ErrorKind::TypeError,
        format!("{what} is not implemented until Phase 6"),
    )
}

/// Call (spec 7.3.13): invoke a callable value with a this value and an
/// argument list. Phase 4 function values carry no [[Call]] body, so any
/// invocation reports the pending Phase 7 capability.
pub(crate) fn call(callee: &Value, _this: Value, _args: &[Value]) -> Result<Value, JsError> {
    match callee {
        Value::Function(_) => Err(JsError::new(
            ErrorKind::TypeError,
            "calling functions is not implemented until Phase 7".into(),
        )),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "value is not a function".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, evaluate};
    use crux::string::JsString;

    fn run(source: &str) -> Result<Value, JsError> {
        evaluate(source)
    }

    #[test]
    fn evaluates_a_trivial_script_to_a_value() {
        // Exit criterion: a trivial hardcoded AST evaluates to a value.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let realm = agent.current_realm().unwrap();
        let program = syntax::ast::Program {
            body: vec![Stmt {
                span: crux::Span::new(0, 0),
                kind: StmtKind::Expr(Expr {
                    span: crux::Span::new(0, 0),
                    kind: ExprKind::Literal(Literal::Number(42.0)),
                }),
            }],
            span: crux::Span::new(0, 0),
        };
        let script = Handle::new(crate::script::ScriptRecord {
            realm,
            code: program,
        });
        let value = crate::script::script_evaluation(&mut agent, &script).unwrap();
        assert_eq!(value, Value::Number(42.0));
    }

    #[test]
    fn literal_kinds_produce_values() {
        assert_eq!(run("null;").unwrap(), Value::Null);
        assert_eq!(run("true;").unwrap(), Value::Boolean(true));
        assert_eq!(run("1.5;").unwrap(), Value::Number(1.5));
        assert_eq!(
            run("\"hi\";").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("hi")))
        );
        assert!(matches!(run("1n;").unwrap(), Value::BigInt(_)));
    }

    #[test]
    fn var_declarations_hoist_and_initialize() {
        // The var binding exists (undefined) before its initializer runs.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent.run_script("var x;").unwrap();
        let global = agent.running_context().unwrap().realm.global_object.clone();
        assert_eq!(
            global.get(&JsString::from_utf8("x")).unwrap(),
            Value::Undefined
        );

        run("var y = 7;").unwrap();
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent.run_script("var y = 7;").unwrap();
        let global = agent.running_context().unwrap().realm.global_object.clone();
        assert_eq!(
            global.get(&JsString::from_utf8("y")).unwrap(),
            Value::Number(7.0)
        );
    }

    #[test]
    fn var_inside_blocks_is_global() {
        run("{ var z = 3; }").unwrap();
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent.run_script("{ var z = 3; }").unwrap();
        let global = agent.running_context().unwrap().realm.global_object.clone();
        assert_eq!(
            global.get(&JsString::from_utf8("z")).unwrap(),
            Value::Number(3.0)
        );
    }

    #[test]
    fn lexical_bindings_are_tdz_and_shadow() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        // A let binding is visible (but uninitialized) before its declaration.
        let err = agent.run_script("x; let x = 1;").unwrap_err();
        assert_eq!(err.kind, ErrorKind::ReferenceError);
    }

    #[test]
    fn const_bindings_reject_assignment() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let err = agent.run_script("const c = 1; c = 2;").unwrap_err();
        assert_eq!(err.kind, ErrorKind::TypeError);
    }

    #[test]
    fn sloppy_assignment_creates_globals_strict_throws() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent.run_script("undeclared = 5;").unwrap();
        let global = agent.running_context().unwrap().realm.global_object.clone();
        assert_eq!(
            global.get(&JsString::from_utf8("undeclared")).unwrap(),
            Value::Number(5.0)
        );

        let mut strict_agent = Agent::new();
        strict_agent.initialize_host_defined_realm().unwrap();
        let err = strict_agent
            .run_script("'use strict'; undeclared = 5;")
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ReferenceError);
    }

    #[test]
    fn function_declarations_bind_to_the_global_object() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent.run_script("function f() {}").unwrap();
        let global = agent.running_context().unwrap().realm.global_object.clone();
        let value = global.get(&JsString::from_utf8("f")).unwrap();
        assert!(matches!(value, Value::Function(_)));
    }

    #[test]
    fn class_declarations_create_lexical_bindings() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        // Class bindings are lexical: a var with the same name is an error.
        assert!(agent.run_script("var C; class C {}").is_err());
        assert!(agent.run_script("let C; class C {}").is_err());
        // And the binding initializes to a function-shaped value.
        agent.run_script("class C {}").unwrap();
        let realm = agent.current_realm().unwrap();
        let value = realm
            .global_env
            .get_binding_value(&JsString::from_utf8("C"), true)
            .unwrap();
        assert!(matches!(value, Value::Function(_)));
    }

    #[test]
    fn let_shadowing_var_at_global_scope_is_an_error() {
        // GDI: a var name cannot collide with a lexical declaration.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        assert!(agent.run_script("var x; let x;").is_err());
        assert!(agent.run_script("let x; var x;").is_err());
        // Two lets are also an error.
        assert!(agent.run_script("let x; let x;").is_err());
    }

    #[test]
    fn restricted_global_properties_block_lexical_declarations() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        // Infinity/NaN/undefined are non-configurable globals.
        assert!(agent.run_script("let NaN;").is_err());
        assert!(agent.run_script("const undefined = 1;").is_err());
        // ...but a var is fine (it reuses the existing property).
        agent.run_script("var NaN;").unwrap();
    }

    #[test]
    fn undefined_identifier_is_a_reference_error() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let err = agent.run_script("missing;").unwrap_err();
        assert_eq!(err.kind, ErrorKind::ReferenceError);
    }
}
