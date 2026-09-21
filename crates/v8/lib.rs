//! The `v8` crate's API, implemented by Slag.
//!
//! A `v8`-crate consumer — `deno_core`, `serde_v8`, and everything above them —
//! compiles against these names. Behind them is Slag's engine, called directly:
//! no FFI, no `binding.cc`, no `bindgen`, no V8 build. Those exist in
//! `rusty_v8` only to cross into C++, and here there is nothing to cross.
//!
//! # Sunset note
//!
//! This crate is a migration bridge, not a second product. It exists so a host
//! can move off V8 without a rewrite; it stops growing once hosts use `slag`
//! directly, and it is deleted at the end of that migration.
//!
//! # How the shapes differ from the crate this stands in for
//!
//! Three differences are deliberate and visible to a consumer:
//!
//! - Handles are `Clone`, not `Copy`. Engine values are `Rc`-backed, so a
//!   handle cannot be a plain pointer. Code that relies on implicit copies
//!   needs an explicit `.clone()`.
//! - Instance methods live on `Local<T>`, and static constructors on the tag
//!   `T` — `Object::new(scope)` there, `array.get(scope, key)` here. The tag
//!   carries no payload, so it cannot host an instance method.
//! - An operation that needs a realm needs it *in reach*: a handle does not
//!   carry one, so the bridge keeps the thread's entered realm in a slot that
//!   `ContextScope` maintains.

mod context;
mod data;
mod handle;
mod isolate;
mod object;
mod primitives;
mod property;
mod realm;
mod scope;
mod script;
mod value;

pub use context::ContextOptions;
pub use data::*;
pub use handle::{Global, Handle, Local, MaybeLocal};
pub use isolate::{CreateParams, Isolate, OwnedIsolate};
pub use primitives::{NewStringType, WriteFlags, null, undefined};
pub use property::*;
pub use scope::{
    CallbackScope, ContextScope, HandleScope, NewHandleScope, PinCallbackScope, PinScope,
    PinnedRef, ScopeInit, ScopeStorage,
};
pub use script::ScriptOrigin;

use std::rc::Rc;

use runtime::api;

/// The realm a scope operates on.
///
/// The bridge's isolate records the realm most recently entered through a
/// `ContextScope`; the engine tracks its own copy.
pub(crate) fn realm_of(scope: &Isolate) -> Rc<api::Context> {
    scope
        .current_context()
        .or_else(realm::current)
        .expect("bridge bug: operation needs an entered context")
}

/// The realm entered on this thread.
///
/// For the operations the crate we stand in for declares without a scope, so
/// that they keep that signature here.
pub(crate) fn realm_current() -> Rc<api::Context> {
    realm::current().expect("bridge: operation needs an entered context")
}

/// Set `error` as the scope's pending exception, the way the crate we stand in
/// for reports a failed operation that has no error return.
pub(crate) fn throw(scope: &Isolate, error: &crux::error::JsError) {
    let value = match scope.current_context() {
        Some(realm) => realm
            .with_agent(|agent| runtime::builtins::error::to_throwable(agent, error))
            .unwrap_or(crux::value::Value::Undefined),
        None => crux::value::Value::Undefined,
    };
    scope.engine().set_pending_exception(value);
}

#[cfg(test)]
mod tests {
    use crate::{Context, ContextScope, CreateParams, Isolate, Script, String};

    /// The example from the crate this stands in for, run on Slag: a scope, a
    /// context, a string, a compiled script, and a result read back as text.
    #[test]
    fn the_upstream_docs_example_runs_on_slag() {
        let isolate = &mut Isolate::new(CreateParams);

        crate::scope!(let scope, isolate);
        let context = Context::new(scope, Default::default());
        let scope = &mut ContextScope::new(scope, context);

        let code = String::new(scope, "'Hello' + ' World!'").expect("string");
        assert_eq!(code.to_rust_string_lossy(scope), "'Hello' + ' World!'");

        let script = Script::compile(scope, code, None).expect("compile");
        let result = script.run(scope).expect("run");
        let result = result.to_string(scope).expect("to string");
        assert_eq!(result.to_rust_string_lossy(scope), "Hello World!");
    }

    /// The tag hierarchy is a `Deref` chain, so a method declared on `Value` is
    /// reachable from a handle tagged with anything below it. This is the
    /// substitute for the C++ inheritance the shapes rely on.
    #[test]
    fn a_value_method_is_reachable_through_the_tag_chain() {
        let isolate = &mut Isolate::new(CreateParams);
        crate::scope!(let scope, isolate);
        let context = Context::new(scope, Default::default());
        let scope = &mut ContextScope::new(scope, context);

        let text = String::new(scope, "hello").expect("string");
        // `is_string` is declared on `Local<Value>`; `Array` sits three tags
        // below it in the chain.
        let array: crate::Local<'_, crate::data::Array> = text.cast();
        assert!(array.is_string());
        assert!(!array.is_number());
    }

    /// A lone surrogate is an ordinary code unit to the engine, and the string
    /// API has to report it as one. A UTF-8 round trip anywhere in the path
    /// would substitute U+FFFD and lose it, which is the failure this pins.
    #[test]
    fn a_lone_surrogate_survives_the_string_api() {
        let isolate = &mut Isolate::new(CreateParams);
        crate::scope!(let scope, isolate);
        let context = Context::new(scope, Default::default());
        let scope = &mut ContextScope::new(scope, context);

        let text =
            String::new_from_two_byte(scope, &[0xD800, 0x0041], crate::NewStringType::Normal)
                .expect("string");

        // Code units, not characters: the surrogate is one and `A` is another.
        assert_eq!(text.length(), 2);
        assert_eq!(text.to_utf16(), vec![0xD800, 0x0041]);

        // U+FFFD belongs in the lossy rendering, and only there.
        assert_eq!(text.to_rust_string_lossy(scope), "\u{FFFD}A");
    }
}
