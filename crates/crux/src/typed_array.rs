//! The TypedArray element types and the byte conversions of the
//! Integer-Indexed exotic (spec 10.4.5, 25.2.1): a shared byte buffer
//! ([[ArrayBufferData]]) plus the per-element encode/decode used by
//! [[Get]]/[[Set]]/[[GetOwnProperty]]/[[DefineOwnProperty]].

use std::cell::RefCell;
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
#[derive(Debug, Clone)]
pub struct SharedBuffer(pub Rc<RefCell<Vec<u8>>>);

impl SharedBuffer {
    /// Allocate a zero-filled buffer of `byte_length` bytes.
    pub fn new(byte_length: usize) -> Self {
        SharedBuffer(Rc::new(RefCell::new(vec![0u8; byte_length])))
    }

    pub fn byte_length(&self) -> usize {
        self.0.borrow().len()
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
