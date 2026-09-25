//! References the engine hands back after a snapshot is loaded
//! (`v8::ExternalReference`).
//!
//! A snapshot cannot hold a function address: the address is a property of the
//! process that loads it, not of the data, so a snapshot names an *index* into a
//! table of these instead, and the embedder rebuilds the table for every load.
//! That table is why these are a union: the embedder fills in whichever kind of
//! pointer it has, and the slot is read back as the same one.

use std::ffi::c_void;
use std::fmt;

use crate::fast_api::CFunctionInfo;
use crate::function::FunctionCallback;
use crate::interceptor::{
    IndexedPropertyDefinerCallback, IndexedPropertyDeleterCallback, IndexedPropertyGetterCallback,
    IndexedPropertyQueryCallback, IndexedPropertySetterCallback, NamedPropertyDefinerCallback,
    NamedPropertyDeleterCallback, NamedPropertyEnumeratorCallback, NamedPropertyGetterCallback,
    NamedPropertyQueryCallback, NamedPropertySetterCallback,
};

/// One entry of the table a snapshot indexes into (v8::ExternalReference).
#[derive(Clone, Copy)]
pub union ExternalReference {
    /// A callback.
    pub function: FunctionCallback,
    /// A named handler's `[[Get]]`/`[[GetOwnProperty]]` callback.
    pub named_getter: NamedPropertyGetterCallback,
    /// A named handler's `[[Set]]` callback.
    pub named_setter: NamedPropertySetterCallback,
    /// A named handler's `[[HasProperty]]` callback.
    pub named_query: NamedPropertyQueryCallback,
    /// A named handler's `[[Delete]]` callback.
    pub named_deleter: NamedPropertyDeleterCallback,
    /// A handler's `[[OwnPropertyKeys]]` callback, named or indexed (the two
    /// have the same signature).
    pub enumerator: NamedPropertyEnumeratorCallback,
    /// A named handler's `[[DefineOwnProperty]]` callback.
    pub named_definer: NamedPropertyDefinerCallback,
    /// An indexed handler's `[[Get]]`/`[[GetOwnProperty]]` callback.
    pub indexed_getter: IndexedPropertyGetterCallback,
    /// An indexed handler's `[[Set]]` callback.
    pub indexed_setter: IndexedPropertySetterCallback,
    /// An indexed handler's `[[HasProperty]]` callback.
    pub indexed_query: IndexedPropertyQueryCallback,
    /// An indexed handler's `[[Delete]]` callback.
    pub indexed_deleter: IndexedPropertyDeleterCallback,
    /// An indexed handler's `[[DefineOwnProperty]]` callback.
    pub indexed_definer: IndexedPropertyDefinerCallback,
    /// Anything else the host needs back, as a bare pointer.
    pub pointer: *mut c_void,
    /// A fast call's signature.
    pub type_info: *const CFunctionInfo,
}

impl fmt::Debug for ExternalReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: every field is one pointer, so reading any of them reads the
        // same bytes.
        unsafe { self.pointer.fmt(f) }
    }
}

impl PartialEq for ExternalReference {
    fn eq(&self, other: &Self) -> bool {
        // SAFETY: as above.
        unsafe { self.pointer == other.pointer }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field is one pointer, which is what lets a table be filled one way
    /// and compared another.
    #[test]
    fn every_field_is_one_pointer() {
        assert_eq!(
            std::mem::size_of::<ExternalReference>(),
            std::mem::size_of::<*const c_void>()
        );
    }
}
