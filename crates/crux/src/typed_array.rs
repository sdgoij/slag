//! The TypedArray element types and the byte conversions of the
//! Integer-Indexed exotic (spec 10.4.5, 25.2.1): a shared byte buffer
//! ([[ArrayBufferData]]) plus the per-element encode/decode used by
//! [[Get]]/[[Set]]/[[GetOwnProperty]]/[[DefineOwnProperty]].

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


/// The `[[ArrayBufferData]]` block and its geometry box live in the
/// dependency-free `byteblock` crate, so the WebAssembly engine can share a
/// linear memory with an aliased `Memory.prototype.buffer` without depending
/// on the JS value model (`.notes/wasm-analysis.md` §7 item 8). Re-exported
/// under their historical paths, including for the JIT's `offset_of!` reads.
pub use byteblock::{AtomicOp, BlockState, SharedBuffer, WORKERS};

/// The leaf crate reports an out-of-bounds block access with its own error
/// type, so its API carries no `JsError`; the built-ins keep using `?`.
impl From<byteblock::OutOfBounds> for JsError {
    fn from(_: byteblock::OutOfBounds) -> Self {
        JsError::new(ErrorKind::TypeError, "buffer access out of bounds".into())
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

/// The exact integer form of `number` when its low-`bits` conversion is
/// `number as i64` (spec truncation mod 2^bits): an integral value in
/// [-2^31, 2^32), where the f64→i64 cast is exact and its low `bits`
/// reproduce the wrap for every integer element width up to 32 — the
/// overwhelmingly common in-range integer store (a count/byte loop) skips
/// `wrap_signed`'s f64 `rem_euclid`.
fn wrap_bits_fast(number: f64) -> Option<i64> {
    if number.fract() == 0.0 && (-2147483648.0..=4294967295.0).contains(&number) {
        Some(number as i64)
    } else {
        None
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
            out[0] = match wrap_bits_fast(number) {
                Some(raw) => raw as i8 as u8,
                None => wrap_signed(number, 8) as i8 as u8,
            };
            Ok(1)
        }
        ElementType::Uint8 => {
            out[0] = match wrap_bits_fast(number) {
                Some(raw) => raw as u8,
                None => wrap_signed(number, 8) as u8,
            };
            Ok(1)
        }
        ElementType::Uint8Clamped => {
            out[0] = to_uint8_clamp(number);
            Ok(1)
        }
        ElementType::Int16 => {
            let raw = match wrap_bits_fast(number) {
                Some(raw) => raw as i16,
                None => wrap_signed(number, 16) as i16,
            };
            out[..2].copy_from_slice(&raw.to_ne_bytes());
            Ok(2)
        }
        ElementType::Uint16 => {
            let raw = match wrap_bits_fast(number) {
                Some(raw) => raw as u16,
                None => wrap_signed(number, 16) as u16,
            };
            out[..2].copy_from_slice(&raw.to_ne_bytes());
            Ok(2)
        }
        ElementType::Int32 => {
            let raw = match wrap_bits_fast(number) {
                Some(raw) => raw as i32,
                None => wrap_signed(number, 32) as i32,
            };
            out[..4].copy_from_slice(&raw.to_ne_bytes());
            Ok(4)
        }
        ElementType::Uint32 => {
            let raw = match wrap_bits_fast(number) {
                Some(raw) => raw as u32,
                None => wrap_signed(number, 32) as u32,
            };
            out[..4].copy_from_slice(&raw.to_ne_bytes());
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
