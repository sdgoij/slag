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
        #[cfg(not(feature = "workers"))]
        {
            SharedBuffer {
                block: Rc::new(RefCell::new(vec![0u8; byte_length])),
                detached: Rc::new(Cell::new(false)),
            }
        }
        #[cfg(feature = "workers")]
        {
            let words = byte_length.div_ceil(8);
            SharedBuffer {
                block: (0..words)
                    .map(|_| std::sync::atomic::AtomicU64::new(0))
                    .collect::<std::sync::Arc<[_]>>(),
                byte_length: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(byte_length)),
                detached: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
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

    /// Grow/shrink the block to `new_length` (single-agent buffers; growable
    /// SharedArrayBuffers are not shared while growing). Views created before
    /// the resize keep the old block and their captured lengths.
    pub fn resize(&mut self, new_length: usize) -> Result<(), JsError> {
        #[cfg(not(feature = "workers"))]
        {
            self.block.borrow_mut().resize(new_length, 0);
            Ok(())
        }
        #[cfg(feature = "workers")]
        {
            let old = self.read(0, self.byte_length())?;
            let words = new_length.div_ceil(8);
            let new_block = (0..words)
                .map(|_| AtomicU64::new(0))
                .collect::<std::sync::Arc<[_]>>();
            let base = new_block.as_ptr() as *const AtomicU64 as *const u8;
            // SAFETY: `new_block` is unique (freshly built), so writing
            // through its data pointer is sound.
            unsafe {
                std::ptr::copy_nonoverlapping(old.as_ptr(), base as *mut u8, old.len());
            }
            // Replace both the block and its length: views created before the
            // resize keep the old (block, length) pair.
            self.block = new_block;
            self.byte_length = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(new_length));
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

/// Convert a Number value into the element bytes of `element_type`
/// (spec SetValueInBuffer with ToNumber + the element conversion, 25.2.4.2).
fn encode_number(element_type: ElementType, number: f64) -> Result<Vec<u8>, JsError> {
    let bytes = match element_type {
        ElementType::Int8 => (wrap_signed(number, 8) as i8).to_ne_bytes().to_vec(),
        ElementType::Uint8 => (wrap_signed(number, 8) as u8).to_ne_bytes().to_vec(),
        ElementType::Uint8Clamped => to_uint8_clamp(number).to_ne_bytes().to_vec(),
        ElementType::Int16 => (wrap_signed(number, 16) as i16).to_ne_bytes().to_vec(),
        ElementType::Uint16 => (wrap_signed(number, 16) as u16).to_ne_bytes().to_vec(),
        ElementType::Int32 => (wrap_signed(number, 32) as i32).to_ne_bytes().to_vec(),
        ElementType::Uint32 => (wrap_signed(number, 32) as u32).to_ne_bytes().to_vec(),
        ElementType::Float16 => {
            let half = half::f16::from_f64(number);
            half.to_bits().to_ne_bytes().to_vec()
        }
        ElementType::Float32 => (number as f32).to_ne_bytes().to_vec(),
        ElementType::Float64 => number.to_ne_bytes().to_vec(),
        ElementType::BigInt64 | ElementType::BigUint64 => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "BigInt element type requires a BigInt value".into(),
            ));
        }
    };
    Ok(bytes)
}

/// Convert a BigInt value into the element bytes of a BigInt64/BigUint64
/// element (spec ToBigInt64/ToBigUint64, 25.2.4.3).
fn encode_bigint(element_type: ElementType, bigint: &BigInt) -> Result<Vec<u8>, JsError> {
    let bytes = match element_type {
        ElementType::BigInt64 => bigint.0.to_i64().unwrap_or(0).to_ne_bytes().to_vec(),
        ElementType::BigUint64 => bigint.0.to_u64().unwrap_or(0).to_ne_bytes().to_vec(),
        _ => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Number element type requires a Number value".into(),
            ));
        }
    };
    Ok(bytes)
}

/// The bytes a language value encodes to for `element_type`: a BigInt
/// content type coerces with ToBigInt*, anything else with ToNumber.
/// spec 25.2.4.2 (TypedArray [[Set]] element conversion).
pub fn encode_element(element_type: ElementType, value: &Value) -> Result<Vec<u8>, JsError> {
    if matches!(element_type, ElementType::BigInt64 | ElementType::BigUint64) {
        let bigint = match element_type {
            ElementType::BigInt64 => to_big_int64(value)?,
            _ => to_big_uint64(value)?,
        };
        encode_bigint(element_type, &bigint)
    } else {
        encode_number(element_type, to_number(value)?)
    }
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
