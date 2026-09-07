//! The wasm value space and numeric semantics (spec ch. 2.2 / 4.3).
//!
//! Floats are carried as raw IEEE bit patterns so behavior is deterministic;
//! the ratified NaN policy is canonical quiet NaN for arithmetic results
//! (payloads are preserved only by the explicitly bit-wise instructions:
//! `abs`/`neg`/`copysign`).

use crate::instr::NumOp;

/// Cut 11 compiled-ref token encoding: references cross the compiled boundary
/// as opaque u64 tokens carried in `I64` Cranelift values. Token 0 is the
/// null reference; a function reference is tagged with bit 63 and packs its
/// address's instance (bits 32-62, up to 2^31) and full function-index-space
/// index (bits 0-31); an external reference is tagged with bit 62 and packs
/// its payload kind (bits 60-61) and 60-bit payload; an internal `i31` (the
/// `any`-hierarchy reference the compiled subset carries) is tagged with bit
/// 60 (bits 61-63 clear) and packs its canonicalized 31-bit value; an
/// exception reference (`exnref`) is tagged with bit 61 (bits 60/62/63
/// clear) and packs the exception's store-pool id; an internal struct or
/// array object (`any`-hierarchy pool ids) is tagged with bit 59 (bits
/// 57-58 hold the struct/array kind, bits 60-63 clear) and packs the
/// object's store-pool id. An address/payload that does not fit stays
/// interpreted.
pub const REF_NULL_TOKEN: u64 = 0;
pub const REF_FUNC_TAG: u64 = 1 << 63;
pub const REF_EXTERN_TAG: u64 = 1 << 62;
const REF_EXTERN_KIND_SHIFT: u64 = 60;
const REF_EXTERN_PAYLOAD_MASK: u64 = (1 << 60) - 1;
const REF_EXTERN_KIND_MASK: u64 = 0b11 << REF_EXTERN_KIND_SHIFT;
const KIND_HOST: u64 = 0;
const KIND_I31: u64 = 1;
const KIND_STRUCT: u64 = 2;
const KIND_ARRAY: u64 = 3;
/// An internal (non-extern) `i31` reference: bit 60 with bits 61-63 clear.
pub const REF_I31_TAG: u64 = 1 << 60;
const REF_I31_VALUE_MASK: u64 = 0x7fff_ffff;
/// An exception reference (`exnref`): bit 61 with bits 60/62/63 clear, its
/// store-pool id in the low 60 bits.
pub const REF_EXN_TAG: u64 = 1 << 61;
pub const REF_EXN_PAYLOAD_MASK: u64 = (1 << 60) - 1;
/// An internal struct/array object: bit 59 with the struct/array kind in bits
/// 57-58 and the object's store-pool id in the low 57 bits (bits 60-63
/// clear, so it cannot collide with the func/extern/i31/exn regions). Host
/// `any` values ride the same region under a third kind (payload = the u32
/// host id).
pub const REF_GC_TAG: u64 = 1 << 59;
pub const REF_GC_KIND_SHIFT: u64 = 57;
pub const REF_GC_PAYLOAD_MASK: u64 = (1 << 57) - 1;
const GC_STRUCT: u64 = 0;
const GC_ARRAY: u64 = 1;
const GC_HOST: u64 = 2;

/// The compiled token for an internal struct object's pool id.
pub fn struct_ref_token(id: usize) -> Option<u64> {
    if id > REF_GC_PAYLOAD_MASK as usize {
        return None;
    }
    Some(REF_GC_TAG | GC_STRUCT << REF_GC_KIND_SHIFT | id as u64)
}

/// The compiled token for an internal array object's pool id.
pub fn array_ref_token(id: usize) -> Option<u64> {
    if id > REF_GC_PAYLOAD_MASK as usize {
        return None;
    }
    Some(REF_GC_TAG | GC_ARRAY << REF_GC_KIND_SHIFT | id as u64)
}

/// The compiled token for a function address, when both parts fit.
pub fn func_ref_token(instance: usize, index: usize) -> Option<u64> {
    if instance > 0x7fff_ffff || index > u32::MAX as usize {
        return None;
    }
    Some(REF_FUNC_TAG | ((instance as u64) << 32) | index as u64)
}

/// The compiled token for an external reference, when its payload fits.
pub fn extern_ref_token(inner: ExternInner) -> Option<u64> {
    let (kind, payload) = match inner {
        ExternInner::Host(host) => (KIND_HOST, u64::from(host)),
        ExternInner::I31(value) => (KIND_I31, value as u32 as u64),
        ExternInner::Struct(id) => (KIND_STRUCT, id as u64),
        ExternInner::Array(id) => (KIND_ARRAY, id as u64),
    };
    if payload & !REF_EXTERN_PAYLOAD_MASK != 0 {
        return None;
    }
    Some(REF_EXTERN_TAG | kind << REF_EXTERN_KIND_SHIFT | payload)
}

/// Encode an interpreter value as its compiled token. Only the references the
/// compiled subset carries are encodable: null, function references, external
/// references, internal `i31` references, and exception references.
pub fn ref_to_token(value: Value) -> Option<u64> {
    match value {
        Value::Ref(RefValue::Null) => Some(REF_NULL_TOKEN),
        Value::Ref(RefValue::Func(addr)) => func_ref_token(addr.instance, addr.index),
        Value::Ref(RefValue::Extern(inner)) => extern_ref_token(inner),
        Value::Ref(RefValue::I31(value)) => Some(REF_I31_TAG | value as u32 as u64),
        Value::Ref(RefValue::Exn(id)) => {
            if id > REF_EXN_PAYLOAD_MASK as usize {
                return None;
            }
            Some(REF_EXN_TAG | id as u64)
        }
        Value::Ref(RefValue::Struct(id)) => struct_ref_token(id),
        Value::Ref(RefValue::Array(id)) => array_ref_token(id),
        Value::Ref(RefValue::Host(id)) => {
            Some(REF_GC_TAG | GC_HOST << REF_GC_KIND_SHIFT | u64::from(id))
        }
        _ => None,
    }
}

/// Decode a compiled token back to an interpreter reference value.
pub fn token_to_ref(token: u64) -> Option<Value> {
    if token == REF_NULL_TOKEN {
        return Some(Value::Ref(RefValue::Null));
    }
    if token & REF_FUNC_TAG != 0 {
        let instance = ((token >> 32) & 0x7fff_ffff) as usize;
        let index = (token & u64::from(u32::MAX)) as usize;
        return Some(Value::Ref(RefValue::Func(FuncAddr { instance, index })));
    }
    if token & REF_EXTERN_TAG != 0 {
        let inner = match (token & REF_EXTERN_KIND_MASK) >> REF_EXTERN_KIND_SHIFT {
            KIND_HOST => ExternInner::Host(token as u32),
            KIND_I31 => ExternInner::I31(token as u32 as i32),
            KIND_STRUCT => ExternInner::Struct((token & REF_EXTERN_PAYLOAD_MASK) as usize),
            KIND_ARRAY => ExternInner::Array((token & REF_EXTERN_PAYLOAD_MASK) as usize),
            _ => return None,
        };
        return Some(Value::Ref(RefValue::Extern(inner)));
    }
    if token & REF_I31_TAG != 0 {
        return Some(Value::Ref(RefValue::I31(
            (token & REF_I31_VALUE_MASK) as i32,
        )));
    }
    if token & REF_EXN_TAG != 0 {
        return Some(Value::Ref(RefValue::Exn(
            (token & REF_EXN_PAYLOAD_MASK) as usize,
        )));
    }
    if token & REF_GC_TAG != 0 {
        let id = (token & REF_GC_PAYLOAD_MASK) as usize;
        return Some(match (token >> REF_GC_KIND_SHIFT) & 0b11 {
            GC_STRUCT => Value::Ref(RefValue::Struct(id)),
            GC_ARRAY => Value::Ref(RefValue::Array(id)),
            _ => Value::Ref(RefValue::Host(id as u32)),
        });
    }
    None
}

/// Where a function reference points: the full function index space of a
/// store instance. The instance stays alive for the life of the store, so
/// references imported by later modules remain valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuncAddr {
    pub instance: usize,
    pub index: usize,
}

/// The payload an external reference ([`RefValue::Extern`]) wraps. External
/// references are an opaque `extern`-typed box around an internal object
/// (`extern.convert_any`); internalizing unwraps the box again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternInner {
    /// A host payload (`ref.extern n` / `ref.host n`).
    Host(u32),
    /// A wrapped i31 (value semantics; masked to 31 bits).
    I31(i32),
    /// A wrapped struct object: an id into the store's object pool.
    Struct(usize),
    /// A wrapped array object: an id into the store's object pool.
    Array(usize),
}

/// A runtime reference (spec 2.2.5 / 4.2.1). GC values (i31, struct/array
/// objects, and the extern/host wrappers around them) arrive with the GC cut;
/// struct/array payloads are stable ids into the store's object pool so
/// `ref.eq` compares object identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefValue {
    Null,
    Func(FuncAddr),
    /// An external reference: an `extern`-typed box around an internal object.
    Extern(ExternInner),
    /// An exception reference: the id of the exception in the store's pool.
    Exn(usize),
    /// A host payload used as an internal `any` value (`ref.host n`, the
    /// result of internalizing a host external).
    Host(u32),
    /// An unboxed 31-bit scalar reference (canonicalized on creation).
    I31(i32),
    /// A struct object: an id into the store's object pool.
    Struct(usize),
    /// An array object: an id into the store's object pool.
    Array(usize),
}

impl RefValue {
    /// Box an i32 as an i31, truncating to its low 31 bits so every value has
    /// one representation (spec: `ref.i31` takes the value modulo 2^31).
    pub fn i31(value: i32) -> RefValue {
        RefValue::I31(value & 0x7fff_ffff)
    }

    /// Whether this is a non-null reference.
    pub fn is_null(&self) -> bool {
        matches!(self, RefValue::Null)
    }

    /// The host payload behind a host-any or host-wrapping external value, if
    /// any. Used by the runner to compare `ref.host`/`ref.extern` results.
    pub fn host_payload(&self) -> Option<u32> {
        match *self {
            RefValue::Host(n) => Some(n),
            RefValue::Extern(ExternInner::Host(n)) => Some(n),
            _ => None,
        }
    }

    /// Unbox an external reference to the internal object it wraps.
    pub fn unwrap_extern(&self) -> RefValue {
        match *self {
            RefValue::Extern(inner) => inner.into_ref(),
            other => other,
        }
    }
}

impl ExternInner {
    /// Box a non-null internal reference (any value) as an external. Returns
    /// `None` for values that cannot be externalized.
    pub fn wrap(value: RefValue) -> Option<ExternInner> {
        Some(match value {
            RefValue::Host(n) => ExternInner::Host(n),
            RefValue::I31(v) => ExternInner::I31(v),
            RefValue::Struct(id) => ExternInner::Struct(id),
            RefValue::Array(id) => ExternInner::Array(id),
            _ => return None,
        })
    }

    /// Unbox to the internal reference the external wraps.
    pub fn into_ref(self) -> RefValue {
        match self {
            ExternInner::Host(n) => RefValue::Host(n),
            ExternInner::I31(v) => RefValue::I31(v),
            ExternInner::Struct(id) => RefValue::Struct(id),
            ExternInner::Array(id) => RefValue::Array(id),
        }
    }
}

/// A runtime value. Floats are their IEEE bit patterns; v128 vectors are the
/// 128-bit little-endian concatenation of their lanes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value {
    I32(i32),
    I64(i64),
    F32(u32),
    F64(u64),
    V128(u128),
    Ref(RefValue),
}

impl Value {
    pub fn from_bits(kind: u8, bits: u64) -> Option<Value> {
        Some(match kind {
            0x7f => Value::I32(bits as i32),
            0x7e => Value::I64(bits as i64),
            0x7d => Value::F32(bits as u32),
            0x7c => Value::F64(bits),
            _ => return None,
        })
    }
}

/// A runtime exception. Maps to `WebAssembly.RuntimeError` at the JS
/// boundary; inside the interpreter it unwinds to the nearest catch or the
/// call edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trap {
    Unreachable,
    IntegerDivideByZero,
    IntegerOverflow,
    InvalidConversionToInteger,
    OutOfBoundsMemoryAccess,
    OutOfBoundsTableAccess,
    IndirectCallTypeMismatch,
    UndefinedElement,
    UninitializedElement,
    NullReference,
    NullFunctionReference,
    /// `throw_ref` of a null exception reference.
    NullExceptionReference,
    /// A struct operation on a null struct reference.
    NullStructReference,
    /// An array operation on a null array reference.
    NullArrayReference,
    /// An i31 operation on a null i31 reference.
    NullI31Reference,
    /// An index past the end of an array (`out of bounds array access`).
    OutOfBoundsArrayAccess,
    /// `ref.cast`/`extern.convert_any`-adjacent failure (`cast failure`).
    CastFailure,
    CallStackExhausted,
    UnsupportedImport,
    UnknownFunction,
}

/// Canonical quiet NaNs (positive sign, canonical payload).
pub const QNAN32: u32 = 0x7fc0_0000;
pub const QNAN64: u64 = 0x7ff8_0000_0000_0000;

fn is_nan32(bits: u32) -> bool {
    bits & 0x7f80_0000 == 0x7f80_0000 && bits & 0x007f_ffff != 0
}

fn is_nan64(bits: u64) -> bool {
    bits & 0x7ff0_0000_0000_0000 == 0x7ff0_0000_0000_0000 && bits & 0x000f_ffff_ffff_ffff != 0
}

/// Run one numeric instruction over `operands` (bottom-to-top). Every numeric
/// instruction produces exactly one result.
pub fn exec_num(op: NumOp, operands: &[Value]) -> Result<Value, Trap> {
    use NumOp::*;
    // Dispatch by category; unary/binary shapes are handled by helpers below.
    match op {
        // ---- i32 tests and comparisons ----
        I32Eqz => i32_test(operands, |a| a == 0),
        I32Eq => i32_cmp(operands, |a, b| a == b),
        I32Ne => i32_cmp(operands, |a, b| a != b),
        I32LtS => i32_cmp(operands, |a, b| a < b),
        I32LtU => i32_cmp(operands, |a, b| (a as u32) < (b as u32)),
        I32GtS => i32_cmp(operands, |a, b| a > b),
        I32GtU => i32_cmp(operands, |a, b| (a as u32) > (b as u32)),
        I32LeS => i32_cmp(operands, |a, b| a <= b),
        I32LeU => i32_cmp(operands, |a, b| (a as u32) <= (b as u32)),
        I32GeS => i32_cmp(operands, |a, b| a >= b),
        I32GeU => i32_cmp(operands, |a, b| (a as u32) >= (b as u32)),
        I64Eqz => i64_test(operands, |a| a == 0),
        I64Eq => i64_cmp(operands, |a, b| a == b),
        I64Ne => i64_cmp(operands, |a, b| a != b),
        I64LtS => i64_cmp(operands, |a, b| a < b),
        I64LtU => i64_cmp(operands, |a, b| (a as u64) < (b as u64)),
        I64GtS => i64_cmp(operands, |a, b| a > b),
        I64GtU => i64_cmp(operands, |a, b| (a as u64) > (b as u64)),
        I64LeS => i64_cmp(operands, |a, b| a <= b),
        I64LeU => i64_cmp(operands, |a, b| (a as u64) <= (b as u64)),
        I64GeS => i64_cmp(operands, |a, b| a >= b),
        I64GeU => i64_cmp(operands, |a, b| (a as u64) >= (b as u64)),
        F32Eq => f32_cmp(operands, |a, b| a == b),
        F32Ne => f32_cmp(operands, |a, b| a != b),
        F32Lt => f32_cmp(operands, |a, b| a < b),
        F32Gt => f32_cmp(operands, |a, b| a > b),
        F32Le => f32_cmp(operands, |a, b| a <= b),
        F32Ge => f32_cmp(operands, |a, b| a >= b),
        F64Eq => f64_cmp(operands, |a, b| a == b),
        F64Ne => f64_cmp(operands, |a, b| a != b),
        F64Lt => f64_cmp(operands, |a, b| a < b),
        F64Gt => f64_cmp(operands, |a, b| a > b),
        F64Le => f64_cmp(operands, |a, b| a <= b),
        F64Ge => f64_cmp(operands, |a, b| a >= b),

        // ---- i32 arithmetic ----
        I32Clz => i32_unop(operands, |a| a.leading_zeros() as i32),
        I32Ctz => i32_unop(operands, |a| a.trailing_zeros() as i32),
        I32Popcnt => i32_unop(operands, |a| a.count_ones() as i32),
        I32Add => i32_binop(operands, |a, b| a.wrapping_add(b)),
        I32Sub => i32_binop(operands, |a, b| a.wrapping_sub(b)),
        I32Mul => i32_binop(operands, |a, b| a.wrapping_mul(b)),
        I32DivS => {
            let (a, b) = two_i32(operands)?;
            if b == 0 {
                return Err(Trap::IntegerDivideByZero);
            }
            if a == i32::MIN && b == -1 {
                return Err(Trap::IntegerOverflow);
            }
            Ok(Value::I32(a / b))
        }
        I32DivU => {
            let (a, b) = two_i32(operands)?;
            if b == 0 {
                return Err(Trap::IntegerDivideByZero);
            }
            Ok(Value::I32(((a as u32) / (b as u32)) as i32))
        }
        I32RemS => {
            let (a, b) = two_i32(operands)?;
            if b == 0 {
                return Err(Trap::IntegerDivideByZero);
            }
            if a == i32::MIN && b == -1 {
                return Ok(Value::I32(0));
            }
            Ok(Value::I32(a % b))
        }
        I32RemU => {
            let (a, b) = two_i32(operands)?;
            if b == 0 {
                return Err(Trap::IntegerDivideByZero);
            }
            Ok(Value::I32(((a as u32) % (b as u32)) as i32))
        }
        I32And => i32_binop(operands, |a, b| a & b),
        I32Or => i32_binop(operands, |a, b| a | b),
        I32Xor => i32_binop(operands, |a, b| a ^ b),
        I32Shl => i32_binop(operands, |a, b| a.wrapping_shl(b as u32)),
        I32ShrS => i32_binop(operands, |a, b| a.wrapping_shr(b as u32)),
        I32ShrU => i32_binop(operands, |a, b| ((a as u32).wrapping_shr(b as u32)) as i32),
        I32Rotl => i32_binop(operands, |a, b| a.rotate_left(b as u32)),
        I32Rotr => i32_binop(operands, |a, b| a.rotate_right(b as u32)),

        // ---- i64 arithmetic ----
        I64Clz => i64_unop(operands, |a| a.leading_zeros() as i64),
        I64Ctz => i64_unop(operands, |a| a.trailing_zeros() as i64),
        I64Popcnt => i64_unop(operands, |a| a.count_ones() as i64),
        I64Add => i64_binop(operands, |a, b| a.wrapping_add(b)),
        I64Sub => i64_binop(operands, |a, b| a.wrapping_sub(b)),
        I64Mul => i64_binop(operands, |a, b| a.wrapping_mul(b)),
        I64DivS => {
            let (a, b) = two_i64(operands)?;
            if b == 0 {
                return Err(Trap::IntegerDivideByZero);
            }
            if a == i64::MIN && b == -1 {
                return Err(Trap::IntegerOverflow);
            }
            Ok(Value::I64(a / b))
        }
        I64DivU => {
            let (a, b) = two_i64(operands)?;
            if b == 0 {
                return Err(Trap::IntegerDivideByZero);
            }
            Ok(Value::I64(((a as u64) / (b as u64)) as i64))
        }
        I64RemS => {
            let (a, b) = two_i64(operands)?;
            if b == 0 {
                return Err(Trap::IntegerDivideByZero);
            }
            if a == i64::MIN && b == -1 {
                return Ok(Value::I64(0));
            }
            Ok(Value::I64(a % b))
        }
        I64RemU => {
            let (a, b) = two_i64(operands)?;
            if b == 0 {
                return Err(Trap::IntegerDivideByZero);
            }
            Ok(Value::I64(((a as u64) % (b as u64)) as i64))
        }
        I64And => i64_binop(operands, |a, b| a & b),
        I64Or => i64_binop(operands, |a, b| a | b),
        I64Xor => i64_binop(operands, |a, b| a ^ b),
        I64Shl => i64_binop(operands, |a, b| a.wrapping_shl(b as u32)),
        I64ShrS => i64_binop(operands, |a, b| a.wrapping_shr(b as u32)),
        I64ShrU => i64_binop(operands, |a, b| ((a as u64).wrapping_shr(b as u32)) as i64),
        I64Rotl => i64_binop(operands, |a, b| a.rotate_left(b as u32)),
        I64Rotr => i64_binop(operands, |a, b| a.rotate_right(b as u32)),

        // ---- f32 arithmetic ----
        F32Abs => f32_bitop(operands, |a| a & 0x7fff_ffff),
        F32Neg => f32_bitop(operands, |a| a ^ 0x8000_0000),
        F32Ceil => f32_unop(operands, f32::ceil),
        F32Floor => f32_unop(operands, f32::floor),
        F32Trunc => f32_unop(operands, f32::trunc),
        F32Nearest => f32_unop(operands, f32::round_ties_even),
        F32Sqrt => f32_unop(operands, f32::sqrt),
        F32Add => f32_binop(operands, |a, b| a + b),
        F32Sub => f32_binop(operands, |a, b| a - b),
        F32Mul => f32_binop(operands, |a, b| a * b),
        F32Div => f32_binop(operands, |a, b| a / b),
        F32Min => f32_binop(operands, fmin32),
        F32Max => f32_binop(operands, fmax32),
        F32Copysign => {
            let (a, b) = two_bits(operands)?;
            Ok(Value::F32((a & 0x7fff_ffff) | (b & 0x8000_0000)))
        }

        // ---- f64 arithmetic ----
        F64Abs => f64_bitop(operands, |a| a & 0x7fff_ffff_ffff_ffff),
        F64Neg => f64_bitop(operands, |a| a ^ 0x8000_0000_0000_0000),
        F64Ceil => f64_unop(operands, f64::ceil),
        F64Floor => f64_unop(operands, f64::floor),
        F64Trunc => f64_unop(operands, f64::trunc),
        F64Nearest => f64_unop(operands, f64::round_ties_even),
        F64Sqrt => f64_unop(operands, f64::sqrt),
        F64Add => f64_binop(operands, |a, b| a + b),
        F64Sub => f64_binop(operands, |a, b| a - b),
        F64Mul => f64_binop(operands, |a, b| a * b),
        F64Div => f64_binop(operands, |a, b| a / b),
        F64Min => f64_binop(operands, fmin64),
        F64Max => f64_binop(operands, fmax64),
        F64Copysign => {
            let (a, b) = two_bits64(operands)?;
            Ok(Value::F64(
                (a & 0x7fff_ffff_ffff_ffff) | (b & 0x8000_0000_0000_0000),
            ))
        }

        // ---- conversions ----
        I32WrapI64 => {
            let Value::I64(a) = operands[0] else {
                return Err(Trap::UnknownFunction);
            };
            Ok(Value::I32(a as i32))
        }
        I32TruncF32S => {
            let a = one_f32(operands)?;
            Ok(Value::I32(trunc_to_i32(a as f64)?))
        }
        I32TruncF32U => {
            let a = one_f32(operands)?;
            Ok(Value::I32(trunc_to_u32(a as f64)? as i32))
        }
        I32TruncF64S => {
            let a = one_f64(operands)?;
            Ok(Value::I32(trunc_to_i32(a)?))
        }
        I32TruncF64U => {
            let a = one_f64(operands)?;
            Ok(Value::I32(trunc_to_u32(a)? as i32))
        }
        I64ExtendI32S => {
            let Value::I32(a) = operands[0] else {
                return Err(Trap::UnknownFunction);
            };
            Ok(Value::I64(a as i64))
        }
        I64ExtendI32U => {
            let Value::I32(a) = operands[0] else {
                return Err(Trap::UnknownFunction);
            };
            Ok(Value::I64((a as u32) as i64))
        }
        I64TruncF32S => {
            let a = one_f32(operands)?;
            Ok(Value::I64(trunc_to_i64(a as f64)?))
        }
        I64TruncF32U => {
            let a = one_f32(operands)?;
            Ok(Value::I64(trunc_to_u64(a as f64)? as i64))
        }
        I64TruncF64S => {
            let a = one_f64(operands)?;
            Ok(Value::I64(trunc_to_i64(a)?))
        }
        I64TruncF64U => {
            let a = one_f64(operands)?;
            Ok(Value::I64(trunc_to_u64(a)? as i64))
        }
        F32ConvertI32S => {
            let Value::I32(a) = operands[0] else {
                return Err(Trap::UnknownFunction);
            };
            Ok(Value::F32(canon32((a as f32).to_bits())))
        }
        F32ConvertI32U => {
            let Value::I32(a) = operands[0] else {
                return Err(Trap::UnknownFunction);
            };
            Ok(Value::F32(canon32((a as u32 as f32).to_bits())))
        }
        F32ConvertI64S => {
            let Value::I64(a) = operands[0] else {
                return Err(Trap::UnknownFunction);
            };
            Ok(Value::F32(canon32((a as f32).to_bits())))
        }
        F32ConvertI64U => {
            let Value::I64(a) = operands[0] else {
                return Err(Trap::UnknownFunction);
            };
            Ok(Value::F32(canon32((a as u64 as f32).to_bits())))
        }
        F32DemoteF64 => {
            let a = one_f64(operands)?;
            Ok(Value::F32(canon32((a as f32).to_bits())))
        }
        F64ConvertI32S => {
            let Value::I32(a) = operands[0] else {
                return Err(Trap::UnknownFunction);
            };
            Ok(Value::F64((a as f64).to_bits()))
        }
        F64ConvertI32U => {
            let Value::I32(a) = operands[0] else {
                return Err(Trap::UnknownFunction);
            };
            Ok(Value::F64((a as u32 as f64).to_bits()))
        }
        F64ConvertI64S => {
            let Value::I64(a) = operands[0] else {
                return Err(Trap::UnknownFunction);
            };
            Ok(Value::F64(canon64((a as f64).to_bits())))
        }
        F64ConvertI64U => {
            let Value::I64(a) = operands[0] else {
                return Err(Trap::UnknownFunction);
            };
            Ok(Value::F64(canon64((a as u64 as f64).to_bits())))
        }
        F64PromoteF32 => {
            let a = one_f32(operands)?;
            Ok(Value::F64((a as f64).to_bits()))
        }
        I32ReinterpretF32 => {
            let Value::F32(a) = operands[0] else {
                return Err(Trap::UnknownFunction);
            };
            Ok(Value::I32(a as i32))
        }
        I64ReinterpretF64 => {
            let Value::F64(a) = operands[0] else {
                return Err(Trap::UnknownFunction);
            };
            Ok(Value::I64(a as i64))
        }
        F32ReinterpretI32 => {
            let Value::I32(a) = operands[0] else {
                return Err(Trap::UnknownFunction);
            };
            Ok(Value::F32(a as u32))
        }
        F64ReinterpretI64 => {
            let Value::I64(a) = operands[0] else {
                return Err(Trap::UnknownFunction);
            };
            Ok(Value::F64(a as u64))
        }

        // ---- sign extension ----
        I32Extend8S => i32_unop(operands, |a| a as i8 as i32),
        I32Extend16S => i32_unop(operands, |a| a as i16 as i32),
        I64Extend8S => i64_unop(operands, |a| a as i8 as i64),
        I64Extend16S => i64_unop(operands, |a| a as i16 as i64),
        I64Extend32S => i64_unop(operands, |a| a as i32 as i64),

        // ---- saturating float-to-int ----
        I32TruncSatF32S => {
            let a = one_f32(operands)?;
            Ok(Value::I32(sat_i32_from_f64(a as f64)))
        }
        I32TruncSatF32U => {
            let a = one_f32(operands)?;
            Ok(Value::I32(sat_u32_from_f64(a as f64) as i32))
        }
        I32TruncSatF64S => {
            let a = one_f64(operands)?;
            Ok(Value::I32(sat_i32_from_f64(a)))
        }
        I32TruncSatF64U => {
            let a = one_f64(operands)?;
            Ok(Value::I32(sat_u32_from_f64(a) as i32))
        }
        I64TruncSatF32S => {
            let a = one_f32(operands)?;
            Ok(Value::I64(sat_i64_from_f64(a as f64)))
        }
        I64TruncSatF32U => {
            let a = one_f32(operands)?;
            Ok(Value::I64(sat_u64_from_f64(a as f64) as i64))
        }
        I64TruncSatF64S => {
            let a = one_f64(operands)?;
            Ok(Value::I64(sat_i64_from_f64(a)))
        }
        I64TruncSatF64U => {
            let a = one_f64(operands)?;
            Ok(Value::I64(sat_u64_from_f64(a) as i64))
        }
    }
}

// ---- operand accessors ----

fn one_i32(operands: &[Value]) -> Result<i32, Trap> {
    match operands[0] {
        Value::I32(v) => Ok(v),
        _ => Err(Trap::UnknownFunction),
    }
}

fn one_i64(operands: &[Value]) -> Result<i64, Trap> {
    match operands[0] {
        Value::I64(v) => Ok(v),
        _ => Err(Trap::UnknownFunction),
    }
}

fn one_f32(operands: &[Value]) -> Result<f32, Trap> {
    match operands[0] {
        Value::F32(v) => Ok(f32::from_bits(v)),
        _ => Err(Trap::UnknownFunction),
    }
}

fn one_f64(operands: &[Value]) -> Result<f64, Trap> {
    match operands[0] {
        Value::F64(v) => Ok(f64::from_bits(v)),
        _ => Err(Trap::UnknownFunction),
    }
}

fn two_i32(operands: &[Value]) -> Result<(i32, i32), Trap> {
    match (operands[0], operands[1]) {
        (Value::I32(a), Value::I32(b)) => Ok((a, b)),
        _ => Err(Trap::UnknownFunction),
    }
}

fn two_i64(operands: &[Value]) -> Result<(i64, i64), Trap> {
    match (operands[0], operands[1]) {
        (Value::I64(a), Value::I64(b)) => Ok((a, b)),
        _ => Err(Trap::UnknownFunction),
    }
}

fn two_bits(operands: &[Value]) -> Result<(u32, u32), Trap> {
    match (operands[0], operands[1]) {
        (Value::F32(a), Value::F32(b)) => Ok((a, b)),
        _ => Err(Trap::UnknownFunction),
    }
}

fn two_bits64(operands: &[Value]) -> Result<(u64, u64), Trap> {
    match (operands[0], operands[1]) {
        (Value::F64(a), Value::F64(b)) => Ok((a, b)),
        _ => Err(Trap::UnknownFunction),
    }
}

fn i32_test(operands: &[Value], f: impl Fn(i32) -> bool) -> Result<Value, Trap> {
    Ok(Value::I32(f(one_i32(operands)?) as i32))
}

fn i64_test(operands: &[Value], f: impl Fn(i64) -> bool) -> Result<Value, Trap> {
    Ok(Value::I32(f(one_i64(operands)?) as i32))
}

fn i32_cmp(operands: &[Value], f: impl Fn(i32, i32) -> bool) -> Result<Value, Trap> {
    let (a, b) = two_i32(operands)?;
    Ok(Value::I32(f(a, b) as i32))
}

fn i64_cmp(operands: &[Value], f: impl Fn(i64, i64) -> bool) -> Result<Value, Trap> {
    let (a, b) = two_i64(operands)?;
    Ok(Value::I32(f(a, b) as i32))
}

fn f32_cmp(operands: &[Value], f: impl Fn(f32, f32) -> bool) -> Result<Value, Trap> {
    let (a, b) = two_bits(operands)?;
    Ok(Value::I32(f(f32::from_bits(a), f32::from_bits(b)) as i32))
}

fn f64_cmp(operands: &[Value], f: impl Fn(f64, f64) -> bool) -> Result<Value, Trap> {
    let (a, b) = two_bits64(operands)?;
    Ok(Value::I32(f(f64::from_bits(a), f64::from_bits(b)) as i32))
}

fn i32_unop(operands: &[Value], f: impl Fn(i32) -> i32) -> Result<Value, Trap> {
    Ok(Value::I32(f(one_i32(operands)?)))
}

fn i64_unop(operands: &[Value], f: impl Fn(i64) -> i64) -> Result<Value, Trap> {
    Ok(Value::I64(f(one_i64(operands)?)))
}

fn i32_binop(operands: &[Value], f: impl Fn(i32, i32) -> i32) -> Result<Value, Trap> {
    let (a, b) = two_i32(operands)?;
    Ok(Value::I32(f(a, b)))
}

fn i64_binop(operands: &[Value], f: impl Fn(i64, i64) -> i64) -> Result<Value, Trap> {
    let (a, b) = two_i64(operands)?;
    Ok(Value::I64(f(a, b)))
}

/// Unary float op that canonicalizes NaN results.
fn f32_unop(operands: &[Value], f: impl Fn(f32) -> f32) -> Result<Value, Trap> {
    let a = one_f32(operands)?;
    Ok(Value::F32(canon32(f(a).to_bits())))
}

fn f64_unop(operands: &[Value], f: impl Fn(f64) -> f64) -> Result<Value, Trap> {
    let a = one_f64(operands)?;
    Ok(Value::F64(canon64(f(a).to_bits())))
}

fn f32_bitop(operands: &[Value], f: impl Fn(u32) -> u32) -> Result<Value, Trap> {
    match operands[0] {
        Value::F32(a) => Ok(Value::F32(f(a))),
        _ => Err(Trap::UnknownFunction),
    }
}

fn f64_bitop(operands: &[Value], f: impl Fn(u64) -> u64) -> Result<Value, Trap> {
    match operands[0] {
        Value::F64(a) => Ok(Value::F64(f(a))),
        _ => Err(Trap::UnknownFunction),
    }
}

fn f32_binop(operands: &[Value], f: impl Fn(f32, f32) -> f32) -> Result<Value, Trap> {
    let (a, b) = two_bits(operands)?;
    Ok(Value::F32(canon32(
        f(f32::from_bits(a), f32::from_bits(b)).to_bits(),
    )))
}

fn f64_binop(operands: &[Value], f: impl Fn(f64, f64) -> f64) -> Result<Value, Trap> {
    let (a, b) = two_bits64(operands)?;
    Ok(Value::F64(canon64(
        f(f64::from_bits(a), f64::from_bits(b)).to_bits(),
    )))
}

// ---- NaN policy ----

fn canon32(bits: u32) -> u32 {
    if is_nan32(bits) { QNAN32 } else { bits }
}

fn canon64(bits: u64) -> u64 {
    if is_nan64(bits) { QNAN64 } else { bits }
}

/// Spec 4.3.3 `min`: NaN-aware and with the signed-zero rule.
fn fmin32(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        return f32::from_bits(QNAN32);
    }
    if a < b {
        return a;
    }
    if b < a {
        return b;
    }
    if a == 0.0 && b == 0.0 && (a.is_sign_negative() || b.is_sign_negative()) {
        return -0.0;
    }
    a
}

fn fmin64(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::from_bits(QNAN64);
    }
    if a < b {
        return a;
    }
    if b < a {
        return b;
    }
    if a == 0.0 && b == 0.0 && (a.is_sign_negative() || b.is_sign_negative()) {
        return -0.0;
    }
    a
}

fn fmax32(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        return f32::from_bits(QNAN32);
    }
    if a > b {
        return a;
    }
    if b > a {
        return b;
    }
    if a == 0.0 && b == 0.0 {
        // Only `max(-0, -0)` is negative.
        return if a.is_sign_negative() && b.is_sign_negative() {
            -0.0
        } else {
            0.0
        };
    }
    a
}

fn fmax64(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::from_bits(QNAN64);
    }
    if a > b {
        return a;
    }
    if b > a {
        return b;
    }
    if a == 0.0 && b == 0.0 {
        // Only `max(-0, -0)` is negative.
        return if a.is_sign_negative() && b.is_sign_negative() {
            -0.0
        } else {
            0.0
        };
    }
    a
}

// ---- truncating and saturating conversions ----

fn trunc_to_i32(a: f64) -> Result<i32, Trap> {
    if a.is_nan() {
        return Err(Trap::InvalidConversionToInteger);
    }
    let t = a.trunc();
    if !(-2147483648.0..2147483648.0).contains(&t) {
        return Err(Trap::IntegerOverflow);
    }
    Ok(t as i32)
}

fn trunc_to_u32(a: f64) -> Result<u32, Trap> {
    if a.is_nan() {
        return Err(Trap::InvalidConversionToInteger);
    }
    let t = a.trunc();
    if t <= -1.0 || t >= 4294967296.0 {
        return Err(Trap::IntegerOverflow);
    }
    Ok(t as u32)
}

fn trunc_to_i64(a: f64) -> Result<i64, Trap> {
    if a.is_nan() {
        return Err(Trap::InvalidConversionToInteger);
    }
    let t = a.trunc();
    if !(-9223372036854775808.0..9223372036854775808.0).contains(&t) {
        return Err(Trap::IntegerOverflow);
    }
    Ok(t as i64)
}

fn trunc_to_u64(a: f64) -> Result<u64, Trap> {
    if a.is_nan() {
        return Err(Trap::InvalidConversionToInteger);
    }
    let t = a.trunc();
    if t <= -1.0 || t >= 18446744073709551616.0 {
        return Err(Trap::IntegerOverflow);
    }
    Ok(t as u64)
}

fn sat_i32_from_f64(a: f64) -> i32 {
    if a.is_nan() {
        return 0;
    }
    let t = a.trunc();
    if t <= -2147483649.0 {
        return i32::MIN;
    }
    if t >= 2147483648.0 {
        return i32::MAX;
    }
    t as i32
}

fn sat_u32_from_f64(a: f64) -> u32 {
    if a.is_nan() {
        return 0;
    }
    let t = a.trunc();
    if t <= -1.0 {
        return 0;
    }
    if t >= 4294967296.0 {
        return u32::MAX;
    }
    t as u32
}

fn sat_i64_from_f64(a: f64) -> i64 {
    if a.is_nan() {
        return 0;
    }
    let t = a.trunc();
    // 2^63 as f64 is exactly representable.
    if t <= -9223372036854777856.0 {
        return i64::MIN;
    }
    if t >= 9223372036854775808.0 {
        return i64::MAX;
    }
    t as i64
}

fn sat_u64_from_f64(a: f64) -> u64 {
    if a.is_nan() {
        return 0;
    }
    let t = a.trunc();
    if t <= -1.0 {
        return 0;
    }
    if t >= 18446744073709551616.0 {
        return u64::MAX;
    }
    t as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(op: NumOp, operands: &[Value]) -> Result<Value, Trap> {
        exec_num(op, operands)
    }

    #[test]
    fn integer_division_and_remainder() {
        assert_eq!(
            run(NumOp::I32DivS, &[Value::I32(-7), Value::I32(3)]).unwrap(),
            Value::I32(-2)
        );
        assert_eq!(
            run(NumOp::I32RemS, &[Value::I32(-7), Value::I32(3)]).unwrap(),
            Value::I32(-1)
        );
        assert_eq!(
            run(NumOp::I32DivU, &[Value::I32(-1), Value::I32(1)]).unwrap(),
            Value::I32(-1)
        );
        assert_eq!(
            run(NumOp::I32DivS, &[Value::I32(1), Value::I32(0)]),
            Err(Trap::IntegerDivideByZero)
        );
        assert_eq!(
            run(NumOp::I32DivS, &[Value::I32(i32::MIN), Value::I32(-1)]),
            Err(Trap::IntegerOverflow)
        );
        assert_eq!(
            run(NumOp::I32RemS, &[Value::I32(i32::MIN), Value::I32(-1)]).unwrap(),
            Value::I32(0)
        );
    }

    #[test]
    fn shifts_mask_the_count() {
        assert_eq!(
            run(NumOp::I32Shl, &[Value::I32(1), Value::I32(33)]).unwrap(),
            Value::I32(2)
        );
        assert_eq!(
            run(NumOp::I64ShrU, &[Value::I64(-1), Value::I64(65)]).unwrap(),
            Value::I64(i64::MAX)
        );
    }

    #[test]
    fn truncating_conversions_trap_out_of_range() {
        let nan = Value::F32(QNAN32);
        assert_eq!(
            run(NumOp::I32TruncF32S, &[nan]),
            Err(Trap::InvalidConversionToInteger)
        );
        assert_eq!(
            run(NumOp::I32TruncF32S, &[Value::F32(f32::INFINITY.to_bits())]),
            Err(Trap::IntegerOverflow)
        );
        assert_eq!(
            run(
                NumOp::I32TruncF32S,
                &[Value::F32((-2147483904.0f32).to_bits())]
            ),
            Err(Trap::IntegerOverflow)
        );
        assert_eq!(
            run(
                NumOp::I32TruncF32S,
                &[Value::F32((-2147483648.0f32).to_bits())]
            )
            .unwrap(),
            Value::I32(i32::MIN)
        );
    }

    #[test]
    fn saturated_conversions_clamp() {
        assert_eq!(
            run(NumOp::I32TruncSatF32S, &[Value::F32(QNAN32)]).unwrap(),
            Value::I32(0)
        );
        assert_eq!(
            run(
                NumOp::I32TruncSatF32S,
                &[Value::F32(f32::INFINITY.to_bits())]
            )
            .unwrap(),
            Value::I32(i32::MAX)
        );
        assert_eq!(
            run(NumOp::I32TruncSatF64U, &[Value::F64((-1.5f64).to_bits())]).unwrap(),
            Value::I32(0)
        );
    }

    #[test]
    fn float_min_max_handle_nan_and_signed_zero() {
        assert_eq!(
            run(NumOp::F32Min, &[Value::F32(0), Value::F32(0x8000_0000)]).unwrap(),
            Value::F32(0x8000_0000)
        );
        assert_eq!(
            run(NumOp::F32Max, &[Value::F32(0), Value::F32(0x8000_0000)]).unwrap(),
            Value::F32(0)
        );
        assert_eq!(
            run(NumOp::F32Min, &[Value::F32(0), Value::F32(QNAN32)]).unwrap(),
            Value::F32(QNAN32)
        );
    }

    #[test]
    fn float_nearest_rounds_ties_to_even() {
        assert_eq!(
            run(NumOp::F32Nearest, &[Value::F32((2.5f32).to_bits())]).unwrap(),
            Value::F32((2.0f32).to_bits())
        );
        assert_eq!(
            run(NumOp::F32Nearest, &[Value::F32((3.5f32).to_bits())]).unwrap(),
            Value::F32((4.0f32).to_bits())
        );
        assert_eq!(
            run(NumOp::F64Nearest, &[Value::F64((-0.5f64).to_bits())]).unwrap(),
            Value::F64((-0.0f64).to_bits())
        );
    }

    #[test]
    fn neg_flips_the_nan_sign_bit_only() {
        assert_eq!(
            run(NumOp::F64Neg, &[Value::F64(QNAN64)]).unwrap(),
            Value::F64(QNAN64 ^ 0x8000_0000_0000_0000)
        );
    }
}
