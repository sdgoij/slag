//! Property keys and Property Descriptor records (spec 6.1.7).

use crate::string::{AtomId, intern, intern_utf8, lookup};
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

    /// The key's text for diagnostics and error messages.
    pub fn display_string(&self) -> String {
        match self {
            PropertyKey::String(id) => lookup(*id).to_string_lossy(),
            PropertyKey::Symbol(s) => descriptive_string(s),
        }
    }
}

/// A Property Descriptor record (spec 6.2.5.5). Phase 1 covers the
/// data-property fields; the accessor fields ([[Get]]/[[Set]]) join with the
/// object model in Phase 5.
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyDescriptor {
    pub value: Option<Value>,
    pub writable: Option<bool>,
    pub enumerable: Option<bool>,
    pub configurable: Option<bool>,
}

impl PropertyDescriptor {
    /// A data descriptor with all attributes set to `true`.
    pub fn data(value: Value) -> Self {
        Self {
            value: Some(value),
            writable: Some(true),
            enumerable: Some(true),
            configurable: Some(true),
        }
    }

    /// A data descriptor with all attributes set to `false`.
    pub fn none(value: Value) -> Self {
        Self {
            value: Some(value),
            writable: Some(false),
            enumerable: Some(false),
            configurable: Some(false),
        }
    }

    /// spec 6.2.5.7: present if [[Value]] or [[Writable]] is present.
    pub fn is_data_descriptor(&self) -> bool {
        self.value.is_some() || self.writable.is_some()
    }

    /// spec 6.2.5.6: present if neither data nor accessor fields are present.
    pub fn is_generic_descriptor(&self) -> bool {
        !self.is_data_descriptor()
    }

    /// spec 6.2.5.9 CompletePropertyDescriptor for data descriptors: missing
    /// fields take their default values.
    pub fn complete(&mut self) {
        self.value.get_or_insert(Value::Undefined);
        self.writable.get_or_insert(false);
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
            enumerable: None,
            configurable: None,
        };
        d.complete();
        assert_eq!(d.value, Some(Value::Undefined));
        assert_eq!(d.writable, Some(false));
        assert_eq!(d.enumerable, Some(false));
        assert_eq!(d.configurable, Some(false));
    }
}
