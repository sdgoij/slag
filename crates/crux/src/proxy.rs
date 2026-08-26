//! Proxy exotic objects (spec 10.5): every essential internal method is
//! implemented by an ECMAScript handler method, with the invariants of the
//! internal-method table enforced after each trap.
//!
//! Callable and constructible proxies (target is a function value) are
//! supported: `call`/`construct` dispatch on `Value::Object` proxies through
//! the apply/construct traps, and `is_callable`/`is_constructor`/`type_of`
//! report them accordingly.

use std::cell::RefCell;

use crate::error::{ErrorKind, JsError};
use crate::function::call as value_call;
use crate::function::construct as value_construct;
use crate::handle::Handle;
use crate::heap::{GcAny, Trace};
use crate::object::{
    JsObject, Property, value_define_property, value_delete, value_get, value_get_method,
    value_get_own_property, value_get_prototype_of, value_has_property, value_is_extensible,
    value_own_property_keys, value_prevent_extensions, value_set, value_set_prototype_of,
};
use crate::ops::same_value;
use crate::property::{
    PropertyDescriptor, PropertyKey, from_property_descriptor, to_property_descriptor,
};
use crate::string::JsString;
use crate::value::{Value, is_constructor};

/// Creates the `argumentsList` array passed to a proxy trap (spec 7.3.15
/// CreateArrayFromList): the runtime provides the current realm's
/// `%Array.prototype%` through this hook; the null-prototype fallback covers
/// crux-only contexts (unit tests).
type ArrayFromListHook = fn(agent: *mut (), list: &[Value]) -> Result<Value, JsError>;
static ARRAY_FROM_LIST_HOOK: std::sync::OnceLock<ArrayFromListHook> = std::sync::OnceLock::new();

/// Install the CreateArrayFromList provider (the runtime does this once at
/// startup, like the ECMAScript executor hook).
pub fn install_array_from_list_hook(hook: ArrayFromListHook) {
    let _ = ARRAY_FROM_LIST_HOOK.set(hook);
}

/// The [[ProxyTarget]] and [[ProxyHandler]] internal slots. Both are `None`
/// once the proxy has been revoked (spec 10.5). `callable`/`constructible`
/// record the target's callability at creation (ProxyCreate, spec 10.5.15
/// steps 10-11): revocation clears the slots but the proxy keeps its
/// [[Call]]/[[Construct]] internal methods, so `typeof` and IsConstructor
/// must not follow the (now empty) target.
#[derive(Debug, Clone)]
pub struct ProxySlots {
    pub target: RefCell<Option<Value>>,
    pub handler: RefCell<Option<Value>>,
    pub callable: std::cell::Cell<bool>,
    pub constructible: std::cell::Cell<bool>,
}

impl Trace for ProxySlots {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        // The cells are RefCells: `RefCell<T>`'s trace skips a cell that is
        // mutably borrowed mid-collection (per-allocation `--gc-stress`) and
        // aborts the sweep instead of panicking.
        self.target.trace(visit);
        self.handler.trace(visit);
    }
}

fn revoked_error() -> JsError {
    JsError::new(
        ErrorKind::TypeError,
        "Cannot perform operation on a revoked Proxy".into(),
    )
}

/// ValidateNonRevokedProxy (spec 10.5.13): both slots must still be set.
fn resolved(slots: &ProxySlots) -> Result<(Value, Value), JsError> {
    let target = slots.target.borrow().clone().ok_or_else(revoked_error)?;
    let handler = slots.handler.borrow().clone().ok_or_else(revoked_error)?;
    Ok((target, handler))
}

/// Revoke the proxy (used by the future `Proxy.revocable` built-in): both
/// internal slots are cleared and every internal method throws.
pub fn revoke(slots: &ProxySlots) {
    *slots.target.borrow_mut() = None;
    *slots.handler.borrow_mut() = None;
}

fn trap_key(name: &str) -> JsString {
    JsString::from_utf8(name)
}

/// ProxyCreate (spec 10.5.14). Callable targets make a callable (and, when
/// the target is a constructor, constructible) proxy.
pub fn proxy_create(target: Value, handler: Value) -> Result<Handle<JsObject>, JsError> {
    if !target.is_object() && !target.is_function() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Proxy target must be an object".into(),
        ));
    }
    if !handler.is_object() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Proxy handler must be an object".into(),
        ));
    }
    // ProxyCreate (spec 10.5.15 steps 10-11): callability is fixed at
    // creation, before the target is stored.
    let callable = crate::value::is_callable(&target);
    let constructible = is_constructor(&target);
    let proxy = JsObject::proxy_object_create(ProxySlots {
        target: RefCell::new(Some(target)),
        handler: RefCell::new(Some(handler)),
        callable: std::cell::Cell::new(callable),
        constructible: std::cell::Cell::new(constructible),
    });
    Ok(proxy)
}

/// [[GetPrototypeOf]] (spec 10.5.1).
pub fn get_prototype_of(slots: &ProxySlots) -> Result<Option<Handle<JsObject>>, JsError> {
    let (target, handler) = resolved(slots)?;
    let Some(trap) = value_get_method(&handler, &trap_key("getPrototypeOf"))? else {
        return value_get_prototype_of(&target);
    };
    let handler_proto = value_call(&trap, handler, std::slice::from_ref(&target))?;
    if !handler_proto.is_object() && !handler_proto.is_null() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "getPrototypeOf trap must return an object or null".into(),
        ));
    }
    if value_is_extensible(&target)? {
        return Ok(handler_proto.as_object());
    }
    let target_proto = value_get_prototype_of(&target)?;
    let target_proto = match target_proto {
        Some(proto) => Value::Object(proto),
        None => Value::Null,
    };
    if !same_value(&handler_proto, &target_proto) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "getPrototypeOf trap returned a different prototype for a non-extensible target".into(),
        ));
    }
    Ok(handler_proto.as_object())
}

/// [[SetPrototypeOf]] (spec 10.5.2).
pub fn set_prototype_of(
    slots: &ProxySlots,
    proto: Option<Handle<JsObject>>,
) -> Result<bool, JsError> {
    let (target, handler) = resolved(slots)?;
    let Some(trap) = value_get_method(&handler, &trap_key("setPrototypeOf"))? else {
        return value_set_prototype_of(&target, proto);
    };
    let proto = match proto {
        Some(proto) => Value::Object(proto),
        None => Value::Null,
    };
    let result = value_call(&trap, handler, &[target.clone(), proto.clone()])?;
    if !crate::convert::to_boolean(&result) {
        return Ok(false);
    }
    if value_is_extensible(&target)? {
        return Ok(true);
    }
    let target_proto = value_get_prototype_of(&target)?;
    let target_proto = match target_proto {
        Some(proto) => Value::Object(proto),
        None => Value::Null,
    };
    if !same_value(&proto, &target_proto) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "setPrototypeOf trap cannot change the prototype of a non-extensible target".into(),
        ));
    }
    Ok(true)
}

/// [[IsExtensible]] (spec 10.5.3).
pub fn is_extensible(slots: &ProxySlots) -> Result<bool, JsError> {
    let (target, handler) = resolved(slots)?;
    let Some(trap) = value_get_method(&handler, &trap_key("isExtensible"))? else {
        return value_is_extensible(&target);
    };
    let result = value_call(&trap, handler, std::slice::from_ref(&target))?;
    let trap_result = crate::convert::to_boolean(&result);
    let target_result = value_is_extensible(&target)?;
    if trap_result != target_result {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "isExtensible trap must agree with the target".into(),
        ));
    }
    Ok(trap_result)
}

/// [[PreventExtensions]] (spec 10.5.4).
pub fn prevent_extensions(slots: &ProxySlots) -> Result<bool, JsError> {
    let (target, handler) = resolved(slots)?;
    let Some(trap) = value_get_method(&handler, &trap_key("preventExtensions"))? else {
        return value_prevent_extensions(&target);
    };
    let result = value_call(&trap, handler, std::slice::from_ref(&target))?;
    let trap_result = crate::convert::to_boolean(&result);
    if trap_result && value_is_extensible(&target)? {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "preventExtensions trap cannot report true for an extensible target".into(),
        ));
    }
    Ok(trap_result)
}

/// [[GetOwnProperty]] (spec 10.5.5).
pub fn get_own_property(
    slots: &ProxySlots,
    key: &PropertyKey,
) -> Result<Option<Property>, JsError> {
    let (target, handler) = resolved(slots)?;
    let Some(trap) = value_get_method(&handler, &trap_key("getOwnPropertyDescriptor"))? else {
        return value_get_own_property(&target, key);
    };
    let trap_result = value_call(&trap, handler, &[target.clone(), key_value(key)])?;
    if !trap_result.is_object() && !trap_result.is_undefined() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "getOwnPropertyDescriptor trap must return an object or undefined".into(),
        ));
    }
    let target_desc = value_get_own_property(&target, key)?;
    if trap_result.is_undefined() {
        if let Some(target_desc) = &target_desc {
            if !target_desc.configurable {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "getOwnPropertyDescriptor trap reported a non-configurable target property as absent".into(),
                ));
            }
            if !value_is_extensible(&target)? {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "getOwnPropertyDescriptor trap reported an own property as absent on a non-extensible target".into(),
                ));
            }
        }
        return Ok(None);
    }
    let mut result_desc = to_property_descriptor(&trap_result)?;
    result_desc.complete();
    let extensible_target = value_is_extensible(&target)?;
    if !crate::object::is_compatible_property_descriptor(
        extensible_target,
        &result_desc,
        target_desc.as_ref(),
    )? {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "getOwnPropertyDescriptor trap result is incompatible with the target property".into(),
        ));
    }
    if result_desc.configurable == Some(false) {
        let target_non_configurable = match &target_desc {
            Some(desc) => !desc.configurable,
            None => false,
        };
        if !target_non_configurable {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "getOwnPropertyDescriptor trap reported a non-configurable property that is configurable on the target".into(),
            ));
        }
        if result_desc.writable == Some(false)
            && let Some(target_desc) = &target_desc
            && target_desc.writable() == Some(true)
        {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "getOwnPropertyDescriptor trap reported a non-configurable, non-writable property that is writable on the target".into(),
            ));
        }
    }
    Property::from_descriptor(&result_desc)
        .map(Some)
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::TypeError,
                "getOwnPropertyDescriptor trap produced an invalid descriptor".into(),
            )
        })
}

/// [[DefineOwnProperty]] (spec 10.5.6).
pub fn define_own_property(
    slots: &ProxySlots,
    key: &PropertyKey,
    desc: &PropertyDescriptor,
) -> Result<bool, JsError> {
    let (target, handler) = resolved(slots)?;
    let Some(trap) = value_get_method(&handler, &trap_key("defineProperty"))? else {
        return value_define_property(&target, key, desc);
    };
    let desc_obj = from_property_descriptor(desc, None)?;
    let result = value_call(&trap, handler, &[target.clone(), key_value(key), desc_obj])?;
    if !crate::convert::to_boolean(&result) {
        return Ok(false);
    }
    let target_desc = value_get_own_property(&target, key)?;
    let extensible_target = value_is_extensible(&target)?;
    let setting_config_false = desc.configurable == Some(false);
    match &target_desc {
        None => {
            if !extensible_target {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "defineProperty trap cannot add a property to a non-extensible target".into(),
                ));
            }
            if setting_config_false {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "defineProperty trap cannot add a non-configurable property".into(),
                ));
            }
        }
        Some(target_desc) => {
            if !crate::object::is_compatible_property_descriptor(
                extensible_target,
                desc,
                Some(target_desc),
            )? {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "defineProperty trap descriptor is incompatible with the target property"
                        .into(),
                ));
            }
            if setting_config_false && target_desc.configurable {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "defineProperty trap cannot make a configurable target property non-configurable".into(),
                ));
            }
            if target_desc.is_data()
                && !target_desc.configurable
                && target_desc.writable() == Some(true)
                && desc.writable == Some(false)
            {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "defineProperty trap cannot make a writable target property non-writable"
                        .into(),
                ));
            }
        }
    }
    Ok(true)
}

/// [[HasProperty]] (spec 10.5.7).
pub fn has_property(slots: &ProxySlots, key: &PropertyKey) -> Result<bool, JsError> {
    let (target, handler) = resolved(slots)?;
    let Some(trap) = value_get_method(&handler, &trap_key("has"))? else {
        return value_has_property(&target, key);
    };
    let result = value_call(&trap, handler, &[target.clone(), key_value(key)])?;
    let trap_result = crate::convert::to_boolean(&result);
    if !trap_result && let Some(target_desc) = value_get_own_property(&target, key)? {
        if !target_desc.configurable {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "has trap reported a non-configurable target property as absent".into(),
            ));
        }
        if !value_is_extensible(&target)? {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "has trap reported an own property as absent on a non-extensible target".into(),
            ));
        }
    }
    Ok(trap_result)
}

/// [[Get]] (spec 10.5.8).
pub fn get(slots: &ProxySlots, key: &PropertyKey, receiver: Value) -> Result<Value, JsError> {
    let (target, handler) = resolved(slots)?;
    let Some(trap) = value_get_method(&handler, &trap_key("get"))? else {
        return value_get(&target, key, receiver);
    };
    let trap_result = value_call(&trap, handler, &[target.clone(), key_value(key), receiver])?;
    if let Some(target_desc) = value_get_own_property(&target, key)?
        && !target_desc.configurable
    {
        if target_desc.is_data() && target_desc.writable() == Some(false) {
            let target_value = target_desc.value().unwrap_or(Value::Undefined);
            if !same_value(&trap_result, &target_value) {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "get trap returned a different value for a non-writable, non-configurable data property".into(),
                ));
            }
        } else if target_desc.is_accessor()
            && target_desc.getter().is_none()
            && !trap_result.is_undefined()
        {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "get trap must return undefined for a non-configurable accessor with no getter"
                    .into(),
            ));
        }
    }
    Ok(trap_result)
}

/// [[Set]] (spec 10.5.9).
pub fn set(
    slots: &ProxySlots,
    key: &PropertyKey,
    value: Value,
    receiver: Value,
) -> Result<bool, JsError> {
    let (target, handler) = resolved(slots)?;
    let Some(trap) = value_get_method(&handler, &trap_key("set"))? else {
        return value_set(&target, key, value, receiver, false);
    };
    let result = value_call(
        &trap,
        handler,
        &[target.clone(), key_value(key), value.clone(), receiver],
    )?;
    if !crate::convert::to_boolean(&result) {
        return Ok(false);
    }
    if let Some(target_desc) = value_get_own_property(&target, key)?
        && !target_desc.configurable
    {
        if target_desc.is_data() && target_desc.writable() == Some(false) {
            let target_value = target_desc.value().unwrap_or(Value::Undefined);
            if !same_value(&value, &target_value) {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "set trap tried to change a non-writable, non-configurable data property"
                        .into(),
                ));
            }
        } else if target_desc.is_accessor() && target_desc.setter().is_none() {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "set trap tried to set a non-configurable accessor with no setter".into(),
            ));
        }
    }
    Ok(true)
}

/// [[Delete]] (spec 10.5.10).
pub fn delete(slots: &ProxySlots, key: &PropertyKey) -> Result<bool, JsError> {
    let (target, handler) = resolved(slots)?;
    let Some(trap) = value_get_method(&handler, &trap_key("deleteProperty"))? else {
        return value_delete(&target, key);
    };
    let result = value_call(&trap, handler, &[target.clone(), key_value(key)])?;
    if !crate::convert::to_boolean(&result) {
        return Ok(false);
    }
    if let Some(target_desc) = value_get_own_property(&target, key)? {
        if !target_desc.configurable {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "deleteProperty trap reported deletion of a non-configurable target property"
                    .into(),
            ));
        }
        if !value_is_extensible(&target)? {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "deleteProperty trap reported deletion of a property on a non-extensible target"
                    .into(),
            ));
        }
    }
    Ok(true)
}

/// [[OwnPropertyKeys]] (spec 10.5.11).
pub fn own_property_keys(slots: &ProxySlots) -> Result<Vec<PropertyKey>, JsError> {
    let (target, handler) = resolved(slots)?;
    let Some(trap) = value_get_method(&handler, &trap_key("ownKeys"))? else {
        return value_own_property_keys(&target);
    };
    let trap_result_array = value_call(&trap, handler, std::slice::from_ref(&target))?;
    let trap_result = create_list_from_array_like(&trap_result_array)?;
    // A PropertyKey's hash is content-stable: a rope's first hash materializes
    // its flat cache (OnceLock), but the cached form never changes the hash
    // output, so using it as a set key is sound.
    #[allow(clippy::mutable_key_type)]
    let mut seen = std::collections::HashSet::new();
    for item in &trap_result {
        if !seen.insert(item.clone()) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "ownKeys trap returned duplicate keys".into(),
            ));
        }
    }
    let extensible_target = value_is_extensible(&target)?;
    let target_keys = value_own_property_keys(&target)?;
    let mut target_configurable_keys = Vec::new();
    let mut target_non_configurable_keys = Vec::new();
    for target_key in &target_keys {
        let desc = value_get_own_property(&target, target_key)?;
        if let Some(desc) = desc {
            if desc.configurable {
                target_configurable_keys.push(target_key.clone());
            } else {
                target_non_configurable_keys.push(target_key.clone());
            }
        } else {
            target_configurable_keys.push(target_key.clone());
        }
    }
    if extensible_target && target_non_configurable_keys.is_empty() {
        return Ok(trap_result);
    }
    let mut unchecked = trap_result.clone();
    for required in &target_non_configurable_keys {
        let Some(position) = unchecked.iter().position(|k| k == required) else {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "ownKeys trap omitted a non-configurable target key".into(),
            ));
        };
        unchecked.remove(position);
    }
    if extensible_target {
        return Ok(trap_result);
    }
    for required in &target_configurable_keys {
        let Some(position) = unchecked.iter().position(|k| k == required) else {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "ownKeys trap omitted an own key of a non-extensible target".into(),
            ));
        };
        unchecked.remove(position);
    }
    if !unchecked.is_empty() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "ownKeys trap returned keys not present on a non-extensible target".into(),
        ));
    }
    Ok(trap_result)
}

/// [[Call]] (spec 10.5.12): only reachable for proxies whose target is
/// callable; the apply trap receives the argument list as an Array.
pub fn apply(slots: &ProxySlots, this: Value, args: &[Value]) -> Result<Value, JsError> {
    let (target, handler) = resolved(slots)?;
    let Some(trap) = value_get_method(&handler, &trap_key("apply"))? else {
        return value_call(&target, this, args);
    };
    let arg_array = create_array_from_list(args)?;
    value_call(&trap, handler, &[target, this, arg_array])
}

/// [[Construct]] (spec 10.5.13): the construct trap receives the argument
/// list as an Array and must return an object.
pub fn construct(slots: &ProxySlots, args: &[Value], new_target: &Value) -> Result<Value, JsError> {
    let (target, handler) = resolved(slots)?;
    if !is_constructor(&target) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Proxy target is not a constructor".into(),
        ));
    }
    let Some(trap) = value_get_method(&handler, &trap_key("construct"))? else {
        return value_construct(&target, args, new_target);
    };
    let arg_array = create_array_from_list(args)?;
    let new_obj = value_call(&trap, handler, &[target, arg_array, new_target.clone()])?;
    if !new_obj.is_object() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "construct trap must return an object".into(),
        ));
    }
    Ok(new_obj)
}

/// The property key as a language value (a String or Symbol).
fn key_value(key: &PropertyKey) -> Value {
    match key {
        PropertyKey::String(id) => Value::String(Handle::new(crate::string::lookup(*id))),
        PropertyKey::Symbol(sym) => Value::Symbol(Handle::new(sym.clone())),
    }
}

/// LengthOfArrayLike (spec 7.3.20) + CreateListFromArrayLike (spec 7.3.18)
/// with the ~property-key~ element kind, as the ownKeys trap requires:
/// every element must be a String or Symbol (spec 7.3.18 step 6.d).
fn create_list_from_array_like(value: &Value) -> Result<Vec<PropertyKey>, JsError> {
    let length = length_of_array_like(value)?;
    let mut list = Vec::with_capacity(length as usize);
    for index in 0..length {
        let element = value_get(
            value,
            &PropertyKey::from_utf8(&index.to_string()),
            value.clone(),
        )?;
        let key = if element.is_string() || element.is_symbol() {
            crate::convert::to_property_key(&element)?
        } else {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "ownKeys trap must return a list of strings and symbols".into(),
            ));
        };
        list.push(key);
    }
    Ok(list)
}

fn length_of_array_like(value: &Value) -> Result<u64, JsError> {
    if !value.is_object() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Value is not an object".into(),
        ));
    }
    let length = value_get(value, &PropertyKey::from_utf8("length"), value.clone())?;
    let length = crate::convert::to_length(crate::convert::to_number(&length)?);
    Ok(length)
}

/// CreateArrayFromList (spec 7.3.17).
fn create_array_from_list(list: &[Value]) -> Result<Value, JsError> {
    let agent = crate::function::current_agent();
    if let Some(hook) = ARRAY_FROM_LIST_HOOK.get().copied()
        && !agent.is_null()
        && let Ok(array) = hook(agent, list)
    {
        return Ok(array);
    }
    let array = JsObject::array_create(None, list.len() as f64)?;
    for (index, element) in list.iter().enumerate() {
        array.create_data_property(&JsString::from_utf8(&index.to_string()), element.clone())?;
    }
    Ok(Value::Object(array))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::function::Function;
    use crate::object::ObjectKind;
    use crate::value::is_callable;

    fn key(text: &str) -> JsString {
        JsString::from_utf8(text)
    }

    fn builtin(
        name: &str,
        f: impl Fn(&Value, &[Value]) -> Result<Value, JsError> + 'static,
    ) -> Value {
        Value::Function(
            Function::create_builtin(Some(key(name)), 0, Box::new(f), None, None).unwrap(),
        )
    }

    fn plain_object() -> Handle<JsObject> {
        JsObject::ordinary_object_create(None)
    }

    fn trap_handler(trap: &str, result: Value) -> Value {
        let handler = plain_object();
        handler
            .create_data_property(&key(trap), builtin(trap, move |_, _| Ok(result.clone())))
            .unwrap();
        Value::Object(handler)
    }

    fn proxy_of(target: Handle<JsObject>, handler: Value) -> Handle<JsObject> {
        proxy_create(Value::Object(target), handler).unwrap()
    }

    #[test]
    fn create_requires_object_target_and_handler() {
        assert!(proxy_create(Value::Number(1.0), Value::Object(plain_object())).is_err());
        assert!(proxy_create(Value::Object(plain_object()), Value::Null).is_err());
        let proxy =
            proxy_create(Value::Object(plain_object()), Value::Object(plain_object())).unwrap();
        assert!(matches!(proxy.kind, ObjectKind::Proxy(_)));
    }

    #[test]
    fn forwarding_without_traps_reaches_the_target() {
        let target = plain_object();
        target
            .create_data_property(&key("x"), Value::Number(5.0))
            .unwrap();
        let proxy = proxy_of(target, Value::Object(plain_object()));
        assert_eq!(proxy.get(&key("x")).unwrap(), Value::Number(5.0));
        assert!(proxy.has_own_property(&key("x")).unwrap());
        assert!(proxy.set(&key("y"), Value::Number(6.0), false).unwrap());
        assert_eq!(proxy.get(&key("y")).unwrap(), Value::Number(6.0));
        assert!(proxy.delete(&key("y")).unwrap());
        assert!(!proxy.has_property(&key("y")).unwrap());
        let names: Vec<String> = proxy
            .own_property_keys()
            .unwrap()
            .iter()
            .map(|k| k.display_string())
            .collect();
        assert_eq!(names, ["x"]);
    }

    #[test]
    fn get_trap_receives_target_key_and_receiver() {
        let target = plain_object();
        target
            .create_data_property(&key("x"), Value::Number(1.0))
            .unwrap();
        let captured = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorder = captured.clone();
        let handler = plain_object();
        handler
            .create_data_property(
                &key("get"),
                builtin("get", move |this, args| {
                    assert_eq!(args.len(), 3);
                    recorder.borrow_mut().push((
                        this.clone(),
                        args[0].clone(),
                        args[1].clone(),
                        args[2].clone(),
                    ));
                    Ok(Value::Number(99.0))
                }),
            )
            .unwrap();
        let proxy = proxy_of(target, Value::Object(handler));
        let receiver = Value::Object(proxy);
        assert_eq!(
            proxy
                .get_with_receiver_key(&PropertyKey::from_utf8("x"), receiver.clone())
                .unwrap(),
            Value::Number(99.0)
        );
        let (this, trap_target, trap_key, trap_receiver) = &captured.borrow()[0];
        // The trap's `this` is the handler object.
        assert!(this.is_object());
        assert_eq!(trap_key, &Value::String(Handle::new(key("x"))));
        assert!(same_value(trap_receiver, &receiver));
        assert!(trap_target.is_object());
    }

    #[test]
    fn get_invariant_non_writable_non_configurable() {
        let target = plain_object();
        target
            .define_property(&key("x"), &PropertyDescriptor::none(Value::Number(1.0)))
            .unwrap();
        // Trap reports a different value: invariant violation -> TypeError.
        let proxy = proxy_of(target, trap_handler("get", Value::Number(2.0)));
        assert!(proxy.get(&key("x")).is_err());
        // Trap reports the same value: allowed.
        let proxy = proxy_of(target, trap_handler("get", Value::Number(1.0)));
        assert_eq!(proxy.get(&key("x")).unwrap(), Value::Number(1.0));
    }

    #[test]
    fn set_invariant_non_writable_non_configurable() {
        let target = plain_object();
        target
            .define_property(&key("x"), &PropertyDescriptor::none(Value::Number(1.0)))
            .unwrap();
        let proxy = proxy_of(target, trap_handler("set", Value::Boolean(true)));
        // Setting the same value is allowed.
        assert!(proxy.set(&key("x"), Value::Number(1.0), false).unwrap());
        // Changing it violates the invariant.
        assert!(proxy.set(&key("x"), Value::Number(2.0), false).is_err());
    }

    #[test]
    fn has_trap_cannot_hide_non_configurable_properties() {
        let target = plain_object();
        target
            .define_property(&key("x"), &PropertyDescriptor::none(Value::Number(1.0)))
            .unwrap();
        let proxy = proxy_of(target, trap_handler("has", Value::Boolean(false)));
        assert!(proxy.has_property(&key("x")).is_err());
    }

    #[test]
    fn delete_trap_cannot_delete_non_configurable_properties() {
        let target = plain_object();
        target
            .define_property(&key("x"), &PropertyDescriptor::none(Value::Number(1.0)))
            .unwrap();
        let proxy = proxy_of(target, trap_handler("deleteProperty", Value::Boolean(true)));
        assert!(proxy.delete(&key("x")).is_err());
    }

    #[test]
    fn own_keys_trap_must_include_non_configurable_keys() {
        let target = plain_object();
        target
            .define_property(&key("x"), &PropertyDescriptor::none(Value::Number(1.0)))
            .unwrap();
        // Omitting "x" violates the invariant.
        let empty = JsObject::array_create(None, 0.0).unwrap();
        let proxy = proxy_of(target, trap_handler("ownKeys", Value::Object(empty)));
        assert!(proxy.own_property_keys().is_err());
        // Including it is fine.
        let arr = JsObject::array_create(None, 1.0).unwrap();
        arr.create_data_property(&key("0"), Value::String(Handle::new(key("x"))))
            .unwrap();
        let proxy = proxy_of(target, trap_handler("ownKeys", Value::Object(arr)));
        let names: Vec<String> = proxy
            .own_property_keys()
            .unwrap()
            .iter()
            .map(|k| k.display_string())
            .collect();
        assert_eq!(names, ["x"]);
    }

    #[test]
    fn is_extensible_trap_must_agree_with_target() {
        let target = plain_object();
        let proxy = proxy_of(target, trap_handler("isExtensible", Value::Boolean(false)));
        assert!(proxy.is_extensible().is_err());
    }

    #[test]
    fn prevent_extensions_trap_cannot_lie() {
        let target = plain_object();
        let proxy = proxy_of(
            target,
            trap_handler("preventExtensions", Value::Boolean(true)),
        );
        assert!(proxy.prevent_extensions().is_err());
    }

    #[test]
    fn get_prototype_of_invariant_for_non_extensible_target() {
        let target = plain_object();
        assert!(target.prevent_extensions().unwrap());
        let proxy = proxy_of(
            target,
            trap_handler("getPrototypeOf", Value::Object(plain_object())),
        );
        // Non-extensible target with a non-null prototype trap result that
        // differs from the target's null prototype: TypeError.
        assert!(proxy.get_prototype_of().is_err());
    }

    #[test]
    fn revoked_proxies_throw() {
        let target = plain_object();
        let handler = plain_object();
        let proxy = proxy_create(Value::Object(target), Value::Object(handler)).unwrap();
        let ObjectKind::Proxy(slots) = &proxy.kind else {
            unreachable!()
        };
        revoke(slots);
        assert!(proxy.get(&key("x")).is_err());
        assert!(proxy.set(&key("x"), Value::Undefined, false).is_err());
        assert!(proxy.own_property_keys().is_err());
        assert!(proxy.is_extensible().is_err());
    }

    #[test]
    fn callable_proxy_runs_the_apply_trap() {
        let target = Function::create_builtin(
            Some(key("f")),
            1,
            Box::new(|_, _| Ok(Value::Undefined)),
            None,
            None,
        )
        .unwrap();
        let recorded = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorder = recorded.clone();
        let handler = plain_object();
        handler
            .create_data_property(
                &trap_key("apply"),
                builtin("apply", move |_, args| {
                    recorder.borrow_mut().push(args.to_vec());
                    Ok(Value::Number(7.0))
                }),
            )
            .unwrap();
        let proxy = proxy_create(Value::Function(target), Value::Object(handler)).unwrap();
        assert!(is_callable(&Value::Object(proxy)));
        let result = value_call(
            &Value::Object(proxy),
            Value::Undefined,
            &[Value::Number(3.0)],
        )
        .unwrap();
        assert_eq!(result, Value::Number(7.0));
        // The trap received target, thisArg, and the args as an Array.
        let recorded = recorded.borrow();
        assert_eq!(recorded.len(), 1);
        assert!(recorded[0][0].is_function());
        assert_eq!(recorded[0][1], Value::Undefined);
        if let Some(arr) = recorded[0][2].as_object() {
            assert_eq!(arr.get(&key("0")).unwrap(), Value::Number(3.0));
            assert_eq!(arr.get(&key("length")).unwrap(), Value::Number(1.0));
        } else {
            panic!("argArray must be an Array, got {:?}", recorded[0][2]);
        }
    }

    #[test]
    fn constructible_proxy_runs_the_construct_trap() {
        let target = Function::create_builtin(
            Some(key("C")),
            0,
            Box::new(|_, _| Ok(Value::Undefined)),
            Some(Box::new(|_, _| Ok(Value::Object(plain_object())))),
            None,
        )
        .unwrap();
        let handler = plain_object();
        handler
            .create_data_property(
                &trap_key("construct"),
                builtin("construct", |_, _| Ok(Value::Object(plain_object()))),
            )
            .unwrap();
        let proxy = proxy_create(Value::Function(target), Value::Object(handler)).unwrap();
        assert!(is_constructor(&Value::Object(proxy)));
        let proxy_value = Value::Object(proxy);
        let result = value_construct(&proxy_value, &[], &proxy_value).unwrap();
        assert!(result.is_object());
    }
}
