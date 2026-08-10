//! ECMAScript error types (spec ch. 17, 20).

use std::fmt;

/// The six ECMAScript native error types (spec ch. 17, 20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    EvalError,
    RangeError,
    ReferenceError,
    SyntaxError,
    TypeError,
    UriError,
}

/// An ECMAScript error before it is wrapped in a spec `Error` object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsError {
    pub kind: ErrorKind,
    pub message: String,
}

impl JsError {
    pub const fn new(kind: ErrorKind, message: String) -> Self {
        Self { kind, message }
    }
}

impl fmt::Display for JsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for JsError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_error_displays_kind_and_message() {
        let err = JsError::new(ErrorKind::TypeError, "boom".into());
        assert_eq!(err.to_string(), "TypeError: boom");
    }

    #[test]
    fn js_error_is_a_std_error() {
        let err = JsError::new(ErrorKind::SyntaxError, "oops".into());
        let boxed: Box<dyn std::error::Error> = Box::new(err.clone());
        assert_eq!(boxed.to_string(), "SyntaxError: oops");
    }

    #[test]
    fn error_kind_covers_all_native_errors() {
        let kinds = [
            ErrorKind::EvalError,
            ErrorKind::RangeError,
            ErrorKind::ReferenceError,
            ErrorKind::SyntaxError,
            ErrorKind::TypeError,
            ErrorKind::UriError,
        ];
        assert_eq!(kinds.len(), 6);
    }
}
