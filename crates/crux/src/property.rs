//! Property keys and Property Descriptor records (spec 6.1.7).

use crate::string::{AtomId, JsString, intern, intern_utf8, lookup};
use crate::symbol::{Symbol, descriptive_string};
use crate::value::Value;

/// A property key: an interned String or a Symbol (spec 6.1.7.6).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PropertyKey {
    String(AtomId),
    Symbol(Symbol),
}

impl PropertyKey {
    pub fn from_utf8(text: &str) -> Self {
        Self::String(intern_utf8(text))
    }

    pub fn from_utf16(units: &[u16]) -> Self {
        Self::String(intern(units))
    }

    /// The key for a JsString property name.
    pub fn from_js_string(text: &JsString) -> Self {
        Self::String(intern(text.as_slice()))
    }

    /// The key's text for diagnostics and error messages.
    pub fn display_string(&self) -> String {
        match self {
            PropertyKey::String(id) => lookup(*id).to_string_lossy(),
            PropertyKey::Symbol(s) => descriptive_string(s),
        }
    }
}

/// A Property Descriptor record (spec 6.2.5.5): data fields ([[Value]],
/// [[Writable]]) plus accessor fields ([[Get]], [[Set]]).
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyDescriptor {
    pub value: Option<Value>,
    pub writable: Option<bool>,
    pub get: Option<Value>,
    pub set: Option<Value>,
    pub enumerable: Option<bool>,
    pub configurable: Option<bool>,
}

impl PropertyDescriptor {
    /// A data descriptor with all attributes set to `true`.
    pub fn data(value: Value) -> Self {
        Self {
            value: Some(value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(true),
            configurable: Some(true),
        }
    }

    /// A data descriptor with all attributes set to `false`.
    pub fn none(value: Value) -> Self {
        Self {
            value: Some(value),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        }
    }

    /// An accessor descriptor with the given getter and setter.
    pub fn accessor(get: Option<Value>, set: Option<Value>) -> Self {
        Self {
            value: None,
            writable: None,
            get,
            set,
            enumerable: Some(true),
            configurable: Some(true),
        }
    }

    /// spec 6.2.5.7: present if [[Value]] or [[Writable]] is present.
    pub fn is_data_descriptor(&self) -> bool {
        self.value.is_some() || self.writable.is_some()
    }

    /// spec 6.2.5.6: present if [[Get]] or [[Set]] is present.
    pub fn is_accessor_descriptor(&self) -> bool {
        self.get.is_some() || self.set.is_some()
    }

    /// spec 6.2.5.8: present if neither data nor accessor fields are present.
    pub fn is_generic_descriptor(&self) -> bool {
        !self.is_data_descriptor() && !self.is_accessor_descriptor()
    }

    /// Whether the descriptor has no fields at all.
    pub fn is_empty(&self) -> bool {
        self.value.is_none()
            && self.writable.is_none()
            && self.get.is_none()
            && self.set.is_none()
            && self.enumerable.is_none()
            && self.configurable.is_none()
    }

    /// spec 6.2.5.9 CompletePropertyDescriptor: missing fields take their
    /// default values (the table-object-property-attributes defaults).
    pub fn complete(&mut self) {
        if self.is_generic_descriptor() || self.is_data_descriptor() {
            self.value.get_or_insert(Value::Undefined);
            self.writable.get_or_insert(false);
        } else {
            self.get.get_or_insert(Value::Undefined);
            self.set.get_or_insert(Value::Undefined);
        }
        self.enumerable.get_or_insert(false);
        self.configurable.get_or_insert(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_key_from_text_and_interner_agree() {
        let a = PropertyKey::from_utf8("length");
        let b = PropertyKey::from_utf8("length");
        assert_eq!(a, b);
        assert_eq!(a.display_string(), "length");
        let wide = PropertyKey::from_utf16(&[0xD83D, 0xDE00]);
        assert_eq!(wide.display_string(), "\u{1F600}");
    }

    #[test]
    fn data_descriptor_defaults() {
        let d = PropertyDescriptor::data(Value::Undefined);
        assert_eq!(d.writable, Some(true));
        assert_eq!(d.enumerable, Some(true));
        assert_eq!(d.configurable, Some(true));
        assert!(d.is_data_descriptor());
        assert!(!d.is_generic_descriptor());
    }

    #[test]
    fn none_descriptor_flips_attributes() {
        let d = PropertyDescriptor::none(Value::Undefined);
        assert_eq!(d.writable, Some(false));
        assert_eq!(d.enumerable, Some(false));
        assert_eq!(d.configurable, Some(false));
    }

    #[test]
    fn generic_descriptor_has_no_data_fields() {
        let d = PropertyDescriptor {
            value: None,
            writable: None,
            get: None,
            set: None,
            enumerable: Some(true),
            configurable: None,
        };
        assert!(d.is_generic_descriptor());
        assert!(!d.is_data_descriptor());
    }

    #[test]
    fn complete_fills_defaults() {
        let mut d = PropertyDescriptor {
            value: None,
            writable: None,
            get: None,
            set: None,
            enumerable: None,
            configurable: None,
        };
        d.complete();
        assert_eq!(d.value, Some(Value::Undefined));
        assert_eq!(d.writable, Some(false));
        assert_eq!(d.enumerable, Some(false));
        assert_eq!(d.configurable, Some(false));
    }

    #[test]
    fn complete_fills_accessor_defaults() {
        // An accessor descriptor with both fields present stays accessor; the
        // missing enumerable/configurable fields take their default values.
        let mut d = PropertyDescriptor::accessor(Some(Value::Undefined), Some(Value::Undefined));
        d.enumerable = None;
        d.configurable = None;
        d.complete();
        assert_eq!(d.get, Some(Value::Undefined));
        assert_eq!(d.set, Some(Value::Undefined));
        assert_eq!(d.enumerable, Some(false));
        assert_eq!(d.configurable, Some(false));
        assert!(d.is_accessor_descriptor());
        assert!(!d.is_data_descriptor());
    }
}
