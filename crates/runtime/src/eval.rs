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
use crux::value::{Value, ValueKind};
use syntax::ast::{
    BindingPattern, ExprKind, ForBinding, ForInit, Program, Stmt, StmtKind, VarDeclKind,
};

use crate::agent::Agent;
use crate::context::{initialize_referenced_binding, put_value, resolve_binding};
use crate::env::{EnvRecord, EnvRef, new_declarative_environment, new_object_environment};
use crate::expr::{eval_expr, eval_reference, get_iterator, iterator_close, iterator_step};
use crate::flow::{Completion, completion_to_result};
use crate::script::bound_names;

/// Evaluate a whole program (the Evaluation of |Script|, spec 16.1.6 step
/// 11). The completion value is the value of the last evaluated statement.
/// Script bodies compile to steps and run on the VM, so the whole engine
/// executes bytecode (the top-level path is compiled like any body).
/// `fast_script` certifies top-level *script* evaluation for the
/// script-level binding fast path; eval bodies pass `false` (they can see
/// the caller's lexical environment). `source` (Cut 63) keys the per-agent
/// compiled-body cache — re-evaluating the same source reuses the compiled
/// IR (the parse still runs; the compile and the JIT machine code do not
/// repeat). `jsx` is part of that key: the same text desugars differently
/// under the JSX goal. `None` compiles fresh (modules evaluate once).
pub fn eval_program(
    agent: &mut Agent,
    program: &Program,
    strict: bool,
    fast_script: bool,
    jsx: bool,
    source: Option<&crux::JsString>,
) -> Result<Value, JsError> {
    let body = match source {
        Some(source) => {
            let key = (source.clone(), strict, fast_script, jsx);
            if let Some(body) = agent.script_bodies.get(&key) {
                body.clone()
            } else {
                let body = std::rc::Rc::new(crate::ir::compile_statements(
                    &program.body,
                    strict,
                    fast_script,
                )?);
                agent.script_bodies.insert(key, body.clone());
                body
            }
        }
        None => std::rc::Rc::new(crate::ir::compile_statements(
            &program.body,
            strict,
            fast_script,
        )?),
    };
    let env = agent.running_context()?.lexical_environment;
    let mut vm = agent.take_vm(env, strict);
    // Cut 16: a slotified script's declared vars live in the frame — the
    // prologue already loaded them from the global object, so no argument
    // copies are needed.
    if let Some(scope) = &body.scope {
        vm.setup_frame(scope, &[]);
    }
    // Cut 65: a certified script runs through the JIT when the hook is
    // installed — `run_jit_body_loop` compiles the body on first use and
    // falls back to `vm.start` (the interpreter) inside for a body with no
    // compiled code or an unsupported step. The top-level bench loops
    // (`n = f(n)` fused global call-stores) previously ran interpreted;
    // the machine code now executes them with the leaf callees in-frame.
    let outcome = if body.scope.is_some() {
        let mut body_rc = body.clone();
        crate::jit::run_jit_body_loop(agent, &mut vm, &mut body_rc)
    } else {
        vm.start(agent, &body)
    };
    let completion = match outcome {
        Ok(crate::ir::VmOutcome::Completed(completion)) => {
            // Cut 65: a compiled script falls off the end with the machine
            // code's past-the-end value (`undefined`) — `run_jit_body_loop`
            // maps it to `Completion::Return` (the shape function bodies
            // complete with). Scripts cannot contain `return`, so every
            // `Return` from this path is that marker; the script's real
            // completion is the register the compiled completion steps wrote
            // (`vm.completion`/`completion_is_empty`), read exactly like the
            // interpreter's fall-off-end arm (`run_inner_inner`'s `None`).
            // The interpreter fallback (`vm.start`) never produces a `Return`.
            match completion {
                crate::flow::Completion::Return(_) => {
                    if vm.completion_is_empty {
                        crate::flow::Completion::Empty
                    } else {
                        crate::flow::Completion::Normal(vm.completion)
                    }
                }
                other => other,
            }
        }
        Ok(crate::ir::VmOutcome::Suspended(_)) => {
            agent.return_vm(vm);
            return Err(JsError::new(
                ErrorKind::SyntaxError,
                "yield/await in a script top level".into(),
            ));
        }
        // `run_inner`'s driver consumes tail calls internally (scripts cannot
        // contain return statements at all); an escaped one is an internal
        // invariant violation.
        Ok(crate::ir::VmOutcome::TailCall(_)) => {
            agent.return_vm(vm);
            return Err(JsError::new(
                ErrorKind::SyntaxError,
                "tail call in a script top level".into(),
            ));
        }
        Err(error) => {
            agent.return_vm(vm);
            return Err(error);
        }
    };
    // The script's `using` resources are disposed when the list completes
    // (spec 14.2.3 step 6), like the walked statement list.
    let result = dispose_env_resources(agent, &env, Ok(completion));
    agent.return_vm(vm);
    let completion = result?;
    completion_to_result(completion)
}

pub(crate) fn eval_statement_list(
    agent: &mut Agent,
    stmts: &[Stmt],
    strict: bool,
) -> Result<Completion, JsError> {
    // The list runs in the current lexical environment, whose `using`
    // resources are disposed when the list completes (spec 14.2.3 step 6:
    // DisposeResources of the block/function/eval environment).
    let env = agent.running_context()?.lexical_environment;
    let completion = eval_statement_list_inner(agent, stmts, strict);
    dispose_env_resources(agent, &env, completion)
}

fn eval_statement_list_inner(
    agent: &mut Agent,
    stmts: &[Stmt],
    strict: bool,
) -> Result<Completion, JsError> {
    // The running value of the list so far: ~empty~ until a value-producing
    // statement runs (spec 14.2.1: UpdateEmpty(s, sl)).
    let mut list_value = Value::Undefined;
    let mut list_is_empty = true;
    let mut completion = Completion::Empty;
    for (index, stmt) in stmts.iter().enumerate() {
        completion = eval_statement(agent, stmt, strict)?;
        match &mut completion {
            Completion::Normal(value) => {
                list_value = *value;
                list_is_empty = false;
            }
            Completion::Break { value, .. } | Completion::Continue { value, .. }
                if index > 0 && value.is_none() && !list_is_empty =>
            {
                *value = Some(list_value);
            }
            _ => {}
        }
        if !matches!(completion, Completion::Normal(_) | Completion::Empty) {
            break;
        }
    }
    // UpdateEmpty (spec 14.2.1 step 3): an ~empty~ trailing completion
    // inherits the value of the preceding statement list.
    match completion {
        Completion::Empty if !list_is_empty => Ok(Completion::Normal(list_value)),
        other => Ok(other),
    }
}

/// Evaluate a single statement (spec 14.x runtime semantics). Shared with the
/// resumable-function IR, which batches suspension-free statements.
pub(crate) fn eval_statement(
    agent: &mut Agent,
    stmt: &Stmt,
    strict: bool,
) -> Result<Completion, JsError> {
    match &stmt.kind {
        StmtKind::Empty | StmtKind::Debugger => Ok(Completion::Empty),
        StmtKind::Expr(expr) => eval_expr(agent, expr, strict).map(Completion::Normal),
        StmtKind::VarDecl { kind, decls } => {
            match kind {
                VarDeclKind::Var => eval_var_declarations(agent, decls, strict)?,
                VarDeclKind::Using => {
                    eval_using_declarations(agent, decls, strict, DisposalKind::Sync)?
                }
                VarDeclKind::AwaitUsing => {
                    eval_using_declarations(agent, decls, strict, DisposalKind::Async)?
                }
                VarDeclKind::Let | VarDeclKind::Const => {
                    eval_lexical_declarations(agent, decls, strict)?;
                }
            }
            Ok(Completion::Empty)
        }
        StmtKind::UsingDecl { is_await, decls } => {
            eval_using_declarations(
                agent,
                decls,
                strict,
                if *is_await {
                    DisposalKind::Async
                } else {
                    DisposalKind::Sync
                },
            )?;
            Ok(Completion::Empty)
        }
        StmtKind::FunctionDecl(f) if f.statement_position && !strict => {
            eval_statement_position_function(agent, f, stmt, strict)?;
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
            // spec 14.3.2 step 2: ResolveBinding runs before the initializer,
            // so a `with` binding object's property is the assignment target
            // even when the initializer mutates the object.
            let reference = match &decl.pattern {
                BindingPattern::Ident(name) => Some(crate::context::resolve_binding(
                    agent,
                    &crux::lookup(*name),
                    strict,
                )?),
                _ => None,
            };
            let value = eval_named_initializer(agent, init, pattern_ident(&decl.pattern), strict)?;
            match reference {
                // spec 14.3.2 step 2.e: PutValue the pre-resolved reference.
                Some(reference) => crate::context::put_value(agent, &reference, value)?,
                // BindingInitialization with no environment resolves and
                // PutValue's the hoisted var binding (spec 14.3.2 step 3).
                None => crate::binding::binding_initialization(
                    agent,
                    &decl.pattern,
                    value,
                    None,
                    strict,
                )?,
            }
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
    eval_lexical_binding_list(agent, decls, strict, DisposalKind::Normal)
}

/// One `using`/`await using` binding list (spec 15.14.4 BindingEvaluation):
/// like a lexical declaration, but each initialized value is registered with
/// AddDisposableResource before its binding is initialized.
fn eval_using_declarations(
    agent: &mut Agent,
    decls: &[syntax::ast::VarDeclarator],
    strict: bool,
    kind: DisposalKind,
) -> Result<(), JsError> {
    eval_lexical_binding_list(agent, decls, strict, kind)
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum DisposalKind {
    Normal,
    Sync,
    Async,
}

/// Evaluate a lexical declaration's BindingList against the running lexical
/// environment (spec 14.2.2 step 2): evaluate each initializer, register
/// `using` resources, then InitializeReferencedBinding.
fn eval_lexical_binding_list(
    agent: &mut Agent,
    decls: &[syntax::ast::VarDeclarator],
    strict: bool,
    kind: DisposalKind,
) -> Result<(), JsError> {
    // BindingInitialization against the running LexicalEnvironment: the
    // bindings were created uninitialized by declaration instantiation and
    // are filled with InitializeBinding (spec 14.2.2 step 2.c).
    let env = agent.running_context()?.lexical_environment;
    for decl in decls {
        let value = match &decl.init {
            Some(init) => {
                eval_named_initializer(agent, init, pattern_ident(&decl.pattern), strict)?
            }
            None => Value::Undefined,
        };
        if kind != DisposalKind::Normal {
            let resource = create_disposable_resource(agent, &value, kind)?;
            env.add_disposable_resource(resource);
        }
        crate::binding::binding_initialization(agent, &decl.pattern, value, Some(&env), strict)?;
    }
    Ok(())
}

/// The identifier of a simple binding pattern, if any.
fn pattern_ident(pattern: &BindingPattern) -> Option<crux::string::AtomId> {
    match pattern {
        BindingPattern::Ident(name) => Some(*name),
        _ => None,
    }
}

/// Evaluate a binding initializer, applying the inferred name of an
/// anonymous function/class definition (spec 14.2.2 step 2.d / 14.3.2 step
/// 2.b). An anonymous class expression receives the name through the
/// definition itself so its static field initializers observe it.
fn eval_named_initializer(
    agent: &mut Agent,
    init: &syntax::ast::Expr,
    binding: Option<crux::string::AtomId>,
    strict: bool,
) -> Result<Value, JsError> {
    if let (ExprKind::Class(class), Some(binding)) = (&init.kind, binding)
        && class.name.is_none()
    {
        return crate::class::class_definition_evaluation(agent, class, Some(binding), strict);
    }
    let value = eval_expr(agent, init, strict)?;
    if let Some(binding) = binding
        && crate::function::is_anonymous_function_definition(init)
    {
        let display = crate::function::default_binding_display_name(Some(crux::lookup(binding)))
            .unwrap_or_else(|| crux::lookup(binding));
        crate::function::set_function_name(&value, &display, None)?;
    }
    Ok(value)
}

/// CreateDisposableResource (spec 9.3.1): capture the @@dispose method of an
/// initialized `using` value; *null*/*undefined* values register nothing, and
/// non-objects, missing, and non-callable dispose methods are TypeErrors.
pub(crate) fn create_disposable_resource(
    agent: &mut Agent,
    value: &Value,
    kind: DisposalKind,
) -> Result<crate::env::DisposableResource, JsError> {
    if matches!(value.kind(), ValueKind::Null | ValueKind::Undefined) {
        return Ok(crate::env::DisposableResource {
            value: Value::Undefined,
            method: Value::Undefined,
            hint: if kind == DisposalKind::Async {
                crate::env::DisposalHint::Async
            } else {
                crate::env::DisposalHint::Sync
            },
        });
    }
    if crate::context::as_object(value).is_none() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "using declarations may only initialize objects".into(),
        ));
    }
    let symbol = if kind == DisposalKind::Async {
        "@@asyncDispose"
    } else {
        "@@dispose"
    };
    // spec 9.3.1 GetDisposeMethod: an async context falls back to the sync
    // @@dispose method.
    let mut method = crate::expr::get_method(agent, value, symbol)?;
    if kind == DisposalKind::Async && method.is_none() {
        method = crate::expr::get_method(agent, value, "@@dispose")?;
    }
    let method = method.unwrap_or(Value::Undefined);
    if matches!(method.kind(), ValueKind::Undefined) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Object is not disposable".into(),
        ));
    }
    Ok(crate::env::DisposableResource {
        value: *value,
        method,
        hint: if kind == DisposalKind::Async {
            crate::env::DisposalHint::Async
        } else {
            crate::env::DisposalHint::Sync
        },
    })
}

/// DisposeResources (spec 9.4.3): run an environment's registered dispose
/// methods in reverse order, folding a throwing disposal into the completion
/// — a SuppressedError when the completion is itself a throw, otherwise the
/// disposal error replaces it.
pub(crate) fn dispose_env_resources(
    agent: &mut Agent,
    env: &EnvRef,
    completion: Result<Completion, JsError>,
) -> Result<Completion, JsError> {
    // The overwhelmingly common case (a body with no `using`) avoids the
    // drain's `mem::take` entirely — a plain emptiness read.
    if !env.has_disposable_resources() {
        return completion;
    }
    let resources = env.drain_disposable_resources();
    let mut completion = completion;
    for resource in resources.iter().rev() {
        if resource.method.is_undefined() {
            continue;
        }
        match crate::function::call(agent, &resource.method, resource.value, &[]) {
            Ok(_) => {}
            Err(disposal_error) => {
                let disposal_value = crate::promise::error_value(agent, &disposal_error);
                match &mut completion {
                    Ok(Completion::Throw(original)) => {
                        let suppressed = std::mem::replace(original, Value::Undefined);
                        *original = crate::builtins::disposable::make_suppressed_error(
                            agent,
                            disposal_value,
                            suppressed,
                        )?;
                    }
                    Err(original) if original.value.is_some() => {
                        let suppressed = crate::promise::error_value(agent, original);
                        completion = Ok(Completion::Throw(
                            crate::builtins::disposable::make_suppressed_error(
                                agent,
                                disposal_value,
                                suppressed,
                            )?,
                        ));
                    }
                    _ => {
                        completion = Err(JsError::new(
                            ErrorKind::TypeError,
                            "Uncaught disposal error".into(),
                        )
                        .with_value(disposal_value));
                    }
                }
            }
        }
    }
    completion
}

/// Annex B: a statement-position function declaration (`if (x) function
/// f(){}` in sloppy code) behaves as if the declaration were wrapped in a
/// block — a block-scoped binding for the function's own scope plus the
/// var-scoped hoist, copied into the variable environment at evaluation.
pub(crate) fn eval_statement_position_function(
    agent: &mut Agent,
    f: &syntax::ast::Function,
    stmt: &Stmt,
    strict: bool,
) -> Result<(), JsError> {
    let old_env = agent.running_context()?.lexical_environment;
    let block_env = new_declarative_environment(Some(old_env));
    block_declaration_instantiation(agent, std::slice::from_ref(stmt), &block_env, strict)?;
    agent.running_context_mut()?.lexical_environment = block_env;
    eval_function_declaration(agent, f, strict)?;
    agent.running_context_mut()?.lexical_environment = old_env;
    Ok(())
}

/// FunctionDeclaration evaluation (spec 15.2.6): instantiate the function
/// object against the current lexical environment and bind it in the
/// VariableEnvironment.
pub(crate) fn eval_function_declaration(
    agent: &mut Agent,
    f: &syntax::ast::Function,
    strict: bool,
) -> Result<(), JsError> {
    let Some(name) = f.name else {
        return Ok(());
    };
    let name = crux::lookup(name);
    let running = agent.running_context()?;
    let variable_env = running.variable_environment;
    let lexical_env = running.lexical_environment;

    // Annex B.3.2.2 / B.3.3.3: a block-level function declaration in sloppy
    // code copies its (block-scoped) binding into the variable environment
    // when it is evaluated. When the innermost environment binds the name but
    // the block did not hoist it (a strict block, or a let/const that
    // suppressed the hoist), the declaration is dead: evaluation is empty.
    if let EnvRecord::Declarative(block_env) = &*lexical_env {
        if block_env.annex_b_hoists(&name) {
            let fobj = lexical_env.get_binding_value(&name, false)?;
            variable_env.set_mutable_binding(&name, fobj, false)?;
            return Ok(());
        }
        if lexical_env.has_binding(&name)? {
            return Ok(());
        }
    }

    // Annex B statement-position form: `if (x) function f(){}` binds the
    // variable environment at evaluation. The binding was created (or
    // deliberately not created) by FunctionDeclarationInstantiation; a
    // parameter binding is never overwritten.
    if !strict && f.statement_position {
        if !variable_env.has_binding(&name)? || variable_env.is_parameter_binding(&name) {
            return Ok(());
        }
        let func_obj = crate::function::instantiate_function(
            agent,
            f,
            lexical_env,
            strict,
            Vec::new(),
            Vec::new(),
            false,
        )?;
        variable_env.set_mutable_binding(&name, func_obj, false)?;
        return Ok(());
    }

    // A top-level function declaration: declaration instantiation created
    // and initialized the binding; evaluation is empty (spec 15.2.6). A
    // binding deleted at runtime stays deleted — the declaration must not
    // re-create it.
    Ok(())
}

/// ClassDeclaration evaluation (spec 15.7.14 BindingClassDeclarationEvaluation):
/// ClassDefinitionEvaluation binds the class in its own scope, then the
/// declaration's outer binding (created uninitialized by declaration
/// instantiation) is initialized with the constructor.
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
    let class_value = crate::class::class_definition_evaluation(agent, class, Some(name), strict)?;
    let name = crux::lookup(name);
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
    eval_block_stmts(agent, &block.stmts, strict)
}

/// The shared body of `eval_block` and the block-like statement lists of
/// try/finally/switch-case: instantiate in a fresh declarative env and run.
fn eval_block_stmts(
    agent: &mut Agent,
    stmts: &[Stmt],
    strict: bool,
) -> Result<Completion, JsError> {
    let old_env = agent.running_context()?.lexical_environment;
    let block_env = new_declarative_environment(Some(old_env));
    block_declaration_instantiation(agent, stmts, &block_env, strict)?;
    agent.running_context_mut()?.lexical_environment = block_env;
    let result = eval_statement_list(agent, stmts, strict);
    agent.running_context_mut()?.lexical_environment = old_env;
    result
}

/// BlockDeclarationInstantiation (spec 14.2.4): create the block's lexical
/// bindings uninitialized; block-level function declarations are instantiated
/// and initialized immediately.
pub(crate) fn block_declaration_instantiation(
    agent: &mut Agent,
    stmts: &[Stmt],
    block_env: &EnvRef,
    strict: bool,
) -> Result<(), JsError> {
    block_declaration_instantiation_iter(agent, stmts.iter(), block_env, strict)
}

/// Like `block_declaration_instantiation`, over an arbitrary statement
/// iterator (a switch's case consequents share one block scope).
pub(crate) fn block_declaration_instantiation_iter<'a>(
    agent: &mut Agent,
    stmts: impl Iterator<Item = &'a Stmt>,
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
                        if *kind == VarDeclKind::Const
                            || matches!(*kind, VarDeclKind::Using | VarDeclKind::AwaitUsing)
                        {
                            // `using` bindings are immutable like `const`
                            // (spec 15.14.2).
                            block_env.create_immutable_binding(&name, true)?;
                        } else {
                            block_env.create_mutable_binding(&name, false)?;
                        }
                    }
                }
            }
            StmtKind::UsingDecl { decls, .. } => {
                for decl in decls {
                    let mut names = Vec::new();
                    bound_names(&decl.pattern, &mut names);
                    for name in names {
                        block_env.create_immutable_binding(&name, true)?;
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
                    let already_bound = block_env.has_binding(&name)?;
                    // Annex B.3.2.1: a sloppy block hoists its function
                    // declarations to the variable environment — resetting an
                    // existing binding to *undefined* — unless the block
                    // already binds the name. The binding itself was created
                    // (or deliberately not created) by the enclosing
                    // declaration instantiation (B.3.3.x), which applied the
                    // early-error checks; only set it here. The hoist covers
                    // plain FunctionDeclarations only — generator and async
                    // declarations stay block-scoped.
                    if !strict && !already_bound && !f.is_async && !f.is_generator {
                        let variable_env = agent.running_context()?.variable_environment;
                        // Only names whose hoist B.3.3.x deemed applicable
                        // (no enclosing lexical conflict) are marked; the
                        // binding exists when the hoist applies.
                        let hoistable = agent
                            .running_context()?
                            .annex_b_hoistable
                            .borrow()
                            .contains(&(f.span.start, f.span.end));
                        let hoisted = if !hoistable {
                            false
                        } else {
                            match &*variable_env {
                                EnvRecord::Global(_) => {
                                    if variable_env.has_binding(&name)?
                                        && !variable_env.has_lexical_declaration(&name)
                                    {
                                        variable_env.set_mutable_binding(
                                            &name,
                                            Value::Undefined,
                                            false,
                                        )?;
                                        true
                                    } else {
                                        false
                                    }
                                }
                                _ if variable_env.has_binding(&name)? => {
                                    variable_env.set_mutable_binding(
                                        &name,
                                        Value::Undefined,
                                        false,
                                    )?;
                                    true
                                }
                                _ => false,
                            }
                        };
                        if hoisted {
                            block_env.add_annex_b_function(name.clone());
                        }
                    }
                    if !already_bound {
                        block_env.create_mutable_binding(&name, false)?;
                        let func_obj = crate::function::instantiate_function(
                            agent,
                            f,
                            *block_env,
                            strict,
                            Vec::new(),
                            Vec::new(),
                            false,
                        )?;
                        block_env.initialize_binding(&name, func_obj)?;
                    }
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
            other => return Ok(other.update_empty(iteration_result)),
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
            other => return Ok(other.update_empty(iteration_result)),
        }
        let test_value = eval_expr(agent, test, strict)?;
        if !to_boolean(&test_value) {
            return Ok(Completion::Normal(iteration_result));
        }
    }
}

/// CreatePerIterationEnvironment (spec 14.7.4.3): a fresh declarative
/// environment over the current one's outer, with the per-iteration bindings
/// copied from the current environment's values. Closures created in the
/// body capture this env, giving each iteration its own `let`/`const`
/// bindings instead of one shared loop environment.
fn create_per_iteration_environment(
    agent: &mut Agent,
    per_iteration: &[crux::string::JsString],
) -> Result<EnvRef, JsError> {
    let last = agent.running_context()?.lexical_environment;
    let outer = last.outer().ok_or_else(|| {
        JsError::new(
            ErrorKind::ReferenceError,
            "No outer environment for per-iteration bindings".into(),
        )
    })?;
    let env = new_declarative_environment(Some(outer));
    for name in per_iteration {
        let value = last.get_binding_value(name, false)?;
        env.create_mutable_binding(name, false)?;
        env.initialize_binding(name, value)?;
    }
    Ok(env)
}

/// ForStatement evaluation (spec 14.7.4): `let`/`const` heads bind in a
/// fresh loop environment; each iteration gets its own copy of the head's
/// bindings so closures capture per-iteration values (spec 14.7.4.3).
fn eval_for(
    agent: &mut Agent,
    init: Option<&ForInit>,
    test: Option<&syntax::ast::Expr>,
    update: Option<&syntax::ast::Expr>,
    body: &Stmt,
    strict: bool,
    labels: &[crux::string::AtomId],
) -> Result<Completion, JsError> {
    let old_env = agent.running_context()?.lexical_environment;
    let mut fresh_env: Option<EnvRef> = None;
    let mut per_iteration: Vec<crux::string::JsString> = Vec::new();
    let head_result = match init {
        None => Ok(()),
        Some(ForInit::Expr(expr)) => eval_expr(agent, expr, strict).map(|_| ()),
        Some(ForInit::VarDecl { kind, decls }) => {
            if *kind == VarDeclKind::Var {
                eval_var_declarations(agent, decls, strict)
            } else {
                // spec 14.7.4.2 steps 2-8: the loop environment is the running
                // environment while the lexical declaration's initializers
                // run (closures capture it), and `const` heads share one
                // binding while `let`/`using` heads get per-iteration copies.
                let env = new_declarative_environment(Some(old_env));
                for decl in decls {
                    let mut names = Vec::new();
                    crate::script::bound_names(&decl.pattern, &mut names);
                    for name in &names {
                        if *kind == VarDeclKind::Const
                            || matches!(*kind, VarDeclKind::Using | VarDeclKind::AwaitUsing)
                        {
                            // `using` heads bind immutably like `const` and,
                            // like `const`, get no per-iteration copies.
                            env.create_immutable_binding(name, true)?;
                        } else {
                            env.create_mutable_binding(name, false)?;
                        }
                    }
                    if *kind != VarDeclKind::Const
                        && !matches!(*kind, VarDeclKind::Using | VarDeclKind::AwaitUsing)
                    {
                        crate::script::bound_names(&decl.pattern, &mut per_iteration);
                    }
                }
                agent.running_context_mut()?.lexical_environment = env;
                fresh_env = Some(env);
                let kind = match *kind {
                    VarDeclKind::Using => DisposalKind::Sync,
                    VarDeclKind::AwaitUsing => DisposalKind::Async,
                    _ => DisposalKind::Normal,
                };
                let result = eval_lexical_binding_list(agent, decls, strict, kind);
                if let Err(error) = result {
                    // spec 14.7.4.2 step 9: an abrupt head disposes the loop
                    // environment's resources before propagating.
                    let env = agent.running_context()?.lexical_environment;
                    let disposed = dispose_env_resources(agent, &env, Err(error));
                    agent.running_context_mut()?.lexical_environment = old_env;
                    return disposed;
                }
                Ok(())
            }
        }
    };
    if let Err(error) = head_result {
        if fresh_env.is_some() {
            agent.running_context_mut()?.lexical_environment = old_env;
        }
        return Err(error);
    }
    // spec 14.7.4.2 step 2: the first per-iteration environment is created
    // before the first test; the loop installs a fresh copy each iteration.
    if !per_iteration.is_empty() {
        let env = create_per_iteration_environment(agent, &per_iteration)?;
        agent.running_context_mut()?.lexical_environment = env;
    }
    let mut iteration_result = Value::Undefined;
    let result = loop {
        if let Some(test) = test {
            let test_value = eval_expr(agent, test, strict)?;
            if !to_boolean(&test_value) {
                break Ok(Completion::Normal(iteration_result));
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
                break Ok(Completion::Normal(value.unwrap_or(iteration_result)));
            }
            Completion::Break {
                target: Some(l),
                value,
            } if labels.contains(&l) => {
                break Ok(Completion::Break {
                    target: Some(l),
                    value: Some(value.unwrap_or(iteration_result)),
                });
            }
            other => break Ok(other.update_empty(iteration_result)),
        }
        // spec 14.7.4.2 step 5: a fresh environment is created *before* the
        // increment runs, so closures capture the unmutated per-iteration
        // binding and the increment targets the next iteration's copy.
        if !per_iteration.is_empty() {
            let env = create_per_iteration_environment(agent, &per_iteration)?;
            agent.running_context_mut()?.lexical_environment = env;
        }
        if let Some(update) = update {
            eval_expr(agent, update, strict)?;
        }
    };
    if let Some(fresh_env) = fresh_env {
        // spec 14.7.4.2 step 13: the loop environment's `using` resources are
        // disposed when the loop completes.
        let result = dispose_env_resources(agent, &fresh_env, result);
        agent.running_context_mut()?.lexical_environment = old_env;
        result
    } else {
        result
    }
}

/// ForIn/ForOfBodyEvaluation: the enumerable string keys of `rhs` and its
/// prototype chain, in walk order (spec 14.7.5.6 steps 2-6), each tagged with
/// the prototype-chain level it was found at, so a key deleted during
/// enumeration can be re-checked against its own level at visit time (spec
/// EnumerateObjectProperties). Shared with the resumable-function IR's
/// `ForInBegin` step.
pub(crate) fn for_in_key_levels(
    agent: &mut Agent,
    rhs: &Value,
) -> Result<Vec<(usize, Value)>, JsError> {
    // GC-2: the keys are freshly-boxed `Value::String` handles collected
    // into a local Vec the conservative stack scan cannot see, and each
    // `Handle::new` fires a per-allocation stress collection — suppress the
    // window so the half-built Vec cannot be swept (the caller roots it once
    // it lands in the Vm's `for_in_stack`).
    let _stress = crate::ir::StressSuppress::new();
    for_in_key_levels_inner(agent, rhs)
}

fn for_in_key_levels_inner(agent: &mut Agent, rhs: &Value) -> Result<Vec<(usize, Value)>, JsError> {
    // PropertyKey hashes are content-stable (a rope description's first hash
    // materializes its flat cache, which never changes the hash output).
    #[allow(clippy::mutable_key_type)]
    let mut seen: HashSet<PropertyKey> = HashSet::new();
    let mut keys: Vec<(usize, Value)> = Vec::new();
    // ToObject of the enumerated value (spec step 2): functions box to
    // themselves, so a callable receiver enumerates its own properties too.
    if let Some(obj) = crate::context::as_object(rhs) {
        let mut current = Some(obj);
        let mut level = 0;
        while let Some(obj) = current {
            for key in obj.own_property_keys()? {
                let PropertyKey::String(_) = key else {
                    continue;
                };
                if !seen.insert(key.clone()) {
                    continue;
                }
                if let Some(property) = obj.get_own_property_key(&key)? {
                    // EnumerateObjectProperties reads each descriptor
                    // (spec 9.4.6.4 step 4): a module namespace descriptor
                    // reads the live binding, throwing a ReferenceError for
                    // an uninitialized export.
                    if matches!(obj.kind, crux::object::ObjectKind::ModuleNamespace(_))
                        && let Some(module) = agent.module_namespaces.get(&obj.id()).cloned()
                        && let PropertyKey::String(id) = &key
                    {
                        crate::module::namespace_get(agent, &module, &crux::lookup(*id))?;
                    }
                    if property.enumerable {
                        keys.push((level, key_value(&key)));
                    }
                }
            }
            current = obj.get_prototype_of()?;
            level += 1;
        }
    } else if let Some(text) = rhs.as_string() {
        // ToObject of a primitive string: its own enumerable index keys.
        for index in 0..text.len() {
            keys.push((
                0,
                Value::String(Handle::new(JsString::from_utf8(&index.to_string()))),
            ));
        }
    } else if matches!(rhs.kind(), ValueKind::Undefined | ValueKind::Null) {
        return Ok(Vec::new());
    }
    Ok(keys)
}

/// Whether `key` (a string value) is still an enumerable own property of the
/// `level`-th object in `obj`'s prototype chain (a key deleted during
/// enumeration is skipped — spec EnumerateObjectProperties step 5.a.v).
pub(crate) fn key_enumerable_at_level(
    obj: &Handle<crux::object::JsObject>,
    level: usize,
    key: &Value,
) -> Result<bool, JsError> {
    let ValueKind::String(key) = key.kind() else {
        return Ok(false);
    };
    let key = PropertyKey::from_js_string(key.as_ref());
    let mut current = Some(*obj);
    for _ in 0..level {
        let Some(next) = current else {
            return Ok(false);
        };
        current = next.get_prototype_of()?;
    }
    let Some(obj) = current else {
        return Ok(false);
    };
    match obj.get_own_property_key(&key)? {
        Some(property) => Ok(property.enumerable),
        None => Ok(false),
    }
}

/// ForIn/OfHeadEvaluation (spec 14.7.5.5): the RHS runs in a TDZ environment
/// that uninitializedly binds the lexical head names, so a reference to them
/// from the head expression throws.
fn eval_for_head(
    agent: &mut Agent,
    left: &ForBinding,
    right: &syntax::ast::Expr,
    strict: bool,
) -> Result<Value, JsError> {
    let tdz_names = match left {
        ForBinding::VarDecl { kind, pattern, .. } if *kind != VarDeclKind::Var => {
            let mut names = Vec::new();
            crate::script::bound_names(pattern, &mut names);
            names
        }
        _ => Vec::new(),
    };
    if tdz_names.is_empty() {
        return eval_expr(agent, right, strict);
    }
    let old_env = agent.running_context()?.lexical_environment;
    let tdz_env = new_declarative_environment(Some(old_env));
    for name in &tdz_names {
        tdz_env.create_mutable_binding(name, false)?;
    }
    agent.running_context_mut()?.lexical_environment = tdz_env;
    let result = eval_expr(agent, right, strict);
    agent.running_context_mut()?.lexical_environment = old_env;
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
    // Annex B.2.6: `for (var a = init in expr)` — the initializer runs once
    // and binds `a` before the RHS is evaluated (sloppy mode only).
    if let ForBinding::VarDecl {
        kind: VarDeclKind::Var,
        pattern,
        init: Some(init_expr),
        ..
    } = left
    {
        let init_value = eval_expr(agent, init_expr, strict)?;
        crate::binding::binding_initialization(agent, pattern, init_value, None, strict)?;
    }
    let rhs = eval_for_head(agent, left, right, strict)?;
    let base_object = crate::context::as_object(&rhs);
    let keys = for_in_key_levels(agent, &rhs)?;
    let mut iteration_result = Value::Undefined;
    // GC-2: the loop holds the keys Vec (a native heap buffer the stack
    // scan cannot see) across the per-iteration binding and body evaluation,
    // which allocate — suppress `--gc-stress` for the loop so the not-yet-
    // consumed keys cannot be swept.
    let _stress = crate::ir::StressSuppress::new();
    for (level, key) in keys {
        if let Some(base) = &base_object
            && !key_enumerable_at_level(base, level, &key)?
        {
            continue;
        }
        let (restore, iteration_env) = for_binding_put(agent, left, key, strict)?;
        let result = eval_statement(agent, body, strict);
        let completion = if let Some(iteration_env) = iteration_env {
            let result = dispose_env_resources(agent, &iteration_env, result);
            if let Some(outer) = restore {
                agent.running_context_mut()?.lexical_environment = outer;
            }
            result?
        } else {
            result?
        };
        match completion {
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
            other => return Ok(other.update_empty(iteration_result)),
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
/// environment is installed for `let`/`const`/`using` heads; the caller
/// restores it and disposes its resources after the body. Returns the outer
/// environment to restore and the iteration environment to dispose.
fn for_binding_put(
    agent: &mut Agent,
    left: &ForBinding,
    value: Value,
    strict: bool,
) -> Result<(Option<EnvRef>, Option<EnvRef>), JsError> {
    match left {
        ForBinding::Expr(expr) => {
            // Destructuring heads (`for ([a, b] of …)`) are assignment
            // patterns, not references (spec 14.7.6 step 6.b).
            if matches!(&expr.kind, ExprKind::Array(_) | ExprKind::Object(_)) {
                crate::binding::destructuring_assignment(agent, expr, value, strict)?;
                return Ok((None, None));
            }
            let reference = eval_reference(agent, expr, strict)?;
            put_value(agent, &reference, value)?;
            Ok((None, None))
        }
        ForBinding::VarDecl { kind, pattern, .. } => {
            if *kind == VarDeclKind::Var {
                // BindingInitialization with no environment: the hoisted var
                // binding is resolved and PutValue'd (spec 14.7.5.6 step 5.b).
                crate::binding::binding_initialization(agent, pattern, value, None, strict)?;
                Ok((None, None))
            } else {
                let outer = agent.running_context()?.lexical_environment;
                let env = new_declarative_environment(Some(outer));
                let mut names = Vec::new();
                crate::script::bound_names(pattern, &mut names);
                for name in &names {
                    if *kind == VarDeclKind::Const
                        || matches!(*kind, VarDeclKind::Using | VarDeclKind::AwaitUsing)
                    {
                        env.create_immutable_binding(name, true)?;
                    } else {
                        env.create_mutable_binding(name, false)?;
                    }
                }
                // spec 14.7.5.6 step 5.e: the iteration environment is the
                // running environment while BindingInitialization runs, so a
                // default initializer's closure captures the per-iteration
                // binding.
                agent.running_context_mut()?.lexical_environment = env;
                if matches!(*kind, VarDeclKind::Using | VarDeclKind::AwaitUsing) {
                    // ForIn/OfBodyEvaluation: AddDisposableResource runs before
                    // InitializeReferencedBinding (spec 14.7.5.6 step 5.g).
                    let kind = if *kind == VarDeclKind::AwaitUsing {
                        DisposalKind::Async
                    } else {
                        DisposalKind::Sync
                    };
                    let resource = create_disposable_resource(agent, &value, kind)?;
                    env.add_disposable_resource(resource);
                }
                crate::binding::binding_initialization(agent, pattern, value, Some(&env), strict)?;
                Ok((Some(outer), Some(env)))
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
    let rhs = eval_for_head(agent, left, right, strict)?;
    let iterator = get_iterator(agent, &rhs)?;
    let mut iteration_result = Value::Undefined;
    loop {
        let Some(value) = iterator_step(agent, &iterator)? else {
            return Ok(Completion::Normal(iteration_result));
        };
        // A destructuring-assignment error in the head closes the iterator
        // (ForIn/OfBodyEvaluation: abrupt status → IteratorClose); only a
        // throwing `return` replaces the error (spec 7.4.11).
        let (restore, iteration_env) = match for_binding_put(agent, left, value, strict) {
            Ok(restore) => restore,
            Err(error) => {
                crate::expr::iterator_close_throw(agent, &iterator)?;
                return Err(error);
            }
        };
        let result = eval_statement(agent, body, strict);
        let completion = match result {
            Ok(completion) => completion,
            Err(error) => {
                if let Some(iteration_env) = iteration_env {
                    let _ = dispose_env_resources(agent, &iteration_env, Err(error.clone()));
                }
                if let Some(outer) = restore {
                    agent.running_context_mut()?.lexical_environment = outer;
                }
                crate::expr::iterator_close_throw(agent, &iterator)?;
                return Err(error);
            }
        };
        let completion = if let Some(iteration_env) = iteration_env {
            let result = dispose_env_resources(agent, &iteration_env, Ok(completion));
            if let Some(outer) = restore {
                agent.running_context_mut()?.lexical_environment = outer;
            }
            result?
        } else {
            completion
        };
        match completion {
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
            Completion::Throw(value) => {
                // A throwing body closes the iterator with the throw
                // completion: the original error wins over a throwing `return`
                // method or `return` lookup (spec 7.4.11 steps 6-7).
                crate::expr::iterator_close_throw(agent, &iterator)?;
                return Ok(Completion::Throw(value));
            }
            other => {
                iterator_close(agent, &iterator)?;
                return Ok(other.update_empty(iteration_result));
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

/// SwitchStatement evaluation (spec 14.13.2): case tests run in order until a
/// match; consequents fall through until `break`.
fn eval_switch(
    agent: &mut Agent,
    discriminant: &syntax::ast::Expr,
    cases: &[syntax::ast::SwitchCase],
    strict: bool,
) -> Result<Completion, JsError> {
    // spec 14.13.2 steps 1-5: the discriminant is evaluated in the enclosing
    // environment, then a fresh CaseBlock scope is created and installed
    // before the selectors run, so selectors and consequents see the case
    // block's lexical declarations.
    let discriminant = eval_expr(agent, discriminant, strict)?;
    let old_env = agent.running_context()?.lexical_environment;
    let case_env = new_declarative_environment(Some(old_env));
    block_declaration_instantiation_iter(
        agent,
        cases.iter().flat_map(|c| c.consequent.iter()),
        &case_env,
        strict,
    )?;
    agent.running_context_mut()?.lexical_environment = case_env;
    let result = (|| -> Result<Completion, JsError> {
        // Find the matching case, evaluating selectors in order (spec
        // 14.13.3 CaseClauseIsSelected).
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
        // CaseBlockEvaluation (spec 14.13.3): a running result value V is
        // carried across the executed case clauses; each clause's value
        // replaces it when non-empty, and an abrupt completion is UpdateEmpty'd
        // with V before it propagates.
        let mut result_value = Value::Undefined;
        for case in &cases[start..] {
            let completion = eval_statement_list(agent, &case.consequent, strict)?;
            match completion {
                Completion::Empty => {}
                Completion::Normal(value) => result_value = value,
                Completion::Break {
                    target: None,
                    value,
                } => {
                    // The switch consumes an unlabeled break, carrying the
                    // running V when the break's own value is empty
                    // (LabelledEvaluation of BreakableStatement).
                    return Ok(Completion::Normal(value.unwrap_or(result_value)));
                }
                other => return Ok(other.update_empty(result_value)),
            }
        }
        Ok(Completion::Normal(result_value))
    })();
    agent.running_context_mut()?.lexical_environment = old_env;
    result
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
    let result = eval_block_stmts(agent, &block.stmts, strict);
    let handled = match result {
        Ok(Completion::Throw(value)) => match handler {
            Some(handler) => eval_catch(agent, handler, value, strict)?,
            None => Completion::Throw(value),
        },
        Ok(other) => other,
        Err(error) => match handler {
            // Engine errors become real Error objects (spec ch. 17).
            Some(handler) => {
                let thrown = crate::builtins::error::to_throwable(agent, &error)?;
                eval_catch(agent, handler, thrown, strict)?
            }
            None => return run_finalizer(agent, finalizer, Err(error), strict),
        },
    };
    run_finalizer(agent, finalizer, Ok(handled), strict)
}

fn run_finalizer(
    agent: &mut Agent,
    finalizer: Option<&syntax::ast::Block>,
    result: Result<Completion, JsError>,
    strict: bool,
) -> Result<Completion, JsError> {
    let Some(finalizer) = finalizer else {
        // spec 14.15.2/14.15.3: TryStatement returns ? UpdateEmpty(C,
        // *undefined*) even without a finalizer.
        return result.map(|completion| completion.update_empty(Value::Undefined));
    };
    match eval_block_stmts(agent, &finalizer.stmts, strict)? {
        Completion::Normal(_) | Completion::Empty => {
            // F is normal: the try's result (block/catch) is the answer,
            // still UpdateEmpty'd with *undefined* (spec 14.15.2 step 4).
            result.map(|completion| completion.update_empty(Value::Undefined))
        }
        other => Ok(other.update_empty(Value::Undefined)),
    }
}

fn eval_catch(
    agent: &mut Agent,
    handler: &syntax::ast::CatchClause,
    thrown: Value,
    strict: bool,
) -> Result<Completion, JsError> {
    let old_env = agent.running_context()?.lexical_environment;
    // spec 18.2.3: the catch parameter binds in its own declarative
    // environment; the body is a fresh block env over it, so a body function
    // named like the parameter gets its own block binding (Annex B).
    let param_env = new_declarative_environment(Some(old_env));
    param_env.mark_catch_param_env();
    agent.running_context_mut()?.lexical_environment = param_env;
    if let Some(param) = &handler.param {
        // The catch parameter's bound names are created uninitialized, then
        // BindingInitialization fills them (the parameter may be a
        // destructuring pattern). The running env is the parameter env so
        // closures created by default initializers capture it (spec
        // 15.1.7 step 7).
        let mut names = Vec::new();
        crate::script::bound_names(param, &mut names);
        for name in &names {
            param_env.create_mutable_binding(name, false)?;
        }
        crate::binding::binding_initialization(agent, param, thrown, Some(&param_env), strict)?;
    }
    let body_env = new_declarative_environment(Some(param_env));
    block_declaration_instantiation(agent, &handler.body.stmts, &body_env, strict)?;
    agent.running_context_mut()?.lexical_environment = body_env;
    let result = eval_statement_list(agent, &handler.body.stmts, strict);
    agent.running_context_mut()?.lexical_environment = old_env;
    result
}

/// WithStatement evaluation (spec 14.15.4): an object environment whose
/// bindings are the with-object's properties; primitives are boxed (step 2:
/// ToObject).
fn eval_with(
    agent: &mut Agent,
    object: &syntax::ast::Expr,
    body: &Stmt,
    strict: bool,
) -> Result<Completion, JsError> {
    let object_value = eval_expr(agent, object, strict)?;
    let object_value = crate::context::to_object(agent, &object_value)?;
    let obj = crate::context::as_object(&object_value).ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "Cannot use 'with' on a non-object value".into(),
        )
    })?;
    let old_env = agent.running_context()?.lexical_environment;
    let with_env = new_object_environment(obj, true, Some(old_env));
    agent.running_context_mut()?.lexical_environment = with_env;
    let result = eval_statement(agent, body, strict);
    agent.running_context_mut()?.lexical_environment = old_env;
    // spec 14.15.4 step 8: UpdateEmpty(C, *undefined*).
    result.map(|completion| completion.update_empty(Value::Undefined))
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

    /// Run `source` on a thread with a large native stack and check its
    /// completion value. The interpreter collapses only SELF tail calls:
    /// the mutual `even`/`odd` recursion below grows a fresh interpreter
    /// frame set per call (~160 KB per level in the unoptimized test build),
    /// so `even(10)`'s ~12 levels overflow the default 2 MiB test-thread
    /// stack on some targets. The `Value` cannot cross threads, so the check
    /// runs here and only the boolean crosses back.
    fn run_deep(source: &'static str, check: impl Fn(&Value) -> bool + Send + 'static) -> bool {
        std::thread::Builder::new()
            .name("deep-recursion".into())
            .stack_size(64 << 20)
            .spawn(move || run(source).is_ok_and(|value| check(&value)))
            .expect("spawns the deep-recursion thread")
            .join()
            .expect("the deep-recursion thread panicked")
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
            source: crux::string::JsString::from_utf8(""),
            jsx: false,
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
        let global = agent.running_context().unwrap().realm.global_object;
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
        let global = agent.running_context().unwrap().realm.global_object;
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
        let global = agent.running_context().unwrap().realm.global_object;
        let value = global.get(&JsString::from_utf8("f")).unwrap();
        assert!(matches!(value.kind(), ValueKind::Function(_)));
    }

    #[test]
    fn class_declarations_create_lexical_bindings() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent.run_script("class C {}").unwrap();
        let global = agent.running_context().unwrap().realm.global_object;
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
        assert!(matches!(arr.kind(), ValueKind::Object(_)));
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
        // Non-object, non-null __proto__ values set neither the prototype nor
        // an own property (B.3.1 step 6): the definition is a no-op, so
        // `__proto__` reads the %Object.prototype% accessor (the prototype).
        assert_eq!(
            run("let o = { __proto__: 5 }; o.__proto__ === Object.prototype && !o.hasOwnProperty('__proto__')")
                .unwrap(),
            Value::Boolean(true)
        );
        // Shorthand __proto__ is an ordinary data property, and a duplicate
        // shorthand is permitted (Annex B.3.1); the second one wins.
        assert_eq!(
            run("var __proto__ = 2; \
                 var obj = { __proto__, __proto__ }; \
                 obj.hasOwnProperty('__proto__') + ',' + obj.__proto__")
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("true,2")))
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
                let a = match args.first().map(|v| v.kind()) {
                    Some(ValueKind::Number(n)) => n,
                    _ => 0.0,
                };
                let b = match args.get(1).map(|v| v.kind()) {
                    Some(ValueKind::Number(n)) => n,
                    _ => 0.0,
                };
                Ok(Value::Number(a + b))
            }),
            None,
            None,
        )
        .unwrap();
        let global = agent.running_context().unwrap().realm.global_object;
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
            Box::new(|this, _| match this.kind() {
                ValueKind::Object(obj) => obj.get(&JsString::from_utf8("x")),
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
        let proto_for_ctor = proto;
        let ctor = crux::Function::create_builtin(
            Some(JsString::from_utf8("C")),
            0,
            Box::new(|_, _| Ok(Value::Undefined)),
            Some(Box::new(move |_, _| {
                Ok(Value::Object(
                    crux::object::JsObject::ordinary_object_create(Some(proto_for_ctor)),
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
        let global = agent.running_context().unwrap().realm.global_object;
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
    fn construct_observes_warm_prototype_value_writes() {
        // The constructor's `prototype` read is served by the (function id,
        // "prototype") member value cell when warm (the L1c record-
        // discipline oracle), so every `new C()` must observe the CURRENT
        // `C.prototype` — including across repeated WARM in-place stores
        // (the second `C.prototype = ...` is an L1a warm write that fronts
        // the value cell without re-resolving the property).
        let source = "function C() { this.x = 1; }\n\
                     C.prototype = { a: 1 };\n\
                     C.prototype = { a: 2 };\n\
                     var o1 = new C();\n\
                     C.prototype = { a: 3 };\n\
                     var o2 = new C();\n\
                     var o3 = new C();\n\
                     Object.getPrototypeOf(o1).a + ',' + Object.getPrototypeOf(o2).a + ',' + Object.getPrototypeOf(o3).a;";
        assert_eq!(
            run(source).unwrap(),
            Value::String(Handle::new(JsString::from_utf8("2,3,3")))
        );
        // A class constructor (non-writable prototype) and a function whose
        // prototype is redefined as an accessor stay exact through the full
        // Get path.
        assert_eq!(
            run("class D {}; var d = new D(); Object.getPrototypeOf(d) === D.prototype").unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn warm_stores_across_many_distinct_objects_keep_separate_cells() {
        // The L1a store cells live in a SEPARATE, larger direct-mapped table
        // (MEMBER_WRITE_CELLS) than the read-side value cells (MEMBER_CELLS):
        // a store-cell miss falls back to the full [[Set]], so more than 16
        // distinct (object, name) warm-store pairs must not alias each other
        // out of the cache. Regression guard for the split-index discipline:
        // the store probe/record indexes the write table by its own mask
        // while the value-cell front keeps the read table's mask (a shared
        // index once wrote the 16-wide read table at a >16 write index and
        // panicked out of bounds).
        let source = "var os = [];\n\
                     for (var j = 0; j < 64; j++) { os[j] = { x: j }; }\n\
                     for (var k = 0; k < 64; k++) { os[k].x = k + 1; }\n\
                     for (var k = 0; k < 64; k++) { os[k].x = os[k].x + 1000; }\n\
                     os[0].x + '|' + os[31].x + '|' + os[63].x;";
        assert_eq!(
            run(source).unwrap(),
            Value::String(Handle::new(JsString::from_utf8("1001|1032|1064")))
        );
    }

    #[test]
    fn chain_reads_observe_warm_value_writes_to_the_found_link() {
        // The prototype-chain read cache re-reads the found property LIVE
        // (through the found link's member value cell when warm, else the
        // recorded vector slot) instead of returning a cached value, so a
        // chain read must observe a VALUE write to the found prototype
        // link's own property — including repeated warm in-place stores.
        // First link and a two-link chain.
        let source = "function A() {}\n\
                     A.prototype.m = function () { return 1; };\n\
                     function B() {}\n\
                     B.prototype = Object.create(A.prototype);\n\
                     B.prototype.m = function () { return 10; };\n\
                     var out = [];\n\
                     var a = new A();\n\
                     var b = new B();\n\
                     for (var i = 0; i < 3; i++) {\n\
                       out.push(a.m());\n\
                       out.push(b.m());\n\
                       if (i === 0) {\n\
                         A.prototype.m = function () { return 2; };\n\
                         B.prototype.m = function () { return 20; };\n\
                       }\n\
                     }\n\
                     out.join(',');";
        assert_eq!(
            run(source).unwrap(),
            Value::String(Handle::new(JsString::from_utf8("1,10,2,20,2,20")))
        );
        // A two-link chain read whose value lives on the second link
        // observes a warm write there.
        assert_eq!(
            run("function C() {}\n\
                C.prototype = { base: 5 };\n\
                function D() {}\n\
                D.prototype = Object.create(C.prototype);\n\
                var d = new D();\n\
                var r = [d.base];\n\
                C.prototype.base = 9;\n\
                r.push(d.base);\n\
                r.join(',');")
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("5,9")))
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
        let iterator_for_method = iterator;
        iterable
            .define_property_key(
                &crux::property::PropertyKey::Symbol(crux::symbol::well_known("iterator")),
                &crux::property::PropertyDescriptor::data(Value::Function(
                    crux::Function::create_builtin(
                        Some(JsString::from_utf8("[Symbol.iterator]")),
                        0,
                        Box::new(move |_, _| Ok(Value::Object(iterator_for_method))),
                        None,
                        None,
                    )
                    .unwrap(),
                )),
            )
            .unwrap();
        let global = agent.running_context().unwrap().realm.global_object;
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
        // `in` works on object RHS; instanceof works once the Phase 8
        // built-ins install Object and %Function.prototype%.@@hasInstance%.
        assert_eq!(run("({}) instanceof Object").unwrap(), Value::Boolean(true));
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
    fn optional_call_nullish_callee_short_circuits() {
        // A nullish callee short-circuits the whole chain to *undefined*
        // without evaluating the arguments or calling (spec 13.3.7.1). The
        // compiled path's optional-call tail must pop the receiver, the
        // callee, and the dup'd callee: a leaked receiver shifts every later
        // stack read (regression: `null?.(1) === undefined` threw).
        assert_eq!(run("let f; f?.(1)").unwrap(), Value::Undefined);
        assert_eq!(
            run("let f; f?.(1) === undefined").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("let o = { m: null }; o.m?.(1) === undefined").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("let o = null; o?.m?.(1) === undefined").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("let o = { m() { return 42; } }; o.m?.(1)").unwrap(),
            Value::Number(42.0)
        );
        assert_eq!(
            run("let o = { m() { return this.x; }, x: 5 }; o.m?.(1)").unwrap(),
            Value::Number(5.0)
        );
        // The receiver must not leak into the surrounding statement either.
        assert_eq!(run("let f; f?.(1); 7").unwrap(), Value::Number(7.0));
    }

    #[test]
    fn mid_chain_optional_member_keeps_short_circuiting() {
        // A `?.` in the middle of a chain short-circuits the WHOLE chain:
        // the links after it (plain members, calls, indexes) are skipped, not
        // evaluated against the chain value (spec 13.4.3). Regression: the
        // object chain's depth bookkeeping cleared the short-circuit flag
        // before the next link's guard, so `a?.b.m` read `.m` of the chain
        // value (a TypeError) instead of yielding *undefined*.
        assert_eq!(
            run("let o = { a: null }; o.a?.b.m").unwrap(),
            Value::Undefined
        );
        assert_eq!(
            run("let o = { a: null }; o.a?.b.m(1)").unwrap(),
            Value::Undefined
        );
        assert_eq!(
            run("let o = { a: null }; o.a?.b[0].x").unwrap(),
            Value::Undefined
        );
        assert_eq!(
            run("let o = { a: { b: 5 } }; o.a?.b").unwrap(),
            Value::Number(5.0)
        );
        // Parentheses terminate the chain: the member/call on the
        // parenthesized value always runs (throwing on a nullish result).
        assert!(run("let o = { a: null }; (o.a?.b).c").is_err());
        assert!(run("let o = { a: null }; (o.a?.b)()").is_err());
        // The short-circuit flag must not leak past the statement: a later
        // guarded call still runs.
        assert_eq!(
            run("let o = { a: null }; let g = () => 3; o.a?.b; g() + 1").unwrap(),
            Value::Number(4.0)
        );
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

    // ---- Phase 4 binding semantics: hoisting, TDZ, redeclaration ----

    #[test]
    fn var_is_undefined_before_its_initializer_runs() {
        // var bindings hoist and initialize to *undefined* during
        // instantiation, before any statement runs (spec 16.1.8).
        assert_eq!(
            run("function f() { var y = x; var x = 1; return y; } f()").unwrap(),
            Value::Undefined
        );
        assert_eq!(
            run("function f() { return x; var x = 1; } f()").unwrap(),
            Value::Undefined
        );
    }

    #[test]
    fn function_declarations_hoist_above_use() {
        // Function declarations instantiate before the body runs, so a call
        // appearing before the declaration still resolves it.
        assert_eq!(
            run("f(); function f() { return 42; }").unwrap(),
            Value::Number(42.0)
        );
        assert_eq!(
            run("function g() { return f(); function f() { return 7; } } g()").unwrap(),
            Value::Number(7.0)
        );
    }

    #[test]
    fn var_hoists_out_of_blocks_to_function_scope() {
        assert_eq!(
            run("function f() { { var x = 1; } return x; } f()").unwrap(),
            Value::Number(1.0)
        );
    }

    #[test]
    fn let_and_const_are_in_tdz_until_initialized() {
        // Reading a binding before its declaration throws a ReferenceError,
        // including via `typeof` (spec 14.2.1 / 13.3.1).
        let err = run("{ typeof x; let x; }").unwrap_err();
        assert_eq!(err.kind, ErrorKind::ReferenceError);
        // A self-referencing initializer reads the binding while it is still
        // uninitialized.
        let err = run("let x = x;").unwrap_err();
        assert_eq!(err.kind, ErrorKind::ReferenceError);
        let err = run("const c = c;").unwrap_err();
        assert_eq!(err.kind, ErrorKind::ReferenceError);
    }

    #[test]
    fn for_let_headers_bind_lexically() {
        // A let-headed loop binds fresh per iteration and reads its own
        // binding correctly after initialization.
        assert_eq!(
            run("let s = 0; for (let i = 0; i < 3; i++) { s += i; } s").unwrap(),
            Value::Number(3.0)
        );
    }

    #[test]
    fn closures_created_before_let_initialization_hit_tdz() {
        // A closure created while a `let` is still uninitialized captures the
        // uninitialized binding; calling it throws (spec 14.2.1).
        let err = run("var f; f = function () { return x; }; f(); let x = 1;").unwrap_err();
        assert_eq!(err.kind, ErrorKind::ReferenceError);
        let err = run("function g() { let f = () => x; return f(); let x = 1; } g()").unwrap_err();
        assert_eq!(err.kind, ErrorKind::ReferenceError);
    }

    #[test]
    fn tdz_in_class_field_initializers() {
        // Instance field initializers run at construction; a field reading a
        // block-scoped binding still in its TDZ throws.
        let err = run("{ class C { f = x; } new C(); let x = 1; }").unwrap_err();
        assert_eq!(err.kind, ErrorKind::ReferenceError);
    }

    #[test]
    fn var_redeclaration_is_allowed() {
        assert_eq!(run("var a; var a; a = 5; a").unwrap(), Value::Number(5.0));
        assert_eq!(run("var a = 1; var a = 2; a").unwrap(), Value::Number(2.0));
    }

    #[test]
    fn function_and_var_declarations_may_share_a_name() {
        // `function f() {} var f;` — the var is absorbed by the function
        // declaration (spec 16.1.7).
        assert_eq!(
            run("function f() { return 1; } var f; f()").unwrap(),
            Value::Number(1.0)
        );
    }

    #[test]
    fn redeclaration_errors_are_syntax_errors() {
        // Duplicate lexical names, and lexical/var or lexical/function
        // clashes in the same statement list, are parse-time SyntaxErrors
        // (spec 14.2.1 / 15.2.6 / 16.1.7 early errors).
        for source in [
            "let a; let a;",
            "const a = 1; const a = 2;",
            "var a; let a;",
            "var a = 1; let a = 2;",
            "let f; function f() {}",
        ] {
            let err = run(source).unwrap_err();
            assert_eq!(err.kind, ErrorKind::SyntaxError, "{source}");
        }
    }

    #[test]
    fn var_declarations_create_global_object_properties() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent.run_script("var g = 1;").unwrap();
        let global = agent.running_context().unwrap().realm.global_object;
        assert_eq!(
            global.get(&JsString::from_utf8("g")).unwrap(),
            Value::Number(1.0)
        );
        assert_eq!(
            agent.run_script("globalThis.g").unwrap(),
            Value::Number(1.0)
        );
    }

    #[test]
    fn lexical_declarations_do_not_create_global_object_properties() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent.run_script("let g2 = 2;").unwrap();
        let global = agent.running_context().unwrap().realm.global_object;
        assert!(!global.has_own_property(&JsString::from_utf8("g2")).unwrap());
        assert_eq!(agent.run_script("globalThis.g2").unwrap(), Value::Undefined);
        assert_eq!(agent.run_script("g2").unwrap(), Value::Number(2.0));
    }

    #[test]
    fn sloppy_undeclared_assignment_creates_a_global_property() {
        // A sloppy write to an undeclared name inside a function still
        // creates a property on the global object (spec 9.2.1.5).
        assert_eq!(
            run("(function () { u = 1; })(); globalThis.u").unwrap(),
            Value::Number(1.0)
        );
    }

    #[test]
    fn strict_undeclared_assignment_throws() {
        let err = run("(function () { 'use strict'; v = 1; })()").unwrap_err();
        assert_eq!(err.kind, ErrorKind::ReferenceError);
    }

    // ---- Phase 8 eval scoping matrix ----

    #[test]
    fn direct_eval_sees_the_callers_local_vars() {
        assert_eq!(
            run("function f() { var x = 1; return eval('x'); } f()").unwrap(),
            Value::Number(1.0)
        );
    }

    #[test]
    fn direct_eval_declares_vars_in_the_callers_scope() {
        assert_eq!(
            run("function f() { eval('var y = 2;'); return y; } f()").unwrap(),
            Value::Number(2.0)
        );
    }

    #[test]
    fn indirect_eval_runs_in_global_scope() {
        // An indirect eval's vars land on the global object, visible to the
        // surrounding script afterwards.
        assert_eq!(
            run("(0, eval)('var z = 1;'); typeof z").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("number")))
        );
    }

    #[test]
    fn indirect_eval_does_not_see_caller_locals() {
        let err = run("function f() { var x = 1; return (0, eval)('x'); } f()").unwrap_err();
        assert_eq!(err.kind, ErrorKind::ReferenceError);
    }

    #[test]
    fn strict_eval_gets_its_own_variable_scope() {
        // A strict caller makes direct eval strict, so eval-declared vars
        // stay inside the eval's own environment (spec 19.2.1.1).
        assert_eq!(
            run("'use strict'; function f() { eval('var w = 1;'); return typeof w; } f()").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("undefined")))
        );
        // A "use strict" directive inside the eval'd source has the same
        // effect even when the caller is sloppy.
        assert_eq!(
            run("function f() { eval('\"use strict\"; var w = 1;'); return typeof w; } f()")
                .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("undefined")))
        );
    }

    #[test]
    fn eval_returns_its_completion_value() {
        assert_eq!(run("eval('1 + 2')").unwrap(), Value::Number(3.0));
        assert_eq!(run("eval('let e = 6; e + 1')").unwrap(), Value::Number(7.0));
    }

    // ---- Phase 4 block scoping ----

    #[test]
    fn block_let_shadows_outer_scope() {
        assert_eq!(
            run("let x = 1; { let x = 2; x }").unwrap(),
            Value::Number(2.0)
        );
        assert_eq!(
            run("let x = 1; { let x = 2; } x").unwrap(),
            Value::Number(1.0)
        );
    }

    #[test]
    fn for_let_creates_per_iteration_bindings() {
        // Each iteration gets a fresh `i`; closures capture the per-iteration
        // binding instead of the loop's final value (spec 14.7.4.3).
        assert_eq!(
            run("let fns = []; for (let i = 0; i < 3; i++) { fns.push(() => i); } fns[0]() + fns[1]() + fns[2]()")
                .unwrap(),
            Value::Number(3.0)
        );
        // A var-headed loop shares one binding, so every closure sees the
        // final value.
        assert_eq!(
            run("let fns = []; for (var i = 0; i < 3; i++) { fns.push(() => i); } fns[0]() + fns[1]() + fns[2]()")
                .unwrap(),
            Value::Number(9.0)
        );
    }

    #[test]
    fn for_let_head_is_lexical_not_a_global_var() {
        // A `let`-headed for loop must not create a global binding (the
        // var-scoped-declaration collector previously treated every for-head
        // declarator as `var`).
        assert_eq!(run("'i' in globalThis").unwrap(), Value::Boolean(false));
        // The initializer self-reference hits the loop binding's TDZ instead
        // of resolving to a hoisted global var.
        assert!(matches!(
            run("for (let x = x; ; ) { break; }"),
            Err(error) if error.kind == crux::ErrorKind::ReferenceError
        ));
        // A `var`-headed loop still creates a global binding.
        assert_eq!(
            run("for (var y = 1; ; ) { break; } 'y' in globalThis").unwrap(),
            Value::Boolean(true)
        );
        // for-of/for-in `let` heads are lexical too.
        assert_eq!(
            run("for (let z of [1]) {} 'z' in globalThis").unwrap(),
            Value::Boolean(false)
        );
    }

    #[test]
    fn statement_lists_preserve_empty_completions() {
        // A list of only empty/declaration statements is ~empty~ (spec
        // 14.2.1), so an enclosing UpdateEmpty fills it with the preceding
        // value instead of overwriting it with *undefined*.
        assert_eq!(run("1; {}").unwrap(), Value::Number(1.0));
        assert_eq!(
            run("6; switch ('a') { case 'a': 7; default: }").unwrap(),
            Value::Number(7.0)
        );
        assert_eq!(run("1;;;;;").unwrap(), Value::Number(1.0));
        assert_eq!(run("var x = 1; x;").unwrap(), Value::Number(1.0));
    }

    #[test]
    fn switch_completion_values_carry_across_cases() {
        // CaseBlockEvaluation carries a running V across the executed case
        // clauses (spec 14.13.3); empty clauses keep it, and abrupt
        // completions are UpdateEmpty'd with it.
        assert_eq!(
            run("1; switch ('a') { case 'a': 2; default: 3; }").unwrap(),
            Value::Number(3.0)
        );
        assert_eq!(
            run("6; switch ('a') { case 'a': 7; case 'b': break; default: }").unwrap(),
            Value::Number(7.0)
        );
        assert_eq!(
            run("1; switch ('a') { case 'a': 2; case 'b': 3; break; }").unwrap(),
            Value::Number(3.0)
        );
        assert_eq!(
            run("13; do { switch ('a') { case 'a': 14; case 'b': continue; } } while (false)")
                .unwrap(),
            Value::Number(14.0)
        );
        assert_eq!(
            run("5; switch ('b') { case 'a': 8; default: }").unwrap(),
            Value::Undefined
        );
    }

    #[test]
    fn try_finally_completion_values_fill_empty_abrupts() {
        // TryStatement returns UpdateEmpty(F, undefined): an empty `break` in
        // the finalizer is filled with *undefined*, not the enclosing list's
        // value (spec 14.15.2 step 4).
        assert_eq!(
            run("99; do { -99; try { 39 } finally { 42; break; -2 }; } while (false);").unwrap(),
            Value::Number(42.0)
        );
        assert_eq!(
            run("99; do { -99; try { 39 } finally { break; -2 }; } while (false);").unwrap(),
            Value::Undefined
        );
        assert_eq!(
            run("99; do { -99; try { [].x.x } catch (e) { -1 } finally { break; -3 }; } while (false);")
                .unwrap(),
            Value::Undefined
        );
    }

    #[test]
    fn with_statement_boxes_primitives_and_fills_empty() {
        // WithStatement boxes non-objects (spec 14.15.4 step 2) and returns
        // UpdateEmpty(C, undefined) (step 8).
        assert_eq!(
            run("var o = 2; var foo = 1; with (o) { foo = 42; } foo").unwrap(),
            Value::Number(42.0)
        );
        assert_eq!(
            run("8; do { 9; with ({}) { break; } 7; } while (false)").unwrap(),
            Value::Undefined
        );
        assert_eq!(
            run("1; do { 2; with ({}) { 3; break; } 4; } while (false);").unwrap(),
            Value::Number(3.0)
        );
        assert_eq!(
            run("8; do { 9; with ({}) { 10; continue; } 11; } while (false)").unwrap(),
            Value::Number(10.0)
        );
    }

    #[test]
    fn var_declaration_resolves_before_initializer() {
        // spec 14.3.2: ResolveBinding runs before the initializer, so a
        // `with` binding object's property is the assignment target even when
        // the initializer deletes it.
        assert_eq!(
            run("var obj = { test262id: 1 }; with (obj) { var test262id = delete obj.test262id; } JSON.stringify([obj.test262id, test262id])")
                .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("[true,null]")))
        );
    }

    #[test]
    fn catch_param_env_hosts_default_initializer_closures() {
        // CatchClauseEvaluation installs the parameter environment before
        // BindingInitialization, so a default initializer's closure captures
        // the parameter bindings (spec 15.1.7 step 7).
        assert_eq!(
            run("let x = 'outside'; try { throw ['inside'] } catch ([x, _ = function () { return x; }]) {} ")
                .unwrap(),
            Value::Undefined
        );
        assert_eq!(
            run("var p; try { throw ['in'] } catch ([x, _ = (p = function () { return x; })]) {} p()")
                .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("in")))
        );
    }

    #[test]
    fn for_of_head_names_are_in_tdz_during_rhs_evaluation() {
        assert!(matches!(
            run("let x = 1; for (let x of [x]) {}"),
            Err(error) if error.kind == crux::ErrorKind::ReferenceError
        ));
        assert!(matches!(
            run("let x = 1; for (const x in { x }) {}"),
            Err(error) if error.kind == crux::ErrorKind::ReferenceError
        ));
        // A closure created in the head captures the TDZ environment.
        assert!(matches!(
            run("let x = 'outside'; var probe; for (let x of (probe = () => x, [])) ; probe()"),
            Err(error) if error.kind == crux::ErrorKind::ReferenceError
        ));
    }

    #[test]
    fn for_in_skips_keys_deleted_during_enumeration() {
        assert_eq!(
            run("var o = { aa: 1, ba: 2, ca: 3 }; var acc = ''; for (var k in o) { delete o.ba; acc += k; } acc")
                .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("aaca")))
        );
    }

    #[test]
    fn eval_function_binding_stays_deleted() {
        // A top-level eval function declaration evaluates to ~empty~, so a
        // runtime `delete` is not undone by the declaration's evaluation
        // (spec 15.2.6).
        assert_eq!(
            run("(function () { eval('function f() {} delete f;'); return typeof f; })()").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("undefined")))
        );
    }

    #[test]
    fn using_declarations_dispose_at_block_exit() {
        assert_eq!(
            run("var d = []; var r1 = { [Symbol.dispose]() { d.push(1); } }; var r2 = { [Symbol.dispose]() { d.push(2); } }; { using a = r1, b = r2; } JSON.stringify(d)")
                .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("[2,1]")))
        );
        // Disposed when a subsequent initializer throws.
        assert_eq!(
            run("var disposed = false; var r = { [Symbol.dispose]() { disposed = true; } }; function boom() { throw new Error('x'); } try { using a = r, b = boom(); } catch (e) {} disposed")
                .unwrap(),
            Value::Boolean(true)
        );
        // Disposed on an abrupt completion of the block.
        assert_eq!(
            run("var disposed = false; var r = { [Symbol.dispose]() { disposed = true; } }; { using a = r; throw new Error('x'); }")
                .unwrap_err()
                .kind,
            crux::ErrorKind::TypeError
        );
        // `using x = null` registers nothing and disposes nothing.
        assert_eq!(run("4; { using x = null; }").unwrap(), Value::Number(4.0));
    }

    #[test]
    fn using_in_for_of_head_disposes_per_iteration() {
        assert_eq!(
            run("var d = []; var r = { [Symbol.dispose]() { d.push(1); } }; for (using x of [r]) {} d.length")
                .unwrap(),
            Value::Number(1.0)
        );
        assert!(matches!(
            run("let x = 1; for (using x of [x]) {}"),
            Err(error) if error.kind == crux::ErrorKind::ReferenceError
        ));
    }

    #[test]
    fn indirect_eval_is_always_sloppy() {
        // strictCaller only applies to direct eval (spec 19.2.1.1 step 10);
        // an indirect eval's `with` and unresolvable assignments are sloppy.
        assert_eq!(
            run("var count = 0; (0, eval)('unresolvable_assignment = 7; count += 1;'); JSON.stringify([count, unresolvable_assignment])")
                .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("[1,7]")))
        );
        assert_eq!(
            run("(0, eval)('with ({}) {} 5')").unwrap(),
            Value::Number(5.0)
        );
    }

    #[test]
    fn strict_eval_rejects_reserved_word_bindings() {
        assert!(matches!(
            run("'use strict'; eval('var public = 1;')"),
            Err(error) if error.kind == crux::ErrorKind::SyntaxError
        ));
        assert!(matches!(
            run("function f() { 'use strict'; eval('var arguments;'); } f()"),
            Err(error) if error.kind == crux::ErrorKind::SyntaxError
        ));
        // `with` in a strict eval is a SyntaxError.
        assert!(matches!(
            run("'use strict'; eval('with ({}) {}')"),
            Err(error) if error.kind == crux::ErrorKind::SyntaxError
        ));
    }

    #[test]
    fn top_level_using_in_eval_is_a_syntax_error() {
        assert!(matches!(
            run("eval('using x = null;')"),
            Err(error) if error.kind == crux::ErrorKind::SyntaxError
        ));
        // Nested in a block it is fine.
        assert_eq!(
            run("eval('{ using x = null; } 5')").unwrap(),
            Value::Number(5.0)
        );
    }

    #[test]
    fn block_level_generator_declarations_stay_block_scoped() {
        // The Annex B var hoist covers plain function declarations only;
        // generator and async declarations remain block-scoped.
        assert!(matches!(
            run("switch (0) { default: function * x() {} } x"),
            Err(error) if error.kind == crux::ErrorKind::ReferenceError
        ));
    }

    #[test]
    fn loop_counter_operands_lower_to_register_runs() {
        // The acc-path loop counter (`PushAcc`) as a binary RIGHT operand
        // (`s += i`) or a plain member-store VALUE (`o.x = i`) previously
        // kept the whole loop body on the step path. Both positions resolve
        // the counter from the dedicated field at execution time, so they
        // lower to register runs.
        fn register_runs(
            agent: &mut Agent,
            src: &str,
            name: &str,
        ) -> Vec<Box<[crate::ir::LeafOp]>> {
            agent.run_script(src).unwrap();
            let ir = compiled_body_of(agent, name);
            ir.steps
                .iter()
                .filter_map(|s| match s {
                    crate::ir::Step::RunRegBody { ops } => Some(ops.clone()),
                    _ => None,
                })
                .collect()
        }
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        // `s += i`: the counter is the binary's right operand — the run
        // loads it into the accumulator and combines the late-read frame
        // slot (`BinLeftReg`).
        let var_chain = register_runs(
            &mut agent,
            "function g(o, n) { var s = 0; for (var i = 0; i < n; i++) { s += i; } return s; }",
            "g",
        );
        assert_eq!(var_chain.len(), 1, "the s += i body must lower");
        assert!(
            var_chain[0]
                .iter()
                .any(|op| matches!(op, crate::ir::LeafOp::LoadCounter)),
            "the run must load the counter operand"
        );
        assert!(
            var_chain[0].iter().any(|op| {
                matches!(
                    op,
                    crate::ir::LeafOp::BinStoreReg {
                        op: syntax::ast::BinaryOp::Add,
                        ..
                    }
                )
            }),
            "the run must fuse the slot compound into one bin+store op"
        );
        assert!(
            !var_chain[0]
                .iter()
                .any(|op| matches!(op, crate::ir::LeafOp::StoreReg { .. })),
            "the fused compound must not also carry a separate store"
        );
        // `o.x = i`: the counter is the member store's value operand (the
        // store resolves it from the field after the object load).
        let member_store = register_runs(
            &mut agent,
            "function h(o, n) { for (var i = 0; i < n; i++) { o.x = i; } return o.x; }",
            "h",
        );
        assert_eq!(member_store.len(), 1, "the o.x = i body must lower");
        assert!(
            member_store[0].iter().any(|op| {
                matches!(
                    op,
                    crate::ir::LeafOp::StoreMemberName {
                        value: crate::ir::RegOperand::Counter,
                        ..
                    }
                )
            }),
            "the run must store the counter value onto the member"
        );
    }

    #[test]
    fn slot_compounds_fuse_the_bin_store_tail_into_one_op() {
        // A statement-position local compound whose RHS resolved into the
        // accumulator (`s += i`, `s = s + i`) lowers to `BinLeftReg` +
        // `StoreReg` back into the SAME slot; the store directly consumes
        // the binary's result, so the tail fuses into one `BinStoreReg` op.
        // A store into a DIFFERENT slot (`x = s + i`) keeps the pair.
        fn register_runs(
            agent: &mut Agent,
            src: &str,
            name: &str,
        ) -> Vec<Box<[crate::ir::LeafOp]>> {
            agent.run_script(src).unwrap();
            let ir = compiled_body_of(agent, name);
            ir.steps
                .iter()
                .filter_map(|s| match s {
                    crate::ir::Step::RunRegBody { ops } => Some(ops.clone()),
                    _ => None,
                })
                .collect()
        }
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        // `s = s + i` — the same slot on both sides, RHS in the accumulator.
        let self_assign = register_runs(
            &mut agent,
            "function g(n) { var s = 0; for (var i = 0; i < n; i++) { s = s + i; } return s; }",
            "g",
        );
        assert_eq!(self_assign.len(), 1, "the s = s + i body must lower");
        assert!(
            self_assign[0]
                .iter()
                .any(|op| matches!(op, crate::ir::LeafOp::BinStoreReg { .. })),
            "the same-slot self assign must fuse into one bin+store op"
        );
        assert!(
            !self_assign[0]
                .iter()
                .any(|op| matches!(op, crate::ir::LeafOp::BinLeftReg { .. })),
            "the fused run must not carry the unfused pair"
        );
        // `x = s + i` — the store targets a different slot than the
        // binary's left, so the pair is not a slot RMW and stays as-is.
        let cross = register_runs(
            &mut agent,
            "function h(n) { var s = 0; var x = 0; for (var i = 0; i < n; i++) { x = s + i; } return x + s; }",
            "h",
        );
        assert_eq!(cross.len(), 1, "the x = s + i body must lower");
        assert!(
            cross[0]
                .iter()
                .any(|op| matches!(op, crate::ir::LeafOp::BinLeftReg { .. })),
            "a cross-slot store keeps the BinLeftReg"
        );
    }

    #[test]
    fn fused_slot_compounds_match_the_pair_semantics() {
        // The fused tail must agree with the unfused pair over binary ops
        // and the string-concat slow path (the exact apply_binary
        // semantics).
        assert_eq!(
            run("function g(n) { var s = 0; for (var i = 0; i < n; i++) { s += i; } return s; } g(100);")
                .unwrap(),
            Value::Number(4950.0)
        );
        assert_eq!(
            run("function g(n) { var s = 1; for (var i = 0; i < n; i++) { s *= 2; } return s; } g(10);")
                .unwrap(),
            Value::Number(1024.0)
        );
        assert_eq!(
            run("function g(n) { var s = 0; for (var i = 0; i < n; i++) { s = s + i; } return s; } g(100);")
                .unwrap(),
            Value::Number(4950.0)
        );
        // A string accumulator through the fused op (`s += i` with a string
        // `s` takes the concat slow path inside the single op).
        assert_eq!(
            run("function g(n) { var s = ''; for (var i = 0; i < n; i++) { s += i; } return s; } g(4);")
                .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("0123")))
        );
    }

    #[test]
    fn statement_position_local_updates_fuse_into_register_runs() {
        // A statement-position local update (`l++;`, `++l;`, `l--;`) has no
        // register form: its `UpdateLocal` pushes a result the trailing
        // `SetCompletion` discards, so `s += i; l++;` kept the update on the
        // step path — one extra dispatch per iteration. A statement-position
        // update is exactly `UpdateLocal` immediately followed by
        // `SetCompletion`: the run now stores the update in place
        // (`UpdateReg`, no push) and both statements lower to one run. The
        // expression-position forms (`x = l++`, `a[l++] = v`) keep the
        // deferred `PostInc` write-back.
        fn steps(
            agent: &mut Agent,
            src: &str,
            name: &str,
        ) -> (usize, Vec<Box<[crate::ir::LeafOp]>>) {
            agent.run_script(src).unwrap();
            let ir = compiled_body_of(agent, name);
            let step_updates = ir
                .steps
                .iter()
                .filter(|s| matches!(s, crate::ir::Step::UpdateLocal { .. }))
                .count();
            let runs = ir
                .steps
                .iter()
                .filter_map(|s| match s {
                    crate::ir::Step::RunRegBody { ops } => Some(ops.clone()),
                    _ => None,
                })
                .collect();
            (step_updates, runs)
        }
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let (updates, runs) = steps(
            &mut agent,
            "function g(n) { var s = 0; var l = 0; for (var i = 0; i < n; i++) { s += i; l++; } return s; }",
            "g",
        );
        assert_eq!(
            updates, 0,
            "the statement l++ must not stay on the step path"
        );
        assert_eq!(
            runs.len(),
            1,
            "the two-statement body must lower to one run"
        );
        assert!(
            runs[0]
                .iter()
                .any(|op| matches!(op, crate::ir::LeafOp::UpdateReg { .. })),
            "the run must contain the fused local update"
        );
        // Prefix (`++l`) and decrement (`l--`) fuse the same way, and a
        // numeric-string counter exercises the register op's general
        // ToNumeric updater (only a Number takes the inline f64 path).
        let (updates, runs) = steps(
            &mut agent,
            "function h(n) { var l = '5'; var s = 0; for (var i = 0; i < n; i++) { s += i; ++l; } return s; }",
            "h",
        );
        assert_eq!(updates, 0, "the statement ++l must fuse");
        assert_eq!(runs.len(), 1);
        let (updates, runs) = steps(
            &mut agent,
            "function k(n) { var l = 10; for (var i = 0; i < n; i++) { l--; } return l; }",
            "k",
        );
        assert_eq!(updates, 0, "the statement l-- must fuse");
        assert_eq!(runs.len(), 1);
        // The expression-position postfix (`x = l++`) still lowers through
        // the deferred `PostInc` operand (no `UpdateReg`), and the results
        // match the step path exactly.
        let result = agent
            .run_script(
                "function p(n) { var x = 0; var l = 0; for (var i = 0; i < n; i++) { x = l++; } return x; } p(5);",
            )
            .unwrap();
        assert_eq!(result, Value::Number(4.0));
    }

    #[test]
    fn strict_eq_if_conditions_fuse_into_one_jump() {
        // An `if (x === N)` / `x !== N` condition inside a certified loop
        // body compiled to LoadLocal + BinaryImm + JumpIfFalse per iteration
        // (three step dispatches — the buildString guard `l === 10000`). The
        // test now fuses into ONE `JumpIfEqImm`/`JumpIfNeqImm` step: a
        // slot-vs-numeric-literal strict-equality test that jumps when the
        // test is FALSE. Strict equality against a Number is coercion-free
        // (only the Number `imm` itself matches), so the fused step needs no
        // general evaluator; a non-Number value (a String, a BigInt, an
        // Object) simply takes the false path.
        fn steps(agent: &mut Agent, src: &str, name: &str) -> Vec<crate::ir::Step> {
            agent.run_script(src).unwrap();
            let ir = compiled_body_of(agent, name);
            ir.steps.to_vec()
        }
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let steps = steps(
            &mut agent,
            "function g(n) { var l = 0; var c = 0; for (var i = 0; i < n; i++) { l++; if (l === 10000) { c++; } } return c; }",
            "g",
        );
        assert!(
            steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::JumpIfEqImm { imm: 10000.0, .. })),
            "the === N condition must fuse into a JumpIfEqImm"
        );
        assert!(
            !steps.iter().any(|s| matches!(
                s,
                crate::ir::Step::BinaryImm {
                    op: syntax::ast::BinaryOp::StrictEqual,
                    ..
                }
            )),
            "no general === binary may remain for the fused condition"
        );
        // Semantics: the fused strict test is exact across value kinds — a
        // numeric-string never matches a Number, `!==` fires on it, and a
        // Number boundary still trips the guard.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let value = agent
            .run_script(
                "function g(n) { var l = 0; var c = 0; for (var i = 0; i < n; i++) { l++; if (l === 10000) { c++; } } return c; } \
                 function h(n) { var l = '5'; var c = 0; for (var i = 0; i < n; i++) { if (l === 5) { c++; } } return c; } \
                 function k(n) { var l = 0; var c = 0; for (var i = 0; i < n; i++) { l++; if (l !== 10000) { c++; } } return c; } \
                 g(25000) * 100 + h(3) * 10 + k(5)",
            )
            .unwrap();
        // g: l increments 1..25000 and hits 10000 exactly once (1); h: "5"
        // !== 5, never (0); k: l increments 1..5, never 10000, so all five
        // iterations count (5).
        assert_eq!(value, Value::Number(105.0));
    }

    #[test]
    fn certified_loop_ifs_skip_the_redundant_completion_reset() {
        // Inside a certified loop body the statements lower to register runs
        // that never touch the completion register and the loop's own
        // trailing NormalizeCompletion defines its completion, so the per-if
        // ResetCompletion (emitted so the if's NormalizeCompletion can turn
        // an empty register into Normal(undefined)) is a redundant
        // per-iteration dispatch. The gate drops it only there; an `if`
        // outside a loop keeps it (its enclosing block/ListEnd can observe
        // the emptiness).
        fn reset_count(agent: &mut Agent, src: &str, name: &str) -> usize {
            agent.run_script(src).unwrap();
            let ir = compiled_body_of(agent, name);
            ir.steps
                .iter()
                .filter(|s| matches!(s, crate::ir::Step::ResetCompletion))
                .count()
        }
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        // A loop-body if: the per-iteration reset is gone. The loop's own
        // leading ResetCompletion (before the init, once) remains, so g
        // carries exactly one — before the change the if's per-iteration
        // reset made it two.
        assert_eq!(
            reset_count(
                &mut agent,
                "function g(n) { var l = 0; var c = 0; for (var i = 0; i < n; i++) { l++; if (l === 10000) { c++; } } return c; }",
                "g"
            ),
            1,
            "a certified loop-body if must not carry the per-iteration reset"
        );
        // A plain (non-loop) if keeps its reset.
        assert_eq!(
            reset_count(
                &mut agent,
                "function h(x) { var z = 0; if (x) { z = 1; } else { z = 2; } return z; }",
                "h"
            ),
            1,
            "a non-loop if keeps its ResetCompletion"
        );
    }

    #[test]
    fn member_compound_and_computed_stores_lower_to_register_runs() {
        // A member RMW in a certified loop (`o.x += 1`) and a plain member
        // store of a computed value (`o.x = o.x + 1`) previously kept the
        // whole statement on the step path (the compound step, the `Dup`,
        // and the Acc-value store all lacked register forms). Now the whole
        // body lowers to one register run: the read (`GetMemberNameLocal`),
        // the combine (BinConst/BinImm/BinReg/... on the cached old value),
        // and the store (`StoreMemberNameLocal`, which reads the object
        // from its frame slot at store time).
        fn register_runs(
            agent: &mut Agent,
            src: &str,
            name: &str,
        ) -> Vec<Box<[crate::ir::LeafOp]>> {
            agent.run_script(src).unwrap();
            let ir = compiled_body_of(agent, name);
            ir.steps
                .iter()
                .filter_map(|s| match s {
                    crate::ir::Step::RunRegBody { ops } => Some(ops.clone()),
                    _ => None,
                })
                .collect()
        }
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        // `o.x += 1` (a binary compound with a const RHS).
        let compound = register_runs(
            &mut agent,
            "function c(o, n) { for (var i = 0; i < n; i++) { o.x += 1; } return o.x; }",
            "c",
        );
        assert_eq!(compound.len(), 1, "the o.x += 1 body must lower");
        let ops = &compound[0];
        assert!(
            ops.iter()
                .any(|op| matches!(op, crate::ir::LeafOp::GetMemberNameLocal { .. })),
            "the run must read the old value"
        );
        assert!(
            ops.iter().any(|op| matches!(
                op,
                crate::ir::LeafOp::BinConst {
                    op: syntax::ast::BinaryOp::Add,
                    ..
                }
            )),
            "the run must combine the old value with the RHS"
        );
        assert!(
            ops.iter().any(|op| matches!(
                op,
                crate::ir::LeafOp::StoreMemberNameLocal { object_slot: 0, .. }
            )),
            "the run must store the result onto the slot object's member"
        );
        // `o.x += i` (the loop counter RHS spills the cached old value).
        let counter_rhs = register_runs(
            &mut agent,
            "function g(o, n) { for (var i = 0; i < n; i++) { o.x += i; } return o.x; }",
            "g",
        );
        assert_eq!(counter_rhs.len(), 1, "the o.x += i body must lower");
        assert!(
            counter_rhs[0]
                .iter()
                .any(|op| matches!(op, crate::ir::LeafOp::BinAccPop { .. })),
            "the counter RHS must spill-combine the old value"
        );
        // `o.x = o.x + 1` (a plain store of a computed value).
        let computed = register_runs(
            &mut agent,
            "function h(o, n) { for (var i = 0; i < n; i++) { o.x = o.x + 1; } return o.x; }",
            "h",
        );
        assert_eq!(computed.len(), 1, "the o.x = o.x + 1 body must lower");
        assert!(
            computed[0].iter().any(|op| matches!(
                op,
                crate::ir::LeafOp::StoreMemberNameLocal { object_slot: 0, .. }
            )),
            "the computed-value store must target the slot object"
        );
    }

    #[test]
    fn statement_member_updates_lower_to_register_runs() {
        // A statement-position `o.x++`/`o.x--` in a certified loop: read the
        // old value (`GetMemberNameLocal`), apply the ToNumeric update
        // (`UpdateAcc`), and store it back onto the slot object's member
        // (`StoreMemberNameLocal`). An expression-position update (its value
        // feeds a later step) must stay on the step path.
        fn register_runs(
            agent: &mut Agent,
            src: &str,
            name: &str,
        ) -> Vec<Box<[crate::ir::LeafOp]>> {
            agent.run_script(src).unwrap();
            let ir = compiled_body_of(agent, name);
            ir.steps
                .iter()
                .filter_map(|s| match s {
                    crate::ir::Step::RunRegBody { ops } => Some(ops.clone()),
                    _ => None,
                })
                .collect()
        }
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let statement = register_runs(
            &mut agent,
            "function u(o, n) { for (var i = 0; i < n; i++) { o.x++; } return o.x; }",
            "u",
        );
        assert_eq!(statement.len(), 1, "the o.x++ body must lower");
        assert!(
            statement[0].iter().any(|op| matches!(
                op,
                crate::ir::LeafOp::UpdateAcc {
                    op: syntax::ast::UpdateOp::Increment
                }
            )),
            "the run must apply the update"
        );
        assert!(
            statement[0].iter().any(|op| matches!(
                op,
                crate::ir::LeafOp::StoreMemberNameLocal { object_slot: 0, .. }
            )),
            "the run must store onto the slot object"
        );
        // An expression-position update (`s += o.x++`) keeps the step path
        // (its result feeds the add).
        let expression = register_runs(
            &mut agent,
            "function e(o, n) { var s = 0; for (var i = 0; i < n; i++) { s += o.x++; } return s; }",
            "e",
        );
        assert_eq!(expression.len(), 0, "the expression update stays step-path");
    }

    #[test]
    fn computed_member_rmw_lowers_to_register_runs() {
        // A statement-position computed member compound (`o[k] += 1`) or
        // update (`o[k]++`) in a certified loop, and an explicit
        // `o[k] = o[k] + 1`, now lower to one fused register op per
        // statement (`CompoundMemberComputedLocal`/
        // `UpdateMemberComputedLocal`/`StoreMemberComputedLocal`). A shape
        // the fused form cannot preserve (a side-effectful RHS, an
        // expression-position update whose value feeds later steps) stays
        // on the step path.
        fn register_runs(
            agent: &mut Agent,
            src: &str,
            name: &str,
        ) -> Vec<Box<[crate::ir::LeafOp]>> {
            agent.run_script(src).unwrap();
            let ir = compiled_body_of(agent, name);
            ir.steps
                .iter()
                .filter_map(|s| match s {
                    crate::ir::Step::RunRegBody { ops } => Some(ops.clone()),
                    _ => None,
                })
                .collect()
        }
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        // `o[k] += 1`: one fused compound op (key converted once, shared by
        // its internal read and write).
        let compound = register_runs(
            &mut agent,
            "function c(o, k, n) { for (var i = 0; i < n; i++) { o[k] += 1; } return o[k]; }",
            "c",
        );
        assert_eq!(compound.len(), 1, "the o[k] += 1 body must lower");
        assert!(
            matches!(
                &compound[0][..],
                [crate::ir::LeafOp::CompoundMemberComputedLocal {
                    object_slot: 0,
                    key: crate::ir::RegOperand::Reg {
                        slot: 1,
                        tdz: false,
                    },
                    op: syntax::ast::AssignOp::AddAssign,
                    rhs: crate::ir::RegOperand::Const(value),
                }] if *value == crux::Value::Number(1.0)
            ),
            "the o[k] += 1 statement must be exactly one fused op, got {:?}",
            compound[0]
        );
        // `o[k]++`: one fused update op.
        let update = register_runs(
            &mut agent,
            "function u(o, k, n) { for (var i = 0; i < n; i++) { o[k]++; } return o[k]; }",
            "u",
        );
        assert_eq!(update.len(), 1, "the o[k]++ body must lower");
        assert!(
            matches!(
                &update[0][..],
                [crate::ir::LeafOp::UpdateMemberComputedLocal {
                    object_slot: 0,
                    key: crate::ir::RegOperand::Reg {
                        slot: 1,
                        tdz: false,
                    },
                    op: syntax::ast::UpdateOp::Increment,
                }]
            ),
            "the o[k]++ statement must be exactly one fused op, got {:?}",
            update[0]
        );
        // A literal Number key (`o[0] += 1`) lowers with the key as a
        // `Const` operand — the fused core then reaches its numeric element
        // fast paths.
        let const_key = register_runs(
            &mut agent,
            "function n(o, n) { for (var i = 0; i < n; i++) { o[0] += 1; } return o[0]; }",
            "n",
        );
        assert_eq!(const_key.len(), 1, "the o[0] += 1 body must lower");
        assert!(
            matches!(
                &const_key[0][..],
                [crate::ir::LeafOp::CompoundMemberComputedLocal {
                    object_slot: 0,
                    key: crate::ir::RegOperand::Const(value),
                    op: syntax::ast::AssignOp::AddAssign,
                    rhs: crate::ir::RegOperand::Const(rhs),
                }] if *value == crux::Value::Number(0.0) && *rhs == crux::Value::Number(1.0)
            ),
            "the o[0] += 1 statement must carry the Const key, got {:?}",
            const_key[0]
        );
        // `o[k] = o[k] + 1`: the plain computed store of a computed value
        // (the store defers its own key conversion, matching the step
        // path).
        let explicit = register_runs(
            &mut agent,
            "function e(o, k, n) { for (var i = 0; i < n; i++) { o[k] = o[k] + 1; } return o[k]; }",
            "e",
        );
        assert_eq!(explicit.len(), 1, "the o[k] = o[k] + 1 body must lower");
        assert!(
            explicit[0].iter().any(|op| matches!(
                op,
                crate::ir::LeafOp::StoreMemberComputedLocal {
                    object_slot: 0,
                    key: crate::ir::RegOperand::Reg {
                        slot: 1,
                        tdz: false,
                    },
                }
            )),
            "the run must store through the re-read key slot"
        );
        // A side-effectful RHS (`o[k] += o.y` — a member read that must run
        // AFTER the old-value read) keeps the whole statement on the step
        // path: the fused form cannot defer its read past RHS work.
        let rhs_read = register_runs(
            &mut agent,
            "function h(o, k, n) { for (var i = 0; i < n; i++) { o[k] += o.y; } return o[k]; }",
            "h",
        );
        assert_eq!(
            rhs_read.len(),
            0,
            "a computed compound with a member-read RHS stays step-path"
        );
        // An expression-position computed update (`s += o[k]++`) leaves its
        // result unconsumed and stays on the step path.
        let expression = register_runs(
            &mut agent,
            "function x(o, k, n) { var s = 0; for (var i = 0; i < n; i++) { s += o[k]++; } return s; }",
            "x",
        );
        assert_eq!(
            expression.len(),
            0,
            "the expression-position computed update stays step-path"
        );
    }

    #[test]
    fn counter_keyed_computed_access_lowers_to_register_runs() {
        // The typed-array element rows access `ta[k]` with the acc-path loop
        // counter as the computed key. The lowering previously rejected a
        // `Counter` key (and capped the counter reads at one), so the whole
        // body dispatched per step. A single-counter computed READ
        // (`s += ta[k]`) and the computed-store shape (`ta[k] = k & 255` —
        // the counter as the key AND in the value expression, two reads)
        // now lower to one register run each.
        fn register_runs(
            agent: &mut Agent,
            src: &str,
            name: &str,
        ) -> Vec<Box<[crate::ir::LeafOp]>> {
            agent.run_script(src).unwrap();
            let ir = compiled_body_of(agent, name);
            ir.steps
                .iter()
                .filter_map(|s| match s {
                    crate::ir::Step::RunRegBody { ops } => Some(ops.clone()),
                    _ => None,
                })
                .collect()
        }
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        // The read: one run with a Counter-keyed computed read.
        let reads = register_runs(
            &mut agent,
            "function f(ta) { var L = ta.length; var s = 0; \
             for (var k = 0; k < L; k++) { s += ta[k]; } return s; }",
            "f",
        );
        assert_eq!(reads.len(), 1, "the counter-keyed read body must lower");
        assert!(
            reads[0].iter().any(|op| matches!(
                op,
                crate::ir::LeafOp::GetMemberComputedLocal {
                    object_slot: 0,
                    key: crate::ir::RegOperand::Counter,
                    ..
                }
            )),
            "the run must read through the Counter key"
        );
        // The write row shape: the counter is the key AND the value's
        // operand (two reads) — one run, a Counter-keyed computed store.
        let writes = register_runs(
            &mut agent,
            "function g(ta) { var L = ta.length; \
             for (var k = 0; k < L; k++) { ta[k] = k & 255; } return L; }",
            "g",
        );
        assert_eq!(writes.len(), 1, "the counter-keyed store body must lower");
        assert!(
            writes[0].iter().any(|op| matches!(
                op,
                crate::ir::LeafOp::StoreMemberComputedLocal {
                    object_slot: 0,
                    key: crate::ir::RegOperand::Counter,
                }
            )),
            "the run must store through the Counter key"
        );
        // Behavior: the register-run rows match the step-path semantics
        // (byte values, the uint8 wrap, OOB-free range).
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let value = agent
            .run_script(
                "var ta = new Uint8Array(1000); var s = 0; \
                 for (var k = 0; k < ta.length; k++) { ta[k] = k & 255; s += ta[k]; } \
                 ta[0] * 1000000 + ta[255] * 1000 + ta[256] + s",
            )
            .unwrap();
        // ta[k] = k & 255: ta[0] = 0, ta[255] = 255, ta[256] = 0 (256 wraps);
        // s = sum of k & 255 over 0..1000 = 3 full 0..255 cycles (32640 each)
        // + the 0..231 tail (26796) = 124716.
        assert_eq!(value, Value::Number(255.0 * 1000.0 + 124_716.0));
    }

    #[test]
    fn counter_keyed_computed_compound_and_update_lower() {
        // The `Dup2` duplicated `(object, key)` pair of the computed compound
        // / update path now re-reads a `Counter` key (the loop-counter field
        // is run-invariant, so the read-side and write-side resolutions see
        // the same value), so `ta[k] += 1` fuses into ONE
        // `CompoundMemberComputedLocal` op and `ta[k]++` into ONE
        // `UpdateMemberComputedLocal` op — the step-path compound on a typed
        // array converted the key to a string and re-resolved per iteration
        // (~30x slower).
        fn register_runs(
            agent: &mut Agent,
            src: &str,
            name: &str,
        ) -> Vec<Box<[crate::ir::LeafOp]>> {
            agent.run_script(src).unwrap();
            let ir = compiled_body_of(agent, name);
            ir.steps
                .iter()
                .filter_map(|s| match s {
                    crate::ir::Step::RunRegBody { ops } => Some(ops.clone()),
                    _ => None,
                })
                .collect()
        }
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let compounds = register_runs(
            &mut agent,
            "function h(ta) { var L = ta.length; \
             for (var k = 0; k < L; k++) { ta[k] += 1; } return L; }",
            "h",
        );
        assert_eq!(compounds.len(), 1, "the compound body must lower");
        assert!(
            compounds[0].iter().any(|op| matches!(
                op,
                crate::ir::LeafOp::CompoundMemberComputedLocal {
                    object_slot: 0,
                    key: crate::ir::RegOperand::Counter,
                    op: syntax::ast::AssignOp::AddAssign,
                    rhs: crate::ir::RegOperand::Const(_),
                }
            )),
            "the run must fuse the Counter-keyed compound"
        );
        let updates = register_runs(
            &mut agent,
            "function u(ta) { var L = ta.length; \
             for (var k = 0; k < L; k++) { ta[k]++; } return L; }",
            "u",
        );
        assert_eq!(updates.len(), 1, "the update body must lower");
        assert!(
            updates[0].iter().any(|op| matches!(
                op,
                crate::ir::LeafOp::UpdateMemberComputedLocal {
                    object_slot: 0,
                    key: crate::ir::RegOperand::Counter,
                    ..
                }
            )),
            "the run must fuse the Counter-keyed update"
        );
        // Behavior across typed-array kinds: the register-run compound and
        // update match the step-path semantics (uint8 wrap, Int16 negatives,
        // float64 +=).
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let value = agent
            .run_script(
                "var u8 = new Uint8Array(256); var i16 = new Int16Array(8); \
                 for (var k = 0; k < 256; k++) { u8[k] = k; } \
                 for (var k = 0; k < 256; k++) { u8[k] += 1; } \
                 for (var k = 0; k < 8; k++) { i16[k] = k - 4; } \
                 for (var k = 0; k < 8; k++) { i16[k]++; } \
                 u8[0] + u8[254] + u8[255] * 1000 + i16[0] + i16[7]",
            )
            .unwrap();
        // u8[k] = k then += 1: u8[0] = 1, u8[254] = 255, u8[255] = 0 (255+1
        // wraps to 0). i16[k] = k - 4 then ++: i16[0] = -3, i16[7] = 4.
        assert_eq!(value, Value::Number(1.0 + 255.0 + 0.0 + (-3.0) + 4.0));
    }

    #[test]
    fn for_body_list_wrappers_drop_only_without_abrupt_control() {
        // A `for` body block whose statements cannot transfer out of it (no
        // break/continue/return/throw, no suspension) compiles without the
        // per-iteration `ListBegin`/`ListEnd` — the loop's own
        // `ResetCompletion`/`NormalizeCompletion` make the wrapper's
        // save/restore unobservable. A body with an abrupt transfer keeps
        // the pair (the transfer would skip the matching `ListEnd`).
        fn list_step_count(agent: &mut Agent, src: &str, name: &str) -> usize {
            agent.run_script(src).unwrap();
            let ir = compiled_body_of(agent, name);
            ir.steps
                .iter()
                .filter(|s| matches!(s, crate::ir::Step::ListBegin | crate::ir::Step::ListEnd))
                .count()
        }
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        // The append-only body has no nested block either — no wrappers at
        // all.
        assert_eq!(
            list_step_count(
                &mut agent,
                "function f(n) { var a = []; var l = 0; \
                 for (var i = 0; i < n; i++) { a[l++] = i; } return l; }",
                "f"
            ),
            0
        );
        // The buildString guard shape: the guard's own consequent block
        // (`{ c++; l = 0; }`) is now fully register (statement-position
        // `c++` has a register form, and `l = 0` is a fused store), so its
        // all-register pair is absorbed — the loop body's wrappers are gone
        // (the body has no abrupt transfer), leaving none. The absorption is
        // sound: nothing step-path reads the consequent's completion before
        // the `if` normalizes it, and the shape runs byte-identical under
        // the JIT and the interpreter.
        assert_eq!(
            list_step_count(
                &mut agent,
                "function g(n) { var a = []; var l = 0; var c = 0; \
                 for (var i = 0; i < n; i++) { a[l++] = i; if (l === 10000) { c++; l = 0; } } \
                 return c; }",
                "g"
            ),
            0
        );
        // A break or continue (even guarded by an `if`) keeps the body's
        // pair.
        assert_eq!(
            list_step_count(
                &mut agent,
                "function h(n) { var a = []; var l = 0; \
                 for (var i = 0; i < n; i++) { a[l++] = i; if (l > 10000) break; } return l; }",
                "h"
            ),
            2
        );
        assert_eq!(
            list_step_count(
                &mut agent,
                "function k(n) { var a = []; var l = 0; \
                 for (var i = 0; i < n; i++) { a[l++] = i; if (l > 10000) continue; } return l; }",
                "k"
            ),
            2
        );
        // A return keeps the pair too.
        assert_eq!(
            list_step_count(
                &mut agent,
                "function m(n) { var a = []; var l = 0; \
                 for (var i = 0; i < n; i++) { a[l++] = i; if (l > 10000) return l; } return l; }",
                "m"
            ),
            2
        );
    }

    #[test]
    fn dropped_for_body_wrapper_keeps_loop_completion() {
        // The completion of a loop whose body block ends in an `if` with an
        // empty alternate — a shape whose per-iteration wrapper is dropped —
        // still normalizes exactly like the wrapped equivalent.
        assert_eq!(
            run("var a = []; var l = 0; var c = 0; \
                 for (var i = 0; i < 20; i++) { a[l++] = i; if (l === 10) { c++; l = 0; } } \
                 c + a.length + l")
            .unwrap(),
            // The guard resets `l` (not the array), so indices 0-9 are
            // written twice: c = 2, a.length = 10, l = 0.
            Value::Number(12.0)
        );
        assert_eq!(
            run("var a = []; var l = 0; 1; \
                 for (var i = 0; i < 3; i++) { a[l++] = i; if (i === 1) { } }")
            .unwrap(),
            Value::Undefined
        );
        // An empty body block keeps the empty completion (undefined after
        // the loop's normalize), not a stale accumulation.
        assert_eq!(
            run("var i = 0; 7; for (; i < 3; i++) { }").unwrap(),
            Value::Undefined
        );
    }

    /// The compiled body of a function bound on the global object.
    fn compiled_body_of(agent: &mut Agent, name: &str) -> std::rc::Rc<crate::ir::CompiledBody> {
        let global = agent.running_context().unwrap().realm.global_object;
        let value = global.get(&JsString::from_utf8(name)).unwrap();
        let ValueKind::Function(function) = value.kind() else {
            panic!("{name} is not a function");
        };
        agent
            .ecma_functions
            .get(&function.id())
            .expect("function is registered")
            .ir
            .clone()
            .expect("function has compiled IR")
    }

    #[test]
    fn fast_path_params_compile_to_frame_slots() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent.run_script("function f(x) { return x + 1; }").unwrap();
        let ir = compiled_body_of(&mut agent, "f");
        assert!(ir.scope.is_some(), "a simple param body must be certified");
        assert!(
            ir.steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::LoadLocal { .. }))
        );
        assert!(
            !ir.steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::LoadIdent { .. })),
            "param reads must not walk the environment"
        );
    }

    #[test]
    fn fast_path_loop_bindings_are_frame_slots() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script("function loop() { var n = 0; for (var i = 0; i < 100; i++) { n += i * 2; } return n; }")
            .unwrap();
        let ir = compiled_body_of(&mut agent, "loop");
        assert!(ir.scope.is_some(), "the loop body must be certified");
        assert!(
            ir.steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::InitLocal { .. }))
        );
        // Cut 4/15: the loop test fuses into a slot op and the update
        // into the fused canonical-loop head.
        assert!(
            ir.steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::JumpIfLtImm { .. }))
        );
        assert!(
            ir.steps.iter().any(|s| {
                matches!(
                    s,
                    crate::ir::Step::FastLoopHead {
                        var: crate::ir::FastLoopVar::Slot(_) | crate::ir::FastLoopVar::Counter,
                        ..
                    }
                )
            }),
            "the fused canonical loop head must own the increment"
        );
        assert!(
            !ir.steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::LoadIdent { .. })),
            "binding reads must not walk the environment"
        );
    }

    #[test]
    fn braced_fused_store_bodies_lower_to_register_runs() {
        // M7 slice 1a: a braced loop body whose last statement is a
        // self-balancing fused store (`{ n += 1; }` — the store pops its
        // own value and is followed only by the block's `ListEnd`) must
        // close a register run exactly like the unbraced form. Before the
        // commit rule recognized the fused store as a statement boundary,
        // the trailing `ListEnd` left the run uncommitted and the whole
        // braced body dispatched per step.
        fn register_runs(
            agent: &mut Agent,
            src: &str,
            name: &str,
        ) -> Vec<Box<[crate::ir::LeafOp]>> {
            agent.run_script(src).unwrap();
            let ir = compiled_body_of(agent, name);
            ir.steps
                .iter()
                .filter_map(|s| match s {
                    crate::ir::Step::RunRegBody { ops } => Some(ops.clone()),
                    _ => None,
                })
                .collect()
        }
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let braced = register_runs(
            &mut agent,
            "function braced() { var n = 0; for (var i = 0; i < 10; i++) { n += 1; } return n; }",
            "braced",
        );
        assert_eq!(braced.len(), 1, "the braced fused-store body must lower");
        assert!(
            braced[0]
                .iter()
                .any(|op| matches!(op, crate::ir::LeafOp::StoreReg { .. })),
            "the run must contain the accumulator store"
        );
        // The unbraced equivalent lowers to the same run.
        let unbraced = register_runs(
            &mut agent,
            "function flat() { var n = 0; for (var i = 0; i < 10; i++) n += 1; return n; }",
            "flat",
        );
        assert_eq!(unbraced.len(), 1);
        assert_eq!(braced[0].len(), unbraced[0].len());
        // The braced body's `ListBegin`/`ListEnd` pair is ABSORBED into the
        // run span (an all-register block's wrappers are a completion
        // no-op), so the whole loop body is the single run — no list steps
        // remain in the function.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script(
                "function absorbed() { var n = 0; for (var i = 0; i < 10; i++) { n += 1; } return n; }",
            )
            .unwrap();
        let ir = compiled_body_of(&mut agent, "absorbed");
        assert!(
            !ir.steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::ListBegin | crate::ir::Step::ListEnd)),
            "the all-register braced loop body must absorb its list wrappers"
        );
        // A fused-store statement BEFORE a control-flow statement closes its
        // run at the store (the `if`'s reset/jump steps stay on the step
        // path); the statement inside the `if`'s block lowers too.
        let segmented = register_runs(
            &mut agent,
            "function seg() { var n = 0; for (var i = 0; i < 10; i++) { n += 1; if (n > 3) { n = 0; } } return n; }",
            "seg",
        );
        assert_eq!(segmented.len(), 2, "both fused-store statements must lower");
        assert_eq!(
            segmented[0].len(),
            unbraced[0].len(),
            "the pre-`if` statement lowers to the same run as the flat body"
        );
    }

    #[test]
    fn braced_member_store_body_keeps_its_set_completion_absorbed() {
        // The member-store statement's step-path form leaves the assigned
        // value for its trailing `SetCompletion` to pop; the register form
        // consumes it, so the run must ABSORB the `SetCompletion` (commit at
        // it, never at the member store). Ending the run at the store would
        // strand the pop on the step path — the `a[l++] = i` buildString
        // corruption. The braced body must still lower to ONE run whose ops
        // end in the member store.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script(
                "function f() { var a = []; var l = 0; \
                 for (var i = 0; i < 10; i++) { a[l++] = i; } return a[9] + l; }",
            )
            .unwrap();
        let ir = compiled_body_of(&mut agent, "f");
        let runs: Vec<&Box<[crate::ir::LeafOp]>> = ir
            .steps
            .iter()
            .filter_map(|s| match s {
                crate::ir::Step::RunRegBody { ops } => Some(ops),
                _ => None,
            })
            .collect();
        assert_eq!(runs.len(), 1);
        assert!(matches!(
            runs[0].last(),
            Some(crate::ir::LeafOp::StoreMemberComputed { .. })
        ));
        // And the statement's value still completes correctly at the top
        // level (the absorbed `SetCompletion` is the statement's only pop).
        assert_eq!(
            run("var a = []; var l = 0; a[l++] = 7; a[0] * 10 + l").unwrap(),
            Value::Number(71.0)
        );
    }

    #[test]
    fn slice23_args_alias_gates_read_only_params() {
        fn args_alias(agent: &mut Agent, src: &str, name: &str) -> bool {
            agent.run_script(src).unwrap();
            let ir = compiled_body_of(agent, name);
            ir.scope.as_ref().expect("certified").args_alias
        }
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        // Read-only params qualify.
        assert!(args_alias(
            &mut agent,
            "function a(x) { return x + 1; }",
            "a"
        ));
        assert!(args_alias(
            &mut agent,
            "function b(a, b) { return a + b; }",
            "b"
        ));
        // A param write, a `var` slot, `this`, and `arguments` disqualify.
        assert!(!args_alias(
            &mut agent,
            "function c(x) { x = x + 1; return x; }",
            "c"
        ));
        assert!(!args_alias(
            &mut agent,
            "function d(x) { var t = 2; return x + t; }",
            "d"
        ));
        assert!(!args_alias(
            &mut agent,
            "function e(x) { return this.u + x; }",
            "e"
        ));
        assert!(!args_alias(
            &mut agent,
            "function g(x) { return arguments.length + x; }",
            "g"
        ));
        // A closure inside the body writing the param counts as a write.
        assert!(!args_alias(
            &mut agent,
            "function h(x) { var c = () => { x = 1; }; return x; }",
            "h"
        ));
    }

    #[test]
    fn slice23_caller_slot_args_behave() {
        // The fused call-store shape with a read-only-param register leaf
        // reads the arg from the caller's frame slot.
        assert_eq!(
            run("function f(x) { return x + 1; } var n = 0; for (var i = 0; i < 1000; i++) { n = f(n); } n")
                .unwrap(),
            Value::Number(1000.0)
        );
        // A param-WRITING callee must not corrupt the caller's variable.
        assert_eq!(
            run("function w(x) { x = x + 1; return x; } var n = 0; for (var i = 0; i < 1000; i++) { n = w(n); } n")
                .unwrap(),
            Value::Number(1000.0)
        );
        // The arg slot is a different variable than the target.
        assert_eq!(
            run("function f(x) { return x + 1; } var m = 10; var n = 0; for (var i = 0; i < 1000; i++) { n = f(m); } n + m")
                .unwrap(),
            Value::Number(21.0)
        );
        // A slot-callee (certified-value var) closure with a read-only param.
        assert_eq!(
            run("var g = function (x) { return x + 2; }; var n = 0; for (var i = 0; i < 1000; i++) { n = g(n); } n")
                .unwrap(),
            Value::Number(2000.0)
        );
    }

    #[test]
    fn slice22_acc_counter_gates_to_number_inits_and_stores() {
        fn head_var(agent: &mut Agent, src: &str, name: &str) -> crate::ir::FastLoopVar {
            agent.run_script(src).unwrap();
            let ir = compiled_body_of(agent, name);
            ir.steps
                .iter()
                .find_map(|s| match s {
                    crate::ir::Step::FastLoopHead { var, .. } => Some(*var),
                    _ => None,
                })
                .expect("a fused loop head")
        }
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        // Number inits (plain, negative-literal, parenthesized) and a
        // Number-literal body write stay on the acc path.
        assert!(matches!(
            head_var(
                &mut agent,
                "function a() { var n = 0; for (var i = 0; i < 100; i++) { n += i; } return n; }",
                "a"
            ),
            crate::ir::FastLoopVar::Counter
        ));
        assert!(matches!(
            head_var(
                &mut agent,
                "function b() { var n = 0; for (var i = -5; i < 100; i++) { n += i; } return n; }",
                "b"
            ),
            crate::ir::FastLoopVar::Counter
        ));
        assert!(matches!(
            head_var(
                &mut agent,
                "function c() { var n = 0; for (var i = (2); i < 100; i++) { if (i === 3) { i = 5; } n += i; } return n; }",
                "c"
            ),
            crate::ir::FastLoopVar::Counter
        ));
        // A multi-decl head's LAST counter initializer decides.
        assert!(matches!(
            head_var(
                &mut agent,
                "function g() { var n = 0; for (var i = 5, i = 3; i < 100; i++) { n += i; } return n; }",
                "g"
            ),
            crate::ir::FastLoopVar::Counter
        ));
        // A String init, a String body write, a BigInt init, and a
        // multi-decl head whose last counter init is not a Number fall back
        // to the slot path.
        assert!(matches!(
            head_var(
                &mut agent,
                "function d() { var n = 0; for (var i = \"0\"; i < 100; i++) { n += i; } return n; }",
                "d"
            ),
            crate::ir::FastLoopVar::Slot(_)
        ));
        assert!(matches!(
            head_var(
                &mut agent,
                "function e() { var n = 0; for (var i = 0; i < 100; i++) { if (i === 3) { i = \"x\"; } n += i; } return n; }",
                "e"
            ),
            crate::ir::FastLoopVar::Slot(_)
        ));
        assert!(matches!(
            head_var(
                &mut agent,
                "function f() { var n = 0; for (var i = 5n; i < 100; i++) { n += i; } return n; }",
                "f"
            ),
            crate::ir::FastLoopVar::Slot(_)
        ));
        assert!(matches!(
            head_var(
                &mut agent,
                "function h() { var n = 0; for (var i = 5, i = \"x\"; i < 100; i++) { n += i; } return n; }",
                "h"
            ),
            crate::ir::FastLoopVar::Slot(_)
        ));
    }

    #[test]
    fn fast_path_arrow_binds_params_to_slots() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent.run_script("var f = (x) => x + 1;").unwrap();
        let ir = compiled_body_of(&mut agent, "f");
        assert!(ir.scope.is_some(), "a simple arrow must be certified");
        assert!(
            !ir.steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::LoadIdent { .. })),
            "param reads must not walk the environment"
        );
        assert_eq!(
            run("var f = (x) => x + 1; f(41)").unwrap(),
            Value::Number(42.0)
        );
    }

    #[test]
    fn fast_path_call_binds_params_to_slots() {
        assert_eq!(
            run("function f(x) { return x + 1; } f(41)").unwrap(),
            Value::Number(42.0)
        );
    }

    #[test]
    fn fast_path_capture_free_closure_certifies() {
        // Cut 3 continuation: a closure that captures nothing from the body
        // can be created by a certified body (its own body compiles
        // separately).
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script("function f() { return function() { return 42; }; }")
            .unwrap();
        let ir = compiled_body_of(&mut agent, "f");
        assert!(
            ir.scope.is_some(),
            "a capture-free closure must not bail the body"
        );
        assert!(
            ir.steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::CreateFunction { .. })),
            "the certified body must create the closure"
        );
        assert_eq!(
            run("function f() { return function() { return 42; }; } f()()").unwrap(),
            Value::Number(42.0)
        );
        // A closure whose free name resolves globally is fine too.
        assert_eq!(
            run("function g() { return function() { return Math.max(1, 2); }; } g()()").unwrap(),
            Value::Number(2.0)
        );
    }

    #[test]
    fn fast_path_capturing_closure_uses_context() {
        // Cut 3 continuation: a closure that reads or writes a body binding
        // now certifies — the binding moves into the per-call capture
        // context the closure reaches through the environment.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script("function f() { var x = 5; return function() { return x; }; }")
            .unwrap();
        let ir = compiled_body_of(&mut agent, "f");
        assert!(
            ir.scope.is_some(),
            "a capturing closure must keep the body certified"
        );
        assert!(
            ir.steps
                .iter()
                .any(|s| { matches!(s, crate::ir::Step::InitContextSlot { .. }) }),
            "the captured binding must initialize through the capture context"
        );
        // The counter-factory shape: the closure mutates the captured var.
        assert_eq!(
            run("function counter() { var c = 0; return function() { c++; return c; }; } var f = counter(); f() + f() * 10")
                .unwrap(),
            Value::Number(21.0)
        );
        // A captured param initializes from the call argument.
        assert_eq!(
            run("function make(x) { return (y) => x + y; } make(5)(7)").unwrap(),
            Value::Number(12.0)
        );
        // Two closures sharing one captured binding stay in sync.
        assert_eq!(
            run("function s() { var x = 1; var f = () => x; var g = () => { x = x + 1; return x; }; return f() + g() + f() * 10; } s()")
                .unwrap(),
            Value::Number(23.0)
        );
        // A closure shadowing a body name with its own declaration is fine.
        assert_eq!(
            run("function shadow() { var x = 1; return function() { var x = 2; return x; }; } shadow()()")
                .unwrap(),
            Value::Number(2.0)
        );
        // An arrow's lexical `this` observes the body's absence of `this`,
        // so it bails and the this binding survives.
        assert_eq!(
            run("function t() { return () => this.v; } t.call({ v: 7 })()").unwrap(),
            Value::Number(7.0)
        );
    }

    #[test]
    fn fast_path_captured_let_tdz_and_loop_heads() {
        // A captured `let` starts in the TDZ: the closure called before the
        // init throws the ReferenceError.
        assert!(matches!(
            run("function f() { var g = () => x; return g; let x = 5; } f()()"),
            Err(error) if error.kind == crux::ErrorKind::ReferenceError
        ));
        // A closure inside a loop capturing the loop-head `let` certifies:
        // the loop runs per-iteration envs holding a fresh copy per
        // iteration, which the closures capture (spec 14.7.4.3).
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script("function f() { var a = []; for (let i = 0; i < 3; i++) { a.push(() => i); } return a; }")
            .unwrap();
        let ir = compiled_body_of(&mut agent, "f");
        assert!(
            ir.scope.is_some(),
            "a loop-head capture must keep the body certified"
        );
        assert!(
            ir.steps.iter().any(|s| {
                matches!(
                    s,
                    crate::ir::Step::EnterPerIteration { .. }
                        | crate::ir::Step::LoadPerIteration { .. }
                        | crate::ir::Step::UpdatePerIteration { .. }
                )
            }),
            "the certified body must run per-iteration envs for the head"
        );
        assert_eq!(
            run("function f() { var a = []; for (let i = 0; i < 3; i++) { a.push(() => i); } return a[0]() + a[1]() * 10 + a[2]() * 100; } f()")
                .unwrap(),
            Value::Number(210.0)
        );
        // A captured `var` in a loop shares one binding (all closures see
        // the final value — the per-iteration freshness is lexical-only).
        assert_eq!(
            run("function v() { var a = []; for (var i = 0; i < 3; i++) { a.push(() => i); } return a[0]() + a[1]() * 10 + a[2]() * 100; } v()")
                .unwrap(),
            Value::Number(333.0)
        );
    }

    #[test]
    fn fast_path_for_of_in_heads_certify() {
        // A `var` for-of head certifies like a `For` head (Cut 3 gap item
        // 7): the element binds the frame slot directly, no per-iteration
        // environment.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script(
                "function f() { var s = 0; for (var v of [1, 2, 3]) { s += v; } return s; }",
            )
            .unwrap();
        let ir = compiled_body_of(&mut agent, "f");
        assert!(ir.scope.is_some(), "a var for-of head must certify");
        assert!(
            ir.steps.iter().any(|s| matches!(
                s,
                crate::ir::Step::ForOfNext { .. } | crate::ir::Step::ForOfNextBindLocal { .. }
            )),
            "the loop must run the iteration protocol"
        );
        assert!(
            ir.steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::ForOfNextBindLocal { .. })),
            "a slot head fuses the protocol step and the bind (Cut 21)"
        );
        assert!(
            !ir.steps.iter().any(|s| matches!(
                s,
                crate::ir::Step::ForOfBindLocal { .. }
                    | crate::ir::Step::ForOfBind { .. }
                    | crate::ir::Step::LoadIdent { .. }
            )),
            "the fused bind replaces the separate step; no env machinery"
        );
        assert!(
            ir.env_constant,
            "an uncaptured var for-of head contributes no env"
        );
        assert_eq!(
            run("function f() { var s = 0; for (var v of [1, 2, 3]) { s += v; } return s; } f()")
                .unwrap(),
            Value::Number(6.0)
        );

        // An uncaptured lexical head certifies too: the flat slot is
        // re-initialized per iteration (its freshness is unobservable
        // without closures, which the scan rejects).
        agent
            .run_script(
                "function g() { var s = 0; for (let v of [1, 2, 3]) { s += v * 2; } return s; }",
            )
            .unwrap();
        let ir = compiled_body_of(&mut agent, "g");
        assert!(
            ir.scope.is_some(),
            "an uncaptured let for-of head must certify"
        );
        assert!(
            ir.env_constant,
            "an uncaptured lexical head contributes no env"
        );
        assert_eq!(
            run(
                "function g() { var s = 0; for (let v of [1, 2, 3]) { s += v * 2; } return s; } g()"
            )
            .unwrap(),
            Value::Number(12.0)
        );

        // A captured lexical head uses the per-iteration machinery: fresh
        // copies the body's closures observe (spec 14.7.5.6).
        agent
            .run_script("function h() { var a = []; for (let v of [10, 20, 30]) { a.push(() => v); } return a; }")
            .unwrap();
        let ir = compiled_body_of(&mut agent, "h");
        assert!(
            ir.scope.is_some(),
            "a captured let for-of head must keep the body certified"
        );
        assert!(
            ir.steps.iter().any(|s| matches!(
                s,
                crate::ir::Step::EnterPerIteration { .. }
                    | crate::ir::Step::StorePerIteration { .. }
            )),
            "a captured for-of head must run per-iteration envs"
        );
        assert!(
            !ir.env_constant,
            "the per-iteration env machinery is env-changing"
        );
        assert_eq!(
            run("function h() { var a = []; for (let v of [10, 20, 30]) { a.push(() => v); } return a[0]() + a[1]() + a[2](); } h()")
                .unwrap(),
            Value::Number(60.0)
        );

        // A for-in var head certifies.
        agent
            .run_script("function k() { var s = 0; for (var key in { a: 1, b: 2 }) { s += key.length; } return s; }")
            .unwrap();
        let ir = compiled_body_of(&mut agent, "k");
        assert!(ir.scope.is_some(), "a var for-in head must certify");
        assert!(
            ir.steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::ForInNext { .. })),
            "for-in must run the enumeration protocol"
        );
        assert_eq!(
            run("function k() { var s = 0; for (var key in { a: 1, b: 2 }) { s += key.length; } return s; } k()")
                .unwrap(),
            Value::Number(2.0)
        );

        // A destructuring head keeps the whole body on the env path.
        agent
            .run_script("function d() { var s = 0; for (var [a, b] of [[1, 2]]) { s += a + b; } return s; }")
            .unwrap();
        let ir = compiled_body_of(&mut agent, "d");
        assert!(
            ir.scope.is_none(),
            "a destructuring for-of head must keep the env path"
        );
        assert_eq!(
            run("function d() { var s = 0; for (var [a, b] of [[1, 2]]) { s += a + b; } return s; } d()")
                .unwrap(),
            Value::Number(3.0)
        );
    }

    #[test]
    fn fast_path_per_iteration_loop_heads() {
        // Body reads/writes of a captured head go through the per-iteration
        // env; a write before the closure creation is what the closure sees
        // (within an iteration the binding is shared).
        assert_eq!(
            run("function f() { var a = []; for (let i = 0; i < 3; i++) { i = i * 10; a.push(() => i); } return a[0]() + a[1]() * 10; } f()")
                .unwrap(),
            Value::Number(100.0)
        );
        // The update targets the next iteration's copy: the test sees the
        // incremented value.
        assert_eq!(
            run(
                "function f() { var s = 0; for (let i = 0; i < 3; i++) { s += i; } return s; } f()"
            )
            .unwrap(),
            Value::Number(3.0)
        );
        // Nested loops capture both heads; the outer head resolves through
        // the inner per-iteration env's outer chain.
        assert_eq!(
            run("function f() { var a = []; for (let i = 0; i < 2; i++) { for (let j = 0; j < 2; j++) { a.push(() => i + j); } } return a[0]() + a[1]() + a[2]() + a[3](); } f()")
                .unwrap(),
            Value::Number(4.0)
        );
        // A mixed head: the captured name gets a per-iteration env, the
        // non-captured name stays a frame slot.
        assert_eq!(
            run("function f() { var a = []; var s = 0; for (let i = 0, k = 100; i < 3; i++, k++) { s += k; a.push(() => i); } return a[0]() + a[1]() * 10 + a[2]() * 100 + s; } f()")
                .unwrap(),
            Value::Number(513.0)
        );
        // break and continue unwind the per-iteration env correctly.
        assert_eq!(
            run("function f() { var a = []; for (let i = 0; i < 10; i++) { if (i === 2) break; a.push(() => i); } return a[0]() + a[1]() * 10; } f()")
                .unwrap(),
            Value::Number(10.0)
        );
        assert_eq!(
            run("function f() { var a = []; for (let i = 0; i < 4; i++) { if (i === 2) continue; a.push(() => i); } return a[0]() + a[1]() * 10 + a[2]() * 100; } f()")
                .unwrap(),
            Value::Number(310.0)
        );
        // A labeled break from a nested block (the block contributes no env
        // even though the body has captured bindings).
        assert_eq!(
            run("function f() { var a = []; outer: for (let i = 0; i < 5; i++) { { if (i === 1) break outer; } a.push(() => i); } return a[0](); } f()")
                .unwrap(),
            Value::Number(0.0)
        );
        // The head init can reference a frame-slot binding (a param): the
        // init compiles to bytecode, not the tree-walker.
        assert_eq!(
            run("function f(n) { var a = []; for (let i = n; i < n + 2; i++) { a.push(() => i); } return a[0]() + a[1]() * 10; } f(5)")
                .unwrap(),
            Value::Number(65.0)
        );
        // Escaped closures keep per-iteration values after the call.
        assert_eq!(
            run("function f() { var a = []; for (let i = 0; i < 3; i++) { a.push(() => i); } return a; } var g = f(); g[0]() + g[1]() * 10 + g[2]() * 100")
                .unwrap(),
            Value::Number(210.0)
        );
        // A closure that writes the head mutates its own iteration's env.
        assert_eq!(
            run("function f() { var a = []; for (let i = 0; i < 3; i++) { a.push(() => { i++; return i; }); } return a; } var h = f(); h[0]() + h[1]() * 10 + h[2]() * 100")
                .unwrap(),
            Value::Number(321.0)
        );
        // A zero-iteration loop leaves no closures and restores the capture
        // context (a later captured binding read must still work).
        assert_eq!(
            run("function f() { var a = []; for (let i = 0; i < 0; i++) { a.push(() => i); } var x = 1; var g = () => x; return a.length + g(); } f()")
                .unwrap(),
            Value::Number(1.0)
        );
    }

    #[test]
    fn fast_path_block_captured_let_needs_no_block_env() {
        // A block-level `let` captured by a closure must not create a block
        // env (it would shadow the capture context and break the flat
        // context-slot layout): the block contributes no env, the binding
        // lives in the capture context.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script("function f() { let x = 1; { let y = 2; return () => x + y; } }")
            .unwrap();
        let ir = compiled_body_of(&mut agent, "f");
        assert!(ir.scope.is_some(), "the body must stay certified");
        assert!(
            !ir.steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::EnterBlock { .. })),
            "a captured block let must not allocate a block env"
        );
        assert_eq!(
            run("function f() { let x = 1; { let y = 2; return () => x + y; } } f()()").unwrap(),
            Value::Number(3.0)
        );
        // A captured lexical declared inside a loop body is fresh per
        // iteration (the body block re-creates each iteration) — the
        // certified per-iteration machinery covers only loop-head names, so
        // such a body stays on the env path and keeps the freshness.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script("function g() { var a = []; for (let i = 0; i < 2; i++) { let y = i + 10; a.push(() => i + y); } return a; }")
            .unwrap();
        let irg = compiled_body_of(&mut agent, "g");
        assert!(
            irg.scope.is_none(),
            "a loop-body capture must bail to the env path"
        );
        assert_eq!(
            run("function f() { var a = []; for (let i = 0; i < 2; i++) { let y = i + 10; a.push(() => i + y); } return a[0]() + a[1]() * 100; } f()")
                .unwrap(),
            Value::Number(1210.0)
        );
    }

    #[test]
    fn fast_path_nested_context_chain() {
        // Cut 3 continuation (nested context chains): a certified closure's
        // references to an enclosing certified body's captured bindings
        // compile to static context-chain reads (`LoadContextSlot { depth ≥
        // 1 }`) instead of env walks.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script("function make(x) { return (y) => x + y; }")
            .unwrap();
        let ir = compiled_body_of(&mut agent, "make");
        assert!(ir.scope.is_some(), "the body must certify");
        // The closure records make's capture context: its `x` reference
        // compiles to a static context-chain read instead of an env walk.
        let x = crux::intern_utf8("x");
        assert!(
            ir.steps.iter().any(|s| {
                matches!(
                    s,
                    crate::ir::Step::CreateArrow { outer_chain, .. }
                        if outer_chain.len() == 1 && outer_chain[0] == vec![x]
                )
            }),
            "the closure must record make's capture context"
        );
        assert_eq!(
            run("function make(x) { return (y) => x + y; } make(5)(7)").unwrap(),
            Value::Number(12.0)
        );
        // A closure with its own captures reads the outer binding one hop
        // deeper (depth 1: its own context first).
        assert_eq!(
            run("function f() { var x = 10; return function (a) { var own = a * 2; return () => x + own; }; } f(1)(2)()")
                .unwrap(),
            Value::Number(14.0)
        );
        // A depth-2 chain: outer -> middle -> inner reading the outermost
        // capture through both contexts.
        assert_eq!(
            run("function o() { let b = 100; return function m() { let d = 10; return function i(y) { return b + d + y; }; }; } o()()(1)")
                .unwrap(),
            Value::Number(111.0)
        );
        // A named function expression's self-binding scope is transparent to
        // the chain walk.
        assert_eq!(
            run("function f() { let n = 42; return function inner() { return n; }; } f()()")
                .unwrap(),
            Value::Number(42.0)
        );
        // A nested closure can write an enclosing mutable binding.
        assert_eq!(
            run("function f() { var c = 0; return () => { c += 10; return c; }; } var g = f(); g() + g() * 10")
                .unwrap(),
            Value::Number(210.0)
        );
        // Writing an enclosing `const` throws (the static store checks
        // immutability; sloppy bodies throw too, per spec 16.1.8).
        assert!(matches!(
            run("function f() { const k = 5; return () => { k = 9; }; } f()()"),
            Err(error) if error.kind == crux::ErrorKind::TypeError
        ));
        assert!(matches!(
            run("function f() { const k = 5; return () => { k++; }; } f()()"),
            Err(error) if error.kind == crux::ErrorKind::TypeError
        ));
        // The chain walk reaches past a per-iteration env: a closure created
        // inside a certified loop reads a non-head capture statically.
        assert_eq!(
            run("function f() { var t = 0; var a = []; for (let i = 0; i < 3; i++) { t += i; a.push(() => i + t); } return a[0]() + a[1]() * 10 + a[2]() * 100; } f()")
                .unwrap(),
            Value::Number(543.0)
        );
        // The env-path boundary: an uncertified middle body breaks the static
        // chain; the env walk keeps the resolution correct.
        assert_eq!(
            run("function f() { var x = 7; return (function () { try { throw 1; } catch (e) { return () => x * 2; } })()(); } f()")
                .unwrap(),
            Value::Number(14.0)
        );
        // A closure referencing a name that is not an enclosing capture
        // resolves globally, not through the chain.
        assert_eq!(
            run("function f() { return () => Math.max(1, 2); } f()()").unwrap(),
            Value::Number(2.0)
        );
        // A closure referencing bindings from two different enclosing levels
        // (the test262 Atomics harness shape): each resolves to its own
        // body's context on the chain.
        assert_eq!(
            run("function t(f) { var bad = [function(v) { return -1; }]; for (var i = 0; i < bad.length; ++i) { var gen = bad[i]; try { f(gen); } catch (e) { e.message += ' (idx ' + gen + '.)'; throw e; } } } function outer(TA) { let view = TA; t(function(gen) { let run = function() { return gen(view); }; if (run() !== -1) throw new Error('bad'); }); } outer(0)")
                .unwrap(),
            Value::Undefined
        );
        // An intermediate body with no captures of its own contributes no
        // context hop: the innermost closure reaches past it.
        assert_eq!(
            run("function capParam(a) { return function () { return (b) => a + b; }; } capParam(5)()(3)")
                .unwrap(),
            Value::Number(8.0)
        );
    }

    #[test]
    fn fast_path_function_declarations() {
        // A top-level function declaration certifies: the name is hoisted and
        // initialized with the closure at body entry (spec 10.2.11), and the
        // declaration statement itself is empty (15.2.6). Block-level and
        // statement-position declarations (Annex B, two bindings) stay on the
        // env path.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script("function f() { function g() { return 42; } return g(); }")
            .unwrap();
        let ir = compiled_body_of(&mut agent, "f");
        assert!(ir.scope.is_some(), "the body must certify");
        assert!(
            ir.steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::FunctionDeclInit { .. })),
            "the hoisted declaration must be initialized at entry"
        );
        assert!(
            !ir.steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::FunctionDecl { .. })),
            "the declaration statement must be empty in a certified body"
        );
        // A forward reference (hoisting) and recursion through the declared
        // name.
        assert_eq!(
            run("function f() { var x = g; function g(n) { return n <= 1 ? 1 : n * g(n - 1); } return x(5); } f()")
                .unwrap(),
            Value::Number(120.0)
        );
        // var + function sharing a name: the var initializer's assignment
        // overwrites the hoisted function in statement order.
        assert_eq!(
            run("function f() { var g = 1; function g() { return 2; } return g; } f()").unwrap(),
            Value::Number(1.0)
        );
        // Mutual recursion: both names are captured, so the scan allocates
        // context slots and each closure resolves the other through the
        // capture context.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script("function f() { function even(n) { return n === 0 ? true : odd(n - 1); } function odd(n) { return n === 0 ? false : even(n - 1); } return even(3); }")
            .unwrap();
        let mir = compiled_body_of(&mut agent, "f");
        assert!(mir.scope.is_some(), "mutual recursion must certify");
        let mut init_slots: Vec<Option<usize>> = mir
            .steps
            .iter()
            .filter_map(|s| match s {
                crate::ir::Step::FunctionDeclInit { context_slot, .. } => Some(*context_slot),
                _ => None,
            })
            .collect();
        init_slots.sort();
        assert_eq!(
            init_slots,
            vec![Some(0), Some(1)],
            "both declarations must live in the capture context"
        );
        assert!(
            run_deep(
                "function f() { function even(n) { return n === 0 ? true : odd(n - 1); } function odd(n) { return n === 0 ? false : even(n - 1); } return even(10); } f()",
                |v| v == &Value::Boolean(true),
            ),
            "even(10) mutual recursion must terminate"
        );
        assert!(
            run_deep(
                "function f() { function even(n) { return n === 0 ? true : odd(n - 1); } function odd(n) { return n === 0 ? false : even(n - 1); } return odd(7); } f()",
                |v| v == &Value::Boolean(true),
            ),
            "odd(7) mutual recursion must terminate"
        );
        // A closure capturing the declared name reads it from the capture
        // context after the body returns.
        assert_eq!(
            run("function f() { var a = []; function g() { return 9; } a.push(() => g); return a[0]()(); } f()")
                .unwrap(),
            Value::Number(9.0)
        );
        // A nested certified closure inside the declared function.
        assert_eq!(
            run("function f() { function g(x) { return (y) => x + y; } return g(5)(3); } f()")
                .unwrap(),
            Value::Number(8.0)
        );
        // Block-level declarations now certify via Annex B (two bindings:
        // the block binding + the hoisted var — see
        // fast_path_annex_b_block_functions).
        assert_eq!(
            run("function f() { { function g() { return 1; } } return g(); } f()").unwrap(),
            Value::Number(1.0)
        );
    }

    #[test]
    fn nested_function_own_this_and_arguments_keep_the_body_certified() {
        // A nested NON-ARROW function has its OWN `this`/`arguments`, bound
        // at its own call — so a body containing one that reads them must
        // not bail to the env path. Previously any `this` inside a nested
        // closure bailed the enclosing body's certification (~5x on the
        // construct-churn shape; measured 2026-09-04: a function-wrapped
        // `new C(i)` loop ~123ms -> ~19ms). An ARROW still observes the
        // analyzed body's lexical `this`, so those keep the env path.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script(
                "function build() { function C(x) { this.x = x; } var s = 0; \
                 for (var i = 0; i < 10; i++) { var o = new C(i); s += o.x; } return s; }",
            )
            .unwrap();
        let ir = compiled_body_of(&mut agent, "build");
        assert!(
            ir.scope.is_some(),
            "a nested constructor's own `this` must keep the body certified"
        );
        assert_eq!(agent.run_script("build()").unwrap(), Value::Number(45.0));
        // The nested function's `arguments` is its own (the nested call's,
        // not the enclosing body's).
        assert_eq!(
            run(
                "function outer() { function inner(a, b) { return arguments[0] + arguments[1]; } \
                 return inner(3, 4) + inner(5, 6); } outer()"
            )
            .unwrap(),
            Value::Number(18.0)
        );
        // A nested function called as a method binds its OWN this to the
        // receiver.
        assert_eq!(
            run(
                "function make() { function getX() { return this.x; } return getX; } \
                 var f = make(); f.call({ x: 42 });"
            )
            .unwrap(),
            Value::Number(42.0)
        );
        // An arrow inside the analyzed body still observes the body's
        // lexical `this` (env path; the value must flow through).
        assert_eq!(
            run("function t() { return () => this.v; } t.call({ v: 7 })()").unwrap(),
            Value::Number(7.0)
        );
    }

    #[test]
    fn this_capturing_arrows_certify() {
        // Tasklist 2.3: an arrow created in a certified non-arrow body that
        // references `this` no longer bails the body. The body captures its
        // `this` into a context slot at entry and the arrow reads it through
        // the context chain — so a method with a `this` callback (the
        // common `arr.forEach(v => this.x += v)` shape) certifies and runs
        // on the fast path instead of the env machinery.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script(
                "function build(n) { var add = () => this.k; var s = 0; \
                 for (var i = 0; i < n; i++) { s += add() + i; } return s; }",
            )
            .unwrap();
        let ir = compiled_body_of(&mut agent, "build");
        assert!(
            ir.scope.is_some(),
            "a body whose arrow captures `this` must certify"
        );
        let scope = ir.scope.as_ref().unwrap();
        assert!(
            scope.captured_this.is_some(),
            "the body must capture `this` into its context"
        );
        assert!(scope.this_slot.is_some(), "the capture forces a this slot");
        assert_eq!(
            run("function build(n) { var add = () => this.k; var s = 0; \
                 for (var i = 0; i < n; i++) { s += add() + i; } return s; } \
                 build.call({ k: 3 }, 5)")
            .unwrap(),
            Value::Number(25.0)
        );
        // The arrow is created once and escapes; a later call observes the
        // capturing body's `this` (not undefined / globalThis).
        assert_eq!(
            run("function make() { return () => this.v; } var f = make.call({ v: 9 }); f()")
                .unwrap(),
            Value::Number(9.0)
        );
        // Arrow-in-arrow: the inner arrow reads the same captured `this`
        // through the outer arrow's context chain.
        assert_eq!(
            run("function make() { return () => () => this.v; } \
                 var f = make.call({ v: 11 }); f()()")
            .unwrap(),
            Value::Number(11.0)
        );
        // A nested function inside the certified body creates its own arrow
        // capturing ITS OWN this (the nested call's receiver), not the outer
        // body's.
        assert_eq!(
            run(
                "function outer() { function inner() { var f = () => this.v; return f(); } \
                 return inner.call({ v: 1 }) + inner.call({ v: 2 }); } outer()"
            )
            .unwrap(),
            Value::Number(3.0)
        );
        // Sloppy `this` coercion: a plain call's `this` is the global
        // object, so the captured value must be the global object.
        assert_eq!(
            run("function g() { var f = () => this === globalThis; return f(); } g()").unwrap(),
            Value::Boolean(true)
        );
        // A standalone arrow whose enclosing body is NOT certified (an arrow
        // at script top level has globalThis; strict undefined) still works
        // through the env path.
        assert_eq!(
            run("var f = () => this === globalThis; f()").unwrap(),
            Value::Boolean(true)
        );
        // An ENV-PATH arrow (rest params do not certify) created inside a
        // certified body that captured `this` still resolves its lexical
        // `this` — the capture context serves as its this-environment
        // (Object.keys/proxy-keys regression: the trap getters' rest arrows
        // must see the handler, not the global object).
        assert_eq!(
            run(
                "function make() { return (...args) => this.v + args.length; } \
                 var f = make.call({ v: 5 }); f(1, 2)"
            )
            .unwrap(),
            Value::Number(7.0)
        );
        assert_eq!(
            run("var ok = false; var handler = { \
                 get ownKeys() { return (...args) => { ok = this === handler; return ['a']; }; } }; \
                 var p = new Proxy({}, handler); Object.keys(p); ok;")
            .unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn string_primitive_member_reads_serve_length_and_units_without_boxing() {
        // Tasklist 1.4: reading a property off a primitive string boxed a
        // fresh String-exotic wrapper per access (a 448B JsObject + a 64B
        // [[StringData]] copy) on every read path; the shared member reads
        // now serve `length` (spec 10.4.3.4 StringGet step 1) and in-range
        // canonical index reads (spec 10.4.3.5 StringGetOwnProperty — an own
        // data property holding the single code unit) directly off the
        // string. These asserts pin the semantics the fast paths must
        // preserve.
        assert_eq!(
            run("'abc'.length + ':' + ''.length").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("3:0")))
        );
        // `.length` counts code UNITS, not code points (a surrogate pair is
        // 2).
        assert_eq!(run("'\u{1F600}'.length").unwrap(), Value::Number(2.0));
        // In-range index reads return the single code unit; the astral first
        // unit is a lone surrogate.
        assert_eq!(
            run("'abc'[0] + '|' + 'abc'[2]").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("a|c")))
        );
        assert_eq!(
            run("'\u{1F600}'[0]").unwrap(),
            Value::String(Handle::new(JsString::from_utf16(&[0xD83D])))
        );
        // Out-of-range and non-index keys fall through to the prototype
        // chain (no own property exists there).
        assert_eq!(
            run("'abc'[3] === undefined && 'abc'[-1] === undefined").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("String.prototype[5] = 'x'; 'abc'[5]").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("x")))
        );
        // An in-range index is an OWN property of the boxed receiver, so it
        // shadows a prototype patch at the same key.
        assert_eq!(
            run("String.prototype[0] = 'PATCHED'; 'abc'[0]").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("a")))
        );
        // A literal-string computed key reads `.length` through the named
        // path too; boxed String objects still read via their own machinery.
        assert_eq!(
            run(
                "var s = 'abcdef'; var r = 0; for (var i = 0; i < 10; i++) { r += s['length']; } \
                 r + ':' + (new String('xy')).length"
            )
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("60:2")))
        );
    }

    #[test]
    fn string_primitive_method_reads_resolve_on_the_prototype_chain() {
        // Tasklist follow-on: a method call on a primitive string boxes a
        // fresh String-exotic wrapper for the member READ of the method
        // (charAt/charCodeAt/indexOf and the s[i]-style index reads each
        // paid a 448B wrapper + 64B [[StringData]] copy per call on every
        // read path). The shared member helpers now resolve chain reads on a
        // string primitive directly against %String.prototype% with the
        // primitive as the [[Get]] receiver. These asserts pin the semantics
        // the direct resolution must preserve: the wrapper's own virtual
        // `length`/in-range indices shadow the chain, every other key reads
        // the live chain (data props, patched methods, accessors seeing the
        // primitive receiver, proxy links, symbol keys), and out-of-range
        // numeric keys fall to the chain.
        assert_eq!(
            run("'abc'.charCodeAt(1) + ':' + 'abc'.charAt(2) + ':' + 'abc'.indexOf('b')").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("98:c:1")))
        );
        // A data-property patch is read live and called (with `this` = the
        // primitive, which sloppy this-coercion boxes — `this` is a String
        // object, not the literal).
        assert_eq!(
            run("var orig = String.prototype.charAt; \
                 String.prototype.charAt = function () { return this; }; \
                 var out = typeof 'ab'.charAt(0) + ':' + ('ab'.charAt(0) instanceof String); \
                 String.prototype.charAt = orig; out")
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("object:true")))
        );
        // An accessor on the chain sees the PRIMITIVE as `this` when strict
        // (the engine's [[Get]] receiver is the primitive; only sloppy
        // this-coercion boxes it).
        assert_eq!(
            run("Object.defineProperty(String.prototype, 'slop', { get: function () { return typeof this; } }); \
                 Object.defineProperty(String.prototype, 'strct', { get: function () { 'use strict'; return typeof this; } }); \
                 var slop = 'a'.slop; var strct = 'a'.strct; \
                 delete String.prototype.slop; delete String.prototype.strct; slop + ':' + strct")
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("object:string")))
        );
        // An exotic (proxy) link later in the chain runs its [[Get]] with
        // the primitive receiver.
        assert_eq!(
            run("var log = []; \
                 var p = new Proxy({}, { get: function (t, k, r) { 'use strict'; log.push(typeof r); return 7; } }); \
                 Object.setPrototypeOf(String.prototype, p); \
                 var got = 'a'.zz; Object.setPrototypeOf(String.prototype, Object.prototype); \
                 got + ':' + log.join(',')")
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("7:string")))
        );
        // A symbol-keyed chain read (the iterator method) resolves.
        assert_eq!(
            run("typeof 'abc'[Symbol.iterator] === 'function'").unwrap(),
            Value::Boolean(true)
        );
        // In-range index reads (numeric and literal-string keys) serve the
        // single code unit and shadow chain patches; out-of-range falls to
        // the chain.
        assert_eq!(
            run("String.prototype[0] = 'PATCHED'; \
                 String.prototype[9] = 'nine'; \
                 var a = 'ab'[0]; var b = 'ab'['1']; var o = 'ab'[9]; \
                 delete String.prototype[0]; delete String.prototype[9]; \
                 a + '|' + b + '|' + o")
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("a|b|nine")))
        );
    }

    #[test]
    fn non_string_primitive_method_reads_resolve_on_the_prototype_chain() {
        // Tasklist recommended-order follow-on: Number/Boolean/BigInt/Symbol
        // primitives had the SAME per-read wrapper boxing as strings did
        // (each method read built a fresh wrapper object — a 448B JsObject
        // plus an agent table insert for the boxed value — before the chain
        // read; the string-only 1.4/1.5 helper left them on the generic
        // path). The shared member helpers now route every primitive to
        // `get_primitive_member`, which resolves the key against the kind's
        // cached %X.prototype% with the primitive as the [[Get]] receiver.
        // These asserts pin the semantics the direct resolution must
        // preserve, kind by kind.
        assert_eq!(
            run("(1.25).toFixed(1) + ':' + (255).toString(16) + ':' + (10).toPrecision(3)")
                .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("1.3:ff:10.0")))
        );
        assert_eq!(
            run("true.toString() + ':' + false.valueOf()").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("true:false")))
        );
        // A data-property patch on the prototype is read live.
        assert_eq!(
            run("var npo = Number.prototype.toFixed; \
                 Number.prototype.toFixed = function () { return 'X'; }; \
                 var patched = (1.5).toFixed(); Number.prototype.toFixed = npo; patched")
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("X")))
        );
        // Accessors on the chain see the primitive receiver: a strict getter
        // on %Number.prototype% receives `this` = the number (only sloppy
        // this-coercion boxes it).
        assert_eq!(
            run("Object.defineProperty(Number.prototype, 'slop', { get: function () { return typeof this; } }); \
                 Object.defineProperty(Number.prototype, 'strct', { get: function () { 'use strict'; return typeof this; } }); \
                 var slop = (5).slop; var strct = (5).strct; \
                 delete Number.prototype.slop; delete Number.prototype.strct; slop + ':' + strct")
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("object:number")))
        );
        // An exotic (proxy) link later in the chain runs its [[Get]] with
        // the primitive receiver.
        assert_eq!(
            run("var log = []; \
                 var p = new Proxy({}, { get: function (t, k, r) { 'use strict'; log.push(typeof r); return 3; } }); \
                 Object.setPrototypeOf(Number.prototype, p); \
                 var got = (5).zz; Object.setPrototypeOf(Number.prototype, Object.prototype); \
                 got + ':' + log.join(',')")
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("3:number")))
        );
        // Symbol chain reads: the `description` accessor and `toString` data
        // property resolve off %Symbol.prototype%.
        assert_eq!(
            run("Symbol('d').description + ':' + (Symbol().description === undefined) + ':' + Symbol('x').toString()")
                .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("d:true:Symbol(x)")))
        );
        // BigInt chain reads.
        assert_eq!(
            run("123n.toString() + ':' + (123n).toString(2)").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("123:1111011")))
        );
        // Boxed wrapper objects still read through their own machinery.
        assert_eq!(
            run("new Number(5).toFixed(1) + ':' + new Number(5).valueOf() + ':' + Object(5).toFixed(1)")
                .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("5.0:5:5.0")))
        );
    }

    #[test]
    fn fast_path_annex_b_block_functions() {
        // Cut 6 first slice (Annex B): a block-level function declaration
        // in a sloppy body certifies — the block binding is a frame slot
        // initialized at block entry (14.2.3), and when the hoist applies
        // (B.3.3.4) the function-scoped var binding is reset to *undefined*
        // at block entry (B.3.2.1) and the declaration statement copies the
        // block binding's current value into it (B.3.3.3). Statement-
        // position declarations and captured names stay on the env path.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script("function f() { { function g() { return 1; } } return g(); }")
            .unwrap();
        let ir = compiled_body_of(&mut agent, "f");
        assert!(ir.scope.is_some(), "the body must certify");
        let scope = ir.scope.as_ref().unwrap();
        assert_eq!(scope.annex_b.len(), 1, "the block function gets an entry");
        let entry = &scope.annex_b[0];
        assert!(
            entry.var_slot.is_some(),
            "the sloppy plain declaration hoists"
        );
        assert!(
            !ir.steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::EnterBlock { .. })),
            "the block must stay env-free"
        );
        assert!(
            ir.steps.iter().any(|s| matches!(
                s,
                crate::ir::Step::FunctionDeclInit {
                    frame_slot: Some(slot),
                    ..
                } if *slot == entry.block_slot
            )),
            "the block entry initializes the block binding"
        );
        assert!(
            ir.steps.iter().any(|s| matches!(
                s,
                crate::ir::Step::LoadLocal { slot } if *slot == entry.block_slot
            )),
            "the declaration statement copies the block binding"
        );
        // The hoisted var binding is visible outside the block.
        assert_eq!(
            run("function f() { { function g() { return 1; } } return g(); } f()").unwrap(),
            Value::Number(1.0)
        );
        // An early block exit leaves the var binding undefined.
        assert_eq!(
            run("function f() { L: { break L; function g() {} } return typeof g; } f()").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("undefined")))
        );
        // Writing the block binding does not leak into the var binding (the
        // declaration statement already copied the block value).
        assert_eq!(
            run("function f() { { function g() {} g = 5; } return typeof g; } f()").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("function")))
        );
        // A strict body's block declaration is block-scoped only.
        assert_eq!(
            run(
                "function f() { 'use strict'; { function g() { return 1; } return typeof g; } } f()"
            )
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("function")))
        );
        assert_eq!(
            run(
                "function f() { 'use strict'; { function g() { return 1; } } return typeof g; } f()"
            )
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("undefined")))
        );
        // A top-level lexical conflict suppresses the hoist.
        assert_eq!(
            run("function f() { let g = 1; { function g() { return 2; } var inside = g(); } return [inside, g].join(','); } f()")
                .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("2,1")))
        );
        // A parameter conflict suppresses the hoist too.
        assert_eq!(
            run("function f(g) { { function g() { return 3; } var inside = g(); } return [inside, g].join(','); } f(1)")
                .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("3,1")))
        );
        // Duplicate block declarations: the second is dead (14.2.3).
        assert_eq!(
            run("function f() { { function g() { return 1; } function g() { return 2; } return g(); } } f()")
                .unwrap(),
            Value::Number(1.0)
        );
        // Sibling blocks with the same name each bind their own declaration
        // (the already-bound check is per block); the last declaration's
        // copy wins for the var binding.
        assert_eq!(
            run("function f() { var updated; (function () { { function g() { return 'first declaration'; } } { function g() { return 'second declaration'; } } updated = g; }()); return updated(); } f()")
                .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("second declaration")))
        );
        // A closure capturing the block fn stays on the env path.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script("function f() { var a = []; { function g() { return 3; } a.push(() => g); } return a[0]()(); }")
            .unwrap();
        let cir = compiled_body_of(&mut agent, "f");
        assert!(
            cir.scope.is_none(),
            "a captured block fn must stay on the env path"
        );
        assert_eq!(
            run("function f() { var a = []; { function g() { return 3; } a.push(() => g); } return a[0]()(); } f()")
                .unwrap(),
            Value::Number(3.0)
        );
        // A statement-position declaration now certifies too (see
        // fast_path_annex_b_statement_functions).
        assert_eq!(
            run("function f() { if (true) function g() {} return typeof g; } f()").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("function")))
        );
    }

    #[test]
    fn fast_path_annex_b_statement_functions() {
        // Cut 6 continuation (Annex B): a statement-position function
        // declaration (`if (x) function f() {}`) certifies — the hoisted
        // var binding is the only observable binding (the env path's
        // transient block env is unobservable), so the statement
        // instantiates the closure into its slot. A dead declaration (a
        // parameter conflict, or a branch that never runs) leaves the var
        // binding untouched.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script("function f() { if (true) function g() { return 1; } return g(); }")
            .unwrap();
        let ir = compiled_body_of(&mut agent, "f");
        assert!(ir.scope.is_some(), "the body must certify");
        let scope = ir.scope.as_ref().unwrap();
        assert_eq!(
            scope.statement_fns.len(),
            1,
            "the statement-position declaration gets an entry"
        );
        assert!(
            ir.steps.iter().any(|s| matches!(
                s,
                crate::ir::Step::FunctionDeclInit {
                    frame_slot: Some(_),
                    ..
                }
            )),
            "the statement instantiates the closure into the var slot"
        );
        assert!(
            !ir.steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::FunctionDecl { .. })),
            "the env path is not used"
        );
        // The var binding holds the function after the branch runs.
        assert_eq!(
            run("function f() { if (true) function g() { return 42; } return g(); } f()").unwrap(),
            Value::Number(42.0)
        );
        // A branch that never runs leaves the var binding undefined.
        assert_eq!(
            run("function f() { if (false) function g() { return 1; } return typeof g; } f()")
                .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("undefined")))
        );
        // A parameter name is never overwritten (B.3.2).
        assert_eq!(
            run("function f(g) { if (true) function g() { return 3; } return g; } f(1)").unwrap(),
            Value::Number(1.0)
        );
        // A closure capturing the hoisted var binding reads it from the
        // capture context.
        assert_eq!(
            run("function f() { var a = []; if (true) function g() { return 5; } a.push(() => g); return a[0]()(); } f()")
                .unwrap(),
            Value::Number(5.0)
        );
    }

    #[test]
    fn fast_path_for_head_lexical_scopes_to_the_loop() {
        // A lexical `for`-head binding is scoped to the for statement: a
        // reference after the loop would leak the flat slot (the loop left
        // the head's value in it), so the scan bails the body to the env
        // path where the binding is unresolvable.
        assert_eq!(
            run("function f() { for (let i = 0; i < 1; i++) {} return typeof i; } f()").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("undefined")))
        );
        assert!(matches!(
            run("function f() { for (let i = 0; i < 1; i++) {} return i; } f()"),
            Err(error) if error.kind == crux::ErrorKind::ReferenceError
        ));
    }

    #[test]
    fn fast_path_strict_arguments_unmapped() {
        // Cut 3 continuation (unmapped arguments slice): a strict body's
        // own `arguments` reads get a frame slot the certified call fills
        // with the unmapped arguments object.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script("function f() { 'use strict'; return arguments.length; }")
            .unwrap();
        let ir = compiled_body_of(&mut agent, "f");
        assert!(
            ir.scope.is_some(),
            "strict arguments must keep the body certified"
        );
        assert!(
            ir.steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::CreateArguments { .. })),
            "the certified body must create the arguments object at entry"
        );
        assert_eq!(
            run("function f() { 'use strict'; return arguments.length + arguments[0] + arguments[1]; } f(1, 2)")
                .unwrap(),
            Value::Number(5.0)
        );
        // Beyond-arity arguments land in the object too.
        assert_eq!(
            run("function g(a) { 'use strict'; return arguments.length * 10 + arguments[1]; } g(1, 2, 3)")
                .unwrap(),
            Value::Number(32.0)
        );
        // The strict `callee` accessor throws.
        assert!(matches!(
            run("function h() { 'use strict'; return arguments.callee; } h()"),
            Err(error) if error.kind == crux::ErrorKind::TypeError
        ));
        // Sloppy simple-param bodies get the mapped object aliasing their
        // formals through the capture context (see fast_path_mapped_arguments).
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script("function s(a) { return arguments[0]; }")
            .unwrap();
        let ir = compiled_body_of(&mut agent, "s");
        assert!(
            ir.scope.is_some(),
            "sloppy arguments must keep the body certified"
        );
        assert!(
            ir.steps.iter().any(|s| matches!(
                s,
                crate::ir::Step::CreateArguments {
                    mapped: Some(_),
                    ..
                }
            )),
            "the certified body must create the mapped arguments object at entry"
        );
        // A `var arguments` body bails: the env path decides whether an
        // arguments object exists at all.
        agent
            .run_script("function v(a) { var arguments; return arguments[0]; }")
            .unwrap();
        assert!(
            compiled_body_of(&mut agent, "v").scope.is_none(),
            "a `var arguments` body must stay on the env path"
        );
        // An arrow's `arguments` is lexical — stays on the env path.
        assert_eq!(
            run("function outer() { return () => arguments[0]; } outer(5)()").unwrap(),
            Value::Number(5.0)
        );
    }

    #[test]
    fn fast_path_this_slot() {
        // Cut 3 continuation (this slots): a non-arrow body referencing
        // `this` certifies with a `this` slot the call fills with the
        // OrdinaryCallBindThis result.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent.run_script("function m() { return this.x; }").unwrap();
        let ir = compiled_body_of(&mut agent, "m");
        assert!(
            ir.scope.is_some(),
            "a `this` body must keep the certified frame path"
        );
        // Method call: the receiver lands in the `this` slot.
        assert_eq!(
            run("var o = { x: 5, m: function() { return this.x; } }; o.m()").unwrap(),
            Value::Number(5.0)
        );
        // Plain call, sloppy: `this` coerces to the global object.
        assert_eq!(
            run("function f() { return this; } typeof f()").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("object")))
        );
        // Plain call, strict: `this` stays undefined.
        assert_eq!(
            run("function g() { 'use strict'; return this; } g() === undefined").unwrap(),
            Value::Boolean(true)
        );
        // Construct: the constructed object lands in the `this` slot.
        assert_eq!(
            run("function C(x) { this.x = x; } var c = new C(42); c.x").unwrap(),
            Value::Number(42.0)
        );
        // Class methods certify too (strict, non-arrow).
        assert_eq!(
            run("class A { constructor(v) { this.v = v; } m() { return this.v; } } new A(7).m()")
                .unwrap(),
            Value::Number(7.0)
        );
        // An arrow's `this` is lexical — stays on the env path.
        assert_eq!(
            run("function outer() { return () => this.x; } outer.call({ x: 9 })()").unwrap(),
            Value::Number(9.0)
        );
    }

    #[test]
    fn fast_path_var_hoists_to_undefined() {
        assert_eq!(
            run("function g() { return x; var x = 1; } g()").unwrap(),
            Value::Undefined
        );
    }

    #[test]
    fn fast_path_var_has_no_tdz() {
        assert_eq!(
            run("function h() { x = 5; var x; return x; } h()").unwrap(),
            Value::Number(5.0)
        );
    }

    #[test]
    fn fast_path_ignores_extra_call_arguments() {
        // Extra arguments must not land in `var` slots (spec 10.2.11).
        assert_eq!(
            run("function f(a) { return x; var x; } f(1, 2)").unwrap(),
            Value::Undefined
        );
        assert_eq!(
            run("function f(a, b) { return a + b; } f(1, 2, 3)").unwrap(),
            Value::Number(3.0)
        );
    }

    #[test]
    fn fast_path_for_var_head_initializes_the_slot() {
        assert_eq!(
            run("function loop() { var n = 0; for (var i = 0; i < 1000; i++) { n += i; } return n; } loop()")
                .unwrap(),
            Value::Number(499500.0)
        );
    }

    #[test]
    fn fast_path_var_in_block_is_function_scoped() {
        assert_eq!(
            run("function k() { var s = 0; { var q = 2; s += q; } return s + q; } k()").unwrap(),
            Value::Number(4.0)
        );
    }

    #[test]
    fn fast_path_update_and_compound_assign() {
        assert_eq!(
            run("function u() { var n = 5; n++; return n; } u()").unwrap(),
            Value::Number(6.0)
        );
        assert_eq!(
            run("function p() { var n = 5; return ++n; } p()").unwrap(),
            Value::Number(6.0)
        );
        assert_eq!(
            run("function q() { var n = 5; return n++; } q()").unwrap(),
            Value::Number(5.0)
        );
        assert_eq!(
            run("function c() { var n = 2; n += 3; return n; } c()").unwrap(),
            Value::Number(5.0)
        );
    }

    #[test]
    fn fast_path_assignment_expression_keeps_its_value() {
        // An assignment is an expression: its value must remain on the stack
        // (spec 13.15.3 step 3) even when the target is a frame slot — the
        // harness's `while ((x = f()) !== expected)` loop shape depends on it.
        assert_eq!(
            run("function f() { var a = 0; return (a = 5) + 1; } f()").unwrap(),
            Value::Number(6.0)
        );
        assert_eq!(
            run("function g() { var a; var b; b = a = 7; return b; } g()").unwrap(),
            Value::Number(7.0)
        );
        assert_eq!(
            run("function h() { var n = 0; var r = -1; while ((r = n) !== 3) { n++; } return r; } h()")
                .unwrap(),
            Value::Number(3.0)
        );
        assert_eq!(
            run("function j() { var x = 0; var r; while ((r = ++x) < 3) {} return r; } j()")
                .unwrap(),
            Value::Number(3.0)
        );
    }

    #[test]
    fn fast_path_typeof_reads_the_slot() {
        // `typeof` of a slot binding must read the frame, not walk the
        // environment (a fast body has no env binding for the name). The
        // harness's `$DETACHBUFFER` shape (`typeof buffer !== "object"`)
        // depends on it.
        assert_eq!(
            run("function f(x) { return typeof x; } f(42)").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("number")))
        );
        assert_eq!(
            run("function g() { var v = 1; return typeof v; } g()").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("number")))
        );
        assert_eq!(
            run("function h(b) { if (typeof b !== 'object' || b === null || typeof b.transfer !== 'function') { return 'no'; } return 'yes'; } h({ transfer: function () {} })")
                .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("yes")))
        );
        assert_eq!(
            run("function k() { return typeof missing_global; } k()").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("undefined")))
        );
    }

    #[test]
    fn fast_path_delete_of_a_slot_is_false() {
        // A slot binding is never unresolvable, so sloppy `delete x` is false
        // (spec 13.5.1.2) — the env-path `DeleteIdent` would report true.
        assert_eq!(
            run("function d() { var x = 1; return delete x; } d()").unwrap(),
            Value::Boolean(false)
        );
    }

    #[test]
    fn fast_path_reads_outer_bindings_through_the_env() {
        assert_eq!(
            run("var base = 10; function add(x) { return x + base; } add(5)").unwrap(),
            Value::Number(15.0)
        );
    }

    #[test]
    fn slow_path_this_body_keeps_behavior() {
        assert_eq!(
            run("function m() { return this.x; } m.call({ x: 9 })").unwrap(),
            Value::Number(9.0)
        );
        // Cut 3 continuation: a non-arrow `this` body now certifies with a
        // `this` slot (see fast_path_this_slot); the behavior above is
        // unchanged. An arrow's lexical `this` still bails.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent.run_script("var f = () => this.x;").unwrap();
        let ir = compiled_body_of(&mut agent, "f");
        assert!(
            ir.scope.is_none(),
            "an arrow `this` body must stay on the env path"
        );
    }

    #[test]
    fn slow_path_arguments_body_keeps_behavior() {
        assert_eq!(
            run("function a() { return arguments[0]; } a(7)").unwrap(),
            Value::Number(7.0)
        );
        // Cut 3 continuation: a sloppy simple-param `arguments` body now
        // certifies with the mapped object (see fast_path_mapped_arguments);
        // a `var arguments` body still bails — the env path decides whether
        // an arguments object exists at all.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script("function a() { var arguments; return arguments[0]; }")
            .unwrap();
        let ir = compiled_body_of(&mut agent, "a");
        assert!(
            ir.scope.is_none(),
            "a `var arguments` body must stay on the env path"
        );
        assert_eq!(
            run("function a() { var arguments; return arguments[0]; } a(7)").unwrap(),
            Value::Number(7.0)
        );
    }

    #[test]
    fn fast_path_mapped_arguments() {
        // Cut 3 continuation (mapped arguments slice): a sloppy simple-param
        // body observing `arguments` certifies with every param captured —
        // the mapped object's accessors and the body's `LoadContextSlot`s
        // share the capture-context bindings, so `arguments[i]` aliases the
        // parameter both ways.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script("function f(a, b) { arguments[0] = 9; return a + b + arguments.length; }")
            .unwrap();
        let ir = compiled_body_of(&mut agent, "f");
        assert!(
            ir.scope.is_some(),
            "sloppy mapped arguments must keep the body certified"
        );
        assert!(
            ir.steps.iter().any(|s| matches!(
                s,
                crate::ir::Step::CreateArguments {
                    mapped: Some(formals),
                    ..
                } if formals.len() == 2
            )),
            "the certified body must create the mapped object with the formals"
        );
        // `arguments[0] = 9` writes the parameter binding (9 + 2 + 2).
        assert_eq!(
            run("function f(a, b) { arguments[0] = 9; return a + b + arguments.length; } f(1, 2)")
                .unwrap(),
            Value::Number(13.0)
        );
        // The other direction: `a = 8` is visible through `arguments[0]`.
        assert_eq!(
            run("function g(a) { a = 8; return arguments[0]; } g(1)").unwrap(),
            Value::Number(8.0)
        );
        // Beyond-arity arguments land in the object without mapping.
        assert_eq!(
            run("function h(a) { return arguments.length * 10 + arguments[1]; } h(1, 2, 3)")
                .unwrap(),
            Value::Number(32.0)
        );
        // The sloppy `callee` is the function itself (the strict accessor
        // throws — covered in fast_path_strict_arguments_unmapped).
        assert_eq!(
            run("function k() { return arguments.callee === k; } k()").unwrap(),
            Value::Boolean(true)
        );
    }

    // ---- Cut 4: fused loop test + update ----

    #[test]
    fn cut4_loop_fuses_test_and_update() {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent
            .run_script("function loop() { var n = 0; for (var i = 0; i < 100; i++) { n += i; } return n; }")
            .unwrap();
        let ir = compiled_body_of(&mut agent, "loop");
        assert!(ir.scope.is_some(), "the loop body must be certified");
        assert!(
            ir.steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::JumpIfLtImm { .. }))
        );
        assert!(
            ir.steps.iter().any(|s| {
                matches!(
                    s,
                    crate::ir::Step::FastLoopHead {
                        var: crate::ir::FastLoopVar::Slot(_) | crate::ir::FastLoopVar::Counter,
                        ..
                    }
                )
            }),
            "the fused canonical loop head must own the increment"
        );
        assert!(
            !ir.steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::JumpIfFalse(_))),
            "the loop test must not emit LoadLocal + BinaryImm + JumpIfFalse"
        );
        assert!(
            !ir.steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::UpdateLocal { .. })),
            "the for-update must not emit UpdateLocal + Pop"
        );
    }

    #[test]
    fn cut4_fused_loop_behaves() {
        assert_eq!(
            run("function loop() { var n = 0; for (var i = 0; i < 1000; i++) { n += i; } return n; } loop()")
                .unwrap(),
            Value::Number(499500.0)
        );
        // The while-test fuses too; a statement-position `i++` (whose value
        // matters) stays on the general path.
        assert_eq!(
            run("function w() { var n = 0; var i = 0; while (i < 1000) { n += i; i++; } return n; } w()")
                .unwrap(),
            Value::Number(499500.0)
        );
        // Descending loop: `i > 0` and `i--` fuse.
        assert_eq!(
            run(
                "function d() { var n = 0; for (var i = 10; i > 0; i--) { n += i; } return n; } d()"
            )
            .unwrap(),
            Value::Number(55.0)
        );
        // `<=` / `>=` fuse.
        assert_eq!(
            run(
                "function e() { var n = 0; for (var i = 0; i <= 5; i++) { n += i; } return n; } e()"
            )
            .unwrap(),
            Value::Number(15.0)
        );
        assert_eq!(
            run("function f2() { var n = 0; for (var i = 5; i >= 0; i--) { n += i; } return n; } f2()")
                .unwrap(),
            Value::Number(15.0)
        );
    }

    #[test]
    fn acc_path_labeled_transfers_out_sync_the_counter_binding() {
        // Cut 17 acc-path: the counter lives in `loop_counter` and the
        // compiler syncs it back to the binding with a single
        // `FastLoopStore` at the loop's END step. A labeled break/continue
        // to a label declared OUTSIDE the loop body (the loop's own label
        // or an enclosing label) jumps past that step, which used to leave
        // the binding at its pre-loop value (0) — observable after the
        // transfer. Such bodies now fall back to the slot path (the head
        // writes the binding every iteration), so the exit values are
        // correct: s = 15 with i = 5.
        assert_eq!(
            run("function f() { var s = 0; outer: for (var i = 0; i < 10; i++) { s += i; if (i === 5) break outer; } return s * 100 + i; } f()")
                .unwrap(),
            Value::Number(1505.0)
        );
        // Labeled `continue` to an enclosing loop from an inner var-head
        // loop: the inner counter j = 2 when the continue fires.
        assert_eq!(
            run("function g() { var s = 0; outer: for (var i = 2; i >= 0; i--) { for (var j = 0; j < 10; j++) { if (j === 2) continue outer; s++; } } return s * 10 + j; } g()")
                .unwrap(),
            Value::Number(62.0)
        );
        // A label wrapping a BLOCK around the loop: `break outer` skips the
        // loop's own end entirely.
        assert_eq!(
            run("function h() { var s = 0; outer: { for (var i = 0; i < 10; i++) { s += i; if (i === 5) break outer; } } return s * 100 + i; } h()")
                .unwrap(),
            Value::Number(1505.0)
        );
        // A labeled transfer to a label declared INSIDE the body stays on
        // the fast path and is exact (the break lands before the end step).
        assert_eq!(
            run("function k() { var s = 0; for (var i = 0; i < 10; i++) { lab: { if (i === 5) break lab; s += i; } } return s; } k()")
                .unwrap(),
            Value::Number(40.0)
        );
        // `continue` to the loop's OWN label also leaves the binding
        // correct on every exit (the acc gate rejects it conservatively).
        assert_eq!(
            run("function m() { var s = 0; outer: for (var i = 0; i < 10; i++) { if (i === 5) continue outer; s += i; } return s * 100 + i; } m()")
                .unwrap(),
            Value::Number(4010.0)
        );
    }

    #[test]
    fn cut4_fused_test_coerces_and_skips_compound() {
        // The fused comparison keeps the general abstract semantics: a
        // string slot value coerces numerically against the Number literal.
        assert_eq!(
            run("function s() { var i = '5'; var n = 0; while (i < 10) { n++; i = String(Number(i) + 1); } return n; } s()")
                .unwrap(),
            Value::Number(5.0)
        );
        // `i += 1` is NOT fused (`+=` concatenates strings, `++` does not),
        // so the loop body's string compound still concatenates.
        assert_eq!(
            run("function c() { var s = 'a'; for (var i = 0; i < 2; i++) { s += 'b'; } return s; } c()")
                .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("abb")))
        );
    }

    // ---- Cut 5: script-level bindings (top-level vars) ----

    #[test]
    fn fast_script_compiles_globals_to_direct_access() {
        let program =
            parser::parse_script("var n = 0; for (var i = 0; i < 100; i++) { n += i; } n").unwrap();
        let body = crate::ir::compile_statements(&program.body, false, true).unwrap();
        assert!(
            body.script_globals.is_some(),
            "the script must be certified"
        );
        assert!(
            body.steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::StoreGlobal { .. }))
        );
        // Cut 16: a closed-world script's declared vars are frame slots —
        // the loop's test fuses against a slot and the global sync steps
        // are the prologue/epilogue.
        assert!(
            body.steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::JumpIfLtImm { .. }))
        );
        assert!(
            body.steps.iter().any(|s| {
                matches!(
                    s,
                    crate::ir::Step::FastLoopHead {
                        var: crate::ir::FastLoopVar::Slot(_) | crate::ir::FastLoopVar::Counter,
                        ..
                    }
                )
            }),
            "the fused canonical loop head must own the slot increment"
        );
        assert!(
            body.scope.is_some(),
            "a closed-world script must get frame slots"
        );
        assert!(
            !body
                .steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::LoadIdent { .. })),
            "declared var reads must not walk the environment"
        );
        assert!(
            !body
                .steps
                .iter()
                .any(|s| matches!(s, crate::ir::Step::JumpIfFalse(_))),
            "the loop test must fuse against the global binding"
        );
        // The eval/module mode never certifies.
        let body = crate::ir::compile_statements(&program.body, false, false).unwrap();
        assert!(
            body.script_globals.is_none(),
            "eval/module mode must stay env-path"
        );
    }

    #[test]
    fn fast_script_behavior() {
        assert_eq!(
            run("var n = 0; for (var i = 0; i < 1000; i++) { n += i * 2; } n").unwrap(),
            Value::Number(999000.0)
        );
        // Var hoisting: the instantiation created the global property.
        assert_eq!(run("x; var x = 1;").unwrap(), Value::Undefined);
        // `typeof` of an undeclared name is "undefined", not an error.
        assert_eq!(
            run("typeof not_declared_anywhere").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("undefined")))
        );
        // Reading an undeclared name still throws.
        assert!(matches!(
            run("not_declared_anywhere"),
            Err(error) if error.kind == crux::ErrorKind::ReferenceError
        ));
    }

    #[test]
    fn fast_script_bails_preserve_env_behavior() {
        // try/catch (a catch param shadows a same-named global), `with`,
        // and direct eval all fall back to the env path and keep their
        // semantics.
        assert_eq!(
            run("var n = 0; try { throw 7; } catch (e) { n = e; } n").unwrap(),
            Value::Number(7.0)
        );
        assert_eq!(
            run("var n = 0; var o = { x: 9 }; with (o) { n = x; } n").unwrap(),
            Value::Number(9.0)
        );
        assert_eq!(
            run("var n = 0; eval('var n = 5;'); n").unwrap(),
            Value::Number(5.0)
        );
    }

    #[test]
    fn fast_script_globals_share_the_global_object() {
        // Two script evaluations in one realm: the second sees the first's
        // vars — the fast path reads the global object directly.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        agent.run_script("var x = 41;").unwrap();
        let value = agent.run_script("x + 1").unwrap();
        assert_eq!(value, Value::Number(42.0));
    }
}
