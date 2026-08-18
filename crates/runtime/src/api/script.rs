//! The V8-shaped [`Script`]: parse at compile time, evaluate at run time.

use crux::error::JsError;

use super::context::Context;
use super::handle::{Local, MaybeLocal};

/// A compiled Script (v8::Script). The source is parsed at [`compile`]
/// time so syntax errors surface there; [`run`](Self::run) evaluates it.
pub struct Script {
    source: String,
}

impl Script {
    /// Parse `source` in `context`'s realm. A syntax error is returned
    /// directly (the caller decides how to surface it).
    pub fn compile(context: &Context, source: &str) -> Result<Self, JsError> {
        crate::script::parse_script(source, context.realm().clone())?;
        Ok(Self {
            source: source.to_string(),
        })
    }

    /// Evaluate the script; on failure set the pending exception and return
    /// `Nothing` (v8::Script::Run semantics).
    pub fn run(&self, context: &Context) -> MaybeLocal {
        context.eval(&self.source)
    }

    /// Evaluate the script, returning the engine error directly.
    pub fn try_run(&self, context: &Context) -> Result<Local, JsError> {
        context.try_eval(&self.source)
    }
}
