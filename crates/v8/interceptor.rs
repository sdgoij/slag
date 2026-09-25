//! The named property handler: what a host installs on an object template so the
//! engine asks it about properties (`v8::NamedPropertyHandlerConfiguration`).
//!
//! A handler is a flag set and a few callbacks. The bridge keeps the host's
//! configuration under the template's address, and a context made from that
//! template (`ContextOptions::global_template`) builds its realm's global object
//! as a `crux::host::HostOps` object whose methods call back into them — so a
//! host's `descriptor`, `setter` and `definer` answers reach the engine's
//! internal methods, with `Intercepted::kNo` meaning "not intercepted, run the
//! ordinary method", which is the fallback the seam already documents.
//!
//! Only the three callbacks deno's `global_template_middleware` test configures
//! are here. The getter, query, deleter and enumerator callbacks, and the whole
//! indexed variant `ext/node/ops/vm.rs` also needs, are **absent rather than
//! present and silent**: a host that needs one gets a compile error here instead
//! of a handler that is never called.
//!
//! Two divergences, both stated rather than hidden. `PropertyHandlerFlags`'
//! `NON_MASKING` is accepted and not honoured — the engine consults host ops
//! before its own own-property lookup, and walking the prototype chain from
//! inside that lookup would re-enter this same handler — so a handler is called
//! for a name the chain also carries, which a `kNo` answer makes invisible in
//! the result. And `Intercepted::kThrow` is refused by name: the crate we stand
//! in for lets the callback's *thrown value* propagate, and a `crux` internal
//! method has no way to carry one out.

use std::cell::RefCell;
use std::rc::Rc;

use runtime::api;

use crate::data::{Name, Object};
use crate::function::ReturnValue;
use crate::handle::{Local, Payload};
use crate::property_descriptor::PropertyDescriptor;
use crate::scope::PinScope;
use crate::support::{MapFnFrom, UnitType};
use crate::{Intercepted, PropertyHandlerFlags};

/// What a `descriptor` callback reads (v8::PropertyCallbackInfo).
pub struct DescriptorCallbackInfo {
    context: api::Context,
    key: crux::property::PropertyKey,
    holder: crux::value::Value,
    return_value: RefCell<Option<crux::value::Value>>,
}

/// What a `setter` callback reads, with the value being set.
pub struct SetterCallbackInfo {
    context: api::Context,
    key: crux::property::PropertyKey,
    holder: crux::value::Value,
    return_value: RefCell<Option<crux::value::Value>>,
    value: crux::value::Value,
}

/// What a `definer` callback reads, with the descriptor being defined.
pub struct DefinerCallbackInfo {
    context: api::Context,
    key: crux::property::PropertyKey,
    holder: crux::value::Value,
    return_value: RefCell<Option<crux::value::Value>>,
    descriptor: PropertyDescriptor,
}

/// The parts every property callback is given: which object and key the engine
/// is asking about, in which realm, and where the answer goes.
///
/// `None` when the thread has no entered realm, which is not a situation a
/// property operation can reach: the engine entered it before running script.
macro_rules! property_parts {
    ($name:ident $(, $extra:ident : $extra_ty:ty)*) => {
        impl $name {
            fn parts(
                object: &crux::object::JsObject,
                key: &crux::property::PropertyKey,
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
                Local::from_engine(api::Local::from(self.holder.clone()))
            }

            /// Where the callback's answer goes.
            fn slot(&self) -> &RefCell<Option<crux::value::Value>> {
                &self.return_value
            }

        }
    };
}

property_parts!(DescriptorCallbackInfo);

impl DescriptorCallbackInfo {
    /// What the callback left in the slot, or *undefined* if it left nothing —
    /// which is what the crate we stand in for reads.
    fn answer(&self) -> crux::value::Value {
        (*self.return_value.borrow()).unwrap_or(crux::value::Value::Undefined)
    }
}
property_parts!(SetterCallbackInfo, value: crux::value::Value);
property_parts!(DefinerCallbackInfo, descriptor: PropertyDescriptor);

/// The `args` a property callback is given (v8::PropertyCallbackArguments).
pub struct PropertyCallbackArguments<'s> {
    holder: Local<'s, Object>,
}

impl<'s> PropertyCallbackArguments<'s> {
    /// The object the internal method ran on
    /// (`v8::PropertyCallbackArguments::holder`).
    pub fn holder(&self) -> Local<'s, Object> {
        self.holder
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

/// The callback the bridge calls for a `descriptor` query, in the raw shape
/// (`v8::NamedPropertyDescriptorCallback`).
pub type NamedPropertyDescriptorCallback =
    unsafe extern "C" fn(*const DescriptorCallbackInfo) -> Intercepted;

/// The callback the bridge calls for a `[[Set]]`, in the raw shape
/// (`v8::NamedPropertySetterCallback`).
pub type NamedPropertySetterCallback =
    unsafe extern "C" fn(*const SetterCallbackInfo) -> Intercepted;

/// The callback the bridge calls for a `[[DefineOwnProperty]]`, in the raw shape
/// (`v8::NamedPropertyDefinerCallback`).
pub type NamedPropertyDefinerCallback =
    unsafe extern "C" fn(*const DefinerCallbackInfo) -> Intercepted;

impl<F> MapFnFrom<F> for NamedPropertyDescriptorCallback
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
        unsafe extern "C" fn adapter<F>(info: *const DescriptorCallbackInfo) -> Intercepted
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
            let args = PropertyCallbackArguments {
                holder: info.holder(),
            };
            let rv = ReturnValue::from_slot(info.slot());
            (F::get())(scope, key, args, rv)
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
            Local<'s, crate::data::Value>,
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
                    Local<'s, crate::data::Value>,
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
            let value: Local<'_, crate::data::Value> =
                Local::from_engine(api::Local::from(info.value));
            let args = PropertyCallbackArguments {
                holder: info.holder(),
            };
            let rv: ReturnValue<'_, ()> = ReturnValue::from_slot(info.slot());
            (F::get())(scope, key, value, args, rv)
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
            let args = PropertyCallbackArguments {
                holder: info.holder(),
            };
            let rv: ReturnValue<'_, ()> = ReturnValue::from_slot(info.slot());
            (F::get())(scope, key, &info.descriptor, args, rv)
        }

        adapter::<F>
    }
}

/// How a host wants to be asked about an object's properties
/// (`v8::NamedPropertyHandlerConfiguration`).
#[derive(Clone, Copy, Debug)]
pub struct NamedPropertyHandlerConfiguration {
    pub(crate) flags: PropertyHandlerFlags,
    pub(crate) descriptor: Option<NamedPropertyDescriptorCallback>,
    pub(crate) setter: Option<NamedPropertySetterCallback>,
    pub(crate) definer: Option<NamedPropertyDefinerCallback>,
}

impl Default for NamedPropertyHandlerConfiguration {
    fn default() -> Self {
        Self {
            flags: PropertyHandlerFlags::NONE,
            descriptor: None,
            setter: None,
            definer: None,
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
    /// on `NON_MASKING`.
    pub fn flags(mut self, flags: PropertyHandlerFlags) -> Self {
        self.flags = flags;
        self
    }

    /// The `[[GetOwnProperty]]` callback, already in its raw shape
    /// (`v8::NamedPropertyHandlerConfiguration::descriptor_raw`).
    pub fn descriptor_raw(mut self, callback: NamedPropertyDescriptorCallback) -> Self {
        self.descriptor = Some(callback);
        self
    }

    /// The `[[Set]]` callback, already in its raw shape
    /// (`v8::NamedPropertyHandlerConfiguration::setter_raw`).
    pub fn setter_raw(mut self, callback: NamedPropertySetterCallback) -> Self {
        self.setter = Some(callback);
        self
    }

    /// The `[[DefineOwnProperty]]` callback, already in its raw shape
    /// (`v8::NamedPropertyHandlerConfiguration::definer_raw`).
    pub fn definer_raw(mut self, callback: NamedPropertyDefinerCallback) -> Self {
        self.definer = Some(callback);
        self
    }
}

/// A handler installed on a global template, as the engine's host-defined
/// internal methods for the realm that template makes.
#[derive(Debug)]
pub(crate) struct GlobalHandler {
    config: Rc<NamedPropertyHandlerConfiguration>,
}

impl GlobalHandler {
    pub(crate) fn new(config: Rc<NamedPropertyHandlerConfiguration>) -> Self {
        Self { config }
    }
}

/// A `kThrow` answer. See this module's note: the answer is refused rather than
/// replaced with a plausible-looking error.
fn thrown() -> crux::error::JsError {
    crux::error::JsError::new(
        crux::error::ErrorKind::TypeError,
        "a named property handler answered kThrow, whose thrown value this bridge cannot carry"
            .into(),
    )
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
        let callback = self.config.descriptor?;
        let info = DescriptorCallbackInfo::parts(object, key)?;
        // SAFETY: the callback pointer is the host's, and the info is alive for
        // the call.
        Some(match unsafe { callback(&info) } {
            // Not intercepted: the engine's own [[GetOwnProperty]] runs.
            Intercepted::kNo | Intercepted::kYesKeepExisting => return None,
            Intercepted::kYes => {
                crux::property::to_property_descriptor(&info.answer()).map(property_of)
            }
            Intercepted::kThrow => Err(thrown()),
        })
    }

    fn set(
        &self,
        object: &crux::object::JsObject,
        key: &crux::property::PropertyKey,
        value: &crux::value::Value,
        _receiver: &crux::value::Value,
    ) -> Option<Result<bool, crux::error::JsError>> {
        let callback = self.config.setter?;
        let info = SetterCallbackInfo::parts(object, key, *value)?;
        Some(match unsafe { callback(&info) } {
            // Not intercepted: the engine's own OrdinarySet runs.
            Intercepted::kNo => return None,
            // The host did the set, or the engine must keep what is there: both
            // are a set the caller can see as done.
            Intercepted::kYes | Intercepted::kYesKeepExisting => Ok(true),
            Intercepted::kThrow => Err(thrown()),
        })
    }

    fn define_property(
        &self,
        object: &crux::object::JsObject,
        key: &crux::property::PropertyKey,
        desc: &crux::property::PropertyDescriptor,
    ) -> Option<Result<bool, crux::error::JsError>> {
        let callback = self.config.definer?;
        let info =
            DefinerCallbackInfo::parts(object, key, PropertyDescriptor::from_engine(desc.clone()))?;
        Some(match unsafe { callback(&info) } {
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
    use crate::{ContextOptions, ContextScope, CreateParams, Isolate, MapFnTo, ObjectTemplate};

    thread_local! {
        /// What the handler was asked, in order — the test's own record, since a
        /// handler is a host's code and nothing else observes it.
        static CALLS: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    }

    fn record(what: &'static str) {
        CALLS.with(|calls| calls.borrow_mut().push(what));
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
        _value: Local<'s, crate::data::Value>,
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
        let context = crate::Context::new(
            handle_scope,
            ContextOptions {
                global_template: Some(template),
                ..Default::default()
            },
        );
        let scope = &mut ContextScope::new(handle_scope, context);

        crate::test_support::eval(
            scope,
            "Object.defineProperty(globalThis, 'key', { value: 9, enumerable: true,              configurable: true, writable: true }); globalThis.other = 1;",
        );

        let calls = CALLS.with(|calls| calls.borrow().clone());
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
            _value: Local<'s, crate::data::Value>,
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
        let context = crate::Context::new(
            handle_scope,
            ContextOptions {
                global_template: Some(template),
                ..Default::default()
            },
        );
        let scope = &mut ContextScope::new(handle_scope, context);

        crate::test_support::eval(scope, "globalThis.handled = 1;");
        assert_eq!(
            crate::test_support::eval_number(scope, "'handled' in globalThis ? 1 : 0"),
            0.0,
            "the set the host handled is the set, so no property was created"
        );
    }
}
