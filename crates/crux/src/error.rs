//! ECMAScript error types (spec ch. 17, 20).

use std::fmt;

use crate::span::Span;

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
#[derive(Debug, Clone, PartialEq)]
pub struct JsError {
    pub kind: ErrorKind,
    pub message: String,
    /// The thrown language value, when the error came from a user `throw`
    /// (promise rejections and `try`/`catch` of async throws need it).
    pub value: Option<crate::value::Value>,
    /// The source span the error refers to, when known.
    pub span: Option<Span>,
}

impl JsError {
    pub const fn new(kind: ErrorKind, message: String) -> Self {
        Self {
            kind,
            message,
            value: None,
            span: None,
        }
    }

    /// Attach the thrown language value (a user `throw` crossing a Rust
    /// boundary).
    pub fn with_value(mut self, value: crate::value::Value) -> Self {
        self.value = Some(value);
        self
    }

    pub fn with_span(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
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
    fn js_error_carries_optional_span() {
        let span = Span::new(10, 20);
        let err = JsError::new(ErrorKind::SyntaxError, "oops".into()).with_span(span);
        assert_eq!(err.span, Some(span));
        assert_eq!(JsError::new(ErrorKind::SyntaxError, "x".into()).span, None);
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
