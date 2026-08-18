//! Binding initialization (spec 13.2.3.x): BindingInitialization,
//! IteratorBindingInitialization, PropertyBindingInitialization, and
//! KeyedBindingInitialization for declaration and parameter bindings.
//!
//! `env` is `Some` for lexical declarations and parameters (the binding was
//! created uninitialized by declaration instantiation and is filled with
//! InitializeBinding); `None` for `var` statements, which resolve and PutValue
//! the hoisted binding instead (spec 13.2.3.4 note).

use crux::error::{ErrorKind, JsError};
use crux::object::JsObject;
use crux::property::PropertyKey;
use crux::string::JsString;
use crux::value::{Value, ValueKind};
use syntax::ast::{
    ArrayBindingElement, ArrayElement, ArrayLiteral, AssignOp, BindingElement, BindingPattern,
    Expr, ExprKind, ObjectBindingProperty, ObjectLiteral, ObjectProperty, PropertyName,
};

use crate::agent::Agent;
use crate::context::{Reference, get_property_key, put_value, resolve_binding};
use crate::env::EnvRef;
use crate::expr::{eval_expr, eval_reference, get_iterator, iterator_close, iterator_step};

/// BindingInitialization (spec 13.2.3.4): bind a pattern to a value.
pub fn binding_initialization(
    agent: &mut Agent,
    pattern: &BindingPattern,
    value: Value,
    env: Option<&EnvRef>,
    strict: bool,
) -> Result<(), JsError> {
    match pattern {
        BindingPattern::Ident(name) => {
            let name = crux::lookup(*name);
            initialize_bound_name(agent, &name, value, env, strict)
        }
        BindingPattern::Object(props) => {
            if matches!(value.kind(), ValueKind::Undefined | ValueKind::Null) {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "Cannot destructure null or undefined".into(),
                ));
            }
            bind_object_pattern(agent, props, &value, env, strict)
        }
        BindingPattern::Array(elements) => bind_array_pattern(agent, elements, value, env, strict),
    }
}

/// IteratorBindingInitialization of a parameter list (spec 13.2.3.5): bind
/// each parameter in order, with defaults, patterns, and a final rest
/// parameter collecting the remaining arguments.
pub fn iterator_binding_initialization(
    agent: &mut Agent,
    params: &[BindingElement],
    args: &[Value],
    env: Option<&EnvRef>,
    strict: bool,
) -> Result<(), JsError> {
    for (index, param) in params.iter().enumerate() {
        if param.rest {
            let array = crate::builtins::array::array_from_values(
                agent,
                args.get(index..).unwrap_or_default(),
            )?;
            return bind_rest_element(agent, param, array, env, strict);
        }
        let value = args.get(index).cloned().unwrap_or(Value::Undefined);
        bind_element(agent, param, value, env, strict)?;
    }
    Ok(())
}

/// InitializeBoundName (spec 13.2.3.4): fill a pre-created binding, or
/// resolve and PutValue when the caller created no binding (var statements).
fn initialize_bound_name(
    agent: &mut Agent,
    name: &JsString,
    value: Value,
    env: Option<&EnvRef>,
    strict: bool,
) -> Result<(), JsError> {
    match env {
        Some(env) => env.initialize_binding(name, value),
        None => {
            let reference = resolve_binding(agent, name, strict)?;
            put_value(agent, &reference, value)
        }
    }
}

/// Bind one binding element to a value, applying the default initializer when
/// the value is *undefined* (spec 13.2.3.5 SingleNameBinding/BindingElement).
fn bind_element(
    agent: &mut Agent,
    element: &BindingElement,
    value: Value,
    env: Option<&EnvRef>,
    strict: bool,
) -> Result<(), JsError> {
    let name = binding_ident_name(&element.pattern);
    let value = apply_element_default(agent, value, element.init.as_ref(), name.as_ref(), strict)?;
    match &element.pattern {
        BindingPattern::Ident(name) => {
            let name = crux::lookup(*name);
            initialize_bound_name(agent, &name, value, env, strict)
        }
        pattern => binding_initialization(agent, pattern, value, env, strict),
    }
}

/// Bind a rest element (array/parameter rest) to a materialized array.
fn bind_rest_element(
    agent: &mut Agent,
    element: &BindingElement,
    array: Value,
    env: Option<&EnvRef>,
    strict: bool,
) -> Result<(), JsError> {
    match &element.pattern {
        BindingPattern::Ident(name) => {
            let name = crux::lookup(*name);
            initialize_bound_name(agent, &name, array, env, strict)
        }
        pattern => binding_initialization(agent, pattern, array, env, strict),
    }
}

/// ObjectBindingPattern: KeyedBindingInitialization for each property, then
/// the rest property collects the unbound enumerable own keys.
fn bind_object_pattern(
    agent: &mut Agent,
    props: &[ObjectBindingProperty],
    value: &Value,
    env: Option<&EnvRef>,
    strict: bool,
) -> Result<(), JsError> {
    let mut excluded: Vec<PropertyKey> = Vec::new();
    for prop in props {
        match prop {
            ObjectBindingProperty::Property { key, element, .. } => {
                let key = property_name_to_key(agent, key, strict)?;
                keyed_binding_initialization(agent, element, value, &key, env, strict)?;
                excluded.push(key);
            }
            ObjectBindingProperty::Rest(element) => {
                rest_binding_initialization(agent, element, value, &excluded, env, strict)?;
            }
        }
    }
    Ok(())
}

/// KeyedBindingInitialization (spec 13.2.3.7): bind an element to a property
/// of `value`, applying the default when the property is *undefined*.
fn keyed_binding_initialization(
    agent: &mut Agent,
    element: &BindingElement,
    value: &Value,
    key: &PropertyKey,
    env: Option<&EnvRef>,
    strict: bool,
) -> Result<(), JsError> {
    // SingleNameBinding resolves its target before GetV (spec 13.2.3.7
    // steps 1-3): the binding's observable resolution (a `with` object's
    // HasBinding, a Proxy `has` trap) precedes the property read.
    // A nested pattern has no lhs of its own; the bind happens inside it.
    let resolved = match &element.pattern {
        BindingPattern::Ident(atom) => {
            let name = crux::lookup(*atom);
            match env {
                Some(env) => Some(ResolvedTarget::Initialize(env.clone(), name)),
                None => Some(ResolvedTarget::Reference(resolve_binding(
                    agent, &name, strict,
                )?)),
            }
        }
        _ => None,
    };
    let value = get_property_key(agent, value, key, value.clone())?;
    let name = binding_ident_name(&element.pattern);
    let value = apply_element_default(agent, value, element.init.as_ref(), name.as_ref(), strict)?;
    match resolved {
        Some(ResolvedTarget::Initialize(env, name)) => env.initialize_binding(&name, value),
        Some(ResolvedTarget::Reference(reference)) => put_value(agent, &reference, value),
        None => binding_initialization(agent, &element.pattern, value, env, strict),
    }
}

/// The pre-resolved write target of a keyed SingleNameBinding.
enum ResolvedTarget {
    /// InitializeReferencedBinding of a pre-created lexical binding.
    Initialize(EnvRef, JsString),
    /// PutValue of the resolved `var`/assignment reference.
    Reference(Reference),
}

/// RestBindingInitialization (spec 13.2.3.6): the remaining enumerable own
/// properties, minus the bound names, become a fresh object.
fn rest_binding_initialization(
    agent: &mut Agent,
    element: &BindingElement,
    value: &Value,
    excluded: &[PropertyKey],
    env: Option<&EnvRef>,
    strict: bool,
) -> Result<(), JsError> {
    // CopyDataProperties (spec 13.2.3.6): the rest object is an ordinary
    // object with %Object.prototype% as its prototype.
    let rest_obj = rest_object(agent)?;
    copy_data_properties_excluding(agent, &rest_obj, value, excluded)?;
    let BindingPattern::Ident(name) = &element.pattern else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Object rest must be a binding identifier".into(),
        ));
    };
    let name = crux::lookup(*name);
    initialize_bound_name(agent, &name, Value::Object(rest_obj), env, strict)
}

/// ArrayBindingPattern: consume the iterator, binding elements and holes in
/// order and collecting the rest element. IteratorClose runs when the pattern
/// ends without exhausting the iterator; an abrupt iterator step marks the
/// iterator done so `return` is not called (spec 13.2.3.4).
fn bind_array_pattern(
    agent: &mut Agent,
    elements: &[ArrayBindingElement],
    value: Value,
    env: Option<&EnvRef>,
    strict: bool,
) -> Result<(), JsError> {
    let iterator = get_iterator(agent, &value)?;
    let mut done = false;
    let result = (|| -> Result<(), JsError> {
        for element in elements {
            match element {
                ArrayBindingElement::Hole => match iterator_step(agent, &iterator) {
                    Ok(None) => done = true,
                    Ok(Some(_)) => {}
                    Err(error) => {
                        done = true;
                        return Err(error);
                    }
                },
                ArrayBindingElement::Element(element) => match iterator_step(agent, &iterator) {
                    Ok(Some(next)) => bind_element(agent, element, next, env, strict)?,
                    Ok(None) => {
                        done = true;
                        bind_element(agent, element, Value::Undefined, env, strict)?;
                    }
                    Err(error) => {
                        done = true;
                        return Err(error);
                    }
                },
                ArrayBindingElement::Rest(element) => {
                    let mut collected = Vec::new();
                    loop {
                        match iterator_step(agent, &iterator) {
                            Ok(Some(next)) => collected.push(next),
                            Ok(None) => {
                                done = true;
                                break;
                            }
                            Err(error) => {
                                done = true;
                                return Err(error);
                            }
                        }
                    }
                    let array = crate::builtins::array::array_from_values(agent, &collected)?;
                    bind_rest_element(agent, element, array, env, strict)?;
                }
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            // IteratorClose only when the iterator was not exhausted by the
            // pattern (a done-first iterator must not have its `return`
            // called).
            if done {
                Ok(())
            } else {
                iterator_close(agent, &iterator)
            }
        }
        Err(error) => {
            if done {
                return Err(error);
            }
            // An error from the pattern body closes the not-done iterator
            // once; a throwing `return` replaces the error (spec 7.4.11).
            crate::expr::iterator_close_throw(agent, &iterator)?;
            Err(error)
        }
    }
}

/// Evaluation of a PropertyName in a binding pattern (spec 13.2.5.5).
fn property_name_to_key(
    agent: &mut Agent,
    name: &PropertyName,
    strict: bool,
) -> Result<PropertyKey, JsError> {
    match name {
        PropertyName::Ident(id) => Ok(PropertyKey::String(*id)),
        PropertyName::Str(text) => Ok(PropertyKey::from_js_string(text)),
        PropertyName::Number(n) => Ok(PropertyKey::from_js_string(&crux::convert::to_string(
            &Value::Number(*n),
        )?)),
        PropertyName::Computed(expr) => {
            let value = eval_expr(agent, expr, strict)?;
            crux::convert::to_property_key(&value)
        }
    }
}

/// A fresh ordinary object with %Object.prototype% as its prototype, for
/// object rest patterns (CopyDataProperties, spec 7.3.25).
pub(crate) fn rest_object(agent: &Agent) -> Result<crux::handle::Handle<JsObject>, JsError> {
    let proto = agent
        .current_realm()?
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| crate::context::as_object(&value));
    Ok(JsObject::ordinary_object_create(proto))
}

/// CopyDataProperties (spec 14.1.16) with an excluded-name list: copy the
/// enumerable own properties of `from` to `to`, skipping excluded keys.
/// `null`/`undefined` contribute nothing; other primitives are boxed first
/// (a String contributes its index properties).
pub fn copy_data_properties_excluding(
    agent: &mut Agent,
    to: &crux::object::JsObject,
    from: &Value,
    excluded: &[PropertyKey],
) -> Result<(), JsError> {
    if matches!(from.kind(), ValueKind::Null | ValueKind::Undefined) {
        return Ok(());
    }
    let ValueKind::Object(from_obj) = crate::context::to_object(agent, from)?.kind() else {
        return Ok(());
    };
    for key in from_obj.own_property_keys()? {
        if excluded.contains(&key) {
            continue;
        }
        let property = from_obj.get_own_property_key(&key)?;
        if let Some(property) = property {
            if !property.enumerable {
                continue;
            }
            // CopyDataProperties reads the value with Get after the
            // enumerable check (spec 7.3.25 step 6.c.i-ii): for proxies this
            // is what invokes the `get` trap, and the descriptor's data value
            // would be a trap artifact.
            let value = from_obj.get_key(&key)?;
            to.create_data_property_key(&key, value)?;
        }
    }
    Ok(())
}

/// DestructuringAssignmentEvaluation (spec 13.15.4): assign to the targets
/// of an Array/Object assignment pattern from `value`. Unlike
/// `binding_initialization` (which fills pre-created environment bindings),
/// the writes go through references (PutValue); the target expressions are
/// evaluated in order as part of the pattern.
pub fn destructuring_assignment(
    agent: &mut Agent,
    target: &Expr,
    value: Value,
    strict: bool,
) -> Result<(), JsError> {
    match &target.kind {
        ExprKind::Array(lit) => array_assignment(agent, lit, value, strict),
        ExprKind::Object(lit) => object_assignment(agent, lit, value, strict),
        ExprKind::Paren(inner) => destructuring_assignment(agent, inner, value, strict),
        _ => {
            let reference = eval_reference(agent, target, strict)?;
            put_value(agent, &reference, value)
        }
    }
}

/// Write `value` to a single assignment target: a nested Array/Object pattern
/// recurses, anything else is a reference (with a lazily-converted computed
/// key, spec 13.15.4.2).
fn assign_target(
    agent: &mut Agent,
    target: &Expr,
    value: Value,
    strict: bool,
) -> Result<(), JsError> {
    match &target.kind {
        ExprKind::Array(lit) => array_assignment(agent, lit, value, strict),
        ExprKind::Object(lit) => object_assignment(agent, lit, value, strict),
        ExprKind::Paren(inner) => assign_target(agent, inner, value, strict),
        _ => {
            let reference = crate::expr::eval_assignment_target(agent, target, strict)?;
            reference.put(agent, value)
        }
    }
}

/// ArrayAssignmentPattern (spec 13.15.5.2): consume the RHS iterator, assign
/// elements and holes in order, and assign the rest element a fresh array of
/// the remaining values. IteratorClose runs when the pattern ends without
/// exhausting the iterator, and on errors from reference evaluation or
/// PutValue; an abrupt iterator step marks the iterator done so `return` is
/// not called (spec 13.15.5.5).
fn array_assignment(
    agent: &mut Agent,
    lit: &ArrayLiteral,
    value: Value,
    strict: bool,
) -> Result<(), JsError> {
    let iterator = get_iterator(agent, &value)?;
    let mut done = false;
    let result = (|| -> Result<(), JsError> {
        for element in &lit.elements {
            match element {
                ArrayElement::Hole => match iterator_step(agent, &iterator) {
                    Ok(None) => done = true,
                    Ok(Some(_)) => {}
                    Err(error) => {
                        done = true;
                        return Err(error);
                    }
                },
                ArrayElement::Expr(expr) => {
                    // A simple target's reference is evaluated before the
                    // iterator steps (spec 13.15.5.5 note); an abrupt
                    // reference leaves the iterator open so the close below
                    // runs. A nested pattern steps in its place instead.
                    let (inner, init) = match &expr.kind {
                        ExprKind::Assign {
                            op: AssignOp::Assign,
                            target,
                            value: initializer,
                        } => (target.as_ref(), Some(initializer.as_ref())),
                        _ => (expr, None),
                    };
                    let nested = matches!(&inner.kind, ExprKind::Array(_) | ExprKind::Object(_));
                    let reference = if nested {
                        None
                    } else {
                        Some(crate::expr::eval_assignment_target(agent, inner, strict)?)
                    };
                    let next = match iterator_step(agent, &iterator) {
                        Ok(Some(next)) => next,
                        Ok(None) => {
                            done = true;
                            Value::Undefined
                        }
                        Err(error) => {
                            done = true;
                            return Err(error);
                        }
                    };
                    let name = match &inner.kind {
                        ExprKind::Ident(id) => Some(crux::lookup(*id)),
                        _ => None,
                    };
                    let next = apply_element_default(agent, next, init, name.as_ref(), strict)?;
                    match reference {
                        Some(reference) => reference.put(agent, next)?,
                        None => assign_target(agent, inner, next, strict)?,
                    }
                }
                ArrayElement::Spread(expr) => {
                    // The rest target's reference is evaluated before the
                    // remaining values are collected (spec 13.15.5.5); an
                    // abrupt reference leaves the iterator open.
                    let nested = matches!(&expr.kind, ExprKind::Array(_) | ExprKind::Object(_));
                    let reference = if nested {
                        None
                    } else {
                        Some(crate::expr::eval_assignment_target(agent, expr, strict)?)
                    };
                    let mut collected = Vec::new();
                    loop {
                        match iterator_step(agent, &iterator) {
                            Ok(Some(next)) => collected.push(next),
                            Ok(None) => {
                                done = true;
                                break;
                            }
                            Err(error) => {
                                done = true;
                                return Err(error);
                            }
                        }
                    }
                    let array = crate::builtins::array::array_from_values(agent, &collected)?;
                    match reference {
                        Some(reference) => reference.put(agent, array)?,
                        None => assign_target(agent, expr, array, strict)?,
                    }
                }
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            // IteratorClose only when the iterator was not exhausted by the
            // pattern; an empty pattern always closes (spec 13.15.5.2).
            if done {
                Ok(())
            } else {
                iterator_close(agent, &iterator)
            }
        }
        Err(error) => {
            if done {
                return Err(error);
            }
            // An error from the pattern body closes the not-done iterator
            // once; a throwing `return` replaces the error (spec 7.4.11).
            crate::expr::iterator_close_throw(agent, &iterator)?;
            Err(error)
        }
    }
}

/// ObjectAssignmentPattern (spec 13.15.4.2): RequireObjectCoercible, assign
/// each property (defaults when the property is *undefined*), then the rest
/// property collects the unbound enumerable own keys.
fn object_assignment(
    agent: &mut Agent,
    lit: &ObjectLiteral,
    value: Value,
    strict: bool,
) -> Result<(), JsError> {
    if matches!(value.kind(), ValueKind::Undefined | ValueKind::Null) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot destructure null or undefined".into(),
        ));
    }
    let mut excluded: Vec<PropertyKey> = Vec::new();
    for prop in &lit.props {
        match prop {
            ObjectProperty::Init {
                key, value: target, ..
            } => {
                let key = property_name_to_key(agent, key, strict)?;
                // KeyedDestructuringAssignmentEvaluation (spec 13.15.4.2):
                // the target reference is evaluated before the property value
                // is read, with its computed key conversion deferred to
                // PutValue.
                let (inner, init) = match &target.kind {
                    ExprKind::Assign {
                        op: AssignOp::Assign,
                        target,
                        value: initializer,
                    } => (target.as_ref(), Some(initializer.as_ref())),
                    _ => (target, None),
                };
                let nested = matches!(&inner.kind, ExprKind::Array(_) | ExprKind::Object(_));
                let lazy = if nested {
                    None
                } else {
                    Some(crate::expr::eval_assignment_target(agent, inner, strict)?)
                };
                let prop_value = get_property_key(agent, &value, &key, value.clone())?;
                let name = match &inner.kind {
                    ExprKind::Ident(id) => Some(crux::lookup(*id)),
                    _ => None,
                };
                let rhs = apply_element_default(agent, prop_value, init, name.as_ref(), strict)?;
                match lazy {
                    Some(lazy) => lazy.put(agent, rhs)?,
                    None => assign_target(agent, inner, rhs, strict)?,
                }
                excluded.push(key);
            }
            ObjectProperty::Spread(expr) => {
                let rest = rest_object(agent)?;
                copy_data_properties_excluding(agent, &rest, &value, &excluded)?;
                assign_target(agent, expr, Value::Object(rest), strict)?;
            }
            _ => {
                return Err(JsError::new(
                    ErrorKind::SyntaxError,
                    "Invalid destructuring assignment target".into(),
                ));
            }
        }
    }
    Ok(())
}

/// Apply an element's default initializer when `value` is *undefined*, then
/// the spec's name-inference rule: an anonymous-function default assigned to
/// an identifier target takes the identifier as its `name` (13.2.3.5/13.2.3.7,
/// 13.15.4.1).
fn apply_element_default(
    agent: &mut Agent,
    value: Value,
    init: Option<&Expr>,
    name: Option<&JsString>,
    strict: bool,
) -> Result<Value, JsError> {
    let used_default = matches!(value.kind(), ValueKind::Undefined) && init.is_some();
    let value = if used_default {
        eval_expr(agent, init.expect("checked"), strict)?
    } else {
        value
    };
    if used_default
        && let Some(name) = name
        && let Some(init) = init
        && crate::function::is_anonymous_function_definition(init)
    {
        crate::function::set_function_name(&value, name, None)?;
    }
    Ok(value)
}

/// The bound name of an identifier pattern, if the pattern is one.
fn binding_ident_name(pattern: &BindingPattern) -> Option<JsString> {
    match pattern {
        BindingPattern::Ident(id) => Some(crux::lookup(*id)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::evaluate;

    #[test]
    fn var_bindings_put_through_resolution() {
        // `var`-style binding initialization resolves the hoisted binding and
        // PutValues the initializer (spec 13.2.3.4 note) — patterns land on
        // the pre-created binding.
        assert_eq!(
            evaluate("var [a, b] = [1, 2]; a + b").unwrap(),
            Value::Number(3.0)
        );
        assert_eq!(
            evaluate("var { p, q } = { p: 5, q: 6 }; p + q").unwrap(),
            Value::Number(11.0)
        );
    }

    #[test]
    fn lexical_bindings_initialize_in_place() {
        // Lexical declarations pass the pre-created environment; the binding
        // is filled with InitializeBinding (spec 13.2.3.4 step 2).
        assert_eq!(evaluate("let [x] = [7]; x").unwrap(), Value::Number(7.0));
        assert_eq!(
            evaluate("const { y } = { y: 9 }; y").unwrap(),
            Value::Number(9.0)
        );
    }

    #[test]
    fn rest_binding_excludes_bound_names() {
        assert_eq!(
            evaluate("let { a, ...rest } = { a: 1, b: 2, c: 3 }; rest.b + rest.c").unwrap(),
            Value::Number(5.0)
        );
        assert_eq!(
            evaluate("let [first, ...tail] = [1, 2, 3, 4]; tail.length").unwrap(),
            Value::Number(3.0)
        );
    }

    #[test]
    fn binding_patterns_respect_tdz() {
        // The pattern's value is evaluated before the binding initializes, so
        // a self-referencing element hits the TDZ.
        let err = evaluate("let [x] = [y]; let y = 1;").unwrap_err();
        assert_eq!(err.kind, ErrorKind::ReferenceError);
        // A default initializer for an element of the same binding runs while
        // the binding is still uninitialized.
        let err = evaluate("let { a = a } = {};").unwrap_err();
        assert_eq!(err.kind, ErrorKind::ReferenceError);
    }
}
