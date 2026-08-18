//! Handle types for the V8-shaped API: [`Local`], [`Global`], [`MaybeLocal`],
//! and the (advisory) handle-scope markers.

use crux::handle::Handle;
use crux::object::JsObject;
use crux::string::JsString;
use crux::value::Value;

/// A scoped handle over a language value (v8::Local).
///
/// `crux` values are `Rc`-backed, so a `Local` is valid for as long as it
/// exists: no GC rooting and no handle-scope discipline. [`HandleScope`] and
/// [`EscapableHandleScope`] exist so V8-idiom code compiles unchanged; they
/// are markers.
#[derive(Debug, Clone, PartialEq)]
pub struct Local(pub(crate) Value);

impl Local {
    pub fn undefined() -> Self {
        Self(Value::Undefined)
    }

    pub fn null() -> Self {
        Self(Value::Null)
    }

    pub fn boolean(value: bool) -> Self {
        Self(Value::Boolean(value))
    }

    pub fn number(value: f64) -> Self {
        Self(Value::Number(value))
    }

    pub fn string(value: impl Into<String>) -> Self {
        let text = value.into();
        Self(Value::String(Handle::new(JsString::from_utf8(&text))))
    }

    /// Wrap an object handle.
    pub fn object(object: Handle<JsObject>) -> Self {
        Self(Value::Object(object))
    }

    /// `typeof` of the value (spec 7.2.6).
    pub fn type_of(&self) -> &'static str {
        crux::value::type_of(&self.0)
    }

    pub fn is_undefined(&self) -> bool {
        matches!(self.0, Value::Undefined)
    }

    pub fn is_null(&self) -> bool {
        matches!(self.0, Value::Null)
    }

    pub fn is_boolean(&self) -> bool {
        matches!(self.0, Value::Boolean(_))
    }

    pub fn is_number(&self) -> bool {
        matches!(self.0, Value::Number(_))
    }

    pub fn is_string(&self) -> bool {
        matches!(self.0, Value::String(_))
    }

    pub fn is_symbol(&self) -> bool {
        matches!(self.0, Value::Symbol(_))
    }

    pub fn is_bigint(&self) -> bool {
        matches!(self.0, Value::BigInt(_))
    }

    pub fn is_object(&self) -> bool {
        matches!(self.0, Value::Object(_))
    }

    /// Whether the value is callable (spec 7.2.3).
    pub fn is_function(&self) -> bool {
        crux::value::is_callable(&self.0)
    }

    /// Whether the value is constructible (spec 7.2.4).
    pub fn is_constructor(&self) -> bool {
        crux::value::is_constructor(&self.0)
    }

    pub fn as_boolean(&self) -> Option<bool> {
        match self.0 {
            Value::Boolean(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_number(&self) -> Option<f64> {
        match self.0 {
            Value::Number(value) => Some(value),
            _ => None,
        }
    }

    /// The string's lossy UTF-8 rendering when the value is a String.
    pub fn as_string(&self) -> Option<String> {
        match &self.0 {
            Value::String(s) => Some(s.to_string_lossy()),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<Handle<JsObject>> {
        self.0.as_object()
    }

    /// The underlying crux value.
    pub fn value(&self) -> &Value {
        &self.0
    }

    pub fn into_value(self) -> Value {
        self.0
    }
}

impl From<Value> for Local {
    fn from(value: Value) -> Self {
        Self(value)
    }
}

impl From<Local> for Value {
    fn from(local: Local) -> Self {
        local.0
    }
}

/// A persistent handle: keeps a value alive for as long as the handle
/// exists (v8::Global). Values are `Rc`-backed, so a `Global` holds a
/// strong reference either way; the type exists to mirror `v8::Global`'s
/// intent and move semantics.
#[derive(Debug, Clone)]
pub struct Global(Value);

impl Global {
    /// A persistent handle over a value.
    pub fn new(value: Local) -> Self {
        Self(value.0)
    }

    /// An empty handle (v8::Global::Empty).
    pub fn empty() -> Self {
        Self(Value::Undefined)
    }

    pub fn is_empty(&self) -> bool {
        matches!(self.0, Value::Undefined)
    }

    pub fn get(&self) -> Local {
        Local(self.0.clone())
    }

    pub fn reset(&mut self, local: Local) {
        self.0 = local.0;
    }

    pub fn clear(&mut self) {
        self.0 = Value::Undefined;
    }
}

/// A maybe-value: `Nothing` when an operation failed and a pending exception
/// was set (v8::MaybeLocal).
#[derive(Debug)]
pub enum MaybeLocal {
    Some(Local),
    Nothing,
}

impl MaybeLocal {
    pub fn is_empty(&self) -> bool {
        matches!(self, MaybeLocal::Nothing)
    }

    pub fn to_local(&self) -> Option<Local> {
        match self {
            MaybeLocal::Some(local) => Some(local.clone()),
            MaybeLocal::Nothing => None,
        }
    }

    /// v8::MaybeLocal::ToLocalChecked: panic on `Nothing` (the V8
    /// convention — the caller asserts the operation cannot fail).
    pub fn to_local_checked(self) -> Local {
        match self {
            MaybeLocal::Some(local) => local,
            MaybeLocal::Nothing => panic!("MaybeLocal::to_local_checked on Nothing"),
        }
    }
}

/// RAII marker grouping a set of local handles. Advisory under `Rc`; kept so
/// V8-idiom code compiles unchanged.
#[derive(Debug, Default)]
pub struct HandleScope(());

impl HandleScope {
    pub fn new() -> Self {
        Self(())
    }
}

/// RAII marker that can promote one inner `Local` to the enclosing scope
/// (v8::EscapableHandleScope). Under `Rc` the promotion is a clone.
#[derive(Debug, Default)]
pub struct EscapableHandleScope(());

impl EscapableHandleScope {
    pub fn new() -> Self {
        Self(())
    }

    pub fn escape(&self, local: Local) -> Local {
        local
    }
}
