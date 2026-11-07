//! The `Value` facade (rquickjs-shaped): a handle over a language value.

use crux::value::Value as CruxValue;
use runtime::api::Local;

/// A JavaScript value (rquickjs's `Value`).
///
/// It copies freely and stays valid for as long as the value it names is
/// rooted. Values held across calls should be stored in a persistent handle,
/// not on the Rust heap, for the same reason `api::Local` documents: a
/// `Vec<Value>` is invisible to the conservative stack scan.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Value(Local);

impl Value {
    pub fn undefined() -> Self {
        Self(Local::undefined())
    }

    pub fn null() -> Self {
        Self(Local::null())
    }

    pub fn bool(value: bool) -> Self {
        Self(Local::boolean(value))
    }

    pub fn number(value: f64) -> Self {
        Self(Local::number(value))
    }

    pub fn string(value: impl Into<String>) -> Self {
        Self(Local::string(value))
    }

    pub(crate) fn from_local(local: Local) -> Self {
        Self(local)
    }

    pub(crate) fn from_crux(value: CruxValue) -> Self {
        Self(Local::from_value(value))
    }

    pub(crate) fn as_local(&self) -> &Local {
        &self.0
    }

    pub fn is_undefined(&self) -> bool {
        self.0.is_undefined()
    }

    pub fn is_null(&self) -> bool {
        self.0.is_null()
    }

    pub fn is_boolean(&self) -> bool {
        self.0.is_boolean()
    }

    pub fn is_number(&self) -> bool {
        self.0.is_number()
    }

    pub fn is_string(&self) -> bool {
        self.0.is_string()
    }

    pub fn is_symbol(&self) -> bool {
        self.0.is_symbol()
    }

    pub fn is_bigint(&self) -> bool {
        self.0.is_bigint()
    }

    pub fn is_object(&self) -> bool {
        self.0.is_object()
    }

    /// Whether the value is callable (spec 7.2.3).
    pub fn is_function(&self) -> bool {
        self.0.is_function()
    }

    /// Whether the value is constructible (spec 7.2.4).
    pub fn is_constructor(&self) -> bool {
        self.0.is_constructor()
    }

    /// `typeof` of the value (spec 7.2.6).
    pub fn type_of(&self) -> &'static str {
        self.0.type_of()
    }

    pub fn as_boolean(&self) -> Option<bool> {
        self.0.as_boolean()
    }

    pub fn as_number(&self) -> Option<f64> {
        self.0.as_number()
    }

    /// The string's lossy UTF-8 rendering when the value is a String.
    pub fn as_string(&self) -> Option<String> {
        self.0.as_string()
    }

    /// The object half of the value, when it is an object or function.
    pub fn as_object(&self) -> Option<crate::object::Object> {
        self.0
            .as_object()
            .map(|_| crate::object::Object::from_local(self.0))
    }

    /// The function half of the value, when it is callable.
    pub fn as_function(&self) -> Option<crate::function::Function> {
        self.0
            .is_function()
            .then(|| crate::function::Function::from_local(self.0))
    }
}
