//! Decorator evaluation (the stage-3 decorators proposal): calling decorators
//! with a context object and applying their return values.
//!
//! Class decorators are implemented here; element decorators reuse the same
//! context/initializer machinery as they land.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crux::Function;
use crux::error::{ErrorKind, JsError};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::PropertyKey;
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use syntax::ast::Expr;

use crate::agent::Agent;

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
}
