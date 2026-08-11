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
use crate::context::{ExecutionContext, PrivateEnvironment};
use crate::env::{EnvRef, new_declarative_environment, new_function_environment};
use crate::eval::eval_statement_list;
use crate::expr::eval_expr;
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

/// [[ConstructorKind]] (spec 10.2.1): whether a constructor must call
/// `super()` to create its `this`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstructorKind {
    Base,
    Derived,
}

/// A ClassFieldDefinition Record (spec 15.7.14): a field's property key plus
/// the initializer expression, evaluated per instance in the class scope.
#[derive(Debug, Clone)]
pub struct ClassField {
    pub name: crux::property::PropertyKey,
    /// Set for private fields: the Private Name's id instead of `name`.
    pub private_name: Option<u64>,
    pub init: Option<syntax::ast::Expr>,
    pub environment: EnvRef,
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
    /// [[HomeObject]]: set for methods (super property access).
    pub home_object: Option<Value>,
    /// [[ConstructorKind]]: ~derived~ for classes with a heritage.
    pub constructor_kind: ConstructorKind,
    /// [[IsClassConstructor]]: class constructors reject bare calls.
    pub is_class_constructor: bool,
    /// [[Fields]]: instance fields initialized when the constructor runs.
    pub fields: Vec<ClassField>,
    /// [[PrivateMethods]]: instance private methods/accessors added to each
    /// instance by InitializeInstanceElements.
    pub private_methods: Vec<crux::object::PrivateElement>,
    /// [[PrivateEnvironment]]: the class's private names, captured so method
    /// bodies and field initializers can resolve `#name`.
    pub private_environment: Option<Handle<PrivateEnvironment>>,
    /// The heritage constructor of a derived class (GetSuperConstructor).
    pub super_constructor: Option<Value>,
    /// The synthesized `constructor(...args) { super(...args); }` — its
    /// [[Construct]] passes the arguments without the iterator protocol
    /// (spec 15.7.14 step 23 note).
    pub default_derived: bool,
    pub realm: Handle<Realm>,
    pub is_async: bool,
    pub is_generator: bool,
    /// The exact source text of the definition (Function.prototype.toString,
    /// spec 20.2.3.5); `None` for synthesized/native callables.
    pub source: Option<JsString>,
    /// The compiled resumable body (PLAN §4.5) for generator/async
    /// functions; ordinary functions evaluate the AST directly.
    pub ir: Option<crate::ir::CompiledBody>,
}

/// FunctionBodyContainsUseStrict (spec 15.2.1): a `"use strict"` directive in
/// the function body's directive prologue.
pub fn function_is_strict(f: &syntax::ast::Function) -> bool {
    body_is_strict(&f.body)
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

/// IsAnonymousFunctionDefinition (spec 14.1.9): a FunctionExpression without
/// a name, an ArrowFunction, or a ClassExpression without a name. Such
/// definitions receive their name from the surrounding binding or property
/// via SetFunctionName.
pub fn is_anonymous_function_definition(expr: &syntax::ast::Expr) -> bool {
    match &expr.kind {
        ExprKind::Function(f) => f.name.is_none(),
        ExprKind::Arrow { .. } => true,
        ExprKind::Class(c) => c.name.is_none(),
        _ => false,
    }
}

/// SetFunctionName (spec 10.2.7): redefine the `name` own data property of
/// a function value. The property is configurable, so this also replaces the
/// empty name anonymous functions are created with. `prefix` ("get"/"set") is
/// joined with a space per the spec's step 3.
pub fn set_function_name(
    function: &Value,
    name: &JsString,
    prefix: Option<&str>,
) -> Result<(), JsError> {
    let Value::Function(function) = function else {
        return Ok(());
    };
    let name = match prefix {
        Some(prefix) => JsString::from_utf8(&format!("{prefix} {}", name.to_string_lossy())),
        None => name.clone(),
    };
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
/// The definition kind for OrdinaryFunctionCreate (spec 10.2.1.3): whether
/// the function is a method or accessor (no `prototype` own property), the
/// async/generator flags, and whether it is a class constructor.
#[derive(Debug, Clone, Copy)]
pub struct DefinitionKind {
    pub is_method: bool,
    pub is_async: bool,
    pub is_generator: bool,
    pub is_class_constructor: bool,
}

impl DefinitionKind {
    fn function(is_async: bool, is_generator: bool) -> Self {
        Self {
            is_method: false,
            is_async,
            is_generator,
            is_class_constructor: false,
        }
    }

    fn method(is_async: bool, is_generator: bool) -> Self {
        Self {
            is_method: true,
            is_async,
            is_generator,
            is_class_constructor: false,
        }
    }
}

pub fn instantiate_function(
    agent: &mut Agent,
    f: &syntax::ast::Function,
    environment: EnvRef,
    enclosing_strict: bool,
) -> Result<Value, JsError> {
    instantiate_function_with_source(agent, f, environment, enclosing_strict, None)
}

/// Like `instantiate_function`, with an explicit source text (module
/// declarations instantiate before the module context is pushed, so the
/// running context cannot provide it).
pub fn instantiate_function_with_source(
    agent: &mut Agent,
    f: &syntax::ast::Function,
    environment: EnvRef,
    enclosing_strict: bool,
    source: Option<JsString>,
) -> Result<Value, JsError> {
    let source = source.or_else(|| capture_source(agent, f.span));
    register_function(
        agent,
        f.name.map(crux::lookup),
        f.params.clone(),
        f.body.clone(),
        environment,
        enclosing_strict,
        DefinitionKind::function(f.is_async, f.is_generator),
        source,
    )
}

/// Instantiate a method definition (object literal `m() {}` and class
/// methods): an ordinary function with no `prototype` own property and no
/// name until SetFunctionName; [[HomeObject]] is attached by `make_method`.
pub fn instantiate_method(
    agent: &mut Agent,
    f: &syntax::ast::Function,
    environment: EnvRef,
    enclosing_strict: bool,
) -> Result<Value, JsError> {
    let source = capture_source(agent, f.span);
    register_function(
        agent,
        None,
        f.params.clone(),
        f.body.clone(),
        environment,
        enclosing_strict,
        DefinitionKind::method(f.is_async, f.is_generator),
        source,
    )
}

/// The accessor form of OrdinaryFunctionCreate (spec 15.4.3 getters and
/// setters): an ordinary function with no `prototype` and no name until
/// SetFunctionName.
pub fn instantiate_accessor(
    agent: &mut Agent,
    params: Vec<BindingElement>,
    body: Block,
    environment: EnvRef,
    enclosing_strict: bool,
) -> Result<Value, JsError> {
    register_function(
        agent,
        None,
        params,
        body,
        environment,
        enclosing_strict,
        DefinitionKind::method(false, false),
        None,
    )
}

/// The class constructor (ClassDefinitionEvaluation steps 8-9): an ordinary
/// function with no `prototype` until MakeConstructor, [[IsClassConstructor]]
/// true (bare calls throw), and the class name as its eventual name.
pub fn instantiate_class_constructor(
    agent: &mut Agent,
    params: Vec<BindingElement>,
    body: Block,
    environment: EnvRef,
    enclosing_strict: bool,
) -> Result<Value, JsError> {
    instantiate_class_constructor_with(agent, params, body, environment, enclosing_strict, false)
}

/// Like `instantiate_class_constructor`, marking the synthesized default
/// derived constructor whose [[Construct]] bypasses the iterator protocol.
pub fn instantiate_default_derived_constructor(
    agent: &mut Agent,
    environment: EnvRef,
    enclosing_strict: bool,
) -> Result<Value, JsError> {
    instantiate_class_constructor_with(
        agent,
        Vec::new(),
        Block {
            stmts: Vec::new(),
            span: crux::Span::new(0, 0),
        },
        environment,
        enclosing_strict,
        true,
    )
}

fn instantiate_class_constructor_with(
    agent: &mut Agent,
    params: Vec<BindingElement>,
    body: Block,
    environment: EnvRef,
    enclosing_strict: bool,
    default_derived: bool,
) -> Result<Value, JsError> {
    let function = register_function(
        agent,
        None,
        params,
        body,
        environment,
        enclosing_strict,
        DefinitionKind {
            is_method: true,
            is_async: false,
            is_generator: false,
            is_class_constructor: true,
        },
        None,
    )?;
    if default_derived
        && let Value::Function(function) = &function
        && let Some(data) = agent.ecma_functions.get_mut(&function.id())
    {
        data.default_derived = true;
    }
    Ok(function)
}

/// MakeMethod (spec 10.2.12): attach the [[HomeObject]] slot that `super`
/// property access resolves through.
pub fn make_method(agent: &mut Agent, function: &Value, home_object: Value) -> Result<(), JsError> {
    let Value::Function(function) = function else {
        return Ok(());
    };
    let Some(data) = agent.ecma_functions.get_mut(&function.id()) else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot set a HomeObject on a non-ECMAScript function".into(),
        ));
    };
    data.home_object = Some(home_object);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn register_function(
    agent: &mut Agent,
    name: Option<JsString>,
    params: Vec<BindingElement>,
    body: Block,
    environment: EnvRef,
    enclosing_strict: bool,
    kind: DefinitionKind,
    source: Option<JsString>,
) -> Result<Value, JsError> {
    let strict = body_is_strict(&body) || enclosing_strict;
    let this_mode = if strict {
        ThisMode::Strict
    } else {
        ThisMode::Sloppy
    };
    let realm = agent.current_realm()?;
    let mut data = EcmaFunction {
        name: name.clone(),
        params: params.clone(),
        body,
        environment,
        this_mode,
        strict,
        home_object: None,
        constructor_kind: ConstructorKind::Base,
        is_class_constructor: kind.is_class_constructor,
        fields: Vec::new(),
        private_methods: Vec::new(),
        private_environment: None,
        super_constructor: None,
        default_derived: false,
        realm,
        is_async: kind.is_async,
        is_generator: kind.is_generator,
        source,
        ir: None,
    };
    if kind.is_generator || kind.is_async {
        // Resumable bodies compile to the step IR; ordinary functions keep
        // the tree-walking evaluator.
        data.ir = Some(crate::ir::compile_body(&data)?);
    }
    let function = Function::new(name.clone());
    agent.ecma_functions.insert(function.id(), data);
    set_function_properties(&function, &params, name.as_ref())?;
    if !kind.is_method {
        make_constructor(&function)?;
    }
    set_function_prototype(agent, &function)?;
    Ok(Value::Function(function))
}

/// The exact source slice of a definition (Function.prototype.toString),
/// cut from the running context's source text using the definition's span.
/// Returns `None` when no source is tracked (synthesized/native callables).
fn capture_source(agent: &Agent, span: crux::Span) -> Option<JsString> {
    let source = agent.running_context().ok()?.source.clone()?;
    let (start, end) = (span.start as usize, span.end as usize);
    if start >= end || end > source.len() {
        return None;
    }
    Some(JsString::from_utf16(&source.as_slice()[start..end]))
}

/// OrdinaryFunctionCreate step: `F.[[Prototype]]` is %Function.prototype%
/// once the Function builtins install it.
fn set_function_prototype(agent: &Agent, function: &Handle<Function>) -> Result<(), JsError> {
    let proto = agent
        .current_realm()?
        .intrinsics
        .get("%Function.prototype%")
        .and_then(|value| crate::context::as_object(&value));
    function.object.set_prototype_of(proto)?;
    Ok(())
}

/// FunctionBodyContainsUseStrict for a body block (spec 15.2.1).
fn body_is_strict(body: &Block) -> bool {
    for stmt in &body.stmts {
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

/// CreateDynamicFunction's OrdinaryFunctionCreate (spec 20.2.1.1 step 41): an
/// ordinary sloppy function whose [[Prototype]] comes from
/// GetPrototypeFromConstructor and whose environment is the global one.
/// `parser::parse_function` already named it `anonymous` and checked the
/// early errors.
pub fn instantiate_dynamic_function(
    agent: &mut Agent,
    f: &syntax::ast::Function,
    environment: EnvRef,
    proto: Handle<JsObject>,
) -> Result<Value, JsError> {
    let value = register_function(
        agent,
        f.name.map(crux::lookup),
        f.params.clone(),
        f.body.clone(),
        environment,
        false,
        DefinitionKind::function(false, false),
        None,
    )?;
    // GetPrototypeFromConstructor wins over the default %Function.prototype%.
    let Value::Function(function) = &value else {
        unreachable!("register_function returns a function");
    };
    function.object.set_prototype_of(Some(proto))?;
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
    let mut data = EcmaFunction {
        name: None,
        params,
        body,
        environment,
        this_mode: ThisMode::Lexical,
        strict: enclosing_strict,
        home_object: None,
        constructor_kind: ConstructorKind::Base,
        is_class_constructor: false,
        fields: Vec::new(),
        private_methods: Vec::new(),
        private_environment: None,
        super_constructor: None,
        default_derived: false,
        realm,
        is_async,
        is_generator: false,
        source: None,
        ir: None,
    };
    if is_async {
        data.ir = Some(crate::ir::compile_body(&data)?);
    }
    let function = Function::new(None);
    let params = data.params.clone();
    agent.ecma_functions.insert(function.id(), data);
    set_function_properties(&function, &params, None)?;
    set_function_prototype(agent, &function)?;
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
            crux::function::FunctionKind::EcmaScript => {
                if agent
                    .ecma_functions
                    .get(&function.id())
                    .is_some_and(|data| data.is_class_constructor)
                {
                    // spec 10.2.1: [[IsClassConstructor]] is true, so the
                    // function must be called with `new` (step 5).
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Class constructor cannot be invoked without 'new'".into(),
                    ));
                }
                let data = agent.ecma_functions.get(&function.id());
                if data.is_some_and(|data| data.is_async) {
                    return crate::async_await::call_async_function(agent, function, this, args);
                }
                if data.is_some_and(|data| data.is_generator) {
                    return crate::generator::call_generator(agent, function, this, args);
                }
                ordinary_call(agent, function, this, args)
            }
            crux::function::FunctionKind::Bound {
                target,
                bound_this,
                bound_args,
            } => {
                let mut all = bound_args.clone();
                all.extend_from_slice(args);
                call(agent, target, bound_this.clone(), &all)
            }
            _ => {
                // Agent-dependent built-ins (the Function constructor and the
                // %Function.prototype% methods) cannot run inside the crux
                // closures; dispatch them here by intrinsic identity (the
                // %eval% pattern).
                if let Some(result) =
                    crate::builtins::function::dispatch_call(agent, callee, &this, args)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::object::dispatch_call(agent, callee, &this, args)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::boolean::dispatch_call(agent, callee, &this, args)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::bigint::dispatch_call(agent, callee, &this, args)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::date::dispatch_call(agent, callee, &this, args)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::symbol::dispatch_call(agent, callee, &this, args)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::error::dispatch_call(agent, callee, &this, args)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::math::dispatch_call(agent, callee, &this, args)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::number::dispatch_call(agent, callee, &this, args)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::weakref::dispatch_call(agent, callee, &this, args)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::promise::dispatch_call(agent, callee, &this, args)
                {
                    return result;
                }
                if let Some(result) = crate::async_await::dispatch_resume(agent, callee, args) {
                    return result;
                }
                if let Some(result) =
                    crate::async_await::dispatch_async_from_sync(agent, callee, args)
                {
                    return result;
                }
                if let Some(result) = crate::generator::dispatch_call(agent, callee, &this, args) {
                    return result;
                }
                if let Some(result) = crate::module::dispatch_import_resolver(agent, callee, args) {
                    return result;
                }
                crux::function::call(callee, this, args)
            }
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
            _ => {
                if let Some(result) =
                    crate::builtins::function::dispatch_construct(agent, callee, args, new_target)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::object::dispatch_construct(agent, callee, args, new_target)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::boolean::dispatch_construct(agent, callee, args, new_target)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::bigint::dispatch_construct(agent, callee, args, new_target)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::date::dispatch_construct(agent, callee, args, new_target)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::symbol::dispatch_construct(agent, callee, args, new_target)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::error::dispatch_construct(agent, callee, args, new_target)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::weakref::dispatch_construct(agent, callee, args, new_target)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::number::dispatch_construct(agent, callee, args, new_target)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::promise::dispatch_construct(agent, callee, args, new_target)
                {
                    return result;
                }
                crux::function::construct(callee, args, new_target)
            }
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
        private_environment: data.private_environment.clone(),
        source: agent
            .running_context()
            .ok()
            .and_then(|context| context.source.clone()),
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
        Completion::Throw(value) => {
            Err(JsError::new(ErrorKind::TypeError, format!("Uncaught {value:?}")).with_value(value))
        }
        Completion::Break { .. } | Completion::Continue { .. } => Err(JsError::new(
            ErrorKind::SyntaxError,
            "Illegal break/continue statement".into(),
        )),
    }
}

/// The `[[Construct]]` of an ordinary function (spec 10.2.1): create `this`
/// from the constructor's prototype, bind it, and run the body. Base class
/// constructors initialize instance fields before the body; derived
/// constructors leave `this` uninitialized until `super()` binds it.
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
    let derived = data.constructor_kind == ConstructorKind::Derived;
    // Base constructors create `this` up front (spec 10.2.1 steps 2-4);
    // derived constructors receive it from `super()`.
    let this = if derived {
        Value::Undefined
    } else {
        // OrdinaryCreateFromConstructor (spec 10.2.4): newTarget's
        // `prototype`, or a null-prototype object until %Object.prototype%.
        let prototype = crate::context::get_property(
            agent,
            new_target,
            &JsString::from_utf8("prototype"),
            new_target.clone(),
        )?;
        let proto = match prototype {
            Value::Object(obj) => Some(obj),
            _ => None,
        };
        Value::Object(JsObject::ordinary_object_create(proto))
    };
    let function_value = function.self_value();
    let old_env = data.environment.clone();
    let function_env = new_function_environment(
        Some(old_env),
        function_value.clone(),
        new_target.clone(),
        false,
    );
    agent.execution_context_stack.push(ExecutionContext {
        function: Some(function_value.clone()),
        realm: data.realm.clone(),
        script_or_module: None,
        lexical_environment: function_env.clone(),
        variable_environment: function_env.clone(),
        private_environment: data.private_environment.clone(),
        source: agent
            .running_context()
            .ok()
            .and_then(|context| context.source.clone()),
    });
    let result = (|| -> Result<Value, JsError> {
        if data.default_derived {
            // spec 15.7.14 step 23 (derived branch): Construct the superclass
            // with the original arguments and the current newTarget — the
            // arguments are passed directly, without the iterator protocol.
            let Some(super_ctor) = data.super_constructor.clone() else {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "A class extending null cannot be constructed".into(),
                ));
            };
            let this = crate::function::construct(agent, &super_ctor, args, new_target)?;
            function_env.bind_this_value(this.clone())?;
            initialize_instance_elements(agent, &this, &function_value)?;
            return Ok(this);
        }
        if derived {
            // `this` stays uninitialized: accessing it before `super()` is a
            // ReferenceError (the FunctionEnv's uninitialized status).
        } else {
            function_env.bind_this_value(this.clone())?;
            // spec 10.2.1 steps 8-9: instance fields initialize before the
            // constructor body runs.
            initialize_instance_elements(agent, &this, &function_value)?;
        }
        function_declaration_instantiation(agent, &function_value, &data, args, &function_env)?;
        let completed = eval_statement_list(agent, &data.body.stmts, data.strict)?;
        // spec 10.2.1 [[Construct]] steps 15-21: an object return wins; a base
        // constructor falls back to `this`; a derived constructor returns the
        // `super()`-bound `this` (or throws on any other value).
        match completed {
            Completion::Return(value) => match value {
                Value::Object(_) | Value::Function(_) => Ok(value),
                _ if derived => {
                    if matches!(value, Value::Undefined) {
                        function_env.get_this_binding()
                    } else {
                        Err(JsError::new(
                            ErrorKind::TypeError,
                            "Derived constructors may only return object or undefined".into(),
                        ))
                    }
                }
                _ => Ok(this),
            },
            Completion::Normal(_) | Completion::Empty => {
                if derived {
                    function_env.get_this_binding()
                } else {
                    Ok(this)
                }
            }
            Completion::Throw(value) => Err(JsError::new(
                ErrorKind::TypeError,
                format!("Uncaught {value:?}"),
            )
            .with_value(value)),
            Completion::Break { .. } | Completion::Continue { .. } => Err(JsError::new(
                ErrorKind::SyntaxError,
                "Illegal break/continue statement".into(),
            )),
        }
    })();
    agent.execution_context_stack.pop();
    result
}

/// InitializeInstanceElements (spec 7.3.27): define `ctor`'s instance
/// private methods and fields on `obj` in order, evaluating each initializer
/// with `this` = obj.
pub fn initialize_instance_elements(
    agent: &mut Agent,
    obj: &Value,
    ctor: &Value,
) -> Result<(), JsError> {
    let Value::Function(ctor) = ctor else {
        return Ok(());
    };
    let data = agent.ecma_functions.get(&ctor.id()).cloned();
    let Some(data) = data else {
        return Ok(());
    };
    let Value::Object(obj) = obj else {
        return Ok(());
    };
    // Private methods/accessors first (spec 7.3.27 step 1).
    for method in &data.private_methods {
        obj.private_element_add(method.clone())?;
    }
    // Fields (spec 7.3.27 step 2): private fields via PrivateFieldAdd.
    for field in &data.fields {
        // DefineField (spec 7.3.23): the initializer runs in the class scope
        // with the current `this` (the running context already chains to the
        // class environment through the constructor's [[Environment]]).
        let value = match &field.init {
            Some(init) => eval_expr(agent, init, true)?,
            None => Value::Undefined,
        };
        if let Some(name_id) = field.private_name {
            obj.private_element_add(crux::object::PrivateElement {
                name_id,
                kind: crux::object::PrivateElementKind::Field(value),
            })?;
        } else {
            obj.create_data_property_or_throw_key(&field.name, value)?;
        }
    }
    Ok(())
}

/// FunctionDeclarationInstantiation (spec 16.1.8): bind the parameters,
/// create the `arguments` object, hoist var bindings (initialized to
/// *undefined*) and top-level function declarations, and set up the lexical
/// environment for the body. Non-simple parameter lists (defaults, rest,
/// destructuring) bind in a separate parameter environment via
/// IteratorBindingInitialization.
/// FunctionDeclarationInstantiation (spec 16.1.8): bind formals, arguments,
/// vars, and functions into the function environment. Shared with the
/// async-function machinery.
pub(crate) fn function_declaration_instantiation(
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

    // The environment split (spec 16.1.8 steps 38-43): a non-simple
    // parameter list binds in its own environment, and the body's vars live
    // in a further environment so defaults cannot see them.
    let (param_env, variable_env) = if simple {
        (function_env.clone(), function_env.clone())
    } else {
        let param_env = new_declarative_environment(Some(function_env.clone()));
        let variable_env = new_declarative_environment(Some(param_env.clone()));
        (param_env, variable_env)
    };

    // Parameter bindings are created up front (uninitialized), so a default
    // initializer referencing an earlier parameter hits its TDZ correctly.
    for param in &data.params {
        let mut names = Vec::new();
        bound_names(&param.pattern, &mut names);
        for name in names {
            if !param_env.has_binding(&name)? {
                param_env.create_mutable_binding(&name, false)?;
            }
        }
    }

    // The arguments object (spec 16.1.8 steps 58-70) is created before the
    // formals bind, so a default initializer can reference `arguments`.
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
            param_env.create_immutable_binding(&JsString::from_utf8("arguments"), false)?;
        } else {
            param_env.create_mutable_binding(&JsString::from_utf8("arguments"), false)?;
        }
        param_env.initialize_binding(&JsString::from_utf8("arguments"), arguments_obj)?;
        param_bindings.push(JsString::from_utf8("arguments"));
    }

    // IteratorBindingInitialization of the formals (spec 16.1.8 step 79):
    // positional for simple lists, full binding for non-simple ones.
    if simple {
        for (index, param) in data.params.iter().enumerate() {
            let BindingPattern::Ident(name) = &param.pattern else {
                unreachable!("simple parameter lists are identifiers")
            };
            let name = crux::lookup(*name);
            let value = args.get(index).cloned().unwrap_or(Value::Undefined);
            param_env.initialize_binding(&name, value)?;
        }
    } else {
        agent.running_context_mut()?.lexical_environment = param_env.clone();
        crate::binding::iterator_binding_initialization(
            agent,
            &data.params,
            args,
            Some(&param_env),
            strict,
        )?;
        // spec 16.1.8 step 44: the VariableEnvironment switches to the body's
        // record only after the formals bind, so a direct eval inside a
        // default sees the callee's environment (closures created there
        // resolve eval-introduced vars through it).
        agent.running_context_mut()?.variable_environment = variable_env.clone();
    }

    // Var bindings: created and initialized to *undefined* during
    // instantiation (spec 16.1.8 steps 34-36 simple path). For non-simple
    // lists a var sharing a parameter name is a separate binding that starts
    // with the parameter's value (steps 44-51).
    let mut instantiated: Vec<JsString> = if simple {
        param_bindings.clone()
    } else {
        Vec::new()
    };
    for decl in top_level_var_scoped_declarations(&data.body.stmts) {
        let crate::script::VarScopedDecl::Variable(names) = decl else {
            continue;
        };
        for name in names {
            if instantiated.contains(&name) {
                continue;
            }
            instantiated.push(name.clone());
            variable_env.create_mutable_binding(&name, false)?;
            if !simple {
                let initial = if !param_bindings.contains(&name) || func_names.contains(&name) {
                    Value::Undefined
                } else {
                    param_env.get_binding_value(&name, false)?
                };
                variable_env.initialize_binding(&name, initial)?;
            } else {
                variable_env.initialize_binding(&name, Value::Undefined)?;
            }
        }
    }

    // The body's lexical environment: strict functions share the variable
    // env; sloppy functions get a fresh declarative record so direct eval
    // cannot see the var bindings (spec 16.1.8 steps 37-42).
    let lexical_env = if strict {
        variable_env.clone()
    } else {
        new_declarative_environment(Some(variable_env.clone()))
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
        variable_env.set_mutable_binding(&name, func_obj, false)?;
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
    use crate::agent::{Agent, evaluate};

    fn run(source: &str) -> Result<Value, JsError> {
        evaluate(source)
    }

    fn number(value: f64) -> Value {
        Value::Number(value)
    }

    fn object(props: &[(&str, f64)]) -> Value {
        let obj = crux::object::JsObject::ordinary_object_create(None);
        for (key, value) in props {
            obj.create_data_property(&JsString::from_utf8(key), Value::Number(*value))
                .unwrap();
        }
        Value::Object(obj)
    }

    /// A native stand-in for an array: an object whose `@@iterator` method
    /// yields the given values. The global `Array` builtin and its
    /// `@@iterator` arrive with the builtins phase, so array-destructuring
    /// tests iterate this until then.
    fn iterable(values: Vec<Value>) -> Value {
        let values_clone = values.clone();
        let index = std::cell::Cell::new(0usize);
        let next = crux::Function::create_builtin(
            Some(JsString::from_utf8("next")),
            0,
            Box::new(move |_, _| {
                let i = index.get();
                let result = crux::object::JsObject::ordinary_object_create(None);
                if i < values_clone.len() {
                    index.set(i + 1);
                    result.create_data_property(
                        &JsString::from_utf8("value"),
                        values_clone[i].clone(),
                    )?;
                    result.create_data_property(
                        &JsString::from_utf8("done"),
                        Value::Boolean(false),
                    )?;
                } else {
                    result.create_data_property(&JsString::from_utf8("value"), Value::Undefined)?;
                    result
                        .create_data_property(&JsString::from_utf8("done"), Value::Boolean(true))?;
                }
                Ok(Value::Object(result))
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
        Value::Object(iterable)
    }

    /// Run a script with a global `iter` holding a native iterable of the
    /// given values.
    fn run_with_iterable(source: &str, values: Vec<Value>) -> Result<Value, JsError> {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let global = agent.running_context().unwrap().realm.global_object.clone();
        global
            .create_data_property(&JsString::from_utf8("iter"), iterable(values))
            .unwrap();
        agent.run_script(source)
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
    fn for_heads_capture_per_iteration_bindings() {
        // Each iteration's closure sees its own `let` binding (spec
        // 14.7.4.3 CreatePerIterationEnvironment).
        assert_eq!(
            run("let fs = []; for (let i = 0; i < 3; i++) { fs[i] = function () { return i; }; } fs[0]() + fs[1]() * 10 + fs[2]() * 100")
                .unwrap(),
            number(210.0)
        );
        assert_eq!(
            run("let fs = []; for (let i = 0; i < 3; i++) { let f = function () { return i; }; fs[i] = f; } fs[1]()")
                .unwrap(),
            number(1.0)
        );
        // `var` heads share a single binding: all closures see the final value.
        assert_eq!(
            run("let fs = []; for (var i = 0; i < 3; i++) { fs[i] = function () { return i; }; } fs[0]() + fs[2]()")
                .unwrap(),
            number(6.0)
        );
        // `const` heads get fresh copies per iteration too.
        assert_eq!(
            run_with_iterable(
                "let fs = []; let k = 0; for (const v of iter) { fs[k] = () => v; k++; } fs[0]() + fs[1]()",
                vec![number(1.0), number(2.0)]
            )
            .unwrap(),
            number(3.0)
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

    #[test]
    fn default_parameters_apply_when_undefined() {
        assert_eq!(
            run("function f(a = 10) { return a; } f()").unwrap(),
            number(10.0)
        );
        assert_eq!(
            run("function f(a = 10) { return a; } f(undefined)").unwrap(),
            number(10.0)
        );
        assert_eq!(
            run("function f(a = 10) { return a; } f(5)").unwrap(),
            number(5.0)
        );
        assert_eq!(
            run("function f(a = 10) { return a; } f(null)").unwrap(),
            Value::Null
        );
    }

    #[test]
    fn default_parameters_reference_earlier_parameters() {
        // A default can read earlier parameters and previous bindings.
        assert_eq!(
            run("function f(a, b = a * 2) { return b; } f(3)").unwrap(),
            number(6.0)
        );
        assert_eq!(
            run("function f(a, b = a, c = b) { return c; } f(4)").unwrap(),
            number(4.0)
        );
        // A default cannot see a later parameter (TDZ: uninitialized).
        assert!(run("function f(a = b, b) { return a; } f(undefined, 2)").is_err());
        // Defaults cannot see the body's var bindings.
        assert!(run("function f(a = x) { var x = 1; return a; } f()").is_err());
    }

    #[test]
    fn rest_parameters_collect_remaining_arguments() {
        assert_eq!(
            run("function f(a, ...rest) { return rest.length + ':' + rest[0] + rest[1]; } f(1, 2, 3)")
                .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("2:23")))
        );
        assert_eq!(
            run("function f(...r) { return r.length; } f()").unwrap(),
            number(0.0)
        );
        assert_eq!(
            run("function f(...r) { return r.length; } f(1, 2)").unwrap(),
            number(2.0)
        );
        // length counts only the leading simple parameters.
        assert_eq!(
            run("function f(a, b = 1, ...r) {} f.length").unwrap(),
            number(1.0)
        );
    }

    #[test]
    fn destructured_parameters_bind_from_objects_and_arrays() {
        assert_eq!(
            run("function f({ x, y }) { return x + y; } f({ x: 1, y: 2 })").unwrap(),
            number(3.0)
        );
        // Array patterns consume any iterable; `Array.prototype[@@iterator]`
        // joins the builtins phase, so a native iterable stands in.
        assert_eq!(
            run_with_iterable(
                "function f([a, b]) { return a - b; } f(iter)",
                vec![number(5.0), number(2.0)]
            )
            .unwrap(),
            number(3.0)
        );
        assert_eq!(
            run_with_iterable(
                "function f({ x = 10 }, [a = 1]) { return x + a; } f({}, iter)",
                vec![]
            )
            .unwrap(),
            number(11.0)
        );
        // A default can destructure and reference earlier parameters.
        assert_eq!(
            run("function f(n, { x = n } = {}) { return x; } f(7)").unwrap(),
            number(7.0)
        );
        // Destructuring null/undefined throws a TypeError.
        assert!(run("function f({ x }) { return x; } f(null)").is_err());
    }

    #[test]
    fn non_simple_parameter_lists_get_unmapped_arguments() {
        // With defaults/rest/destructuring the arguments object is unmapped.
        assert_eq!(
            run("function f(a, b = 2) { arguments[0] = 99; return a; } f(1)").unwrap(),
            number(1.0)
        );
        assert_eq!(
            run("function f(...r) { arguments[0] = 99; return arguments[0]; } f(1)").unwrap(),
            number(99.0)
        );
        assert_eq!(
            run("function f({ x }) { return arguments[0].x; } f({ x: 5 })").unwrap(),
            number(5.0)
        );
    }

    #[test]
    fn arguments_name_conflict_edge_cases() {
        // A param named `arguments` suppresses the arguments object entirely.
        assert_eq!(
            run("function f(arguments) { return arguments; } f(5)").unwrap(),
            number(5.0)
        );
        // `var arguments` shares the simple-path arguments binding: the
        // initializer overwrites it (spec 16.1.8 steps 44-51).
        assert_eq!(
            run("function f() { var arguments = 3; return arguments; } f()").unwrap(),
            number(3.0)
        );
        // A top-level `function arguments` suppresses the object (simple path).
        assert_eq!(
            run("function f() { function arguments() { return 1; } return arguments(); } f()")
                .unwrap(),
            number(1.0)
        );
        // Non-simple lists always get the arguments object, even with a
        // function/var named `arguments` in the body.
        assert_eq!(
            run("function f(a = 1) { function arguments() {} return typeof arguments; } f()")
                .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("function")))
        );
        // A default can reference `arguments` (created before formals bind,
        // spec 16.1.8 steps 58-79).
        assert_eq!(
            run("function f(x = arguments.length) { return x; } f()").unwrap(),
            number(0.0)
        );
    }

    #[test]
    fn object_literal_methods_and_accessors() {
        // Method shorthand binds `this` to the receiver (spec 15.4.3).
        assert_eq!(
            run("let o = { x: 5, m() { return this.x; } }; o.m()").unwrap(),
            number(5.0)
        );
        // Method names are inferred from the property key.
        assert_eq!(
            run("let o = { m() {} }; o.m.name").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("m")))
        );
        // Methods are not constructors: no own `prototype` property.
        assert_eq!(
            run("let o = { m() {} }; typeof o.m.prototype").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("undefined")))
        );
        // Getters and setters share one accessor property.
        assert_eq!(
            run("let o = { _v: 1, get v() { return this._v; }, set v(x) { this._v = x; } }; o.v")
                .unwrap(),
            number(1.0)
        );
        assert_eq!(
            run("let o = { _v: 1, get v() { return this._v; }, set v(x) { this._v = x; } }; o.v = 9; o.v")
                .unwrap(),
            number(9.0)
        );
        // Getter-only accessors read; sets silently fail in sloppy mode.
        assert_eq!(
            run("let o = { get x() { return 3; } }; o.x").unwrap(),
            number(3.0)
        );
        assert_eq!(
            run("let o = { get x() { return 3; } }; o.x = 9; o.x").unwrap(),
            number(3.0)
        );
    }

    #[test]
    fn super_property_access_uses_home_object() {
        // `super.x` reads through the method's [[HomeObject]] prototype
        // (spec 9.2.4.5 + 13.3.6.2).
        assert_eq!(
            run("let proto = { x: 42 }; let o = { __proto__: proto, m() { return super.x; } }; o.m()")
                .unwrap(),
            number(42.0)
        );
        // A method call through `super` keeps the current `this`.
        assert_eq!(
            run("let proto = { m() { return this.v; } }; let o = { __proto__: proto, v: 7, n() { return super.m(); } }; o.n()")
                .unwrap(),
            number(7.0)
        );
        // Computed super keys and nested super through the prototype chain.
        assert_eq!(
            run("let base = { a: 1 }; let mid = { __proto__: base, b: 2, m() { return super.a; } }; let o = { __proto__: mid, m() { return super.m(); } }; o.m()")
                .unwrap(),
            number(1.0)
        );
        // Arrows inside a method share the enclosing HomeObject.
        assert_eq!(
            run("let proto = { x: 11 }; let o = { __proto__: proto, m() { let f = () => super.x; return f(); } }; o.m()")
                .unwrap(),
            number(11.0)
        );
        // `super` outside a method is a syntax error.
        assert!(run("super.x").is_err());
        assert!(run("function f() { super.x; } f()").is_err());
    }

    #[test]
    fn class_declarations_and_constructors() {
        // A class constructor is invoked with `new` (spec 10.2.1).
        assert_eq!(
            run("class C { constructor(x) { this.x = x; } } new C(5).x").unwrap(),
            number(5.0)
        );
        // The default constructor creates an instance with the class
        // prototype; `constructor` points back at the class.
        assert_eq!(
            run("class C {} new C().constructor === C").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("class C {} typeof C.prototype.constructor").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("function")))
        );
        // Class constructors cannot be called without `new`.
        assert!(run("class C {} C()").is_err());
        // The class name and method names are inferred.
        assert_eq!(
            run("class C {} C.name").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("C")))
        );
        assert_eq!(
            run("let D = class {}; D.name").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("D")))
        );
        assert_eq!(
            run("class C { m() {} } C.prototype.m.name").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("m")))
        );
        // `new.target` is the active constructor.
        assert_eq!(
            run("class C { constructor() { this.t = new.target; } } new C().t === C").unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn class_methods_accessors_and_fields() {
        // Instance methods bind `this` to the receiver.
        assert_eq!(
            run("class C { constructor() { this.v = 3; } m() { return this.v; } } new C().m()")
                .unwrap(),
            number(3.0)
        );
        // Instance fields initialize before the constructor body (spec
        // 10.2.1 steps 8-9), in order.
        assert_eq!(
            run("class C { x = 5; y = this.x + 1; } new C().y").unwrap(),
            number(6.0)
        );
        // Static methods, fields, and blocks target the constructor.
        assert_eq!(
            run("class C { static s() { return 3; } } C.s()").unwrap(),
            number(3.0)
        );
        assert_eq!(run("class C { static s = 42; } C.s").unwrap(), number(42.0));
        assert_eq!(
            run("class C { static { this.v = 7; } } C.v").unwrap(),
            number(7.0)
        );
        // Getters/setters merge into one accessor on the prototype.
        assert_eq!(
            run("class A { get v() { return this._v; } set v(x) { this._v = x; } } let a = new A(); a.v = 9; a.v")
                .unwrap(),
            number(9.0)
        );
        // Class expressions, including named ones visible only inside.
        assert_eq!(
            run("let C = class { m() { return 5; } }; new C().m()").unwrap(),
            number(5.0)
        );
        assert_eq!(
            run("let C = class N { m() { return N; } }; new C().m() === C").unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn class_inheritance_super() {
        // The default derived constructor forwards arguments to `super`.
        assert_eq!(
            run("class A { constructor(x) { this.x = x; } } class B extends A {} new B(7).x")
                .unwrap(),
            number(7.0)
        );
        // `super()` binds `this` and initializes the derived fields.
        assert_eq!(
            run("class A { constructor() { this.y = 1; } } class B extends A { constructor() { super(); this.z = 2; } } new B().y + new B().z")
                .unwrap(),
            number(3.0)
        );
        // `this` before `super()` is a TDZ ReferenceError.
        assert!(
            run("class A {} class B extends A { constructor() { this.bad = 1; } } new B()")
                .is_err()
        );
        // `super.m()` resolves through the prototype chain, keeping `this`.
        assert_eq!(
            run("class A { m() { return 1; } } class B extends A { m() { return super.m() + 1; } } new B().m()")
                .unwrap(),
            number(2.0)
        );
        assert_eq!(
            run("class A { n() { return 10; } } class B extends A { n() { return super.n() * 2; } } class C extends B {} new C().n()")
                .unwrap(),
            number(20.0)
        );
        // `super.constructor` is the heritage constructor.
        assert_eq!(
            run("class A {} class B extends A { m() { return super.constructor === A; } } new B().m()")
                .unwrap(),
            Value::Boolean(true)
        );
        // A derived constructor returning an object wins; other values throw.
        assert_eq!(
            run("class A {} class B extends A { constructor() { super(); return { z: 9 }; } } new B().z")
                .unwrap(),
            number(9.0)
        );
        assert!(
            run("class A {} class B extends A { constructor() { super(); return 1; } } new B()")
                .is_err()
        );
        // `extends` requires a constructor or null.
        assert!(run("class A extends 42 {}").is_err());
    }

    #[test]
    fn class_private_fields_and_methods() {
        // Private fields read/write through `this.#x` (PrivateGet/PrivateSet).
        assert_eq!(
            run("class C { #x = 1; get() { return this.#x; } } new C().get()").unwrap(),
            number(1.0)
        );
        assert_eq!(
            run("class C { #x = 1; set(v) { this.#x = v; } get() { return this.#x; } } let c = new C(); c.set(9); c.get()")
                .unwrap(),
            number(9.0)
        );
        // Fields initialize in order: `#x = this.y` before `y = 2` reads
        // undefined (spec 15.7.14 [[Fields]] order).
        assert_eq!(
            run("class C { #x = this.y; y = 2; get() { return this.#x; } } new C().get()").unwrap(),
            Value::Undefined
        );
        // Private fields and methods are per-instance and independent.
        assert!(run("class C { #x = 5; } let a = new C(); let b = new C(); a.#x").is_err());
        // `#x in obj` is the brand check (spec 13.11.1).
        assert_eq!(
            run("class C { #x = 1; m(o) { return #x in o; } } let c = new C(); c.m(c)").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("class C { #x = 1; m(o) { return #x in o; } } let c = new C(); c.m({})").unwrap(),
            Value::Boolean(false)
        );
        // Private methods with brand checks, and private accessors.
        assert_eq!(
            run("class C { #m() { return 42; } call() { return this.#m(); } } new C().call()")
                .unwrap(),
            number(42.0)
        );
        assert_eq!(
            run("class C { get #v() { return this._v; } set #v(x) { this._v = x; } run() { this.#v = 5; return this.#v; } } new C().run()")
                .unwrap(),
            number(5.0)
        );
        // Static private fields resolve through the constructor.
        assert_eq!(
            run("class C { static #s = 3; static get() { return C.#s; } } C.get()").unwrap(),
            number(3.0)
        );
        // Private fields survive inheritance: the derived constructor's
        // `super()` initializes the base class's fields.
        assert_eq!(
            run("class A { #v = 7; get() { return this.#v; } } class B extends A {} new B().get()")
                .unwrap(),
            number(7.0)
        );
        // Accessing a private member outside the class is an error.
        assert!(run("class C { #x = 1; } let c = new C(); c.#x").is_err());
    }

    #[test]
    fn anonymous_functions_infer_names() {
        // var/let/const declarations name an anonymous function (spec 14.3.2,
        // 14.2.2: SetFunctionName from the binding identifier).
        assert_eq!(
            run("var f = function () {}; f.name").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("f")))
        );
        assert_eq!(
            run("let g = () => 0; g.name").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("g")))
        );
        assert_eq!(
            run("const h = function () {}; h.name").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("h")))
        );
        // Object properties name the function (spec 15.4.2 step 5).
        assert_eq!(
            run("let o = { m: function () {} }; o.m.name").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("m")))
        );
        assert_eq!(
            run("let o = { m: () => 0 }; o.m.name").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("m")))
        );
        // Plain assignment to an identifier (spec 13.15.2 step 1.e).
        assert_eq!(
            run("let t; t = function () {}; t.name").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("t")))
        );
        // A named function expression keeps its own name.
        assert_eq!(
            run("var q = function inner() {}; q.name").unwrap(),
            Value::String(Handle::new(JsString::from_utf8("inner")))
        );
    }

    #[test]
    fn destructuring_declarations() {
        assert_eq!(
            run("let { x, y } = { x: 1, y: 2 }; x + y").unwrap(),
            number(3.0)
        );
        assert_eq!(
            run_with_iterable("var [a, b] = iter; a - b", vec![number(1.0), number(2.0)]).unwrap(),
            number(-1.0)
        );
        assert_eq!(run("const { x = 10 } = {}; x").unwrap(), number(10.0));
        // Nested patterns, computed keys, and rest elements.
        assert_eq!(
            run("let { a: { b }, c } = { a: { b: 2 }, c: 3 }; b + c").unwrap(),
            number(5.0)
        );
        assert_eq!(
            run("let key = 'z'; let { [key]: v } = { z: 42 }; v").unwrap(),
            number(42.0)
        );
        assert_eq!(
            run_with_iterable(
                "let [head, ...tail] = iter; tail.length + head",
                vec![number(1.0), number(2.0), number(3.0)]
            )
            .unwrap(),
            number(3.0)
        );
        assert_eq!(
            run("let { a, ...rest } = { a: 1, b: 2, c: 3 }; rest.b + rest.c").unwrap(),
            number(5.0)
        );
        // Destructuring null/undefined throws.
        assert!(run("let { x } = null").is_err());
        assert!(run("var [x] = undefined").is_err());
    }

    #[test]
    fn destructuring_for_heads() {
        assert_eq!(
            run_with_iterable(
                "let r = ''; for (let [a, b] of iter) r += a + b; r",
                vec![
                    iterable(vec![number(1.0), number(2.0)]),
                    iterable(vec![number(3.0), number(4.0)]),
                ]
            )
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("37")))
        );
        assert_eq!(
            run_with_iterable(
                "let r = ''; for (const { x } of iter) r += x; r",
                vec![object(&[("x", 1.0)]), object(&[("x", 2.0)])]
            )
            .unwrap(),
            Value::String(Handle::new(JsString::from_utf8("12")))
        );
        assert_eq!(
            run_with_iterable(
                "var r = 0; for (var [a] of iter) r += a; r",
                vec![iterable(vec![number(1.0)]), iterable(vec![number(2.0)])]
            )
            .unwrap(),
            number(3.0)
        );
        assert_eq!(
            run_with_iterable(
                "var a = 0; for (let [x] = iter; x < 3; x++) { a = x; } a",
                vec![number(2.0)]
            )
            .unwrap(),
            number(2.0)
        );
    }
}
