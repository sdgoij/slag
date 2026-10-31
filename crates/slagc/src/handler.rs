//! The property handler a `v8::NamedPropertyHandlerConfiguration` installs.
//!
//! V8 consults a handler before the engine's own internal method: a `kNo` answer
//! means "not intercepted, run the ordinary method", which is the `Option` the
//! engine's [`HostOps`] seam already expresses. The five value-shaped callbacks
//! are carried here (getter, setter, query, deleter, enumerator). The descriptor
//! and definer pair need a property-descriptor ABI, so they are absent from the
//! C struct rather than accepted and ignored: a host that needs them does not
//! compile, which is the only honest failure mode.
//!
//! The `data` a configuration carries is a language value in V8, so it is pinned
//! for as long as this handler is (one handler per configuration, shared by the
//! objects made from it), and the C++ side gets it back on the callback info.

use std::ffi::c_void;
use std::fmt;

use crux::error::{ErrorKind, JsError};
use crux::heap::{Pin, pin};
use crux::host::HostOps;
use crux::object::JsObject;
use crux::property::PropertyKey;
use crux::value::{Value, ValueKind};

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
    /// The configuration's `data`, as tagged `Value` bits.
    pub data: u64,
    /// The C++ side's trampoline context for the named callbacks.
    pub state: *mut c_void,
    /// The same for the indexed callbacks, which are their own configuration.
    pub indexed_state: *mut c_void,
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
            .finish()
    }
}

/// One configuration's handler, with its `data` kept alive for as long as the
/// objects made from it can call it.
pub(crate) struct CxxHandler {
    handler: SlagPropertyHandler,
    _data: Pin,
}

impl CxxHandler {
    pub(crate) fn new(handler: SlagPropertyHandler) -> Self {
        Self {
            handler,
            _data: pin(Value::from_bits(handler.data)),
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

/// The boolean a deleter wrote into its slot: anything but `true` is a refused
/// delete, which is what an unset slot means.
fn deleted(answer: u64) -> bool {
    matches!(Value::from_bits(answer).kind(), ValueKind::Boolean(true))
}

impl HostOps for CxxHandler {
    fn get(
        &self,
        object: &JsObject,
        key: &PropertyKey,
        receiver: &Value,
    ) -> Option<Result<Value, JsError>> {
        // An array index goes to the indexed callback when one is installed, and
        // to the named one otherwise, which is V8's routing rule.
        if let Some(index) = crux::object::array_index_of(key)
            && let Some(getter) = self.handler.indexed_getter
        {
            let mut answer = 0;
            // SAFETY: as in the named call below.
            let intercepted = unsafe {
                getter(
                    self.handler.indexed_state,
                    self.handler.data,
                    object.self_value().bits(),
                    index as u32,
                    receiver.bits(),
                    &mut answer,
                )
            };
            return match intercepted {
                INTERCEPTED_NO => None,
                INTERCEPTED_THROW => Some(Err(thrown())),
                _ => Some(Ok(Value::from_bits(answer))),
            };
        }
        let getter = self.handler.getter?;
        // The key is a value the callback can name, and the local is what keeps
        // it on the collector's conservative scan while the host runs.
        let key = key_value(key);
        let mut answer = 0;
        // SAFETY: the callback is the host's, and every pointer it gets points
        // at this frame's locals.
        let intercepted = unsafe {
            getter(
                self.handler.state,
                self.handler.data,
                object.self_value().bits(),
                key.bits(),
                receiver.bits(),
                &mut answer,
            )
        };
        match intercepted {
            INTERCEPTED_NO => None,
            INTERCEPTED_THROW => Some(Err(thrown())),
            _ => Some(Ok(Value::from_bits(answer))),
        }
    }

    fn has_property(&self, object: &JsObject, key: &PropertyKey) -> Option<Result<bool, JsError>> {
        // The indexed twin first, the same routing as `get`.
        if let Some(index) = crux::object::array_index_of(key)
            && let Some(query) = self.handler.indexed_query
        {
            let mut answer = 0;
            // SAFETY: as in the named call below.
            let intercepted = unsafe {
                query(
                    self.handler.indexed_state,
                    self.handler.data,
                    object.self_value().bits(),
                    index as u32,
                    &mut answer,
                )
            };
            return match intercepted {
                INTERCEPTED_NO => None,
                INTERCEPTED_THROW => Some(Err(thrown())),
                _ => Some(Ok(true)),
            };
        }
        let query = self.handler.query?;
        let key = key_value(key);
        let mut attributes = 0;
        // SAFETY: as in `get`. The attributes a query sets are dropped: the
        // engine's `has_property` answers existence only.
        let intercepted = unsafe {
            query(
                self.handler.state,
                self.handler.data,
                object.self_value().bits(),
                key.bits(),
                &mut attributes,
            )
        };
        match intercepted {
            INTERCEPTED_NO => None,
            INTERCEPTED_THROW => Some(Err(thrown())),
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
        // The indexed twin first, the same routing as `get`.
        if let Some(index) = crux::object::array_index_of(key)
            && let Some(setter) = self.handler.indexed_setter
        {
            // SAFETY: as in the named call below.
            let intercepted = unsafe {
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
            return match intercepted {
                INTERCEPTED_NO => None,
                INTERCEPTED_THROW => Some(Err(thrown())),
                _ => Some(Ok(true)),
            };
        }
        let setter = self.handler.setter?;
        let key = key_value(key);
        // SAFETY: as in `get`.
        let intercepted = unsafe {
            setter(
                self.handler.state,
                self.handler.data,
                object.self_value().bits(),
                key.bits(),
                value.bits(),
                receiver.bits(),
                i32::from(throw),
            )
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
        // The indexed twin first, the same routing as `get`.
        if let Some(index) = crux::object::array_index_of(key)
            && let Some(deleter) = self.handler.indexed_deleter
        {
            let mut answer = 0;
            // SAFETY: as in the named call below.
            let intercepted = unsafe {
                deleter(
                    self.handler.indexed_state,
                    self.handler.data,
                    object.self_value().bits(),
                    index as u32,
                    &mut answer,
                )
            };
            return match intercepted {
                INTERCEPTED_NO => None,
                INTERCEPTED_THROW => Some(Err(thrown())),
                _ => Some(Ok(deleted(answer))),
            };
        }
        let deleter = self.handler.deleter?;
        let key = key_value(key);
        let mut answer = 0;
        // SAFETY: as in `get`.
        let intercepted = unsafe {
            deleter(
                self.handler.state,
                self.handler.data,
                object.self_value().bits(),
                key.bits(),
                &mut answer,
            )
        };
        match intercepted {
            INTERCEPTED_NO => None,
            INTERCEPTED_THROW => Some(Err(thrown())),
            _ => Some(Ok(deleted(answer))),
        }
    }
}
