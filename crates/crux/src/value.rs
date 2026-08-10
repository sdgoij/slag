//! ECMAScript language values (spec 6.1).

use std::fmt;

use crate::bigint::BigInt;
use crate::handle::Handle;
use crate::string::JsString;
use crate::symbol::Symbol;

/// An ECMAScript language value.
///
/// Phase 1 covers the seven primitive types; the Object type joins in Phase 5
/// with the object model (ch. 10). The derived `PartialEq` is Rust structural
/// equality; JavaScript semantics use `ops::same_value` / `ops::is_strictly_equal`.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Undefined,
    Null,
    Boolean(bool),
    Number(f64),
    BigInt(Handle<BigInt>),
    String(Handle<JsString>),
    Symbol(Handle<Symbol>),
}

/// The `Type` abstract operation (spec 7.2.1) for the Phase 1 value set.
pub fn type_of(value: &Value) -> &'static str {
    match value {
        Value::Undefined => "undefined",
        Value::Null => "null",
        Value::Boolean(_) => "boolean",
        Value::Number(_) => "number",
        Value::BigInt(_) => "bigint",
        Value::String(_) => "string",
        Value::Symbol(_) => "symbol",
    }
}

/// `IsCallable` (spec 7.2.3): no primitive is callable. Object support joins
/// in Phase 5.
pub fn is_callable(_value: &Value) -> bool {
    false
}

/// `IsConstructor` (spec 7.2.4): no primitive is a constructor.
pub fn is_constructor(_value: &Value) -> bool {
    false
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
