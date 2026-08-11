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
use crux::value::Value;
use syntax::ast::{
    ArrayBindingElement, BindingElement, BindingPattern, ObjectBindingProperty, PropertyName,
};

use crate::agent::Agent;
use crate::context::{get_property_key, put_value, resolve_binding};
use crate::env::EnvRef;
use crate::expr::{eval_expr, get_iterator, iterator_close, iterator_step};

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
            if matches!(value, Value::Undefined | Value::Null) {
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
            let array = array_from_values(args.get(index..).unwrap_or_default())?;
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
    let value = if matches!(value, Value::Undefined)
        && let Some(init) = &element.init
    {
        eval_expr(agent, init, strict)?
    } else {
        value
    };
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
    let mut value = get_property_key(agent, value, key, value.clone())?;
    if matches!(value, Value::Undefined)
        && let Some(init) = &element.init
    {
        value = eval_expr(agent, init, strict)?;
    }
    binding_initialization(agent, &element.pattern, value, env, strict)
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
    let rest_obj = JsObject::ordinary_object_create(None);
    copy_data_properties_excluding(&rest_obj, value, excluded)?;
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
/// order and collecting the rest element. The iterator is closed on every
/// completion (spec 13.2.3.4 steps 2-4).
fn bind_array_pattern(
    agent: &mut Agent,
    elements: &[ArrayBindingElement],
    value: Value,
    env: Option<&EnvRef>,
    strict: bool,
) -> Result<(), JsError> {
    let iterator = get_iterator(agent, &value)?;
    let result = (|| -> Result<(), JsError> {
        for element in elements {
            match element {
                ArrayBindingElement::Hole => {
                    iterator_step(agent, &iterator)?;
                }
                ArrayBindingElement::Element(element) => {
                    let next = iterator_step(agent, &iterator)?.unwrap_or(Value::Undefined);
                    bind_element(agent, element, next, env, strict)?;
                }
                ArrayBindingElement::Rest(element) => {
                    let mut collected = Vec::new();
                    while let Some(next) = iterator_step(agent, &iterator)? {
                        collected.push(next);
                    }
                    let array = array_from_values(&collected)?;
                    return bind_rest_element(agent, element, array, env, strict);
                }
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            iterator_close(agent, &iterator)?;
            Ok(())
        }
        Err(error) => {
            let close = iterator_close(agent, &iterator);
            if let Err(close_error) = close {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    format!("Iterator close failed: {}", close_error.message),
                ));
            }
            Err(error)
        }
    }
}

/// A fresh Array of the given values (ArrayCreate + CreateDataProperty).
fn array_from_values(values: &[Value]) -> Result<Value, JsError> {
    let array = JsObject::array_create(None, values.len() as f64)?;
    for (index, value) in values.iter().enumerate() {
        array.create_data_property(&JsString::from_utf8(&index.to_string()), value.clone())?;
    }
    Ok(Value::Object(array))
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

/// CopyDataProperties (spec 14.1.16) with an excluded-name list: copy the
/// enumerable own properties of `from` to `to`, skipping excluded keys and
/// keys `to` already has.
pub fn copy_data_properties_excluding(
    to: &crux::object::JsObject,
    from: &Value,
    excluded: &[PropertyKey],
) -> Result<(), JsError> {
    let Value::Object(from_obj) = from else {
        return Ok(());
    };
    for key in from_obj.own_property_keys()? {
        if excluded.contains(&key) || to.has_own_property_key(&key)? {
            continue;
        }
        let property = from_obj.get_own_property_key(&key)?;
        if let Some(property) = property {
            if !property.enumerable {
                continue;
            }
            let value = match property.kind {
                crux::object::PropertyKind::Data { value, .. } => value,
                crux::object::PropertyKind::Accessor { .. } => from_obj.get_key(&key)?,
            };
            to.create_data_property_key(&key, value)?;
        }
    }
    Ok(())
}
