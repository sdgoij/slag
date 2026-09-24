//! Pending exceptions: [`TryCatch`] observes the isolate's pending
//! exception slot; [`Exception`] creates and throws native errors.

use std::cell::Cell;
use std::cell::RefCell;

use crux::error::JsError;
use crux::handle::Handle;
use crux::string::JsString;
use crux::value::Value;

use super::Isolate;
use super::handle::Local;
/// An exception handler (v8::TryCatch): it owns the exception that was thrown
/// into it, and restores the one that was pending when it was opened.
///
/// The owner is the difference from a plain observer. V8's handler takes the
/// exception *out* of the isolate as it catches it, so `HasCaught`/`Exception`
/// answer what it caught while the isolate is already clear — which is what lets
/// a host read an exception and then call back into JS without handing the
/// engine an exception it has dealt with.
pub struct TryCatch {
    isolate: *mut Isolate,
    /// What this handler caught, moved out of the isolate's slot the first time
    /// the handler is asked. A handler keeps the first one it caught until it is
    /// reset, the same way V8's does.
    caught: RefCell<Option<Value>>,
    /// The exception that was pending when this handler was opened, which is its
    /// parent's and comes back when this one is done.
    saved: Option<Value>,
    rethrown: Cell<bool>,
}

impl TryCatch {
    pub fn new(isolate: &mut Isolate) -> Self {
        let saved = isolate.pending_exception.borrow_mut().take();
        Self {
            isolate: isolate as *mut Isolate,
            caught: RefCell::new(None),
            saved,
            rethrown: Cell::new(false),
        }
    }

    /// The isolate this TryCatch observes.
    pub fn isolate(&self) -> *mut Isolate {
        self.isolate
    }

    /// The handler's view of the isolate.
    fn isolate_ref(&self) -> &Isolate {
        // SAFETY: the isolate outlives the handler, which was made from it.
        unsafe { &*self.isolate }
    }

    /// The exception this handler owns, moving the isolate's pending one into it
    /// on the first ask.
    fn caught(&self) -> Option<Value> {
        let mut caught = self.caught.borrow_mut();
        if caught.is_none()
            && let Some(pending) = self.isolate_ref().pending_exception()
        {
            *caught = Some(pending);
            self.isolate_ref().take_pending_exception();
        }
        *caught
    }

    /// Whether an exception was caught (v8::TryCatch::HasCaught).
    pub fn has_caught(&self) -> bool {
        self.caught().is_some()
    }

    /// Whether the isolate is terminating an execution
    /// (v8::TryCatch::HasTerminated).
    pub fn has_terminated(&self) -> bool {
        self.isolate_ref().is_execution_terminating()
    }

    /// The caught exception value, if any (v8::TryCatch::Exception).
    pub fn exception(&self) -> Option<Local> {
        self.caught().map(Local)
    }

    /// ReThrow: hand the caught exception back to the isolate so the handler
    /// around this one catches it (v8::TryCatch::ReThrow).
    pub fn rethrow(&self) {
        self.rethrown.set(true);
        if let Some(caught) = self.caught() {
            self.isolate_ref().set_pending_exception(caught);
        }
    }

    /// Reset: drop the caught exception (v8::TryCatch::Reset).
    pub fn reset(&self) {
        *self.caught.borrow_mut() = None;
        self.isolate_ref().take_pending_exception();
    }
}

impl Drop for TryCatch {
    fn drop(&mut self) {
        let saved = self.saved.take();
        let caught = self.caught.take();
        let rethrown = self.rethrown.get();
        let isolate = self.isolate_ref();
        if rethrown {
            // Whatever is pending propagates. A rethrow that this handler's
            // `caught` slot still holds (because nothing read it before the
            // rethrow) goes back too, so the handler around this one sees it.
            if let Some(caught) = caught
                && !isolate.has_pending_exception()
            {
                isolate.set_pending_exception(caught);
            }
            return;
        }
        // Not rethrown: what it caught is swallowed, and the exception that was
        // pending before it was opened comes back.
        isolate.take_pending_exception();
        if let Some(saved) = saved {
            isolate.set_pending_exception(saved);
        }
    }
}

/// v8::Exception: create a native error from a realm's error constructor,
/// optionally throwing it.
///
/// Making a value and throwing it are separate entry points because V8 separates
/// them: `Exception::Error` and its siblings only *make* a value
/// (`Isolate::ThrowException` is what throws), and a host that builds an error it
/// will not throw — or that reads `Error.prototype` off one — must not have an
/// unrelated pending exception replaced on the way. deno's
/// `is_instance_of_error` does exactly that on every translation.
pub struct Exception;

impl Exception {
    /// Create an error from the realm's error constructor `ctor_name` (e.g.
    /// `%TypeError%`), leaving the pending exception alone (v8::Exception::Error
    /// and friends).
    ///
    /// A realm whose `ctor_name` is not a constructor makes a string
    /// `"<Name>: <message>"` instead, so a caller always has a throwable value.
    pub fn create_with(
        isolate: &mut Isolate,
        ctor_name: &str,
        message: &str,
    ) -> Result<Local, JsError> {
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
        Ok(Local(value))
    }

    /// Create an error and set it as the pending exception (Isolate::ThrowException).
    fn throw_with(isolate: &mut Isolate, ctor_name: &str, message: &str) -> Result<Local, JsError> {
        let value = Self::create_with(isolate, ctor_name, message)?;
        isolate.set_pending_exception(value.into_value());
        Ok(value)
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
