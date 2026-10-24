//! The byte block behind an ArrayBuffer's `[[ArrayBufferData]]` (spec 25.1.1):
//! a refcounted, aliased byte vector plus the geometry flags the JIT's inline
//! element store re-reads.
//!
//! A leaf crate with no dependencies, because two unrelated owners need the
//! same type: the JS runtime (typed arrays, Atomics, SharedArrayBuffer) and the
//! WebAssembly engine, whose linear memory the JS-API's
//! `Memory.prototype.buffer` must alias rather than copy
//! (`.notes/wasm-analysis.md` §7 item 8). `crux` re-exports these items under
//! their historical paths, so `crux::typed_array::SharedBuffer` and friends
//! still resolve.
//!
//! Single-agent builds store the bytes in an `Rc<RefCell<Vec<u8>>>`:
//! borrow-checked access with no contention, and the `atomic_*` operations are
//! plain read-modify-writes. Under the `workers` feature the block is stored as
//! 8-byte words (`Arc<[AtomicU64]>`, `Send + Sync`, naturally aligned for
//! u32/u64 atomic accesses) so agents on different threads can share it, and the
//! Atomics operations perform real atomic accesses.

#[cfg(not(feature = "workers"))]
use std::cell::{Cell, RefCell};
#[cfg(not(feature = "workers"))]
use std::rc::Rc;
#[cfg(feature = "workers")]
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64};

/// An access outside the block's byte length. The runtime maps this to its own
/// `OutOfBounds` (`crux`'s `From` impl), so the engine and the JS built-ins can
/// share one block without sharing an error type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutOfBounds;

impl std::fmt::Display for OutOfBounds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("buffer access out of bounds")
    }
}

impl std::error::Error for OutOfBounds {}

fn out_of_bounds() -> OutOfBounds {
    OutOfBounds
}

/// What `op` makes of `old`: the operation table every non-atomic path shares —
/// the single-agent paths, and a borrowed block's atomics under `workers` (the
/// host's bytes are not the word array an owned block is there, so there is no
/// machine atomic to hand the operation to).
fn applied_op(op: AtomicOp, old: u64, operand: u64, expected: Option<u64>) -> u64 {
    match op {
        AtomicOp::Add => old.wrapping_add(operand),
        AtomicOp::Sub => old.wrapping_sub(operand),
        AtomicOp::And => old & operand,
        AtomicOp::Or => old | operand,
        AtomicOp::Xor => old ^ operand,
        AtomicOp::Exchange => operand,
        AtomicOp::CompareExchange => {
            if old == expected.unwrap_or(0) {
                operand
            } else {
                old
            }
        }
    }
}

/// The byte base of a storage, shared by every block that looks at it (all
/// clones of those blocks reference one box). One box keeps the JIT's inline
/// element store (gap-close M5c) sound — it re-reads the base through
/// [`SharedBuffer::state`] (the raw box address) on every store, so a resize
/// that moves the storage is picked up by the next store. The field is `pub`
/// (cross-crate `offset_of!` needs pub fields) and exists in both cfg builds
/// with an identical name — `Cell` on the single-agent path, an atomic under
/// `workers` (the box can be shared across agent threads there; the JIT inline
/// is never emitted in that build).
#[derive(Debug)]
pub struct BlockState {
    /// The address of the byte storage's first byte: the Vec's buffer
    /// single-agent (updated on resize — the Vec can realloc), the Arc
    /// word slice's base under `workers` (fixed — a resize only updates
    /// the shared byte length).
    #[cfg(not(feature = "workers"))]
    pub data: Cell<usize>,
    #[cfg(feature = "workers")]
    pub data: std::sync::atomic::AtomicUsize,
}

impl BlockState {
    fn new(data: usize) -> Self {
        #[cfg(not(feature = "workers"))]
        {
            Self {
                data: Cell::new(data),
            }
        }
        #[cfg(feature = "workers")]
        {
            Self {
                data: std::sync::atomic::AtomicUsize::new(data),
            }
        }
    }
}

/// The flags of one ArrayBuffer (spec 25.1.1), shared by every clone of *that
/// buffer's* block and by nothing else: the geometry is the storage's, the
/// flags are the buffer object's, and a block a second buffer is built over
/// gets its own (see [`SharedBuffer::for_new_buffer`]).
///
/// The runtime's `BufferState` is authoritative for all four and these mirror
/// it, because crux's integer-indexed access and the JIT's inline element store
/// read them without reaching the agent.
#[derive(Debug)]
pub struct BufferFlags {
    /// Whether the owning ArrayBuffer has been detached (spec 25.1.2.5).
    #[cfg(not(feature = "workers"))]
    pub detached: Cell<bool>,
    #[cfg(feature = "workers")]
    pub detached: std::sync::atomic::AtomicBool,
    /// Whether the owning ArrayBuffer is immutable (ES2026
    /// `transferToImmutable`): writes through views throw a TypeError.
    #[cfg(not(feature = "workers"))]
    pub immutable: Cell<bool>,
    #[cfg(feature = "workers")]
    pub immutable: std::sync::atomic::AtomicBool,
    /// Whether the owning ArrayBuffer is resizable (spec 25.1.2.4: a
    /// `maxByteLength` was supplied), so crux's TypedArray
    /// [[PreventExtensions]] can reject views that could gain or lose
    /// integer-indexed properties when the buffer is resized (spec 10.4.5.1) —
    /// and so the JIT inline can reject views whose storage can move.
    #[cfg(not(feature = "workers"))]
    pub resizable: Cell<bool>,
    #[cfg(feature = "workers")]
    pub resizable: std::sync::atomic::AtomicBool,
    /// Whether the owning buffer is a SharedArrayBuffer (spec 25.1.3.4); a
    /// shared buffer's views are fixed-length for [[PreventExtensions]].
    #[cfg(not(feature = "workers"))]
    pub is_shared: Cell<bool>,
    #[cfg(feature = "workers")]
    pub is_shared: std::sync::atomic::AtomicBool,
}

impl Default for BufferFlags {
    fn default() -> Self {
        #[cfg(not(feature = "workers"))]
        {
            Self {
                detached: Cell::new(false),
                immutable: Cell::new(false),
                resizable: Cell::new(false),
                is_shared: Cell::new(false),
            }
        }
        #[cfg(feature = "workers")]
        {
            Self {
                detached: std::sync::atomic::AtomicBool::new(false),
                immutable: std::sync::atomic::AtomicBool::new(false),
                resizable: std::sync::atomic::AtomicBool::new(false),
                is_shared: std::sync::atomic::AtomicBool::new(false),
            }
        }
    }
}

/// Whether this build shares byte blocks across agent threads (the
/// `workers` feature). The JIT's inline element store — a plain, non-atomic
/// machine write to the block — is emitted only when this is false (see
/// `crates/jit`'s `emit_typed_array_store_inline`).
pub const WORKERS: bool = cfg!(feature = "workers");

#[cfg(not(feature = "workers"))]
type StateRc = Rc<BlockState>;
#[cfg(feature = "workers")]
type StateRc = std::sync::Arc<BlockState>;

#[cfg(not(feature = "workers"))]
type FlagsRc = Rc<BufferFlags>;
#[cfg(feature = "workers")]
type FlagsRc = std::sync::Arc<BufferFlags>;

/// Memory the host owns, borrowed for as long as a block over it is alive.
///
/// The deleter runs when the last clone of the block goes — the same moment
/// V8's `BackingStore` deleter runs — so the host gets its allocation back
/// exactly when the JavaScript value that used it is gone. `None` is for memory
/// the host means to keep, such as a `static`: there is then nothing to run.
///
/// The bytes are the host's allocation and not this crate's, so there is no
/// storage to describe: the block is a pointer, a length and a way to release
/// it, in both builds. What differs under `workers` is what an *atomic* operation
/// on one does — an owned block there is a word array and hands the operation to
/// the machine, while the host's memory is only bytes, so a borrowed block's
/// atomics are plain accesses. A host must not share a borrowed block between
/// agents for that reason.
struct BorrowedBlock {
    data: *mut u8,
    byte_length: usize,
    deleter: Option<Box<dyn FnOnce()>>,
}

/// Where a borrowed block is held: `Rc` single-agent, `Arc` under `workers`,
/// because there the block itself travels to another agent's thread
/// (`runtime::workers::spawn_worker` moves one) and so must be `Send + Sync`.
#[cfg(not(feature = "workers"))]
type BorrowedRc = Rc<BorrowedBlock>;
#[cfg(feature = "workers")]
type BorrowedRc = std::sync::Arc<BorrowedBlock>;

// SAFETY: the host's bytes and its deleter do not become thread-safe by being
// held here, and this is the contract the constructor's caller accepts: under
// `workers` the block can be moved to the agent thread that uses it, so the
// memory must be usable there and the deleter must tolerate running there. The
// promise lives here rather than in a `Send` bound on the deleter because what a
// host actually passes — a C function pointer and the `*mut c_void` data it was
// registered with — is `Send` in fact and cannot say so to the compiler. V8's
// own backing store makes the same assertion about the same shape.
#[cfg(feature = "workers")]
unsafe impl Send for BorrowedBlock {}
// SAFETY: as `Send` above.
#[cfg(feature = "workers")]
unsafe impl Sync for BorrowedBlock {}

impl std::fmt::Debug for BorrowedBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BorrowedBlock")
            .field("data", &self.data)
            .field("byte_length", &self.byte_length)
            .field("deleter", &self.deleter.is_some())
            .finish()
    }
}

impl Drop for BorrowedBlock {
    fn drop(&mut self) {
        if let Some(deleter) = self.deleter.take() {
            deleter();
        }
    }
}

/// A block's storage under `workers`: a reserved virtual address range whose
/// pages are committed only as the buffer grows.
///
/// A resizable buffer has to hand out a base that never moves — the JIT bakes
/// it into compiled element accesses and every view keeps reading it — so the
/// block used to be allocated at its maximum up front, and
/// `new ArrayBuffer(0, { maxByteLength: 2**30 })` committed a gigabyte for an
/// empty buffer. Reserving the range and committing pages on `resize` keeps the
/// base fixed *and* the process's footprint proportional to what the buffer
/// actually uses.
#[cfg(feature = "workers")]
#[derive(Debug, Clone)]
pub struct WorkersBlock {
    inner: std::sync::Arc<Reservation>,
}

#[cfg(feature = "workers")]
#[derive(Debug)]
struct Reservation {
    /// The reserved range's first byte (dangling for an empty block).
    base: *mut u8,
    /// Address space reserved, in bytes: the block's capacity.
    reserved: usize,
    /// Bytes committed so far, rounded up to the page size. Monotonic: a
    /// `resize` never gives pages back, so a shrink-then-grow costs nothing.
    committed: std::sync::atomic::AtomicUsize,
}

// SAFETY: the reservation is the block's shared storage under `workers`, which
// exists so agents on several threads can address the same bytes. The bytes are
// plain memory, and the atomic operations go through `AtomicU64` on addresses
// inside it; nothing in `Reservation` is mutated without an atomic.
#[cfg(feature = "workers")]
unsafe impl Send for Reservation {}
#[cfg(feature = "workers")]
unsafe impl Sync for Reservation {}

#[cfg(feature = "workers")]
impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.base.is_null() && self.reserved > 0 {
            // SAFETY: `base`/`reserved` are exactly what `vm::reserve` returned
            // for this reservation, released once.
            unsafe { vm::release(self.base, self.reserved) };
        }
    }
}

#[cfg(feature = "workers")]
impl WorkersBlock {
    /// Reserve `capacity` bytes and commit `byte_length` of them, zero-filled.
    fn new(byte_length: usize, capacity: usize) -> Self {
        let capacity = capacity.max(byte_length);
        let base = if capacity == 0 {
            std::ptr::NonNull::<u8>::dangling().as_ptr()
        } else {
            // SAFETY: `reserve` owns nothing the caller holds.
            let base = unsafe { vm::reserve(capacity) };
            assert!(
                !base.is_null(),
                "byteblock: could not reserve {capacity} bytes of address space"
            );
            base
        };
        let reservation = Reservation {
            base,
            reserved: capacity,
            committed: std::sync::atomic::AtomicUsize::new(0),
        };
        assert!(
            byte_length == 0 || reservation.commit_to(byte_length),
            "byteblock: could not commit {byte_length} bytes of a {capacity}-byte reservation"
        );
        Self {
            inner: std::sync::Arc::new(reservation),
        }
    }

    fn as_ptr(&self) -> *const u8 {
        self.inner.base
    }

    fn capacity(&self) -> usize {
        self.inner.reserved
    }

    /// Commit enough pages to cover `len` bytes; false when the range is larger
    /// than the reservation or the platform refuses.
    fn commit(&self, len: usize) -> bool {
        self.inner.commit_to(len)
    }

    /// The number of bytes committed so far (page-rounded), for tests.
    #[cfg(test)]
    pub(crate) fn committed(&self) -> usize {
        self.inner
            .committed
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(feature = "workers")]
impl Reservation {
    fn commit_to(&self, len: usize) -> bool {
        if len > self.reserved {
            return false;
        }
        use std::sync::atomic::Ordering;
        let already = self.committed.load(Ordering::Relaxed);
        if len <= already {
            return true;
        }
        // Commit whole pages, never past the reservation.
        let page = vm::page_size();
        let target = len.div_ceil(page).saturating_mul(page).min(self.reserved);
        // SAFETY: `already` and `target` are page-aligned offsets inside the
        // reservation, and the range `already..target` is not committed yet.
        if !unsafe { vm::commit(self.base.add(already), target - already) } {
            return false;
        }
        self.committed.store(target, Ordering::Relaxed);
        true
    }
}

/// The platform's reserve/commit/release primitives.
///
/// `reserve` claims address space without backing it with physical memory;
/// `commit` makes a prefix usable; `release` gives the range back. On Unix a
/// single `mmap` is both reserve and commit (pages are backed on first touch),
/// so `commit` is a bookkeeping no-op there.
#[cfg(all(feature = "workers", windows))]
mod vm {
    const MEM_COMMIT: u32 = 0x0000_1000;
    const MEM_RESERVE: u32 = 0x0000_2000;
    const MEM_RELEASE: u32 = 0x0000_8000;
    const PAGE_READWRITE: u32 = 0x04;
    /// The page granularity `commit` rounds to. Windows commits in pages; its
    /// 64 KiB *allocation granularity* only applies to `MEM_RESERVE`, which
    /// rounds internally.
    const PAGE_SIZE: usize = 4096;

    unsafe extern "system" {
        fn VirtualAlloc(
            address: *mut core::ffi::c_void,
            size: usize,
            allocation_type: u32,
            protect: u32,
        ) -> *mut core::ffi::c_void;
        fn VirtualFree(address: *mut core::ffi::c_void, size: usize, free_type: u32) -> i32;
    }

    pub(crate) fn page_size() -> usize {
        PAGE_SIZE
    }

    /// # Safety
    /// Caller must release the returned range with [`release`] exactly once.
    pub(crate) unsafe fn reserve(len: usize) -> *mut u8 {
        // SAFETY: a fresh reservation, released by `release`.
        unsafe { VirtualAlloc(std::ptr::null_mut(), len, MEM_RESERVE, PAGE_READWRITE) }.cast()
    }

    /// # Safety
    /// `base..base + len` must be inside a reservation this module made, and
    /// must not already be committed.
    pub(crate) unsafe fn commit(base: *mut u8, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        // SAFETY: as the contract says; committed pages come back zeroed.
        !unsafe { VirtualAlloc(base.cast(), len, MEM_COMMIT, PAGE_READWRITE) }.is_null()
    }

    /// # Safety
    /// `base` must be a reservation from [`reserve`]; called once.
    pub(crate) unsafe fn release(base: *mut u8, _len: usize) {
        // SAFETY: as the contract says.
        unsafe { VirtualFree(base.cast(), 0, MEM_RELEASE) };
    }
}

#[cfg(all(feature = "workers", unix))]
mod vm {
    use core::ffi::{c_int, c_void};

    const PROT_READ: c_int = 1;
    const PROT_WRITE: c_int = 2;
    const MAP_PRIVATE: c_int = 2;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const MAP_ANONYMOUS: c_int = 0x20;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const MAP_ANONYMOUS: c_int = 0x1000;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const MAP_NORESERVE: c_int = 0x4000;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const MAP_NORESERVE: c_int = 0x40;

    unsafe extern "C" {
        fn mmap(
            address: *mut c_void,
            length: usize,
            protect: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
        ) -> *mut c_void;
        fn munmap(address: *mut c_void, length: usize) -> c_int;
    }

    pub(crate) fn page_size() -> usize {
        4096
    }

    /// # Safety
    /// Caller must release the returned range with [`release`] exactly once.
    pub(crate) unsafe fn reserve(len: usize) -> *mut u8 {
        // SAFETY: an anonymous private mapping, released by `release`. The
        // pages are backed only when touched, which is what makes the
        // reservation cheap.
        let base = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE,
                -1,
                0,
            )
        };
        if base as isize == -1 {
            std::ptr::null_mut()
        } else {
            base.cast()
        }
    }

    /// # Safety
    /// `base..base + len` must be inside a reservation this module made.
    pub(crate) unsafe fn commit(_base: *mut u8, _len: usize) -> bool {
        // The mapping is already writable; a page is backed on first touch.
        true
    }

    /// # Safety
    /// `base` must be a reservation from [`reserve`]; called once.
    pub(crate) unsafe fn release(base: *mut u8, len: usize) {
        // SAFETY: as the contract says.
        unsafe { munmap(base.cast(), len) };
    }
}

/// Targets with neither `VirtualAlloc` nor `mmap` back the reservation eagerly,
/// which is the behaviour this module had before the reservation existed.
#[cfg(all(feature = "workers", not(any(windows, unix))))]
mod vm {
    pub(crate) fn page_size() -> usize {
        4096
    }

    /// # Safety
    /// Caller must release the returned range with [`release`] exactly once.
    pub(crate) unsafe fn reserve(len: usize) -> *mut u8 {
        let words = len.div_ceil(8);
        let block: Box<[std::sync::atomic::AtomicU64]> = (0..words)
            .map(|_| std::sync::atomic::AtomicU64::new(0))
            .collect();
        Box::into_raw(block).cast::<u8>()
    }

    /// # Safety
    /// As the trait's other implementations.
    pub(crate) unsafe fn commit(_base: *mut u8, _len: usize) -> bool {
        true
    }

    /// # Safety
    /// `base` must be a reservation from [`reserve`]; called once.
    pub(crate) unsafe fn release(base: *mut u8, len: usize) {
        let words = len.div_ceil(8);
        // SAFETY: rebuilding the box `reserve` leaked.
        drop(unsafe {
            Box::from_raw(std::slice::from_raw_parts_mut(
                base.cast::<std::sync::atomic::AtomicU64>(),
                words,
            ) as *mut [std::sync::atomic::AtomicU64])
        });
    }
}

/// What the operating system says about a reserved range, for the reservation
/// test. Absent on platforms with no probe, which skips the platform half of
/// the assertion.
#[cfg(all(feature = "workers", test))]
mod probe {
    use core::ffi::c_void;

    #[cfg(windows)]
    #[repr(C)]
    struct MemoryBasicInformation {
        base_address: *mut c_void,
        allocation_base: *mut c_void,
        allocation_protect: u32,
        partition_id: u16,
        _pad1: u16,
        region_size: usize,
        state: u32,
        protect: u32,
        kind: u32,
        _pad2: u32,
    }

    /// Whether the page holding `address` is backed by physical memory, or
    /// `None` when this platform has no way to ask.
    #[cfg(windows)]
    pub(crate) fn is_committed(address: *const u8) -> Option<bool> {
        const MEM_COMMIT: u32 = 0x0000_1000;
        const MEM_RESERVE: u32 = 0x0000_2000;

        unsafe extern "system" {
            fn VirtualQuery(
                address: *const c_void,
                information: *mut MemoryBasicInformation,
                length: usize,
            ) -> usize;
        }

        let mut info = MemoryBasicInformation {
            base_address: std::ptr::null_mut(),
            allocation_base: std::ptr::null_mut(),
            allocation_protect: 0,
            partition_id: 0,
            _pad1: 0,
            region_size: 0,
            state: 0,
            protect: 0,
            kind: 0,
            _pad2: 0,
        };
        // SAFETY: `info` is a writable `MEMORY_BASIC_INFORMATION`, which is what
        // `VirtualQuery` fills in and what its length argument must name.
        let written = unsafe {
            VirtualQuery(
                address.cast(),
                &mut info,
                std::mem::size_of::<MemoryBasicInformation>(),
            )
        };
        if written == 0 {
            return None;
        }
        match info.state {
            MEM_COMMIT => Some(true),
            MEM_RESERVE => Some(false),
            _ => None,
        }
    }

    #[cfg(not(windows))]
    pub(crate) fn is_committed(_address: *const u8) -> Option<bool> {
        None
    }

    /// The process's resident set size in bytes, or `None` where this platform
    /// has no cheap way to read it.
    #[cfg(target_os = "linux")]
    pub(crate) fn resident_bytes() -> Option<usize> {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        // "size resident shared ..." in pages.
        let resident: usize = statm.split_whitespace().nth(1)?.parse().ok()?;
        Some(resident * 4096)
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn resident_bytes() -> Option<usize> {
        None
    }
}

/// The [[ArrayBufferData]] of an ArrayBuffer: a shared byte vector aliased
/// by every TypedArray that views the buffer (spec 25.1.1).
///
/// Single-agent builds store the bytes in an `Rc<RefCell<Vec<u8>>>`:
/// borrow-checked access with no contention, and the `atomic_*` operations
/// are plain read-modify-writes. Under the `workers` feature the block is
/// stored as 8-byte words (`Arc<[AtomicU64]>`, `Send + Sync`, naturally
/// aligned for u32/u64 atomic accesses) so agents on different threads can
/// share it, and the Atomics operations perform real atomic accesses.
#[derive(Debug, Clone)]
pub struct SharedBuffer {
    #[cfg(not(feature = "workers"))]
    block: Rc<RefCell<Vec<u8>>>,
    #[cfg(feature = "workers")]
    block: WorkersBlock,
    #[cfg(feature = "workers")]
    byte_length: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// The host's bytes, when this block borrows rather than owns them.
    ///
    /// `None` for every block the engine allocated, and `block` above is then
    /// the storage. A borrowed block keeps `block` empty, because its bytes live
    /// in the host's allocation: every access path through the block checks this
    /// first and reads the host's memory directly. The address it holds is also
    /// what [`BlockState::data`] points at, so the JIT's inline element store
    /// (which reads the base through the state box) needs no special case —
    /// borrowed memory does not move, so the base is stable by construction.
    borrowed: Option<BorrowedRc>,
    /// The geometry box (see [`BlockState`]): the live byte base, shared by
    /// every block over this storage (`Rc` single-agent, `Arc` under `workers`).
    state_rc: StateRc,
    /// The address of the `state_rc` box's value — the JIT's inline element
    /// store dereferences it (`offset_of!(SharedBuffer, state)` + the
    /// `BlockState` field offsets) to read the live data base.
    /// The `Rc`/`Arc` box layout is not `offset_of!`-expressible across
    /// crates, so the value address is stored raw. Stable for the box's
    /// lifetime; `state_rc` keeps the box alive for every clone.
    pub state: usize,
    /// The flags box (see [`BufferFlags`]): shared by every clone of *this*
    /// block — the buffer object and its views — and renewed when a second
    /// buffer is built over the same storage.
    flags_rc: FlagsRc,
    /// The address of the `flags_rc` box's value, read the same way [`state`]
    /// is (`offset_of!(SharedBuffer, flags)` + the `BufferFlags` field offsets).
    ///
    /// [`state`]: Self::state
    pub flags: usize,
}

/// The read-modify-write operations of the Atomics built-ins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomicOp {
    Add,
    Sub,
    And,
    Or,
    Xor,
    Exchange,
    CompareExchange,
}

impl SharedBuffer {
    /// Allocate a zero-filled buffer of `byte_length` bytes.
    pub fn new(byte_length: usize) -> Self {
        Self::new_with_capacity(byte_length, byte_length)
    }

    /// Allocate a zero-filled buffer with storage for up to `capacity` bytes.
    /// Resizable/growable buffers must keep one base across a resize, so views
    /// created before it stay live: under `workers` the range is *reserved* at
    /// its capacity and committed as the buffer grows, and the single-agent
    /// path resizes its `Vec` in place and refreshes the shared base.
    pub fn new_with_capacity(byte_length: usize, capacity: usize) -> Self {
        #[cfg(not(feature = "workers"))]
        {
            let _ = capacity;
            let block = Rc::new(RefCell::new(vec![0u8; byte_length]));
            let state_rc = Rc::new(BlockState::new(block.borrow().as_ptr() as usize));
            let state = &*state_rc as *const BlockState as usize;
            let flags_rc = FlagsRc::new(BufferFlags::default());
            let flags = &*flags_rc as *const BufferFlags as usize;
            SharedBuffer {
                block,
                borrowed: None,
                state_rc,
                state,
                flags_rc,
                flags,
            }
        }
        #[cfg(feature = "workers")]
        {
            let block = WorkersBlock::new(byte_length, capacity);
            let state_rc = std::sync::Arc::new(BlockState::new(block.as_ptr() as usize));
            let state = &*state_rc as *const BlockState as usize;
            let flags_rc = FlagsRc::new(BufferFlags::default());
            let flags = &*flags_rc as *const BufferFlags as usize;
            SharedBuffer {
                block,
                byte_length: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(byte_length)),
                borrowed: None,
                state_rc,
                state,
                flags_rc,
                flags,
            }
        }
    }

    /// A block over the same bytes that belongs to a **different** buffer
    /// object.
    ///
    /// The storage and its live base are shared — a resize still moves what
    /// every block over these bytes reads — and the flags start fresh, because
    /// they mirror a `BufferState` and that is per object. The runtime calls
    /// this wherever it wraps a block in a buffer that did not create it (a
    /// store a host hands back, a wasm memory's `buffer`): without it, a buffer
    /// built over a detached buffer's storage inherits the detach and reads as
    /// dead while its own `byteLength` is live.
    ///
    /// [`clone`](Self::clone) shares the flags instead, which is what a view of
    /// one buffer needs: it must see that buffer's own detach.
    pub fn for_new_buffer(&self) -> Self {
        let flags_rc = FlagsRc::new(BufferFlags::default());
        let flags = &*flags_rc as *const BufferFlags as usize;
        Self {
            block: self.block.clone(),
            #[cfg(feature = "workers")]
            byte_length: self.byte_length.clone(),
            borrowed: self.borrowed.clone(),
            state_rc: self.state_rc.clone(),
            state: self.state,
            flags_rc,
            flags,
        }
    }

    /// A block over memory the host owns, borrowed rather than copied.
    ///
    /// The host keeps the allocation, so an access through this block is an
    /// access through the host's bytes in both directions — which is what makes
    /// this the constructor for handing memory the host also works with.
    ///
    /// The block owns nothing, so it cannot be resized: the host's allocation is
    /// what it is, and `resize` refuses rather than reallocating memory the host
    /// would still be holding.
    ///
    /// Under `workers` the block travels in an `Arc` like any other, but its
    /// atomic operations stay plain accesses rather than machine atomics, and the
    /// host must not share one of these blocks between agents: the storage here is
    /// the host's byte range, not the word array an owned block is.
    ///
    /// # Safety
    ///
    /// `data` must point at `byte_length` bytes that stay readable and writable
    /// for as long as any clone of this block lives, and that are sound to access
    /// as bytes. `deleter` must be the right way to release them, and runs when
    /// the last clone goes. In a `workers` build the last clone can be dropped on
    /// another thread, so the memory and the deleter must be sound to use there.
    pub unsafe fn borrowed(
        data: *mut u8,
        byte_length: usize,
        deleter: Option<Box<dyn FnOnce()>>,
    ) -> Self {
        let borrowed = BorrowedRc::new(BorrowedBlock {
            data,
            byte_length,
            deleter,
        });
        // The state box's base is the host's address, which borrowed memory does
        // not move out of, so it is set once and never refreshed.
        let state_rc = StateRc::new(BlockState::new(data as usize));
        let state = &*state_rc as *const BlockState as usize;
        #[cfg(not(feature = "workers"))]
        let block = Rc::new(RefCell::new(Vec::new()));
        // Nothing reserved: the bytes are the host's, and every path that would
        // read the block's storage checks `borrowed` first.
        #[cfg(feature = "workers")]
        let block = WorkersBlock::new(0, 0);
        #[cfg(feature = "workers")]
        let byte_length = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(byte_length));
        let flags_rc = FlagsRc::new(BufferFlags::default());
        let flags = &*flags_rc as *const BufferFlags as usize;
        SharedBuffer {
            block,
            #[cfg(feature = "workers")]
            byte_length,
            borrowed: Some(borrowed),
            state_rc,
            state,
            flags_rc,
            flags,
        }
    }

    /// Whether this block borrows the host's memory rather than owning its own.
    ///
    /// A host that hands memory over wants to know: the bytes are then shared
    /// with whoever owns them, which is the point, and a buffer over this block
    /// must not be assumed to have storage of its own.
    pub fn is_borrowed(&self) -> bool {
        self.borrowed.is_some()
    }

    /// Mark the owning buffer detached (mirrors the runtime's `BufferState`).
    pub fn mark_detached(&self) {
        #[cfg(not(feature = "workers"))]
        {
            self.flags_rc.detached.set(true);
        }
        #[cfg(feature = "workers")]
        {
            self.flags_rc
                .detached
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Whether the owning buffer has been detached.
    pub fn is_detached(&self) -> bool {
        #[cfg(not(feature = "workers"))]
        {
            self.flags_rc.detached.get()
        }
        #[cfg(feature = "workers")]
        {
            self.flags_rc
                .detached
                .load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Mark the owning buffer immutable (ES2026 transferToImmutable).
    pub fn mark_immutable(&self) {
        #[cfg(not(feature = "workers"))]
        {
            self.flags_rc.immutable.set(true);
        }
        #[cfg(feature = "workers")]
        {
            self.flags_rc
                .immutable
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Whether the owning buffer is immutable.
    pub fn is_immutable(&self) -> bool {
        #[cfg(not(feature = "workers"))]
        {
            self.flags_rc.immutable.get()
        }
        #[cfg(feature = "workers")]
        {
            self.flags_rc
                .immutable
                .load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Mark the owning buffer resizable (mirrors `BufferState.resizable`).
    pub fn mark_resizable(&self) {
        #[cfg(not(feature = "workers"))]
        {
            self.flags_rc.resizable.set(true);
        }
        #[cfg(feature = "workers")]
        {
            self.flags_rc
                .resizable
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Whether the owning buffer is a resizable ArrayBuffer.
    pub fn is_resizable(&self) -> bool {
        #[cfg(not(feature = "workers"))]
        {
            self.flags_rc.resizable.get()
        }
        #[cfg(feature = "workers")]
        {
            self.flags_rc
                .resizable
                .load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Mark the owning buffer as a SharedArrayBuffer (mirrors
    /// `BufferState.is_shared`).
    pub fn mark_shared(&self) {
        #[cfg(not(feature = "workers"))]
        {
            self.flags_rc.is_shared.set(true);
        }
        #[cfg(feature = "workers")]
        {
            self.flags_rc
                .is_shared
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Whether the owning buffer is a SharedArrayBuffer.
    pub fn is_shared(&self) -> bool {
        #[cfg(not(feature = "workers"))]
        {
            self.flags_rc.is_shared.get()
        }
        #[cfg(feature = "workers")]
        {
            self.flags_rc
                .is_shared
                .load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    pub fn byte_length(&self) -> usize {
        if let Some(borrowed) = &self.borrowed {
            return borrowed.byte_length;
        }
        #[cfg(not(feature = "workers"))]
        {
            self.block.borrow().len()
        }
        #[cfg(feature = "workers")]
        {
            self.byte_length.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    /// The bytes committed so far, page-rounded (tests, `workers` only).
    #[cfg(all(test, feature = "workers"))]
    pub(crate) fn committed(&self) -> usize {
        self.block.committed()
    }

    /// A stable identity for the underlying byte block (the allocation
    /// address), used to key the Atomics wait registry.
    pub fn block_id(&self) -> usize {
        if let Some(borrowed) = &self.borrowed {
            // The identity of borrowed memory is the host's address: two blocks
            // over one allocation are one block to the registry, as they are to
            // everything else that addresses the bytes.
            return borrowed.data as usize;
        }
        #[cfg(not(feature = "workers"))]
        {
            Rc::as_ptr(&self.block) as usize
        }
        #[cfg(feature = "workers")]
        {
            self.block.as_ptr() as usize
        }
    }

    /// The address of the block's first byte.
    ///
    /// This is the *live* base — `resize` refreshes it when the storage moves
    /// (the single-agent `Vec` can realloc) — and it is the same address the
    /// JIT's inline element store reads through [`BlockState::data`], which
    /// every clone shares. A caller addressing the bytes directly must
    /// therefore re-read it after any resize rather than caching it across a
    /// grow, and must not hold a slice built from it across one either.
    ///
    /// # Access discipline
    ///
    /// A single-agent block is borrowed through a `RefCell`, so a caller that
    /// reads or writes through this pointer must not hold the borrow across a
    /// call that borrows the same block.
    ///
    /// Under `workers` the storage is an atomic word array rather than a
    /// `Vec<u8>`, so an access through this pointer is a plain (non-atomic)
    /// one. That is sound only while no other agent can observe the block at
    /// the same time — true for the linear memory of a `WebAssembly.Memory`
    /// the JS-API owns and has not handed to another agent, which is the case
    /// this exists for. A block another agent can see must be touched through
    /// the atomic operations instead.
    pub fn data_ptr(&self) -> *mut u8 {
        #[cfg(not(feature = "workers"))]
        {
            self.state_rc.data.get() as *mut u8
        }
        #[cfg(feature = "workers")]
        {
            self.state_rc
                .data
                .load(std::sync::atomic::Ordering::Relaxed) as *mut u8
        }
    }

    /// Copy `len` bytes out of the block at `offset` (a plain read; the
    /// caller synchronizes concurrent access).
    pub fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>, OutOfBounds> {
        let end = offset.checked_add(len).ok_or_else(out_of_bounds)?;
        if let Some(borrowed) = &self.borrowed {
            if end > borrowed.byte_length {
                return Err(out_of_bounds());
            }
            let mut out = vec![0u8; len];
            // SAFETY: bounds-checked above, and the host promised the bytes
            // stay valid for as long as this block lives.
            unsafe {
                std::ptr::copy_nonoverlapping(borrowed.data.add(offset), out.as_mut_ptr(), len);
            }
            return Ok(out);
        }
        #[cfg(not(feature = "workers"))]
        {
            let data = self.block.borrow();
            data.get(offset..end)
                .map(<[u8]>::to_vec)
                .ok_or_else(out_of_bounds)
        }
        #[cfg(feature = "workers")]
        {
            if end > self.byte_length() {
                return Err(out_of_bounds());
            }
            let base = self.block.as_ptr() as *const AtomicU64 as *const u8;
            let mut out = vec![0u8; len];
            // SAFETY: bounds-checked above; the Arc keeps the block alive.
            unsafe {
                std::ptr::copy_nonoverlapping(base.add(offset), out.as_mut_ptr(), len);
            }
            Ok(out)
        }
    }

    /// Copy `out.len()` bytes out of the block at `offset` into `out` — the
    /// allocation-free sibling of `read`, for the hot typed-array element
    /// read path (the element decodes from a stack buffer, so a per-element
    /// `Vec` would be pure churn). Errors like `read` when the range is out
    /// of bounds.
    pub fn read_into(&self, offset: usize, out: &mut [u8]) -> Result<(), OutOfBounds> {
        let end = offset.checked_add(out.len()).ok_or_else(out_of_bounds)?;
        if let Some(borrowed) = &self.borrowed {
            if end > borrowed.byte_length {
                return Err(out_of_bounds());
            }
            // SAFETY: bounds-checked above, and the host promised the bytes
            // stay valid for as long as this block lives.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    borrowed.data.add(offset),
                    out.as_mut_ptr(),
                    out.len(),
                );
            }
            return Ok(());
        }
        #[cfg(not(feature = "workers"))]
        {
            let data = self.block.borrow();
            data.get(offset..end)
                .map(|slice| out.copy_from_slice(slice))
                .ok_or_else(out_of_bounds)
        }
        #[cfg(feature = "workers")]
        {
            if end > self.byte_length() {
                return Err(out_of_bounds());
            }
            let base = self.block.as_ptr() as *const AtomicU64 as *const u8;
            // SAFETY: bounds-checked above; the Arc keeps the block alive.
            unsafe {
                std::ptr::copy_nonoverlapping(base.add(offset), out.as_mut_ptr(), out.len());
            }
            Ok(())
        }
    }

    /// Copy `bytes` into the block at `offset` (a plain write; the caller
    /// synchronizes concurrent access).
    pub fn write(&self, offset: usize, bytes: &[u8]) -> Result<(), OutOfBounds> {
        let end = offset.checked_add(bytes.len()).ok_or_else(out_of_bounds)?;
        if let Some(borrowed) = &self.borrowed {
            if end > borrowed.byte_length {
                return Err(out_of_bounds());
            }
            // SAFETY: bounds-checked above, and the host promised the bytes
            // stay writable for as long as this block lives.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    borrowed.data.add(offset),
                    bytes.len(),
                );
            }
            return Ok(());
        }
        #[cfg(not(feature = "workers"))]
        {
            let mut data = self.block.borrow_mut();
            let Some(slot) = data.get_mut(offset..end) else {
                return Err(out_of_bounds());
            };
            slot.copy_from_slice(bytes);
            Ok(())
        }
        #[cfg(feature = "workers")]
        {
            if end > self.byte_length() {
                return Err(out_of_bounds());
            }
            let base = self.block.as_ptr() as *const AtomicU64 as *mut u8;
            // SAFETY: bounds-checked above; the Arc keeps the block alive.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), base.add(offset), bytes.len());
            }
            Ok(())
        }
    }

    /// Grow/shrink the block to `new_length`. Under `workers` the block is a
    /// reserved range (resizable/growable buffers reserve their maximum), so a
    /// resize commits the pages the new length needs, zero-fills the newly
    /// exposed region — visible to every view clone — and never moves the base.
    /// The single-agent path resizes the shared `Vec` in place.
    pub fn resize(&mut self, new_length: usize) -> Result<(), OutOfBounds> {
        if self.borrowed.is_some() {
            // Borrowed memory cannot grow: the allocation belongs to the host,
            // which is still holding it. `OutOfBounds` is the only failure this
            // signature can report, and the honest reading of it here is "that
            // length is not inside a block this one can be".
            return Err(out_of_bounds());
        }
        #[cfg(not(feature = "workers"))]
        {
            self.block.borrow_mut().resize(new_length, 0);
            // The Vec can realloc on a grow; refresh the mirror so the JIT
            // inline (and any later view) reads the live base.
            self.state_rc
                .data
                .set(self.block.borrow().as_ptr() as usize);
            Ok(())
        }
        #[cfg(feature = "workers")]
        {
            if new_length > self.block.capacity() {
                return Err(out_of_bounds());
            }
            let old_length = self.byte_length.load(std::sync::atomic::Ordering::Relaxed);
            if new_length > old_length {
                // Commit before writing: the pages past the old length may not
                // be backed yet.
                if !self.block.commit(new_length) {
                    return Err(out_of_bounds());
                }
                let base = self.block.as_ptr() as *mut u8;
                // SAFETY: `new_length <= capacity`, the pages are committed, and
                // the reservation keeps them mapped for the block's life.
                unsafe {
                    std::ptr::write_bytes(base.add(old_length), 0, new_length - old_length);
                }
            }
            self.byte_length
                .store(new_length, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }

    /// The first `size` bytes at `offset` as the native-order integer they
    /// encode, read atomically under `workers` for a block the engine owns (plain
    /// single-agent, and plain in either build for a borrowed block — the host's
    /// bytes are not this crate's word array).
    pub fn atomic_load(&self, offset: usize, size: usize) -> Result<u64, OutOfBounds> {
        if self.borrowed.is_some() {
            let bytes = self.read(offset, size)?;
            return raw_from_bytes(&bytes);
        }
        #[cfg(feature = "workers")]
        {
            if offset + size > self.byte_length() {
                return Err(out_of_bounds());
            }
            Ok(match size {
                1 => unsafe { &*self.atomic_ptr::<AtomicU8>(offset) }
                    .load(std::sync::atomic::Ordering::SeqCst) as u64,
                2 => unsafe { &*self.atomic_ptr::<AtomicU16>(offset) }
                    .load(std::sync::atomic::Ordering::SeqCst) as u64,
                4 => unsafe { &*self.atomic_ptr::<AtomicU32>(offset) }
                    .load(std::sync::atomic::Ordering::SeqCst) as u64,
                8 => unsafe { &*self.atomic_ptr::<AtomicU64>(offset) }
                    .load(std::sync::atomic::Ordering::SeqCst),
                _ => return Err(out_of_bounds()),
            })
        }
        #[cfg(not(feature = "workers"))]
        {
            let bytes = self.read(offset, size)?;
            raw_from_bytes(&bytes)
        }
    }

    /// Store `value` (the native-order integer encoding of the first `size`
    /// bytes) at `offset`, atomically under `workers` for a block the engine owns
    /// and as a plain write for a borrowed one.
    pub fn atomic_store(&self, offset: usize, size: usize, value: u64) -> Result<(), OutOfBounds> {
        if self.borrowed.is_some() {
            return self.write(offset, &bytes_from_raw(value, size)?);
        }
        #[cfg(feature = "workers")]
        {
            if offset + size > self.byte_length() {
                return Err(out_of_bounds());
            }
            match size {
                1 => unsafe { &*self.atomic_ptr::<AtomicU8>(offset) }
                    .store(value as u8, std::sync::atomic::Ordering::SeqCst),
                2 => unsafe { &*self.atomic_ptr::<AtomicU16>(offset) }
                    .store(value as u16, std::sync::atomic::Ordering::SeqCst),
                4 => unsafe { &*self.atomic_ptr::<AtomicU32>(offset) }
                    .store(value as u32, std::sync::atomic::Ordering::SeqCst),
                8 => unsafe { &*self.atomic_ptr::<AtomicU64>(offset) }
                    .store(value, std::sync::atomic::Ordering::SeqCst),
                _ => return Err(out_of_bounds()),
            }
            Ok(())
        }
        #[cfg(not(feature = "workers"))]
        {
            self.write(offset, &bytes_from_raw(value, size)?)?;
            Ok(())
        }
    }

    /// The atomic read-modify-write of `op` on the `size`-byte integer at
    /// `offset`, returning the old value. `expected` is the compare value for
    /// `CompareExchange`. Real atomics under `workers` for a block the engine
    /// owns; a plain RMW in every other case.
    pub fn atomic_rmw(
        &self,
        op: AtomicOp,
        offset: usize,
        size: usize,
        operand: u64,
        expected: Option<u64>,
    ) -> Result<u64, OutOfBounds> {
        if self.borrowed.is_some() {
            // The same read-modify-write, one step removed: `read` and `write`
            // reach the host's bytes, so the compound operation does not need a
            // path of its own through them.
            let mut bytes = self.read(offset, size)?;
            let old = raw_from_bytes(&bytes)?;
            let next = applied_op(op, old, operand, expected);
            bytes.copy_from_slice(&bytes_from_raw(next, size)?);
            self.write(offset, &bytes)?;
            return Ok(old);
        }
        #[cfg(feature = "workers")]
        {
            if offset + size > self.byte_length() {
                return Err(out_of_bounds());
            }
            let ordering = std::sync::atomic::Ordering::SeqCst;
            Ok(match (size, op) {
                (1, AtomicOp::Add) => unsafe { &*self.atomic_ptr::<AtomicU8>(offset) }
                    .fetch_add(operand as u8, ordering)
                    as u64,
                (1, AtomicOp::Sub) => unsafe { &*self.atomic_ptr::<AtomicU8>(offset) }
                    .fetch_sub(operand as u8, ordering)
                    as u64,
                (1, AtomicOp::And) => unsafe { &*self.atomic_ptr::<AtomicU8>(offset) }
                    .fetch_and(operand as u8, ordering)
                    as u64,
                (1, AtomicOp::Or) => unsafe { &*self.atomic_ptr::<AtomicU8>(offset) }
                    .fetch_or(operand as u8, ordering) as u64,
                (1, AtomicOp::Xor) => unsafe { &*self.atomic_ptr::<AtomicU8>(offset) }
                    .fetch_xor(operand as u8, ordering)
                    as u64,
                (1, AtomicOp::Exchange) => unsafe { &*self.atomic_ptr::<AtomicU8>(offset) }
                    .swap(operand as u8, ordering)
                    as u64,
                (1, AtomicOp::CompareExchange) => {
                    let expected = expected.unwrap_or(0) as u8;
                    let result = unsafe { &*self.atomic_ptr::<AtomicU8>(offset) }.compare_exchange(
                        expected,
                        operand as u8,
                        ordering,
                        ordering,
                    );
                    // Both arms carry the previous value.
                    match result {
                        Ok(previous) | Err(previous) => previous as u64,
                    }
                }
                (2, AtomicOp::Add) => unsafe { &*self.atomic_ptr::<AtomicU16>(offset) }
                    .fetch_add(operand as u16, ordering)
                    as u64,
                (2, AtomicOp::Sub) => unsafe { &*self.atomic_ptr::<AtomicU16>(offset) }
                    .fetch_sub(operand as u16, ordering)
                    as u64,
                (2, AtomicOp::And) => unsafe { &*self.atomic_ptr::<AtomicU16>(offset) }
                    .fetch_and(operand as u16, ordering)
                    as u64,
                (2, AtomicOp::Or) => unsafe { &*self.atomic_ptr::<AtomicU16>(offset) }
                    .fetch_or(operand as u16, ordering) as u64,
                (2, AtomicOp::Xor) => unsafe { &*self.atomic_ptr::<AtomicU16>(offset) }
                    .fetch_xor(operand as u16, ordering)
                    as u64,
                (2, AtomicOp::Exchange) => unsafe { &*self.atomic_ptr::<AtomicU16>(offset) }
                    .swap(operand as u16, ordering)
                    as u64,
                (2, AtomicOp::CompareExchange) => {
                    let expected = expected.unwrap_or(0) as u16;
                    let result = unsafe { &*self.atomic_ptr::<AtomicU16>(offset) }
                        .compare_exchange(expected, operand as u16, ordering, ordering);
                    // Both arms carry the previous value.
                    match result {
                        Ok(previous) | Err(previous) => previous as u64,
                    }
                }
                (4, AtomicOp::Add) => unsafe { &*self.atomic_ptr::<AtomicU32>(offset) }
                    .fetch_add(operand as u32, ordering)
                    as u64,
                (4, AtomicOp::Sub) => unsafe { &*self.atomic_ptr::<AtomicU32>(offset) }
                    .fetch_sub(operand as u32, ordering)
                    as u64,
                (4, AtomicOp::And) => unsafe { &*self.atomic_ptr::<AtomicU32>(offset) }
                    .fetch_and(operand as u32, ordering)
                    as u64,
                (4, AtomicOp::Or) => unsafe { &*self.atomic_ptr::<AtomicU32>(offset) }
                    .fetch_or(operand as u32, ordering) as u64,
                (4, AtomicOp::Xor) => unsafe { &*self.atomic_ptr::<AtomicU32>(offset) }
                    .fetch_xor(operand as u32, ordering)
                    as u64,
                (4, AtomicOp::Exchange) => unsafe { &*self.atomic_ptr::<AtomicU32>(offset) }
                    .swap(operand as u32, ordering)
                    as u64,
                (4, AtomicOp::CompareExchange) => {
                    let expected = expected.unwrap_or(0) as u32;
                    let result = unsafe { &*self.atomic_ptr::<AtomicU32>(offset) }
                        .compare_exchange(expected, operand as u32, ordering, ordering);
                    // Both arms carry the previous value.
                    match result {
                        Ok(previous) | Err(previous) => previous as u64,
                    }
                }
                (8, AtomicOp::Add) => {
                    unsafe { &*self.atomic_ptr::<AtomicU64>(offset) }.fetch_add(operand, ordering)
                }
                (8, AtomicOp::Sub) => {
                    unsafe { &*self.atomic_ptr::<AtomicU64>(offset) }.fetch_sub(operand, ordering)
                }
                (8, AtomicOp::And) => {
                    unsafe { &*self.atomic_ptr::<AtomicU64>(offset) }.fetch_and(operand, ordering)
                }
                (8, AtomicOp::Or) => {
                    unsafe { &*self.atomic_ptr::<AtomicU64>(offset) }.fetch_or(operand, ordering)
                }
                (8, AtomicOp::Xor) => {
                    unsafe { &*self.atomic_ptr::<AtomicU64>(offset) }.fetch_xor(operand, ordering)
                }
                (8, AtomicOp::Exchange) => {
                    unsafe { &*self.atomic_ptr::<AtomicU64>(offset) }.swap(operand, ordering)
                }
                (8, AtomicOp::CompareExchange) => {
                    let expected = expected.unwrap_or(0);
                    let result = unsafe { &*self.atomic_ptr::<AtomicU64>(offset) }
                        .compare_exchange(expected, operand, ordering, ordering);
                    // Both arms carry the previous value.
                    match result {
                        Ok(previous) | Err(previous) => previous,
                    }
                }
                _ => return Err(out_of_bounds()),
            })
        }
        #[cfg(not(feature = "workers"))]
        {
            let mut data = self.block.borrow_mut();
            let Some(slot) = data.get_mut(offset..offset + size) else {
                return Err(out_of_bounds());
            };
            let old = raw_from_bytes(slot)?;
            let next = applied_op(op, old, operand, expected);
            if next != old {
                slot.copy_from_slice(&bytes_from_raw(next, size)?);
            }
            Ok(old)
        }
    }

    #[cfg(feature = "workers")]
    fn atomic_ptr<T>(&self, offset: usize) -> *mut T {
        let base = self.block.as_ptr() as *const AtomicU64 as *mut u8;
        let ptr = unsafe { base.add(offset) } as *mut T;
        debug_assert_eq!(ptr as usize % std::mem::align_of::<T>(), 0);
        ptr
    }
}
/// The native-order integer the first `size` bytes encode.
fn raw_from_bytes(bytes: &[u8]) -> Result<u64, OutOfBounds> {
    match bytes.len() {
        1 => Ok(bytes[0] as u64),
        2 => Ok(u16::from_ne_bytes([bytes[0], bytes[1]]) as u64),
        4 => Ok(u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64),
        8 => Ok(u64::from_ne_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])),
        _ => Err(out_of_bounds()),
    }
}

/// The first `size` bytes of the native-order integer `raw`.
fn bytes_from_raw(raw: u64, size: usize) -> Result<Vec<u8>, OutOfBounds> {
    let all = raw.to_ne_bytes();
    match size {
        1 | 2 | 4 | 8 => Ok(all[..size].to_vec()),
        _ => Err(out_of_bounds()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A resizable block reserves its maximum but commits only what it holds
    /// (M-3): `new ArrayBuffer(0, { maxByteLength: 2**30 })` used to commit a
    /// gigabyte for an empty buffer. Growing commits pages; shrinking never
    /// gives them back; the base never moves.
    #[cfg(feature = "workers")]
    #[test]
    fn a_reserved_block_commits_only_what_it_uses() {
        const MAX: usize = 1 << 30;
        let mut block = SharedBuffer::new_with_capacity(0, MAX);
        assert_eq!(block.committed(), 0, "an empty buffer commits no pages");
        let base = block.data_ptr();

        block.resize(64).expect("grow");
        assert!(
            block.committed() >= 64,
            "growing commits the pages it needs"
        );
        assert!(
            block.committed() < MAX,
            "growing to 64 bytes must not commit the whole maximum"
        );
        assert_eq!(block.data_ptr(), base, "the base never moves");

        // The count above is the block's own bookkeeping; ask the operating
        // system too, where it can be asked. A page far inside the reservation
        // but past the buffer's length must still be reserved, not backed.
        let past = probe::is_committed(unsafe { base.add(1 << 20) });
        let inside = probe::is_committed(base);
        if cfg!(windows) {
            // The probe must answer here, or the assertions below would pass
            // without having looked at anything.
            assert_eq!(
                past,
                Some(false),
                "a page past the buffer's length is reserved, not backed"
            );
            assert_eq!(
                inside,
                Some(true),
                "a page inside the buffer's length is backed"
            );
        }
        if let Some(before) = probe::resident_bytes() {
            let big = SharedBuffer::new_with_capacity(0, MAX);
            if let Some(after) = probe::resident_bytes() {
                assert!(
                    after.saturating_sub(before) < MAX / 4,
                    "reserving {MAX} bytes grew the resident set by {} bytes",
                    after.saturating_sub(before)
                );
            }
            drop(big);
        }
        block.write(0, &[1, 2, 3]).expect("write");
        assert_eq!(block.read(0, 3).unwrap(), vec![1, 2, 3]);

        block.resize(0).expect("shrink");
        assert_eq!(block.byte_length(), 0);
        block.resize(4096).expect("grow again");
        assert_eq!(
            block.read(0, 4096).expect("read the regrown range"),
            vec![0u8; 4096],
            "a regrown range reads as zeroes"
        );
        assert_eq!(block.data_ptr(), base);
    }

    /// A borrowed block *is* the host's bytes: a write through it is a write to
    /// the host's allocation and the other way round, which is the whole reason
    /// for the constructor. The state box points at the host's memory too, which
    /// is what the JIT's inline element store reads.
    #[test]
    fn a_borrowed_block_is_the_hosts_bytes() {
        let mut host = vec![1u8, 2, 3, 4];
        // SAFETY: the host's `Vec` outlives the block, which is the contract.
        let mut block = unsafe { SharedBuffer::borrowed(host.as_mut_ptr(), host.len(), None) };

        assert!(block.is_borrowed());
        assert_eq!(block.byte_length(), 4);
        assert_eq!(block.data_ptr(), host.as_mut_ptr());
        assert_eq!(block.read(0, 2).unwrap(), vec![1, 2]);

        block.write(1, &[9, 9]).unwrap();
        assert_eq!(
            host,
            vec![1, 9, 9, 4],
            "the write landed in the host's bytes"
        );
        host[3] = 8;
        assert_eq!(
            block.read(3, 1).unwrap(),
            vec![8],
            "and the host's write is read back"
        );

        // Bounds are the block's, and a refusal leaves the host's memory alone.
        assert!(block.read(3, 2).is_err());
        assert!(block.write(3, &[0, 0]).is_err());
        assert_eq!(host, vec![1, 9, 9, 8]);

        // A clone is the same memory, and the single-agent atomics go through it.
        let clone = block.clone();
        assert_eq!(
            clone.block_id(),
            block.block_id(),
            "one allocation, one identity"
        );
        block.atomic_store(0, 1, 5).unwrap();
        assert_eq!(block.atomic_load(0, 1).unwrap(), 5);
        assert_eq!(block.atomic_rmw(AtomicOp::Add, 0, 1, 2, None).unwrap(), 5);
        assert_eq!(host[0], 7);

        // Growing is refused: the allocation is the host's.
        assert!(block.resize(8).is_err());
        assert_eq!(block.byte_length(), 4);
    }

    /// The deleter is the host's way of getting its memory back, and it runs when
    /// the last clone of the block goes — not before, and exactly once. The counter
    /// is a shared atomic rather than an `Rc` because under `workers` a block is
    /// `Send` and its deleter may run on another thread; a test whose deleter
    /// contradicted that would be lying about the type it builds.
    #[test]
    fn a_wrapping_range_is_out_of_bounds() {
        let block = SharedBuffer::new(8);
        assert!(block.read(usize::MAX, 1).is_err());
        assert!(block.write(usize::MAX, &[1]).is_err());
        assert!(block.read_into(usize::MAX, &mut [0u8; 1]).is_err());
    }

    #[test]
    fn a_borrowed_block_releases_its_memory_once() {
        let mut host = vec![0u8; 2];
        let freed = std::sync::Arc::new(AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&freed);
        // SAFETY: as above; the deleter is the only thing that touches `host`'s
        // allocation, and it runs after every clone is gone.
        let block = unsafe {
            SharedBuffer::borrowed(
                host.as_mut_ptr(),
                host.len(),
                Some(Box::new(move || {
                    counted.fetch_add(1, Ordering::SeqCst);
                })),
            )
        };
        let clone = block.clone();

        drop(block);
        assert_eq!(
            freed.load(Ordering::SeqCst),
            0,
            "a live clone keeps the host's memory"
        );
        drop(clone);
        assert_eq!(
            freed.load(Ordering::SeqCst),
            1,
            "the last clone releases it, once"
        );
    }
}
