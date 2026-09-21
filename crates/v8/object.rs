//! Objects and arrays (`v8::Object`, `v8::Array`).

use runtime::api;

use crate::data::{Array, Object, Value};
use crate::handle::Local;
use crate::scope::PinScope;

impl Object {
    /// A new ordinary object in the scope's realm (`v8::Object::New`).
    pub fn new<'s>(scope: &PinScope<'s, '_, ()>) -> Local<'s, Object> {
        let realm = crate::realm_of(scope);
        match api::Object::new(&realm) {
            Ok(object) => Local::from_engine(object),
            Err(error) => {
                crate::throw(scope, &error);
                // The caller has nowhere to put an error: `v8::Object::New`
                // has no failure channel either, so a realm that cannot make an
                // ordinary object is a bridge bug.
                panic!("bridge: creating an object failed: {error}");
            }
        }
    }
}

impl<'s> Local<'s, Object> {
    /// [[Get]] a property (`v8::Object::Get`).
    ///
    /// Only string keys are supported: Slag's public API resolves properties by
    /// name, and a key of any other type is reported the way a failed
    /// conversion is — as a pending exception.
    pub fn get<'a>(&self, scope: &PinScope<'a, '_>, key: Local<Value>) -> Option<Local<'a, Value>> {
        let name = key.engine().as_string()?;
        let realm = crate::realm_of(scope);
        match api::Object::get(&realm, self.engine(), &name) {
            Ok(value) => Some(Local::from_engine(value)),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// [[Set]] a property (`v8::Object::Set`).
    pub fn set(
        &self,
        scope: &PinScope<'_, '_>,
        key: Local<Value>,
        value: Local<Value>,
    ) -> Option<bool> {
        let name = key.engine().as_string()?;
        let realm = crate::realm_of(scope);
        match api::Object::set(&realm, self.engine(), &name, value.engine(), true) {
            Ok(ok) => Some(ok),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }

    /// [[HasProperty]] (`v8::Object::Has`).
    pub fn has(&self, scope: &PinScope<'_, '_>, key: Local<Value>) -> Option<bool> {
        let name = key.engine().as_string()?;
        let realm = crate::realm_of(scope);
        match api::Object::has(&realm, self.engine(), &name) {
            Ok(ok) => Some(ok),
            Err(error) => {
                crate::throw(scope, &error);
                None
            }
        }
    }
}

impl Array {
    /// A new array of `length` holes (`v8::Array::New`).
    pub fn new<'s>(scope: &PinScope<'s, '_, ()>, _length: i32) -> Local<'s, Array> {
        let realm = crate::realm_of(scope);
        match api::Array::new(&realm, &[]) {
            Ok(array) => Local::from_engine(array),
            Err(error) => {
                crate::throw(scope, &error);
                panic!("bridge: creating an array failed: {error}");
            }
        }
    }

    /// A new array from `elements` (`v8::Array::New` with elements).
    pub fn new_with_elements<'s>(
        scope: &PinScope<'s, '_, ()>,
        elements: &[Local<'_, Value>],
    ) -> Local<'s, Array> {
        let realm = crate::realm_of(scope);
        let values: Vec<api::Local> = elements.iter().map(|e| e.engine().clone()).collect();
        match api::Array::new(&realm, &values) {
            Ok(array) => Local::from_engine(array),
            Err(error) => {
                crate::throw(scope, &error);
                panic!("bridge: creating an array failed: {error}");
            }
        }
    }
}

impl<'s> Local<'s, Array> {
    /// The array's `length` (`v8::Array::Length`).
    pub fn length(&self) -> u32 {
        let realm = crate::realm_current();
        api::Array::length(&realm, self.engine()).map_or(0, |len| len as u32)
    }
}
