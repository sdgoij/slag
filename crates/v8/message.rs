//! The message a host makes from an exception, or the engine makes about a
//! stalled top-level await (`v8::Message`).
//!
//! Every answer here comes from the *error object* in the crate we stand in
//! for: `Isolate::CreateMessage` reads a start position, an end position and the
//! script back off the exception (`ComputeLocationFromException`,
//! `v8/src/execution/isolate.cc:3640`) and computes the line and column from
//! them. Slag's error objects carry no such properties, so the position comes
//! from the bridge's own record ([`crate::position`], made where the error was
//! thrown) and the text from the exception the message names.
//!
//! The exception is not the only source: a message minted for a stalled
//! top-level await has no thrown value behind it, so its text is the template
//! V8 fills in for it and its two position answers are the ones a message with
//! no recorded position gives.
//!
//! Two deliberate gaps, both recorded in `.notes/embedding.md` §9:
//!
//! - **`get_stack_trace` is `None`.** V8 answers with a trace only when the
//!   embedder asked for one (`SetCaptureStackTraceForUncaughtExceptions`); the
//!   bridge accepts that setting and carries it nowhere, so a message here has
//!   no trace to hand back. The one place `deno_core` asks, it hands the answer
//!   to an inspector that refuses.
//! - **The text is read the ordinary way off the value.** V8's rendering of an
//!   exception reads `name` and `message` as *data* properties so that no host
//!   code runs while an error is being formatted; here they are read with a
//!   [[Get]], so a getter a host installed on one of them does run. V8's
//!   `#<CtorName>` form for an object whose `toString` is the default is not
//!   reproduced either: a non-error object is described by its built-in tag.

use crux::value::ValueKind;
use runtime::api;

use crate::data::{Message, StackTrace, String, Value};
use crate::handle::{Local, LocalHandle, Payload};
use crate::position::Position;
use crate::scope::PinScope;

/// The rendering of the one message the bridge mints from a template rather
/// than from a thrown value: V8's `kTopLevelAwaitStalled`
/// (`v8/src/common/message-template.h:383`), whose text is this — it has no
/// placeholder to fill.
const TOP_LEVEL_AWAIT_STALLED: &str = "Top-level await promise never resolved";

impl LocalHandle<'_, Message> {
    /// The message text (v8::Message::Get).
    ///
    /// The message V8 makes with
    /// [`Exception::create_message`](crate::Exception::create_message) holds the
    /// uncaught-exception rendering: the `Uncaught %` template
    /// (`v8/src/common/message-template.h:25`) over a side-effect-free string of
    /// the exception (`MessageHandler::GetMessage`,
    /// `v8/src/execution/messages.cc:188`). This is that rendering.
    pub fn get<'a>(&self, scope: &PinScope<'a, '_>) -> Local<'a, String> {
        if matches!(self.payload(), Payload::TemplateMessage { .. }) {
            return Local::from_engine(api::Local::string(TOP_LEVEL_AWAIT_STALLED));
        }
        Local::from_engine(api::Local::string(format!(
            "Uncaught {}",
            side_effect_free(scope, self.engine())
        )))
    }

    /// The resource name of the script the error came from
    /// (v8::Message::GetScriptResourceName), when the bridge recorded one.
    ///
    /// `None` for an error with no recorded position — every runtime error, and
    /// a compile whose host passed no origin — and for one whose origin named
    /// the script with something other than a string. A message minted for a
    /// stalled top-level await answers the module's own name instead, which is
    /// what V8 fills that message with.
    pub fn get_script_resource_name<'a>(
        &self,
        scope: &PinScope<'a, '_>,
    ) -> Option<Local<'a, Value>> {
        if let Payload::TemplateMessage { module } = self.payload() {
            let name = module.name()?;
            return Some(Local::from_engine(api::Local::string(&name)));
        }
        let name = recorded(scope, self.engine())?.name?;
        Some(Local::from_engine(api::Local::string(&*name)))
    }

    /// The number, 1-based, of the line the error is on
    /// (v8::Message::GetLineNumber).
    ///
    /// `None` when the bridge recorded no position, which is also what the crate
    /// we stand in for answers when it cannot name a line. A message minted for a
    /// stalled top-level await answers the line the suspended body is at.
    pub fn get_line_number(&self, scope: &PinScope<'_, '_>) -> Option<usize> {
        if let Payload::TemplateMessage { module } = self.payload() {
            return stalled_location(module).map(|location| location.line as usize);
        }
        Some(recorded(scope, self.engine())?.line as usize)
    }

    /// The index, 0-based, within the line of the first character of the error
    /// (v8::Message::GetStartColumn).
    ///
    /// `Message::kNoColumnInfo` — 0 — when no position was recorded, which the
    /// crate we stand in for declares as 0.
    pub fn get_start_column(&self) -> usize {
        if let Payload::TemplateMessage { module } = self.payload() {
            // The engine's columns are 1-based and this answer is V8's, which
            // counts from zero.
            return stalled_location(module)
                .map_or(0, |location| location.column.saturating_sub(1) as usize);
        }
        // The crate we stand in for declares this one without a scope, so the
        // isolate it asks is the one whose realm this thread has entered; with
        // none entered there is no record to find.
        let Some(realm) = crate::realm::current() else {
            return 0;
        };
        let isolate = crate::scope::isolate_of(realm);
        isolate
            .position_of(self.engine())
            .map_or(0, |position| position.column as usize)
    }

    /// The trace the message carries (v8::Message::GetStackTrace).
    ///
    /// `None`: see the module documentation.
    pub fn get_stack_trace<'a>(&self, _scope: &PinScope<'a, '_>) -> Option<Local<'a, StackTrace>> {
        None
    }
}

/// The position a message minted for a stalled top-level await reports: the
/// module's own script and the offset its body is suspended at, which is what
/// V8 fills that message with (the language a host prints is
/// `Top-level await promise never resolved`, and the position is the failing
/// `await`).
///
/// `None` for a module that is not suspended, which a minted message only exists
/// for while it is.
fn stalled_location(module: &api::Module) -> Option<crux::SourceLocation> {
    module
        .stalled_top_level_await_offset()
        .map(|offset| module.source_offset_to_location(offset))
}

/// The position recorded for the exception a message names, if any.
fn recorded(scope: &PinScope<'_, '_>, exception: &api::Local) -> Option<Position> {
    scope.position_of(exception)
}

/// A string for `value` that runs no host code, which is what V8 formats the
/// uncaught-exception message with (`Object::NoSideEffectsToString`,
/// `v8/src/objects/objects.cc:719`).
fn side_effect_free(scope: &PinScope<'_, '_>, value: &api::Local) -> std::string::String {
    match value.value().kind() {
        ValueKind::String(text) => text.to_string_lossy(),
        ValueKind::Symbol(symbol) => crux::symbol::descriptive_string(&symbol),
        ValueKind::Object(_) | ValueKind::Function(_) => object_text(scope, value),
        // `ToString` is what V8 reaches for every other primitive, and it is
        // total on them except for Symbol, which V8 spells out itself above.
        ValueKind::Undefined
        | ValueKind::Null
        | ValueKind::Boolean(_)
        | ValueKind::Number(_)
        | ValueKind::BigInt(_) => crux::convert::to_string(value.value())
            .unwrap_or_else(|_| unreachable!("ToString is total on the other primitives"))
            .to_string_lossy(),
    }
}

/// The text for an object or a callable.
fn object_text(scope: &PinScope<'_, '_>, value: &api::Local) -> std::string::String {
    let realm = crate::realm_of(scope);
    if is_error(&realm, value) {
        // V8's `NoSideEffectsErrorToString`: `name`, then ": ", then `message`,
        // with whichever of the two is empty dropped — and the message alone
        // when the name is.
        let name = text_property(&realm, value, "name");
        let message = text_property(&realm, value, "message");
        return match (name.is_empty(), message.is_empty()) {
            (true, _) => message,
            (false, true) => name,
            (false, false) => format!("{name}: {message}"),
        };
    }
    // `[object Tag]` over the engine's own built-in tag (spec 20.1.3.6 steps
    // 4-14), which is the tag V8 prints. V8 also lets a string `@@toStringTag`
    // override the tag, which a bridge property read cannot reach: the key is a
    // symbol and this bridge reads properties by name.
    match api::Object::builtin_tag(&realm, value) {
        Ok(tag) => format!("[object {tag}]"),
        // A value the engine will not describe — a revoked proxy's target, an
        // object it cannot ToObject — is still an object and nothing more.
        Err(_) => "[object Object]".to_string(),
    }
}

/// Whether the value is an Error instance: V8's `IsErrorObject` checks the
/// [[ErrorData]] slot, and that slot is what the engine brands errors with.
fn is_error(realm: &api::Context, value: &api::Local) -> bool {
    value
        .value()
        .as_object()
        .is_some_and(|object| realm.with_agent(|agent| agent.error_data.contains(&object.id())))
}

/// A property of an object as text, empty when it is absent or not a string.
fn text_property(realm: &api::Context, value: &api::Local, name: &str) -> std::string::String {
    api::Object::get(realm, value, name)
        .ok()
        .and_then(|property| property.as_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{eval, in_context};
    use crate::{
        Context, ContextScope, CreateParams, Exception, Global, Isolate, Script, ScriptOrigin,
    };

    /// Compile `source` under `origin` and make the message V8 would make from
    /// the resulting exception.
    fn message_of_compile_error<'s>(
        scope: &mut PinScope<'s, '_, ()>,
        source: &str,
        origin: Option<&ScriptOrigin<'_>>,
    ) -> Local<'s, Message> {
        let code = String::new(scope, source).expect("string");
        assert!(
            Script::compile(scope, code, origin).is_none(),
            "the source compiles, so there is no error to ask about"
        );
        let pending = scope
            .engine()
            .pending_exception()
            .map(api::Local::from)
            .expect("the compile left its error pending");
        Exception::create_message(scope, Local::from_engine(pending))
    }

    /// Compile `source` under `origin` and keep the exception it threw.
    fn compile_error(
        scope: &mut PinScope<'_, '_, ()>,
        source: &str,
        origin: Option<&ScriptOrigin<'_>>,
    ) -> api::Local {
        let code = String::new(scope, source).expect("string");
        assert!(
            Script::compile(scope, code, origin).is_none(),
            "the source compiles, so there is no error to keep"
        );
        scope
            .engine()
            .pending_exception()
            .map(api::Local::from)
            .expect("the compile left its error pending")
    }

    /// A compile error answers the position the bridge recorded for it: the
    /// script's name as the host gave it, and the line and column of the token
    /// the parser stopped at.
    #[test]
    fn a_compile_error_answers_its_position() {
        in_context!(scope, {
            let name = String::new(scope, "file.js").expect("string");
            let origin = ScriptOrigin::new(
                scope,
                name.into(),
                0,
                0,
                false,
                -1,
                None,
                false,
                false,
                false,
                None,
            );
            // The `)` is the third character of the second line.
            let message = message_of_compile_error(scope, "let a = 1;\n  )\n", Some(&origin));

            let resource = message
                .get_script_resource_name(scope)
                .expect("the origin named the script");
            let resource = Local::<String>::try_from(resource).expect("a string");
            assert_eq!(resource.to_rust_string_lossy(scope), "file.js");
            // One-based line, zero-based column.
            assert_eq!(message.get_line_number(scope), Some(2));
            assert_eq!(message.get_start_column(), 2);
        });
    }

    /// The line and column follow the source rather than the error: the same
    /// error further down the file reports a bigger line.
    #[test]
    fn the_position_follows_the_source() {
        in_context!(scope, {
            let message = message_of_compile_error(scope, "\n\n\n)\n", None);
            assert_eq!(message.get_line_number(scope), Some(4));
            assert_eq!(message.get_start_column(), 0);
            assert!(message.get_script_resource_name(scope).is_none());
        });
    }

    /// A host's origin offsets shift the position the way V8 shifts it.
    #[test]
    fn the_origin_offsets_shift_the_position() {
        in_context!(scope, {
            let name = String::new(scope, "gen.ts").expect("string");
            let origin = ScriptOrigin::new(
                scope,
                name.into(),
                40,
                7,
                false,
                -1,
                None,
                false,
                false,
                false,
                None,
            );
            let message = message_of_compile_error(scope, ")", Some(&origin));
            assert_eq!(message.get_line_number(scope), Some(41));
            assert_eq!(message.get_start_column(), 7);
        });
    }

    /// The record belongs to the isolate and the error's identity, not to the
    /// scope the error was thrown in, so a message made in a later scope still
    /// answers the position.
    #[test]
    fn a_position_survives_the_scope_it_was_recorded_in() {
        let isolate = &mut Isolate::new(CreateParams::default());
        crate::scope!(let handle_scope, isolate);
        let context = Context::new(handle_scope, Default::default());
        let scope = &mut ContextScope::new(handle_scope, context);

        let name = String::new(scope, "gen.ts").expect("string");
        let origin = ScriptOrigin::new(
            scope,
            name.into(),
            0,
            0,
            false,
            -1,
            None,
            false,
            false,
            false,
            None,
        );
        let held = {
            crate::scope!(let inner, &mut ***scope);
            let exception = compile_error(inner, "\n)\n", Some(&origin));
            Global::<Value>::new(inner, Local::from_engine(exception))
        };

        let message = Exception::create_message(scope, held.get(scope));
        assert_eq!(message.get_line_number(scope), Some(2));
        assert_eq!(message.get_start_column(), 0);
        assert!(message.get_script_resource_name(scope).is_some());
    }

    /// An error nothing recorded a position for answers `None` rather than a
    /// position that may name another script, and the trace a message would
    /// carry is absent because nothing captures one.
    #[test]
    fn an_error_without_a_recorded_position_answers_none() {
        in_context!(scope, {
            let boom = String::new(scope, "boom").expect("string");
            let exception = Exception::type_error(scope, boom);
            let message = Exception::create_message(scope, exception);

            assert!(message.get_script_resource_name(scope).is_none());
            assert!(message.get_line_number(scope).is_none());
            assert_eq!(message.get_start_column(), 0);
            assert!(message.get_stack_trace(scope).is_none());
        });
    }

    /// The text is the uncaught rendering of the exception: an error prints as
    /// `name: message`, and every other value prints as the side-effect-free
    /// string V8 would use for it.
    #[test]
    fn the_text_is_the_uncaught_rendering() {
        in_context!(scope, {
            let boom = String::new(scope, "boom").expect("string");
            let cases = [
                (
                    "an Error",
                    Exception::error(scope, boom),
                    "Uncaught Error: boom",
                ),
                (
                    "a subclass",
                    eval(scope, "new TypeError('bad')"),
                    "Uncaught TypeError: bad",
                ),
                ("a string", eval(scope, "'boom'"), "Uncaught boom"),
                ("a number", eval(scope, "42"), "Uncaught 42"),
                ("a bigint", eval(scope, "1n"), "Uncaught 1"),
                (
                    "a symbol",
                    eval(scope, "Symbol('tag')"),
                    "Uncaught Symbol(tag)",
                ),
                ("null", eval(scope, "null"), "Uncaught null"),
                ("undefined", eval(scope, "undefined"), "Uncaught undefined"),
                ("an array", eval(scope, "[1, 2]"), "Uncaught [object Array]"),
                (
                    "a plain object",
                    eval(scope, "({})"),
                    "Uncaught [object Object]",
                ),
                (
                    "a callable",
                    eval(scope, "(() => {})"),
                    "Uncaught [object Function]",
                ),
            ];
            for (what, exception, expected) in cases {
                let message = Exception::create_message(scope, exception);
                assert_eq!(
                    message.get(scope).to_rust_string_lossy(scope),
                    expected,
                    "for {what}"
                );
            }
        });
    }

    /// An error's name and message decide its text, and an error with neither
    /// prints as the empty string V8's rendering leaves.
    #[test]
    fn an_errors_text_comes_from_its_name_and_message() {
        in_context!(scope, {
            let cases = [
                ("new Error()", "Uncaught Error"),
                ("new Error('m')", "Uncaught Error: m"),
                (
                    "Object.assign(new Error('m'), { name: 'Custom' })",
                    "Uncaught Custom: m",
                ),
                (
                    "Object.assign(new Error(), { name: 'Custom' })",
                    "Uncaught Custom",
                ),
                (
                    "(() => { const e = new Error('m'); e.message = 42; return e })()",
                    "Uncaught Error",
                ),
            ];
            for (source, expected) in cases {
                let exception = eval(scope, source);
                let message = Exception::create_message(scope, exception);
                assert_eq!(
                    message.get(scope).to_rust_string_lossy(scope),
                    expected,
                    "for {source}"
                );
            }
        });
    }
}
