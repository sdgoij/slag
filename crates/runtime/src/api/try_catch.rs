//! Pending exceptions: [`TryCatch`] observes the isolate's pending
//! exception slot; [`Exception`] creates and throws native errors.

use std::cell::Cell;

use crux::error::JsError;
use crux::handle::Handle;
use crux::string::JsString;
use crux::value::Value;

use super::Isolate;
use super::handle::Local;
/// A pending-exception observer (v8::TryCatch): created on an isolate, it
/// observes the isolate's pending exception slot, and clears the caught
/// exception on drop unless it was rethrown.
pub struct TryCatch {
    isolate: *mut Isolate,
    saved: Option<Value>,
    rethrown: Cell<bool>,
}

impl TryCatch {
    pub fn new(isolate: &mut Isolate) -> Self {
        let saved = isolate.pending_exception.borrow_mut().take();
        Self {
            isolate: isolate as *mut Isolate,
            saved,
            rethrown: Cell::new(false),
        }
    }

    /// The isolate this TryCatch observes.
    pub fn isolate(&self) -> *mut Isolate {
        self.isolate
    }

    /// Whether a pending exception is set (v8::TryCatch::HasCaught).
    pub fn has_caught(&self) -> bool {
        unsafe { &*self.isolate }.has_pending_exception()
    }

    /// The caught exception value, if set (v8::TryCatch::Exception).
    pub fn exception(&self) -> Option<Local> {
        unsafe { &*self.isolate }.pending_exception().map(Local)
    }

    /// ReThrow: mark the caught exception to stay pending after this
    /// TryCatch is dropped (v8::TryCatch::ReThrow).
    pub fn rethrow(&self) {
        self.rethrown.set(true);
    }

    /// Reset: clear the caught exception (v8::TryCatch::Reset).
    pub fn reset(&self) {
        unsafe { &*self.isolate }.take_pending_exception();
    }
}

impl Drop for TryCatch {
    fn drop(&mut self) {
        let isolate = unsafe { &*self.isolate };
        if self.rethrown.get() {
            return;
        }
        isolate.take_pending_exception();
        if let Some(saved) = self.saved.take() {
            isolate.set_pending_exception(saved);
        }
    }
}

/// v8::Exception: create a native error, throw it (set the pending
/// exception), and return it.
pub struct Exception;

impl Exception {
    /// Create and throw an error from the realm's error constructor
    /// `ctor_name` (e.g. `%TypeError%`).
    fn throw_with(isolate: &mut Isolate, ctor_name: &str, message: &str) -> Result<Local, JsError> {
        let agent = &mut isolate.agent;
        let realm = agent.current_realm()?;
        let ctor = realm.intrinsics.get(ctor_name).unwrap_or(Value::Undefined);
        let value = if crux::value::is_constructor(&ctor) {
            let text = Value::String(Handle::new(JsString::from_utf8(message)));
            crate::function::construct(agent, &ctor, &[text], &ctor)?
        } else {
            Value::String(Handle::new(JsString::from_utf8(&format!(
                "{}: {}",
                ctor_name.trim_matches('%'),
                message
            ))))
        };
        isolate.set_pending_exception(value.clone());
        Ok(Local(value))
    }

    pub fn throw_error(isolate: &mut Isolate, message: &str) -> Result<Local, JsError> {
        Self::throw_with(isolate, "%Error%", message)
    }

    pub fn throw_type_error(isolate: &mut Isolate, message: &str) -> Result<Local, JsError> {
        Self::throw_with(isolate, "%TypeError%", message)
    }

    pub fn throw_range_error(isolate: &mut Isolate, message: &str) -> Result<Local, JsError> {
        Self::throw_with(isolate, "%RangeError%", message)
    }

    pub fn throw_syntax_error(isolate: &mut Isolate, message: &str) -> Result<Local, JsError> {
        Self::throw_with(isolate, "%SyntaxError%", message)
    }

    pub fn throw_reference_error(isolate: &mut Isolate, message: &str) -> Result<Local, JsError> {
        Self::throw_with(isolate, "%ReferenceError%", message)
    }
}
