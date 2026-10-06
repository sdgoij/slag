//! Decorator evaluation (the stage-3 decorators proposal): calling decorators
//! with a context object and applying their return values.
//!
//! Class decorators are implemented here; element decorators reuse the same
//! context/initializer machinery as they land.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use crux::Function;
use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::intern_utf8;
use crux::object::JsObject;
use crux::property::PropertyKey;
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use syntax::ast::{
    AssignOp, BinaryOp, BindingElement, BindingPattern, Block, Expr, ExprKind,
    Function as FunctionAst, MemberExpr, MemberProperty, Stmt, StmtKind,
};

use crate::agent::Agent;
use crate::env::new_declarative_environment;

/// The initializers a site's decorators registered, in registration order.
/// Shared by every decorator of the site; each decorator gets its own
/// "finished" flag so a late `addInitializer` throws.
type InitializerList = Rc<RefCell<Vec<Value>>>;

/// Evaluate a decorator list to its functions, in source order (the proposal
/// evaluates decorator expressions top-to-bottom, interspersed with the
/// computed keys).
pub(crate) fn evaluate_decorators(
    agent: &mut Agent,
    decorators: &[Expr],
    strict: bool,
) -> Result<Vec<Value>, JsError> {
    let mut values = Vec::with_capacity(decorators.len());
    for expr in decorators {
        values.push(crate::expr::eval_expr(agent, expr, strict)?);
    }
    Ok(values)
}

/// Build the `addInitializer` function bound to `initializers`, throwing if
/// called after its decorator returned (`finished`).
fn add_initializer_function(
    agent: &mut Agent,
    initializers: &InitializerList,
    finished: &Rc<Cell<bool>>,
) -> Result<Value, JsError> {
    let initializers = initializers.clone();
    let finished = finished.clone();
    let prototype = function_prototype(agent)?;
    let function = Function::create_builtin(
        Some(JsString::from_utf8("addInitializer")),
        0,
        Box::new(move |_this, args| {
            if finished.get() {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "addInitializer called after decoration finished".into(),
                ));
            }
            let initializer = args.first().cloned().unwrap_or(Value::Undefined);
            if !crux::value::is_callable(&initializer) {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "addInitializer requires a callable".into(),
                ));
            }
            initializers.borrow_mut().push(initializer);
            Ok(Value::Undefined)
        }),
        None,
        prototype,
    )?;
    Ok(Value::Function(function))
}

/// Decorate a class (spec: DecorateClass): call its decorators with the class
/// value and a `kind: "class"` context, in reverse source order, applying a
/// returned callable as the replacement. Returns the (possibly replaced)
/// class and the collected initializers, in registration order.
pub(crate) fn decorate_class(
    agent: &mut Agent,
    name: Option<JsString>,
    decorators: &[Expr],
    value: Value,
    strict: bool,
) -> Result<(Value, Vec<Value>), JsError> {
    let decorators = evaluate_decorators(agent, decorators, strict)?;
    let initializers: InitializerList = Rc::new(RefCell::new(Vec::new()));
    let mut class = value;
    for decorator in decorators.iter().rev() {
        if !crux::value::is_callable(decorator) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "class decorator must be callable".into(),
            ));
        }
        let finished = Rc::new(Cell::new(false));
        let context = class_context(agent, name.clone(), &initializers, &finished)?;
        let result = crate::function::call(agent, decorator, Value::Undefined, &[class, context])?;
        finished.set(true);
        match result.kind() {
            ValueKind::Undefined => {}
            _ if crux::value::is_callable(&result) => class = result,
            _ => {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "class decorator returned a non-callable".into(),
                ));
            }
        }
    }
    let taken = std::mem::take(&mut *initializers.borrow_mut());
    Ok((class, taken))
}

/// A class decorator's context object: `{ kind, name, addInitializer }` — no
/// `access`/`static`/`private`.
fn class_context(
    agent: &mut Agent,
    name: Option<JsString>,
    initializers: &InitializerList,
    finished: &Rc<Cell<bool>>,
) -> Result<Value, JsError> {
    let object = JsObject::ordinary_object_create(object_prototype(agent)?);
    let add_initializer = add_initializer_function(agent, initializers, finished)?;
    object.create_data_property_or_throw_key(
        &PropertyKey::from_utf8("kind"),
        Value::String(Handle::new(JsString::from_utf8("class"))),
    )?;
    object.create_data_property_or_throw_key(
        &PropertyKey::from_utf8("name"),
        name.map(|name| Value::String(Handle::new(name)))
            .unwrap_or(Value::Undefined),
    )?;
    object.create_data_property_or_throw_key(
        &PropertyKey::from_utf8("addInitializer"),
        add_initializer,
    )?;
    Ok(Value::Object(object))
}

/// `%Function.prototype%` as a prototype handle for a freshly created builtin.
fn function_prototype(agent: &mut Agent) -> Result<Option<Handle<JsObject>>, JsError> {
    let realm = agent.current_realm()?;
    Ok(realm
        .intrinsics
        .get("%Function.prototype%")
        .and_then(|value| match value.kind() {
            ValueKind::Function(function) => function.object.handle(),
            _ => None,
        }))
}

/// `%Object.prototype%` as the context object's prototype.
fn object_prototype(agent: &mut Agent) -> Result<Option<Handle<JsObject>>, JsError> {
    let realm = agent.current_realm()?;
    Ok(realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| crate::context::as_object(&value)))
}

/// The kind of element a decorator context distinguishes (S2b: public method,
/// getter, setter, and field).
#[derive(Clone, Copy)]
pub(crate) enum ElementKind {
    Method,
    Getter,
    Setter,
    Field,
    Accessor,
}

impl ElementKind {
    fn as_str(self) -> &'static str {
        match self {
            ElementKind::Method => "method",
            ElementKind::Getter => "getter",
            ElementKind::Setter => "setter",
            ElementKind::Field => "field",
            ElementKind::Accessor => "accessor",
        }
    }

    /// The `access` members present: a method/getter reads, a setter writes,
    /// a field or auto-accessor reads and writes.
    fn access_members(self) -> (bool, bool) {
        match self {
            ElementKind::Method | ElementKind::Getter => (true, false),
            ElementKind::Setter => (false, true),
            ElementKind::Field | ElementKind::Accessor => (true, true),
        }
    }
}

/// A public `access` operation.
#[derive(Clone, Copy)]
enum AccessOp {
    Has,
    Get,
    Set,
}

/// The binding holding a synthesized access function's property key.
const KEY_BINDING: &str = "%key%";

/// A unique span base for each synthesized function, past any real source
/// offset. `shared_function_body` keys its body cache on the function node's
/// address *and* span, and every call here passes the same stack address and
/// (0,0) spans, so two synthesized functions would otherwise share one cached
/// body (the first `has`/`get`/`set` built would answer for all of them).
static NEXT_SYNTH_SPAN: AtomicU32 = AtomicU32::new(0x8000_0000);

/// The name of an element being decorated: a public property key, or a private
/// name paired with the class PrivateEnvironment its synthesized access
/// functions resolve `#name` against.
pub(crate) enum ElementName<'a> {
    Public(&'a PropertyKey),
    Private {
        atom: crux::string::AtomId,
        environment: &'a Handle<crate::context::PrivateEnvironment>,
    },
}

impl ElementName<'_> {
    /// The `context.name` value: the property key, or the `#name` description a
    /// private element exposes (proposal: private elements).
    fn context_value(&self) -> Value {
        match self {
            ElementName::Public(key) => key_value(key),
            ElementName::Private { atom, .. } => Value::String(Handle::new(JsString::from_utf8(
                &format!("#{}", crux::lookup(*atom).to_string_lossy()),
            ))),
        }
    }

    fn is_private(&self) -> bool {
        matches!(self, ElementName::Private { .. })
    }
}

/// A property key as a language value (an `access` function closes over it).
fn key_value(key: &PropertyKey) -> Value {
    match key {
        PropertyKey::String(atom) => Value::String(Handle::new(crux::lookup(*atom))),
        PropertyKey::Symbol(symbol) => Value::Symbol(*symbol),
    }
}

/// Synthesize an `access` function — `has(obj)`, `get(obj)`, or `set(obj, v)`
/// — as an ordinary closure, so the operation runs the full `[[Get]]`/`[[Set]]`/
/// `[[HasProperty]]` protocol (getters, proxies, and all) rather than a raw
/// lookup. A public element closes over the property key; a private one indexes
/// the private name directly and carries the class PrivateEnvironment.
fn synthesize_access_function(
    agent: &mut Agent,
    name: &ElementName,
    op: AccessOp,
) -> Result<Value, JsError> {
    let span_base = NEXT_SYNTH_SPAN.fetch_add(2, Ordering::Relaxed);
    let span = crux::Span::new(span_base, span_base + 1);
    let env = new_declarative_environment(Some(agent.running_context()?.lexical_environment));

    let key_atom = intern_utf8(KEY_BINDING);
    if let ElementName::Public(key) = name {
        let key_name = crux::lookup(key_atom);
        env.create_immutable_binding(&key_name, false)?;
        env.initialize_binding(&key_name, key_value(key))?;
    }

    let obj_atom = intern_utf8("obj");
    let v_atom = intern_utf8("v");
    let ident = |atom| Expr {
        span,
        kind: ExprKind::Ident(atom),
    };
    let member = || Expr {
        span,
        kind: ExprKind::Member(MemberExpr {
            object: Box::new(ident(obj_atom)),
            property: match name {
                ElementName::Public(_) => MemberProperty::Computed(Box::new(ident(key_atom))),
                ElementName::Private { atom, .. } => MemberProperty::Private(*atom),
            },
            property_token: None,
            optional: false,
            span,
        }),
    };
    let has_test = || match name {
        ElementName::Public(_) => Expr {
            span,
            kind: ExprKind::Binary {
                op: BinaryOp::In,
                left: Box::new(ident(key_atom)),
                right: Box::new(ident(obj_atom)),
            },
        },
        ElementName::Private { atom, .. } => Expr {
            span,
            kind: ExprKind::PrivateIn {
                name: *atom,
                object: Box::new(ident(obj_atom)),
            },
        },
    };
    let param = |atom| BindingElement {
        pattern: BindingPattern::Ident(atom),
        init: None,
        rest: false,
        span,
    };
    let (params, stmt) = match op {
        AccessOp::Has => (
            vec![param(obj_atom)],
            Stmt {
                span,
                kind: StmtKind::Return(Some(has_test())),
            },
        ),
        AccessOp::Get => (
            vec![param(obj_atom)],
            Stmt {
                span,
                kind: StmtKind::Return(Some(member())),
            },
        ),
        AccessOp::Set => (
            vec![param(obj_atom), param(v_atom)],
            Stmt {
                span,
                kind: StmtKind::Expr(Expr {
                    span,
                    kind: ExprKind::Assign {
                        op: AssignOp::Assign,
                        target: Box::new(member()),
                        value: Box::new(ident(v_atom)),
                    },
                }),
            },
        ),
    };
    let function = FunctionAst {
        span,
        name: None,
        params,
        body: Block {
            stmts: vec![stmt],
            span,
        },
        is_async: false,
        is_generator: false,
        statement_position: false,
    };
    let closure = crate::function::instantiate_method(agent, &function, env, true)?;
    if let ElementName::Private { environment, .. } = name {
        crate::class::set_private_environment(agent, &closure, environment)?;
    }
    Ok(closure)
}

/// Decorate a public method/getter/setter (spec: DecorateClassElement): call
/// its decorators with the closure and an element context, in reverse source
/// order, applying a returned callable as the replacement. Returns the final
/// closure and the `addInitializer` callbacks, in registration order.
pub(crate) fn decorate_element(
    agent: &mut Agent,
    decorators: &[Expr],
    strict: bool,
    kind: ElementKind,
    name: &ElementName,
    is_static: bool,
    value: Value,
) -> Result<(Value, Vec<Value>), JsError> {
    let decorators = evaluate_decorators(agent, decorators, strict)?;
    let initializers: InitializerList = Rc::new(RefCell::new(Vec::new()));
    let mut current = value;
    for decorator in decorators.iter().rev() {
        if !crux::value::is_callable(decorator) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "decorator must be callable".into(),
            ));
        }
        let finished = Rc::new(Cell::new(false));
        let context = element_context(agent, kind, name, is_static, &initializers, &finished)?;
        let result =
            crate::function::call(agent, decorator, Value::Undefined, &[current, context])?;
        finished.set(true);
        match result.kind() {
            ValueKind::Undefined => {}
            _ if crux::value::is_callable(&result) => current = result,
            _ => {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "method decorator returned a non-callable".into(),
                ));
            }
        }
    }
    let taken = std::mem::take(&mut *initializers.borrow_mut());
    Ok((current, taken))
}

/// Decorate a public field (spec: DecorateClassElement): call its decorators
/// with `undefined` and a field context, in reverse source order. A returned
/// callable is a value initializer applied to the field's value; the
/// `addInitializer` callbacks run after the field is set. Returns the two
/// lists, in registration order.
pub(crate) fn decorate_field(
    agent: &mut Agent,
    decorators: &[Expr],
    strict: bool,
    name: &ElementName,
    is_static: bool,
) -> Result<(Vec<Value>, Vec<Value>), JsError> {
    let decorators = evaluate_decorators(agent, decorators, strict)?;
    let initializers: InitializerList = Rc::new(RefCell::new(Vec::new()));
    let mut value_initializers = Vec::new();
    for decorator in decorators.iter().rev() {
        if !crux::value::is_callable(decorator) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "decorator must be callable".into(),
            ));
        }
        let finished = Rc::new(Cell::new(false));
        let context = element_context(
            agent,
            ElementKind::Field,
            name,
            is_static,
            &initializers,
            &finished,
        )?;
        let result = crate::function::call(
            agent,
            decorator,
            Value::Undefined,
            &[Value::Undefined, context],
        )?;
        finished.set(true);
        match result.kind() {
            ValueKind::Undefined => {}
            _ if crux::value::is_callable(&result) => value_initializers.push(result),
            _ => {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "field decorator returned a non-callable".into(),
                ));
            }
        }
    }
    let extras = std::mem::take(&mut *initializers.borrow_mut());
    Ok((value_initializers, extras))
}

/// A decorated auto-accessor's outcome: the (possibly replaced) get/set, the
/// optional value initializer for the backing storage, and the
/// `addInitializer` callbacks.
pub(crate) struct AccessorDecoration {
    pub get: Value,
    pub set: Value,
    pub init: Option<Value>,
    pub initializers: Vec<Value>,
}

/// Decorate a public auto-accessor (spec: DecorateClassElement, `kind:
/// "accessor"`): call its decorators with the `{ get, set }` pair and an
/// accessor context, in reverse source order. A returned object may carry
/// replacement `get`/`set` and an `init` value initializer.
pub(crate) fn decorate_accessor(
    agent: &mut Agent,
    decorators: &[Expr],
    strict: bool,
    name: &ElementName,
    is_static: bool,
    get: Value,
    set: Value,
) -> Result<AccessorDecoration, JsError> {
    let decorators = evaluate_decorators(agent, decorators, strict)?;
    let initializers: InitializerList = Rc::new(RefCell::new(Vec::new()));
    let mut current_get = get;
    let mut current_set = set;
    let mut current_init = None;
    for decorator in decorators.iter().rev() {
        if !crux::value::is_callable(decorator) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "decorator must be callable".into(),
            ));
        }
        let finished = Rc::new(Cell::new(false));
        let context = element_context(
            agent,
            ElementKind::Accessor,
            name,
            is_static,
            &initializers,
            &finished,
        )?;
        let value = accessor_value(agent, current_get, current_set)?;
        let result = crate::function::call(agent, decorator, Value::Undefined, &[value, context])?;
        finished.set(true);
        match result.kind() {
            ValueKind::Undefined => {}
            ValueKind::Object(_) => {
                if let Some(get) = accessor_result_property(agent, &result, "get")? {
                    current_get = get;
                }
                if let Some(set) = accessor_result_property(agent, &result, "set")? {
                    current_set = set;
                }
                if let Some(init) = accessor_result_property(agent, &result, "init")? {
                    current_init = Some(init);
                }
            }
            _ => {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "accessor decorator returned a non-object".into(),
                ));
            }
        }
    }
    let taken = std::mem::take(&mut *initializers.borrow_mut());
    Ok(AccessorDecoration {
        get: current_get,
        set: current_set,
        init: current_init,
        initializers: taken,
    })
}

/// The `{ get, set }` object an auto-accessor decorator receives.
fn accessor_value(agent: &mut Agent, get: Value, set: Value) -> Result<Value, JsError> {
    let object = JsObject::ordinary_object_create(object_prototype(agent)?);
    object.create_data_property_or_throw_key(&PropertyKey::from_utf8("get"), get)?;
    object.create_data_property_or_throw_key(&PropertyKey::from_utf8("set"), set)?;
    Ok(Value::Object(object))
}

/// Read one `get`/`set`/`init` member of an auto-accessor decorator's returned
/// object: `undefined` (absent) or a callable, else a TypeError.
fn accessor_result_property(
    agent: &mut Agent,
    result: &Value,
    key: &str,
) -> Result<Option<Value>, JsError> {
    let value = crate::context::get_property(agent, result, &JsString::from_utf8(key), *result)?;
    match value.kind() {
        ValueKind::Undefined => Ok(None),
        _ if crux::value::is_callable(&value) => Ok(Some(value)),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            format!("accessor {key} must be callable or undefined"),
        )),
    }
}

/// An element decorator's context object: `{ kind, name, static, private,
/// access, addInitializer }`.
fn element_context(
    agent: &mut Agent,
    kind: ElementKind,
    name: &ElementName,
    is_static: bool,
    initializers: &InitializerList,
    finished: &Rc<Cell<bool>>,
) -> Result<Value, JsError> {
    let object = JsObject::ordinary_object_create(object_prototype(agent)?);
    object.create_data_property_or_throw_key(
        &PropertyKey::from_utf8("kind"),
        Value::String(Handle::new(JsString::from_utf8(kind.as_str()))),
    )?;
    object
        .create_data_property_or_throw_key(&PropertyKey::from_utf8("name"), name.context_value())?;
    object.create_data_property_or_throw_key(
        &PropertyKey::from_utf8("static"),
        Value::Boolean(is_static),
    )?;
    object.create_data_property_or_throw_key(
        &PropertyKey::from_utf8("private"),
        Value::Boolean(name.is_private()),
    )?;

    let access = JsObject::ordinary_object_create(object_prototype(agent)?);
    let has = synthesize_access_function(agent, name, AccessOp::Has)?;
    access.create_data_property_or_throw_key(&PropertyKey::from_utf8("has"), has)?;
    let (wants_get, wants_set) = kind.access_members();
    if wants_get {
        let get = synthesize_access_function(agent, name, AccessOp::Get)?;
        access.create_data_property_or_throw_key(&PropertyKey::from_utf8("get"), get)?;
    }
    if wants_set {
        let set = synthesize_access_function(agent, name, AccessOp::Set)?;
        access.create_data_property_or_throw_key(&PropertyKey::from_utf8("set"), set)?;
    }
    object.create_data_property_or_throw_key(
        &PropertyKey::from_utf8("access"),
        Value::Object(access),
    )?;

    let add_initializer = add_initializer_function(agent, initializers, finished)?;
    object.create_data_property_or_throw_key(
        &PropertyKey::from_utf8("addInitializer"),
        add_initializer,
    )?;
    Ok(Value::Object(object))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::evaluate;

    fn run(source: &str) -> Result<Value, JsError> {
        evaluate(source)
    }

    fn string(value: &str) -> Value {
        Value::String(Handle::new(JsString::from_utf8(value)))
    }

    #[test]
    fn class_decorator_sees_a_class_shaped_context() {
        let result = run(
            "let seen;\n\
             function d(value, context) {\n\
               seen = context.kind + ':' + context.name + ':' + (context.access === undefined) + ':' + Object.keys(context).sort().join(',');\n\
             }\n\
             @d class C {}\n\
             seen;",
        )
        .unwrap();
        assert_eq!(result, string("class:C:true:addInitializer,kind,name"));
    }

    #[test]
    fn class_decorators_call_in_reverse_source_order() {
        let result = run("let log = [];\n\
             function a(value, context) { log.push('a'); }\n\
             function b(value, context) { log.push('b'); }\n\
             @a @b class C {}\n\
             log.join(',');")
        .unwrap();
        assert_eq!(result, string("b,a"));
    }

    #[test]
    fn class_decorator_return_replaces_the_class() {
        let result = run(
            "function d(value, context) { return class extends value { extra() { return 7; } }; }\n\
             @d class C {}\n\
             new C().extra();",
        )
        .unwrap();
        assert_eq!(result, Value::Number(7.0));
    }

    #[test]
    fn class_initializers_run_after_static_fields() {
        let result = run(
            "let seen;\n\
             function d(value, context) { context.addInitializer(function () { seen = this.x; }); }\n\
             @d class C { static x = 5; }\n\
             seen;",
        )
        .unwrap();
        assert_eq!(result, Value::Number(5.0));
    }

    #[test]
    fn anonymous_class_decorator_name_is_undefined() {
        let result = run("let seen;\n\
             function d(value, context) { seen = String(context.name); }\n\
             let C = @d class {};\n\
             seen;")
        .unwrap();
        assert_eq!(result, string("undefined"));
    }

    #[test]
    fn add_initializer_after_return_throws_type_error() {
        let result = run(
            "let late;\n\
             function d(value, context) { late = context.addInitializer; }\n\
             @d class C {}\n\
             let message;\n\
             try { late(function () {}); message = 'no-throw'; } catch (e) { message = e.constructor.name; }\n\
             message;",
        )
        .unwrap();
        assert_eq!(result, string("TypeError"));
    }

    #[test]
    fn non_callable_class_decorator_result_throws() {
        let result = run(
            "function d(value, context) { return 42; }\n\
             let message;\n\
             try { let C = @d class {}; message = 'no-throw'; } catch (e) { message = e.constructor.name; }\n\
             message;",
        )
        .unwrap();
        assert_eq!(result, string("TypeError"));
    }

    #[test]
    fn element_context_is_shaped_by_kind() {
        let result = run(
            "let seen = [];\n\
             function d(value, context) { seen.push(context.kind + ':' + context.name + ':' + Object.keys(context.access).sort().join(',')); return value; }\n\
             class C { @d m() {} @d get g() { return 1; } @d set s(v) {} @d f; }\n\
             seen.join(' ');",
        )
        .unwrap();
        assert_eq!(
            result,
            string("method:m:get,has getter:g:get,has setter:s:has,set field:f:get,has,set")
        );
    }

    #[test]
    fn method_decorator_return_replaces_the_method() {
        let result = run(
            "function d(value, context) { return function () { return 'wrapped'; }; }\n\
             class C { @d m() { return 'orig'; } }\n\
             new C().m();",
        )
        .unwrap();
        assert_eq!(result, string("wrapped"));
    }

    #[test]
    fn field_decorator_value_initializer_transforms_the_value() {
        let result = run(
            "function d(value, context) { return function (init) { return init + 1; }; }\n\
             class C { @d f = 41; }\n\
             new C().f;",
        )
        .unwrap();
        assert_eq!(result, Value::Number(42.0));
    }

    #[test]
    fn instance_method_initializer_runs_before_fields() {
        let result = run(
            "let order = [];\n\
             function d(value, context) { context.addInitializer(function () { order.push('init'); }); return value; }\n\
             class C { @d m() {} f = (order.push('field'), 1); }\n\
             new C();\n\
             order.join(',');",
        )
        .unwrap();
        assert_eq!(result, string("init,field"));
    }

    #[test]
    fn static_method_initializer_runs_before_static_fields() {
        let result = run(
            "let order = [];\n\
             function d(value, context) { context.addInitializer(function () { order.push('init'); }); return value; }\n\
             class C { @d static m() {} static f = (order.push('field'), 1); }\n\
             order.join(',');",
        )
        .unwrap();
        assert_eq!(result, string("init,field"));
    }

    #[test]
    fn access_get_and_set_use_the_property_protocol() {
        let result = run("let acc;\n\
             function d(value, context) { acc = context.access; return value; }\n\
             class C { @d get g() { return this.x; } }\n\
             let c = new C(); c.x = 5;\n\
             String(acc.get(c)) + ',' + acc.has(c);")
        .unwrap();
        assert_eq!(result, string("5,true"));
    }

    #[test]
    fn element_decorators_call_in_reverse_source_order() {
        let result = run("let log = [];\n\
             function a(value, context) { log.push('a'); return value; }\n\
             function b(value, context) { log.push('b'); return value; }\n\
             class C { @a @b m() {} }\n\
             log.join(',');")
        .unwrap();
        assert_eq!(result, string("b,a"));
    }

    #[test]
    fn non_callable_method_decorator_result_throws() {
        let result = run(
            "function d(value, context) { return 5; }\n\
             let message;\n\
             try { class C { @d m() {} } message = 'no-throw'; } catch (e) { message = e.constructor.name; }\n\
             message;",
        )
        .unwrap();
        assert_eq!(result, string("TypeError"));
    }

    #[test]
    fn private_element_context_uses_the_hash_name() {
        let result = run(
            "let seen = [];\n\
             function d(value, context) { seen.push(context.kind + ':' + context.name + ':' + context.private + ':' + Object.keys(context.access).sort().join(',')); return value; }\n\
             class C { @d #m() {} @d #f; }\n\
             seen.join(' ');",
        )
        .unwrap();
        assert_eq!(
            result,
            string("method:#m:true:get,has field:#f:true:get,has,set")
        );
    }

    #[test]
    fn private_field_access_reads_and_writes() {
        let result = run("let acc;\n\
             function d(value, context) { acc = context.access; return value; }\n\
             class C { @d #f = 41; get() { return this.#f; } }\n\
             let c = new C();\n\
             acc.set(c, 99);\n\
             acc.get(c) + ',' + c.get() + ',' + acc.has(c);")
        .unwrap();
        assert_eq!(result, string("99,99,true"));
    }

    #[test]
    fn private_method_access_checks_the_brand_and_reads_the_method() {
        let result = run("let acc;\n\
             function d(value, context) { acc = context.access; return value; }\n\
             class C { @d #m() { return 'M'; } call() { return this.#m(); } }\n\
             let c = new C();\n\
             acc.has(c) + ',' + acc.has({}) + ',' + acc.get(c).call(c);")
        .unwrap();
        assert_eq!(result, string("true,false,M"));
    }

    #[test]
    fn private_field_decorator_value_initializer_transforms() {
        let result = run(
            "function d(value, context) { return function (init) { return init * 2; }; }\n\
             class C { @d #f = 21; get() { return this.#f; } }\n\
             new C().get();",
        )
        .unwrap();
        assert_eq!(result, Value::Number(42.0));
    }

    #[test]
    fn private_method_decorator_return_replaces_the_method() {
        let result = run(
            "function d(value, context) { return function () { return 'W'; }; }\n\
             class C { @d #m() { return 'M'; } call() { return this.#m(); } }\n\
             new C().call();",
        )
        .unwrap();
        assert_eq!(result, string("W"));
    }

    #[test]
    fn accessor_decorator_sees_one_accessor_context() {
        let result = run(
            "let seen;\n\
             function d(value, context) { seen = context.kind + ':' + context.name + ':' + Object.keys(value).sort().join(',') + ':' + Object.keys(context.access).sort().join(','); return value; }\n\
             class C { @d accessor x = 1; }\n\
             seen;",
        )
        .unwrap();
        assert_eq!(result, string("accessor:x:get,set:get,has,set"));
    }

    #[test]
    fn accessor_decorator_wraps_get_set_and_init() {
        let result = run(
            "function d(value, context) {\n\
               const get = value.get, set = value.set;\n\
               return { get() { return 'G:' + get.call(this); }, set(v) { set.call(this, v * 2); }, init(v) { return v + 100; } };\n\
             }\n\
             class C { @d accessor x = 1; }\n\
             let c = new C();\n\
             let before = c.x;\n\
             c.x = 5;\n\
             before + ',' + c.x;",
        )
        .unwrap();
        assert_eq!(result, string("G:101,G:10"));
    }

    #[test]
    fn accessor_access_reads_and_writes() {
        let result = run("let acc;\n\
             function d(value, context) { acc = context.access; return value; }\n\
             class C { @d accessor x = 7; }\n\
             let c = new C();\n\
             let first = acc.get(c);\n\
             acc.set(c, 3);\n\
             acc.has(c) + ',' + first + ',' + c.x;")
        .unwrap();
        assert_eq!(result, string("true,7,3"));
    }

    #[test]
    fn private_auto_accessor_decorates_as_an_accessor() {
        let result = run(
            "let seen;\n\
             function d(value, context) { seen = context.kind + ':' + context.name + ':' + context.private + ':' + Object.keys(value).sort().join(',') + ':' + Object.keys(context.access).sort().join(','); return value; }\n\
             class C { @d accessor #x = 5; getX() { return this.#x; } }\n\
             new C().getX() + '|' + seen;",
        )
        .unwrap();
        assert_eq!(result, string("5|accessor:#x:true:get,set:get,has,set"));
    }

    #[test]
    fn private_auto_accessor_decorator_can_wrap_get_set() {
        let result = run(
            "function d(value, context) { const get = value.get, set = value.set; return { get() { return 'G:' + get.call(this); }, set(v) { set.call(this, v); } }; }\n\
             class C { @d accessor #x = 1; getX() { return this.#x; } setX(v) { this.#x = v; } }\n\
             let c = new C(); let before = c.getX(); c.setX(9); before + ',' + c.getX();",
        )
        .unwrap();
        assert_eq!(result, string("G:1,G:9"));
    }
}
