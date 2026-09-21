//! Handle types for the V8-shaped API: [`Local`], [`Global`], [`MaybeLocal`],
//! and the (advisory) handle-scope markers.

use crux::handle::Handle;
use crux::object::JsObject;
use crux::string::JsString;
use crux::value::Value;

/// A scoped handle over a language value (v8::Local).
///
/// A `Local` is a value, not a pointer into a scope-owned region: it copies
/// freely, it is valid for as long as the value it names is rooted, and it
/// needs no handle-scope discipline. [`HandleScope`] and
/// [`EscapableHandleScope`] exist so V8-idiom code compiles unchanged; they
/// are markers.
#[derive(Debug, Clone, Copy, PartialEq)]
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
        crux::value::is_callable(&self.0)
    }

    /// Whether the value is constructible (spec 7.2.4).
    pub fn is_constructor(&self) -> bool {
        crux::value::is_constructor(&self.0)
    }

    pub fn as_boolean(&self) -> Option<bool> {
        self.0.as_boolean()
    }

    pub fn as_number(&self) -> Option<f64> {
        self.0.as_number()
    }

    /// The string's lossy UTF-8 rendering when the value is a String.
    pub fn as_string(&self) -> Option<String> {
        self.0.as_string().map(|s| s.to_string_lossy())
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
/// exists (v8::Global).
///
/// "Persistent" has to mean it: a `Global` generally lives in host memory the
/// conservative stack scan cannot see, so it holds a [`crux::heap::Pin`] and
/// is a root of every collection until it is dropped. Without that, a
/// collection could free the box and the handle would silently alias whatever
/// reused the address.
#[derive(Debug)]
pub struct Global {
    value: Value,
    /// Held only for its `Drop`: the pin is what makes the handle persistent,
    /// and releasing it on drop is the whole of its API.
    #[allow(dead_code)]
    pin: crux::heap::Pin,
}

impl Global {
    /// A persistent handle over a value.
    pub fn new(value: Local) -> Self {
        Self::pinning(value.0)
    }

    fn pinning(value: Value) -> Self {
        Self {
            value,
            pin: crux::heap::pin(value),
        }
    }

    /// An empty handle (v8::Global::Empty).
    pub fn empty() -> Self {
        Self::pinning(Value::Undefined)
    }

    pub fn is_empty(&self) -> bool {
        self.value.is_undefined()
    }

    pub fn get(&self) -> Local {
        Local(self.value)
    }

    pub fn reset(&mut self, local: Local) {
        *self = Self::pinning(local.0);
    }

    pub fn clear(&mut self) {
        *self = Self::pinning(Value::Undefined);
    }
}

impl Clone for Global {
    fn clone(&self) -> Self {
        Self::pinning(self.value)
    }
}

/// A maybe-value: `Nothing` when an operation failed and a pending exception
/// was set (v8::MaybeLocal).
#[derive(Debug)]
pub enum MaybeLocal {
    Some(Local),
    Nothing,
}

impl Clone for MaybeLocal {
    fn clone(&self) -> Self {
        *self
    }
}

impl Copy for MaybeLocal {}

impl MaybeLocal {
    pub fn is_empty(&self) -> bool {
        matches!(self, MaybeLocal::Nothing)
    }

    pub fn to_local(&self) -> Option<Local> {
        match self {
            MaybeLocal::Some(local) => Some(*local),
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

/// RAII marker grouping a set of local handles. A [`Local`] is a value, so
/// nothing here needs a region to stay valid; the type exists so V8-idiom code
/// compiles unchanged.
#[derive(Debug, Default)]
pub struct HandleScope(());

impl HandleScope {
    pub fn new() -> Self {
        Self(())
    }
}

/// RAII marker that can promote one inner `Local` to the enclosing scope
/// (v8::EscapableHandleScope). Nothing has to be promoted — every `Local`
/// outlives the scope that made it — so this is the identity.
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
