//! The property handler a `v8::NamedPropertyHandlerConfiguration` installs.
//!
//! V8 consults a handler before the engine's own internal method: a `kNo` answer
//! means "not intercepted, run the ordinary method", which is the `Option` the
//! engine's [`HostOps`] seam already expresses. The named handler carries all
//! seven of V8's callbacks, and the indexed one the same set keyed by an array
//! index.
//!
//! The `data` a configuration carries is a language value in V8, so it is pinned
//! for as long as this handler is (one handler per configuration, shared by the
//! objects made from it), and the C++ side gets it back on the callback info.

use std::cell::RefCell;
use std::ffi::c_void;
use std::fmt;
use std::rc::Rc;

use crux::error::{ErrorKind, JsError};
use crux::heap::{Pin, pin};
use crux::host::HostOps;
use crux::object::JsObject;
use crux::property::PropertyKey;
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use slag::CallbackWindow;

/// A property descriptor as the definer callback receives it
/// (`v8::PropertyDescriptor`): each field present-or-absent, because V8's
/// descriptor distinguishes "not mentioned" from "mentioned and false".
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SlagPropertyDescriptor {
    pub has_value: i32,
    pub value: u64,
    pub has_writable: i32,
    pub writable: i32,
    pub has_get: i32,
    pub get: u64,
    pub has_set: i32,
    pub set: u64,
    pub has_enumerable: i32,
    pub enumerable: i32,
    pub has_configurable: i32,
    pub configurable: i32,
}

impl SlagPropertyDescriptor {
    fn of(desc: &crux::property::PropertyDescriptor) -> Self {
        Self {
            has_value: i32::from(desc.value.is_some()),
            value: desc.value.map_or(0, |value| value.bits()),
            has_writable: i32::from(desc.writable.is_some()),
            writable: i32::from(desc.writable.unwrap_or(false)),
            has_get: i32::from(desc.get.is_some()),
            get: desc.get.map_or(0, |value| value.bits()),
            has_set: i32::from(desc.set.is_some()),
            set: desc.set.map_or(0, |value| value.bits()),
            has_enumerable: i32::from(desc.enumerable.is_some()),
            enumerable: i32::from(desc.enumerable.unwrap_or(false)),
            has_configurable: i32::from(desc.configurable.is_some()),
            configurable: i32::from(desc.configurable.unwrap_or(false)),
        }
    }
}

/// V8's `Intercepted`, as the flat ABI carries it.
const INTERCEPTED_NO: i32 = 0;
const INTERCEPTED_THROW: i32 = 3;

/// The C callbacks of a `v8::NamedPropertyHandlerConfiguration`. Each returns
/// V8's `Intercepted` (`0` kNo, `1` kYes, `2` kYesKeepExisting, `3` kThrow) and
/// writes its answer through its out parameter; a null pointer is a callback the
/// host did not install.
///
/// `data` is the configuration's `data` as tagged `Value` bits (the facade sends
/// `slag_undefined()` when it was left unset), and `state` is the C++ trampoline
/// context, which is opaque here.
///
/// The enumerator is missing on purpose. A `v8::NamedPropertyEnumeratorCallback`
/// builds the array it answers with, and allocating one goes through the
/// context, which the operation that called this handler holds borrowed: the
/// same reason `FunctionCallbackInfo` has no `eval`. Until a callback can
/// allocate through the live borrow, the callback is absent from this struct so
/// a host that needs it does not compile.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SlagPropertyHandler {
    /// `(state, data, holder, key, receiver, out)`.
    pub getter: Option<unsafe extern "C" fn(*mut c_void, u64, u64, u64, u64, *mut u64) -> i32>,
    /// `(state, data, holder, key, value, receiver, throw)`.
    pub setter: Option<unsafe extern "C" fn(*mut c_void, u64, u64, u64, u64, u64, i32) -> i32>,
    /// `(state, data, holder, key, out)`: the out slot is where a query puts its
    /// `PropertyAttribute`, which the engine's `has_property` does not carry.
    pub query: Option<unsafe extern "C" fn(*mut c_void, u64, u64, u64, *mut u64) -> i32>,
    /// `(state, data, holder, key, out)`: the out slot holds a boolean.
    pub deleter: Option<unsafe extern "C" fn(*mut c_void, u64, u64, u64, *mut u64) -> i32>,
    /// `(state, data, holder, out)`: [[OwnPropertyKeys]], the out slot receiving
    /// the array of keys the host owns.
    pub enumerator: Option<unsafe extern "C" fn(*mut c_void, u64, u64, *mut u64) -> i32>,
    /// `(state, data, holder, key, out)`: [[GetOwnProperty]], where the out slot
    /// receives the descriptor object.
    pub descriptor: Option<unsafe extern "C" fn(*mut c_void, u64, u64, u64, *mut u64) -> i32>,
    /// `(state, data, holder, key, descriptor, out)`: [[DefineOwnProperty]].
    pub definer: Option<
        unsafe extern "C" fn(
            *mut c_void,
            u64,
            u64,
            u64,
            *const SlagPropertyDescriptor,
            *mut u64,
        ) -> i32,
    >,
    /// The indexed twins, for a key that is an array index: `(state, data,
    /// holder, index, receiver, out)` and so on. V8 routes a key by its shape,
    /// and so does this handler: an array index goes to the indexed callback
    /// when one is installed, and to the named one otherwise.
    pub indexed_getter:
        Option<unsafe extern "C" fn(*mut c_void, u64, u64, u32, u64, *mut u64) -> i32>,
    pub indexed_setter:
        Option<unsafe extern "C" fn(*mut c_void, u64, u64, u32, u64, u64, i32) -> i32>,
    pub indexed_query: Option<unsafe extern "C" fn(*mut c_void, u64, u64, u32, *mut u64) -> i32>,
    pub indexed_deleter: Option<unsafe extern "C" fn(*mut c_void, u64, u64, u32, *mut u64) -> i32>,
    pub indexed_enumerator: Option<unsafe extern "C" fn(*mut c_void, u64, u64, *mut u64) -> i32>,
    /// The indexed twins of `descriptor` and `definer`.
    pub indexed_descriptor:
        Option<unsafe extern "C" fn(*mut c_void, u64, u64, u32, *mut u64) -> i32>,
    pub indexed_definer: Option<
        unsafe extern "C" fn(
            *mut c_void,
            u64,
            u64,
            u32,
            *const SlagPropertyDescriptor,
            *mut u64,
        ) -> i32,
    >,
    /// The configuration's `data`, as tagged `Value` bits.
    pub data: u64,
    /// The C++ side's trampoline context for the named callbacks.
    pub state: *mut c_void,
    /// The same for the indexed callbacks, which are their own configuration.
    pub indexed_state: *mut c_void,
    /// `v8::ObjectTemplate::SetAccessCheckCallback`: `(state, data, holder)`,
    /// answering 1 to allow a cross-realm access and 0 to refuse it. A null
    /// pointer means the template installed no check at all.
    pub access_check: Option<unsafe extern "C" fn(*mut c_void, u64, u64) -> i32>,
    /// The access check's own trampoline context: a template can carry a check
    /// without a named handler, so it cannot share `state`.
    pub access_check_state: *mut c_void,
}

impl fmt::Debug for SlagPropertyHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlagPropertyHandler")
            .field("getter", &self.getter.is_some())
            .field("setter", &self.setter.is_some())
            .field("query", &self.query.is_some())
            .field("deleter", &self.deleter.is_some())
            .field("indexed_getter", &self.indexed_getter.is_some())
            .field("indexed_setter", &self.indexed_setter.is_some())
            .field("indexed_query", &self.indexed_query.is_some())
            .field("indexed_deleter", &self.indexed_deleter.is_some())
            .field("enumerator", &self.enumerator.is_some())
            .field("indexed_enumerator", &self.indexed_enumerator.is_some())
            .field("descriptor", &self.descriptor.is_some())
            .field("definer", &self.definer.is_some())
            .field("indexed_descriptor", &self.indexed_descriptor.is_some())
            .field("indexed_definer", &self.indexed_definer.is_some())
            .field("access_check", &self.access_check.is_some())
            .finish()
    }
}

/// One configuration's handler, with its `data` kept alive for as long as the
/// objects made from it can call it.
pub(crate) struct CxxHandler {
    handler: SlagPropertyHandler,
    _data: Pin,
    /// The context's agent, published as a callback's token (the flat form of
    /// `FunctionCall`'s agent pointer) so a callback can allocate.
    agent: crux::function::AgentPtr,
    /// The context's pending-error slot: a callback that sets one fails the
    /// operation rather than returning, the same contract a host function has.
    pending_error: Rc<RefCell<Option<super::PendingError>>>,
}

impl CxxHandler {
    pub(crate) fn new(
        handler: SlagPropertyHandler,
        agent: crux::function::AgentPtr,
        pending_error: Rc<RefCell<Option<super::PendingError>>>,
    ) -> Self {
        Self {
            handler,
            _data: pin(Value::from_bits(handler.data)),
            agent,
            pending_error,
        }
    }
}

impl fmt::Debug for CxxHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.handler.fmt(f)
    }
}

/// The key the engine asked about, as a value a callback can receive.
fn key_value(key: &PropertyKey) -> Value {
    match key {
        PropertyKey::String(id) => Value::String(crux::handle::Handle::new(crux::lookup(*id))),
        PropertyKey::Symbol(symbol) => Value::Symbol(*symbol),
    }
}

fn thrown() -> JsError {
    JsError::new(
        ErrorKind::Error,
        "a property handler answered kThrow, whose thrown value this ABI cannot carry".into(),
    )
}

/// The error a refused access check raises, spelled the way V8 spells it.
fn illegal_access() -> JsError {
    JsError::new(ErrorKind::TypeError, "Illegal access".into())
}

/// The engine `Property` a descriptor answer describes: the engine's own
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

/// The boolean a deleter wrote into its slot: anything but `true` is a refused
/// delete, which is what an unset slot means.
fn deleted(answer: u64) -> bool {
    matches!(Value::from_bits(answer).kind(), ValueKind::Boolean(true))
}

/// The keys an enumerator answered with: the array it wrote into its slot, read
/// back as engine keys. A number element is an array index (V8's `KeepNumbers`
/// spells an index as a number), a string or a symbol is itself, and anything
/// else is skipped.
fn keys_of_answer(answer: Value) -> Vec<PropertyKey> {
    let Some(array) = answer.as_object() else {
        return Vec::new();
    };
    let length = array
        .get(&JsString::from_utf8("length"))
        .ok()
        .and_then(|value| value.as_number())
        .filter(|length| length.is_finite() && *length > 0.0)
        .unwrap_or(0.0);
    let mut keys = Vec::new();
    for position in 0..length as u64 {
        let Ok(Some(property)) = array.get_own_property_key(&PropertyKey::from_index(position))
        else {
            continue;
        };
        let crux::object::PropertyKind::Data { value, .. } = property.kind else {
            continue;
        };
        if value.is_hole() {
            continue;
        }
        match value.kind() {
            ValueKind::String(text) => keys.push(PropertyKey::from_js_string(&text)),
            ValueKind::Symbol(symbol) => keys.push(PropertyKey::Symbol(symbol)),
            // 2^32 - 2, the largest array index (spec 6.1.7.1).
            ValueKind::Number(number)
                if number.fract() == 0.0 && (0.0..=4_294_967_294.0).contains(&number) =>
            {
                keys.push(PropertyKey::from_index(number as u64));
            }
            _ => {}
        }
    }
    keys
}

impl CxxHandler {
    /// Run `body` as one host callback: the window it needs to allocate is
    /// published for its duration, and an error it recorded fails the operation
    /// the way a host function's does.
    fn callback<T>(&self, body: impl FnOnce() -> T) -> Result<T, JsError> {
        let answer = {
            let _window = CallbackWindow::enter(self.agent);
            body()
        };
        match self.pending_error.borrow_mut().take() {
            Some(pending) => Err(pending.error),
            None => Ok(answer),
        }
    }

    /// Refuse this internal method when the template's access check does
    /// (`v8::ObjectTemplate::SetAccessCheckCallback`).
    ///
    /// Only a cross-realm access is checked, which is what V8's `AccessCheck`
    /// decides before it calls anything: an object of the running realm, or of
    /// no realm at all, is the caller's own. The realm compared against is the
    /// engine's current one, so the check fires for every access JavaScript
    /// makes and for an access a host callback makes while a script runs; a
    /// host reading the object from C++ with no realm current gets the same
    /// answer V8 gives outside a context.
    fn access_denied(&self, object: &JsObject) -> Option<JsError> {
        let check = self.handler.access_check?;
        let realm = object.realm();
        if realm == 0 || realm == crux::object::current_realm() {
            return None;
        }
        let allowed = {
            let _window = CallbackWindow::enter(self.agent);
            // SAFETY: the state and data are the template's, and a template
            // outlives every instance it made.
            unsafe {
                check(
                    self.handler.access_check_state,
                    self.handler.data,
                    object.self_value().bits(),
                )
            }
        };
        (allowed == 0).then(illegal_access)
    }
}

impl HostOps for CxxHandler {
    fn get_own_property(
        &self,
        object: &JsObject,
        key: &PropertyKey,
    ) -> Option<Result<crux::object::Property, JsError>> {
        if let Some(error) = self.access_denied(object) {
            return Some(Err(error));
        }
        let intercepted = match self.callback(|| {
            // The indexed twin first, the same routing as `get`.
            if let Some(index) = crux::object::array_index_of(key)
                && let Some(callback) = self.handler.indexed_descriptor
            {
                let mut answer = 0;
                // SAFETY: as in `get`.
                let code = unsafe {
                    callback(
                        self.handler.indexed_state,
                        self.handler.data,
                        object.self_value().bits(),
                        index as u32,
                        &mut answer,
                    )
                };
                return Some((code, answer));
            }
            let callback = self.handler.descriptor?;
            let key = key_value(key);
            let mut answer = 0;
            // SAFETY: as in `get`.
            let code = unsafe {
                callback(
                    self.handler.state,
                    self.handler.data,
                    object.self_value().bits(),
                    key.bits(),
                    &mut answer,
                )
            };
            Some((code, answer))
        }) {
            Ok(Some(intercepted)) => intercepted,
            Ok(None) => return None,
            Err(error) => return Some(Err(error)),
        };
        Some(match intercepted {
            // Not intercepted: the engine's own [[GetOwnProperty]] runs, which
            // is what makes an enumerated key fall through to absence.
            (INTERCEPTED_NO, _) => return None,
            (INTERCEPTED_THROW, _) => Err(thrown()),
            (_, answer) => {
                crux::property::to_property_descriptor(&Value::from_bits(answer)).map(property_of)
            }
        })
    }

    fn define_property(
        &self,
        object: &JsObject,
        key: &PropertyKey,
        desc: &crux::property::PropertyDescriptor,
    ) -> Option<Result<bool, JsError>> {
        if let Some(error) = self.access_denied(object) {
            return Some(Err(error));
        }
        let descriptor = SlagPropertyDescriptor::of(desc);
        let intercepted = match self.callback(|| {
            if let Some(index) = crux::object::array_index_of(key)
                && let Some(callback) = self.handler.indexed_definer
            {
                let mut answer = 0;
                // SAFETY: as in `get`.
                let code = unsafe {
                    callback(
                        self.handler.indexed_state,
                        self.handler.data,
                        object.self_value().bits(),
                        index as u32,
                        &descriptor,
                        &mut answer,
                    )
                };
                return Some((code, answer));
            }
            let callback = self.handler.definer?;
            let key = key_value(key);
            let mut answer = 0;
            // SAFETY: as in `get`.
            let code = unsafe {
                callback(
                    self.handler.state,
                    self.handler.data,
                    object.self_value().bits(),
                    key.bits(),
                    &descriptor,
                    &mut answer,
                )
            };
            Some((code, answer))
        }) {
            Ok(Some(intercepted)) => intercepted,
            Ok(None) => return None,
            Err(error) => return Some(Err(error)),
        };
        match intercepted {
            // Not intercepted: the engine's own [[DefineOwnProperty]] runs.
            (INTERCEPTED_NO, _) => None,
            (INTERCEPTED_THROW, _) => Some(Err(thrown())),
            (_, answer) => Some(Ok(deleted(answer))),
        }
    }

    fn get(
        &self,
        object: &JsObject,
        key: &PropertyKey,
        receiver: &Value,
    ) -> Option<Result<Value, JsError>> {
        if let Some(error) = self.access_denied(object) {
            return Some(Err(error));
        }
        let intercepted = match self.callback(|| {
            // An array index goes to the indexed callback when one is installed,
            // and to the named one otherwise, which is V8's routing rule.
            if let Some(index) = crux::object::array_index_of(key)
                && let Some(getter) = self.handler.indexed_getter
            {
                let mut answer = 0;
                // SAFETY: as in the named call below.
                let code = unsafe {
                    getter(
                        self.handler.indexed_state,
                        self.handler.data,
                        object.self_value().bits(),
                        index as u32,
                        receiver.bits(),
                        &mut answer,
                    )
                };
                return Some((code, answer));
            }
            let getter = self.handler.getter?;
            // The key is a value the callback can name, and the local is what
            // keeps it on the collector's conservative scan while the host runs.
            let key = key_value(key);
            let mut answer = 0;
            // SAFETY: the callback is the host's, and every pointer it gets
            // points at this frame's locals.
            let code = unsafe {
                getter(
                    self.handler.state,
                    self.handler.data,
                    object.self_value().bits(),
                    key.bits(),
                    receiver.bits(),
                    &mut answer,
                )
            };
            Some((code, answer))
        }) {
            Ok(Some(intercepted)) => intercepted,
            Ok(None) => return None,
            Err(error) => return Some(Err(error)),
        };
        match intercepted {
            (INTERCEPTED_NO, _) => None,
            (INTERCEPTED_THROW, _) => Some(Err(thrown())),
            (_, answer) => Some(Ok(Value::from_bits(answer))),
        }
    }

    fn has_property(&self, object: &JsObject, key: &PropertyKey) -> Option<Result<bool, JsError>> {
        if let Some(error) = self.access_denied(object) {
            return Some(Err(error));
        }
        let intercepted = match self.callback(|| {
            // The indexed twin first, the same routing as `get`.
            if let Some(index) = crux::object::array_index_of(key)
                && let Some(query) = self.handler.indexed_query
            {
                let mut answer = 0;
                // SAFETY: as in the named call below.
                let code = unsafe {
                    query(
                        self.handler.indexed_state,
                        self.handler.data,
                        object.self_value().bits(),
                        index as u32,
                        &mut answer,
                    )
                };
                return Some((code, answer));
            }
            let query = self.handler.query?;
            let key = key_value(key);
            let mut attributes = 0;
            // SAFETY: as in `get`. The attributes a query sets are dropped: the
            // engine's `has_property` answers existence only.
            let code = unsafe {
                query(
                    self.handler.state,
                    self.handler.data,
                    object.self_value().bits(),
                    key.bits(),
                    &mut attributes,
                )
            };
            Some((code, attributes))
        }) {
            Ok(Some(intercepted)) => intercepted,
            Ok(None) => return None,
            Err(error) => return Some(Err(error)),
        };
        match intercepted {
            (INTERCEPTED_NO, _) => None,
            (INTERCEPTED_THROW, _) => Some(Err(thrown())),
            _ => Some(Ok(true)),
        }
    }

    fn set(
        &self,
        object: &JsObject,
        key: &PropertyKey,
        value: &Value,
        receiver: &Value,
        throw: bool,
    ) -> Option<Result<bool, JsError>> {
        if let Some(error) = self.access_denied(object) {
            return Some(Err(error));
        }
        let intercepted = match self.callback(|| {
            // The indexed twin first, the same routing as `get`.
            if let Some(index) = crux::object::array_index_of(key)
                && let Some(setter) = self.handler.indexed_setter
            {
                // SAFETY: as in the named call below.
                let code = unsafe {
                    setter(
                        self.handler.indexed_state,
                        self.handler.data,
                        object.self_value().bits(),
                        index as u32,
                        value.bits(),
                        receiver.bits(),
                        i32::from(throw),
                    )
                };
                return Some(code);
            }
            let setter = self.handler.setter?;
            let key = key_value(key);
            // SAFETY: as in `get`.
            Some(unsafe {
                setter(
                    self.handler.state,
                    self.handler.data,
                    object.self_value().bits(),
                    key.bits(),
                    value.bits(),
                    receiver.bits(),
                    i32::from(throw),
                )
            })
        }) {
            Ok(Some(intercepted)) => intercepted,
            Ok(None) => return None,
            Err(error) => return Some(Err(error)),
        };
        match intercepted {
            INTERCEPTED_NO => None,
            INTERCEPTED_THROW => Some(Err(thrown())),
            // The host did the set, or the engine must keep what is there: both
            // are a set the caller can see as done.
            _ => Some(Ok(true)),
        }
    }

    fn delete(&self, object: &JsObject, key: &PropertyKey) -> Option<Result<bool, JsError>> {
        if let Some(error) = self.access_denied(object) {
            return Some(Err(error));
        }
        let intercepted = match self.callback(|| {
            // The indexed twin first, the same routing as `get`.
            if let Some(index) = crux::object::array_index_of(key)
                && let Some(deleter) = self.handler.indexed_deleter
            {
                let mut answer = 0;
                // SAFETY: as in the named call below.
                let code = unsafe {
                    deleter(
                        self.handler.indexed_state,
                        self.handler.data,
                        object.self_value().bits(),
                        index as u32,
                        &mut answer,
                    )
                };
                return Some((code, answer));
            }
            let deleter = self.handler.deleter?;
            let key = key_value(key);
            let mut answer = 0;
            // SAFETY: as in `get`.
            let code = unsafe {
                deleter(
                    self.handler.state,
                    self.handler.data,
                    object.self_value().bits(),
                    key.bits(),
                    &mut answer,
                )
            };
            Some((code, answer))
        }) {
            Ok(Some(intercepted)) => intercepted,
            Ok(None) => return None,
            Err(error) => return Some(Err(error)),
        };
        match intercepted {
            (INTERCEPTED_NO, _) => None,
            (INTERCEPTED_THROW, _) => Some(Err(thrown())),
            (_, answer) => Some(Ok(deleted(answer))),
        }
    }

    fn own_property_keys(&self, object: &JsObject) -> Option<Result<Vec<PropertyKey>, JsError>> {
        if let Some(error) = self.access_denied(object) {
            return Some(Err(error));
        }
        let indexed = self.handler.indexed_enumerator;
        let named = self.handler.enumerator;
        if indexed.is_none() && named.is_none() {
            // Neither handler enumerates: the engine's own [[OwnPropertyKeys]].
            return None;
        }
        let answers = match self.callback(|| {
            let mut answers = Vec::new();
            // The indexed enumerator is asked first, which is V8's order:
            // element indices before string keys.
            if let Some(callback) = indexed {
                let mut answer = 0;
                // SAFETY: as in `get`.
                let code = unsafe {
                    callback(
                        self.handler.indexed_state,
                        self.handler.data,
                        object.self_value().bits(),
                        &mut answer,
                    )
                };
                answers.push((code, answer));
            }
            if let Some(callback) = named {
                let mut answer = 0;
                // SAFETY: as in `get`.
                let code = unsafe {
                    callback(
                        self.handler.state,
                        self.handler.data,
                        object.self_value().bits(),
                        &mut answer,
                    )
                };
                answers.push((code, answer));
            }
            answers
        }) {
            Ok(answers) => answers,
            Err(error) => return Some(Err(error)),
        };
        let mut keys = Vec::new();
        for (code, answer) in answers {
            match code {
                // Not intercepted: this enumerator adds nothing.
                INTERCEPTED_NO => {}
                INTERCEPTED_THROW => return Some(Err(thrown())),
                _ => keys.extend(keys_of_answer(Value::from_bits(answer))),
            }
        }
        Some(Ok(keys))
    }
}
