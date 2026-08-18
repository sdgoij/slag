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
use crux::value::{Value, ValueKind};
use syntax::ast::{
    ArrowBody, BindingElement, BindingPattern, Block, Expr, ExprKind, Literal, Stmt, StmtKind,
};

use crate::agent::Agent;
use crate::context::{ExecutionContext, PrivateEnvironment};
use crate::env::{EnvRef, new_declarative_environment, new_function_environment};
use crate::eval::eval_statement_list;
use crate::expr::eval_expr;
use crate::flow::Completion;
use crate::ir::{Vm, VmOutcome};
use crate::realm::Realm;
use crate::script::{
    bound_names, is_constant_declaration, top_level_lexically_declared_names,
    top_level_lexically_scoped_declarations, top_level_var_scoped_declarations,
};

/// The crux-side hook that runs ECMAScript function bodies: re-enter the
/// agent's call/construct path with the recorded current agent.
fn crux_ecma_executor(
    agent: *mut (),
    callee: &Value,
    this: Value,
    args: &[Value],
    new_target: Option<&Value>,
) -> Result<Value, JsError> {
    // SAFETY: crux invokes this only while `crux::function::with_agent` has
    // recorded a live `&mut Agent` (see `call`/`construct` above).
    let agent = unsafe { &mut *(agent as *mut Agent) };
    match new_target {
        Some(new_target) => construct(agent, callee, args, new_target),
        None => call(agent, callee, this, args),
    }
}

static INSTALL_ECMA_HOOK: std::sync::Once = std::sync::Once::new();

/// Install the crux ECMAScript executor once per process; `Agent::new` calls
/// this so proxy traps and object coercion can run user-function bodies.
pub fn ensure_ecma_hook() {
    INSTALL_ECMA_HOOK.call_once(|| {
        crux::function::install_ecma_hook(crux_ecma_executor);
        // Proxy trap `argumentsList` arrays carry the current realm's
        // `%Array.prototype%` (CreateArrayFromList, spec 7.3.15).
        crux::proxy::install_array_from_list_hook(|agent, list| {
            // SAFETY: the hook only runs inside a `with_agent` window, where
            // the pointer is a live `&mut Agent`.
            let agent = unsafe { &mut *(agent as *mut crate::agent::Agent) };
            crate::builtins::array::array_from_values(agent, list)
        });
        crux::property::install_object_proto_hook(|agent| {
            // SAFETY: the hook only runs inside a `with_agent` window, where
            // the pointer is a live `&mut Agent`.
            let agent = unsafe { &mut *(agent as *mut crate::agent::Agent) };
            agent.current_realm().ok().and_then(|realm| {
                realm
                    .intrinsics
                    .get("%Object.prototype%")
                    .and_then(|value| crate::context::as_object(&value))
            })
        });
    });
}

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
    /// Shared so clones of the record (ordinary_call reads a copy) keep the
    /// same body AST nodes — the template-object cache keys sites by node
    /// identity within a parse.
    pub body: std::rc::Rc<Block>,
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
    /// A method or accessor definition (no [[Construct]], no `prototype` own
    /// property); class constructors set this too but are constructible.
    pub is_method: bool,
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
    /// [[ClassFieldInitializerName]] (spec 15.7.10 step 8): non-empty for
    /// functions created inside a class field initializer, so a direct eval
    /// in their bodies (or in arrows they create) applies the "Eval Inside
    /// Initializer" early errors (spec 19.2.1.1).
    pub class_field_initializer: bool,
    /// The exact source text of the definition (Function.prototype.toString,
    /// spec 20.2.3.5); `None` for synthesized/native callables.
    pub source: Option<JsString>,
    /// The module this function's source text appears in (import.meta resolves
    /// lexically to it, spec 13.3.7.1); `None` for script and builtin code.
    pub declaring_module: Option<Handle<crate::module::SourceTextModule>>,
    /// The compiled body (PLAN §4.5): every function body compiles to the
    /// step IR, and ordinary calls/constructs run it on the VM. Shared so
    /// the per-call record read does not copy the steps.
    pub ir: Option<std::rc::Rc<crate::ir::CompiledBody>>,
}

/// FunctionBodyContainsUseStrict (spec 15.2.1): a `"use strict"` directive in
/// the function body's directive prologue.
pub fn function_is_strict(agent: &Agent, f: &syntax::ast::Function) -> bool {
    body_is_strict(agent, &f.body, None)
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
        // spec 13.2.1: a parenthesized expression defers to its inner
        // expression, so `(function () {})` is an anonymous function
        // definition (but `(0, function () {})` is not).
        ExprKind::Paren(inner) => is_anonymous_function_definition(inner),
        _ => false,
    }
}

/// The name property of a `export default function/class`: the synthesized
/// `*default*` binding name is renamed to "default" (spec 15.2.3.11
/// InitializeBoundName renames via SetFunctionName). Other names pass
/// through unchanged.
pub(crate) fn default_binding_display_name(name: Option<JsString>) -> Option<JsString> {
    name.map(|text| {
        if text == JsString::from_utf8("*default*") {
            JsString::from_utf8("default")
        } else {
            text
        }
    })
}

/// SetFunctionName (spec 10.2.11): redefine the `name` own data property of
/// a function value. Fresh ECMAScript functions carry a `""` placeholder
/// name; a non-empty own `name` (a static `name` element on a class
/// constructor, which wins over the surrounding binding per
/// ClassDefinitionEvaluation) must not be overwritten. The own descriptor is
/// inspected rather than the value so a static accessor named `name` is never
/// executed. `prefix` ("get"/"set") is joined with a space per spec step 3.
pub fn set_function_name(
    function: &Value,
    name: &JsString,
    prefix: Option<&str>,
) -> Result<(), JsError> {
    let ValueKind::Function(function) = function.kind() else {
        return Ok(());
    };
    let own = function
        .object
        .get_own_property_key(&crux::property::PropertyKey::from_utf8("name"))?;
    // SetFunctionName only runs on functions without a real own `name`: a
    // freshly created function carries the "" placeholder, a bound function
    // carries no own `name` at all. A non-empty own `name` (a static `name`
    // element on a class constructor, which wins over the surrounding binding
    // per ClassDefinitionEvaluation) is left alone. The own descriptor is
    // inspected rather than the value so a static accessor named `name` is
    // never executed.
    let placeholder = match own {
        None => true,
        Some(crux::object::Property {
            kind: crux::object::PropertyKind::Data { value, .. },
            ..
        }) => value.as_string().is_some_and(|s| s.is_empty()),
        _ => false,
    };
    if !placeholder {
        return Ok(());
    }
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
/// `constructor` back-reference on the supplied prototype object. Arrows and
/// methods skip this; async functions never have a `prototype`.
fn make_constructor(
    function: &Handle<Function>,
    prototype: Handle<JsObject>,
    writable: bool,
    add_constructor_property: bool,
) -> Result<(), JsError> {
    // MakeConstructor (spec 10.2.5) only adds the `constructor` property when
    // it creates the prototype itself; a provided prototype (the generator
    // function's %Generator.prototype%-based object) has no own properties.
    if add_constructor_property {
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
    }
    function.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(Value::Object(prototype)),
            writable: Some(writable),
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
    let body = shared_function_body(agent, f, source.as_ref());
    register_function(
        agent,
        default_binding_display_name(f.name.map(crux::lookup)),
        f.params.clone(),
        body,
        environment,
        enclosing_strict,
        DefinitionKind::function(f.is_async, f.is_generator),
        source,
        None,
    )
}

/// Instantiate a function declaration or expression whose body AST is shared
/// across instantiations of the same parse node: two closures from the same
/// factory share the site nodes, so the template-object cache keys by node
/// identity hold (cache-different-functions-same-site.js). The shared body is
/// immutable, so reentrancy is safe. The key carries the node's span and the
/// hash of the node's source slice: raw node addresses are reused after a
/// parse is dropped, and distinct parses of same-length sources (e.g. two
/// modules whose functions sit at identical offsets) can otherwise collide on
/// (address, realm, span) alone.
pub fn shared_function_body(
    agent: &Agent,
    f: &syntax::ast::Function,
    source: Option<&JsString>,
) -> std::rc::Rc<Block> {
    let realm = agent
        .current_realm()
        .map(|realm| crux::handle::Handle::as_ptr(&realm) as usize)
        .unwrap_or(0);
    let source_key = source.map(source_hash).unwrap_or(0);
    let key = (
        f as *const syntax::ast::Function as usize,
        realm,
        f.span.start as usize,
        f.span.end as usize,
        source_key,
    );
    FUNCTION_BODY_CACHE.with(|cache| {
        if let Some(body) = cache.borrow().get(&key) {
            return body.clone();
        }
        let body = std::rc::Rc::new(f.body.clone());
        cache.borrow_mut().insert(key, body.clone());
        body
    })
}

/// A stable content hash of a function's source slice, used to disambiguate
/// cache keys across parses (see `shared_function_body`).
fn source_hash(source: &JsString) -> usize {
    let mut hash: usize = 0xcbf2_9ce4_8422_2325;
    for unit in source.as_slice() {
        hash ^= *unit as usize;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

type FunctionBodyKey = (usize, usize, usize, usize, usize);

type FunctionBodyCache = std::collections::HashMap<FunctionBodyKey, std::rc::Rc<Block>>;

thread_local! {
    static FUNCTION_BODY_CACHE: std::cell::RefCell<FunctionBodyCache> =
        std::cell::RefCell::new(std::collections::HashMap::new());
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
    let body = shared_function_body(agent, f, source.as_ref());
    register_function(
        agent,
        None,
        f.params.clone(),
        body,
        environment,
        enclosing_strict,
        DefinitionKind::method(f.is_async, f.is_generator),
        source,
        None,
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
        std::rc::Rc::new(body),
        environment,
        enclosing_strict,
        DefinitionKind::method(false, false),
        None,
        None,
    )
}

/// The class constructor (ClassDefinitionEvaluation steps 8-9): an ordinary
/// function with no `prototype` until MakeConstructor, [[IsClassConstructor]]
/// true (bare calls throw), and the class name as its eventual name.
pub fn instantiate_class_constructor(
    agent: &mut Agent,
    params: Vec<BindingElement>,
    body: std::rc::Rc<Block>,
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
        std::rc::Rc::new(Block {
            stmts: Vec::new(),
            span: crux::Span::new(0, 0),
        }),
        environment,
        enclosing_strict,
        true,
    )
}

fn instantiate_class_constructor_with(
    agent: &mut Agent,
    params: Vec<BindingElement>,
    body: std::rc::Rc<Block>,
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
        None,
    )?;
    if default_derived
        && let Some(function) = function.as_function()
        && let Some(data) = agent.ecma_functions.get_mut(&function.id())
    {
        data.default_derived = true;
    }
    Ok(function)
}

/// MakeMethod (spec 10.2.12): attach the [[HomeObject]] slot that `super`
/// property access resolves through.
pub fn make_method(agent: &mut Agent, function: &Value, home_object: Value) -> Result<(), JsError> {
    let ValueKind::Function(function) = function.kind() else {
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
    body: std::rc::Rc<Block>,
    environment: EnvRef,
    enclosing_strict: bool,
    kind: DefinitionKind,
    source: Option<JsString>,
    strict: Option<bool>,
) -> Result<Value, JsError> {
    // `strict` overrides the body-directive check (CreateDynamicFunction
    // computes it against its assembled source, which the running context
    // cannot provide); `enclosing_strict` still forces strictness.
    let strict = strict.unwrap_or_else(|| body_is_strict(agent, &body, None)) || enclosing_strict;
    let this_mode = if strict {
        ThisMode::Strict
    } else {
        ThisMode::Sloppy
    };
    let realm = agent.current_realm()?;
    let private_environment = agent.running_context()?.private_environment.clone();
    let declaring_module =
        agent
            .running_context()
            .ok()
            .and_then(|context| match &context.script_or_module {
                Some(crate::context::ScriptOrModule::Module(module)) => Some(module.clone()),
                _ => None,
            });
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
        is_method: kind.is_method,
        fields: Vec::new(),
        private_methods: Vec::new(),
        private_environment,
        super_constructor: None,
        default_derived: false,
        realm,
        is_async: kind.is_async,
        is_generator: kind.is_generator,
        class_field_initializer: false,
        source,
        declaring_module,
        ir: None,
    };
    // Every body compiles to the step IR; the VM executes ordinary bodies
    // the same way it runs the resumable kinds.
    data.ir = Some(std::rc::Rc::new(crate::ir::compile_body(&data)?));
    let function = Function::new(name.clone());
    agent.ecma_functions.insert(function.id(), data);
    set_function_properties(&function, &params, name.as_ref())?;
    // AddRestrictedFunctionProperties (spec 10.2.1): sloppy ordinary
    // functions carry own `caller`/`arguments` (value undefined when no
    // caller is known, non-writable, non-configurable). Strict, async, and
    // generator functions have none, so their reads/writes fall through to
    // the %Function.prototype% accessors. Methods never get them either
    // (spec 10.2.4: own caller/arguments are created only for ordinary
    // non-method functions).
    if !strict && !kind.is_method && !kind.is_async && !kind.is_generator {
        for name in ["caller", "arguments"] {
            function.define_property(
                &JsString::from_utf8(name),
                &PropertyDescriptor {
                    value: Some(Value::Undefined),
                    writable: Some(false),
                    get: None,
                    set: None,
                    enumerable: Some(false),
                    configurable: Some(false),
                },
            )?;
        }
    }
    // Plain async functions are never constructors and have no `prototype`;
    // generator and async-generator functions *and methods* get a `prototype`
    // that inherits %Generator.prototype% / %AsyncGenerator.prototype%,
    // writable per MakeConstructor's default (spec 15.4.5 GeneratorMethod).
    if kind.is_generator || (!kind.is_method && !kind.is_async) {
        let prototype = if kind.is_generator {
            let intrinsic = if kind.is_async {
                "%AsyncGenerator.prototype%"
            } else {
                "%Generator.prototype%"
            };
            let proto = agent
                .current_realm()?
                .intrinsics
                .get(intrinsic)
                .and_then(|value| crate::context::as_object(&value));
            JsObject::ordinary_object_create(proto)
        } else {
            // MakeConstructor (spec 10.2.5 step 2): the prototype is an
            // ordinary object with %Object.prototype% as its prototype.
            let proto = agent
                .current_realm()?
                .intrinsics
                .get("%Object.prototype%")
                .and_then(|value| crate::context::as_object(&value));
            JsObject::ordinary_object_create(proto)
        };
        make_constructor(&function, prototype, true, !kind.is_generator)?;
    }
    set_function_prototype(agent, &function)?;
    Ok(Value::Function(function))
}

/// The exact source slice of a definition (Function.prototype.toString),
/// cut from the running context's source text using the definition's span.
/// Returns `None` when no source is tracked (synthesized/native callables).
pub(crate) fn capture_source(agent: &Agent, span: crux::Span) -> Option<JsString> {
    let source = agent.running_context().ok()?.source.clone()?;
    let (start, end) = (span.start as usize, span.end as usize);
    if start >= end || end > source.len() {
        return None;
    }
    let slice = &source.as_slice()[start..end];
    Some(JsString::from_utf16(slice))
}

/// OrdinaryFunctionCreate step: `F.[[Prototype]]` is the intrinsic prototype
/// of the function's kind — %Function.prototype% for ordinary functions,
/// %GeneratorFunction.prototype% / %AsyncFunction.prototype% /
/// %AsyncGeneratorFunction.prototype% for the resumable kinds.
fn set_function_prototype(agent: &Agent, function: &Handle<Function>) -> Result<(), JsError> {
    let intrinsic = match agent.ecma_functions.get(&function.id()) {
        Some(data) if data.is_generator && data.is_async => "%AsyncGeneratorFunction.prototype%",
        Some(data) if data.is_generator => "%GeneratorFunction.prototype%",
        Some(data) if data.is_async => "%AsyncFunction.prototype%",
        _ => "%Function.prototype%",
    };
    let proto = agent
        .current_realm()?
        .intrinsics
        .get(intrinsic)
        .and_then(|value| crate::context::as_object(&value));
    function.object.set_prototype_of(proto)?;
    Ok(())
}

/// FunctionBodyContainsUseStrict for a body block (spec 15.2.1).
/// `span_source` is the source text the body's spans refer to, for bodies
/// parsed against their own text (CreateDynamicFunction); scripts use the
/// running context's source via capture_source.
fn body_is_strict(agent: &Agent, body: &Block, span_source: Option<&JsString>) -> bool {
    for stmt in &body.stmts {
        let StmtKind::Expr(expr) = &stmt.kind else {
            return false;
        };
        let ExprKind::Literal(Literal::Str(value)) = &expr.kind else {
            return false;
        };
        if directive_is_use_strict(agent, expr, value, span_source) {
            return true;
        }
    }
    false
}

/// Whether a directive-prologue literal is a genuine `"use strict"`
/// directive (spec 14.1.1): its raw source text between the quotes must be
/// exactly `use strict`. The cooked value alone cannot distinguish
/// `'use str\ict'` (line continuation) or `'use\u0020strict'` (escape), which
/// cook to the same value but are not directives. Synthesized bodies without
/// source text fall back to the cooked value.
fn directive_is_use_strict(
    agent: &Agent,
    expr: &Expr,
    cooked: &JsString,
    span_source: Option<&JsString>,
) -> bool {
    if cooked.to_string_lossy() != "use strict" {
        return false;
    }
    let Some(source) = span_source
        .cloned()
        .or_else(|| capture_source(agent, expr.span))
    else {
        return true;
    };
    let units = source.as_slice();
    let (start, end) = (expr.span.start as usize, expr.span.end as usize);
    // capture_source already returns the span slice; an explicit source is
    // the whole text the span refers to, so slice it down first.
    let span_units = if span_source.is_some() {
        if start >= end || end > units.len() {
            return true;
        }
        &units[start..end]
    } else {
        units
    };
    if span_units.len() < 2 {
        return true;
    }
    &span_units[1..span_units.len() - 1]
        == "use strict".encode_utf16().collect::<Vec<u16>>().as_slice()
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
    // spec 15.2.5 step 6: the self-binding is a non-strict immutable binding,
    // so a sloppy-mode assignment to the function's own name is ignored.
    scope.create_immutable_binding(&name, false)?;
    scope.initialize_binding(&name, value.clone())?;
    Ok(value)
}

/// CreateDynamicFunction's OrdinaryFunctionCreate (spec 20.2.1.1 step 41): an
/// ordinary sloppy function whose [[Prototype]] comes from
/// GetPrototypeFromConstructor and whose environment is the global one.
/// `parser::parse_function` already named it `anonymous` and checked the
/// early errors. The async/generator flags come from the parsed form, so the
/// GeneratorFunction/AsyncFunction/AsyncGeneratorFunction constructors reuse
/// this path. `source` is the assembled `function anonymous(...) {...}` text
/// the body's spans refer to: it drives the "use strict" directive check
/// (the running context's source is the caller's script, not the body) and
/// Function.prototype.toString.
pub fn instantiate_dynamic_function(
    agent: &mut Agent,
    f: &syntax::ast::Function,
    environment: EnvRef,
    proto: Handle<JsObject>,
    source: Option<JsString>,
) -> Result<Value, JsError> {
    let strict = source
        .as_ref()
        .map(|source| body_is_strict(agent, &f.body, Some(source)))
        .unwrap_or(false);
    let value = register_function(
        agent,
        f.name.map(crux::lookup),
        f.params.clone(),
        shared_function_body(agent, f, source.as_ref()),
        environment,
        false,
        DefinitionKind::function(f.is_async, f.is_generator),
        source,
        Some(strict),
    )?;
    // GetPrototypeFromConstructor wins over the default %Function.prototype%.
    let ValueKind::Function(function) = value.kind() else {
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
    let private_environment = agent.running_context()?.private_environment.clone();
    let strict = body_is_strict(agent, &body, None) || enclosing_strict;
    let class_field_initializer = agent.field_initializer_depth > 0;
    let mut data = EcmaFunction {
        name: None,
        params,
        body: std::rc::Rc::new(body),
        environment,
        this_mode: ThisMode::Lexical,
        strict,
        home_object: None,
        constructor_kind: ConstructorKind::Base,
        is_class_constructor: false,
        is_method: false,
        fields: Vec::new(),
        private_methods: Vec::new(),
        private_environment,
        super_constructor: None,
        default_derived: false,
        realm,
        is_async,
        is_generator: false,
        class_field_initializer,
        source: None,
        declaring_module: None,
        ir: None,
    };
    data.ir = Some(std::rc::Rc::new(crate::ir::compile_body(&data)?));
    let function = Function::new(None);
    let params = data.params.clone();
    agent.ecma_functions.insert(function.id(), data);
    set_function_properties(&function, &params, None)?;
    set_function_prototype(agent, &function)?;
    Ok(Value::Function(function))
}

/// Call (spec 10.2.1): dispatch an ECMAScript function through its body, and
/// everything else through `crux::function::call`. Bound chains are unwrapped
/// here so they can reach user-function targets. The agent is recorded for
/// the duration so crux-side ECMAScript calls (proxy traps) can reach us.
pub fn call(
    agent: &mut Agent,
    callee: &Value,
    this: Value,
    args: &[Value],
) -> Result<Value, JsError> {
    crux::function::with_agent(agent as *mut Agent as *mut (), || {
        call_inner(agent, callee, this, args)
    })
}

/// The realm whose intrinsic table holds `function`, memoized per function
/// id. Realm builtins identify themselves by intrinsic identity (the
/// `%eval%` dispatch pattern), so a builtin of one realm called from
/// another must run with its own realm current.
pub(crate) fn owning_realm(
    agent: &mut Agent,
    function: &Handle<Function>,
) -> Option<Handle<Realm>> {
    if let Some(cached) = agent.function_realms.borrow().get(&function.id()).cloned() {
        return cached;
    }
    let found = agent
        .realms
        .borrow()
        .iter()
        .find(|realm| {
            realm
                .intrinsics
                .contains(&Value::Function(function.clone()))
        })
        .cloned();
    agent
        .function_realms
        .borrow_mut()
        .insert(function.id(), found.clone());
    found
}

/// Attach a realm-specific throwable value to a kind-only engine error: the
/// error object is created from `realm`'s intrinsics while it is current
/// (the cross-realm fixtures assert the realm of e.g. a class-constructor
/// TypeError).
fn realm_throwable(
    agent: &mut Agent,
    error: JsError,
    realm: Handle<Realm>,
) -> Result<JsError, JsError> {
    if realm.global_object.id() == agent.current_realm()?.global_object.id() {
        return Ok(error);
    }
    agent.push_bootstrap_context(realm);
    let converted = crate::builtins::error::to_throwable(agent, &error);
    agent.execution_context_stack.pop();
    match converted {
        Ok(value) => Ok(error.with_value(value)),
        Err(conversion) => Err(conversion),
    }
}

fn call_inner(
    agent: &mut Agent,
    callee: &Value,
    this: Value,
    args: &[Value],
) -> Result<Value, JsError> {
    // A realm's builtin called while another realm is current (the
    // `$262.createRealm` fixtures) must dispatch with its own realm current:
    // push it, re-enter, and restore. With a single realm the current realm
    // is always the owning one, so the (cached) owning-realm lookup is
    // skipped on every call.
    if agent.realms.borrow().len() > 1
        && let ValueKind::Function(function) = callee.kind()
        && let Some(owning) = owning_realm(agent, &function)
        && owning.global_object.id() != agent.current_realm()?.global_object.id()
    {
        agent.push_bootstrap_context(owning);
        let result = call_inner(agent, callee, this, args);
        let result = match result {
            // A kind-only engine error from the other realm's builtin must
            // surface as that realm's error object (`assert.throws` checks
            // `thrown.constructor`); build it while the realm is current.
            Err(e) if e.value.is_none() => match crate::builtins::error::to_throwable(agent, &e) {
                Ok(value) => Err(e.with_value(value)),
                Err(conversion) => Err(conversion),
            },
            other => other,
        };
        agent.execution_context_stack.pop();
        return result;
    }
    match callee.kind() {
        ValueKind::Function(function) => match &function.kind {
            crux::function::FunctionKind::EcmaScript => {
                if let Some(data) = agent.ecma_functions.get(&function.id())
                    && data.is_class_constructor
                {
                    // spec 10.2.1: [[IsClassConstructor]] is true, so the
                    // function must be called with `new` (step 5). The error
                    // is the class's realm TypeError (the cross-realm
                    // fixtures assert `realm.global.TypeError`).
                    let error = JsError::new(
                        ErrorKind::TypeError,
                        "Class constructor cannot be invoked without 'new'".into(),
                    );
                    return Err(realm_throwable(agent, error, data.realm.clone())?);
                }
                let data = agent.ecma_functions.get(&function.id());
                // A function created inside a class field initializer runs
                // its body with the "Eval Inside Initializer" context (spec
                // 19.2.1.1: `func.[[ClassFieldInitializerName]]`). Only
                // arrows carry the marker, so the body of a plain function
                // resets the context (its `arguments` is its own).
                let marked = data.is_some_and(|data| data.class_field_initializer);
                let is_async_gen = data.is_some_and(|data| data.is_async && data.is_generator);
                let is_async = data.is_some_and(|data| data.is_async);
                let is_generator = data.is_some_and(|data| data.is_generator);
                let saved_depth = agent.field_initializer_depth;
                agent.field_initializer_depth = if marked { saved_depth + 1 } else { 0 };
                let result = if is_async_gen {
                    crate::async_generator::call_async_generator(agent, &function, this, args)
                } else if is_async {
                    crate::async_await::call_async_function(agent, &function, this, args)
                } else if is_generator {
                    crate::generator::call_generator(agent, &function, this, args)
                } else {
                    ordinary_call(agent, &function, this, args)
                };
                agent.field_initializer_depth = saved_depth;
                result
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
                // %eval% pattern). Which dispatch (if any) applies is stable
                // per function object, so the linear chain is memoized in
                // `agent.builtin_dispatch_cache` — plain closure builtins
                // (index 0) skip the chain entirely on warm calls.
                let id = function.id();
                let cached = agent.builtin_dispatch_cache.get(&id).copied();
                let dispatched = match cached {
                    Some(0) => None,
                    Some(index) => builtin_dispatch_at(agent, index, callee, &this, args),
                    None => {
                        // First call: %eval% and %evalScript% are host
                        // operations outside the module dispatch chain, then
                        // the chain itself is resolved and memoized.
                        if agent.current_realm()?.intrinsics.get("%eval%").as_ref() == Some(callee)
                        {
                            let source = args.first().cloned().unwrap_or(Value::Undefined);
                            let text = crate::context::to_string(agent, &source)?;
                            return crate::script::perform_eval(agent, &text, false, false);
                        }
                        // `%evalScript%` (test262's `$262.evalScript` host
                        // operation): evaluate as a Script — global
                        // declaration instantiation rather than eval semantics.
                        if agent
                            .current_realm()?
                            .intrinsics
                            .get("%evalScript%")
                            .as_ref()
                            == Some(callee)
                        {
                            let source = args.first().cloned().unwrap_or(Value::Undefined);
                            let text = crate::context::to_string(agent, &source)?;
                            return agent.run_script(&text.to_string_lossy());
                        }
                        let (index, result) = resolve_builtin_dispatch(agent, callee, &this, args);
                        agent.builtin_dispatch_cache.insert(id, index);
                        result
                    }
                };
                match dispatched {
                    Some(result) => result,
                    // The native closure runs directly here: re-entering
                    // `crux::function::call` would route back through the
                    // agent hook and loop.
                    None => match &function.kind {
                        crux::function::FunctionKind::Builtin {
                            call: Some(native), ..
                        } => native(&this, args),
                        _ => crux::function::call(callee, this, args),
                    },
                }
            }
        },
        _ => crux::function::call(callee, this, args),
    }
}

/// Run the dispatch at `index` (the memoized per-function slot). Each arm
/// mirrors one entry of the original `call_inner` chain; a stale entry
/// (the dispatch no longer applies) returns `None`.
fn builtin_dispatch_at(
    agent: &mut Agent,
    index: u8,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    match index {
        1 => crate::builtins::function::dispatch_call(agent, callee, this, args),
        2 => crate::builtins::object::dispatch_call(agent, callee, this, args),
        3 => crate::builtins::array::dispatch_call(agent, callee, this, args),
        4 => crate::builtins::typed_array::dispatch_call(agent, callee, this, args),
        5 => crate::builtins::keyed::dispatch_call(agent, callee, this, args),
        6 => crate::builtins::array_buffer::dispatch_call(agent, callee, this, args),
        7 => crate::builtins::dataview::dispatch_call(agent, callee, this, args),
        8 => crate::builtins::atomics::dispatch_call(agent, callee, this, args),
        9 => crate::builtins::json::dispatch_call(agent, callee, this, args),
        10 => crate::builtins::boolean::dispatch_call(agent, callee, this, args),
        11 => crate::builtins::bigint::dispatch_call(agent, callee, this, args),
        12 => crate::builtins::date::dispatch_call(agent, callee, this, args),
        13 => crate::builtins::symbol::dispatch_call(agent, callee, this, args),
        14 => crate::builtins::error::dispatch_call(agent, callee, this, args),
        15 => crate::builtins::math::dispatch_call(agent, callee, this, args),
        16 => crate::builtins::number::dispatch_call(agent, callee, this, args),
        17 => crate::builtins::string::dispatch_call(agent, callee, this, args),
        18 => crate::builtins::regexp::dispatch_call(agent, callee, this, args),
        19 => crate::builtins::weakref::dispatch_call(agent, callee, this, args),
        20 => crate::builtins::promise::dispatch_call(agent, callee, this, args),
        21 => crate::async_await::dispatch_resume(agent, callee, args),
        22 => crate::async_await::dispatch_async_from_sync(agent, callee, args),
        23 => crate::generator::dispatch_call(agent, callee, this, args),
        24 => crate::async_generator::dispatch_call(agent, callee, this, args),
        25 => crate::async_generator::dispatch_await(agent, callee, args),
        26 => crate::builtins::async_function::dispatch_call(agent, callee, this, args),
        27 => crate::builtins::iterator::dispatch_call(agent, callee, this, args),
        28 => crate::builtins::async_iterator::dispatch_call(agent, callee, this, args),
        29 => crate::builtins::disposable::dispatch_call(agent, callee, this, args),
        30 => crate::builtins::disposable::dispatch_continuation(agent, callee, args),
        31 => crate::builtins::proxy::dispatch_call(agent, callee, this, args),
        32 => crate::builtins::reflect::dispatch_call(agent, callee, this, args),
        33 => crate::module::dispatch_import_resolver(agent, callee, args),
        34 => crate::async_await::dispatch_async_from_sync_continuation(agent, callee, args),
        35 => crate::builtins::disposable::dispatch_async_body_disposal(agent, callee, args),
        36 => crate::builtins::module_source::dispatch_call(agent, callee, this, args),
        37 => crate::module::dispatch_deferred_module_then(agent, callee, this, args),
        38 => crate::module::dispatch_deferred_module_wait(agent, callee, args),
        39 => crate::builtins::temporal::dispatch_call(agent, callee, this, args),
        _ => None,
    }
}

/// Run the whole dispatch chain once, returning the first matching dispatch
/// index (0 when none applies) with its result.
fn resolve_builtin_dispatch(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> (u8, Option<Result<Value, JsError>>) {
    for index in 1..=39 {
        let result = builtin_dispatch_at(agent, index, callee, this, args);
        if result.is_some() {
            return (index, result);
        }
    }
    (0, None)
}

/// Construct (spec 10.2.1): like `call` for the `new` operator, with
/// newTarget propagation through bound functions. The agent is recorded for
/// the duration so crux-side ECMAScript constructs (proxy traps) reach us.
pub fn construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    crux::function::with_agent(agent as *mut Agent as *mut (), || {
        construct_inner(agent, callee, args, new_target)
    })
}

fn construct_inner(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    // Cross-realm builtin constructors (`new $262.createRealm().global.Array`)
    // dispatch with their own realm current, like `call_inner`.
    if let ValueKind::Function(function) = callee.kind()
        && let Some(owning) = owning_realm(agent, &function)
        && owning.global_object.id() != agent.current_realm()?.global_object.id()
    {
        agent.push_bootstrap_context(owning);
        let result = construct_inner(agent, callee, args, new_target);
        let result = match result {
            // Surface a kind-only engine error as the callee realm's error
            // object before the realm context pops (like `call_inner`).
            Err(e) if e.value.is_none() => match crate::builtins::error::to_throwable(agent, &e) {
                Ok(value) => Err(e.with_value(value)),
                Err(conversion) => Err(conversion),
            },
            other => other,
        };
        agent.execution_context_stack.pop();
        return result;
    }
    match callee.kind() {
        ValueKind::Function(function) => match &function.kind {
            crux::function::FunctionKind::EcmaScript => {
                // Generator, async, and async-generator functions have no
                // [[Construct]] (spec 27.4.2/27.5.2/27.6.2 FunctionAllocate);
                // neither do arrows (lexical this) or methods/accessors.
                if agent
                    .ecma_functions
                    .get(&function.id())
                    .is_some_and(is_constructible_data)
                {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "not a constructor".into(),
                    ));
                }
                let marked = agent
                    .ecma_functions
                    .get(&function.id())
                    .is_some_and(|data| data.class_field_initializer);
                let saved_depth = agent.field_initializer_depth;
                agent.field_initializer_depth = if marked { saved_depth + 1 } else { 0 };
                let result = ordinary_construct(agent, &function, args, new_target);
                agent.field_initializer_depth = saved_depth;
                result
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
                    crate::builtins::array::dispatch_construct(agent, callee, args, new_target)
                {
                    return result;
                }
                if let Some(result) = crate::builtins::typed_array::dispatch_construct(
                    agent, callee, args, new_target,
                ) {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::keyed::dispatch_construct(agent, callee, args, new_target)
                {
                    return result;
                }
                if let Some(result) = crate::builtins::array_buffer::dispatch_construct(
                    agent, callee, args, new_target,
                ) {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::dataview::dispatch_construct(agent, callee, args, new_target)
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
                    crate::builtins::string::dispatch_construct(agent, callee, args, new_target)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::regexp::dispatch_construct(agent, callee, args, new_target)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::promise::dispatch_construct(agent, callee, args, new_target)
                {
                    return result;
                }
                if let Some(result) = crate::builtins::async_function::dispatch_construct(
                    agent, callee, args, new_target,
                ) {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::iterator::dispatch_construct(agent, callee, args, new_target)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::disposable::dispatch_construct(agent, callee, args, new_target)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::proxy::dispatch_construct(agent, callee, args, new_target)
                {
                    return result;
                }
                if let Some(result) =
                    crate::builtins::temporal::dispatch_construct(agent, callee, args, new_target)
                {
                    return result;
                }
                // The native constructor closure runs directly here: re-entering
                // `crux::function::construct` would route back through the
                // agent hook and loop.
                match &function.kind {
                    crux::function::FunctionKind::Builtin {
                        construct: Some(ctor),
                        ..
                    } => ctor(new_target, args),
                    _ => crux::function::construct(callee, args, new_target),
                }
            }
        },
        _ => crux::function::construct(callee, args, new_target),
    }
}

/// Whether the function's slots deny [[Construct]] (spec 7.2.4): arrows
/// (lexical this), methods/accessors, and async/generator functions.
fn is_constructible_data(data: &EcmaFunction) -> bool {
    data.is_async
        || data.is_generator
        || data.this_mode == ThisMode::Lexical
        || (data.is_method && !data.is_class_constructor)
}

/// IsConstructor (spec 7.2.4) at the runtime level. Crux's Function-level
/// check reports every ECMAScript function as constructible; the slots above
/// narrow it (a class extends an arrow, or `new` on a method, must throw).
pub fn is_constructor(agent: &Agent, value: &Value) -> bool {
    let ValueKind::Function(function) = value.kind() else {
        return crux::value::is_constructor(value);
    };
    match &function.kind {
        crux::function::FunctionKind::EcmaScript => agent
            .ecma_functions
            .get(&function.id())
            .is_some_and(|data| !is_constructible_data(data)),
        // A bound function is constructible iff its target is (spec 10.4.1.2).
        crux::function::FunctionKind::Bound { target, .. } => is_constructor(agent, target),
        _ => crux::value::is_constructor(value),
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
    // Copy only the slots a call reads; the record also holds per-function
    // data (source text, compiled IR, class fields) whose clones would
    // re-allocate on every call.
    let (
        old_env,
        this_mode,
        realm,
        private_environment,
        strict,
        params,
        body,
        declaring_module,
        ir,
    ) = {
        let record = agent.ecma_functions.get(&function.id()).ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "Function body is not registered".into(),
            )
        })?;
        (
            record.environment.clone(),
            record.this_mode,
            record.realm.clone(),
            record.private_environment.clone(),
            record.strict,
            record.params.clone(),
            record.body.clone(),
            record.declaring_module.clone(),
            record.ir.clone(),
        )
    };
    let function_value = function.self_value();
    let function_env = new_function_environment(
        Some(old_env),
        function_value.clone(),
        Value::Undefined,
        this_mode == ThisMode::Lexical,
    );
    // PrepareForOrdinaryCall (spec 10.2.1 step 3): the callee context's
    // [[ScriptOrModule]] is the caller's — but `import.meta` resolves
    // lexically to the module a function is declared in (spec 13.3.7.1), so
    // a function's declaring module wins over the caller's.
    let caller_script_or_module = agent
        .running_context()
        .ok()
        .and_then(|context| context.script_or_module.clone());
    let script_or_module = declaring_module
        .map(crate::context::ScriptOrModule::Module)
        .or(caller_script_or_module);
    agent.execution_context_stack.push(ExecutionContext {
        function: Some(function_value.clone()),
        realm,
        script_or_module,
        lexical_environment: function_env.clone(),
        variable_environment: function_env.clone(),
        private_environment,
        source: agent
            .running_context()
            .ok()
            .and_then(|context| context.source.clone()),
        annex_b_hoistable: Default::default(),
    });
    let result = (|| -> Result<Value, JsError> {
        // OrdinaryCallBindThis: strict keeps `this` as-is; sloppy coerces
        // undefined/null to the global object and boxes primitives; lexical
        // binds nothing.
        if this_mode != ThisMode::Lexical {
            let this = if this_mode == ThisMode::Sloppy {
                match this.kind() {
                    ValueKind::Undefined | ValueKind::Null => {
                        let global = agent.running_context()?.realm.global_object.clone();
                        Value::Object(global)
                    }
                    ValueKind::Object(_) | ValueKind::Function(_) => this,
                    _ => crate::context::to_object(agent, &this)?,
                }
            } else {
                this
            };
            function_env.bind_this_value(this)?;
        }
        function_declaration_instantiation(
            agent,
            &function_value,
            &params,
            &body,
            this_mode,
            strict,
            args,
            &function_env,
        )?;
        match ir {
            Some(ir) => run_compiled_body(agent, strict, &ir),
            None => evaluate_body(agent, &body, strict),
        }
    })();
    // A kind-only engine error escapes the body with the function's realm
    // context still current; surface it as that realm's error object now, so
    // a caller from another realm sees the right constructor.
    let result = match result {
        Err(e) if e.value.is_none() => match crate::builtins::error::to_throwable(agent, &e) {
            Ok(value) => Err(e.with_value(value)),
            Err(conversion) => Err(conversion),
        },
        other => other,
    };
    agent.execution_context_stack.pop();
    result
}

/// OrdinaryCallEvaluateBody: evaluate the body; a `return` completion is the
/// result, any other normal completion yields *undefined* (spec 15.2.2).
fn evaluate_body(agent: &mut Agent, body: &Block, strict: bool) -> Result<Value, JsError> {
    let completion = eval_statement_list(agent, &body.stmts, strict)?;
    body_completion_to_value(completion)
}

/// Map a function body's completion to its call result (spec 15.2.2): a
/// `return` completion is the value, any other normal completion is
/// *undefined*; an uncaught throw and stray control transfers are errors.
fn body_completion_to_value(completion: Completion) -> Result<Value, JsError> {
    match completion {
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

/// Run an ordinary function body on the step VM. The body runs in the
/// environment `function_declaration_instantiation` installed on the running
/// context; on completion the body env's `using` resources are disposed —
/// top-level `using` binds into the body env, which has no scope-exit step to
/// dispose it (spec 14.2.3 step 6). Ordinary bodies never suspend.
fn run_compiled_body(
    agent: &mut Agent,
    strict: bool,
    ir: &std::rc::Rc<crate::ir::CompiledBody>,
) -> Result<Value, JsError> {
    let body_env = agent.running_context()?.lexical_environment.clone();
    let mut vm = Vm::new(body_env.clone(), strict);
    let completion = match vm.start(agent, ir)? {
        VmOutcome::Completed(completion) => completion,
        VmOutcome::Suspended(_) => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "ordinary function suspended unexpectedly".into(),
            ));
        }
    };
    let completion = crate::eval::dispose_env_resources(agent, &body_env, Ok(completion))?;
    body_completion_to_value(completion)
}

/// The `[[Construct]]` of an ordinary function (spec 10.2.1): create `this`
/// from the constructor's prototype, bind it, and run the body. Base class
/// constructors initialize instance fields before the body; derived
/// constructors leave `this` uninitialized until `super()` binds it.
/// Whether `value` is a Proxy whose [[ProxyHandler]] slot has been cleared
/// (spec 10.5.13): its internal methods all throw a TypeError.
fn is_revoked_proxy(value: &Value) -> bool {
    matches!(
        value.kind(),
        ValueKind::Object(obj)
            if matches!(&obj.kind, crux::object::ObjectKind::Proxy(slots) if slots.target.borrow().is_none())
    )
}

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
        // `prototype` (an object — including a function value), falling back
        // to %Object.prototype% when it isn't an object (S13.2.2_A3_T1).
        let prototype = crate::context::get_property(
            agent,
            new_target,
            &JsString::from_utf8("prototype"),
            new_target.clone(),
        )?;
        let proto = match crate::context::as_object(&prototype) {
            Some(obj) => Some(obj),
            None => {
                // GetFunctionRealm (spec 10.2.5): a revoked Proxy throws.
                if is_revoked_proxy(new_target) {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Cannot perform operation on a revoked Proxy".into(),
                    ));
                }
                crate::context::get_function_realm(agent, new_target)?
                    .intrinsics
                    .get("%Object.prototype%")
                    .and_then(|value| crate::context::as_object(&value))
            }
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
    let caller_script_or_module = agent
        .running_context()
        .ok()
        .and_then(|context| context.script_or_module.clone());
    let script_or_module = data
        .declaring_module
        .clone()
        .map(crate::context::ScriptOrModule::Module)
        .or(caller_script_or_module);
    agent.execution_context_stack.push(ExecutionContext {
        function: Some(function_value.clone()),
        realm: data.realm.clone(),
        script_or_module,
        lexical_environment: function_env.clone(),
        variable_environment: function_env.clone(),
        private_environment: data.private_environment.clone(),
        source: agent
            .running_context()
            .ok()
            .and_then(|context| context.source.clone()),
        annex_b_hoistable: Default::default(),
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
        function_declaration_instantiation(
            agent,
            &function_value,
            &data.params,
            &data.body,
            data.this_mode,
            data.strict,
            args,
            &function_env,
        )?;
        let completed = match &data.ir {
            Some(ir) => {
                let body_env = agent.running_context()?.lexical_environment.clone();
                let mut vm = Vm::new(body_env.clone(), data.strict);
                let completion = match vm.start(agent, ir)? {
                    VmOutcome::Completed(completion) => completion,
                    VmOutcome::Suspended(_) => {
                        return Err(JsError::new(
                            ErrorKind::TypeError,
                            "constructor body suspended unexpectedly".into(),
                        ));
                    }
                };
                crate::eval::dispose_env_resources(agent, &body_env, Ok(completion))?
            }
            None => eval_statement_list(agent, &data.body.stmts, data.strict)?,
        };
        // spec 10.2.1 [[Construct]] steps 15-21: an object return wins; a base
        // constructor falls back to `this`; a derived constructor returns the
        // `super()`-bound `this` (or throws on any other value).
        match completed {
            Completion::Return(value) => match value.kind() {
                ValueKind::Object(_) | ValueKind::Function(_) => Ok(value),
                _ if derived => {
                    if matches!(value.kind(), ValueKind::Undefined) {
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
    let ValueKind::Function(ctor) = ctor.kind() else {
        return Ok(());
    };
    let data = agent.ecma_functions.get(&ctor.id()).cloned();
    let Some(data) = data else {
        return Ok(());
    };
    let ValueKind::Object(obj) = obj.kind() else {
        return Ok(());
    };
    // Private methods/accessors first (spec 7.3.27 step 1); both private
    // methods and fields cannot be added to a non-extensible object (spec
    // 10.2.10 step 1 / 10.2.13).
    for method in &data.private_methods {
        if !obj.is_extensible()? {
            return Err(private_add_extensible_error());
        }
        obj.private_element_add(method.clone())?;
    }
    // Fields (spec 7.3.27 step 2): private fields via PrivateFieldAdd.
    for field in &data.fields {
        // DefineField (spec 7.3.23): the initializer runs in the class scope
        // with the current `this` (the running context already chains to the
        // class environment through the constructor's [[Environment]]). A
        // direct eval inside an initializer applies the "Eval Inside
        // Initializer" early errors (spec 19.2.1.1).
        let value = match &field.init {
            Some(init) => {
                agent.field_initializer_depth += 1;
                // The initializer runs in a function context of its own
                // (spec 15.7.14 FieldDefinition): `new.target` is
                // *undefined* there, `this` is the instance, and `super`
                // resolves through the constructor's [[HomeObject]] (the
                // class prototype). A direct eval inside the initializer
                // inherits this context, so its `new.target` is also
                // *undefined*.
                let init_env = crate::env::new_function_environment(
                    Some(agent.running_context()?.lexical_environment.clone()),
                    Value::Function(ctor.clone()),
                    Value::Undefined,
                    false,
                );
                init_env.bind_this_value(Value::Object(obj.clone()))?;
                let saved_lexical = agent.running_context_mut()?.lexical_environment.clone();
                agent.running_context_mut()?.lexical_environment = init_env.clone();
                let result = eval_expr(agent, init, true);
                agent.running_context_mut()?.lexical_environment = saved_lexical;
                agent.field_initializer_depth -= 1;
                result?
            }
            None => Value::Undefined,
        };
        if let Some(name_id) = field.private_name {
            // The initializer may have made `this` non-extensible; the add
            // still fails then (spec 10.2.10 step 1).
            if !obj.is_extensible()? {
                return Err(private_add_extensible_error());
            }
            obj.private_element_add(crux::object::PrivateElement {
                name_id,
                kind: crux::object::PrivateElementKind::Field(value),
            })?;
        } else {
            // DefineField on a deferred namespace: the receiver descriptor
            // read triggers the module's evaluation (import-defer
            // [[DefineOwnProperty]] step 2 — the [[GetOwnProperty]] probe).
            crate::module::ensure_deferred_namespace_evaluation_key(agent, &obj, &field.name)?;
            obj.create_data_property_or_throw_key(&field.name, value)?;
        }
    }
    Ok(())
}

fn private_add_extensible_error() -> JsError {
    JsError::new(
        ErrorKind::TypeError,
        "Cannot add private field to a non-extensible object".into(),
    )
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
///
/// Takes the call-relevant slots rather than the whole record: the record
/// also holds the function's source text and compiled IR, whose clones would
/// re-allocate on every call.
#[allow(clippy::too_many_arguments)]
pub(crate) fn function_declaration_instantiation(
    agent: &mut Agent,
    function_value: &Value,
    params: &[BindingElement],
    body: &Block,
    this_mode: ThisMode,
    strict: bool,
    args: &[Value],
    function_env: &EnvRef,
) -> Result<(), JsError> {
    let simple = is_simple_parameter_list(params);

    // Per spec 10.4.4.2/10.4.4.7 the arguments object is an ordinary object
    // with %Object.prototype% as its prototype.
    let arguments_prototype = agent
        .current_realm()
        .ok()
        .and_then(|realm| realm.intrinsics.get("%Object.prototype%"))
        .and_then(|value| crate::context::as_object(&value));

    // BoundNames of the formal parameters.
    let mut param_names: Vec<JsString> = Vec::new();
    for param in params {
        bound_names(&param.pattern, &mut param_names);
    }

    // The arguments object (spec 16.1.8 steps 20-23).
    let lexical_names = top_level_lexically_declared_names(&body.stmts);
    let func_names: Vec<JsString> = body
        .stmts
        .iter()
        .filter_map(|s| match &s.kind {
            StmtKind::FunctionDecl(f) => f.name.map(crux::lookup),
            _ => None,
        })
        .collect();
    let arguments_obj_needed = !(this_mode == ThisMode::Lexical
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
    for param in params {
        let mut names = Vec::new();
        bound_names(&param.pattern, &mut names);
        for name in names {
            if !param_env.has_binding(&name)? {
                param_env.create_mutable_binding(&name, false)?;
            }
            param_env.mark_parameter(&name);
        }
    }

    // The arguments object (spec 16.1.8 steps 58-70) is created before the
    // formals bind, so a default initializer can reference `arguments`.
    let mut param_bindings = param_names.clone();
    if arguments_obj_needed {
        // When the body cannot observe `arguments` (no identifier reference
        // and no direct eval that could introduce one), nothing can read the
        // object, so bind the name to undefined instead of building it. The
        // binding is still created: var/Annex-B instantiation keys off
        // `param_bindings`, and a sloppy `var arguments` must collide with it.
        let arguments_obj = if !simple || crate::script::body_observes_arguments(body) {
            if strict || !simple {
                // spec 10.4.4.9: the `callee` accessor's get/set is the shared
                // %ThrowTypeError% — the same object as Function.prototype's
                // caller/arguments throwers (ThrowTypeError/unique-per-realm-*).
                let thrower = agent
                    .current_realm()?
                    .intrinsics
                    .get("%ThrowTypeError%")
                    .ok_or_else(|| {
                        JsError::new(
                            ErrorKind::TypeError,
                            "%ThrowTypeError% intrinsic missing".into(),
                        )
                    })?;
                Value::Object(JsObject::unmapped_arguments_object_create(
                    arguments_prototype.clone(),
                    args,
                    thrower,
                )?)
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
                    arguments_prototype.clone(),
                    function_value.clone(),
                    &param_names,
                    args,
                    make_getter,
                    make_setter,
                )?)
            }
        } else {
            Value::Undefined
        };
        if strict {
            param_env.create_immutable_binding(&JsString::from_utf8("arguments"), false)?;
        } else {
            param_env.create_mutable_binding(&JsString::from_utf8("arguments"), false)?;
        }
        param_env.mark_parameter(&JsString::from_utf8("arguments"));
        // spec 10.4.4.7/10.4.4.9: the arguments object's @@iterator is
        // %Array.prototype.values% (a Phase 8 join the crux creation sites
        // cannot make).
        if let Some(obj) = crate::context::as_object(&arguments_obj)
            && let Some(values) = agent
                .current_realm()?
                .intrinsics
                .get("%Array.prototype.values%")
        {
            obj.define_property_key(
                &crux::property::PropertyKey::Symbol(
                    crux::symbol::well_known("iterator").as_ref().clone(),
                ),
                &crux::property::PropertyDescriptor {
                    value: Some(values),
                    writable: Some(true),
                    get: None,
                    set: None,
                    enumerable: Some(false),
                    configurable: Some(true),
                },
            )?;
        }
        param_env.initialize_binding(&JsString::from_utf8("arguments"), arguments_obj)?;
        param_bindings.push(JsString::from_utf8("arguments"));
    }

    // IteratorBindingInitialization of the formals (spec 16.1.8 step 79):
    // positional for simple lists, full binding for non-simple ones.
    if simple {
        for (index, param) in params.iter().enumerate() {
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
            params,
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
    for decl in top_level_var_scoped_declarations(&body.stmts) {
        match decl {
            crate::script::VarScopedDecl::Variable(names) => {
                for name in names {
                    if instantiated.contains(&name) {
                        continue;
                    }
                    instantiated.push(name.clone());
                    variable_env.create_mutable_binding(&name, false)?;
                    if !simple {
                        let initial =
                            if !param_bindings.contains(&name) || func_names.contains(&name) {
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
            // Function declarations are var-scoped too (spec 10.2.11 step
            // 27): the binding must exist (non-deletable) before step 30
            // binds the function value, so `delete f` returns false
            // (S13_A12_T2).
            crate::script::VarScopedDecl::Function(f) => {
                let Some(name_atom) = f.name else {
                    continue;
                };
                let name = crux::lookup(name_atom);
                if instantiated.contains(&name) {
                    continue;
                }
                instantiated.push(name.clone());
                variable_env.create_mutable_binding(&name, false)?;
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
    for decl in top_level_lexically_scoped_declarations(&body.stmts) {
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
    for stmt in &body.stmts {
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

    // Annex B.3.3.1: sloppy block-level function declarations hoist a var
    // binding (initialized to *undefined*) unless the name is already
    // var-declared, a parameter/`arguments`, or the hoist would produce an
    // early error (a lexical conflict in an enclosing scope). The decision is
    // recorded so block instantiation (B.3.2.1) can repeat it.
    if !strict {
        for (name, span, hoistable) in crate::script::annex_b_function_hoists(&body.stmts) {
            if param_bindings.contains(&name) || !hoistable {
                continue;
            }
            agent
                .running_context()?
                .annex_b_hoistable
                .borrow_mut()
                .insert((span.start, span.end));
            if instantiated.contains(&name) || func_names.contains(&name) {
                continue;
            }
            variable_env.create_mutable_binding(&name, false)?;
            variable_env.initialize_binding(&name, Value::Undefined)?;
            instantiated.push(name);
        }
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

    #[test]
    fn methods_and_arrows_are_not_constructors() {
        // spec 7.2.4: MethodDefinition and arrow closures have no [[Construct]]
        // (name-invoke-ctor.js, superclass-arrow-function.js).
        assert_eq!(
            run("var o = { m() {} }; var threw = false; try { new o.m(); } catch (e) { threw = e instanceof TypeError; } threw")
                .unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("var fn = () => {}; var threw = false; try { class C extends fn {} } catch (e) { threw = e instanceof TypeError; } threw")
                .unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn generator_methods_get_a_prototype_property() {
        // GeneratorMethod evaluation (spec 15.4.5): the method's `prototype`
        // inherits %Generator.prototype% and is writable/non-configurable
        // (generator-prototype-prop.js).
        assert_eq!(
            run("var m = { *method() {} }.method; \
                 Object.getPrototypeOf(m.prototype) === Object.getPrototypeOf(function* () {}).prototype && \
                 Object.getOwnPropertyDescriptor(m, 'prototype').writable === true && \
                 Object.getOwnPropertyDescriptor(m, 'prototype').configurable === false")
                .unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn new_evaluates_args_before_the_ctor_check() {
        // EvaluateNew (spec 13.3.5.1.1): the argument list runs before
        // IsConstructor (ctorExpr-isCtor-after-args-eval.js).
        assert_eq!(
            run("var x = {}; var threw = false; try { new x(x = Array); } catch (e) { threw = true; } threw && x === Array")
                .unwrap(),
            Value::Boolean(true)
        );
    }
}
