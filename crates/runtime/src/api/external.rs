//! [`External`]: a host pointer wrapped in a JS value (v8::External).

use crux::error::JsError;
use crux::object::{JsObject, ObjectKind};

use super::Isolate;
use super::handle::Local;

/// A host pointer wrapped in a JS value (v8::External). The pointer is
/// stored in the object's kind; the isolate argument is accepted for API
/// shape fidelity only.
pub struct External(crux::value::Value);

impl External {
    /// v8::External::New: wrap a host pointer.
    pub fn new(_isolate: &mut Isolate, pointer: *mut std::ffi::c_void) -> Result<Self, JsError> {
        let object = JsObject::external_object_create(pointer as usize, None);
        Ok(Self(crux::value::Value::Object(object)))
    }

    /// The wrapped pointer (v8::External::Value); null when the value is
    /// not an External.
    pub fn value(&self) -> *mut std::ffi::c_void {
        match &self.0 {
            crux::value::Value::Object(object) => match &object.kind {
                ObjectKind::External(pointer) => *pointer as *mut std::ffi::c_void,
                _ => std::ptr::null_mut(),
            },
            _ => std::ptr::null_mut(),
        }
    }

    /// The External as a language value.
    pub fn as_value(&self) -> Local {
        Local(self.0.clone())
    }
}

impl From<crux::value::Value> for External {
    fn from(value: crux::value::Value) -> Self {
        Self(value)
    }
}
