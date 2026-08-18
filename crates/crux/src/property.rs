//! Property keys and Property Descriptor records (spec 6.1.7, 6.2.5).

use crate::error::{ErrorKind, JsError};
use crate::handle::Handle;
use crate::object::JsObject;
use crate::string::{AtomId, JsString, intern, intern_utf8, lookup};
use crate::symbol::{Symbol, descriptive_string};
use crate::value::{Value, is_callable};

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

/// ToPropertyDescriptor (spec 6.2.5.4): read the descriptor fields off a
/// descriptor object. A descriptor with both data and accessor fields is an
/// error.
pub fn to_property_descriptor(value: &Value) -> Result<PropertyDescriptor, JsError> {
    let obj = if let Some(obj) = value.as_object() {
        obj
    } else if let Some(function) = value.as_function() {
        function.object.clone()
    } else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Property description must be an object".into(),
        ));
    };
    let mut desc = PropertyDescriptor {
        value: None,
        writable: None,
        get: None,
        set: None,
        enumerable: None,
        configurable: None,
    };
    if obj.has_property(&JsString::from_utf8("enumerable"))? {
        desc.enumerable = Some(crate::convert::to_boolean(
            &obj.get(&JsString::from_utf8("enumerable"))?,
        ));
    }
    if obj.has_property(&JsString::from_utf8("configurable"))? {
        desc.configurable = Some(crate::convert::to_boolean(
            &obj.get(&JsString::from_utf8("configurable"))?,
        ));
    }
    if obj.has_property(&JsString::from_utf8("value"))? {
        desc.value = Some(obj.get(&JsString::from_utf8("value"))?);
    }
    if obj.has_property(&JsString::from_utf8("writable"))? {
        desc.writable = Some(crate::convert::to_boolean(
            &obj.get(&JsString::from_utf8("writable"))?,
        ));
    }
    for name in ["get", "set"] {
        let key = JsString::from_utf8(name);
        if obj.has_property(&key)? {
            let method = obj.get(&key)?;
            if !method.is_undefined() && !is_callable(&method) {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    format!("{name} must be a function or undefined"),
                ));
            }
            if name == "get" {
                desc.get = Some(method);
            } else {
                desc.set = Some(method);
            }
        }
    }
    // spec step 9: accessor fields conflict with data fields.
    if (desc.get.is_some() || desc.set.is_some())
        && (desc.value.is_some() || desc.writable.is_some())
    {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Invalid property descriptor: cannot both specify accessors and a value or writable attribute".into(),
        ));
    }
    Ok(desc)
}

/// FromPropertyDescriptor (spec 6.2.5.5): build the descriptor object from a
/// Property Descriptor, copying only the present fields.
/// Returns the current realm's `%Object.prototype%` inside a proxy/descriptor
/// window: the runtime installs this so trap descriptor objects get the
/// realm's prototype (FromPropertyDescriptor, spec 6.2.4.4).
type ObjectProtoHook = fn(agent: *mut ()) -> Option<Handle<JsObject>>;
static OBJECT_PROTO_HOOK: std::sync::OnceLock<ObjectProtoHook> = std::sync::OnceLock::new();

/// Install the `%Object.prototype%` provider (the runtime does this once at
/// startup, like the ECMAScript executor hook).
pub fn install_object_proto_hook(hook: ObjectProtoHook) {
    let _ = OBJECT_PROTO_HOOK.set(hook);
}

/// The current realm's `%Object.prototype%` when the hook is installed and
/// an agent window is active.
pub fn current_object_proto() -> Option<Handle<JsObject>> {
    let agent = crate::function::current_agent();
    if agent.is_null() {
        return None;
    }
    OBJECT_PROTO_HOOK
        .get()
        .copied()
        .and_then(|hook| hook(agent))
}

pub fn from_property_descriptor(
    desc: &PropertyDescriptor,
    prototype: Option<Handle<JsObject>>,
) -> Result<Value, JsError> {
    let obj = JsObject::ordinary_object_create(prototype.or_else(current_object_proto));
    if let Some(value) = &desc.value {
        obj.create_data_property_or_throw(&JsString::from_utf8("value"), value.clone())?;
    }
    if let Some(writable) = desc.writable {
        obj.create_data_property_or_throw(
            &JsString::from_utf8("writable"),
            Value::Boolean(writable),
        )?;
    }
    if let Some(get) = &desc.get {
        obj.create_data_property_or_throw(&JsString::from_utf8("get"), get.clone())?;
    }
    if let Some(set) = &desc.set {
        obj.create_data_property_or_throw(&JsString::from_utf8("set"), set.clone())?;
    }
    if let Some(enumerable) = desc.enumerable {
        obj.create_data_property_or_throw(
            &JsString::from_utf8("enumerable"),
            Value::Boolean(enumerable),
        )?;
    }
    if let Some(configurable) = desc.configurable {
        obj.create_data_property_or_throw(
            &JsString::from_utf8("configurable"),
            Value::Boolean(configurable),
        )?;
    }
    Ok(Value::Object(obj))
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
