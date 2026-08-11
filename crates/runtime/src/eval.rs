//! Statement evaluation (spec ch. 14) with the Completion model
//! (spec 6.2.3): normal completions carry the value, `break`/`continue`
//! transfer control within loops and labels, `return` exits functions, and
//! `throw` carries the thrown language value. Internal `JsError`s propagate
//! through `Result` and are caught by `try`/`catch`.

use std::collections::HashSet;

use crux::convert::to_boolean;
use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::property::PropertyKey;
use crux::string::JsString;
use crux::value::Value;
use syntax::ast::{BindingPattern, ForBinding, ForInit, Program, Stmt, StmtKind, VarDeclKind};

use crate::agent::Agent;
use crate::context::{initialize_referenced_binding, put_value, resolve_binding};
use crate::env::{EnvRef, new_declarative_environment, new_object_environment};
use crate::expr::{eval_expr, eval_reference, get_iterator, iterator_close, iterator_step};
use crate::flow::{Completion, completion_to_result};
use crate::script::bound_names;

/// Evaluate a whole program (the Evaluation of |Script|, spec 16.1.6 step
/// 11). The completion value is the value of the last evaluated statement.
pub fn eval_program(agent: &mut Agent, program: &Program, strict: bool) -> Result<Value, JsError> {
    let completion = eval_statement_list(agent, &program.body, strict)?;
    completion_to_result(completion)
}

pub(crate) fn eval_statement_list(
    agent: &mut Agent,
    stmts: &[Stmt],
    strict: bool,
) -> Result<Completion, JsError> {
    let mut list_value = Value::Undefined;
    let mut completion = Completion::normal();
    for (index, stmt) in stmts.iter().enumerate() {
        completion = eval_statement(agent, stmt, strict)?;
        match &mut completion {
            Completion::Normal(value) => list_value = value.clone(),
            // UpdateEmpty (spec 14.2.2 step 5): an abrupt statement inherits
            // the preceding statement list's value.
            Completion::Break { value, .. } | Completion::Continue { value, .. }
                if index > 0 && value.is_none() =>
            {
                *value = Some(list_value.clone());
            }
            // A declaration/empty statement has an ~empty~ value; the list
            // fills it from the preceding statements.
            Completion::Empty => {
                completion = Completion::Normal(list_value.clone());
            }
            _ => {}
        }
        if !matches!(completion, Completion::Normal(_)) {
            break;
        }
    }
    Ok(completion)
}

fn eval_statement(agent: &mut Agent, stmt: &Stmt, strict: bool) -> Result<Completion, JsError> {
    match &stmt.kind {
        StmtKind::Empty | StmtKind::Debugger => Ok(Completion::Empty),
        StmtKind::Expr(expr) => eval_expr(agent, expr, strict).map(Completion::Normal),
        StmtKind::VarDecl { kind, decls } => {
            match kind {
                VarDeclKind::Var => eval_var_declarations(agent, decls, strict)?,
                VarDeclKind::Let
                | VarDeclKind::Const
                | VarDeclKind::Using
                | VarDeclKind::AwaitUsing => {
                    eval_lexical_declarations(agent, decls, strict)?;
                }
            }
            Ok(Completion::Empty)
        }
        StmtKind::UsingDecl { decls, .. } => {
            eval_lexical_declarations(agent, decls, strict)?;
            Ok(Completion::Empty)
        }
        StmtKind::FunctionDecl(f) => {
            eval_function_declaration(agent, f, strict)?;
            Ok(Completion::Empty)
        }
        StmtKind::ClassDecl(class) => {
            eval_class_declaration(agent, class, strict)?;
            Ok(Completion::Empty)
        }
        StmtKind::Block(block) => eval_block(agent, block, strict),
        StmtKind::If {
            test,
            consequent,
            alternate,
        } => {
            let test_value = eval_expr(agent, test, strict)?;
            let completion = if to_boolean(&test_value) {
                eval_statement(agent, consequent, strict)?
            } else if let Some(alternate) = alternate {
                eval_statement(agent, alternate, strict)?
            } else {
                Completion::normal()
            };
            // spec 14.10.2 step 5: UpdateEmpty with *undefined*.
            Ok(completion.update_empty(Value::Undefined))
        }
        StmtKind::While { test, body } => eval_while(agent, test, body, strict, &[]),
        StmtKind::DoWhile { body, test } => eval_do_while(agent, body, test, strict, &[]),
        StmtKind::For {
            init,
            test,
            update,
            body,
        } => eval_for(
            agent,
            init.as_ref(),
            test.as_ref(),
            update.as_ref(),
            body,
            strict,
            &[],
        ),
        StmtKind::ForIn { left, right, body } => eval_for_in(agent, left, right, body, strict, &[]),
        StmtKind::ForOf {
            left,
            right,
            body,
            is_await,
        } => {
            if *is_await {
                return Err(not_implemented("for await"));
            }
            eval_for_of(agent, left, right, body, strict, &[])
        }
        StmtKind::Labeled { label, body } => eval_labeled(agent, *label, body, strict),
        StmtKind::Break(label) => Ok(Completion::Break {
            target: *label,
            value: None,
        }),
        StmtKind::Continue(label) => Ok(Completion::Continue {
            target: *label,
            value: None,
        }),
        StmtKind::Return(expr) => {
            let value = match expr {
                Some(expr) => eval_expr(agent, expr, strict)?,
                None => Value::Undefined,
            };
            Ok(Completion::Return(value))
        }
        StmtKind::Throw(expr) => {
            let value = eval_expr(agent, expr, strict)?;
            Ok(Completion::Throw(value))
        }
        StmtKind::Try {
            block,
            handler,
            finalizer,
        } => eval_try(agent, block, handler.as_ref(), finalizer.as_ref(), strict),
        StmtKind::Switch {
            discriminant,
            cases,
        } => eval_switch(agent, discriminant, cases, strict),
        StmtKind::With { object, body } => eval_with(agent, object, body, strict),
    }
}

/// VariableStatement evaluation (spec 14.3.2): resolve each binding and
/// PutValue the initializer. The bindings themselves were created by
/// declaration instantiation.
fn eval_var_declarations(
    agent: &mut Agent,
    decls: &[syntax::ast::VarDeclarator],
    strict: bool,
) -> Result<(), JsError> {
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
    Ok(())
}

/// LexicalDeclaration evaluation (spec 14.2.2): evaluate each initializer
/// and InitializeReferencedBinding; bindings without initializers become
/// *undefined*.
fn eval_lexical_declarations(
    agent: &mut Agent,
    decls: &[syntax::ast::VarDeclarator],
    strict: bool,
) -> Result<(), JsError> {
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
    Ok(())
}

/// FunctionDeclaration evaluation (spec 15.2.6): instantiate the function
/// object against the current lexical environment and bind it in the
/// VariableEnvironment.
fn eval_function_declaration(
    agent: &mut Agent,
    f: &syntax::ast::Function,
    strict: bool,
) -> Result<(), JsError> {
    let Some(name) = f.name else {
        return Ok(());
    };
    let env = agent.running_context()?.lexical_environment.clone();
    let func_obj = crate::function::instantiate_function(agent, f, env, strict)?;
    let env = agent.running_context()?.variable_environment.clone();
    let name = crux::lookup(name);
    env.set_mutable_binding(&name, func_obj, false)?;
    Ok(())
}

/// ClassDeclaration evaluation: the binding was created (uninitialized) by
/// declaration instantiation; Phase 4 binds a function-shaped value, and
/// Phase 7 performs ClassDefinitionEvaluation.
fn eval_class_declaration(
    agent: &mut Agent,
    class: &syntax::ast::Class,
    strict: bool,
) -> Result<(), JsError> {
    let Some(name) = class.name else {
        return Err(JsError::new(
            ErrorKind::SyntaxError,
            "Class declarations require a name".into(),
        ));
    };
    let name = crux::lookup(name);
    let class_value = Value::Function(crux::Function::new(Some(name.clone())));
    let reference = resolve_binding(agent, &name, strict)?;
    initialize_referenced_binding(&reference, class_value)?;
    Ok(())
}

/// Block evaluation (spec 14.2.3): a fresh declarative lexical environment
/// for the block's declarations, instantiated before the statements run.
fn eval_block(
    agent: &mut Agent,
    block: &syntax::ast::Block,
    strict: bool,
) -> Result<Completion, JsError> {
    let old_env = agent.running_context()?.lexical_environment.clone();
    let block_env = new_declarative_environment(Some(old_env.clone()));
    block_declaration_instantiation(agent, &block.stmts, &block_env, strict)?;
    agent.running_context_mut()?.lexical_environment = block_env.clone();
    let result = eval_statement_list(agent, &block.stmts, strict);
    agent.running_context_mut()?.lexical_environment = old_env;
    result
}

/// BlockDeclarationInstantiation (spec 14.2.4): create the block's lexical
/// bindings uninitialized; block-level function declarations are instantiated
/// and initialized immediately.
fn block_declaration_instantiation(
    agent: &mut Agent,
    stmts: &[Stmt],
    block_env: &EnvRef,
    strict: bool,
) -> Result<(), JsError> {
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
                    let func_obj =
                        crate::function::instantiate_function(agent, f, block_env.clone(), strict)?;
                    block_env.initialize_binding(&name, func_obj)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn eval_while(
    agent: &mut Agent,
    test: &syntax::ast::Expr,
    body: &Stmt,
    strict: bool,
    labels: &[crux::string::AtomId],
) -> Result<Completion, JsError> {
    let mut iteration_result = Value::Undefined;
    loop {
        let test_value = eval_expr(agent, test, strict)?;
        if !to_boolean(&test_value) {
            return Ok(Completion::Normal(iteration_result));
        }
        match eval_statement(agent, body, strict)? {
            Completion::Normal(value) => iteration_result = value,
            Completion::Empty => {}
            Completion::Continue { target, value }
                if target.is_none() || target.is_some_and(|l| labels.contains(&l)) =>
            {
                if let Some(value) = value {
                    iteration_result = value;
                }
            }
            // The loop consumes an unlabeled break as a normal completion
            // carrying the iteration value (spec 14.14.2 step 2), and hands
            // a matching labeled break up with its value filled.
            Completion::Break {
                target: None,
                value,
            } => {
                return Ok(Completion::Normal(value.unwrap_or(iteration_result)));
            }
            Completion::Break {
                target: Some(l),
                value,
            } if labels.contains(&l) => {
                return Ok(Completion::Break {
                    target: Some(l),
                    value: Some(value.unwrap_or(iteration_result)),
                });
            }
            other => return Ok(other.update_empty(iteration_result.clone())),
        }
    }
}

fn eval_do_while(
    agent: &mut Agent,
    body: &Stmt,
    test: &syntax::ast::Expr,
    strict: bool,
    labels: &[crux::string::AtomId],
) -> Result<Completion, JsError> {
    let mut iteration_result = Value::Undefined;
    loop {
        match eval_statement(agent, body, strict)? {
            Completion::Normal(value) => iteration_result = value,
            Completion::Empty => {}
            Completion::Continue { target, value }
                if target.is_none() || target.is_some_and(|l| labels.contains(&l)) =>
            {
                if let Some(value) = value {
                    iteration_result = value;
                }
            }
            Completion::Break {
                target: None,
                value,
            } => {
                return Ok(Completion::Normal(value.unwrap_or(iteration_result)));
            }
            Completion::Break {
                target: Some(l),
                value,
            } if labels.contains(&l) => {
                return Ok(Completion::Break {
                    target: Some(l),
                    value: Some(value.unwrap_or(iteration_result)),
                });
            }
            other => return Ok(other.update_empty(iteration_result.clone())),
        }
        let test_value = eval_expr(agent, test, strict)?;
        if !to_boolean(&test_value) {
            return Ok(Completion::Normal(iteration_result));
        }
    }
}

/// ForStatement evaluation (spec 14.7.4): `let`/`const` heads bind in a
/// fresh loop environment (per-iteration copies for closures join Phase 7).
fn eval_for(
    agent: &mut Agent,
    init: Option<&ForInit>,
    test: Option<&syntax::ast::Expr>,
    update: Option<&syntax::ast::Expr>,
    body: &Stmt,
    strict: bool,
    labels: &[crux::string::AtomId],
) -> Result<Completion, JsError> {
    let old_env = agent.running_context()?.lexical_environment.clone();
    let mut fresh_env: Option<EnvRef> = None;
    match init {
        None => {}
        Some(ForInit::Expr(expr)) => {
            eval_expr(agent, expr, strict)?;
        }
        Some(ForInit::VarDecl { kind, decls }) => {
            if *kind == VarDeclKind::Var {
                eval_var_declarations(agent, decls, strict)?;
            } else {
                let env = new_declarative_environment(Some(old_env.clone()));
                for decl in decls {
                    let BindingPattern::Ident(name) = &decl.pattern else {
                        return Err(not_implemented("destructuring for-head declarations"));
                    };
                    let name = crux::lookup(*name);
                    if *kind == VarDeclKind::Const {
                        env.create_immutable_binding(&name, true)?;
                    } else {
                        env.create_mutable_binding(&name, false)?;
                    }
                    let value = match &decl.init {
                        Some(init) => eval_expr(agent, init, strict)?,
                        None => Value::Undefined,
                    };
                    env.initialize_binding(&name, value)?;
                }
                agent.running_context_mut()?.lexical_environment = env.clone();
                fresh_env = Some(env);
            }
        }
    }
    let mut iteration_result = Value::Undefined;
    let result = loop {
        if let Some(test) = test {
            let test_value = eval_expr(agent, test, strict)?;
            if !to_boolean(&test_value) {
                break Ok(Completion::Normal(iteration_result.clone()));
            }
        }
        match eval_statement(agent, body, strict)? {
            Completion::Normal(value) => iteration_result = value,
            Completion::Empty => {}
            Completion::Continue { target, value }
                if target.is_none() || target.is_some_and(|l| labels.contains(&l)) =>
            {
                if let Some(value) = value {
                    iteration_result = value;
                }
            }
            Completion::Break {
                target: None,
                value,
            } => {
                break Ok(Completion::Normal(
                    value.unwrap_or(iteration_result.clone()),
                ));
            }
            Completion::Break {
                target: Some(l),
                value,
            } if labels.contains(&l) => {
                break Ok(Completion::Break {
                    target: Some(l),
                    value: Some(value.unwrap_or(iteration_result.clone())),
                });
            }
            other => break Ok(other.update_empty(iteration_result.clone())),
        }
        if let Some(update) = update {
            eval_expr(agent, update, strict)?;
        }
    };
    if fresh_env.is_some() {
        agent.running_context_mut()?.lexical_environment = old_env;
    }
    result
}

/// ForInStatement evaluation (spec 14.7.5): enumerate the enumerable string
/// keys of the object and its prototype chain, skipping duplicates.
fn eval_for_in(
    agent: &mut Agent,
    left: &ForBinding,
    right: &syntax::ast::Expr,
    body: &Stmt,
    strict: bool,
    labels: &[crux::string::AtomId],
) -> Result<Completion, JsError> {
    let rhs = eval_expr(agent, right, strict)?;
    let mut seen: HashSet<PropertyKey> = HashSet::new();
    let mut keys: Vec<Value> = Vec::new();
    match &rhs {
        Value::Object(obj) => {
            let mut current = Some(obj.clone());
            while let Some(obj) = current {
                for key in obj.own_property_keys()? {
                    let PropertyKey::String(_) = key else {
                        continue;
                    };
                    if !seen.insert(key.clone()) {
                        continue;
                    }
                    if let Some(property) = obj.get_own_property_key(&key)?
                        && property.enumerable
                    {
                        keys.push(key_value(&key));
                    }
                }
                current = obj.get_prototype_of()?;
            }
        }
        Value::String(text) => {
            // ToObject of a primitive string: its own enumerable index keys.
            for index in 0..text.len() {
                keys.push(Value::String(Handle::new(JsString::from_utf8(
                    &index.to_string(),
                ))));
            }
        }
        Value::Undefined | Value::Null => return Ok(Completion::normal()),
        _ => {}
    }
    let mut iteration_result = Value::Undefined;
    for key in keys {
        let restore = for_binding_put(agent, left, key, strict)?;
        let result = eval_statement(agent, body, strict);
        if let Some(outer) = restore {
            agent.running_context_mut()?.lexical_environment = outer;
        }
        match result? {
            Completion::Normal(value) => iteration_result = value,
            Completion::Empty => {}
            Completion::Continue { target, value }
                if target.is_none() || target.is_some_and(|l| labels.contains(&l)) =>
            {
                if let Some(value) = value {
                    iteration_result = value;
                }
            }
            Completion::Break {
                target: None,
                value,
            } => {
                return Ok(Completion::Normal(value.unwrap_or(iteration_result)));
            }
            Completion::Break {
                target: Some(l),
                value,
            } if labels.contains(&l) => {
                return Ok(Completion::Break {
                    target: Some(l),
                    value: Some(value.unwrap_or(iteration_result)),
                });
            }
            other => return Ok(other.update_empty(iteration_result.clone())),
        }
    }
    Ok(Completion::Normal(iteration_result))
}

fn key_value(key: &PropertyKey) -> Value {
    match key {
        PropertyKey::String(id) => Value::String(Handle::new(crux::lookup(*id))),
        PropertyKey::Symbol(_) => Value::Undefined,
    }
}

/// ForIn/ForOfBodyEvaluation left-hand side: put the value into the
/// expression reference or the (per-iteration) binding. A fresh per-iteration
/// environment is installed for `let`/`const` heads; the caller restores it
/// after the body.
fn for_binding_put(
    agent: &mut Agent,
    left: &ForBinding,
    value: Value,
    strict: bool,
) -> Result<Option<EnvRef>, JsError> {
    match left {
        ForBinding::Expr(expr) => {
            let reference = eval_reference(agent, expr, strict)?;
            put_value(agent, &reference, value)?;
            Ok(None)
        }
        ForBinding::VarDecl { kind, pattern, .. } => {
            let BindingPattern::Ident(name) = pattern else {
                return Err(not_implemented("destructuring for-in/of bindings"));
            };
            let name = crux::lookup(*name);
            if *kind == VarDeclKind::Var {
                let reference = resolve_binding(agent, &name, strict)?;
                put_value(agent, &reference, value)?;
                Ok(None)
            } else {
                let outer = agent.running_context()?.lexical_environment.clone();
                let env = new_declarative_environment(Some(outer.clone()));
                if *kind == VarDeclKind::Const {
                    env.create_immutable_binding(&name, true)?;
                } else {
                    env.create_mutable_binding(&name, false)?;
                }
                env.initialize_binding(&name, value)?;
                agent.running_context_mut()?.lexical_environment = env;
                Ok(Some(outer))
            }
        }
    }
}

/// ForOfStatement evaluation (spec 14.7.6): the iterator protocol with
/// IteratorClose on early exits.
fn eval_for_of(
    agent: &mut Agent,
    left: &ForBinding,
    right: &syntax::ast::Expr,
    body: &Stmt,
    strict: bool,
    labels: &[crux::string::AtomId],
) -> Result<Completion, JsError> {
    let rhs = eval_expr(agent, right, strict)?;
    let iterator = get_iterator(agent, &rhs)?;
    let mut iteration_result = Value::Undefined;
    loop {
        let Some(value) = iterator_step(agent, &iterator)? else {
            return Ok(Completion::Normal(iteration_result));
        };
        let restore = for_binding_put(agent, left, value, strict)?;
        let result = eval_statement(agent, body, strict);
        if let Some(outer) = restore {
            agent.running_context_mut()?.lexical_environment = outer;
        }
        match result? {
            Completion::Normal(value) => iteration_result = value,
            Completion::Empty => {}
            Completion::Continue { target, value }
                if target.is_none() || target.is_some_and(|l| labels.contains(&l)) =>
            {
                if let Some(value) = value {
                    iteration_result = value;
                }
            }
            Completion::Break {
                target: None,
                value,
            } => {
                iterator_close(agent, &iterator)?;
                return Ok(Completion::Normal(value.unwrap_or(iteration_result)));
            }
            Completion::Break {
                target: Some(l),
                value,
            } if labels.contains(&l) => {
                iterator_close(agent, &iterator)?;
                return Ok(Completion::Break {
                    target: Some(l),
                    value: Some(value.unwrap_or(iteration_result)),
                });
            }
            other => {
                iterator_close(agent, &iterator)?;
                return Ok(other.update_empty(iteration_result.clone()));
            }
        }
    }
}

/// LabelledStatement evaluation (spec 14.13): a chain of labels attaches to
/// the innermost statement. `break label` exits the labelled statement;
/// `continue label` restarts a labelled iteration statement without
/// re-evaluating its head, so the loop consumes the completion itself.
fn eval_labeled(
    agent: &mut Agent,
    label: crux::string::AtomId,
    body: &Stmt,
    strict: bool,
) -> Result<Completion, JsError> {
    let mut labels = vec![label];
    let mut stmt = body;
    while let StmtKind::Labeled {
        label: next,
        body: nested,
    } = &stmt.kind
    {
        labels.push(*next);
        stmt = nested;
    }
    let result = match &stmt.kind {
        StmtKind::While { test, body } => eval_while(agent, test, body, strict, &labels)?,
        StmtKind::DoWhile { body, test } => eval_do_while(agent, body, test, strict, &labels)?,
        StmtKind::For {
            init,
            test,
            update,
            body,
        } => eval_for(
            agent,
            init.as_ref(),
            test.as_ref(),
            update.as_ref(),
            body,
            strict,
            &labels,
        )?,
        StmtKind::ForIn { left, right, body } => {
            eval_for_in(agent, left, right, body, strict, &labels)?
        }
        StmtKind::ForOf {
            left,
            right,
            body,
            is_await,
        } => {
            if *is_await {
                return Err(not_implemented("for await"));
            }
            eval_for_of(agent, left, right, body, strict, &labels)?
        }
        _ => {
            let result = eval_statement(agent, stmt, strict)?;
            // `continue label` naming a non-iteration statement is an early
            // error; `break label` exits the labelled statement normally.
            return match result {
                Completion::Break {
                    target: Some(l),
                    value,
                } if labels.contains(&l) => {
                    Ok(Completion::Normal(value.unwrap_or(Value::Undefined)))
                }
                Completion::Continue {
                    target: Some(l), ..
                } if labels.contains(&l) => Err(JsError::new(
                    ErrorKind::SyntaxError,
                    "Illegal continue statement: label does not denote an iteration statement"
                        .into(),
                )),
                other => Ok(other),
            };
        }
    };
    // LabelledStatement evaluation (spec 14.14.3 step 4): a break whose
    // target names this label completes the statement normally.
    match result {
        Completion::Break {
            target: Some(l),
            value,
        } if labels.contains(&l) => Ok(Completion::Normal(value.unwrap_or(Value::Undefined))),
        other => Ok(other),
    }
}

/// SwitchStatement evaluation (spec 14.12): case tests run in order until a
/// match; consequents fall through until `break`.
fn eval_switch(
    agent: &mut Agent,
    discriminant: &syntax::ast::Expr,
    cases: &[syntax::ast::SwitchCase],
    strict: bool,
) -> Result<Completion, JsError> {
    let discriminant = eval_expr(agent, discriminant, strict)?;
    let mut start = cases.len();
    let mut default_index = cases.len();
    for (index, case) in cases.iter().enumerate() {
        match &case.test {
            None => default_index = index,
            Some(test) => {
                let test_value = eval_expr(agent, test, strict)?;
                if crux::ops::is_strictly_equal(&discriminant, &test_value) {
                    start = index;
                    break;
                }
            }
        }
    }
    if start == cases.len() {
        start = default_index;
    }
    for case in &cases[start..] {
        match eval_statement_list(agent, &case.consequent, strict)? {
            Completion::Normal(_) | Completion::Empty => {}
            // An unlabeled break exits the switch as a normal completion
            // carrying the case list's value (spec 14.14.2 step 2).
            Completion::Break {
                target: None,
                value,
            } => {
                return Ok(Completion::Normal(value.unwrap_or(Value::Undefined)));
            }
            other => return Ok(other),
        }
    }
    Ok(Completion::normal())
}

/// TryStatement evaluation (spec 14.15): internal errors are caught too, and
/// the finally block's completion overrides when it is not normal.
fn eval_try(
    agent: &mut Agent,
    block: &syntax::ast::Block,
    handler: Option<&syntax::ast::CatchClause>,
    finalizer: Option<&syntax::ast::Block>,
    strict: bool,
) -> Result<Completion, JsError> {
    let result = eval_statement_list(agent, &block.stmts, strict);
    let handled = match result {
        Ok(Completion::Throw(value)) => match handler {
            Some(handler) => eval_catch(agent, handler, value, strict)?,
            None => Completion::Throw(value),
        },
        Ok(other) => other,
        Err(error) => match handler {
            // Phase 8 binds real Error objects; until then the message.
            Some(handler) => eval_catch(agent, handler, catch_value(error), strict)?,
            None => return run_finalizer(agent, finalizer, Err(error), strict),
        },
    };
    run_finalizer(agent, finalizer, Ok(handled), strict)
}

/// The value bound to the catch parameter: the thrown language value, or the
/// message of an internal error (Phase 8 creates real Error objects).
fn catch_value(error: JsError) -> Value {
    Value::String(Handle::new(JsString::from_utf8(&error.message)))
}

fn run_finalizer(
    agent: &mut Agent,
    finalizer: Option<&syntax::ast::Block>,
    result: Result<Completion, JsError>,
    strict: bool,
) -> Result<Completion, JsError> {
    let Some(finalizer) = finalizer else {
        return result;
    };
    match eval_statement_list(agent, &finalizer.stmts, strict)? {
        Completion::Normal(_) | Completion::Empty => result,
        other => Ok(other),
    }
}

fn eval_catch(
    agent: &mut Agent,
    handler: &syntax::ast::CatchClause,
    thrown: Value,
    strict: bool,
) -> Result<Completion, JsError> {
    let old_env = agent.running_context()?.lexical_environment.clone();
    let catch_env = new_declarative_environment(Some(old_env.clone()));
    if let Some(param) = &handler.param {
        let BindingPattern::Ident(name) = param else {
            return Err(not_implemented("catch destructuring"));
        };
        let name = crux::lookup(*name);
        catch_env.create_mutable_binding(&name, false)?;
        catch_env.initialize_binding(&name, thrown)?;
    }
    agent.running_context_mut()?.lexical_environment = catch_env;
    let result = eval_statement_list(agent, &handler.body.stmts, strict);
    agent.running_context_mut()?.lexical_environment = old_env;
    result
}

/// WithStatement evaluation (Annex B 13.11): an object environment whose
/// bindings are the with-object's properties.
fn eval_with(
    agent: &mut Agent,
    object: &syntax::ast::Expr,
    body: &Stmt,
    strict: bool,
) -> Result<Completion, JsError> {
    let object_value = eval_expr(agent, object, strict)?;
    let Value::Object(obj) = object_value else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot use 'with' on a non-object value".into(),
        ));
    };
    let old_env = agent.running_context()?.lexical_environment.clone();
    let with_env = new_object_environment(obj, true, Some(old_env.clone()));
    agent.running_context_mut()?.lexical_environment = with_env;
    let result = eval_statement(agent, body, strict);
    agent.running_context_mut()?.lexical_environment = old_env;
    result
}

fn not_implemented(what: &str) -> JsError {
    JsError::new(
        ErrorKind::TypeError,
        format!("{what} is not implemented until Phase 7"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, evaluate};

    fn run(source: &str) -> Result<Value, JsError> {
        evaluate(source)
    }

    #[test]
    fn evaluates_a_trivial_script_to_a_value() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let realm = agent.current_realm().unwrap();
        let program = syntax::ast::Program {
            body: vec![Stmt {
                span: crux::Span::new(0, 0),
                kind: StmtKind::Expr(syntax::ast::Expr {
                    span: crux::Span::new(0, 0),
                    kind: syntax::ast::ExprKind::Literal(syntax::ast::Literal::Number(42.0)),
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
        assert_eq!(run("null").unwrap(), Value::Null);
        assert_eq!(run("true").unwrap(), Value::Boolean(true));
        assert_eq!(run("1.5").unwrap(), Value::Number(1.5));
        assert_eq!(
            run("'hi'").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("hi")))
        );
        assert_eq!(
            run("42n").unwrap(),
            Value::BigInt(Handle::new(crux::BigInt::from(42i64)))
        );
    }

    #[test]
    fn var_declarations_hoist_and_initialize() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent.run_script("var x; var y = 7; x = 3; x").unwrap();
        let global = agent.running_context().unwrap().realm.global_object.clone();
        assert_eq!(
            global.get(&JsString::from_utf8("x")).unwrap(),
            Value::Number(3.0)
        );
        assert_eq!(
            global.get(&JsString::from_utf8("y")).unwrap(),
            Value::Number(7.0)
        );
    }

    #[test]
    fn var_inside_blocks_is_global() {
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
        let result = agent.run_script("{ let x = 1; { let x = 2; x } }").unwrap();
        assert_eq!(result, Value::Number(2.0));
        // Reading a binding before its declaration in the same block is a
        // ReferenceError; `{ let y; y }` is fine (y initializes to undefined).
        assert!(agent.run_script("{ y; let y; }").is_err());
    }

    #[test]
    fn const_bindings_reject_assignment() {
        assert!(run("const c = 1; c = 2;").is_err());
    }

    #[test]
    fn sloppy_assignment_creates_globals_strict_throws() {
        assert_eq!(
            run("undeclared = 5; undeclared").unwrap(),
            Value::Number(5.0)
        );
        assert!(run("'use strict'; undeclared = 5;").is_err());
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
        agent.run_script("class C {}").unwrap();
        let global = agent.running_context().unwrap().realm.global_object.clone();
        assert!(!global.has_own_property(&JsString::from_utf8("C")).unwrap());
    }

    #[test]
    fn let_shadowing_var_at_global_scope_is_an_error() {
        assert!(run("var x = 1; let x = 2;").is_err());
    }

    #[test]
    fn restricted_global_properties_block_lexical_declarations() {
        assert!(run("let NaN = 1;").is_err());
    }

    #[test]
    fn undefined_identifier_is_a_reference_error() {
        assert!(run("nope").is_err());
    }

    // ---- Phase 6 expression tests ----

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(run("1 + 2 * 3").unwrap(), Value::Number(7.0));
        assert_eq!(run("(1 + 2) * 3").unwrap(), Value::Number(9.0));
        assert_eq!(run("10 / 4").unwrap(), Value::Number(2.5));
        assert_eq!(run("10 % 3").unwrap(), Value::Number(1.0));
        assert_eq!(run("2 ** 10").unwrap(), Value::Number(1024.0));
        assert_eq!(run("-5 + 3").unwrap(), Value::Number(-2.0));
        assert_eq!(run("7 - 2 - 1").unwrap(), Value::Number(4.0));
        assert_eq!(run("3 + 4 * 2 ** 2").unwrap(), Value::Number(19.0));
    }

    #[test]
    fn string_concatenation() {
        assert_eq!(
            run("'a' + 'b'").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("ab")))
        );
        assert_eq!(
            run("1 + 'x'").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("1x")))
        );
        assert_eq!(
            run("'x' + 2").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("x2")))
        );
    }

    #[test]
    fn bigint_arithmetic() {
        assert_eq!(
            run("2n + 3n").unwrap(),
            Value::BigInt(Handle::new(crux::BigInt::from(5i64)))
        );
        assert_eq!(
            run("10n % 3n").unwrap(),
            Value::BigInt(Handle::new(crux::BigInt::from(1i64)))
        );
        assert!(run("1n + 1").is_err());
        assert!(run("1n / 0n").is_err());
    }

    #[test]
    fn comparisons_and_equality() {
        assert_eq!(run("1 < 2").unwrap(), Value::Boolean(true));
        assert_eq!(run("2 <= 2").unwrap(), Value::Boolean(true));
        assert_eq!(run("3 > 4").unwrap(), Value::Boolean(false));
        assert_eq!(run("'a' < 'b'").unwrap(), Value::Boolean(true));
        assert_eq!(run("1 == '1'").unwrap(), Value::Boolean(true));
        assert_eq!(run("1 === '1'").unwrap(), Value::Boolean(false));
        assert_eq!(run("null == undefined").unwrap(), Value::Boolean(true));
        assert_eq!(run("NaN == NaN").unwrap(), Value::Boolean(false));
    }

    #[test]
    fn bitwise_and_shift() {
        assert_eq!(run("5 & 3").unwrap(), Value::Number(1.0));
        assert_eq!(run("5 | 3").unwrap(), Value::Number(7.0));
        assert_eq!(run("5 ^ 3").unwrap(), Value::Number(6.0));
        assert_eq!(run("1 << 4").unwrap(), Value::Number(16.0));
        assert_eq!(run("-1 >>> 0").unwrap(), Value::Number(4294967295.0));
        assert_eq!(run("-2 >> 1").unwrap(), Value::Number(-1.0));
        assert_eq!(run("~5").unwrap(), Value::Number(-6.0));
    }

    #[test]
    fn logical_and_conditional() {
        assert_eq!(run("0 && 5").unwrap(), Value::Number(0.0));
        assert_eq!(run("3 && 5").unwrap(), Value::Number(5.0));
        assert_eq!(run("0 || 5").unwrap(), Value::Number(5.0));
        assert_eq!(run("3 || 5").unwrap(), Value::Number(3.0));
        assert_eq!(run("null ?? 5").unwrap(), Value::Number(5.0));
        assert_eq!(run("0 ?? 5").unwrap(), Value::Number(0.0));
        assert_eq!(run("true ? 1 : 2").unwrap(), Value::Number(1.0));
        assert_eq!(run("false ? 1 : 2").unwrap(), Value::Number(2.0));
    }

    #[test]
    fn update_and_compound_assignment() {
        assert_eq!(run("let x = 5; x++").unwrap(), Value::Number(5.0));
        assert_eq!(run("let x = 5; ++x").unwrap(), Value::Number(6.0));
        assert_eq!(run("let x = 5; x += 3; x").unwrap(), Value::Number(8.0));
        assert_eq!(run("let x = 5; x *= 2; x").unwrap(), Value::Number(10.0));
        assert_eq!(run("let x = 5; x **= 2; x").unwrap(), Value::Number(25.0));
    }

    #[test]
    fn typeof_void_delete() {
        assert_eq!(
            run("typeof 1").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("number")))
        );
        assert_eq!(
            run("typeof 'a'").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("string")))
        );
        assert_eq!(
            run("typeof undeclared").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("undefined")))
        );
        assert_eq!(run("void 7").unwrap(), Value::Undefined);
        assert_eq!(
            run("let o = { x: 1 }; delete o.x; o.x").unwrap(),
            Value::Undefined
        );
    }

    #[test]
    fn member_access_and_assignment() {
        assert_eq!(
            run("let o = { a: 1, b: 2 }; o.a + o['b']").unwrap(),
            Value::Number(3.0)
        );
        assert_eq!(
            run("let o = { a: 1 }; o.a = 9; o.a").unwrap(),
            Value::Number(9.0)
        );
        assert_eq!(
            run("let o = {}; o['x'] = 5; o.x").unwrap(),
            Value::Number(5.0)
        );
        assert_eq!(run("let a = [1, 2, 3]; a[1]").unwrap(), Value::Number(2.0));
        assert_eq!(
            run("let a = [1, 2, 3]; a.length").unwrap(),
            Value::Number(3.0)
        );
        assert_eq!(
            run("let a = [1, 2]; a[5] = 9; a.length").unwrap(),
            Value::Number(6.0)
        );
    }

    #[test]
    fn array_and_object_literals() {
        let arr = run("[1, 2, 3]").unwrap();
        assert!(matches!(arr, Value::Object(_)));
        assert_eq!(
            run("let a = [1, 2, 3]; a[0] + a[2]").unwrap(),
            Value::Number(4.0)
        );
        assert_eq!(run("let a = [1, , 3]; a[1]").unwrap(), Value::Undefined);
        assert_eq!(
            run("let a = [1, , 3]; a.length").unwrap(),
            Value::Number(3.0)
        );
        assert_eq!(
            run("let o = { x: 1, y: 2 }; o.x + o.y").unwrap(),
            Value::Number(3.0)
        );
        assert_eq!(
            run("let o = { a: 1 }; let p = { ...o, b: 2 }; p.a + p.b").unwrap(),
            Value::Number(3.0)
        );
    }

    #[test]
    fn object_proto_setter() {
        assert_eq!(
            run("let p = { __proto__: { q: 9 } }; p.q").unwrap(),
            Value::Number(9.0)
        );
        assert_eq!(
            run("let o = { __proto__: null }; o.q").unwrap(),
            Value::Undefined
        );
        assert_eq!(
            run("let o = { __proto__: null, q: 5 }; o.q").unwrap(),
            Value::Number(5.0)
        );
        // Non-object __proto__ values become plain data properties.
        assert_eq!(
            run("let o = { __proto__: 5 }; o.__proto__").unwrap(),
            Value::Number(5.0)
        );
    }

    #[test]
    fn template_literals() {
        assert_eq!(
            run("`a${1 + 1}b`").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("a2b")))
        );
        assert_eq!(
            run("`plain`").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("plain")))
        );
    }

    #[test]
    fn sequence_comma() {
        assert_eq!(run("(1, 2, 3)").unwrap(), Value::Number(3.0));
        assert_eq!(
            run("let x = 0; (x = 1, x + 1)").unwrap(),
            Value::Number(2.0)
        );
    }

    #[test]
    fn eval_calls_perform_eval() {
        // Direct and indirect eval both route to %eval% (spec 13.3.6.1); the
        // completion value of the evaluated Script is the call's result.
        assert_eq!(run("eval('1 + 2')").unwrap(), Value::Number(3.0));
        assert_eq!(run("(0, eval)('2 + 3')").unwrap(), Value::Number(5.0));
        assert_eq!(run("eval('let ev = 4; ev')").unwrap(), Value::Number(4.0));
        assert!(run("eval('nope')").is_err());
    }

    #[test]
    fn calls_and_method_calls_with_native_builtins() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let add = crux::Function::create_builtin(
            Some(JsString::from_utf8("add")),
            2,
            Box::new(|_, args| {
                let a = match args.first() {
                    Some(Value::Number(n)) => *n,
                    _ => 0.0,
                };
                let b = match args.get(1) {
                    Some(Value::Number(n)) => *n,
                    _ => 0.0,
                };
                Ok(Value::Number(a + b))
            }),
            None,
            None,
        )
        .unwrap();
        let global = agent.running_context().unwrap().realm.global_object.clone();
        global
            .create_data_property(&JsString::from_utf8("add"), Value::Function(add))
            .unwrap();
        assert_eq!(agent.run_script("add(2, 3)").unwrap(), Value::Number(5.0));
        assert_eq!(
            agent.run_script("add(1, add(2, 3))").unwrap(),
            Value::Number(6.0)
        );
        // Method calls bind `this` to the receiver.
        let getter = crux::Function::create_builtin(
            Some(JsString::from_utf8("getX")),
            0,
            Box::new(|this, _| match this {
                Value::Object(obj) => obj.get(&JsString::from_utf8("x")),
                _ => Ok(Value::Undefined),
            }),
            None,
            None,
        )
        .unwrap();
        let obj = crux::object::JsObject::ordinary_object_create(None);
        obj.create_data_property(&JsString::from_utf8("x"), Value::Number(7.0))
            .unwrap();
        obj.create_data_property(&JsString::from_utf8("getX"), Value::Function(getter))
            .unwrap();
        global
            .create_data_property(&JsString::from_utf8("obj"), Value::Object(obj))
            .unwrap();
        assert_eq!(agent.run_script("obj.getX()").unwrap(), Value::Number(7.0));
    }

    #[test]
    fn new_and_instanceof_with_native_constructor() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let proto = crux::object::JsObject::ordinary_object_create(None);
        let proto_for_ctor = proto.clone();
        let ctor = crux::Function::create_builtin(
            Some(JsString::from_utf8("C")),
            0,
            Box::new(|_, _| Ok(Value::Undefined)),
            Some(Box::new(move |_, _| {
                Ok(Value::Object(
                    crux::object::JsObject::ordinary_object_create(Some(proto_for_ctor.clone())),
                ))
            })),
            None,
        )
        .unwrap();
        ctor.define_property(
            &JsString::from_utf8("prototype"),
            &crux::property::PropertyDescriptor::data(Value::Object(proto)),
        )
        .unwrap();
        let global = agent.running_context().unwrap().realm.global_object.clone();
        global
            .create_data_property(&JsString::from_utf8("C"), Value::Function(ctor))
            .unwrap();
        assert_eq!(
            agent.run_script("let o = new C(); o instanceof C").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            agent.run_script("let p = {}; p instanceof C").unwrap(),
            Value::Boolean(false)
        );
    }

    #[test]
    fn in_operator() {
        assert_eq!(
            run("let o = { x: 1 }; 'x' in o").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("let o = { x: 1 }; 'y' in o").unwrap(),
            Value::Boolean(false)
        );
        assert!(run("'x' in 5").is_err());
        assert_eq!(run("1 in { 1: 2 }").unwrap(), Value::Boolean(true));
    }

    // ---- Phase 6 statement tests ----

    #[test]
    fn if_else() {
        assert_eq!(run("if (true) 1; else 2;").unwrap(), Value::Number(1.0));
        assert_eq!(run("if (false) 1; else 2;").unwrap(), Value::Number(2.0));
        assert_eq!(
            run("let x = 0; if (1 < 2) x = 7; x").unwrap(),
            Value::Number(7.0)
        );
    }

    #[test]
    fn while_loops() {
        assert_eq!(
            run("let i = 0; let s = 0; while (i < 5) { s += i; i++; } s").unwrap(),
            Value::Number(10.0)
        );
        assert_eq!(
            run("let i = 0; while (i < 3) { i++; if (i === 2) break; } i").unwrap(),
            Value::Number(2.0)
        );
        assert_eq!(
            run("let i = 0; while (i < 3) { i++; if (i === 1) continue; } i").unwrap(),
            Value::Number(3.0)
        );
    }

    #[test]
    fn do_while_loops() {
        assert_eq!(
            run("let i = 0; do { i++; } while (i < 3); i").unwrap(),
            Value::Number(3.0)
        );
        assert_eq!(
            run("let i = 0; do { i++; } while (false); i").unwrap(),
            Value::Number(1.0)
        );
    }

    #[test]
    fn for_loops() {
        assert_eq!(
            run("let s = 0; for (let i = 0; i < 5; i++) { s += i; } s").unwrap(),
            Value::Number(10.0)
        );
        assert_eq!(
            run("let s = 0; for (var i = 0; i < 3; i++) { s += i; } s").unwrap(),
            Value::Number(3.0)
        );
        assert_eq!(
            run("let s = 0; for (let i = 0; ; i++) { if (i >= 4) break; s += i; } s").unwrap(),
            Value::Number(6.0)
        );
        assert_eq!(
            run("let i = 0; for (; i < 3; ) { i++; } i").unwrap(),
            Value::Number(3.0)
        );
    }

    #[test]
    fn labeled_break_continue() {
        assert_eq!(
            run("let s = 0; outer: for (let i = 0; i < 3; i++) { for (let j = 0; j < 3; j++) { if (j === 1) continue outer; s++; } } s")
                .unwrap(),
            Value::Number(3.0)
        );
        assert_eq!(
            run("let i = 0; outer: while (true) { i++; if (i === 3) break outer; } i").unwrap(),
            Value::Number(3.0)
        );
    }

    #[test]
    fn switch_statements() {
        assert_eq!(run("let x = 2; switch (x) { case 1: x = 10; break; case 2: x = 20; break; default: x = 30; } x").unwrap(), Value::Number(20.0));
        assert_eq!(
            run("let x = 9; switch (x) { case 1: x = 10; break; default: x = 30; } x").unwrap(),
            Value::Number(30.0)
        );
        assert_eq!(
            run("let x = 1; switch (x) { case 1: x = 10; case 2: x = 20; break; } x").unwrap(),
            Value::Number(20.0)
        );
    }

    #[test]
    fn throw_and_try_catch() {
        assert!(run("throw 5;").is_err());
        assert_eq!(
            run("try { throw 42; } catch (e) { e }").unwrap(),
            Value::Number(42.0)
        );
        assert_eq!(
            run("let caught = 0; try { undefined_var; } catch (e) { caught = 1; } caught").unwrap(),
            Value::Number(1.0)
        );
        assert_eq!(
            run("try { 1; } catch (e) { 2; } finally { 3; }").unwrap(),
            Value::Number(1.0)
        );
        assert_eq!(
            run("try { throw 1; } catch (e) { 2; } finally { 3; }").unwrap(),
            Value::Number(2.0)
        );
        // Per spec 14.15.2 the try block's completion wins when the finally
        // block completes normally.
        assert_eq!(
            run("try { 1; } finally { 2; }").unwrap(),
            Value::Number(1.0)
        );
    }

    #[test]
    fn for_in_over_object_keys() {
        assert_eq!(
            run("let o = { a: 1, b: 2, c: 3 }; let s = 0; for (let k in o) { s += o[k]; } s")
                .unwrap(),
            Value::Number(6.0)
        );
        assert_eq!(
            run("let o = { a: 1, b: 2 }; let n = 0; for (let k in o) { n++; } n").unwrap(),
            Value::Number(2.0)
        );
    }

    #[test]
    fn for_of_over_custom_iterator() {
        // Hand-built iterator: for-of uses the @@iterator protocol (spec
        // 7.4.2). The iterable is assembled natively because the global
        // `Symbol` constructor lands with the Phase 8 builtins.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let counter = std::cell::Cell::new(0u32);
        let next = crux::Function::create_builtin(
            Some(JsString::from_utf8("next")),
            0,
            Box::new(move |_, _| {
                let i = counter.get();
                counter.set(i + 1);
                let obj = crux::object::JsObject::ordinary_object_create(None);
                if i < 3 {
                    obj.create_data_property(
                        &JsString::from_utf8("value"),
                        Value::Number(i as f64),
                    )?;
                    obj.create_data_property(&JsString::from_utf8("done"), Value::Boolean(false))?;
                } else {
                    obj.create_data_property(&JsString::from_utf8("value"), Value::Undefined)?;
                    obj.create_data_property(&JsString::from_utf8("done"), Value::Boolean(true))?;
                }
                Ok(Value::Object(obj))
            }),
            None,
            None,
        )
        .unwrap();
        let iterator = crux::object::JsObject::ordinary_object_create(None);
        iterator
            .create_data_property(&JsString::from_utf8("next"), Value::Function(next))
            .unwrap();
        let iterable = crux::object::JsObject::ordinary_object_create(None);
        let iterator_for_method = iterator.clone();
        iterable
            .define_property_key(
                &crux::property::PropertyKey::Symbol(
                    crux::symbol::well_known("iterator").as_ref().clone(),
                ),
                &crux::property::PropertyDescriptor::data(Value::Function(
                    crux::Function::create_builtin(
                        Some(JsString::from_utf8("[Symbol.iterator]")),
                        0,
                        Box::new(move |_, _| Ok(Value::Object(iterator_for_method.clone()))),
                        None,
                        None,
                    )
                    .unwrap(),
                )),
            )
            .unwrap();
        let global = agent.running_context().unwrap().realm.global_object.clone();
        global
            .create_data_property(&JsString::from_utf8("iter"), Value::Object(iterable))
            .unwrap();
        assert_eq!(
            agent
                .run_script("let s = 0; for (let x of iter) { s += x; } s")
                .unwrap(),
            Value::Number(3.0)
        );
    }

    #[test]
    fn with_statement() {
        assert_eq!(
            run("let o = { x: 42 }; with (o) { x }").unwrap(),
            Value::Number(42.0)
        );
        assert_eq!(
            run("let o = { x: 1 }; let x = 9; with (o) { x }").unwrap(),
            Value::Number(1.0)
        );
    }

    #[test]
    fn instanceof_and_in() {
        // `in` works on object RHS; instanceof needs a constructor (the
        // Phase 8 built-ins install them).
        assert!(run("({}) instanceof Object").is_err());
        assert_eq!(
            run("let o = { x: 1 }; 'x' in o").unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn nested_blocks_and_scope() {
        assert_eq!(
            run("let x = 1; { let x = 2; { x } }").unwrap(),
            Value::Number(2.0)
        );
        assert_eq!(
            run("let x = 1; { let x = 2; } x").unwrap(),
            Value::Number(1.0)
        );
    }

    #[test]
    fn optional_chaining_short_circuits() {
        assert_eq!(run("let o = null; o?.x").unwrap(), Value::Undefined);
        assert_eq!(
            run("let o = { a: { b: 5 } }; o.a?.b").unwrap(),
            Value::Number(5.0)
        );
        assert!(run("let o = null; o.x").is_err());
        assert_eq!(run("let o = null; o?.x?.y").unwrap(), Value::Undefined);
    }

    #[test]
    fn chained_member_and_method_this() {
        assert_eq!(
            run("let o = { a: { b: { c: 3 } } }; o.a.b.c").unwrap(),
            Value::Number(3.0)
        );
    }

    #[test]
    fn completion_value_of_blocks() {
        assert_eq!(run("{ 1; 2; 3; }").unwrap(), Value::Number(3.0));
        assert_eq!(
            run("if (true) { 1; } else { 2; }").unwrap(),
            Value::Number(1.0)
        );
    }
}
