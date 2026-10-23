//! Executable memory for compiled bodies.
//!
//! W^X: the pages are allocated read-write, the machine code is copied in,
//! and the allocation is then protected read-execute — no page is ever both
//! writable and executable (the `region` crate handles `mmap`/`VirtualAlloc`).

use region::Protection;

/// Owns one executable allocation; the memory stays alive and executable for
/// as long as the value lives.
pub struct ExecutableCode {
    allocation: region::Allocation,
}

impl ExecutableCode {
    /// Copy `bytes` into a fresh executable allocation.
    pub fn new(bytes: &[u8]) -> Result<Self, region::Error> {
        // Allocate RW, copy the code in, then flip the whole (page-aligned)
        // allocation to RX: `protect` rounds to page boundaries, which the
        // allocation already is.
        let mut allocation = region::alloc(bytes.len().max(1), Protection::READ_WRITE)?;
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                allocation.as_mut_ptr::<u8>(),
                bytes.len(),
            );
            region::protect(
                allocation.as_ptr::<u8>(),
                allocation.len(),
                Protection::READ_EXECUTE,
            )?;
            // A raised protection does not invalidate the instruction cache: on
            // a split-cache target the core may still hold lines from whatever
            // previously occupied this address (the page is fresh, the virtual
            // address is not). x86-64 is coherent and compiles this to nothing.
            flush_instruction_cache(allocation.as_ptr::<u8>(), allocation.len());
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

/// Make freshly written machine code visible to instruction fetch.
///
/// AArch64 is the one shipped split-cache target (linux-aarch64); Linux leaves
/// the user-mode cache-maintenance instructions enabled at EL0, so the standard
/// `dc cvau` / `ic ivau` sequence is the flush. Every other shipped target
/// (x86-64) has a coherent instruction cache and needs nothing. Windows on
/// AArch64 would need `FlushInstructionCache` instead, but it is not a shipped
/// target.
#[cfg(all(target_arch = "aarch64", not(target_os = "windows")))]
unsafe fn flush_instruction_cache(start: *const u8, len: usize) {
    use std::arch::asm;
    if len == 0 {
        return;
    }
    // The architectural minimum line is 64 bytes; a larger line is covered by
    // several addresses, which is harmless.
    const LINE: usize = 64;
    let begin = start as usize & !(LINE - 1);
    let end = start as usize + len;
    let mut address = begin;
    while address < end {
        unsafe { asm!("dc cvau, {}", in(reg) address, options(nostack, preserves_flags)) };
        address += LINE;
    }
    unsafe { asm!("dsb ish", options(nostack, preserves_flags)) };
    let mut address = begin;
    while address < end {
        unsafe { asm!("ic ivau, {}", in(reg) address, options(nostack, preserves_flags)) };
        address += LINE;
    }
    unsafe {
        asm!("dsb ish", options(nostack, preserves_flags));
        asm!("isb", options(nostack, preserves_flags));
    }
}

#[cfg(not(all(target_arch = "aarch64", not(target_os = "windows"))))]
unsafe fn flush_instruction_cache(_start: *const u8, _len: usize) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_bytes_into_executable_memory() {
        let code = ExecutableCode::new(&[0x90, 0x90, 0xC3]).expect("allocates");
        assert_eq!(code.len(), region::page::size()); // one page, rounded up
        assert!(!code.is_empty());
        assert!(!code.as_ptr().is_null());
        // The bytes are readable back at the pointer.
        let slice = unsafe { std::slice::from_raw_parts(code.as_ptr(), 3) };
        assert_eq!(slice, &[0x90, 0x90, 0xC3]);
    }

    #[test]
    fn allocation_is_read_execute_after_the_copy() {
        // W^X: after `new`, the page must be executable and no longer
        // writable (the region metadata reflects the current protection).
        let code = ExecutableCode::new(&[0x90, 0xC3]).expect("allocates");
        // `code` owns the allocation; `query` only reads its metadata.
        let region = region::query(code.as_ptr()).expect("queries");
        let protection = region.protection();
        assert!(
            protection.contains(region::Protection::EXECUTE),
            "the code page must be executable"
        );
        assert!(
            !protection.contains(region::Protection::WRITE),
            "the code page must not be writable (W^X)"
        );
    }

    #[test]
    fn rejects_zero_length() {
        // region::alloc rejects size 0; `new` passes at least 1.
        let code = ExecutableCode::new(&[]).expect("allocates");
        assert_eq!(code.len(), region::page::size());
    }
}
