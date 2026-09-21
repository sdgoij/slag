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

/// One entry of the table a snapshot indexes into (v8::ExternalReference).
#[derive(Clone, Copy)]
pub union ExternalReference {
    /// A callback.
    pub function: FunctionCallback,
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
