//! ECMAScript language values (spec 6.1).

use std::fmt;

use crate::bigint::BigInt;
use crate::function::Function;
use crate::handle::Handle;
use crate::object::JsObject;
use crate::string::JsString;
use crate::symbol::Symbol;

/// An ECMAScript language value.
///
/// Phase 1 covers the seven primitive types; the Object and Function types
/// join in Phase 4 with the minimal object shell (the full object model
/// arrives in Phase 5). The derived `PartialEq` is Rust structural equality;
/// JavaScript semantics use `ops::same_value` / `ops::is_strictly_equal`.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Undefined,
    Null,
    Boolean(bool),
    Number(f64),
    BigInt(Handle<BigInt>),
    String(Handle<JsString>),
    Symbol(Handle<Symbol>),
    Object(Handle<JsObject>),
    Function(Handle<Function>),
}

/// The `Type` abstract operation (spec 7.2.1). Proxies over callable
/// functions report `function` like the spec's typeof.
pub fn type_of(value: &Value) -> &'static str {
    match value {
        Value::Undefined => "undefined",
        Value::Null => "null",
        Value::Boolean(_) => "boolean",
        Value::Number(_) => "number",
        Value::BigInt(_) => "bigint",
        Value::String(_) => "string",
        Value::Symbol(_) => "symbol",
        Value::Object(obj) => match &obj.kind {
            crate::object::ObjectKind::Proxy(slots)
                if slots
                    .target
                    .borrow()
                    .as_ref()
                    .map(is_callable)
                    .unwrap_or(false) =>
            {
                "function"
            }
            _ => "object",
        },
        Value::Function(_) => "function",
    }
}

/// `IsCallable` (spec 7.2.3): function values and proxies whose target is
/// callable.
pub fn is_callable(value: &Value) -> bool {
    match value {
        Value::Function(_) => true,
        Value::Object(obj) => match &obj.kind {
            crate::object::ObjectKind::Proxy(slots) => slots
                .target
                .borrow()
                .as_ref()
                .map(is_callable)
                .unwrap_or(false),
            _ => false,
        },
        _ => false,
    }
}

/// `IsConstructor` (spec 7.2.4): built-ins with a [[Construct]], ECMAScript
/// (non-arrow) functions, bound functions, and proxies whose target is a
/// constructor.
pub fn is_constructor(value: &Value) -> bool {
    match value {
        Value::Function(function) => function.is_constructor(),
        Value::Object(obj) => match &obj.kind {
            crate::object::ObjectKind::Proxy(slots) => slots
                .target
                .borrow()
                .as_ref()
                .map(is_constructor)
                .unwrap_or(false),
            _ => false,
        },
        _ => false,
    }
}

impl Value {
    /// The object handle when `self` is an Object value; `None` otherwise.
    /// Function values wrap their object side separately and report `None`.
    pub fn as_object(&self) -> Option<Handle<JsObject>> {
        match self {
            Value::Object(obj) => Some(obj.clone()),
            _ => None,
        }
    }
}

impl fmt::Display for Value {
    /// Diagnostics-only rendering; JavaScript string conversion is
    /// `convert::to_string`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Undefined => f.write_str("undefined"),
            Value::Null => f.write_str("null"),
            Value::Boolean(b) => write!(f, "{b}"),
            Value::Number(n) => write!(f, "{n}"),
            Value::BigInt(b) => write!(f, "{}", b.0),
            Value::String(s) => write!(f, "{s}"),
            Value::Symbol(s) => write!(f, "{}", crate::symbol::descriptive_string(s)),
            Value::Object(_) => f.write_str("[object Object]"),
            Value::Function(fun) => match &fun.name {
                Some(name) => write!(f, "function {name}"),
                None => f.write_str("function"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn number(n: f64) -> Value {
        Value::Number(n)
    }

    #[test]
    fn type_of_all_variants() {
        assert_eq!(type_of(&Value::Undefined), "undefined");
        assert_eq!(type_of(&Value::Null), "null");
        assert_eq!(type_of(&Value::Boolean(true)), "boolean");
        assert_eq!(type_of(&number(1.0)), "number");
        assert_eq!(
            type_of(&Value::BigInt(Handle::new(BigInt::from(1)))),
            "bigint"
        );
        assert_eq!(
            type_of(&Value::String(Handle::new(JsString::from_utf8("x")))),
            "string"
        );
        assert_eq!(
            type_of(&Value::Symbol(Handle::new(Symbol::new(None)))),
            "symbol"
        );
    }

    #[test]
    fn primitives_are_not_callable_or_constructible() {
        for v in [
            Value::Undefined,
            Value::Null,
            Value::Boolean(false),
            number(0.0),
            Value::BigInt(Handle::new(BigInt::from(0))),
            Value::String(Handle::new(JsString::from_utf8(""))),
            Value::Symbol(Handle::new(Symbol::new(None))),
        ] {
            assert!(!is_callable(&v));
            assert!(!is_constructor(&v));
        }
    }

    #[test]
    fn display_for_diagnostics() {
        assert_eq!(Value::Undefined.to_string(), "undefined");
        assert_eq!(Value::Null.to_string(), "null");
        assert_eq!(Value::Boolean(true).to_string(), "true");
        assert_eq!(number(1.5).to_string(), "1.5");
        assert_eq!(
            Value::String(Handle::new(JsString::from_utf8("hi"))).to_string(),
            "hi"
        );
        assert_eq!(
            Value::Symbol(Handle::new(Symbol::new(Some(JsString::from_utf8("k"))))).to_string(),
            "Symbol(k)"
        );
    }
}
