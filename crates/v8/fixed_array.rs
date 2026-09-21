//! Fixed arrays (`v8::FixedArray`): the small, fixed-size array a host reads
//! across the module boundary — the import attributes a resolve callback
//! receives, and a module's request list.
//!
//! The crate we stand in for has a C++ heap object with an integer-indexed
//! backing store. The engine has no such object, so there are two things a
//! host reads here. One is the JavaScript array the bridge built for it, and
//! these are the engine's element reads. The other is a module's requests,
//! which the engine keeps as part of the module's own record rather than as an
//! array of objects: that one carries the payload
//! (`Payload::ModuleRequests`), and its elements are
//! [`ModuleRequest`](crate::data::ModuleRequest) handles. The shape a host
//! uses — a length and `get(index)` — is the one it expects either way, and
//! nothing else is exposed.

use runtime::api;

use crate::data::{Data, FixedArray, Value};
use crate::handle::{Local, LocalHandle, Payload};
use crate::scope::PinScope;

impl<'s> LocalHandle<'s, FixedArray> {
    /// The number of elements (v8::FixedArray::Length).
    pub fn length(&self) -> usize {
        if let Some(module) = self.payload().as_module_requests() {
            return module.request_count();
        }
        let realm = crate::realm_current();
        api::Array::length(&realm, self.engine()).unwrap_or(0.0) as usize
    }

    /// The element at `index` (v8::FixedArray::Get), or `None` past the end.
    pub fn get(&self, scope: &PinScope<'s, '_>, index: usize) -> Option<Local<'s, Data>> {
        if let Some(module) = self.payload().as_module_requests() {
            let index = u32::try_from(index).ok()?;
            (usize::try_from(index).ok()? < module.request_count())
                .then(|| Local::from_payload(Payload::ModuleRequest { module, index }))
        } else {
            let realm = crate::realm_of(scope);
            api::Array::get(&realm, self.engine(), index as u32)
                .ok()
                .map(|value| Local::<Value>::from_engine(value).cast())
        }
    }
}
