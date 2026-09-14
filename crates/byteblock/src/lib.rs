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

/// The shared per-buffer geometry every view reads live (all clones of a
/// [`SharedBuffer`] reference one box): the byte base and the writable
/// flags. One box keeps the JIT's inline element store (gap-close M5c)
/// sound — it re-reads the base and flags via `offset_of!` through
/// [`SharedBuffer::state`] (the raw box address) on every store, so a
/// helper that detaches, freezes, or resizes the buffer mid-run is picked
/// up by the next store. The fields are `pub` (cross-crate `offset_of!`
/// needs pub fields) and exist in both cfg builds with identical names —
/// `Cell` on the single-agent path, atomics under `workers` (the box can
/// be shared across agent threads there; the JIT inline is never emitted
/// in that build).
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
    /// Whether the owning ArrayBuffer has been detached (spec 25.1.2.5).
    /// The runtime's `BufferState.detached` is authoritative; this flag
    /// mirrors it so crux's integer-indexed access can reject detached
    /// views without reaching the agent.
    #[cfg(not(feature = "workers"))]
    pub detached: Cell<bool>,
    #[cfg(feature = "workers")]
    pub detached: std::sync::atomic::AtomicBool,
    /// Whether the owning ArrayBuffer is immutable (ES2026
    /// `transferToImmutable`): writes through views throw a TypeError. The
    /// runtime's `BufferState.immutable` is authoritative; this flag mirrors
    /// it for crux's integer-indexed writes.
    #[cfg(not(feature = "workers"))]
    pub immutable: Cell<bool>,
    #[cfg(feature = "workers")]
    pub immutable: std::sync::atomic::AtomicBool,
    /// Whether the owning ArrayBuffer is resizable (spec 25.1.2.4: a
    /// `maxByteLength` was supplied). Mirrored from the runtime's
    /// `BufferState.resizable` so crux's TypedArray [[PreventExtensions]] can
    /// reject views that could gain or lose integer-indexed properties when
    /// the buffer is resized (spec 10.4.5.1) — and so the JIT inline can
    /// reject views whose storage can move.
    #[cfg(not(feature = "workers"))]
    pub resizable: Cell<bool>,
    #[cfg(feature = "workers")]
    pub resizable: std::sync::atomic::AtomicBool,
    /// Whether the owning buffer is a SharedArrayBuffer (spec 25.1.3.4).
    /// Mirrored from `BufferState.is_shared`; a shared buffer's views are
    /// fixed-length for [[PreventExtensions]] purposes.
    #[cfg(not(feature = "workers"))]
    pub is_shared: Cell<bool>,
    #[cfg(feature = "workers")]
    pub is_shared: std::sync::atomic::AtomicBool,
}

impl BlockState {
    fn new(data: usize) -> Self {
        #[cfg(not(feature = "workers"))]
        {
            Self {
                data: Cell::new(data),
                detached: Cell::new(false),
                immutable: Cell::new(false),
                resizable: Cell::new(false),
                is_shared: Cell::new(false),
            }
        }
        #[cfg(feature = "workers")]
        {
            Self {
                data: std::sync::atomic::AtomicUsize::new(data),
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
    block: std::sync::Arc<[std::sync::atomic::AtomicU64]>,
    #[cfg(feature = "workers")]
    byte_length: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// The shared geometry box (see [`BlockState`]); every clone shares it
    /// (`Rc` single-agent, `Arc` under `workers`).
    state_rc: StateRc,
    /// The address of the `state_rc` box's value — the JIT's inline element
    /// store dereferences it (`offset_of!(SharedBuffer, state)` + the
    /// `BlockState` field offsets) to read the live data base and flags.
    /// The `Rc`/`Arc` box layout is not `offset_of!`-expressible across
    /// crates, so the value address is stored raw. Stable for the box's
    /// lifetime; `state_rc` keeps the box alive for every clone.
    pub state: usize,
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
    /// Resizable/growable buffers pre-allocate their maximum so views created
    /// before a resize keep a live block — under `workers` the block is a
    /// fixed `Arc`, so a resize only updates the shared byte length in place
    /// (the single-agent path resizes its `Vec` in place and shares it).
    pub fn new_with_capacity(byte_length: usize, capacity: usize) -> Self {
        #[cfg(not(feature = "workers"))]
        {
            let _ = capacity;
            let block = Rc::new(RefCell::new(vec![0u8; byte_length]));
            let state_rc = Rc::new(BlockState::new(block.borrow().as_ptr() as usize));
            let state = &*state_rc as *const BlockState as usize;
            SharedBuffer {
                block,
                state_rc,
                state,
            }
        }
        #[cfg(feature = "workers")]
        {
            let words = byte_length.max(capacity).div_ceil(8);
            let block = (0..words)
                .map(|_| std::sync::atomic::AtomicU64::new(0))
                .collect::<std::sync::Arc<[_]>>();
            let state_rc = std::sync::Arc::new(BlockState::new(block.as_ptr()
                as *const std::sync::atomic::AtomicU64
                as *const u8
                as usize));
            let state = &*state_rc as *const BlockState as usize;
            SharedBuffer {
                block,
                byte_length: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(byte_length)),
                state_rc,
                state,
            }
        }
    }

    /// Mark the owning buffer detached (mirrors the runtime's `BufferState`).
    pub fn mark_detached(&self) {
        #[cfg(not(feature = "workers"))]
        {
            self.state_rc.detached.set(true);
        }
        #[cfg(feature = "workers")]
        {
            self.state_rc
                .detached
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Whether the owning buffer has been detached.
    pub fn is_detached(&self) -> bool {
        #[cfg(not(feature = "workers"))]
        {
            self.state_rc.detached.get()
        }
        #[cfg(feature = "workers")]
        {
            self.state_rc
                .detached
                .load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Mark the owning buffer immutable (ES2026 transferToImmutable).
    pub fn mark_immutable(&self) {
        #[cfg(not(feature = "workers"))]
        {
            self.state_rc.immutable.set(true);
        }
        #[cfg(feature = "workers")]
        {
            self.state_rc
                .immutable
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Whether the owning buffer is immutable.
    pub fn is_immutable(&self) -> bool {
        #[cfg(not(feature = "workers"))]
        {
            self.state_rc.immutable.get()
        }
        #[cfg(feature = "workers")]
        {
            self.state_rc
                .immutable
                .load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Mark the owning buffer resizable (mirrors `BufferState.resizable`).
    pub fn mark_resizable(&self) {
        #[cfg(not(feature = "workers"))]
        {
            self.state_rc.resizable.set(true);
        }
        #[cfg(feature = "workers")]
        {
            self.state_rc
                .resizable
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Whether the owning buffer is a resizable ArrayBuffer.
    pub fn is_resizable(&self) -> bool {
        #[cfg(not(feature = "workers"))]
        {
            self.state_rc.resizable.get()
        }
        #[cfg(feature = "workers")]
        {
            self.state_rc
                .resizable
                .load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Mark the owning buffer as a SharedArrayBuffer (mirrors
    /// `BufferState.is_shared`).
    pub fn mark_shared(&self) {
        #[cfg(not(feature = "workers"))]
        {
            self.state_rc.is_shared.set(true);
        }
        #[cfg(feature = "workers")]
        {
            self.state_rc
                .is_shared
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Whether the owning buffer is a SharedArrayBuffer.
    pub fn is_shared(&self) -> bool {
        #[cfg(not(feature = "workers"))]
        {
            self.state_rc.is_shared.get()
        }
        #[cfg(feature = "workers")]
        {
            self.state_rc
                .is_shared
                .load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    pub fn byte_length(&self) -> usize {
        #[cfg(not(feature = "workers"))]
        {
            self.block.borrow().len()
        }
        #[cfg(feature = "workers")]
        {
            self.byte_length.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    /// A stable identity for the underlying byte block (the allocation
    /// address), used to key the Atomics wait registry.
    pub fn block_id(&self) -> usize {
        #[cfg(not(feature = "workers"))]
        {
            Rc::as_ptr(&self.block) as usize
        }
        #[cfg(feature = "workers")]
        {
            self.block.as_ptr() as usize
        }
    }

    /// Copy `len` bytes out of the block at `offset` (a plain read; the
    /// caller synchronizes concurrent access).
    pub fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>, OutOfBounds> {
        #[cfg(not(feature = "workers"))]
        {
            let data = self.block.borrow();
            data.get(offset..offset + len)
                .map(<[u8]>::to_vec)
                .ok_or_else(out_of_bounds)
        }
        #[cfg(feature = "workers")]
        {
            if offset + len > self.byte_length() {
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
        #[cfg(not(feature = "workers"))]
        {
            let data = self.block.borrow();
            data.get(offset..offset + out.len())
                .map(|slice| out.copy_from_slice(slice))
                .ok_or_else(out_of_bounds)
        }
        #[cfg(feature = "workers")]
        {
            if offset + out.len() > self.byte_length() {
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
        #[cfg(not(feature = "workers"))]
        {
            let mut data = self.block.borrow_mut();
            let Some(slot) = data.get_mut(offset..offset + bytes.len()) else {
                return Err(out_of_bounds());
            };
            slot.copy_from_slice(bytes);
            Ok(())
        }
        #[cfg(feature = "workers")]
        {
            if offset + bytes.len() > self.byte_length() {
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

    /// Grow/shrink the block to `new_length`. Under `workers` the block is
    /// pre-allocated to its capacity (resizable/growable buffers), so a
    /// resize only updates the shared byte length — visible to every view
    /// clone — and zero-fills the newly exposed region on a grow. The
    /// single-agent path resizes the shared `Vec` in place.
    pub fn resize(&mut self, new_length: usize) -> Result<(), OutOfBounds> {
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
            let capacity = self.block.len().saturating_mul(8);
            if new_length > capacity {
                return Err(out_of_bounds());
            }
            let old_length = self.byte_length.load(std::sync::atomic::Ordering::Relaxed);
            if new_length > old_length {
                let base = self.block.as_ptr() as *const AtomicU64 as *mut u8;
                // SAFETY: `new_length <= capacity` and the Arc keeps the
                // block alive.
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
    /// encode, read atomically under `workers` (plain under single-agent).
    pub fn atomic_load(&self, offset: usize, size: usize) -> Result<u64, OutOfBounds> {
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
    /// bytes) at `offset`, atomically under `workers`.
    pub fn atomic_store(&self, offset: usize, size: usize, value: u64) -> Result<(), OutOfBounds> {
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
    /// `CompareExchange`. Real atomics under `workers`; a plain RMW otherwise.
    pub fn atomic_rmw(
        &self,
        op: AtomicOp,
        offset: usize,
        size: usize,
        operand: u64,
        expected: Option<u64>,
    ) -> Result<u64, OutOfBounds> {
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
            let next = match op {
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
            };
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
#[cfg(not(feature = "workers"))]
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
#[cfg(not(feature = "workers"))]
fn bytes_from_raw(raw: u64, size: usize) -> Result<Vec<u8>, OutOfBounds> {
    let all = raw.to_ne_bytes();
    match size {
        1 | 2 | 4 | 8 => Ok(all[..size].to_vec()),
        _ => Err(out_of_bounds()),
    }
}
