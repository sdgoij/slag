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
//! Two differences are deliberate and visible to a consumer:
//!
//! - Instance methods live on `Local<T>`, and static constructors on the tag
//!   `T` — `Object::new(scope)` there, `array.get(scope, key)` here. The tag
//!   carries no payload, so it cannot host an instance method.
//! - An operation that needs a realm needs it *in reach*: a handle does not
//!   carry one, so the bridge keeps the thread's entered realm in a slot that
//!   `ContextScope` maintains.

#[allow(non_snake_case)] // the crate we stand in for names the module `V8`.
pub mod V8;
mod array_buffer;
mod bigint;
mod context;
pub mod cppgc;
mod data;
mod exception;
mod external;
mod external_references;
pub mod fast_api;
mod fixed_array;
mod function;
mod handle;
mod heap;
pub mod inspector;
mod interceptor;
mod isolate;
pub mod json;
mod message;
mod microtask;
mod module;
mod object;
mod platform;
mod position;
mod primitive_array;
mod primitives;
mod private;
mod promise;
mod property;
mod property_descriptor;
mod realm;
mod scope;
mod script;
pub mod script_compiler;
mod serialize;
#[cfg(feature = "simdutf")]
pub mod simdutf;
mod snapshot;
mod stack_trace;
mod store;
mod support;
mod template;
#[cfg(test)]
mod test_support;
mod unbound_script;
mod value;
mod wasm;
mod weak;

pub use array_buffer::BackingStoreDeleterCallback;
pub use context::ContextOptions;
pub use data::*;
pub use exception::Exception;
pub use external_references::ExternalReference;
pub use function::{
    ConstructorBehavior, FunctionBuilder, FunctionCallback, FunctionCallbackArguments,
    FunctionCallbackInfo, FunctionCallbackInfoParts, ReturnValue, SideEffectType,
};
pub use handle::{Global, Handle, Local, MaybeLocal};
pub use heap::{GCCallbackFlags, GCType, GcCallback, HeapSpaceStatistics, HeapStatistics};
pub use interceptor::{
    IndexedPropertyDefinerCallback, IndexedPropertyDeleterCallback,
    IndexedPropertyDescriptorCallback, IndexedPropertyEnumeratorCallback,
    IndexedPropertyGetterCallback, IndexedPropertyHandlerConfiguration,
    IndexedPropertyQueryCallback, IndexedPropertySetterCallback, NamedPropertyDefinerCallback,
    NamedPropertyDeleterCallback, NamedPropertyDescriptorCallback, NamedPropertyEnumeratorCallback,
    NamedPropertyGetterCallback, NamedPropertyHandlerConfiguration, NamedPropertyQueryCallback,
    NamedPropertySetterCallback, PropertyCallbackArguments,
};
pub use isolate::{
    CreateParams, HostImportModuleDynamicallyCallback,
    HostImportModuleWithPhaseDynamicallyCallback, HostInitializeImportMetaObjectCallback, Isolate,
    IsolateHandle, NearHeapLimitCallback, OwnedIsolate, PrepareStackTraceCallback,
    PromiseRejectCallback, TimeZoneDetection, UnsafeRawIsolatePtr, WasmAsyncResolvePromiseCallback,
    WasmAsyncSuccess,
};
pub use json::{parse as json_parse, stringify as json_stringify};
pub use microtask::MicrotaskQueue;
pub use module::{ModuleImportPhase, ModuleStatus, SyntheticModuleEvaluationSteps};
pub use object::IntegrityLevel;
pub use platform::{
    IdleTask, Platform, PlatformImpl, Task, new_custom_platform, new_default_platform,
    new_single_threaded_default_platform, new_unprotected_default_platform,
};
pub use primitives::{
    NewStringType, OneByteConst, ValueView, ValueViewData, WriteFlags, latin1_to_utf8, null,
    undefined,
};
pub use promise::{PromiseRejectEvent, PromiseRejectMessage, PromiseState};
pub use property::*;
pub use property_descriptor::PropertyDescriptor;
pub use weak::{TracedReference, Weak, WeakCallbackInfo};

/// The version this bridge reports (`v8::VERSION_STRING`).
pub use V8::VERSION_STRING;

/// When the job queues drain (v8::MicrotasksPolicy).
///
/// The engine defaults to `Explicit` where the crate we stand in for defaults
/// to `Auto`: a host inherits no draining it did not ask for, and says so when
/// it wants one. `Auto` drains when the outermost embedder entry returns; a
/// callback that calls back in is a re-entrant entry, so no job runs under a
/// callback still on the stack.
pub use runtime::api::MicrotasksPolicy;
pub use scope::{
    AllowJavascriptExecutionScope, CallbackScope, ContextScope, EscapableHandleScope, GetIsolate,
    HandleScope, NewAllowJavascriptExecutionScope, NewCallbackScope, NewEscapableHandleScope,
    NewHandleScope, NewTryCatch, PinCallbackScope, PinScope, PinnedRef, ScopeInit, ScopeStorage,
    TryCatch,
};
pub use script::ScriptOrigin;
pub use script_compiler::CachedData;
pub use serialize::{
    ValueDeserializer, ValueDeserializerHeap, ValueDeserializerHelper, ValueDeserializerImpl,
    ValueSerializer, ValueSerializerHeap, ValueSerializerHelper, ValueSerializerImpl,
};
pub use snapshot::{FunctionCodeHandling, StartupData};
pub use support::{
    BackingStore, DefaultTag, MapFnFrom, MapFnTo, Rawable, SharedRef, UniquePtr, UniqueRef,
    UnitType,
};
pub use wasm::{CompiledWasmModule, WasmStreaming};

use runtime::api;

/// The realm a scope operates on.
///
/// The bridge's isolate records the realm most recently entered through a
/// `ContextScope`; the engine tracks its own copy.
pub(crate) fn realm_of(scope: &Isolate) -> api::Context {
    scope
        .current_context()
        .or_else(realm::current)
        .expect("bridge bug: operation needs an entered context")
}

/// The realm entered on this thread.
///
/// For the operations the crate we stand in for declares without a scope, so
/// that they keep that signature here.
pub(crate) fn realm_current() -> api::Context {
    realm::current().expect("bridge: operation needs an entered context")
}

/// Set `error` as the scope's pending exception, the way the crate we stand in
/// for reports a failed operation that has no error return.
///
/// Answers the value that became the exception, which a caller that knows where
/// the error came from hands to [`throw_at`].
pub(crate) fn throw(scope: &Isolate, error: &crux::error::JsError) -> api::Local {
    let value = match scope.current_context() {
        Some(realm) => realm
            .with_agent(|agent| runtime::builtins::error::to_throwable(agent, error))
            .unwrap_or(crux::value::Value::Undefined),
        None => crux::value::Value::Undefined,
    };
    scope.engine().set_pending_exception(value);
    api::Local::from(value)
}

/// The same, for a caller that knows which source the error was parsed from —
/// the compile paths. The error's span indexes that source, so its position is
/// recorded for the `v8::Message` a host later makes from the exception; see
/// [`position`].
pub(crate) fn throw_at(
    scope: &Isolate,
    error: &crux::error::JsError,
    source: &str,
    origin: &position::Origin,
) {
    let thrown = throw(scope, error);
    position::record(scope, &thrown, source, error.span, origin);
}

/// The largest byte length of a typed array whose elements are stored inside the
/// object rather than off-heap (`v8::TYPED_ARRAY_MAX_SIZE_IN_HEAP`).
///
/// Zero, and truthfully so: every view the engine builds is backed by a buffer's
/// own storage, so nothing lives in the object and a host sizing a scratch
/// buffer for `get_contents_raw_parts` has nothing to size it for.
pub const TYPED_ARRAY_MAX_SIZE_IN_HEAP: usize = 0;

#[cfg(test)]
mod tests {
    use crate::{Context, ContextScope, CreateParams, Global, Isolate, Script, String};

    /// The example from the crate this stands in for, run on Slag: a scope, a
    /// context, a string, a compiled script, and a result read back as text.
    #[test]
    fn the_upstream_docs_example_runs_on_slag() {
        let isolate = &mut Isolate::new(CreateParams::default());

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
        let isolate = &mut Isolate::new(CreateParams::default());
        crate::scope!(let scope, isolate);
        let context = Context::new(scope, Default::default());
        let scope = &mut ContextScope::new(scope, context);

        let text = String::new(scope, "hello").expect("string");
        // `is_string` is declared on `Local<Value>`; `Array` sits three tags
        // below it in the chain. The tag is deliberately wrong here, which is
        // why the retag is the unchecked one.
        let array: crate::Local<'_, crate::data::Array> = text.retag();
        assert!(array.is_string());
        assert!(!array.is_number());
    }

    /// A tag *reference* is a receiver, which is the shape the crate we stand in
    /// for has and the one the host's conversions are written against: they call
    /// methods on `&v8::Value` and reinterpret a `&v8::Value` as a `&v8::String`
    /// once the predicate agrees (`deno/libs/core/runtime/ops.rs:265-278`). Both
    /// work here because a tag's address is the handle's own payload.
    #[test]
    fn a_tag_reference_is_a_receiver() {
        with_value("'a string'", |value| {
            let reference: &Value = value;
            // The address check is the runtime half of this test: if the tag
            // carried anything but the handle's payload, these two would differ
            // and the reads below would be reading some other value.
            assert!(
                std::ptr::eq(reference.payload(), value.payload()),
                "a tag reference names the handle's own payload"
            );

            assert!(reference.is_string());

            // SAFETY: the predicate above is the check the reinterpretation
            // needs and it just passed — the same contract the host relies on.
            let text: &crate::data::String = unsafe { std::mem::transmute(reference) };
            // `length` is `String`'s own; `type_repr` is declared on `Value`,
            // four tags up, so this is also the inherited lookup through a tag
            // reference.
            assert_eq!(text.length(), 8);
            assert_eq!(text.type_repr(), "string");
        });
    }

    /// A lone surrogate is an ordinary code unit to the engine, and the string
    /// API has to report it as one. A UTF-8 round trip anywhere in the path
    /// would substitute U+FFFD and lose it, which is the failure this pins.
    #[test]
    fn a_lone_surrogate_survives_the_string_api() {
        let isolate = &mut Isolate::new(CreateParams::default());
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

    /// A scoped script handle dies with the scope that owns its text, so a
    /// persistent handle over one has to keep the text itself and hand it back
    /// to whatever scope it is read in.
    #[test]
    fn a_persistent_script_survives_the_scope_it_was_compiled_in() {
        let isolate = &mut Isolate::new(CreateParams::default());
        crate::scope!(let scope, isolate);
        let context = Context::new(scope, Default::default());
        let scope = &mut ContextScope::new(scope, context);

        let persistent = {
            crate::scope!(let inner, &mut ***scope);
            let code = String::new(inner, "6 * 7").expect("string");
            let script = Script::compile(inner, code, None).expect("compile");
            Global::<Script>::new(inner, script)
        };

        let value = persistent.get(scope).run(scope).expect("run");
        assert_eq!(
            value
                .to_string(scope)
                .expect("to string")
                .to_rust_string_lossy(scope),
            "42"
        );
    }

    use crate::Local;
    use crate::data::Value;

    /// Evaluate `source` in a fresh isolate and context, and hand the result to
    /// `check`.
    fn with_value(source: &str, check: impl FnOnce(&Local<'_, Value>)) {
        let isolate = &mut Isolate::new(CreateParams::default());
        crate::scope!(let scope, isolate);
        let context = Context::new(scope, Default::default());
        let scope = &mut ContextScope::new(scope, context);

        let code = String::new(scope, source).expect("string");
        let script = Script::compile(scope, code, None).expect("compile");
        let value = script.run(scope).expect("run");
        check(&value);
    }

    /// Every brand comes from the engine's own instance table, so an object a
    /// script hands a foreign prototype to is not that brand — which is the
    /// answer the crate we stand in for gives, and not the one a walk of the
    /// prototype chain would.
    #[test]
    fn a_borrowed_prototype_is_not_a_brand() {
        with_value("Object.create(Map.prototype)", |value| {
            assert!(!value.is_map())
        });
        with_value("Object.create(Error.prototype)", |value| {
            assert!(!value.is_native_error())
        });
        with_value("Object.create(Date.prototype)", |value| {
            assert!(!value.is_date())
        });
    }

    #[test]
    fn the_container_brands_are_recognized() {
        with_value("new Map()", |value| assert!(value.is_map()));
        with_value("new Set()", |value| assert!(value.is_set()));
        with_value("new WeakMap()", |value| assert!(value.is_weak_map()));
        with_value("new WeakSet()", |value| assert!(value.is_weak_set()));
        with_value("new Date()", |value| assert!(value.is_date()));
        with_value("/x/", |value| assert!(value.is_reg_exp()));
        with_value("Promise.resolve(1)", |value| assert!(value.is_promise()));
        with_value("new Map().entries()", |value| {
            assert!(value.is_map_iterator())
        });
        with_value("new Set().values()", |value| {
            assert!(value.is_set_iterator())
        });
        with_value("(function () { return arguments; })()", |value| {
            assert!(value.is_arguments_object())
        });
        with_value("(function* () {})()", |value| {
            assert!(value.is_generator_object())
        });
        // An async generator is a generator instance to the engine, but its
        // state lives in a table of its own, so it is not one here either.
        with_value("(async function* () {})()", |value| {
            assert!(!value.is_generator_object())
        });
    }

    #[test]
    fn the_buffer_brands_are_recognized() {
        with_value("new ArrayBuffer(8)", |value| {
            assert!(value.is_array_buffer());
            assert!(!value.is_shared_array_buffer());
            assert!(!value.is_array_buffer_view());
        });
        with_value("new SharedArrayBuffer(8)", |value| {
            assert!(value.is_shared_array_buffer());
            assert!(!value.is_array_buffer());
        });
        with_value("new DataView(new ArrayBuffer(8))", |value| {
            assert!(value.is_data_view());
            assert!(value.is_array_buffer_view());
            assert!(!value.is_typed_array());
        });
        with_value("new Uint8Array(4)", |value| {
            assert!(value.is_typed_array());
            assert!(value.is_array_buffer_view());
            assert!(!value.is_data_view());
        });
    }

    #[test]
    fn a_boxed_primitive_is_told_apart_from_its_primitive() {
        with_value("1", |value| {
            assert!(value.is_number());
            assert!(!value.is_number_object());
        });
        with_value("new Number(1)", |value| {
            assert!(value.is_number_object());
            assert!(!value.is_number());
        });
        with_value("new Boolean(true)", |value| {
            assert!(value.is_boolean_object())
        });
        with_value("Object(Symbol())", |value| {
            assert!(value.is_symbol_object())
        });
        with_value("Object(1n)", |value| assert!(value.is_big_int_object()));
        with_value("new String('x')", |value| {
            assert!(value.is_string_object());
            assert!(!value.is_string());
        });
        with_value("'x'", |value| {
            assert!(value.is_string());
            assert!(!value.is_string_object());
        });
    }

    #[test]
    fn the_integer_predicates_are_ranges() {
        with_value("2147483647", |value| assert!(value.is_int32()));
        with_value("2147483648", |value| assert!(!value.is_int32()));
        with_value("4294967295", |value| {
            assert!(value.is_uint32());
            assert!(!value.is_int32());
        });
        with_value("1.5", |value| {
            assert!(!value.is_int32());
            assert!(!value.is_uint32());
        });
    }

    /// The kind is a property of the function itself, and an async generator is
    /// neither of the two kinds the crate we stand in for names.
    #[test]
    fn the_function_kinds_are_read_per_function() {
        with_value("(function () {})", |value| {
            assert!(value.is_function());
            assert!(!value.is_generator_function());
            assert!(!value.is_async_function());
        });
        with_value("(function* () {})", |value| {
            assert!(value.is_generator_function());
            assert!(!value.is_async_function());
        });
        with_value("(async function () {})", |value| {
            assert!(value.is_async_function());
            assert!(!value.is_generator_function());
        });
        with_value("(async function* () {})", |value| {
            assert!(!value.is_async_function());
            assert!(!value.is_generator_function());
        });
    }

    /// `[[ErrorData]]` is the slot, not the prototype, so a subclass instance is
    /// a native error and a plain object wearing `Error.prototype` is not.
    #[test]
    fn a_native_error_is_recognized_by_its_internal_slot() {
        with_value("new TypeError('x')", |value| {
            assert!(value.is_native_error())
        });
        with_value("class E extends Error {}; new E()", |value| {
            assert!(value.is_native_error())
        });
    }

    #[test]
    fn type_repr_names_the_type() {
        with_value("null", |value| assert_eq!(value.type_repr(), "null"));
        with_value("undefined", |value| {
            assert_eq!(value.type_repr(), "undefined")
        });
        with_value("1n", |value| assert_eq!(value.type_repr(), "bigint"));
        with_value("Symbol('s')", |value| {
            assert_eq!(value.type_repr(), "symbol")
        });
        with_value("[]", |value| assert_eq!(value.type_repr(), "array"));
        with_value("(function () {})", |value| {
            assert_eq!(value.type_repr(), "function")
        });
        with_value("new Map()", |value| assert_eq!(value.type_repr(), "Map"));
        with_value("new Uint8Array(2)", |value| {
            assert_eq!(value.type_repr(), "Uint8Array")
        });
        with_value("new (class extends Uint8Array {})(2)", |value| {
            assert_eq!(value.type_repr(), "Uint8Array")
        });
        with_value("(function* () {})", |value| {
            assert_eq!(value.type_repr(), "Generator function")
        });
        with_value("new Proxy({}, {})", |value| {
            assert_eq!(value.type_repr(), "Proxy")
        });
    }

    /// The crate has one version statement: the constant a host reports and the
    /// function one asks answer the same thing, and neither claims to be V8.
    #[test]
    fn the_version_string_is_what_get_version_reports() {
        assert_eq!(crate::VERSION_STRING, crate::V8::get_version());
        assert!(crate::VERSION_STRING.starts_with("slag (v8 API "));
    }
}
