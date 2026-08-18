//! Pending-exception plumbing shared by the C surfaces: converting engine
//! errors into thrown values and filling exception out-params.

use crux::error::JsError;
use crux::handle::Handle;
use crux::string::JsString;
use crux::value::Value;
use runtime::api::Isolate;

use crate::tables::retain_value;

/// Convert an engine error into a thrown value and set it as the isolate's
/// pending exception (spec ch. 17: a real Error object when the built-ins
/// are installed, else the message string). `out` is the C exception
/// out-param (a host-ref id slot): filled with the thrown value when
/// present, so the caller can hand it back to the host.
///
/// # Safety
///
/// `isolate` must point at a live isolate.
pub unsafe fn throw(isolate: *mut Isolate, error: &JsError, out: Option<&mut u64>) {
    let value = convert(isolate, error);
    unsafe { &*isolate }.set_pending_exception(value.clone());
    if let Some(slot) = out {
        *slot = retain_value(value);
    }
}

fn convert(isolate: *mut Isolate, error: &JsError) -> Value {
    let agent = unsafe { &mut *(&*isolate).agent_ptr() };
    runtime::builtins::error::to_throwable(agent, error).unwrap_or_else(|_| {
        Value::String(Handle::new(JsString::from_utf8(&format!(
            "{}: {}",
            kind_name(error.kind),
            error.message
        ))))
    })
}

fn kind_name(kind: crux::ErrorKind) -> &'static str {
    match kind {
        crux::ErrorKind::EvalError => "EvalError",
        crux::ErrorKind::RangeError => "RangeError",
        crux::ErrorKind::ReferenceError => "ReferenceError",
        crux::ErrorKind::SyntaxError => "SyntaxError",
        crux::ErrorKind::TypeError => "TypeError",
        crux::ErrorKind::UriError => "URIError",
    }
}

#[cfg(test)]
mod tests {
    use runtime::api::{Context, Isolate};

    use super::*;

    #[test]
    fn throw_sets_the_pending_exception_and_fills_the_out_param() {
        let mut isolate = Isolate::new();
        let _context = Context::new(&mut isolate).unwrap();
        let error = JsError::new(crux::ErrorKind::TypeError, "boom".into());
        let mut out = 0;
        unsafe { throw(&mut *isolate, &error, Some(&mut out)) };
        assert!(isolate.has_pending_exception());
        assert!(out != 0);
        assert_eq!(crate::tables::value(out), isolate.pending_exception());
        crate::tables::release_value(out);
    }

    #[test]
    fn throw_without_out_param_still_sets_pending() {
        let mut isolate = Isolate::new();
        let _context = Context::new(&mut isolate).unwrap();
        let error = JsError::new(crux::ErrorKind::RangeError, "boom".into());
        unsafe { throw(&mut *isolate, &error, None) };
        assert!(isolate.has_pending_exception());
    }
}
