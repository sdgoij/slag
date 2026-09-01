//! The TypedArray element types and the byte conversions of the
//! Integer-Indexed exotic (spec 10.4.5, 25.2.1): a shared byte buffer
//! ([[ArrayBufferData]]) plus the per-element encode/decode used by
//! [[Get]]/[[Set]]/[[GetOwnProperty]]/[[DefineOwnProperty]].

#[cfg(not(feature = "workers"))]
use std::cell::{Cell, RefCell};
#[cfg(not(feature = "workers"))]
use std::rc::Rc;

use num_traits::ToPrimitive;

use crate::BigInt;
use crate::convert::{to_big_int64, to_big_uint64, to_number, to_uint8_clamp};
use crate::error::{ErrorKind, JsError};
use crate::value::Value;

/// [[ContentType]] of a TypedArray (spec 25.2.4.1): which element
/// conversions apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentType {
    Number,
    BigInt,
}

/// The element type of a TypedArray (spec 25.2.1 table): the byte size and
/// the encode/decode rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementType {
    Int8,
    Uint8,
    Uint8Clamped,
    Int16,
    Uint16,
    Int32,
    Uint32,
    Float16,
    Float32,
    Float64,
    BigInt64,
    BigUint64,
}

impl ElementType {
    /// The element size in bytes (spec 25.2.1: [[ArrayElementSize]]).
    pub fn size(self) -> usize {
        match self {
            ElementType::Int8 | ElementType::Uint8 | ElementType::Uint8Clamped => 1,
            ElementType::Int16 | ElementType::Uint16 | ElementType::Float16 => 2,
            ElementType::Int32 | ElementType::Uint32 | ElementType::Float32 => 4,
            ElementType::Float64 | ElementType::BigInt64 | ElementType::BigUint64 => 8,
        }
    }

    /// [[ContentType]] of the elements (spec 25.2.1 table).
    pub fn content_type(self) -> ContentType {
        match self {
            ElementType::BigInt64 | ElementType::BigUint64 => ContentType::BigInt,
            _ => ContentType::Number,
        }
    }

    /// The `%Int8Array%`-style name, minus the "Array" suffix: the
    /// [[TypedArrayName]] of a kind.
    pub fn name(self) -> &'static str {
        match self {
            ElementType::Int8 => "Int8",
            ElementType::Uint8 => "Uint8",
            ElementType::Uint8Clamped => "Uint8Clamped",
            ElementType::Int16 => "Int16",
            ElementType::Uint16 => "Uint16",
            ElementType::Int32 => "Int32",
            ElementType::Uint32 => "Uint32",
            ElementType::Float16 => "Float16",
            ElementType::Float32 => "Float32",
            ElementType::Float64 => "Float64",
            ElementType::BigInt64 => "BigInt64",
            ElementType::BigUint64 => "BigUint64",
        }
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
    block: std::sync::Arc<[std::sync::atomic::AtomicU64]>,
    #[cfg(feature = "workers")]
    byte_length: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Whether the owning ArrayBuffer has been detached (spec 25.1.2.5). The
    /// runtime's `BufferState.detached` is authoritative; this flag mirrors it
    /// so crux's integer-indexed access can reject detached views without
    /// reaching the agent. Views clone the same Rc/Arc, so the flag is shared.
    #[cfg(not(feature = "workers"))]
    detached: Rc<Cell<bool>>,
    #[cfg(feature = "workers")]
    detached: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Whether the owning ArrayBuffer is immutable (ES2026
    /// `transferToImmutable`): writes through views throw a TypeError. The
    /// runtime's `BufferState.immutable` is authoritative; this flag mirrors
    /// it for crux's integer-indexed writes.
    #[cfg(not(feature = "workers"))]
    immutable: Rc<Cell<bool>>,
    #[cfg(feature = "workers")]
    immutable: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Whether the owning ArrayBuffer is resizable (spec 25.1.2.4: a
    /// `maxByteLength` was supplied). Mirrored from the runtime's
    /// `BufferState.resizable` so crux's TypedArray [[PreventExtensions]] can
    /// reject views that could gain or lose integer-indexed properties when
    /// the buffer is resized (spec 10.4.5.1).
    #[cfg(not(feature = "workers"))]
    resizable: Rc<Cell<bool>>,
    #[cfg(feature = "workers")]
    resizable: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Whether the owning buffer is a SharedArrayBuffer (spec 25.1.3.4).
    /// Mirrored from `BufferState.is_shared`; a shared buffer's views are
    /// fixed-length for [[PreventExtensions]] purposes.
    #[cfg(not(feature = "workers"))]
    is_shared: Rc<Cell<bool>>,
    #[cfg(feature = "workers")]
    is_shared: std::sync::Arc<std::sync::atomic::AtomicBool>,
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

fn out_of_bounds() -> JsError {
    JsError::new(ErrorKind::TypeError, "buffer access out of bounds".into())
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
            SharedBuffer {
                block: Rc::new(RefCell::new(vec![0u8; byte_length])),
                detached: Rc::new(Cell::new(false)),
                immutable: Rc::new(Cell::new(false)),
                resizable: Rc::new(Cell::new(false)),
                is_shared: Rc::new(Cell::new(false)),
            }
        }
        #[cfg(feature = "workers")]
        {
            let words = byte_length.max(capacity).div_ceil(8);
            SharedBuffer {
                block: (0..words)
                    .map(|_| std::sync::atomic::AtomicU64::new(0))
                    .collect::<std::sync::Arc<[_]>>(),
                byte_length: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(byte_length)),
                detached: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                immutable: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                resizable: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                is_shared: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }
        }
    }

    /// Mark the owning buffer detached (mirrors the runtime's `BufferState`).
    pub fn mark_detached(&self) {
        #[cfg(not(feature = "workers"))]
        {
            self.detached.set(true);
        }
        #[cfg(feature = "workers")]
        {
            self.detached
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Whether the owning buffer has been detached.
    pub fn is_detached(&self) -> bool {
        #[cfg(not(feature = "workers"))]
        {
            self.detached.get()
        }
        #[cfg(feature = "workers")]
        {
            self.detached.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Mark the owning buffer immutable (ES2026 transferToImmutable).
    pub fn mark_immutable(&self) {
        #[cfg(not(feature = "workers"))]
        {
            self.immutable.set(true);
        }
        #[cfg(feature = "workers")]
        {
            self.immutable
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Whether the owning buffer is immutable.
    pub fn is_immutable(&self) -> bool {
        #[cfg(not(feature = "workers"))]
        {
            self.immutable.get()
        }
        #[cfg(feature = "workers")]
        {
            self.immutable.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Mark the owning buffer resizable (mirrors `BufferState.resizable`).
    pub fn mark_resizable(&self) {
        #[cfg(not(feature = "workers"))]
        {
            self.resizable.set(true);
        }
        #[cfg(feature = "workers")]
        {
            self.resizable
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Whether the owning buffer is a resizable ArrayBuffer.
    pub fn is_resizable(&self) -> bool {
        #[cfg(not(feature = "workers"))]
        {
            self.resizable.get()
        }
        #[cfg(feature = "workers")]
        {
            self.resizable.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Mark the owning buffer as a SharedArrayBuffer (mirrors
    /// `BufferState.is_shared`).
    pub fn mark_shared(&self) {
        #[cfg(not(feature = "workers"))]
        {
            self.is_shared.set(true);
        }
        #[cfg(feature = "workers")]
        {
            self.is_shared
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Whether the owning buffer is a SharedArrayBuffer.
    pub fn is_shared(&self) -> bool {
        #[cfg(not(feature = "workers"))]
        {
            self.is_shared.get()
        }
        #[cfg(feature = "workers")]
        {
            self.is_shared.load(std::sync::atomic::Ordering::SeqCst)
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
    pub fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>, JsError> {
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

    /// Copy `bytes` into the block at `offset` (a plain write; the caller
    /// synchronizes concurrent access).
    pub fn write(&self, offset: usize, bytes: &[u8]) -> Result<(), JsError> {
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
    pub fn resize(&mut self, new_length: usize) -> Result<(), JsError> {
        #[cfg(not(feature = "workers"))]
        {
            self.block.borrow_mut().resize(new_length, 0);
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
    pub fn atomic_load(&self, offset: usize, size: usize) -> Result<u64, JsError> {
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
    pub fn atomic_store(&self, offset: usize, size: usize, value: u64) -> Result<(), JsError> {
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
    ) -> Result<u64, JsError> {
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

#[cfg(feature = "workers")]
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64};

/// The native-order integer the first `size` bytes encode.
#[cfg(not(feature = "workers"))]
fn raw_from_bytes(bytes: &[u8]) -> Result<u64, JsError> {
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
fn bytes_from_raw(raw: u64, size: usize) -> Result<Vec<u8>, JsError> {
    let all = raw.to_ne_bytes();
    match size {
        1 | 2 | 4 | 8 => Ok(all[..size].to_vec()),
        _ => Err(out_of_bounds()),
    }
}

/// ToInt8/ToUint8/ToInt16/... (spec 7.1.9-7.1.13 and the 2^k variants): the
/// truncated Number wrapped into the signed/unsigned element width. NaN and
/// infinities map to 0.
fn wrap_signed(number: f64, bits: u32) -> i64 {
    if number.is_nan() || number.is_infinite() {
        return 0;
    }
    let modulus = 1u64 << bits;
    let wrapped = number.trunc().rem_euclid(modulus as f64) as u64;
    let half = modulus >> 1;
    if wrapped >= half {
        (wrapped as i64) - (modulus as i64)
    } else {
        wrapped as i64
    }
}

/// The IEEE 754 binary16 bit pattern nearest to `x` (round-half-to-even),
/// used by the Float16 element conversion and `Math.f16round` (spec
/// 25.2.4.2 / 21.3.2.15). Rounds directly from the full 53-bit f64
/// mantissa: an intermediate binary32 step (the `half` crate's x86 F16C
/// path) or a premature 11-bit rounding would lose the sticky bits that
/// decide the subnormal boundary (e.g. the f64 one ULP above 2^-25 must
/// round up to the smallest subnormal, not to 0).
pub fn f16_from_f64(x: f64) -> u16 {
    if x.is_nan() {
        return 0x7E00;
    }
    if x == 0.0 {
        return if x.is_sign_negative() { 0x8000 } else { 0 };
    }
    if x.is_infinite() {
        return if x.is_sign_negative() { 0xFC00 } else { 0x7C00 };
    }
    let bits = x.to_bits();
    let sign = ((bits >> 63) as u16) << 15;
    let biased = ((bits >> 52) & 0x7FF) as i32;
    let fraction = bits & 0xF_FFFF_FFFF_FFFF;
    let (mantissa, exponent) = if biased == 0 {
        // Subnormal f64 input: value = fraction × 2^-1074; normalize so the
        // leading bit sits at 2^52.
        let shift = fraction.leading_zeros() as i32 - 11;
        (fraction << shift, -1074 - shift)
    } else {
        (fraction | (1 << 52), biased - 1075)
    };
    // value = mantissa × 2^exponent, mantissa ∈ [2^52, 2^53), so the
    // unbiased exponent of the value is exponent + 52. The normal/subnormal
    // decision must use the full precision: the smallest normal f16 is 2^-14.
    if exponent + 52 >= -14 {
        // Normal f16: round the 53-bit mantissa to 11 bits (drop 42),
        // ties-to-even.
        let dropped = 42;
        let half = (mantissa >> (dropped - 1)) & 1;
        let sticky = mantissa & ((1u64 << (dropped - 1)) - 1) != 0;
        let mut mantissa = mantissa >> dropped;
        let mut exponent = exponent + dropped;
        if half == 1 && (sticky || mantissa & 1 == 1) {
            mantissa += 1;
            if mantissa == 1 << 11 {
                mantissa >>= 1;
                exponent += 1;
            }
        }
        // The 11-bit mantissa's leading bit sits at 2^10, so the f16 biased
        // exponent is (exponent + 10) + 15.
        let biased = exponent + 25;
        if biased >= 31 {
            return sign | 0x7C00;
        }
        return sign | ((biased as u16) << 10) | (mantissa as u16 & 0x3FF);
    }
    // Subnormal f16: value = mantissa × 2^exponent in units of the smallest
    // subnormal 2^-24: significand = mantissa × 2^(exponent + 24), rounded
    // to the nearest integer (10 bits, ties-to-even).
    let shift = exponent + 24;
    let right = -shift;
    if right >= 54 {
        // mantissa < 2^53, so the significand fraction is < 2^-1 → 0.
        return sign;
    }
    let lost = mantissa & ((1u64 << right) - 1);
    let rounded = (mantissa >> right)
        + if lost > (1u64 << (right - 1))
            || (lost == 1u64 << (right - 1) && (mantissa >> right) & 1 == 1)
        {
            1
        } else {
            0
        };
    if rounded == 0 {
        sign
    } else if rounded >= 1 << 10 {
        // Rounded up to the smallest normal 2^-14.
        sign | (1 << 10)
    } else {
        sign | rounded as u16
    }
}

/// The largest element size (Float64 / BigInt64 / BigUint64).
pub const MAX_ELEMENT_SIZE: usize = 8;

/// Convert a Number value into the element bytes of `element_type`
/// (spec SetValueInBuffer with ToNumber + the element conversion, 25.2.4.2),
/// writing them into `out[..size]` and returning `size`. No allocation — the
/// per-element write paths (`typed_array_element_set`, the JIT store helper)
/// previously paid a fresh `Vec<u8>` per element.
fn encode_number_into(
    element_type: ElementType,
    number: f64,
    out: &mut [u8; MAX_ELEMENT_SIZE],
) -> Result<usize, JsError> {
    match element_type {
        ElementType::Int8 => {
            out[0] = wrap_signed(number, 8) as i8 as u8;
            Ok(1)
        }
        ElementType::Uint8 => {
            out[0] = wrap_signed(number, 8) as u8;
            Ok(1)
        }
        ElementType::Uint8Clamped => {
            out[0] = to_uint8_clamp(number);
            Ok(1)
        }
        ElementType::Int16 => {
            out[..2].copy_from_slice(&(wrap_signed(number, 16) as i16).to_ne_bytes());
            Ok(2)
        }
        ElementType::Uint16 => {
            out[..2].copy_from_slice(&(wrap_signed(number, 16) as u16).to_ne_bytes());
            Ok(2)
        }
        ElementType::Int32 => {
            out[..4].copy_from_slice(&(wrap_signed(number, 32) as i32).to_ne_bytes());
            Ok(4)
        }
        ElementType::Uint32 => {
            out[..4].copy_from_slice(&(wrap_signed(number, 32) as u32).to_ne_bytes());
            Ok(4)
        }
        ElementType::Float16 => {
            out[..2].copy_from_slice(&f16_from_f64(number).to_ne_bytes());
            Ok(2)
        }
        ElementType::Float32 => {
            out[..4].copy_from_slice(&(number as f32).to_ne_bytes());
            Ok(4)
        }
        ElementType::Float64 => {
            out[..8].copy_from_slice(&number.to_ne_bytes());
            Ok(8)
        }
        ElementType::BigInt64 | ElementType::BigUint64 => Err(JsError::new(
            ErrorKind::TypeError,
            "BigInt element type requires a BigInt value".into(),
        )),
    }
}

/// Convert a BigInt value into the element bytes of a BigInt64/BigUint64
/// element (spec ToBigInt64/ToBigUint64, 25.2.4.3), writing them into
/// `out[..8]` and returning 8.
fn encode_bigint_into(
    element_type: ElementType,
    bigint: &BigInt,
    out: &mut [u8; MAX_ELEMENT_SIZE],
) -> Result<usize, JsError> {
    match element_type {
        ElementType::BigInt64 => {
            out.copy_from_slice(&bigint.0.to_i64().unwrap_or(0).to_ne_bytes());
            Ok(8)
        }
        ElementType::BigUint64 => {
            out.copy_from_slice(&bigint.0.to_u64().unwrap_or(0).to_ne_bytes());
            Ok(8)
        }
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "Number element type requires a Number value".into(),
        )),
    }
}

/// The bytes a language value encodes to for `element_type`: a BigInt
/// content type coerces with ToBigInt*, anything else with ToNumber.
/// spec 25.2.4.2 (TypedArray [[Set]] element conversion). Writes into
/// `out[..size]` and returns `size` — the allocation-free form every
/// per-element write path uses.
pub fn encode_element_into(
    element_type: ElementType,
    value: &Value,
    out: &mut [u8; MAX_ELEMENT_SIZE],
) -> Result<usize, JsError> {
    if matches!(element_type, ElementType::BigInt64 | ElementType::BigUint64) {
        let bigint = match element_type {
            ElementType::BigInt64 => to_big_int64(value)?,
            _ => to_big_uint64(value)?,
        };
        encode_bigint_into(element_type, &bigint, out)
    } else {
        encode_number_into(element_type, to_number(value)?, out)
    }
}

/// The allocated form of [`encode_element_into`] — kept for the callers
/// that need an owned buffer (DataView's endianness swap, `fill`'s
/// encode-once) rather than a stack slice.
pub fn encode_element(element_type: ElementType, value: &Value) -> Result<Vec<u8>, JsError> {
    let mut out = [0u8; MAX_ELEMENT_SIZE];
    let size = encode_element_into(element_type, value, &mut out)?;
    Ok(out[..size].to_vec())
}

/// The language value stored in the element bytes at `offset` (spec
/// GetValueFromBuffer with the element conversion, 25.2.4.1).
pub fn decode_element(
    element_type: ElementType,
    buffer: &[u8],
    offset: usize,
) -> Result<Value, JsError> {
    let size = element_type.size();
    let value = match element_type {
        ElementType::Int8 => {
            let bytes: [u8; 1] = buffer[offset..offset + size].try_into().map_err(|_| {
                JsError::new(
                    ErrorKind::TypeError,
                    "TypedArray element read out of bounds".into(),
                )
            })?;
            Value::Number(i8::from_ne_bytes(bytes) as f64)
        }
        ElementType::Uint8 | ElementType::Uint8Clamped => {
            let bytes: [u8; 1] = buffer[offset..offset + size].try_into().map_err(|_| {
                JsError::new(
                    ErrorKind::TypeError,
                    "TypedArray element read out of bounds".into(),
                )
            })?;
            Value::Number(bytes[0] as f64)
        }
        ElementType::Int16 => {
            let bytes: [u8; 2] = buffer[offset..offset + size].try_into().map_err(|_| {
                JsError::new(
                    ErrorKind::TypeError,
                    "TypedArray element read out of bounds".into(),
                )
            })?;
            Value::Number(i16::from_ne_bytes(bytes) as f64)
        }
        ElementType::Uint16 => {
            let bytes: [u8; 2] = buffer[offset..offset + size].try_into().map_err(|_| {
                JsError::new(
                    ErrorKind::TypeError,
                    "TypedArray element read out of bounds".into(),
                )
            })?;
            Value::Number(u16::from_ne_bytes(bytes) as f64)
        }
        ElementType::Int32 => {
            let bytes: [u8; 4] = buffer[offset..offset + size].try_into().map_err(|_| {
                JsError::new(
                    ErrorKind::TypeError,
                    "TypedArray element read out of bounds".into(),
                )
            })?;
            Value::Number(i32::from_ne_bytes(bytes) as f64)
        }
        ElementType::Uint32 => {
            let bytes: [u8; 4] = buffer[offset..offset + size].try_into().map_err(|_| {
                JsError::new(
                    ErrorKind::TypeError,
                    "TypedArray element read out of bounds".into(),
                )
            })?;
            Value::Number(u32::from_ne_bytes(bytes) as f64)
        }
        ElementType::Float16 => {
            let bytes: [u8; 2] = buffer[offset..offset + size].try_into().map_err(|_| {
                JsError::new(
                    ErrorKind::TypeError,
                    "TypedArray element read out of bounds".into(),
                )
            })?;
            Value::Number(half::f16::from_bits(u16::from_ne_bytes(bytes)).to_f64())
        }
        ElementType::Float32 => {
            let bytes: [u8; 4] = buffer[offset..offset + size].try_into().map_err(|_| {
                JsError::new(
                    ErrorKind::TypeError,
                    "TypedArray element read out of bounds".into(),
                )
            })?;
            Value::Number(f32::from_ne_bytes(bytes) as f64)
        }
        ElementType::Float64 => {
            let bytes: [u8; 8] = buffer[offset..offset + size].try_into().map_err(|_| {
                JsError::new(
                    ErrorKind::TypeError,
                    "TypedArray element read out of bounds".into(),
                )
            })?;
            Value::Number(f64::from_ne_bytes(bytes))
        }
        ElementType::BigInt64 => {
            let bytes: [u8; 8] = buffer[offset..offset + size].try_into().map_err(|_| {
                JsError::new(
                    ErrorKind::TypeError,
                    "TypedArray element read out of bounds".into(),
                )
            })?;
            Value::BigInt(crate::handle::Handle::new(BigInt::from(
                i64::from_ne_bytes(bytes),
            )))
        }
        ElementType::BigUint64 => {
            let bytes: [u8; 8] = buffer[offset..offset + size].try_into().map_err(|_| {
                JsError::new(
                    ErrorKind::TypeError,
                    "TypedArray element read out of bounds".into(),
                )
            })?;
            Value::BigInt(crate::handle::Handle::new(BigInt::from(
                u64::from_ne_bytes(bytes),
            )))
        }
    };
    Ok(value)
}
