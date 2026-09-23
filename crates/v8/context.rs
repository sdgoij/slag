//! Contexts: the realm a scope operates on (`v8::Context`).

use std::ffi::c_void;

use runtime::api;

use crate::MapFnTo;
use crate::String as JsString;
use crate::data::{Context, Function, Object, ObjectTemplate};
use crate::function::{ConstructorBehavior, FunctionCallbackArguments, ReturnValue};
use crate::handle::{Local, LocalHandle, Payload};
use crate::scope::PinScope;

/// The property V8 installs the console under, on the extras binding object and
/// on the global object.
const CONSOLE_PROPERTY: &str = "console";

/// The methods V8 installs on the console it hands out
/// (`Genesis::InitializeConsole`, `v8/src/init/bootstrapper.cc:5617`), in the
/// order it installs them.
const CONSOLE_METHODS: [&str; 23] = [
    "debug",
    "error",
    "info",
    "log",
    "warn",
    "dir",
    "dirxml",
    "table",
    "trace",
    "group",
    "groupCollapsed",
    "groupEnd",
    "clear",
    "count",
    "countReset",
    "assert",
    "profile",
    "profileEnd",
    "time",
    "timeLog",
    "timeEnd",
    "timeStamp",
    "context",
];

/// Options for [`Context::new`] (`v8::ContextOptions`).
///
/// Slag has no realm configuration yet; the field exists because hosts set it.
#[derive(Default)]
pub struct ContextOptions<'s> {
    pub global_template: Option<Local<'s, ObjectTemplate>>,
}

impl Context {
    /// Create a realm on the scope's isolate and leave it entered.
    ///
    /// The engine pushes the realm's bootstrap execution context when the
    /// context is created, so with one context per isolate the context is
    /// current for as long as it exists.
    #[allow(clippy::new_ret_no_self)]
    pub fn new<'s>(
        scope: &PinScope<'s, '_, ()>,
        options: ContextOptions<'_>,
    ) -> Local<'s, Context> {
        let mut isolate = scope.isolate_ptr();
        // A global template that carries a named property handler makes the
        // realm's global object a host-defined one, so the engine's internal
        // methods on it ask the host; without one the global stays ordinary.
        let handler = options
            .global_template
            .and_then(|template| template.named_property_handler());
        let context = match handler {
            Some(handler) => api::Context::new_with_global_ops(
                isolate.engine_mut(),
                Some(std::rc::Rc::new(crate::interceptor::GlobalHandler::new(
                    handler,
                ))),
            ),
            None => api::Context::new(isolate.engine_mut()),
        }
        .expect("bridge: creating a realm cannot fail outside OOM");
        // The engine's isolate owns the realm, so this handle only names it;
        // recording it here is what lets a scope-less operation find it.
        isolate.set_current_context(Some(context));
        // The engine makes the new realm current on its isolate; mirror that
        // here so operations with no scope to read it from can find it.
        crate::realm::enter(context);
        let handle = Local::from_payload(Payload::Context(context));
        // V8 fills a new context's extras binding object, and its global
        // object, as it creates the context (`Genesis::InitializeConsole`), so
        // a host finds the console without asking for either.
        install_console(scope, handle);
        handle
    }

    /// A context restored from a snapshot (`v8::Context::FromSnapshot`).
    ///
    /// A context slot the isolate's blob names becomes a fresh realm with that
    /// slot's attached data rooted in it, which the host then reads out through
    /// [`get_context_data_from_snapshot_once`]. A slot the blob does not name
    /// answers `None` — the same refusal the crate we stand in for makes with
    /// an empty `MaybeLocal`, so a host takes its own other branch. The realm
    /// which is passed over that way stays the isolate's current one, which is
    /// the realm a host that asked for a slot its blob does not have would have
    /// made anyway.
    ///
    /// A restored context gets what a new one gets — the engine's realm and the
    /// console `Context::new` installs — plus the blob's data: what the blob
    /// carries is the host's own attached state, not the engine's realm, which
    /// is rebuilt deterministically either way.
    ///
    /// [`get_context_data_from_snapshot_once`]: crate::HandleScope::get_context_data_from_snapshot_once
    pub fn from_snapshot<'s>(
        scope: &PinScope<'s, '_, ()>,
        context_snapshot_index: usize,
        options: ContextOptions<'_>,
    ) -> Option<Local<'s, Context>> {
        let mut isolate = scope.isolate_ptr();
        let handle = Self::new(scope, options);
        let context = handle.context();
        if !crate::snapshot::restore_context(&mut isolate, context, context_snapshot_index) {
            return None;
        }
        Some(handle)
    }
}

/// Fill a new context as V8 fills it: the extras binding object carries a
/// console, and the global object names the same one.
fn install_console<'s>(scope: &PinScope<'s, '_, ()>, context: Local<'s, Context>) {
    let extras = context.get_extras_binding_object(scope);
    let console = console_object(scope).unwrap_or_else(|| {
        // The same invariant the extras binding object's own creation states: a
        // realm that cannot make an ordinary object or a function is a bridge
        // bug, not a failure a host can act on.
        panic!("bridge: creating the console failed")
    });
    let realm = crate::realm_of(scope);
    // `JSObject::AddProperty(..., DONT_ENUM)`: writable and configurable, and
    // not enumerable, on both objects.
    for target in [extras, context.global(scope)] {
        if let Err(error) = api::Object::define(
            &realm,
            target.engine(),
            CONSOLE_PROPERTY,
            console.engine(),
            true,
            false,
            true,
        ) {
            panic!("bridge: defining the console failed: {error}");
        }
    }
}

/// The console V8 puts on a context's extras binding object
/// (`Genesis::InitializeConsole`, `v8/src/init/bootstrapper.cc:5617`).
///
/// Every method does nothing, which is what V8's own do when the isolate has no
/// console delegate: `ConsoleCall` returns before it reaches one
/// (`v8/src/builtins/builtins-console.cc:158`), and the crate we stand in for
/// exposes no way to install one. It matters that this one stays silent rather
/// than printing — a host that wraps it calls *both* consoles
/// (`deno_core`'s `callConsole`, `runtime/bindings.rs:1753`), so a bridge that
/// printed here would print every message twice.
///
/// The methods are built as V8 builds them: zero-length, named after the
/// property, enumerable — `deno_core`'s bootstrap walks them with `Object.keys`
/// (`01_core.js:756`), so a method that were not enumerable would be invisible
/// to it — and with no `[[Construct]]`, so `new console.log()` throws the
/// `TypeError` V8's throws. V8 builds these without a `.prototype` as well
/// (`CreateFunctionForBuiltinWithoutPrototype`); this bridge's function path
/// gives every function one, which is a shape difference it has everywhere and
/// not one a host acts on here.
///
/// V8 also tags the console (`InstallToStringTag(isolate_, console, "console")`),
/// which is absent for the reason §9 already records for
/// `get_constructor_name`: the engine's object API takes string keys, so a
/// symbol-keyed define means reaching past it and that gap is a named follow-up.
fn console_object<'s>(scope: &PinScope<'s, '_, ()>) -> Option<Local<'s, Object>> {
    let realm = crate::realm_of(scope);
    let console = api::Object::new(&realm).ok()?;
    for name in CONSOLE_METHODS {
        let template = Function::builder(console_method)
            .constructor_behavior(ConstructorBehavior::Throw)
            .into_template(scope);
        template.set_class_name(JsString::new(scope, name)?);
        let method = template.get_function(scope)?;
        match api::Object::set(&realm, &console, name, method.engine(), false) {
            Ok(true) => {}
            _ => return None,
        }
    }
    Some(Local::from_engine(console))
}

/// One of the console's methods, and what it does with no console delegate:
/// nothing (see [`console_object`]).
fn console_method(
    _scope: &mut PinScope<'_, '_>,
    _args: FunctionCallbackArguments,
    _rv: ReturnValue,
) {
}

/// The callback every console method this bridge installs is built from, as the
/// address a snapshot's external-reference table names.
///
/// A snapshot cannot carry a callback's address — it is a property of the process
/// that loads the blob — so it carries an index into the table the host rebuilds
/// for every load, and this callback is the *bridge's* own: the console is
/// installed on every context this bridge makes, so the host that hands over a
/// blob never had a chance to register it. The bridge therefore puts it in the
/// table itself; see [`engine_table`](crate::snapshot::engine_table). One
/// callback for every method, because the methods are one function and differ
/// only in the property name they are defined under.
pub(crate) fn console_callback() -> crate::function::FunctionCallback {
    console_method.map_fn_to()
}

impl<'s> LocalHandle<'s, Context> {
    /// The context's global object (`v8::Context::Global`).
    pub fn global(&self, _scope: &PinScope<'s, '_, ()>) -> Local<'s, Object> {
        Local::from_engine(self.context().global())
    }

    /// Store a host pointer in a slot on this context
    /// (`v8::Context::SetAlignedPointerInEmbedderData`).
    ///
    /// Slag's contexts have no embedder-data slots, so the bridge keeps them,
    /// on the isolate and keyed by this context. A slot that was never written
    /// reads back null, which is what the crate we stand in for answers too.
    pub fn set_aligned_pointer_in_embedder_data(&self, index: i32, value: *mut c_void) {
        self.slots_isolate()
            .set_context_slot(self.identity(), index, value as usize);
    }

    /// The host pointer in a slot on this context
    /// (`v8::Context::GetAlignedPointerFromEmbedderData`), null when nothing was
    /// stored there.
    pub fn get_aligned_pointer_from_embedder_data(&self, index: i32) -> *mut c_void {
        self.slots_isolate().context_slot(self.identity(), index) as *mut c_void
    }

    /// Forget every slot written on this context
    /// (`v8::Context::ClearAllSlots`).
    ///
    /// The crate clears the *values* it keeps for the context, including its own
    /// bookkeeping; the slots here are the host's pointers, so this forgets the
    /// pointers and the host keeps what they point at — which is the same
    /// contract, one side of it written down.
    pub fn clear_all_slots(&self) {
        self.slots_isolate().clear_context_slots(self.identity());
    }

    /// The context's extras binding object
    /// (`v8::Context::GetExtrasBindingObject`).
    ///
    /// There, V8 makes this object per context and fills it from the embedder's
    /// snapshot, with the console among its properties — `deno_core` reads
    /// `console` out of it (`runtime/bindings.rs:373`). Here it is created on the
    /// first ask, which [`Context::new`] makes as it creates the context, and it
    /// is the same object on every ask: a host stashing its own bindings on it
    /// needs that, and V8's is one object too.
    ///
    /// The rest of a snapshot's bindings are not here: this bridge cannot restore
    /// one (see [`SnapshotCreator`](crate::SnapshotCreator), whose `create_blob`
    /// says why), so what the host's bootstrap puts in the object is what it
    /// holds, and the host is the one that knows what that is.
    pub fn get_extras_binding_object<'a>(&self, scope: &PinScope<'a, '_, ()>) -> Local<'a, Object> {
        let isolate = self.slots_isolate();
        let key = self.identity();
        if let Some(held) = isolate.extras_binding(key) {
            return Local::from_engine(held);
        }
        let realm = crate::realm_of(scope);
        let object = match api::Object::new(&realm) {
            Ok(object) => object,
            Err(error) => {
                crate::throw(scope, &error);
                // The crate has no failure channel here either, so a realm that
                // cannot make an ordinary object is a bridge bug.
                panic!("bridge: creating the extras binding object failed: {error}");
            }
        };
        let handle: Local<'_, Object> = Local::from_engine(object);
        isolate.set_extras_binding(key, handle);
        handle
    }

    /// The isolate the slots live on.
    fn slots_isolate(&self) -> crate::Isolate {
        // SAFETY: a context lives in the agent of a live isolate, and the engine
        // isolate is the first field of `IsolateInner`, which is what makes the
        // two addresses the same — see `Isolate::from_engine_ptr`.
        unsafe { crate::Isolate::from_engine_ptr(self.context().isolate()) }
    }

    /// What this context's slots are keyed by: its global object's id, which is
    /// how the bridge already tells two contexts apart.
    fn identity(&self) -> u64 {
        self.context()
            .global()
            .value()
            .as_object()
            .map(|object| object.id())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::c_void;

    use super::CONSOLE_METHODS;
    use crate::data::Object;
    use crate::handle::Local;
    use crate::scope::PinScope;
    use crate::test_support::in_context;
    use crate::{Context, ContextOptions};

    /// The extras binding object is one per context and the same on every ask,
    /// it carries the console V8 puts there, the global object names that same
    /// console, and a host that puts something of its own there finds it again.
    #[test]
    fn the_extras_binding_object_is_one_per_context() {
        in_context!(scope, {
            let context = scope.get_current_context();
            let first = context.get_extras_binding_object(scope);
            let second = context.get_extras_binding_object(scope);
            assert_eq!(first, second, "the same object on every ask");

            // Both places `deno_core` reads it from: the extras binding object
            // (`runtime/bindings.rs:373`) and the global object
            // (`01_core.js:827`), which name the same console.
            let key = crate::String::new(scope, "console").unwrap();
            let console = first.get(scope, key.into()).expect("get");
            assert!(!console.is_undefined(), "the extras binding object has one");
            let key = crate::String::new(scope, "console").unwrap();
            let global = context.global(scope).get(scope, key.into()).expect("get");
            assert_eq!(global, console, "and the global object names it too");

            // What the host puts there it gets back, which is what the object is
            // for: a place for the bootstrap's own bindings.
            let key = crate::String::new(scope, "host").unwrap();
            first
                .set(scope, key.into(), crate::Number::new(scope, 1.0).into())
                .expect("set");
            let again = context.get_extras_binding_object(scope);
            assert_eq!(again, first);
        });
    }

    /// The value a script evaluates to, as text.
    fn evaluated(scope: &mut PinScope<'_, '_>, source: &str) -> String {
        crate::test_support::eval(scope, source).to_rust_string_lossy(scope)
    }

    /// One field of the property descriptor a script evaluates to, as text.
    fn descriptor_field(scope: &mut PinScope<'_, '_>, source: &str, field: &str) -> String {
        let descriptor = Local::<Object>::try_from(crate::test_support::eval(scope, source))
            .expect("a property descriptor");
        let key = crate::String::new(scope, field).expect("string");
        descriptor
            .get(scope, key.into())
            .expect("the field")
            .to_rust_string_lossy(scope)
    }

    /// The console has V8's methods, enumerably so — `deno_core`'s bootstrap
    /// walks them with `Object.keys` — and every one of them does nothing.
    #[test]
    fn the_console_has_v8s_methods_and_none_of_them_run() {
        in_context!(scope, {
            assert_eq!(
                evaluated(scope, "Object.keys(console).join()"),
                CONSOLE_METHODS.join(","),
                "every method is an own enumerable property"
            );
            assert_eq!(evaluated(scope, "typeof console.log"), "function");
            assert_eq!(evaluated(scope, "console.log.name"), "log");
            assert_eq!(evaluated(scope, "console.log.length"), "0");

            // What V8's do with no console delegate installed: nothing at all.
            assert_eq!(evaluated(scope, "String(console.log('gone'))"), "undefined");

            // No `[[Construct]]`, as V8 builds them, so `new` refuses.
            assert_eq!(
                evaluated(
                    scope,
                    "(() => { try { new console.log(); return 'constructed'; } \
                     catch (e) { return e.constructor.name; } })()"
                ),
                "TypeError"
            );

            // The console is not an enumerable property of the global object,
            // which is the attribute V8 gives it there.
            let on_global = "Object.getOwnPropertyDescriptor(globalThis, 'console')";
            assert_eq!(descriptor_field(scope, on_global, "enumerable"), "false");
            assert_eq!(descriptor_field(scope, on_global, "writable"), "true");
        });
    }

    /// An isolate that booted from source has no contexts to restore, and the
    /// entry point says so rather than handing back a context that is not the
    /// one a blob names.
    #[test]
    fn a_context_without_a_blob_cannot_be_restored() {
        in_context!(scope, {
            assert!(
                Context::from_snapshot(scope, 0, ContextOptions::default()).is_none(),
                "the isolate booted from source, so it has no snapshot to restore"
            );
        });
    }

    /// A host pointer lands in the slot it was put in and only there, and a slot
    /// the host never wrote reads back null rather than a stale neighbour.
    #[test]
    fn a_host_pointer_round_trips_through_a_context_slot() {
        in_context!(scope, {
            let context = scope.get_current_context();
            let pointer = 0x1234usize as *mut c_void;

            assert!(context.get_aligned_pointer_from_embedder_data(3).is_null());

            context.set_aligned_pointer_in_embedder_data(3, pointer);
            assert_eq!(context.get_aligned_pointer_from_embedder_data(3), pointer);
            assert!(context.get_aligned_pointer_from_embedder_data(4).is_null());

            // A second slot, so one write cannot be mistaken for another.
            let other = 0x5678usize as *mut c_void;
            context.set_aligned_pointer_in_embedder_data(4, other);
            assert_eq!(context.get_aligned_pointer_from_embedder_data(3), pointer);
            assert_eq!(context.get_aligned_pointer_from_embedder_data(4), other);
        });
    }

    /// Clearing the slots forgets every index on this context, and nothing else.
    #[test]
    fn clearing_the_slots_forgets_what_was_written() {
        in_context!(scope, {
            let context = scope.get_current_context();
            let pointer = 0x1234usize as *mut c_void;
            context.set_aligned_pointer_in_embedder_data(1, pointer);
            context.set_aligned_pointer_in_embedder_data(2, pointer);
            assert_eq!(context.get_aligned_pointer_from_embedder_data(1), pointer);

            context.clear_all_slots();
            assert!(context.get_aligned_pointer_from_embedder_data(1).is_null());
            assert!(context.get_aligned_pointer_from_embedder_data(2).is_null());

            // And a write after the clear lands where it should, so the map is
            // still usable rather than merely emptied.
            context.set_aligned_pointer_in_embedder_data(2, pointer);
            assert_eq!(context.get_aligned_pointer_from_embedder_data(2), pointer);
        });
    }
}
