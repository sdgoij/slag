//! Host pointers as values (`v8::External`).

use std::ffi::c_void;

use runtime::api;

use crate::data::External;
use crate::handle::{Local, LocalHandle};
use crate::scope::PinScope;

impl External {
    /// A value wrapping a host pointer (`v8::External::New`).
    pub fn new<'s>(_scope: &PinScope<'s, '_, ()>, value: *mut c_void) -> Local<'s, External> {
        let object = crux::object::JsObject::external_object_create(value as usize, None);
        Local::from_engine(api::Local::from(crux::value::Value::Object(object)))
    }
}

impl<'s> LocalHandle<'s, External> {
    /// The wrapped pointer (`v8::External::Value`), null for a value that is
    /// not an external.
    pub fn value(&self) -> *mut c_void {
        api::External::from(*self.engine().value()).value()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{eval, in_context};

    #[test]
    fn a_host_pointer_round_trips() {
        in_context!(scope, {
            let pointer = 0x1234usize as *mut c_void;
            let external = External::new(scope, pointer);
            assert_eq!(external.value(), pointer);
            assert!(external.is_external());
            assert!(!eval(scope, "({})").is_external());
        });
    }
}
