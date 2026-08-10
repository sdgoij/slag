//! Ordinary objects (spec ch. 10) — the shell the execution model needs.
//!
//! Phase 4 pulls forward just enough of the object model for the global
//! object, object environment records, and global function bindings: an
//! ordinary object with data properties and the internal methods those
//! algorithms call. The full property-descriptor machinery (accessors,
//! invariants), exotics, and callable function objects join in Phase 5-7.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{ErrorKind, JsError};
use crate::handle::Handle;
use crate::ops::same_value;
use crate::property::PropertyDescriptor;
use crate::string::JsString;
use crate::value::Value;

static NEXT_OBJECT_ID: AtomicU64 = AtomicU64::new(1);

/// A data property stored on an ordinary object.
#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    pub value: Value,
    pub writable: bool,
    pub enumerable: bool,
    pub configurable: bool,
}

impl Property {
    pub fn new(value: Value, writable: bool, enumerable: bool, configurable: bool) -> Self {
        Self {
            value,
            writable,
            enumerable,
            configurable,
        }
    }
}

/// An ordinary ECMAScript object (spec 10.1). Equality is identity: each
/// object carries a unique `id` (mirroring `Symbol`), so `Handle<JsObject>`
/// equality and the derived `PartialEq` on `Value` are identity tests.
#[derive(Debug, Clone)]
pub struct JsObject {
    id: u64,
    /// [[Prototype]]; `None` when the prototype is *null*.
    pub prototype: Option<Handle<JsObject>>,
    /// [[Extensible]].
    pub extensible: bool,
    /// Own properties in insertion order (the [[OwnPropertyKeys]] string
    /// order for ordinary objects).
    pub properties: RefCell<Vec<(JsString, Property)>>,
}

impl PartialEq for JsObject {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for JsObject {}

impl JsObject {
    /// OrdinaryObjectCreate (spec 10.1.13).
    pub fn ordinary_object_create(prototype: Option<Handle<JsObject>>) -> Handle<JsObject> {
        Handle::new(Self {
            id: NEXT_OBJECT_ID.fetch_add(1, Ordering::Relaxed),
            prototype,
            extensible: true,
            properties: RefCell::new(Vec::new()),
        })
    }

    /// OrdinaryGetOwnProperty (spec 10.1.7.1): `None` when the object does
    /// not have an own property `key`.
    pub fn get_own_property(&self, key: &JsString) -> Option<Property> {
        self.properties
            .borrow()
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, p)| p.clone())
    }

    /// spec 7.3.12 HasOwnProperty.
    pub fn has_own_property(&self, key: &JsString) -> bool {
        self.properties.borrow().iter().any(|(name, _)| name == key)
    }

    /// spec 7.3.13 HasProperty: walks the prototype chain.
    pub fn has_property(&self, key: &JsString) -> bool {
        let mut current = Some(self);
        while let Some(obj) = current {
            if obj.has_own_property(key) {
                return true;
            }
            current = obj.prototype.as_deref();
        }
        false
    }

    /// OrdinaryGet (spec 10.1.8.3): prototype walk returning data property
    /// values. Accessor properties arrive with Phase 5.
    pub fn get(&self, key: &JsString) -> Result<Value, JsError> {
        let mut current = Some(self);
        while let Some(obj) = current {
            if let Some(prop) = obj.get_own_property(key) {
                return Ok(prop.value);
            }
            current = obj.prototype.as_deref();
        }
        Ok(Value::Undefined)
    }

    /// OrdinarySet (spec 10.1.9.3) with the receiver equal to `self`.
    /// Non-writable properties fail: silently when `throw` is false, with a
    /// TypeError when it is true.
    pub fn set(&self, key: &JsString, value: Value, throw: bool) -> Result<bool, JsError> {
        let mut current = Some(self);
        loop {
            let Some(obj) = current else {
                if self.extensible {
                    return self.define_property(
                        key,
                        &PropertyDescriptor {
                            value: Some(value),
                            writable: Some(true),
                            enumerable: Some(true),
                            configurable: Some(true),
                        },
                    );
                }
                if throw {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Cannot set property on non-extensible object".into(),
                    ));
                }
                return Ok(false);
            };
            if let Some(own) = obj.get_own_property(key) {
                if !own.writable {
                    if throw {
                        return Err(JsError::new(
                            ErrorKind::TypeError,
                            format!(
                                "Cannot assign to read only property {:?}",
                                key.to_string_lossy()
                            ),
                        ));
                    }
                    return Ok(false);
                }
                // Writable data property: update the receiver's own copy.
                // An existing property keeps its attributes; a new one is
                // created with CreateDataProperty semantics.
                if self.has_own_property(key) {
                    return self.define_property(
                        key,
                        &PropertyDescriptor {
                            value: Some(value),
                            writable: None,
                            enumerable: None,
                            configurable: None,
                        },
                    );
                }
                return self.create_data_property(key, value);
            }
            current = obj.prototype.as_deref();
        }
    }

    /// OrdinaryDefineOwnProperty (spec 10.1.6.3) for data and generic
    /// descriptors; accessors join in Phase 5.
    pub fn define_property(
        &self,
        key: &JsString,
        desc: &PropertyDescriptor,
    ) -> Result<bool, JsError> {
        match self.get_own_property(key) {
            None => {
                if !self.extensible {
                    return Ok(false);
                }
                let mut complete = desc.clone();
                complete.complete();
                let prop = Property::new(
                    complete.value.unwrap(),
                    complete.writable.unwrap(),
                    complete.enumerable.unwrap(),
                    complete.configurable.unwrap(),
                );
                self.properties.borrow_mut().push((key.clone(), prop));
                Ok(true)
            }
            Some(mut current) => {
                if desc.value.is_none()
                    && desc.writable.is_none()
                    && desc.enumerable.is_none()
                    && desc.configurable.is_none()
                {
                    return Ok(true);
                }
                if !current.configurable {
                    if desc.configurable == Some(true) {
                        return Ok(false);
                    }
                    if let Some(enumerable) = desc.enumerable
                        && enumerable != current.enumerable
                    {
                        return Ok(false);
                    }
                    if desc.is_data_descriptor() && !current.writable {
                        if desc.writable == Some(true) {
                            return Ok(false);
                        }
                        if let Some(value) = &desc.value
                            && !same_value(value, &current.value)
                        {
                            return Ok(false);
                        }
                    }
                }
                if let Some(value) = &desc.value {
                    current.value = value.clone();
                }
                if let Some(writable) = desc.writable {
                    current.writable = writable;
                }
                if let Some(enumerable) = desc.enumerable {
                    current.enumerable = enumerable;
                }
                if let Some(configurable) = desc.configurable {
                    current.configurable = configurable;
                }
                let mut props = self.properties.borrow_mut();
                if let Some((_, slot)) = props.iter_mut().find(|(name, _)| name == key) {
                    *slot = current;
                }
                Ok(true)
            }
        }
    }

    /// spec 7.3.4 CreateDataProperty.
    pub fn create_data_property(&self, key: &JsString, value: Value) -> Result<bool, JsError> {
        self.define_property(key, &PropertyDescriptor::data(value))
    }

    /// spec 7.3.5 CreateDataPropertyOrThrow.
    pub fn create_data_property_or_throw(
        &self,
        key: &JsString,
        value: Value,
    ) -> Result<(), JsError> {
        if !self.create_data_property(key, value)? {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Cannot define property".into(),
            ));
        }
        Ok(())
    }

    /// spec 7.3.6 DefinePropertyOrThrow.
    pub fn define_property_or_throw(
        &self,
        key: &JsString,
        desc: &PropertyDescriptor,
    ) -> Result<(), JsError> {
        if !self.define_property(key, desc)? {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Cannot redefine property".into(),
            ));
        }
        Ok(())
    }

    /// OrdinaryDelete (spec 10.1.10.2).
    pub fn delete(&self, key: &JsString) -> Result<bool, JsError> {
        let mut props = self.properties.borrow_mut();
        if let Some(index) = props.iter().position(|(name, _)| name == key) {
            if props[index].1.configurable {
                props.remove(index);
                return Ok(true);
            }
            return Ok(false);
        }
        Ok(true)
    }

    /// spec 7.3.10 IsExtensible.
    pub fn is_extensible(&self) -> bool {
        self.extensible
    }

    /// OrdinaryGetPrototypeOf (spec 10.1.1.1).
    pub fn get_prototype_of(&self) -> Option<Handle<JsObject>> {
        self.prototype.clone()
    }

    /// spec 7.3.11 GetMethod: `None` when the property is *undefined* or
    /// absent, a TypeError when it is present but not callable.
    pub fn get_method(&self, key: &JsString) -> Result<Option<Value>, JsError> {
        let value = self.get(key)?;
        match value {
            Value::Undefined | Value::Null => Ok(None),
            v if crate::value::is_callable(&v) => Ok(Some(v)),
            _ => Err(JsError::new(
                ErrorKind::TypeError,
                format!("{:?} is not a function", key.to_string_lossy()),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(text: &str) -> JsString {
        JsString::from_utf8(text)
    }

    #[test]
    fn objects_are_identity_equal() {
        let a = JsObject::ordinary_object_create(None);
        let b = JsObject::ordinary_object_create(None);
        assert_eq!(a, a);
        assert_ne!(a, b);
    }

    #[test]
    fn create_data_property_then_get() {
        let obj = JsObject::ordinary_object_create(None);
        obj.create_data_property(&key("x"), Value::Number(1.0))
            .unwrap();
        assert!(obj.has_own_property(&key("x")));
        assert_eq!(obj.get(&key("x")).unwrap(), Value::Number(1.0));
        assert_eq!(obj.get(&key("missing")).unwrap(), Value::Undefined);
    }

    #[test]
    fn set_updates_value_keeping_attributes() {
        let obj = JsObject::ordinary_object_create(None);
        obj.define_property(
            &key("x"),
            &PropertyDescriptor {
                value: Some(Value::Number(1.0)),
                writable: Some(true),
                enumerable: Some(false),
                configurable: Some(false),
            },
        )
        .unwrap();
        assert!(obj.set(&key("x"), Value::Number(2.0), false).unwrap());
        let prop = obj.get_own_property(&key("x")).unwrap();
        assert_eq!(prop.value, Value::Number(2.0));
        assert!(!prop.enumerable);
    }

    #[test]
    fn non_writable_property_rejects_set() {
        let obj = JsObject::ordinary_object_create(None);
        obj.define_property(
            &key("x"),
            &PropertyDescriptor {
                value: Some(Value::Number(1.0)),
                writable: Some(false),
                enumerable: Some(true),
                configurable: true.into(),
            },
        )
        .unwrap();
        assert!(!obj.set(&key("x"), Value::Number(2.0), false).unwrap());
        assert!(obj.set(&key("x"), Value::Number(2.0), true).is_err());
        assert_eq!(obj.get(&key("x")).unwrap(), Value::Number(1.0));
    }

    #[test]
    fn prototype_walk_finds_inherited_properties() {
        let proto = JsObject::ordinary_object_create(None);
        proto
            .create_data_property(&key("p"), Value::Number(7.0))
            .unwrap();
        let obj = JsObject::ordinary_object_create(Some(proto));
        assert!(obj.has_property(&key("p")));
        assert!(!obj.has_own_property(&key("p")));
        assert_eq!(obj.get(&key("p")).unwrap(), Value::Number(7.0));
    }

    #[test]
    fn delete_requires_configurable() {
        let obj = JsObject::ordinary_object_create(None);
        obj.define_property(&key("fixed"), &PropertyDescriptor::none(Value::Number(1.0)))
            .unwrap();
        obj.create_data_property(&key("free"), Value::Number(2.0))
            .unwrap();
        assert!(!obj.delete(&key("fixed")).unwrap());
        assert!(obj.delete(&key("free")).unwrap());
        assert!(!obj.has_property(&key("free")));
        assert!(obj.delete(&key("absent")).unwrap());
    }

    #[test]
    fn non_extensible_rejects_new_properties() {
        let mut obj = JsObject::ordinary_object_create(None);
        Handle::get_mut(&mut obj).unwrap().extensible = false;
        assert!(
            !obj.create_data_property(&key("x"), Value::Undefined)
                .unwrap()
        );
        assert!(
            obj.create_data_property_or_throw(&key("x"), Value::Undefined)
                .is_err()
        );
    }
}
