//! ECMAScript function objects (spec ch. 15): the 10.2.1 slots live in the
//! agent's `ecma_functions` table (keyed by function identity), and this
//! module implements `Call`/`Construct` for ordinary functions — parameter
//! binding, FunctionDeclarationInstantiation, `this`/`new.target` handling,
//! and the `arguments` object.

use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::PropertyDescriptor;
use crux::string::JsString;
use crux::value::Value;
use syntax::ast::{
    ArrowBody, BindingElement, BindingPattern, Block, ExprKind, Literal, Stmt, StmtKind,
};

use crate::agent::Agent;
use crate::context::ExecutionContext;
use crate::env::{EnvRef, new_declarative_environment, new_function_environment};
use crate::eval::eval_statement_list;
use crate::flow::Completion;
use crate::realm::Realm;
use crate::script::{
    bound_names, is_constant_declaration, top_level_lexically_declared_names,
    top_level_lexically_scoped_declarations, top_level_var_scoped_declarations,
};

/// [[ThisMode]] (spec 10.2.1): how the function binds `this`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ThisMode {
    /// Arrow functions: no `this` binding of their own.
    Lexical,
    Strict,
    Sloppy,
}

/// The spec 10.2.1 internal slots of an ordinary ECMAScript function,
/// registered per function object.
#[derive(Debug, Clone)]
pub struct EcmaFunction {
    pub name: Option<JsString>,
    pub params: Vec<BindingElement>,
    pub body: Block,
    /// [[Environment]]: the lexical environment at instantiation.
    pub environment: EnvRef,
    pub this_mode: ThisMode,
    /// [[Strict]]: the body's strictness (distinct from [[ThisMode]] for
    /// arrows, which are always ~lexical~).
    pub strict: bool,
    /// [[HomeObject]]: set for methods (super property access, Phase 7).
    pub home_object: Option<Value>,
    pub realm: Handle<Realm>,
    pub is_async: bool,
    pub is_generator: bool,
}

fn not_implemented(what: &str) -> JsError {
    JsError::new(
        ErrorKind::TypeError,
        format!("{what} is not implemented until later Phase 7 work"),
    )
}

/// FunctionBodyContainsUseStrict (spec 15.2.1): a `"use strict"` directive in
/// the function body's directive prologue.
pub fn function_is_strict(f: &syntax::ast::Function) -> bool {
    for stmt in &f.body.stmts {
        let StmtKind::Expr(expr) = &stmt.kind else {
            return false;
        };
        let ExprKind::Literal(Literal::Str(value)) = &expr.kind else {
            return false;
        };
        if value.to_string_lossy() == "use strict" {
            return true;
        }
    }
    false
}

/// IsSimpleParameterList (spec 15.1.1): every parameter is a bare binding
/// identifier with no initializer and no rest.
pub fn is_simple_parameter_list(params: &[BindingElement]) -> bool {
    params
        .iter()
        .all(|p| !p.rest && p.init.is_none() && matches!(p.pattern, BindingPattern::Ident(_)))
}

/// SetFunctionLength (spec 10.2.6): the number of parameters before the
/// first default or rest parameter.
pub fn function_length(params: &[BindingElement]) -> u64 {
    let mut count = 0u64;
    for param in params {
        if param.rest || param.init.is_some() {
            break;
        }
        count += 1;
    }
    count
}

/// SetFunctionName (spec 10.2.7) plus the `length` property; `prototype` is
/// added separately by `make_constructor` for non-arrow functions.
fn set_function_properties(
    function: &Handle<Function>,
    params: &[BindingElement],
    name: Option<&JsString>,
) -> Result<(), JsError> {
    function.define_property(
        &JsString::from_utf8("length"),
        &PropertyDescriptor {
            value: Some(Value::Number(function_length(params) as f64)),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    let name = name.cloned().unwrap_or_else(|| JsString::from_utf8(""));
    function.define_property(
        &JsString::from_utf8("name"),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(name))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// MakeConstructor (spec 10.2.5): the `prototype` property with a
/// `constructor` back-reference. Arrows and methods skip this.
fn make_constructor(function: &Handle<Function>) -> Result<(), JsError> {
    let prototype = JsObject::ordinary_object_create(None);
    prototype.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(function.self_value()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    function.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(Value::Object(prototype)),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    Ok(())
}

/// InstantiateOrdinaryFunctionObject (spec 15.2.4): register a function
/// declaration or expression. `strict` is the enclosing code's strictness,
/// inherited when the body has no directive of its own.
pub fn instantiate_function(
    agent: &mut Agent,
    f: &syntax::ast::Function,
    environment: EnvRef,
    enclosing_strict: bool,
) -> Result<Value, JsError> {
    if f.is_async || f.is_generator {
        return Err(not_implemented("async and generator functions"));
    }
    let name = f.name.map(crux::lookup);
    let strict = function_is_strict(f) || enclosing_strict;
    let this_mode = if strict {
        ThisMode::Strict
    } else {
        ThisMode::Sloppy
    };
    let realm = agent.current_realm()?;
    let data = EcmaFunction {
        name: name.clone(),
        params: f.params.clone(),
        body: f.body.clone(),
        environment,
        this_mode,
        strict,
        home_object: None,
        realm,
        is_async: f.is_async,
        is_generator: f.is_generator,
    };
    let function = Function::new(name.clone());
    agent.ecma_functions.insert(function.id(), data);
    set_function_properties(&function, &f.params, name.as_ref())?;
    make_constructor(&function)?;
    Ok(Value::Function(function))
}

/// InstantiateOrdinaryFunctionExpression (spec 15.2.5): a named function
/// expression additionally binds its name immutably in a fresh scope.
pub fn instantiate_function_expression(
    agent: &mut Agent,
    f: &syntax::ast::Function,
    environment: EnvRef,
    enclosing_strict: bool,
) -> Result<Value, JsError> {
    let Some(name) = f.name else {
        return instantiate_function(agent, f, environment, enclosing_strict);
    };
    let name = crux::lookup(name);
    let scope = new_declarative_environment(Some(environment));
    let value = instantiate_function(agent, f, scope.clone(), enclosing_strict)?;
    scope.create_immutable_binding(&name, true)?;
    scope.initialize_binding(&name, value.clone())?;
    Ok(value)
}

/// Instantiate an arrow function: `[[ThisMode]]` is ~lexical~ and there is
/// no `prototype` (spec 15.3.2).
pub fn instantiate_arrow(
    agent: &mut Agent,
    is_async: bool,
    params: Vec<BindingElement>,
    body: ArrowBody,
    environment: EnvRef,
    enclosing_strict: bool,
) -> Result<Value, JsError> {
    if is_async {
        return Err(not_implemented("async arrow functions"));
    }
    let body = match body {
        ArrowBody::Expr(expr) => {
            let span = expr.span;
            Block {
                stmts: vec![Stmt {
                    span,
                    kind: StmtKind::Return(Some(*expr)),
                }],
                span,
            }
        }
        ArrowBody::Block(block) => block,
    };
    let realm = agent.current_realm()?;
    let data = EcmaFunction {
        name: None,
        params,
        body,
        environment,
        this_mode: ThisMode::Lexical,
        strict: enclosing_strict,
        home_object: None,
        realm,
        is_async: false,
        is_generator: false,
    };
    let function = Function::new(None);
    let params = data.params.clone();
    agent.ecma_functions.insert(function.id(), data);
    set_function_properties(&function, &params, None)?;
    Ok(Value::Function(function))
}

/// Call (spec 10.2.1): dispatch an ECMAScript function through its body, and
/// everything else through `crux::function::call`. Bound chains are unwrapped
/// here so they can reach user-function targets.
pub fn call(
    agent: &mut Agent,
    callee: &Value,
    this: Value,
    args: &[Value],
) -> Result<Value, JsError> {
    match callee {
        Value::Function(function) => match &function.kind {
            crux::function::FunctionKind::EcmaScript => ordinary_call(agent, function, this, args),
            crux::function::FunctionKind::Bound {
                target,
                bound_this,
                bound_args,
            } => {
                let mut all = bound_args.clone();
                all.extend_from_slice(args);
                call(agent, target, bound_this.clone(), &all)
            }
            _ => crux::function::call(callee, this, args),
        },
        _ => crux::function::call(callee, this, args),
    }
}

/// Construct (spec 10.2.1): like `call` for the `new` operator, with
/// newTarget propagation through bound functions.
pub fn construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    match callee {
        Value::Function(function) => match &function.kind {
            crux::function::FunctionKind::EcmaScript => {
                ordinary_construct(agent, function, args, new_target)
            }
            crux::function::FunctionKind::Bound {
                target, bound_args, ..
            } => {
                let mut all = bound_args.clone();
                all.extend_from_slice(args);
                let target_value = target.clone();
                let new_target =
                    if crux::ops::same_value(&Value::Function(function.clone()), new_target) {
                        &target_value
                    } else {
                        new_target
                    };
                construct(agent, target, &all, new_target)
            }
            _ => crux::function::construct(callee, args, new_target),
        },
        _ => crux::function::construct(callee, args, new_target),
    }
}

/// PrepareForOrdinaryCall (spec 10.2.1.2) + OrdinaryCallBindThis (10.2.1.1)
/// + OrdinaryCallEvaluateBody: the full `[[Call]]` of an ordinary function.
fn ordinary_call(
    agent: &mut Agent,
    function: &Handle<Function>,
    this: Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let data = agent
        .ecma_functions
        .get(&function.id())
        .cloned()
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "Function body is not registered".into(),
            )
        })?;
    let function_value = function.self_value();
    let old_env = data.environment.clone();
    let function_env = new_function_environment(
        Some(old_env),
        function_value.clone(),
        Value::Undefined,
        data.this_mode == ThisMode::Lexical,
    );
    agent.execution_context_stack.push(ExecutionContext {
        function: Some(function_value.clone()),
        realm: data.realm.clone(),
        script_or_module: None,
        lexical_environment: function_env.clone(),
        variable_environment: function_env.clone(),
        private_environment: None,
    });
    let result = (|| -> Result<Value, JsError> {
        // OrdinaryCallBindThis: strict keeps `this`; sloppy coerces
        // undefined/null to the global object; lexical binds nothing.
        if data.this_mode != ThisMode::Lexical {
            let this = if data.this_mode == ThisMode::Sloppy
                && matches!(this, Value::Undefined | Value::Null)
            {
                let global = agent.running_context()?.realm.global_object.clone();
                Value::Object(global)
            } else {
                this
            };
            function_env.bind_this_value(this)?;
        }
        function_declaration_instantiation(agent, &function_value, &data, args, &function_env)?;
        evaluate_body(agent, &data)
    })();
    agent.execution_context_stack.pop();
    result
}

/// OrdinaryCallEvaluateBody: evaluate the body; a `return` completion is the
/// result, any other normal completion yields *undefined* (spec 15.2.2).
fn evaluate_body(agent: &mut Agent, data: &EcmaFunction) -> Result<Value, JsError> {
    match eval_statement_list(agent, &data.body.stmts, data.strict)? {
        Completion::Return(value) => Ok(value),
        Completion::Normal(_) | Completion::Empty => Ok(Value::Undefined),
        Completion::Throw(value) => Err(JsError::new(
            ErrorKind::TypeError,
            format!("Uncaught {value:?}"),
        )),
        Completion::Break { .. } | Completion::Continue { .. } => Err(JsError::new(
            ErrorKind::SyntaxError,
            "Illegal break/continue statement".into(),
        )),
    }
}

/// The `[[Construct]]` of an ordinary function (spec 10.2.1): create `this`
/// from the constructor's prototype, bind it, and run the body.
fn ordinary_construct(
    agent: &mut Agent,
    function: &Handle<Function>,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    let data = agent
        .ecma_functions
        .get(&function.id())
        .cloned()
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "Function body is not registered".into(),
            )
        })?;
    if data.this_mode == ThisMode::Lexical {
        return Err(JsError::new(
            ErrorKind::TypeError,
            format!("{} is not a constructor", type_of(&function.self_value())),
        ));
    }
    // GetPrototypeFromConstructor (spec 10.2.4): newTarget's `prototype`
    // when it is an object; %Object.prototype% joins with the Phase 8
    // intrinsics, until then a null-prototype object.
    let prototype = crate::context::get_property(
        new_target,
        &JsString::from_utf8("prototype"),
        new_target.clone(),
    )?;
    let proto = match prototype {
        Value::Object(obj) => Some(obj),
        _ => None,
    };
    let this = Value::Object(JsObject::ordinary_object_create(proto));
    let function_value = function.self_value();
    let old_env = data.environment.clone();
    let function_env = new_function_environment(
        Some(old_env),
        function_value.clone(),
        new_target.clone(),
        false,
    );
    agent.execution_context_stack.push(ExecutionContext {
        function: Some(function_value),
        realm: data.realm.clone(),
        script_or_module: None,
        lexical_environment: function_env.clone(),
        variable_environment: function_env.clone(),
        private_environment: None,
    });
    let result = (|| -> Result<Value, JsError> {
        function_env.bind_this_value(this.clone())?;
        function_declaration_instantiation(
            agent,
            &function.self_value(),
            &data,
            args,
            &function_env,
        )?;
        match eval_statement_list(agent, &data.body.stmts, data.strict)? {
            // spec 10.2.1 [[Construct]]: an object return wins; base
            // constructors fall back to `this` for any other value.
            Completion::Return(value) => match value {
                Value::Object(_) | Value::Function(_) => Ok(value),
                _ => Ok(this),
            },
            Completion::Normal(_) | Completion::Empty => Ok(this),
            Completion::Throw(value) => Err(JsError::new(
                ErrorKind::TypeError,
                format!("Uncaught {value:?}"),
            )),
            Completion::Break { .. } | Completion::Continue { .. } => Err(JsError::new(
                ErrorKind::SyntaxError,
                "Illegal break/continue statement".into(),
            )),
        }
    })();
    agent.execution_context_stack.pop();
    result
}

/// FunctionDeclarationInstantiation (spec 16.1.8), simple-parameter path:
/// bind the parameters, create the `arguments` object, hoist var bindings
/// (initialized to *undefined*) and top-level function declarations, and set
/// up the lexical environment for the body.
fn function_declaration_instantiation(
    agent: &mut Agent,
    function_value: &Value,
    data: &EcmaFunction,
    args: &[Value],
    function_env: &EnvRef,
) -> Result<(), JsError> {
    let strict = data.strict;
    let simple = is_simple_parameter_list(&data.params);

    // BoundNames of the formal parameters.
    let mut param_names: Vec<JsString> = Vec::new();
    for param in &data.params {
        bound_names(&param.pattern, &mut param_names);
    }

    // IteratorBindingInitialization of the formals (spec 16.1.8 step 31 for
    // the simple path): bind each parameter positionally.
    for (index, param) in data.params.iter().enumerate() {
        let BindingPattern::Ident(name) = &param.pattern else {
            return Err(not_implemented("destructured and default parameters"));
        };
        if param.rest || param.init.is_some() {
            return Err(not_implemented("rest and default parameters"));
        }
        let name = crux::lookup(*name);
        if !function_env.has_binding(&name)? {
            function_env.create_mutable_binding(&name, false)?;
        }
        let value = args.get(index).cloned().unwrap_or(Value::Undefined);
        function_env.initialize_binding(&name, value)?;
    }

    // The arguments object (spec 16.1.8 steps 20-23).
    let lexical_names = top_level_lexically_declared_names(&data.body.stmts);
    let func_names: Vec<JsString> = data
        .body
        .stmts
        .iter()
        .filter_map(|s| match &s.kind {
            StmtKind::FunctionDecl(f) => f.name.map(crux::lookup),
            _ => None,
        })
        .collect();
    let arguments_obj_needed = !(data.this_mode == ThisMode::Lexical
        || param_names.contains(&JsString::from_utf8("arguments"))
        || (simple
            && (func_names.contains(&JsString::from_utf8("arguments"))
                || lexical_names.contains(&JsString::from_utf8("arguments")))));
    let mut param_bindings = param_names.clone();
    if arguments_obj_needed {
        let arguments_obj = if strict || !simple {
            Value::Object(JsObject::unmapped_arguments_object_create(args)?)
        } else {
            let env = function_env.clone();
            let make_getter = move |name: &JsString| -> Value {
                let env = env.clone();
                let name = name.clone();
                Value::Function(
                    Function::create_builtin(
                        None,
                        0,
                        Box::new(move |_, _| env.get_binding_value(&name, false)),
                        None,
                        None,
                    )
                    .unwrap_or_else(|_| Function::new(None)),
                )
            };
            let env = function_env.clone();
            let make_setter = move |name: &JsString| -> Value {
                let env = env.clone();
                let name = name.clone();
                Value::Function(
                    Function::create_builtin(
                        None,
                        0,
                        Box::new(move |_, value| {
                            let value = value.first().cloned().unwrap_or(Value::Undefined);
                            env.set_mutable_binding(&name, value, false)?;
                            Ok(Value::Undefined)
                        }),
                        None,
                        None,
                    )
                    .unwrap_or_else(|_| Function::new(None)),
                )
            };
            Value::Object(JsObject::mapped_arguments_object_create(
                function_value.clone(),
                &param_names,
                args,
                make_getter,
                make_setter,
            )?)
        };
        if strict {
            function_env.create_immutable_binding(&JsString::from_utf8("arguments"), false)?;
        } else {
            function_env.create_mutable_binding(&JsString::from_utf8("arguments"), false)?;
        }
        function_env.initialize_binding(&JsString::from_utf8("arguments"), arguments_obj)?;
        param_bindings.push(JsString::from_utf8("arguments"));
    }

    // Var bindings: created and initialized to *undefined* during
    // instantiation (spec 16.1.8 steps 33-36, simple path).
    let mut instantiated = param_bindings;
    for decl in top_level_var_scoped_declarations(&data.body.stmts) {
        let crate::script::VarScopedDecl::Variable(names) = decl else {
            continue;
        };
        for name in names {
            if !instantiated.contains(&name) {
                instantiated.push(name.clone());
                function_env.create_mutable_binding(&name, false)?;
                function_env.initialize_binding(&name, Value::Undefined)?;
            }
        }
    }

    // The body's lexical environment: strict functions share the function
    // env; sloppy functions get a fresh declarative record so direct eval
    // cannot see the var bindings (spec 16.1.8 steps 37-42).
    let lexical_env = if strict {
        function_env.clone()
    } else {
        new_declarative_environment(Some(function_env.clone()))
    };
    agent.running_context_mut()?.lexical_environment = lexical_env.clone();

    // Lexically declared names: instantiated but not initialized (spec
    // 16.1.8 steps 43-48).
    for decl in top_level_lexically_scoped_declarations(&data.body.stmts) {
        let constant = is_constant_declaration(decl);
        let mut names = Vec::new();
        bound_names_of_decl(decl, &mut names);
        for name in names {
            if constant {
                lexical_env.create_immutable_binding(&name, true)?;
            } else {
                lexical_env.create_mutable_binding(&name, false)?;
            }
        }
    }

    // Top-level function declarations: instantiated against the lexical env
    // and bound in the variable env; the last declaration of a name wins
    // (spec 16.1.8 steps 49-52).
    let mut funcs: Vec<&syntax::ast::Function> = Vec::new();
    for stmt in &data.body.stmts {
        if let StmtKind::FunctionDecl(f) = &stmt.kind
            && let Some(name) = f.name
        {
            if let Some(slot) = funcs.iter_mut().find(|g| g.name == Some(name)) {
                *slot = f;
            } else {
                funcs.push(f);
            }
        }
    }
    for f in funcs {
        let name = crux::lookup(f.name.unwrap());
        let func_obj = instantiate_function(agent, f, lexical_env.clone(), strict)?;
        function_env.set_mutable_binding(&name, func_obj, false)?;
    }
    Ok(())
}

/// The bound names of a lexical declaration (a let/const/class/using stmt).
fn bound_names_of_decl(stmt: &Stmt, out: &mut Vec<JsString>) {
    match &stmt.kind {
        StmtKind::VarDecl { decls, .. } | StmtKind::UsingDecl { decls, .. } => {
            for decl in decls {
                bound_names(&decl.pattern, out);
            }
        }
        StmtKind::ClassDecl(class) => {
            if let Some(name) = class.name {
                out.push(crux::lookup(name));
            }
        }
        _ => {}
    }
}

fn type_of(value: &Value) -> &'static str {
    crux::value::type_of(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::evaluate;

    fn run(source: &str) -> Result<Value, JsError> {
        evaluate(source)
    }

    fn number(value: f64) -> Value {
        Value::Number(value)
    }

    #[test]
    fn basic_calls_bind_parameters_and_return() {
        assert_eq!(
            run("function f(a, b) { return a + b; } f(2, 3)").unwrap(),
            number(5.0)
        );
        // Missing arguments are *undefined*; extra arguments are ignored.
        assert_eq!(
            run("function g(a) { return a; } g()").unwrap(),
            Value::Undefined
        );
        assert_eq!(
            run("function h(a) { return a; } h(1, 2, 3)").unwrap(),
            number(1.0)
        );
        // A bare `return` and a fall-through body both yield *undefined*.
        assert_eq!(
            run("function r() { return; } r()").unwrap(),
            Value::Undefined
        );
        assert_eq!(run("function n() { 42; } n()").unwrap(), Value::Undefined);
    }

    #[test]
    fn var_and_function_declarations_hoist() {
        // Var bindings initialize to *undefined* at instantiation.
        assert_eq!(
            run("function f() { return x; var x = 5; } f()").unwrap(),
            Value::Undefined
        );
        assert_eq!(
            run("function f() { var x = 5; return x; } f()").unwrap(),
            number(5.0)
        );
        // Function declarations are instantiated before the body runs.
        assert_eq!(
            run("function f() { return g(); function g() { return 1; } } f()").unwrap(),
            number(1.0)
        );
        // A var with the same name as a parameter shares the parameter.
        assert_eq!(
            run("function f(a) { var a = 7; return a; } f(1)").unwrap(),
            number(7.0)
        );
    }

    #[test]
    fn recursion_and_closures() {
        assert_eq!(
            run("function fact(n) { return n <= 1 ? 1 : n * fact(n - 1); } fact(5)").unwrap(),
            number(120.0)
        );
        assert_eq!(
            run("function outer(x) { return function (y) { return x + y; }; } outer(2)(3)")
                .unwrap(),
            number(5.0)
        );
    }

    #[test]
    fn this_binding_modes() {
        // Sloppy: a bare call coerces `this` to the global object.
        assert_eq!(
            run("function f() { return this === globalThis; } f()").unwrap(),
            Value::Boolean(true)
        );
        // Strict: `this` stays *undefined* for a bare call.
        assert_eq!(
            run("function f() { 'use strict'; return this; } f()").unwrap(),
            Value::Undefined
        );
        // Strictness is inherited from the enclosing code.
        assert_eq!(
            run("'use strict'; function f() { return this; } f()").unwrap(),
            Value::Undefined
        );
        // A method call binds `this` to the receiver.
        assert_eq!(
            run("let o = { x: 5, m: function () { return this.x; } }; o.m()").unwrap(),
            number(5.0)
        );
    }

    #[test]
    fn arguments_object_is_mapped_in_sloppy_mode() {
        assert_eq!(
            run("function f(a, b) { return arguments.length + arguments[0] + arguments[1]; } f(1, 2)")
                .unwrap(),
            number(5.0)
        );
        // Sloppy simple parameters: the arguments object is mapped.
        assert_eq!(
            run("function f(a) { arguments[0] = 99; return a; } f(1)").unwrap(),
            number(99.0)
        );
        // Strict functions get an unmapped arguments object.
        assert_eq!(
            run("function f(a) { 'use strict'; arguments[0] = 99; return a; } f(1)").unwrap(),
            number(1.0)
        );
    }

    #[test]
    fn declaration_statements_have_empty_completions() {
        // Function/var declarations complete with an ~empty~ value that
        // inherits the preceding statement list's value (spec 14.2.2).
        assert_eq!(run("1; function f() {}").unwrap(), number(1.0));
        assert_eq!(run("1; var x = 5;").unwrap(), number(1.0));
        assert_eq!(run("function f() {}").unwrap(), Value::Undefined);
        assert_eq!(run("var x;").unwrap(), Value::Undefined);
    }

    #[test]
    fn constructors_initialize_this() {
        assert_eq!(
            run("function C(x) { this.x = x; } new C(5).x").unwrap(),
            number(5.0)
        );
        // A returned object wins; a returned primitive is ignored.
        assert_eq!(
            run("function C() { return { y: 7 }; } new C().y").unwrap(),
            number(7.0)
        );
        assert_eq!(
            run("function C() { this.z = 1; return 2; } new C().z").unwrap(),
            number(1.0)
        );
    }

    #[test]
    fn function_expressions_and_names() {
        // A named function expression binds its own name inside.
        assert_eq!(
            run("var g = function inner() { return typeof inner; }; g()").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("function")))
        );
        // length/name/prototype properties (spec 10.2.5-10.2.7).
        assert_eq!(run("function f(a, b) {} f.length").unwrap(), number(2.0));
        assert_eq!(
            run("function f(a, b) {} f.name").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("f")))
        );
        assert_eq!(
            run("function f() {} f.prototype.constructor === f").unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn arrow_functions_are_lexical() {
        assert_eq!(run("((a, b) => a + b)(2, 3)").unwrap(), number(5.0));
        assert_eq!(run("(() => 42)()").unwrap(), number(42.0));
        // Arrows capture `this` lexically from the enclosing function.
        assert_eq!(
            run("let o = { x: 5, m: function () { let inner = () => this; return inner() === this; } }; o.m()")
                .unwrap(),
            Value::Boolean(true)
        );
        // Arrows have no arguments object of their own.
        assert_eq!(
            run("(() => typeof arguments)()").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("undefined")))
        );
    }
}
