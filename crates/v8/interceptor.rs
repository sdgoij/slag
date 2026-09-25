//! Property handlers: what a host installs on an object template so the engine
//! asks it about properties (`v8::NamedPropertyHandlerConfiguration` and
//! `v8::IndexedPropertyHandlerConfiguration`).
//!
//! A handler is a flag set and up to seven callbacks. The bridge keeps the
//! host's configurations under the template's address, and a context made from
//! that template (`ContextOptions::global_template`) builds its realm's global
//! object as a `crux::host::HostOps` object whose methods call back into them —
//! so a host's answers reach the engine's internal methods, with
//! `Intercepted::kNo` meaning "not intercepted, run the ordinary method", which
//! is the fallback the seam already documents.
//!
//! The engine expresses all seven V8 answers already, so the bridge is a
//! translation rather than new machinery:
//!
//! | V8 callback | `crux::host::HostOps` method |
//! |---|---|
//! | `descriptor` | `get_own_property` |
//! | `getter` | `get` |
//! | `query` | `has_property` |
//! | `setter` | `set` |
//! | `deleter` | `delete` |
//! | `enumerator` | `own_property_keys` |
//! | `definer` | `define_property` |
//!
//! A key's *kind* routes the named handler from the indexed one, the rule V8
//! uses: an index key (a canonical numeric string, spec 6.1.7.1) goes to the
//! indexed handler when it has the callback, and otherwise to the named one. A
//! handler that was never installed, or a callback that answers `kNo`, leaves
//! the engine's ordinary method running.
//!
//! Divergences, stated rather than hidden:
//!
//! * `PropertyHandlerFlags`' `NON_MASKING` is accepted and not honoured — the
//!   engine consults host ops before its own own-property lookup, and walking the
//!   prototype chain from inside that lookup would re-enter this same handler —
//!   so a handler is called for a name the chain also carries, which a `kNo`
//!   answer makes invisible in the result. `ALL_CAN_READ` and
//!   `ONLY_INTERCEPT_STRINGS` are likewise accepted and not honoured.
//! * `Intercepted::kThrow` is refused by name: the crate we stand in for lets the
//!   callback's *thrown value* propagate, and a `crux` internal method has no way
//!   to carry one out.
//! * The `query` callback's return slot carries a `PropertyAttribute` in V8; the
//!   engine's `has_property` answers a bare `bool`, so the attributes a query
//!   sets are dropped. Existence is preserved: `kYes` is "present".
//! * The two enumerators are asked and their answers concatenated (the indexed
//!   one first, because V8 orders element indices before string keys), where V8
//!   itself enumerates the object's own elements and asks the host only for the
//!   rest. The engine's seam is a single `own_property_keys`, and a host object
//!   here has no own element table of its own, so the two agree for the case
//!   this exists for; they would part if a host installed one enumerator and
//!   not the other.

use std::cell::RefCell;
use std::rc::Rc;

use runtime::api;

use crate::data::{Array, Boolean, Integer, Name, Object, Value as EngineHandleValue};
use crate::function::ReturnValue;
use crate::handle::{Local, Payload};
use crate::property_descriptor::PropertyDescriptor;
use crate::scope::PinScope;
use crate::support::{MapFnFrom, UnitType};
use crate::{Intercepted, PropertyHandlerFlags};

/// The handlers a global template carries. V8 keeps the named and indexed
/// configurations apart — a template may have either or both — so this is what
/// the template stores under its one host-state slot.
#[derive(Clone, Debug, Default)]
pub(crate) struct TemplateHandlers {
    pub(crate) named: Option<Rc<NamedPropertyHandlerConfiguration>>,
    pub(crate) indexed: Option<Rc<IndexedPropertyHandlerConfiguration>>,
}

impl TemplateHandlers {
    /// Whether either configuration is present, which is what decides whether
    /// the realm's global object is host-defined at all.
    pub(crate) fn has_any(&self) -> bool {
        self.named.is_some() || self.indexed.is_some()
    }
}

/// The parts every keyed property callback is given: which object and key the
/// engine is asking about, in which realm, how the operation would fail, and
/// where the answer goes.
///
/// `parts` is `None` when the thread has no entered realm, which is not a
/// situation a property operation can reach: the engine entered it before
/// running script.
macro_rules! keyed_info {
    ($(#[$doc:meta])* $name:ident $(, $extra:ident : $extra_ty:ty)*) => {
        $(#[$doc])*
        pub struct $name {
            context: api::Context,
            key: crux::property::PropertyKey,
            holder: crux::value::Value,
            return_value: RefCell<Option<crux::value::Value>>,
            throw_on_error: bool,
            $($extra: $extra_ty,)*
        }

        impl $name {
            fn parts(
                object: &crux::object::JsObject,
                key: &crux::property::PropertyKey,
                throw_on_error: bool,
                $($extra: $extra_ty,)*
            ) -> Option<Self> {
                let context = crate::realm::current()?;
                Some(Self {
                    context,
                    key: key.clone(),
                    // The object the internal method ran on, as a value: it is
                    // what `v8::PropertyCallbackArguments::holder` names.
                    holder: object.self_value(),
                    return_value: RefCell::new(None),
                    throw_on_error,
                    $($extra,)*
                })
            }

            /// The realm the operation runs in.
            fn context<'s>(&'s self) -> Local<'s, crate::data::Context> {
                Local::from_payload(Payload::Context(self.context))
            }

            /// The key the engine asked about.
            fn key<'s>(&'s self) -> Local<'s, Name> {
                key_handle(&self.key)
            }

            /// The object the operation ran on.
            fn holder<'s>(&'s self) -> Local<'s, Object> {
                Local::from_engine(api::Local::from(self.holder))
            }

            /// Where the callback's answer goes.
            fn slot(&self) -> &RefCell<Option<crux::value::Value>> {
                &self.return_value
            }

            /// What the callback is told about the operation.
            fn args<'s>(&'s self) -> PropertyCallbackArguments<'s> {
                PropertyCallbackArguments {
                    holder: self.holder(),
                    throw_on_error: self.throw_on_error,
                }
            }
        }
    };
}

/// What a value-shaped answer holds: the slot's value, or *undefined* if the
/// callback left nothing — which is what the crate we stand in for reads.
macro_rules! answered_info {
    ($name:ident) => {
        impl $name {
            fn answer(&self) -> crux::value::Value {
                (*self.return_value.borrow()).unwrap_or(crux::value::Value::Undefined)
            }
        }
    };
}

keyed_info!(
    /// What a `getter` callback reads, and what a `descriptor` callback reads
    /// too: the crate we stand in for gives both the same
    /// `v8::PropertyCallbackInfo<Value>`, so this is one struct.
    GetterCallbackInfo
);

keyed_info!(
    /// What a `query` callback reads (`v8::PropertyCallbackInfo<Integer>`).
    QueryCallbackInfo
);

keyed_info!(
    /// What a `setter` callback reads, with the value being set and the
    /// operation's `Throw` argument.
    SetterCallbackInfo,
    value: crux::value::Value
);

keyed_info!(
    /// What a `deleter` callback reads (`v8::PropertyCallbackInfo<Boolean>`).
    DeleterCallbackInfo
);

keyed_info!(
    /// What a `definer` callback reads, with the descriptor being defined.
    DefinerCallbackInfo,
    descriptor: PropertyDescriptor
);

// Only the callbacks whose answer the engine reads need the slot reader: a
// getter's value, a descriptor's record, and a deleter's boolean. A setter's
// and a definer's slot is `()`, and a query's attributes are dropped (see this
// module's note).
answered_info!(GetterCallbackInfo);
answered_info!(DeleterCallbackInfo);

/// What an `enumerator` callback reads
/// (`v8::PropertyCallbackInfo<Array>`): no key, because the question is about
/// the object as a whole.
pub struct EnumeratorCallbackInfo {
    context: api::Context,
    holder: crux::value::Value,
    return_value: RefCell<Option<crux::value::Value>>,
}

impl EnumeratorCallbackInfo {
    fn parts(object: &crux::object::JsObject) -> Option<Self> {
        let context = crate::realm::current()?;
        Some(Self {
            context,
            holder: object.self_value(),
            return_value: RefCell::new(None),
        })
    }

    /// The realm the operation runs in.
    fn context<'s>(&'s self) -> Local<'s, crate::data::Context> {
        Local::from_payload(Payload::Context(self.context))
    }

    /// The object the operation ran on.
    fn holder<'s>(&'s self) -> Local<'s, Object> {
        Local::from_engine(api::Local::from(self.holder))
    }

    /// Where the callback's answer goes.
    fn slot(&self) -> &RefCell<Option<crux::value::Value>> {
        &self.return_value
    }

    /// What the callback is told about the operation. An enumeration has no
    /// `Throw` argument, so this reports false.
    fn args<'s>(&'s self) -> PropertyCallbackArguments<'s> {
        PropertyCallbackArguments {
            holder: self.holder(),
            throw_on_error: false,
        }
    }

    /// What the callback left in the slot, or *undefined*.
    fn answer(&self) -> crux::value::Value {
        (*self.return_value.borrow()).unwrap_or(crux::value::Value::Undefined)
    }
}

/// The `args` a property callback is given (v8::PropertyCallbackArguments).
pub struct PropertyCallbackArguments<'s> {
    holder: Local<'s, Object>,
    throw_on_error: bool,
}

impl<'s> PropertyCallbackArguments<'s> {
    /// The object the internal method ran on
    /// (`v8::PropertyCallbackArguments::holder`).
    pub fn holder(&self) -> Local<'s, Object> {
        self.holder
    }

    /// Whether the operation throws when it fails
    /// (`v8::PropertyCallbackArguments::should_throw_on_error`).
    ///
    /// This is the `Throw` argument of the `[[Set]]` that brought the callback
    /// here (spec 10.1.9.3 step 3, which is a strict-mode store), and false for
    /// every other operation: only a set has one in the crate we stand in for.
    pub fn should_throw_on_error(&self) -> bool {
        self.throw_on_error
    }
}

/// The key a callback reads, as the crate's `Name` handle: a string key is the
/// string, a symbol key the symbol — the two kinds a named handler is about.
fn key_handle<'s>(key: &crux::property::PropertyKey) -> Local<'s, Name> {
    let value = match key {
        crux::property::PropertyKey::String(id) => {
            crux::value::Value::String(crux::handle::Handle::new(crux::lookup(*id)))
        }
        crux::property::PropertyKey::Symbol(symbol) => crux::value::Value::Symbol(*symbol),
    };
    Local::from_engine(api::Local::from(value))
}

/// The `getter`/`descriptor` callback, in the raw shape
/// (`v8::NamedPropertyGetterCallback`).
pub type NamedPropertyGetterCallback =
    unsafe extern "C" fn(*const GetterCallbackInfo) -> Intercepted;

/// The same callback in the template's `descriptor` slot
/// (`v8::NamedPropertyDescriptorCallback`): the crate we stand in for leaves the
/// two the same function *type* — the descriptor is what a getter writes for
/// `[[GetOwnProperty]]` — and deno's `ExternalReference` table depends on it,
/// storing a descriptor under `named_getter`.
pub type NamedPropertyDescriptorCallback = NamedPropertyGetterCallback;

/// The `setter` callback, in the raw shape
/// (`v8::NamedPropertySetterCallback`).
pub type NamedPropertySetterCallback =
    unsafe extern "C" fn(*const SetterCallbackInfo) -> Intercepted;

/// The `query` callback, in the raw shape
/// (`v8::NamedPropertyQueryCallback`).
pub type NamedPropertyQueryCallback = unsafe extern "C" fn(*const QueryCallbackInfo) -> Intercepted;

/// The `deleter` callback, in the raw shape
/// (`v8::NamedPropertyDeleterCallback`).
pub type NamedPropertyDeleterCallback =
    unsafe extern "C" fn(*const DeleterCallbackInfo) -> Intercepted;

/// The `definer` callback, in the raw shape
/// (`v8::NamedPropertyDefinerCallback`).
pub type NamedPropertyDefinerCallback =
    unsafe extern "C" fn(*const DefinerCallbackInfo) -> Intercepted;

/// The `enumerator` callback, in the raw shape
/// (`v8::NamedPropertyEnumeratorCallback`).
///
/// The indexed enumerator has the same signature and the same job — fill the
/// slot with keys — so [`IndexedPropertyEnumeratorCallback`] is this type.
pub type NamedPropertyEnumeratorCallback =
    unsafe extern "C" fn(*const EnumeratorCallbackInfo) -> ();

/// The indexed `getter` callback, in the raw shape
/// (`v8::IndexedPropertyGetterCallback`): the index as a number, where the named
/// handler is given the key.
pub type IndexedPropertyGetterCallback =
    unsafe extern "C" fn(*const GetterCallbackInfo, u32) -> Intercepted;

/// The indexed `descriptor` callback (`v8::IndexedPropertyDescriptorCallback`),
/// the same function type as the indexed getter, as the named pair is.
pub type IndexedPropertyDescriptorCallback = IndexedPropertyGetterCallback;

/// The indexed `setter` callback (`v8::IndexedPropertySetterCallback`).
pub type IndexedPropertySetterCallback =
    unsafe extern "C" fn(*const SetterCallbackInfo, u32) -> Intercepted;

/// The indexed `query` callback (`v8::IndexedPropertyQueryCallback`).
pub type IndexedPropertyQueryCallback =
    unsafe extern "C" fn(*const QueryCallbackInfo, u32) -> Intercepted;

/// The indexed `deleter` callback (`v8::IndexedPropertyDeleterCallback`).
pub type IndexedPropertyDeleterCallback =
    unsafe extern "C" fn(*const DeleterCallbackInfo, u32) -> Intercepted;

/// The indexed `definer` callback (`v8::IndexedPropertyDefinerCallback`).
pub type IndexedPropertyDefinerCallback =
    unsafe extern "C" fn(*const DefinerCallbackInfo, u32) -> Intercepted;

/// The indexed `enumerator` callback (`v8::IndexedPropertyEnumeratorCallback`):
/// the same function as the named one.
pub type IndexedPropertyEnumeratorCallback = NamedPropertyEnumeratorCallback;

impl<F> MapFnFrom<F> for NamedPropertyGetterCallback
where
    F: UnitType
        + for<'s, 'i> Fn(
            &mut PinScope<'s, 'i>,
            Local<'s, Name>,
            PropertyCallbackArguments<'s>,
            ReturnValue<'s>,
        ) -> Intercepted,
{
    fn mapping() -> Self {
        unsafe extern "C" fn adapter<F>(info: *const GetterCallbackInfo) -> Intercepted
        where
            F: UnitType
                + for<'s, 'i> Fn(
                    &mut PinScope<'s, 'i>,
                    Local<'s, Name>,
                    PropertyCallbackArguments<'s>,
                    ReturnValue<'s>,
                ) -> Intercepted,
        {
            // SAFETY: the bridge built `info` and keeps it alive for the call.
            let info = unsafe { &*info };
            // SAFETY: the callback runs inside the engine's property operation,
            // which is where the entered realm comes from.
            let context = info.context();
            crate::callback_scope!(unsafe scope, context);
            let key = info.key();
            let args = info.args();
            let rv = ReturnValue::from_slot(info.slot());
            (F::get())(scope, key, args, rv)
        }

        adapter::<F>
    }
}

impl<F> MapFnFrom<F> for IndexedPropertyGetterCallback
where
    F: UnitType
        + for<'s, 'i> Fn(
            &mut PinScope<'s, 'i>,
            u32,
            PropertyCallbackArguments<'s>,
            ReturnValue<'s>,
        ) -> Intercepted,
{
    fn mapping() -> Self {
        unsafe extern "C" fn adapter<F>(info: *const GetterCallbackInfo, index: u32) -> Intercepted
        where
            F: UnitType
                + for<'s, 'i> Fn(
                    &mut PinScope<'s, 'i>,
                    u32,
                    PropertyCallbackArguments<'s>,
                    ReturnValue<'s>,
                ) -> Intercepted,
        {
            // SAFETY: the bridge built `info` and keeps it alive for the call.
            let info = unsafe { &*info };
            // SAFETY: the callback runs inside the engine's property operation,
            // which is where the entered realm comes from.
            let context = info.context();
            crate::callback_scope!(unsafe scope, context);
            let args = info.args();
            let rv = ReturnValue::from_slot(info.slot());
            (F::get())(scope, index, args, rv)
        }

        adapter::<F>
    }
}

impl<F> MapFnFrom<F> for NamedPropertySetterCallback
where
    F: UnitType
        + for<'s, 'i> Fn(
            &mut PinScope<'s, 'i>,
            Local<'s, Name>,
            Local<'s, EngineHandleValue>,
            PropertyCallbackArguments<'s>,
            ReturnValue<'s, ()>,
        ) -> Intercepted,
{
    fn mapping() -> Self {
        unsafe extern "C" fn adapter<F>(info: *const SetterCallbackInfo) -> Intercepted
        where
            F: UnitType
                + for<'s, 'i> Fn(
                    &mut PinScope<'s, 'i>,
                    Local<'s, Name>,
                    Local<'s, EngineHandleValue>,
                    PropertyCallbackArguments<'s>,
                    ReturnValue<'s, ()>,
                ) -> Intercepted,
        {
            // SAFETY: the bridge built `info` and keeps it alive for the call.
            let info = unsafe { &*info };
            // SAFETY: the callback runs inside the engine's property operation,
            // which is where the entered realm comes from.
            let context = info.context();
            crate::callback_scope!(unsafe scope, context);
            let key = info.key();
            let value: Local<'_, EngineHandleValue> =
                Local::from_engine(api::Local::from(info.value));
            let args = info.args();
            let rv: ReturnValue<'_, ()> = ReturnValue::from_slot(info.slot());
            (F::get())(scope, key, value, args, rv)
        }

        adapter::<F>
    }
}

impl<F> MapFnFrom<F> for IndexedPropertySetterCallback
where
    F: UnitType
        + for<'s, 'i> Fn(
            &mut PinScope<'s, 'i>,
            u32,
            Local<'s, EngineHandleValue>,
            PropertyCallbackArguments<'s>,
            ReturnValue<'s, ()>,
        ) -> Intercepted,
{
    fn mapping() -> Self {
        unsafe extern "C" fn adapter<F>(info: *const SetterCallbackInfo, index: u32) -> Intercepted
        where
            F: UnitType
                + for<'s, 'i> Fn(
                    &mut PinScope<'s, 'i>,
                    u32,
                    Local<'s, EngineHandleValue>,
                    PropertyCallbackArguments<'s>,
                    ReturnValue<'s, ()>,
                ) -> Intercepted,
        {
            // SAFETY: the bridge built `info` and keeps it alive for the call.
            let info = unsafe { &*info };
            // SAFETY: the callback runs inside the engine's property operation,
            // which is where the entered realm comes from.
            let context = info.context();
            crate::callback_scope!(unsafe scope, context);
            let value: Local<'_, EngineHandleValue> =
                Local::from_engine(api::Local::from(info.value));
            let args = info.args();
            let rv: ReturnValue<'_, ()> = ReturnValue::from_slot(info.slot());
            (F::get())(scope, index, value, args, rv)
        }

        adapter::<F>
    }
}

impl<F> MapFnFrom<F> for NamedPropertyQueryCallback
where
    F: UnitType
        + for<'s, 'i> Fn(
            &mut PinScope<'s, 'i>,
            Local<'s, Name>,
            PropertyCallbackArguments<'s>,
            ReturnValue<'s, Integer>,
        ) -> Intercepted,
{
    fn mapping() -> Self {
        unsafe extern "C" fn adapter<F>(info: *const QueryCallbackInfo) -> Intercepted
        where
            F: UnitType
                + for<'s, 'i> Fn(
                    &mut PinScope<'s, 'i>,
                    Local<'s, Name>,
                    PropertyCallbackArguments<'s>,
                    ReturnValue<'s, Integer>,
                ) -> Intercepted,
        {
            // SAFETY: the bridge built `info` and keeps it alive for the call.
            let info = unsafe { &*info };
            // SAFETY: the callback runs inside the engine's property operation,
            // which is where the entered realm comes from.
            let context = info.context();
            crate::callback_scope!(unsafe scope, context);
            let key = info.key();
            let args = info.args();
            let rv: ReturnValue<'_, Integer> = ReturnValue::from_slot(info.slot());
            (F::get())(scope, key, args, rv)
        }

        adapter::<F>
    }
}

impl<F> MapFnFrom<F> for IndexedPropertyQueryCallback
where
    F: UnitType
        + for<'s, 'i> Fn(
            &mut PinScope<'s, 'i>,
            u32,
            PropertyCallbackArguments<'s>,
            ReturnValue<'s, Integer>,
        ) -> Intercepted,
{
    fn mapping() -> Self {
        unsafe extern "C" fn adapter<F>(info: *const QueryCallbackInfo, index: u32) -> Intercepted
        where
            F: UnitType
                + for<'s, 'i> Fn(
                    &mut PinScope<'s, 'i>,
                    u32,
                    PropertyCallbackArguments<'s>,
                    ReturnValue<'s, Integer>,
                ) -> Intercepted,
        {
            // SAFETY: the bridge built `info` and keeps it alive for the call.
            let info = unsafe { &*info };
            // SAFETY: the callback runs inside the engine's property operation,
            // which is where the entered realm comes from.
            let context = info.context();
            crate::callback_scope!(unsafe scope, context);
            let args = info.args();
            let rv: ReturnValue<'_, Integer> = ReturnValue::from_slot(info.slot());
            (F::get())(scope, index, args, rv)
        }

        adapter::<F>
    }
}

impl<F> MapFnFrom<F> for NamedPropertyDeleterCallback
where
    F: UnitType
        + for<'s, 'i> Fn(
            &mut PinScope<'s, 'i>,
            Local<'s, Name>,
            PropertyCallbackArguments<'s>,
            ReturnValue<'s, Boolean>,
        ) -> Intercepted,
{
    fn mapping() -> Self {
        unsafe extern "C" fn adapter<F>(info: *const DeleterCallbackInfo) -> Intercepted
        where
            F: UnitType
                + for<'s, 'i> Fn(
                    &mut PinScope<'s, 'i>,
                    Local<'s, Name>,
                    PropertyCallbackArguments<'s>,
                    ReturnValue<'s, Boolean>,
                ) -> Intercepted,
        {
            // SAFETY: the bridge built `info` and keeps it alive for the call.
            let info = unsafe { &*info };
            // SAFETY: the callback runs inside the engine's property operation,
            // which is where the entered realm comes from.
            let context = info.context();
            crate::callback_scope!(unsafe scope, context);
            let key = info.key();
            let args = info.args();
            let rv: ReturnValue<'_, Boolean> = ReturnValue::from_slot(info.slot());
            (F::get())(scope, key, args, rv)
        }

        adapter::<F>
    }
}

impl<F> MapFnFrom<F> for IndexedPropertyDeleterCallback
where
    F: UnitType
        + for<'s, 'i> Fn(
            &mut PinScope<'s, 'i>,
            u32,
            PropertyCallbackArguments<'s>,
            ReturnValue<'s, Boolean>,
        ) -> Intercepted,
{
    fn mapping() -> Self {
        unsafe extern "C" fn adapter<F>(info: *const DeleterCallbackInfo, index: u32) -> Intercepted
        where
            F: UnitType
                + for<'s, 'i> Fn(
                    &mut PinScope<'s, 'i>,
                    u32,
                    PropertyCallbackArguments<'s>,
                    ReturnValue<'s, Boolean>,
                ) -> Intercepted,
        {
            // SAFETY: the bridge built `info` and keeps it alive for the call.
            let info = unsafe { &*info };
            // SAFETY: the callback runs inside the engine's property operation,
            // which is where the entered realm comes from.
            let context = info.context();
            crate::callback_scope!(unsafe scope, context);
            let args = info.args();
            let rv: ReturnValue<'_, Boolean> = ReturnValue::from_slot(info.slot());
            (F::get())(scope, index, args, rv)
        }

        adapter::<F>
    }
}

impl<F> MapFnFrom<F> for NamedPropertyDefinerCallback
where
    F: UnitType
        + for<'s, 'i> Fn(
            &mut PinScope<'s, 'i>,
            Local<'s, Name>,
            &PropertyDescriptor,
            PropertyCallbackArguments<'s>,
            ReturnValue<'s, ()>,
        ) -> Intercepted,
{
    fn mapping() -> Self {
        unsafe extern "C" fn adapter<F>(info: *const DefinerCallbackInfo) -> Intercepted
        where
            F: UnitType
                + for<'s, 'i> Fn(
                    &mut PinScope<'s, 'i>,
                    Local<'s, Name>,
                    &PropertyDescriptor,
                    PropertyCallbackArguments<'s>,
                    ReturnValue<'s, ()>,
                ) -> Intercepted,
        {
            // SAFETY: the bridge built `info` and keeps it alive for the call.
            let info = unsafe { &*info };
            // SAFETY: the callback runs inside the engine's property operation,
            // which is where the entered realm comes from.
            let context = info.context();
            crate::callback_scope!(unsafe scope, context);
            let key = info.key();
            let args = info.args();
            let rv: ReturnValue<'_, ()> = ReturnValue::from_slot(info.slot());
            (F::get())(scope, key, &info.descriptor, args, rv)
        }

        adapter::<F>
    }
}

impl<F> MapFnFrom<F> for IndexedPropertyDefinerCallback
where
    F: UnitType
        + for<'s, 'i> Fn(
            &mut PinScope<'s, 'i>,
            u32,
            &PropertyDescriptor,
            PropertyCallbackArguments<'s>,
            ReturnValue<'s, ()>,
        ) -> Intercepted,
{
    fn mapping() -> Self {
        unsafe extern "C" fn adapter<F>(info: *const DefinerCallbackInfo, index: u32) -> Intercepted
        where
            F: UnitType
                + for<'s, 'i> Fn(
                    &mut PinScope<'s, 'i>,
                    u32,
                    &PropertyDescriptor,
                    PropertyCallbackArguments<'s>,
                    ReturnValue<'s, ()>,
                ) -> Intercepted,
        {
            // SAFETY: the bridge built `info` and keeps it alive for the call.
            let info = unsafe { &*info };
            // SAFETY: the callback runs inside the engine's property operation,
            // which is where the entered realm comes from.
            let context = info.context();
            crate::callback_scope!(unsafe scope, context);
            let args = info.args();
            let rv: ReturnValue<'_, ()> = ReturnValue::from_slot(info.slot());
            (F::get())(scope, index, &info.descriptor, args, rv)
        }

        adapter::<F>
    }
}

impl<F> MapFnFrom<F> for NamedPropertyEnumeratorCallback
where
    F: UnitType
        + for<'s, 'i> Fn(&mut PinScope<'s, 'i>, PropertyCallbackArguments<'s>, ReturnValue<'s, Array>),
{
    fn mapping() -> Self {
        unsafe extern "C" fn adapter<F>(info: *const EnumeratorCallbackInfo)
        where
            F: UnitType
                + for<'s, 'i> Fn(
                    &mut PinScope<'s, 'i>,
                    PropertyCallbackArguments<'s>,
                    ReturnValue<'s, Array>,
                ),
        {
            // SAFETY: the bridge built `info` and keeps it alive for the call.
            let info = unsafe { &*info };
            // SAFETY: the callback runs inside the engine's property operation,
            // which is where the entered realm comes from.
            let context = info.context();
            crate::callback_scope!(unsafe scope, context);
            let args = info.args();
            let rv: ReturnValue<'_, Array> = ReturnValue::from_slot(info.slot());
            (F::get())(scope, args, rv)
        }

        adapter::<F>
    }
}

/// How a host wants to be asked about an object's named properties
/// (`v8::NamedPropertyHandlerConfiguration`).
#[derive(Clone, Copy, Debug)]
pub struct NamedPropertyHandlerConfiguration {
    pub(crate) flags: PropertyHandlerFlags,
    pub(crate) getter: Option<NamedPropertyGetterCallback>,
    pub(crate) setter: Option<NamedPropertySetterCallback>,
    pub(crate) query: Option<NamedPropertyQueryCallback>,
    pub(crate) deleter: Option<NamedPropertyDeleterCallback>,
    pub(crate) enumerator: Option<NamedPropertyEnumeratorCallback>,
    pub(crate) definer: Option<NamedPropertyDefinerCallback>,
    pub(crate) descriptor: Option<NamedPropertyDescriptorCallback>,
}

impl Default for NamedPropertyHandlerConfiguration {
    fn default() -> Self {
        Self {
            flags: PropertyHandlerFlags::NONE,
            getter: None,
            setter: None,
            query: None,
            deleter: None,
            enumerator: None,
            definer: None,
            descriptor: None,
        }
    }
}

impl NamedPropertyHandlerConfiguration {
    /// A configuration with no callbacks and no flags
    /// (`v8::NamedPropertyHandlerConfiguration::New`).
    pub fn new() -> Self {
        Self::default()
    }

    /// The flags the handler is consulted under
    /// (`v8::NamedPropertyHandlerConfiguration::flags`). See this module's note
    /// on the flags that are accepted and not honoured.
    pub fn flags(mut self, flags: PropertyHandlerFlags) -> Self {
        self.flags = flags;
        self
    }

    /// The `[[Get]]` callback, already in its raw shape
    /// (`v8::NamedPropertyHandlerConfiguration::getter_raw`).
    pub fn getter_raw(mut self, callback: NamedPropertyGetterCallback) -> Self {
        self.getter = Some(callback);
        self
    }

    /// The `[[Set]]` callback, already in its raw shape
    /// (`v8::NamedPropertyHandlerConfiguration::setter_raw`).
    pub fn setter_raw(mut self, callback: NamedPropertySetterCallback) -> Self {
        self.setter = Some(callback);
        self
    }

    /// The `[[HasProperty]]` callback, already in its raw shape
    /// (`v8::NamedPropertyHandlerConfiguration::query_raw`).
    pub fn query_raw(mut self, callback: NamedPropertyQueryCallback) -> Self {
        self.query = Some(callback);
        self
    }

    /// The `[[Delete]]` callback, already in its raw shape
    /// (`v8::NamedPropertyHandlerConfiguration::deleter_raw`).
    pub fn deleter_raw(mut self, callback: NamedPropertyDeleterCallback) -> Self {
        self.deleter = Some(callback);
        self
    }

    /// The `[[OwnPropertyKeys]]` callback, already in its raw shape
    /// (`v8::NamedPropertyHandlerConfiguration::enumerator_raw`).
    pub fn enumerator_raw(mut self, callback: NamedPropertyEnumeratorCallback) -> Self {
        self.enumerator = Some(callback);
        self
    }

    /// The `[[DefineOwnProperty]]` callback, already in its raw shape
    /// (`v8::NamedPropertyHandlerConfiguration::definer_raw`).
    pub fn definer_raw(mut self, callback: NamedPropertyDefinerCallback) -> Self {
        self.definer = Some(callback);
        self
    }

    /// The `[[GetOwnProperty]]` callback, already in its raw shape
    /// (`v8::NamedPropertyHandlerConfiguration::descriptor_raw`).
    pub fn descriptor_raw(mut self, callback: NamedPropertyDescriptorCallback) -> Self {
        self.descriptor = Some(callback);
        self
    }
}

/// How a host wants to be asked about an object's indexed properties
/// (`v8::IndexedPropertyHandlerConfiguration`).
#[derive(Clone, Copy, Debug)]
pub struct IndexedPropertyHandlerConfiguration {
    pub(crate) flags: PropertyHandlerFlags,
    pub(crate) getter: Option<IndexedPropertyGetterCallback>,
    pub(crate) setter: Option<IndexedPropertySetterCallback>,
    pub(crate) query: Option<IndexedPropertyQueryCallback>,
    pub(crate) deleter: Option<IndexedPropertyDeleterCallback>,
    pub(crate) enumerator: Option<IndexedPropertyEnumeratorCallback>,
    pub(crate) definer: Option<IndexedPropertyDefinerCallback>,
    pub(crate) descriptor: Option<IndexedPropertyDescriptorCallback>,
}

impl Default for IndexedPropertyHandlerConfiguration {
    fn default() -> Self {
        Self {
            flags: PropertyHandlerFlags::NONE,
            getter: None,
            setter: None,
            query: None,
            deleter: None,
            enumerator: None,
            definer: None,
            descriptor: None,
        }
    }
}

impl IndexedPropertyHandlerConfiguration {
    /// A configuration with no callbacks and no flags
    /// (`v8::IndexedPropertyHandlerConfiguration::New`).
    pub fn new() -> Self {
        Self::default()
    }

    /// The flags the handler is consulted under
    /// (`v8::IndexedPropertyHandlerConfiguration::flags`). See this module's
    /// note on the flags that are accepted and not honoured.
    pub fn flags(mut self, flags: PropertyHandlerFlags) -> Self {
        self.flags = flags;
        self
    }

    /// The `[[Get]]` callback, already in its raw shape
    /// (`v8::IndexedPropertyHandlerConfiguration::getter_raw`).
    pub fn getter_raw(mut self, callback: IndexedPropertyGetterCallback) -> Self {
        self.getter = Some(callback);
        self
    }

    /// The `[[Set]]` callback, already in its raw shape
    /// (`v8::IndexedPropertyHandlerConfiguration::setter_raw`).
    pub fn setter_raw(mut self, callback: IndexedPropertySetterCallback) -> Self {
        self.setter = Some(callback);
        self
    }

    /// The `[[HasProperty]]` callback, already in its raw shape
    /// (`v8::IndexedPropertyHandlerConfiguration::query_raw`).
    pub fn query_raw(mut self, callback: IndexedPropertyQueryCallback) -> Self {
        self.query = Some(callback);
        self
    }

    /// The `[[Delete]]` callback, already in its raw shape
    /// (`v8::IndexedPropertyHandlerConfiguration::deleter_raw`).
    pub fn deleter_raw(mut self, callback: IndexedPropertyDeleterCallback) -> Self {
        self.deleter = Some(callback);
        self
    }

    /// The `[[OwnPropertyKeys]]` callback, already in its raw shape
    /// (`v8::IndexedPropertyHandlerConfiguration::enumerator_raw`).
    pub fn enumerator_raw(mut self, callback: IndexedPropertyEnumeratorCallback) -> Self {
        self.enumerator = Some(callback);
        self
    }

    /// The `[[DefineOwnProperty]]` callback, already in its raw shape
    /// (`v8::IndexedPropertyHandlerConfiguration::definer_raw`).
    pub fn definer_raw(mut self, callback: IndexedPropertyDefinerCallback) -> Self {
        self.definer = Some(callback);
        self
    }

    /// The `[[GetOwnProperty]]` callback, already in its raw shape
    /// (`v8::IndexedPropertyHandlerConfiguration::descriptor_raw`).
    pub fn descriptor_raw(mut self, callback: IndexedPropertyDescriptorCallback) -> Self {
        self.descriptor = Some(callback);
        self
    }
}

/// A handler installed on a global template, as the engine's host-defined
/// internal methods for the realm that template makes.
#[derive(Debug)]
pub(crate) struct GlobalHandler {
    handlers: Rc<TemplateHandlers>,
}

impl GlobalHandler {
    pub(crate) fn new(handlers: Rc<TemplateHandlers>) -> Self {
        Self { handlers }
    }

    fn named(&self) -> Option<&NamedPropertyHandlerConfiguration> {
        self.handlers.named.as_deref()
    }

    fn indexed(&self) -> Option<&IndexedPropertyHandlerConfiguration> {
        self.handlers.indexed.as_deref()
    }

    /// The index an index key stands for, or `None` for a string or symbol key
    /// (spec 6.1.7.1). This is the split V8 routes the two handlers by.
    fn index_of(key: &crux::property::PropertyKey) -> Option<u32> {
        crux::object::array_index_of(key).map(|index| index as u32)
    }

    /// Call a `getter` or `descriptor` callback — they share one signature — at
    /// whichever site the key routes to, and answer what it reported.
    fn call_value(
        &self,
        object: &crux::object::JsObject,
        key: &crux::property::PropertyKey,
        named: impl Fn(&NamedPropertyHandlerConfiguration) -> Option<NamedPropertyGetterCallback>,
        indexed: impl Fn(&IndexedPropertyHandlerConfiguration) -> Option<IndexedPropertyGetterCallback>,
    ) -> Option<(Intercepted, crux::value::Value)> {
        if let Some(index) = Self::index_of(key)
            && let Some(callback) = self.indexed().and_then(&indexed)
        {
            let info = GetterCallbackInfo::parts(object, key, false)?;
            // SAFETY: the callback pointer is the host's, and the info is alive
            // for the call.
            return Some((unsafe { callback(&info, index) }, info.answer()));
        }
        let callback = self.named().and_then(named)?;
        let info = GetterCallbackInfo::parts(object, key, false)?;
        // SAFETY: as above.
        Some((unsafe { callback(&info) }, info.answer()))
    }

    fn call_setter(
        &self,
        object: &crux::object::JsObject,
        key: &crux::property::PropertyKey,
        value: &crux::value::Value,
        throw_on_error: bool,
    ) -> Option<Intercepted> {
        if let Some(index) = Self::index_of(key)
            && let Some(callback) = self.indexed().and_then(|config| config.setter)
        {
            let info = SetterCallbackInfo::parts(object, key, throw_on_error, *value)?;
            // SAFETY: the callback pointer is the host's, and the info is alive
            // for the call.
            return Some(unsafe { callback(&info, index) });
        }
        let callback = self.named().and_then(|config| config.setter)?;
        let info = SetterCallbackInfo::parts(object, key, throw_on_error, *value)?;
        // SAFETY: as above.
        Some(unsafe { callback(&info) })
    }

    fn call_query(
        &self,
        object: &crux::object::JsObject,
        key: &crux::property::PropertyKey,
    ) -> Option<Intercepted> {
        if let Some(index) = Self::index_of(key)
            && let Some(callback) = self.indexed().and_then(|config| config.query)
        {
            let info = QueryCallbackInfo::parts(object, key, false)?;
            // SAFETY: the callback pointer is the host's, and the info is alive
            // for the call.
            return Some(unsafe { callback(&info, index) });
        }
        let callback = self.named().and_then(|config| config.query)?;
        let info = QueryCallbackInfo::parts(object, key, false)?;
        // SAFETY: as above.
        Some(unsafe { callback(&info) })
    }

    fn call_deleter(
        &self,
        object: &crux::object::JsObject,
        key: &crux::property::PropertyKey,
    ) -> Option<Result<bool, crux::error::JsError>> {
        let (intercepted, answer) = if let Some(index) = Self::index_of(key)
            && let Some(callback) = self.indexed().and_then(|config| config.deleter)
        {
            let info = DeleterCallbackInfo::parts(object, key, false)?;
            // SAFETY: the callback pointer is the host's, and the info is alive
            // for the call.
            (unsafe { callback(&info, index) }, info.answer())
        } else {
            let callback = self.named().and_then(|config| config.deleter)?;
            let info = DeleterCallbackInfo::parts(object, key, false)?;
            // SAFETY: as above.
            (unsafe { callback(&info) }, info.answer())
        };
        Some(match intercepted {
            // Not intercepted: the engine's own [[Delete]] runs.
            Intercepted::kNo => return None,
            // The deleter's `ReturnValue<Boolean>` is the operation's result.
            Intercepted::kYes | Intercepted::kYesKeepExisting => Ok(deleted(&answer)),
            Intercepted::kThrow => Err(thrown()),
        })
    }

    fn call_definer(
        &self,
        object: &crux::object::JsObject,
        key: &crux::property::PropertyKey,
        desc: &crux::property::PropertyDescriptor,
    ) -> Option<Intercepted> {
        let descriptor = PropertyDescriptor::from_engine(desc.clone());
        if let Some(index) = Self::index_of(key)
            && let Some(callback) = self.indexed().and_then(|config| config.definer)
        {
            let info = DefinerCallbackInfo::parts(object, key, false, descriptor)?;
            // SAFETY: the callback pointer is the host's, and the info is alive
            // for the call.
            return Some(unsafe { callback(&info, index) });
        }
        let callback = self.named().and_then(|config| config.definer)?;
        let info = DefinerCallbackInfo::parts(object, key, false, descriptor)?;
        // SAFETY: as above.
        Some(unsafe { callback(&info) })
    }
}

/// A `kThrow` answer. See this module's note: the answer is refused rather than
/// replaced with a plausible-looking error.
fn thrown() -> crux::error::JsError {
    crux::error::JsError::new(
        crux::error::ErrorKind::TypeError,
        "a property handler answered kThrow, whose thrown value this bridge cannot carry".into(),
    )
}

/// The boolean a deleter's answer slot holds. The crate we stand in for reads
/// the slot's value as a boolean; anything else is false, which is what an
/// unset slot means there.
fn deleted(answer: &crux::value::Value) -> bool {
    match answer.kind() {
        crux::value::ValueKind::Boolean(value) => value,
        _ => false,
    }
}

/// The keys an enumerator answered with: the `Array` it wrote into its slot,
/// read back as engine keys. A number element is a canonical index key (V8's
/// `KeepNumbers` spells an index as a number); a string or a symbol element is
/// itself; anything else is skipped.
fn keys_of_answer(answer: &crux::value::Value) -> Vec<crux::property::PropertyKey> {
    let Some(array) = answer.as_object() else {
        return Vec::new();
    };
    let length = api::Array::length(&crate::realm_current(), &api::Local::from(*answer))
        .map_or(0.0, |length| length);
    let mut keys = Vec::new();
    for position in 0..length as u64 {
        let slot = crux::property::PropertyKey::from_index(position);
        let Ok(Some(property)) = array.get_own_property_key(&slot) else {
            continue;
        };
        let crux::object::PropertyKind::Data { value, .. } = property.kind else {
            continue;
        };
        if value.is_hole() {
            continue;
        }
        match value.kind() {
            crux::value::ValueKind::String(text) => {
                keys.push(crux::property::PropertyKey::from_js_string(&text));
            }
            crux::value::ValueKind::Symbol(symbol) => {
                keys.push(crux::property::PropertyKey::Symbol(symbol));
            }
            // 2^32 - 2, the largest array index (spec 6.1.7.1).
            crux::value::ValueKind::Number(number)
                if number.fract() == 0.0 && (0.0..=4_294_967_294.0).contains(&number) =>
            {
                keys.push(crux::property::PropertyKey::from_index(number as u64));
            }
            _ => {}
        }
    }
    keys
}

/// The property a `descriptor` answer describes: the engine's own
/// ToPropertyDescriptor, then the record the internal method answers with.
fn property_of(descriptor: crux::property::PropertyDescriptor) -> crux::object::Property {
    let enumerable = descriptor.enumerable.unwrap_or(false);
    let configurable = descriptor.configurable.unwrap_or(false);
    match (descriptor.value, descriptor.get, descriptor.set) {
        (Some(value), None, None) => crux::object::Property::data(
            value,
            descriptor.writable.unwrap_or(false),
            enumerable,
            configurable,
        ),
        (_, get, set) => crux::object::Property::accessor(get, set, enumerable, configurable),
    }
}

impl crux::host::HostOps for GlobalHandler {
    fn get_own_property(
        &self,
        object: &crux::object::JsObject,
        key: &crux::property::PropertyKey,
    ) -> Option<Result<crux::object::Property, crux::error::JsError>> {
        let (intercepted, answer) = self.call_value(
            object,
            key,
            |config| config.descriptor,
            |config| config.descriptor,
        )?;
        Some(match intercepted {
            // Not intercepted: the engine's own [[GetOwnProperty]] runs.
            Intercepted::kNo | Intercepted::kYesKeepExisting => return None,
            Intercepted::kYes => crux::property::to_property_descriptor(&answer).map(property_of),
            Intercepted::kThrow => Err(thrown()),
        })
    }

    fn get(
        &self,
        object: &crux::object::JsObject,
        key: &crux::property::PropertyKey,
        _receiver: &crux::value::Value,
    ) -> Option<Result<crux::value::Value, crux::error::JsError>> {
        let (intercepted, answer) =
            self.call_value(object, key, |config| config.getter, |config| config.getter)?;
        Some(match intercepted {
            // Not intercepted: the engine's own [[Get]] runs.
            Intercepted::kNo => return None,
            Intercepted::kYes | Intercepted::kYesKeepExisting => Ok(answer),
            Intercepted::kThrow => Err(thrown()),
        })
    }

    fn has_property(
        &self,
        object: &crux::object::JsObject,
        key: &crux::property::PropertyKey,
    ) -> Option<Result<bool, crux::error::JsError>> {
        let intercepted = self.call_query(object, key)?;
        Some(match intercepted {
            // Not intercepted: the engine's own [[HasProperty]] runs.
            Intercepted::kNo => return None,
            Intercepted::kYes | Intercepted::kYesKeepExisting => Ok(true),
            Intercepted::kThrow => Err(thrown()),
        })
    }

    fn set(
        &self,
        object: &crux::object::JsObject,
        key: &crux::property::PropertyKey,
        value: &crux::value::Value,
        _receiver: &crux::value::Value,
        throw_on_error: bool,
    ) -> Option<Result<bool, crux::error::JsError>> {
        let intercepted = self.call_setter(object, key, value, throw_on_error)?;
        Some(match intercepted {
            // Not intercepted: the engine's own OrdinarySet runs.
            Intercepted::kNo => return None,
            // The host did the set, or the engine must keep what is there: both
            // are a set the caller can see as done.
            Intercepted::kYes | Intercepted::kYesKeepExisting => Ok(true),
            Intercepted::kThrow => Err(thrown()),
        })
    }

    fn delete(
        &self,
        object: &crux::object::JsObject,
        key: &crux::property::PropertyKey,
    ) -> Option<Result<bool, crux::error::JsError>> {
        self.call_deleter(object, key)
    }

    fn own_property_keys(
        &self,
        object: &crux::object::JsObject,
    ) -> Option<Result<Vec<crux::property::PropertyKey>, crux::error::JsError>> {
        let indexed = self.indexed().and_then(|config| config.enumerator);
        let named = self.named().and_then(|config| config.enumerator);
        if indexed.is_none() && named.is_none() {
            // Neither handler enumerates: the engine's own [[OwnPropertyKeys]].
            return None;
        }
        let mut keys = Vec::new();
        // The indexed enumerator answers with the element indices, which V8
        // orders before the string keys (see this module's note on the split).
        if let Some(callback) = indexed {
            let info = EnumeratorCallbackInfo::parts(object)?;
            // SAFETY: the callback pointer is the host's, and the info is alive
            // for the call.
            unsafe { callback(&info) };
            keys.extend(keys_of_answer(&info.answer()));
        }
        if let Some(callback) = named {
            let info = EnumeratorCallbackInfo::parts(object)?;
            // SAFETY: as above.
            unsafe { callback(&info) };
            keys.extend(keys_of_answer(&info.answer()));
        }
        Some(Ok(keys))
    }

    fn define_property(
        &self,
        object: &crux::object::JsObject,
        key: &crux::property::PropertyKey,
        desc: &crux::property::PropertyDescriptor,
    ) -> Option<Result<bool, crux::error::JsError>> {
        let intercepted = self.call_definer(object, key, desc)?;
        Some(match intercepted {
            // Not intercepted: the engine's own [[DefineOwnProperty]] runs.
            Intercepted::kNo => return None,
            Intercepted::kYes | Intercepted::kYesKeepExisting => Ok(true),
            Intercepted::kThrow => Err(thrown()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContextOptions, CreateParams, Isolate, MapFnTo, ObjectTemplate};

    thread_local! {
        /// What the handler was asked, in order — the test's own record, since a
        /// handler is a host's code and nothing else observes it.
        static CALLS: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    }

    fn record(what: &'static str) {
        CALLS.with(|calls| calls.borrow_mut().push(what));
    }

    fn calls() -> Vec<&'static str> {
        CALLS.with(|calls| calls.borrow().clone())
    }

    fn descriptor<'s>(
        _scope: &mut PinScope<'s, '_>,
        _key: Local<'s, Name>,
        _args: PropertyCallbackArguments<'s>,
        _rv: ReturnValue,
    ) -> Intercepted {
        record("descriptor");
        Intercepted::kNo
    }

    fn setter<'s>(
        _scope: &mut PinScope<'s, '_>,
        _key: Local<'s, Name>,
        _value: Local<'s, EngineHandleValue>,
        _args: PropertyCallbackArguments<'s>,
        _rv: ReturnValue<'s, ()>,
    ) -> Intercepted {
        record("setter");
        Intercepted::kNo
    }

    fn definer<'s>(
        _scope: &mut PinScope<'s, '_>,
        _key: Local<'s, Name>,
        _descriptor: &PropertyDescriptor,
        _args: PropertyCallbackArguments<'s>,
        _rv: ReturnValue<'s, ()>,
    ) -> Intercepted {
        record("definer");
        Intercepted::kNo
    }

    /// The handler deno's `global_template_middleware` installs: the three
    /// callbacks, answering `kNo`, under `NON_MASKING | HAS_NO_SIDE_EFFECT`.
    fn config() -> NamedPropertyHandlerConfiguration {
        NamedPropertyHandlerConfiguration::new()
            .flags(PropertyHandlerFlags::NON_MASKING | PropertyHandlerFlags::HAS_NO_SIDE_EFFECT)
            .descriptor_raw(descriptor.map_fn_to())
            .setter_raw(setter.map_fn_to())
            .definer_raw(definer.map_fn_to())
    }

    /// Build a context over a global template, with the scope already entered.
    macro_rules! context_over {
        ($handle_scope:ident, $template:expr) => {{
            let context = crate::Context::new(
                $handle_scope,
                ContextOptions {
                    global_template: Some($template),
                    ..Default::default()
                },
            );
            crate::ContextScope::new($handle_scope, context)
        }};
    }

    /// What a global handler is for: the engine asks the host instead of running
    /// its own internal method, and `kNo` hands the operation back — so the
    /// script behaves exactly as it would with no handler, while every answer was
    /// the host's.
    #[test]
    fn a_global_handler_is_consulted_and_k_no_gives_the_operation_back() {
        CALLS.with(|calls| calls.borrow_mut().clear());
        let isolate = &mut Isolate::new(CreateParams::default());
        crate::scope!(let handle_scope, isolate);
        let template = ObjectTemplate::new(handle_scope);
        template.set_named_property_handler(config());
        let scope = &mut context_over!(handle_scope, template);

        crate::test_support::eval(
            scope,
            "Object.defineProperty(globalThis, 'key', { value: 9, enumerable: true,              configurable: true, writable: true }); globalThis.other = 1;",
        );

        let calls = calls();
        for what in ["definer", "setter", "descriptor"] {
            assert!(
                calls.contains(&what),
                "the host's {what} was not consulted: {calls:?}"
            );
        }
        // Every answer was `kNo`, so the engine's own operations ran too.
        assert_eq!(
            crate::test_support::eval_number(
                scope,
                "globalThis.other === 1 && globalThis.key === 9 ? 1 : 0"
            ),
            1.0,
            "a kNo answer leaves the ordinary operation running"
        );
    }

    /// A handler that *did* the set: `kYes` means the engine must not, so the
    /// value never lands as a property — the answer is what the operation is.
    #[test]
    fn a_setter_answering_k_yes_keeps_the_engine_out_of_the_set() {
        fn yes<'s>(
            _scope: &mut PinScope<'s, '_>,
            _key: Local<'s, Name>,
            _value: Local<'s, EngineHandleValue>,
            _args: PropertyCallbackArguments<'s>,
            _rv: ReturnValue<'s, ()>,
        ) -> Intercepted {
            Intercepted::kYes
        }

        let isolate = &mut Isolate::new(CreateParams::default());
        crate::scope!(let handle_scope, isolate);
        let template = ObjectTemplate::new(handle_scope);
        template.set_named_property_handler(
            NamedPropertyHandlerConfiguration::new().setter_raw(yes.map_fn_to()),
        );
        let scope = &mut context_over!(handle_scope, template);

        crate::test_support::eval(scope, "globalThis.handled = 1;");
        assert_eq!(
            crate::test_support::eval_number(scope, "'handled' in globalThis ? 1 : 0"),
            0.0,
            "the set the host handled is the set, so no property was created"
        );
    }

    /// A named `getter` answering `kYes` with a value is what a read of that
    /// name is, and the answer is not consulted for any other name.
    #[test]
    fn a_named_getter_answers_the_read_it_names() {
        fn getter<'s>(
            _scope: &mut PinScope<'s, '_>,
            key: Local<'s, Name>,
            _args: PropertyCallbackArguments<'s>,
            rv: ReturnValue,
        ) -> Intercepted {
            if key.engine().as_string().as_deref() == Some("answered") {
                record("named_getter");
                rv.set_double(42.0);
                return Intercepted::kYes;
            }
            Intercepted::kNo
        }

        CALLS.with(|calls| calls.borrow_mut().clear());
        let isolate = &mut Isolate::new(CreateParams::default());
        crate::scope!(let handle_scope, isolate);
        let template = ObjectTemplate::new(handle_scope);
        template.set_named_property_handler(
            NamedPropertyHandlerConfiguration::new().getter_raw(getter.map_fn_to()),
        );
        let scope = &mut context_over!(handle_scope, template);

        assert_eq!(
            crate::test_support::eval_number(scope, "globalThis.answered"),
            42.0,
            "the getter's value is what the read is"
        );
        assert!(
            calls().contains(&"named_getter"),
            "the host's getter was not consulted"
        );
        // A name the getter does not answer for is the engine's own, so the
        // ordinary property is still there.
        assert_eq!(
            crate::test_support::eval_number(scope, "globalThis.other = 7; globalThis.other"),
            7.0,
            "a kNo answer leaves the ordinary read running"
        );
    }

    /// An index key routes to the indexed handler, not the named one: the
    /// indexed getter answers for `[3]` and the named getter is not consulted
    /// for it, while the named getter still answers for a name.
    #[test]
    fn an_index_key_routes_to_the_indexed_handler() {
        fn named_getter<'s>(
            _scope: &mut PinScope<'s, '_>,
            key: Local<'s, Name>,
            _args: PropertyCallbackArguments<'s>,
            rv: ReturnValue,
        ) -> Intercepted {
            let name = key.engine().as_string().unwrap_or_default();
            if !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit()) {
                // The engine handed the named handler an index key, which is
                // the routing this test is about: answer something no other
                // path can, so the read says which handler ran.
                record("named_index");
                rv.set_double(-1.0);
                return Intercepted::kYes;
            }
            if name == "named" {
                record("named_getter");
                rv.set_double(7.0);
                return Intercepted::kYes;
            }
            Intercepted::kNo
        }

        fn indexed_getter<'s>(
            _scope: &mut PinScope<'s, '_>,
            index: u32,
            _args: PropertyCallbackArguments<'s>,
            rv: ReturnValue,
        ) -> Intercepted {
            record("indexed_getter");
            rv.set_double(f64::from(index) + 100.0);
            Intercepted::kYes
        }

        CALLS.with(|calls| calls.borrow_mut().clear());
        let isolate = &mut Isolate::new(CreateParams::default());
        crate::scope!(let handle_scope, isolate);
        let template = ObjectTemplate::new(handle_scope);
        template.set_named_property_handler(
            NamedPropertyHandlerConfiguration::new().getter_raw(named_getter.map_fn_to()),
        );
        template.set_indexed_property_handler(
            IndexedPropertyHandlerConfiguration::new().getter_raw(indexed_getter.map_fn_to()),
        );
        let scope = &mut context_over!(handle_scope, template);

        assert_eq!(
            crate::test_support::eval_number(scope, "globalThis[3]"),
            103.0,
            "the indexed getter's value is what the indexed read is"
        );
        let after_index = calls();
        assert!(
            after_index.contains(&"indexed_getter"),
            "the indexed getter was not consulted: {after_index:?}"
        );
        assert!(
            !after_index.contains(&"named_index"),
            "an index key reached the named handler: {after_index:?}"
        );

        // A string key still reaches the named handler.
        assert_eq!(
            crate::test_support::eval_number(scope, "globalThis.named"),
            7.0,
            "the named getter's value is what the named read is"
        );
        assert!(
            calls().contains(&"named_getter"),
            "a name did not reach the named handler"
        );
    }

    /// `should_throw_on_error` is the `[[Set]]`'s `Throw` argument: true for a
    /// strict-mode store, false for a sloppy one.
    #[test]
    fn should_throw_on_error_is_the_sets_throw_argument() {
        fn watching_setter<'s>(
            _scope: &mut PinScope<'s, '_>,
            _key: Local<'s, Name>,
            _value: Local<'s, EngineHandleValue>,
            args: PropertyCallbackArguments<'s>,
            _rv: ReturnValue<'s, ()>,
        ) -> Intercepted {
            record(if args.should_throw_on_error() {
                "throw"
            } else {
                "quiet"
            });
            Intercepted::kNo
        }

        CALLS.with(|calls| calls.borrow_mut().clear());
        let isolate = &mut Isolate::new(CreateParams::default());
        crate::scope!(let handle_scope, isolate);
        let template = ObjectTemplate::new(handle_scope);
        template.set_named_property_handler(
            NamedPropertyHandlerConfiguration::new().setter_raw(watching_setter.map_fn_to()),
        );
        let scope = &mut context_over!(handle_scope, template);

        // A strict-mode store to a non-writable global: the operation would
        // throw, so the callback is told so. The write is refused, and the
        // script catches the TypeError.
        CALLS.with(|calls| calls.borrow_mut().clear());
        crate::test_support::eval(
            scope,
            "\"use strict\"; Object.defineProperty(globalThis, 'readonly', { value: 1, writable: false, configurable: false }); try { globalThis.readonly = 2; } catch {}",
        );
        let strict = calls();
        assert!(
            strict.contains(&"throw"),
            "a strict-mode store did not report Throw: {strict:?}"
        );

        // A sloppy store is not a throwing one.
        CALLS.with(|calls| calls.borrow_mut().clear());
        crate::test_support::eval(scope, "globalThis.sloppy = 1;");
        let sloppy = calls();
        assert!(
            sloppy.contains(&"quiet"),
            "a sloppy store reported Throw: {sloppy:?}"
        );
        assert!(
            !sloppy.contains(&"throw"),
            "a sloppy store reported Throw: {sloppy:?}"
        );
    }
}
