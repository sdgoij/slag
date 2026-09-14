//! Statement completions (spec 6.2.3): the values a statement evaluation
//! produces. Every completion carries a `[[Value]]`; `break`/`continue`/`return`
//! additionally transfer control, and `throw` carries the thrown language
//! value. Internal `JsError`s propagate separately via `Result` and are caught
//! by `try`/`catch` as the thrown value.

use crux::error::JsError;
use crux::heap::{GcAny, Trace};
use crux::string::AtomId;
use crux::value::Value;

#[derive(Debug, Clone)]
pub enum Completion {
    /// A normal completion; `~empty~` statements produce *undefined*.
    Normal(Value),
    /// NormalCompletion(~empty~): a declaration, empty, or debugger
    /// statement, whose value `UpdateEmpty` fills from the enclosing
    /// statement list (spec 14.2.2 step 5).
    Empty,
    /// `break label` / `continue label`: `target` is the label (or `None`),
    /// `value` is `~empty~` until `UpdateEmpty` fills it from the enclosing
    /// statement list or loop.
    Break {
        target: Option<AtomId>,
        value: Option<Value>,
    },
    Continue {
        target: Option<AtomId>,
        value: Option<Value>,
    },
    Return(Value),
    Throw(Value),
}

impl Trace for Completion {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        match self {
            Completion::Normal(value) | Completion::Return(value) | Completion::Throw(value) => {
                value.trace(visit)
            }
            Completion::Break { value, .. } | Completion::Continue { value, .. } => {
                if let Some(value) = value {
                    value.trace(visit);
                }
            }
            Completion::Empty => {}
        }
    }
}

impl Completion {
    pub fn normal() -> Self {
        Completion::Normal(Value::Undefined)
    }

    /// UpdateEmpty (spec 6.2.3.4): fill an `~empty~` completion value.
    pub fn update_empty(self, value: Value) -> Self {
        match self {
            Completion::Empty => Completion::Normal(value),
            Completion::Break {
                target,
                value: None,
            } => Completion::Break {
                target,
                value: Some(value),
            },
            Completion::Continue {
                target,
                value: None,
            } => Completion::Continue {
                target,
                value: Some(value),
            },
            other => other,
        }
    }

    /// The completion's `[[Value]]`, with `~empty~` read as *undefined*.
    pub fn value(&self) -> Value {
        match self {
            Completion::Normal(value) => *value,
            Completion::Empty => Value::Undefined,
            Completion::Break { value, .. } | Completion::Continue { value, .. } => {
                (*value).unwrap_or(Value::Undefined)
            }
            Completion::Return(value) | Completion::Throw(value) => *value,
        }
    }
}

/// The error an ECMAScript `throw` becomes when it crosses into Rust. The
/// thrown value rides on [`JsError::value`], so a later `to_throwable` rethrows
/// it with its identity intact (an enclosing catch observes the original
/// value); the message is only a fallback for a host that prints the error
/// without rendering the value ([`crate::embed::Context::describe_thrown`]
/// renders it the way Node's uncaught handler does).
pub fn uncaught_error(value: Value) -> JsError {
    JsError::new(
        crux::error::ErrorKind::TypeError,
        "Uncaught exception".into(),
    )
    .with_value(value)
}

/// Convert the script-level completion into the evaluator's `Result`
/// (Phase 6): `Normal` yields its value; `return`/`break`/`continue` cannot
/// escape a script; `throw` becomes an error carrying the thrown value.
pub fn completion_to_result(completion: Completion) -> Result<Value, JsError> {
    match completion {
        Completion::Normal(value) => Ok(value),
        Completion::Empty => Ok(Value::Undefined),
        Completion::Throw(value) => Err(uncaught_error(value)),
        Completion::Return(_) | Completion::Break { .. } | Completion::Continue { .. } => {
            Err(JsError::new(
                crux::error::ErrorKind::SyntaxError,
                "Illegal control flow at the top level".into(),
            ))
        }
    }
}
