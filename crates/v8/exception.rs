//! Native errors (`v8::Exception`).
//!
//! Creating one sets the isolate's pending exception, as it does in the crate
//! we stand in for, where the caller throws what it made.

use runtime::api;

use crate::data::{Message, String, Value};
use crate::handle::{Local, Payload};
use crate::scope::PinScope;

/// The native error constructors (v8::Exception).
pub struct Exception;

impl Exception {
    /// The message to read about `exception`
    /// (v8::Exception::CreateMessage).
    ///
    /// Nothing is copied: V8 builds a record at the throw site and the message
    /// answers from it, so the handle this returns names the exception itself
    /// and every answer is read from it when a host asks — see
    /// [`message`](crate::message) for what each one is.
    pub fn create_message<'s>(
        _scope: &PinScope<'s, '_, ()>,
        exception: Local<'s, Value>,
    ) -> Local<'s, Message> {
        Local::from_payload(Payload::Value(exception.into_engine()))
    }

    /// A new `Error` (v8::Exception::Error).
    pub fn error<'s>(scope: &PinScope<'s, '_, ()>, message: Local<'_, String>) -> Local<'s, Value> {
        Self::throw_with(scope, message, api::Exception::throw_error)
    }

    /// A new `TypeError` (v8::Exception::TypeError).
    pub fn type_error<'s>(
        scope: &PinScope<'s, '_, ()>,
        message: Local<'_, String>,
    ) -> Local<'s, Value> {
        Self::throw_with(scope, message, api::Exception::throw_type_error)
    }

    /// A new `RangeError` (v8::Exception::RangeError).
    pub fn range_error<'s>(
        scope: &PinScope<'s, '_, ()>,
        message: Local<'_, String>,
    ) -> Local<'s, Value> {
        Self::throw_with(scope, message, api::Exception::throw_range_error)
    }

    /// A new `SyntaxError` (v8::Exception::SyntaxError).
    pub fn syntax_error<'s>(
        scope: &PinScope<'s, '_, ()>,
        message: Local<'_, String>,
    ) -> Local<'s, Value> {
        Self::throw_with(scope, message, api::Exception::throw_syntax_error)
    }

    /// A new `ReferenceError` (v8::Exception::ReferenceError).
    pub fn reference_error<'s>(
        scope: &PinScope<'s, '_, ()>,
        message: Local<'_, String>,
    ) -> Local<'s, Value> {
        Self::throw_with(scope, message, api::Exception::throw_reference_error)
    }

    fn throw_with<'s>(
        scope: &PinScope<'s, '_, ()>,
        message: Local<'_, String>,
        throw: fn(&mut api::Isolate, &str) -> Result<api::Local, crux::error::JsError>,
    ) -> Local<'s, Value> {
        // A message that is not text leaves the error without one; the error
        // itself is still the right error.
        let text = message.engine().as_string().unwrap_or_default();
        let mut isolate = scope.isolate_ptr();
        match throw(isolate.engine_mut(), &text) {
            Ok(value) => Local::from_engine(value),
            // The engine's constructors fall back to a plain string for a value
            // it cannot throw, so this is unreachable outside a realm with no
            // error constructors at all.
            Err(_) => Local::from_engine(api::Local::undefined()),
        }
    }
}
