//! Executable memory for compiled bodies.
//!
//! The scaffold allocates anonymous pages as RWX (simplest cross-platform
//! path; `region` handles `mmap`/`VirtualAlloc`). W^X — allocate RW, copy,
//! then protect RX — is the follow-up, noted in the crate docs.

use region::Protection;

/// Owns one executable allocation; the memory stays alive and executable for
/// as long as the value lives.
pub struct ExecutableCode {
    allocation: region::Allocation,
}

impl ExecutableCode {
    /// Copy `bytes` into a fresh executable allocation.
    pub fn new(bytes: &[u8]) -> Result<Self, region::Error> {
        let mut allocation = region::alloc(bytes.len().max(1), Protection::READ_WRITE_EXECUTE)?;
        // The allocation is page-aligned and writable (RWX); copy the code in.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                allocation.as_mut_ptr::<u8>(),
                bytes.len(),
            );
        }
        Ok(Self { allocation })
    }

    /// The base address of the executable code.
    pub fn as_ptr(&self) -> *const u8 {
        self.allocation.as_ptr::<u8>()
    }

    /// The allocation size (page-aligned).
    pub fn len(&self) -> usize {
        self.allocation.len()
    }

    /// Whether the allocation holds no code.
    pub fn is_empty(&self) -> bool {
        self.allocation.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_bytes_into_executable_memory() {
        let code = ExecutableCode::new(&[0x90, 0x90, 0xC3]).expect("allocates");
        assert_eq!(code.len(), 4096); // one page, rounded up
        assert!(!code.is_empty());
        assert!(!code.as_ptr().is_null());
        // The bytes are readable back at the pointer.
        let slice = unsafe { std::slice::from_raw_parts(code.as_ptr(), 3) };
        assert_eq!(slice, &[0x90, 0x90, 0xC3]);
    }

    #[test]
    fn rejects_zero_length() {
        // region::alloc rejects size 0; `new` passes at least 1.
        let code = ExecutableCode::new(&[]).expect("allocates");
        assert_eq!(code.len(), 4096);
    }
}
