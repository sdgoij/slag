//! The `Exception`/`Error` facade (rquickjs-shaped).

use std::fmt;

use crux::error::{ErrorKind, JsError};

use crate::value::Value;

/// An exception surfaced from an eval or call (rquickjs's `Error`).
#[derive(Debug, Clone)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    /// The thrown language value, when it was a user `throw`.
    thrown: Option<Value>,
}

impl Error {
    pub(crate) fn from_js_error(error: JsError) -> Self {
        let kind = error.kind;
        let message = error.message;
        let thrown = error.value.map(Value::from_crux);
        Self {
            kind,
            message,
            thrown,
        }
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    /// The value a user `throw` carried, when the exception was one.
    pub fn thrown(&self) -> Option<Value> {
        self.thrown
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for Error {}

impl From<JsError> for Error {
    fn from(error: JsError) -> Self {
        Self::from_js_error(error)
    }
}
