//! Private names (`v8::Private`).
//!
//! The engine has no private-name kind, so the bridge models one the way the
//! language allows: a private name is a **symbol** the isolate mints and keeps,
//! and a private property is a symbol-keyed property. That is invisible to the
//! walks a private name has to be invisible to — `Object.keys`, `for-in`,
//! `JSON.stringify`, `in`, `Object.getOwnPropertyNames` — but it is *not*
//! invisible to `Object.getOwnPropertySymbols`, and a V8 private name is not a
//! property at all. A host that hands one of these names to script is therefore
//! handing out something script can find on any object it shares: the property
//! is hidden from ordinary use and not from a script that goes looking. V8's own
//! hiding needs a private-name kind in the engine; this is the honest half-step,
//! and `.notes/embedding.md` §9 records it rather than leaving it to be
//! discovered.

use crate::data::{Private, String as JsString};
use crate::handle::Local;
use crate::scope::PinScope;

impl Private {
    /// The private name that goes with `name` (`v8::Private::ForApi`).
    ///
    /// One name, one private: the isolate keeps the symbol it minted for a
    /// description, so every request for the same description is the same name.
    /// That is what the call is for — a host and its script agreeing on a key —
    /// and what the crate's own documentation promises ("if a symbol with this
    /// name has not been retrieved in the same isolate before, it is created").
    ///
    /// A request with no name is the empty name, shared like any other: the
    /// crate's documentation warns that these names form a single namespace and
    /// should be qualified, which is the same hazard.
    ///
    /// The crate's `Private::name` — reading a name's description back — is
    /// absent: nothing has asked for it yet.
    pub fn for_api<'s>(
        scope: &PinScope<'s, '_, ()>,
        name: Option<Local<'s, JsString>>,
    ) -> Local<'s, Private> {
        let isolate = scope.isolate_ptr();
        // Code units, not text: a name is an arbitrary string, and the registry
        // has to tell two names apart exactly.
        let description = name
            .and_then(|name| name.engine().value().as_string())
            .map(|text| text.as_slice().to_vec());
        let value = isolate.private_symbol(description.as_deref());
        Local::<Private>::from_engine(value)
    }

    /// A fresh private name, one per call (`v8::Private::new`).
    ///
    /// Where [`for_api`](Self::for_api) dedupes by description — one description,
    /// one name — this consults no registry, so two calls with the same
    /// description are two distinct names, which is what the crate's own
    /// documentation says it is for. The isolate keeps what it minted, so the
    /// result is still recognized as a private name.
    pub fn new<'s>(
        scope: &PinScope<'s, '_, ()>,
        name: Option<Local<'s, JsString>>,
    ) -> Local<'s, Private> {
        let isolate = scope.isolate_ptr();
        // Code units, not text, for the same reason `for_api` reads them so.
        let description = name
            .and_then(|name| name.engine().value().as_string())
            .map(|text| text.as_slice().to_vec());
        let value = isolate.private_symbol_fresh(description.as_deref());
        Local::<Private>::from_engine(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Data, Object, Value};
    use crate::test_support::{bind, eval, eval_number, in_context};

    /// A name the isolate has not minted for a description again: same text,
    /// same name; different text, different name.
    #[test]
    fn one_description_is_one_name() {
        in_context!(scope, {
            let description = JsString::new(scope, "Deno#field").expect("string");
            let first = Private::for_api(scope, Some(description));
            let again = Private::for_api(scope, Some(description));
            assert_eq!(first, again);

            let other = Private::for_api(
                scope,
                Some(JsString::new(scope, "Deno#other").expect("string")),
            );
            assert_ne!(first, other);

            // And the data a name came from casts back to a name, which is the
            // question the crate's own snapshot machinery asks.
            let data = first.cast::<Data>();
            assert!(Local::<Private>::try_from(data).is_ok());
        });
    }

    /// A private property holds what was put there, reads as absent before that,
    /// and stays out of the ordinary walks. The last assertion is the
    /// divergence the module header records: `getOwnPropertySymbols` finds it,
    /// where a V8 private name would not be found at all.
    #[test]
    fn a_private_property_round_trips() {
        in_context!(scope, {
            let object = Local::<Object>::try_from(eval(scope, "({ a: 1 })")).expect("object");
            let key = Private::for_api(
                scope,
                Some(JsString::new(scope, "Deno#hidden").expect("string")),
            );
            assert!(object.get_private(scope, key).is_none());

            let value = eval(scope, "42");
            assert_eq!(object.set_private(scope, key, value), Some(true));
            let stored = object.get_private(scope, key).expect("stored");
            assert_eq!(stored, value);

            // A second name does not see the first one's property.
            let other = Private::for_api(
                scope,
                Some(JsString::new(scope, "Deno#other").expect("string")),
            );
            assert!(object.get_private(scope, other).is_none());

            let object: Local<'_, Value> = object.cast();
            bind(scope, "o", object);
            assert_eq!(eval_number(scope, "Object.keys(o).length"), 1.0);
            assert_eq!(
                eval_number(scope, "Object.getOwnPropertyNames(o).length"),
                1.0
            );
            assert_eq!(eval_number(scope, "('Deno#hidden' in o) ? 1 : 0"), 0.0);
            assert_eq!(
                eval_number(scope, "Object.getOwnPropertySymbols(o).length"),
                1.0,
                "the name is a property, which the module header states as a divergence"
            );
            assert_eq!(
                eval_number(scope, "o[Object.getOwnPropertySymbols(o)[0]]"),
                42.0
            );
        });
    }

    /// The one asymmetry the symbol-backed model carries, pinned: a name set on a
    /// prototype is readable through `[[Get]]` on a receiver that inherits it, and
    /// is not present in the receiver's own private table.
    #[test]
    fn a_private_name_is_read_through_the_chain_and_present_only_own() {
        in_context!(scope, {
            let key = Private::for_api(
                scope,
                Some(JsString::new(scope, "Deno#chain").expect("string")),
            );
            let prototype = Local::<Object>::try_from(eval(scope, "({})")).expect("object");
            assert_eq!(
                prototype.set_private(scope, key, eval(scope, "5")),
                Some(true)
            );
            let prototype_value: Local<'_, Value> = prototype.cast();
            bind(scope, "privatePrototype", prototype_value);

            let derived = Local::<Object>::try_from(eval(scope, "Object.create(privatePrototype)"))
                .expect("object");
            assert_eq!(
                derived.has_private(scope, key),
                Some(false),
                "a private name is present only on the receiver's own table"
            );
            let read = derived
                .get_private(scope, key)
                .expect("read through the chain");
            assert_eq!(read, eval(scope, "5"));
        });
    }

    /// `Private::new` is the other half of `for_api`: a fresh name on every call,
    /// even for a description just used, and still a private name to the cast that
    /// asks — which is what the isolate's second store is for.
    #[test]
    fn a_fresh_name_is_new_every_call_and_still_private() {
        in_context!(scope, {
            let description = JsString::new(scope, "Deno#fresh").expect("string");
            let first = Private::new(scope, Some(description));
            let second = Private::new(scope, Some(description));
            assert_ne!(first, second, "Private::new mints a new name every call");

            // The control: `for_api` deliberately does not.
            let for_api_first = Private::for_api(scope, Some(description));
            let for_api_second = Private::for_api(scope, Some(description));
            assert_eq!(for_api_first, for_api_second);

            // A fresh name is still recognized as a private name.
            let data = first.cast::<Data>();
            assert!(Local::<Private>::try_from(data).is_ok());
        });
    }

    /// The presence question is the own table, and the delete removes from it: a
    /// private property is absent before a set, present after, gone after a
    /// delete, and a second name never sees the first one's property. A fresh name
    /// works as a key the same way, which is the pairing this part adds.
    #[test]
    fn a_private_property_is_present_then_deleted() {
        in_context!(scope, {
            let object = Local::<Object>::try_from(eval(scope, "({})")).expect("object");
            let key = Private::for_api(
                scope,
                Some(JsString::new(scope, "Deno#present").expect("string")),
            );
            assert_eq!(object.has_private(scope, key), Some(false));

            assert_eq!(object.set_private(scope, key, eval(scope, "7")), Some(true));
            assert_eq!(object.has_private(scope, key), Some(true));

            // A different name does not see it, and a fresh name is usable too.
            let other = Private::for_api(
                scope,
                Some(JsString::new(scope, "Deno#other").expect("string")),
            );
            assert_eq!(object.has_private(scope, other), Some(false));
            let fresh = Private::new(
                scope,
                Some(JsString::new(scope, "Deno#freshkey").expect("string")),
            );
            assert_eq!(
                object.set_private(scope, fresh, eval(scope, "9")),
                Some(true)
            );
            assert_eq!(object.has_private(scope, fresh), Some(true));
            assert_eq!(object.delete_private(scope, fresh), Some(true));
            assert_eq!(object.has_private(scope, fresh), Some(false));

            assert_eq!(object.delete_private(scope, key), Some(true));
            assert_eq!(object.has_private(scope, key), Some(false));
            assert!(object.get_private(scope, key).is_none());
            // `[[Delete]]` succeeds on a property that is not there, so a second
            // delete answers `true` too: the answer is "gone", not "was present".
            assert_eq!(object.delete_private(scope, key), Some(true));
        });
    }
}
