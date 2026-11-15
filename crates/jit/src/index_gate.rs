//! The canonical-index key gate for the element read/store arms.
//!
//! `a[i]` and `a[i] = v` may serve an element buffer only when the key is a
//! canonical array index: an integral Number in `[0, 2^32-1)`. Both the
//! per-step lowerer (`compiler.rs`) and the optimizing tier (`opt_lower.rs`)
//! emit the gate at eight sites (dense read, dense store, append, typed read,
//! typed store); the arithmetic lives here for the same reason
//! [`crate::cells`] exists — two forks drift, and a drifted gate is a wrong
//! value, not a slow path.

use cranelift_codegen::ir::condcodes::FloatCC;
use cranelift_codegen::ir::{InstBuilder, Value, types};
use cranelift_frontend::FunctionBuilder;

/// The exclusive upper bound on a canonical array index (`2^32 - 1`).
pub(crate) const MAX_ARRAY_INDEX: f64 = 4294967295.0;

/// Whether `num` (the key's bits read as an f64) is a canonical array index;
/// returns the index as an `i64` and the verdict.
///
/// The proof is a round trip plus a bound: converting an integral double in
/// `[0, 2^63)` to `i64` and back is exact, so
/// `from_sint(to_sint_sat(num)) == num` holds only for an integral value.
/// Fractional, negative, huge and non-double keys are rejected — and a
/// NaN-boxed heap/undefined value bitcasts to a NaN, whose compare is false,
/// so no separate is-double block is needed. `-0.0` passes as index 0, which
/// is the canonicalization `ToPropertyKey` applies before a computed read.
///
/// The SIGNED saturating pair is deliberate: x86-64 has `cvttsd2si`
/// (`fcvt_to_sint_sat`) and `cvtsi2sd` (`fcvt_from_sint`) for the signed
/// conversions, while the unsigned forms have no instruction and Cranelift
/// expands each into a multi-branch software sequence. Measured on
/// `scratch/probe/elem_num.js`, the unsigned pair put both sequences in the
/// element read's address chain (~1.5x the row's time). The accepted set is
/// unchanged: the `>= 0` bound rejects what `ToUint64Sat` would have
/// saturated to 0, and both forms reject a value whose round trip differs.
pub(crate) fn canonical_index(builder: &mut FunctionBuilder, num: Value) -> (Value, Value) {
    let idx = builder.ins().fcvt_to_sint_sat(types::I64, num);
    let back = builder.ins().fcvt_from_sint(types::F64, idx);
    let integral = builder.ins().fcmp(FloatCC::Equal, back, num);
    let zero = builder.ins().f64const(0.0);
    let non_negative = builder.ins().fcmp(FloatCC::GreaterThanOrEqual, num, zero);
    let max = builder.ins().f64const(MAX_ARRAY_INDEX);
    let below_max = builder.ins().fcmp(FloatCC::LessThan, num, max);
    let in_range = builder.ins().band(non_negative, below_max);
    let ok = builder.ins().band(integral, in_range);
    (idx, ok)
}

/// The `i64` conversion of a value already proven an integral, non-negative
/// Number below `2^63` — a canonical index element-count, an `at()` argument,
/// or an array length. Single-instruction (`cvttsd2si`) where
/// `fcvt_to_uint_sat` would be a software sequence; saturating, so an
/// out-of-range value still never traps (it lands far above any real bound).
pub(crate) fn index_from_number(builder: &mut FunctionBuilder, num: Value) -> Value {
    builder.ins().fcvt_to_sint_sat(types::I64, num)
}

/// The `f64` of an `i64` count already known non-negative and below `2^63`
/// (a slots length). Single-instruction (`cvtsi2sd`) where `fcvt_from_uint`
/// would be a software sequence.
pub(crate) fn number_from_index(builder: &mut FunctionBuilder, index: Value) -> Value {
    builder.ins().fcvt_from_sint(types::F64, index)
}
