//! Native errors (`v8::Exception`).
//!
//! Creating one makes the value and nothing else, as in the crate we stand in
//! for: throwing is `scope.throw_exception`, which the caller does.

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
        Self::create(scope, message, "%Error%")
    }

    /// A new `TypeError` (v8::Exception::TypeError).
    pub fn type_error<'s>(
        scope: &PinScope<'s, '_, ()>,
        message: Local<'_, String>,
    ) -> Local<'s, Value> {
        Self::create(scope, message, "%TypeError%")
    }

    /// A new `RangeError` (v8::Exception::RangeError).
    pub fn range_error<'s>(
        scope: &PinScope<'s, '_, ()>,
        message: Local<'_, String>,
    ) -> Local<'s, Value> {
        Self::create(scope, message, "%RangeError%")
    }

    /// A new `SyntaxError` (v8::Exception::SyntaxError).
    pub fn syntax_error<'s>(
        scope: &PinScope<'s, '_, ()>,
        message: Local<'_, String>,
    ) -> Local<'s, Value> {
        Self::create(scope, message, "%SyntaxError%")
    }

    /// A new `ReferenceError` (v8::Exception::ReferenceError).
    pub fn reference_error<'s>(
        scope: &PinScope<'s, '_, ()>,
        message: Local<'_, String>,
    ) -> Local<'s, Value> {
        Self::create(scope, message, "%ReferenceError%")
    }

    fn create<'s>(
        scope: &PinScope<'s, '_, ()>,
        message: Local<'_, String>,
        ctor_name: &str,
    ) -> Local<'s, Value> {
        // A message that is not text leaves the error without one; the error
        // itself is still the right error.
        let text = message.engine().as_string().unwrap_or_default();
        let mut isolate = scope.isolate_ptr();
        match api::Exception::create_with(isolate.engine_mut(), ctor_name, &text) {
            Ok(value) => Local::from_engine(value),
            // The engine's constructors fall back to a plain string for a value
            // it cannot throw, so this is unreachable outside a realm with no
            // error constructors at all.
            Err(_) => Local::from_engine(api::Local::undefined()),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::data::String as JsString;
    use crate::test_support::in_context;

    /// Making one is not throwing one (v8::Exception::Error and its siblings
    /// only *make* the value). deno's `is_instance_of_error` builds an empty
    /// `Error` to read `Error.prototype` while translating an exception, so a
    /// create that also threw would replace the exception being translated.
    #[test]
    fn creating_an_error_leaves_the_isolate_alone() {
        in_context!(scope, {
            let boom = JsString::new(scope, "boom").expect("string");
            let exception = super::Exception::type_error(scope, boom);
            assert!(exception.is_object());
            assert!(!scope.engine().has_pending_exception());
            assert_eq!(exception.to_rust_string_lossy(scope), "TypeError: boom");
        });
    }

    /// The error deno's fast path throws is built entirely through this crate —
    /// `v8::Exception::error`, the prototype re-parented onto
    /// `TypeError.prototype`, an own `name` — and deno reads the stack back off
    /// it. The stack is a real capture, so it carries the frame the error was
    /// made in; the header is not asserted because V8 formats it when `.stack`
    /// is read (see §9's open item on that).
    #[test]
    fn a_natively_built_error_carries_the_frame_it_was_made_in() {
        use crate::data::{Object, Value};
        use crate::function::{FunctionCallbackArguments, ReturnValue};
        use crate::test_support::{bind, eval, in_context};
        use crate::{Function, Local};

        const MESSAGE: &str = "expected type `v8::data::Boolean`, got `v8::data::Value`";

        fn build<'s>(
            scope: &mut crate::scope::PinScope<'s, '_>,
            _args: FunctionCallbackArguments<'s>,
            rv: ReturnValue<'s>,
        ) {
            let mut isolate = scope.isolate_ptr();
            let exception =
                runtime::api::Exception::create_with(isolate.engine_mut(), "%Error%", MESSAGE)
                    .expect("error");
            let value: Local<Value> = Local::from_engine(exception);
            let object = value.try_cast::<Object>().expect("object");
            if let Some(prototype) = crate::realm_of(scope).intrinsic("%TypeError.prototype%") {
                object.set_prototype(scope, Local::from_engine(prototype.into()));
            }
            let key = crate::data::String::new(scope, "name").expect("string");
            let name = crate::data::String::new(scope, "TypeError").expect("string");
            object.create_data_property(scope, key.into(), name.into());
            rv.set(object.into());
        }

        in_context!(scope, {
            let function = Function::builder(build).build(scope).expect("function");
            bind(scope, "build", function.cast::<Value>());
            let stack = eval(scope, "build().stack").to_rust_string_lossy(scope);
            assert!(
                stack.contains(MESSAGE) && stack.contains("\n    at <anonymous>:1:"),
                "the message and the frame it was made in: {stack}"
            );
        });
    }
}
