//! Property descriptors (`v8::PropertyDescriptor`).
//!
//! The crate we stand in for keeps a C++ descriptor and hands it to
//! `Object::DefineProperty`, which is an ordinary `DefineOwnProperty`: a field
//! the descriptor never set is left as it was. The engine's descriptor is the
//! same partial record — optional fields, absent meaning "not mentioned" — so
//! this carries that one rather than a second copy of the same shape.

use runtime::api;

use crate::data::Value;
use crate::handle::Local;

/// A property descriptor (`v8::PropertyDescriptor`).
#[derive(Debug, Clone)]
pub struct PropertyDescriptor(crux::property::PropertyDescriptor);

impl Default for PropertyDescriptor {
    fn default() -> Self {
        Self::new()
    }
}

impl PropertyDescriptor {
    /// A descriptor the engine handed over, for a callback that reads one
    /// (v8's named property handler passes one to a `definer`).
    pub(crate) fn from_engine(descriptor: crux::property::PropertyDescriptor) -> Self {
        Self(descriptor)
    }
    /// A descriptor that mentions nothing (`v8::PropertyDescriptor`).
    pub fn new() -> Self {
        Self(crux::property::PropertyDescriptor {
            value: None,
            writable: None,
            get: None,
            set: None,
            enumerable: None,
            configurable: None,
        })
    }

    /// A data descriptor holding `value`, and nothing else
    /// (`v8::PropertyDescriptor(Local<Value>)`).
    pub fn new_from_value(value: Local<Value>) -> Self {
        Self(crux::property::PropertyDescriptor {
            value: Some(*value.engine().value()),
            ..Self::new().0
        })
    }

    /// A data descriptor holding `value` and `writable`
    /// (`v8::PropertyDescriptor(Local<Value>, bool)`).
    pub fn new_from_value_writable(value: Local<Value>, writable: bool) -> Self {
        Self(crux::property::PropertyDescriptor {
            value: Some(*value.engine().value()),
            writable: Some(writable),
            ..Self::new().0
        })
    }

    /// An accessor descriptor holding `get` and `set`
    /// (`v8::PropertyDescriptor(Local<Value>, Local<Value>)`).
    pub fn new_from_get_set(get: Local<Value>, set: Local<Value>) -> Self {
        Self(crux::property::PropertyDescriptor {
            get: Some(*get.engine().value()),
            set: Some(*set.engine().value()),
            ..Self::new().0
        })
    }

    /// The `configurable` field; `false` when it is not mentioned.
    pub fn configurable(&self) -> bool {
        self.0.configurable.unwrap_or(false)
    }

    /// The `enumerable` field; `false` when it is not mentioned.
    pub fn enumerable(&self) -> bool {
        self.0.enumerable.unwrap_or(false)
    }

    /// The `writable` field; `false` when it is not mentioned.
    pub fn writable(&self) -> bool {
        self.0.writable.unwrap_or(false)
    }

    /// The `value` field, or *undefined* when it is not mentioned — where the
    /// crate we stand in for hands back an empty handle its own callers unwrap.
    pub fn value(&self) -> Local<'_, Value> {
        Local::from_engine(api::Local::from(
            self.0.value.unwrap_or(crux::value::Value::Undefined),
        ))
    }

    /// The `get` field, or *undefined* when it is not mentioned.
    pub fn get(&self) -> Local<'_, Value> {
        Local::from_engine(api::Local::from(
            self.0.get.unwrap_or(crux::value::Value::Undefined),
        ))
    }

    /// The `set` field, or *undefined* when it is not mentioned.
    pub fn set(&self) -> Local<'_, Value> {
        Local::from_engine(api::Local::from(
            self.0.set.unwrap_or(crux::value::Value::Undefined),
        ))
    }

    pub fn has_configurable(&self) -> bool {
        self.0.configurable.is_some()
    }

    pub fn has_enumerable(&self) -> bool {
        self.0.enumerable.is_some()
    }

    pub fn has_writable(&self) -> bool {
        self.0.writable.is_some()
    }

    pub fn has_value(&self) -> bool {
        self.0.value.is_some()
    }

    pub fn has_get(&self) -> bool {
        self.0.get.is_some()
    }

    pub fn has_set(&self) -> bool {
        self.0.set.is_some()
    }

    pub fn set_enumerable(&mut self, enumerable: bool) {
        self.0.enumerable = Some(enumerable);
    }

    pub fn set_configurable(&mut self, configurable: bool) {
        self.0.configurable = Some(configurable);
    }

    /// The engine's descriptor, which is what a define is handed.
    pub(crate) fn engine(&self) -> &crux::property::PropertyDescriptor {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Object;
    use crate::test_support::{eval, in_context};

    /// A descriptor built from an accessor pair defines exactly those two
    /// fields, and reads back through the property it defined.
    #[test]
    fn an_accessor_descriptor_defines_what_it_mentions() {
        in_context!(scope, {
            let getter = eval(scope, "(function () { return 7; })");
            let setter = eval(scope, "(function () {})");

            let mut descriptor = PropertyDescriptor::new_from_get_set(getter, setter);
            descriptor.set_enumerable(true);
            descriptor.set_configurable(true);
            assert!(descriptor.has_get() && descriptor.has_set());
            assert!(!descriptor.has_value() && !descriptor.has_writable());
            assert!(descriptor.enumerable() && descriptor.configurable());
            assert!(!descriptor.writable());

            let object = Object::new(scope);
            let key = crate::String::new(scope, "seven").expect("string").into();
            assert_eq!(object.define_property(scope, key, &descriptor), Some(true));

            let key = crate::String::new(scope, "seven").expect("string").into();
            let value = object.get(scope, key).expect("get");
            assert_eq!(
                Local::<crate::data::Number>::try_from(value)
                    .expect("number")
                    .value(),
                7.0
            );
        });
    }
}
