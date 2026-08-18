//! JSON helpers (v8::JSON).

use crux::error::JsError;

use super::context::Context;
use super::handle::Local;
use super::object::global_function;

/// JSON helpers (v8::JSON).
pub struct Json;

impl Json {
    /// v8::JSON::Parse: parse a JSON string into a value.
    pub fn parse(context: &Context, source: &str) -> Result<Local, JsError> {
        let parse = global_function(context, "JSON", "parse")?;
        context.try_call(&parse, &Local::undefined(), &[Local::string(source)])
    }

    /// v8::JSON::Stringify: serialize a value to a JSON string.
    pub fn stringify(context: &Context, value: &Local) -> Result<Local, JsError> {
        let stringify = global_function(context, "JSON", "stringify")?;
        context.try_call(&stringify, &Local::undefined(), std::slice::from_ref(value))
    }
}
