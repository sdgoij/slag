//! JSON (`v8::json`).
//!
//! The engine's `JSON` is the one this calls, so these are conversions and the
//! error handling a host expects — not a second parser.

use runtime::api;

use crate::data::{String as JsString, Value};
use crate::handle::Local;
use crate::scope::PinScope;

/// Parse `json_string` (v8::json::Parse).
///
/// `None` means the text is not JSON, with the engine's SyntaxError pending —
/// the shape a host's `TryCatch` is written around.
pub fn parse<'s>(
    scope: &PinScope<'s, '_>,
    json_string: Local<'_, JsString>,
) -> Option<Local<'s, Value>> {
    let realm = crate::realm_of(scope);
    let text = json_string.to_rust_string_lossy(scope);
    match api::Json::parse(&realm, &text) {
        Ok(value) => Some(Local::from_engine(value)),
        Err(error) => {
            crate::throw(scope, &error);
            None
        }
    }
}

/// Serialize `json_object` (v8::json::Stringify).
///
/// `None` means the value could not be serialized — a cycle or a BigInt, which
/// the engine throws for — with that error pending.
///
/// One quirk is reproduced rather than smoothed over: a value JSON leaves out
/// (a function, a symbol) serializes to *undefined*, and the crate we stand in
/// for renders whatever the serializer produced as a string, so a host there —
/// and here — reads the text `undefined` rather than an empty handle.
pub fn stringify<'s>(
    scope: &PinScope<'s, '_>,
    json_object: Local<'_, Value>,
) -> Option<Local<'s, JsString>> {
    let realm = crate::realm_of(scope);
    match api::Json::stringify(&realm, json_object.engine()) {
        Ok(value) => Some(match value.as_string() {
            Some(text) => Local::from_engine(api::Local::string(text)),
            None => Local::from_engine(api::Local::string("undefined")),
        }),
        Err(error) => {
            crate::throw(scope, &error);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Local;
    use crate::data::{Object, String as JsString, Value};
    use crate::test_support::in_context;

    #[test]
    fn parsing_and_serializing_round_trip_through_the_engine() {
        in_context!(scope, {
            let text = JsString::new(scope, r#"{"a":[1,2],"b":"x"}"#).expect("string");
            let value = super::parse(scope, text).expect("parse");
            let object = Local::<Object>::try_from(value).expect("object");
            let key = JsString::new(scope, "b").expect("string").into();
            let inner = object.get(scope, key).expect("get");
            assert_eq!(inner.to_rust_string_lossy(scope), "x");

            let text = super::stringify(scope, value).expect("stringify");
            assert_eq!(text.to_rust_string_lossy(scope), r#"{"a":[1,2],"b":"x"}"#);
        });
    }

    /// Bad JSON is a pending exception, not a panic: a host's `TryCatch` is
    /// what reads it.
    #[test]
    fn bad_json_parses_to_nothing_with_the_error_pending() {
        in_context!(scope, {
            let text = JsString::new(scope, "{").expect("string");
            assert!(super::parse(scope, text).is_none());
            assert!(scope.engine().has_pending_exception());
        });
    }

    /// A value JSON leaves out is the text `undefined`, as the crate we stand
    /// in for renders it.
    #[test]
    fn a_value_json_leaves_out_serializes_to_undefined_text() {
        in_context!(scope, {
            let code = JsString::new(scope, "(function () {})").expect("string");
            let script = crate::Script::compile(scope, code, None).expect("compile");
            let value: Local<'_, Value> = script.run(scope).expect("run");
            let text = super::stringify(scope, value).expect("stringify");
            assert_eq!(text.to_rust_string_lossy(scope), "undefined");
        });
    }
}
