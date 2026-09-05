//! v128 vector semantics (spec ch. 4.3.5).
//!
//! A v128 is carried as a `u128` whose lane `i` occupies bytes
//! `i*size .. i*size+size` little-endian (lane 0 in the low bits), matching
//! the spec's little-endian lane addressing. Lane helpers below read/write
//! lanes by byte width without allocating.
//!
//! Wave 1 covers the const/splat/lane/memory/bitwise/boolean/shift/compare
//! families; arithmetic families land with their fixture files in later
//! waves, and unimplemented 0xfd subopcodes decode as unsupported (so their
//! modules count as *pending*, never as wrong).

/// Read lane `idx` of width `size` bytes as unsigned bits.
#[inline]
pub fn lane_bits(v: u128, idx: usize, size: usize) -> u64 {
    let shift = (idx * size) as u32 * 8;
    let mask = (1u128 << (size * 8)) - 1;
    ((v >> shift) & mask) as u64
}

/// Write `bits` into lane `idx` of width `size` bytes.
#[inline]
pub fn set_lane(v: u128, idx: usize, size: usize, bits: u64) -> u128 {
    let shift = (idx * size) as u32 * 8;
    let mask = (1u128 << (size * 8)) - 1;
    v & !(mask << shift) | ((bits as u128 & mask) << shift)
}

/// Lane width in bytes for a lane kind.
pub fn lane_bytes(kind: LaneKind) -> usize {
    match kind {
        LaneKind::I8 => 1,
        LaneKind::I16 => 2,
        LaneKind::I32 | LaneKind::F32 => 4,
        LaneKind::I64 | LaneKind::F64 => 8,
    }
}

/// How many lanes of a kind fit in 128 bits.
pub fn lane_count(kind: LaneKind) -> usize {
    16 / lane_bytes(kind)
}

/// The scalar category of a vector lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneKind {
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
}

/// Turn a scalar value into its bit pattern for a lane kind.
pub fn scalar_to_lane_bits(value: crate::values::Value, kind: LaneKind) -> Option<u64> {
    Some(match (kind, value) {
        (LaneKind::I8 | LaneKind::I16 | LaneKind::I32, crate::values::Value::I32(v)) => {
            v as u32 as u64
        }
        (LaneKind::I64, crate::values::Value::I64(v)) => v as u64,
        (LaneKind::F32, crate::values::Value::F32(bits)) => u64::from(bits),
        (LaneKind::F64, crate::values::Value::F64(bits)) => bits,
        _ => return None,
    })
}

/// Interpret `bits` (a lane's raw value) as a scalar value of `kind` (the
/// signed/unsigned distinction only matters for the integer extract forms).
pub fn lane_bits_to_value(bits: u64, kind: LaneKind) -> crate::values::Value {
    use crate::values::Value;
    match kind {
        LaneKind::I8 | LaneKind::I16 | LaneKind::I32 => Value::I32(bits as u32 as i32),
        LaneKind::I64 => Value::I64(bits as i64),
        LaneKind::F32 => Value::F32(bits as u32),
        LaneKind::F64 => Value::F64(bits),
    }
}

/// The operand/result shape of a pure-register v128 subopcode (a `Vec`
/// instruction). `Lane` shapes carry the scalar lane kind; the signed
/// extract forms are distinguished so validation knows the scalar result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VecSig {
    /// v128 -> v128 (`v128.not`, per-shape unary ops, conversions).
    Unop,
    /// v128 -> v128 (`v128.not`).
    Not,
    /// v128 v128 -> v128 (bitwise ops, comparisons, integer/fp arithmetic).
    Binop,
    /// v128 v128 v128 -> v128 (`v128.bitselect`).
    Ternop,
    /// v128 i32 -> v128 (integer shifts: the count is an i32 on top).
    Shift,
    /// scalar -> v128.
    Splat(LaneKind),
    /// v128 -> scalar; small-int lanes sign-extend (`.extract_lane_s`).
    ExtractS(LaneKind),
    /// v128 -> scalar; small-int lanes zero-extend (`.extract_lane_u`).
    ExtractU(LaneKind),
    /// v128 -> scalar (i32/i64/f32/f64 lanes: `.extract_lane`).
    Extract(LaneKind),
    /// v128 scalar -> v128 (`*.replace_lane`).
    Replace(LaneKind),
    /// v128 -> i32 (`v128.any_true`, `*.all_true`).
    Test,
    /// v128 -> i32 (`*.bitmask`).
    Bitmask,
}

/// The signature for a pure-register v128 subopcode. `None` means the
/// opcode is not (yet) implemented; the decoder keeps such modules
/// unsupported rather than mis-typed.
pub fn sig(sub: u16) -> Option<VecSig> {
    use LaneKind::*;
    Some(match sub {
        // v128.not.
        0x4d => VecSig::Not,
        // v128 bitwise (and/andnot/or/xor) + swizzle: binary.
        0x0e | 0x4e..=0x51 => VecSig::Binop,
        // v128.bitselect.
        0x52 => VecSig::Ternop,
        // v128.any_true + the integer *.all_true forms.
        0x53 | 0x63 | 0x83 | 0xa3 | 0xc3 => VecSig::Test,
        // Integer *.bitmask.
        0x64 | 0x84 | 0xa4 | 0xc4 => VecSig::Bitmask,
        // Splats.
        0x0f => VecSig::Splat(I8),
        0x10 => VecSig::Splat(I16),
        0x11 => VecSig::Splat(I32),
        0x12 => VecSig::Splat(I64),
        0x13 => VecSig::Splat(F32),
        0x14 => VecSig::Splat(F64),
        // Extract lane (small ints signed/unsigned; wider lanes single form).
        0x15 => VecSig::ExtractS(I8),
        0x16 => VecSig::ExtractU(I8),
        0x18 => VecSig::ExtractS(I16),
        0x19 => VecSig::ExtractU(I16),
        0x1b => VecSig::Extract(I32),
        0x1d => VecSig::Extract(I64),
        0x1f => VecSig::Extract(F32),
        0x21 => VecSig::Extract(F64),
        // Replace lane.
        0x17 => VecSig::Replace(I8),
        0x1a => VecSig::Replace(I16),
        0x1c => VecSig::Replace(I32),
        0x1e => VecSig::Replace(I64),
        0x20 => VecSig::Replace(F32),
        0x22 => VecSig::Replace(F64),
        // Integer shifts (v128 + i32 count).
        0x6b..=0x6d | 0x8b..=0x8d | 0xab..=0xad | 0xcb..=0xcd => VecSig::Shift,
        // Integer comparisons.
        0x23..=0x40 | 0xd6..=0xdb => VecSig::Binop,
        // Floating-point comparisons.
        0x41..=0x4c => VecSig::Binop,
        // ---- integer arithmetic (Cut 7b) ----
        // i8x16: abs/neg/popcnt, rounding-free unops.
        0x60..=0x62 => VecSig::Unop,
        // i8x16 saturating arithmetic, min/max, avgr: binary.
        0x6e..=0x73 | 0x76..=0x79 | 0x7b => VecSig::Binop,
        // i8x16.narrow_i16x8_s/u.
        0x65 | 0x66 => VecSig::Binop,
        // i16x8: abs/neg unops; sat/mul/min/max/avgr/q15mulr/narrow binary.
        0x80 | 0x81 => VecSig::Unop,
        0x82 | 0x85 | 0x86 | 0x8e..=0x93 | 0x95..=0x99 | 0x9b => VecSig::Binop,
        // i32x4: abs/neg unops; add/sub/mul/min/max binary.
        0xa0 | 0xa1 => VecSig::Unop,
        0xae | 0xb1 | 0xb5..=0xb9 => VecSig::Binop,
        // i32x4.dot_i16x8_s.
        0xba => VecSig::Binop,
        // i64x2: abs/neg unops; add/sub/mul binary.
        0xc0 | 0xc1 => VecSig::Unop,
        0xce | 0xd1 | 0xd5 => VecSig::Binop,
        // ---- shape-converting unary ops ----
        // i16x8.extend_low/high_i8x16_s/u, i32x4/i64x2 extend forms.
        0x87..=0x8a | 0xa7..=0xaa | 0xc7..=0xca => VecSig::Unop,
        // extadd_pairwise (i16x8 from i8x16, i32x4 from i16x8).
        0x7c..=0x7f => VecSig::Unop,
        // extmul low/high (i16x8/i32x4/i64x2 forms).
        0x9c..=0x9f | 0xbc..=0xbf | 0xdc..=0xdf => VecSig::Binop,
        // ---- float unary/binary ----
        // f32x4/f64x2 rounding and demote/promote/convert unops.
        0x5e | 0x5f | 0x67..=0x6a | 0x74 | 0x75 | 0x7a | 0x94 => VecSig::Unop,
        0xe0 | 0xe1 | 0xe3 | 0xec | 0xed | 0xef => VecSig::Unop,
        0xe4..=0xeb | 0xf0..=0xf7 => VecSig::Binop,
        // Float/int conversions (trunc_sat, convert).
        0xf8..=0xff => VecSig::Unop,
        _ => return None,
    })
}

/// The lane kind of an extract/replace immediate's opcode.
pub fn lane_kind_of(sub: u16) -> Option<LaneKind> {
    use LaneKind::*;
    Some(match sub {
        0x15..=0x17 => I8,
        0x18..=0x1a => I16,
        0x1b | 0x1c => I32,
        0x1d | 0x1e => I64,
        0x1f | 0x20 => F32,
        0x21 | 0x22 => F64,
        _ => return None,
    })
}

// ---- lane math kernels ----

/// Sign-extend a `size`-byte lane to an i64.
#[inline]
fn lane_signed(v: u128, idx: usize, size: usize) -> i64 {
    match size {
        1 => lane_bits(v, idx, 1) as u8 as i8 as i64,
        2 => lane_bits(v, idx, 2) as u16 as i16 as i64,
        4 => lane_bits(v, idx, 4) as u32 as i32 as i64,
        _ => lane_bits(v, idx, 8) as i64,
    }
}

/// All-ones mask of a `size`-byte lane, as a u64.
#[inline]
fn lane_ones(size: usize) -> u64 {
    if size == 8 {
        u64::MAX
    } else {
        (1u64 << (size * 8)) - 1
    }
}

/// Compare two lanes of an integer shape, returning the all-ones/all-zero
/// mask for the lane. `op` selects the relational operation.
fn int_cmp_lane(op: IntCmp, a: i64, b: i64, size: usize) -> u64 {
    use IntCmp::*;
    let signed = matches!(op, LtS | GtS | LeS | GeS);
    let result = if signed {
        match op {
            Eq => a == b,
            Ne => a != b,
            LtS => a < b,
            GtS => a > b,
            LeS => a <= b,
            GeS => a >= b,
            _ => unreachable!(),
        }
    } else {
        let (au, bu) = (a as u64, b as u64);
        match op {
            Eq => au == bu,
            Ne => au != bu,
            LtU => au < bu,
            GtU => au > bu,
            LeU => au <= bu,
            GeU => au >= bu,
            _ => unreachable!(),
        }
    };
    if result { lane_ones(size) } else { 0 }
}

#[derive(Clone, Copy)]
enum IntCmp {
    Eq,
    Ne,
    LtS,
    LtU,
    GtS,
    GtU,
    LeS,
    LeU,
    GeS,
    GeU,
}

/// Map a wave comparison subopcode to (integer shape size in bytes, op).
fn int_cmp(sub: u16) -> Option<(usize, IntCmp)> {
    use IntCmp::*;
    if (0xd6..=0xdb).contains(&sub) {
        // i64x2 has no unsigned comparisons: eq/ne/lt_s/gt_s/le_s/ge_s.
        let ops = [Eq, Ne, LtS, GtS, LeS, GeS];
        let op = *ops.get((sub - 0xd6) as usize)?;
        return Some((8usize, op));
    }
    let (size, index) = match sub {
        0x23..=0x2c => (1usize, sub - 0x23),
        0x2d..=0x36 => (2usize, sub - 0x2d),
        0x37..=0x40 => (4usize, sub - 0x37),
        _ => return None,
    };
    let ops = [Eq, Ne, LtS, LtU, GtS, GtU, LeS, LeU, GeS, GeU];
    ops.get(index as usize).map(|op| (size, *op))
}

/// Integer add/sub (wrapping within the lane width).
fn int_add_sub(sub: u16) -> Option<usize> {
    match sub {
        0x6e | 0x71 => Some(1),
        0x8e | 0x91 => Some(2),
        0xae | 0xb1 => Some(4),
        0xce | 0xd1 => Some(8),
        _ => None,
    }
}

/// Integer shift: which shape and direction a subopcode selects.
fn int_shift(sub: u16) -> Option<(usize, bool)> {
    // (size bytes, arithmetic right shift?)
    match sub {
        0x6b..=0x6d => Some((1, sub == 0x6c)),
        0x8b..=0x8d => Some((2, sub == 0x8c)),
        0xab..=0xad => Some((4, sub == 0xac)),
        0xcb..=0xcd => Some((8, sub == 0xcc)),
        _ => None,
    }
}

fn fp_cmp(sub: u16) -> Option<(usize, FpCmp)> {
    use FpCmp::*;
    let (size, index) = match sub {
        0x41..=0x46 => (4usize, sub - 0x41),
        0x47..=0x4c => (8usize, sub - 0x47),
        _ => return None,
    };
    let ops = [Eq, Ne, Lt, Gt, Le, Ge];
    ops.get(index as usize).map(|op| (size, *op))
}

#[derive(Clone, Copy)]
enum FpCmp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

/// Execute a binary vector op (two v128 operands, one v128 result). Returns
/// `None` when the subopcode is not implemented.
pub fn exec_binop(sub: u16, a: u128, b: u128) -> Option<u128> {
    match sub {
        // v128.and / andnot / or / xor.
        0x4e => return Some(a & b),
        0x4f => return Some(a & !b),
        0x50 => return Some(a | b),
        0x51 => return Some(a ^ b),
        // i8x16.swizzle: select bytes of `a` by lane index bytes of `b`.
        0x0e => {
            let mut out = 0u128;
            for i in 0..16 {
                let index = lane_bits(b, i, 1) as usize;
                out = set_lane(
                    out,
                    i,
                    1,
                    if index < 16 {
                        lane_bits(a, index, 1)
                    } else {
                        0
                    },
                );
            }
            return Some(out);
        }
        // Integer comparisons.
        _ if int_cmp(sub).is_some() => {
            let (size, op) = int_cmp(sub)?;
            let mut out = 0u128;
            for i in 0..(16 / size) {
                out = set_lane(
                    out,
                    i,
                    size,
                    int_cmp_lane(op, lane_signed(a, i, size), lane_signed(b, i, size), size),
                );
            }
            return Some(out);
        }
        // Floating-point comparisons.
        _ if fp_cmp(sub).is_some() => {
            let (size, op) = fp_cmp(sub)?;
            let mut out = 0u128;
            for i in 0..(16 / size) {
                let result = fp_cmp_lane(size, op, lane_bits(a, i, size), lane_bits(b, i, size));
                out = set_lane(out, i, size, if result { lane_ones(size) } else { 0 });
            }
            return Some(out);
        }
        _ => {}
    }
    // Integer add/sub/mul/saturating/min/max/avgr/q15mulr/narrow/extmul/dot.
    if let Some(out) = exec_int_binop(sub, a, b) {
        return Some(out);
    }
    // Floating-point arithmetic (add/sub/mul/div/min/max/pmin/pmax).
    fp_arith_binop(sub, a, b)
}

/// The integer two-vector arithmetic beyond plain add/sub (which the caller
/// handles above).
fn exec_int_binop(sub: u16, a: u128, b: u128) -> Option<u128> {
    // Plain wrapping add/sub.
    if int_add_sub(sub).is_some() {
        let size = int_add_sub(sub)?;
        let add = matches!(sub, 0x6e | 0x8e | 0xae | 0xce);
        let mut out = 0u128;
        for i in 0..(16 / size) {
            let x = lane_signed(a, i, size);
            let y = lane_signed(b, i, size);
            let r = if add {
                x.wrapping_add(y)
            } else {
                x.wrapping_sub(y)
            };
            out = set_lane(out, i, size, r as u64);
        }
        return Some(out);
    }
    let sat = match sub {
        // i8x16 saturating arithmetic.
        0x6f => Some((1usize, true, true)),
        0x70 => Some((1, false, true)),
        0x72 => Some((1, true, false)),
        0x73 => Some((1, false, false)),
        // i16x8 saturating arithmetic.
        0x8f => Some((2usize, true, true)),
        0x90 => Some((2, false, true)),
        0x92 => Some((2, true, false)),
        0x93 => Some((2, false, false)),
        _ => None,
    };
    if let Some((size, signed, add)) = sat {
        let mut out = 0u128;
        for i in 0..(16 / size) {
            let x = if signed {
                lane_signed(a, i, size) as i128
            } else {
                lane_bits(a, i, size) as i128
            };
            let y = if signed {
                lane_signed(b, i, size) as i128
            } else {
                lane_bits(b, i, size) as i128
            };
            let sum = if add { x + y } else { x - y };
            let (lo, hi) = match (signed, size) {
                (true, 1) => (-128i128, 127i128),
                (false, 1) => (0, 255),
                (true, 2) => (-32768, 32767),
                (false, 2) => (0, 65535),
                _ => return None,
            };
            let clamped = sum.clamp(lo, hi);
            out = set_lane(out, i, size, clamped as u64);
        }
        return Some(out);
    }
    // Unsigned average (avgr_u): (a + b + 1) >> 1 per lane.
    if matches!(sub, 0x7b | 0x9b) {
        let size = if sub == 0x7b { 1usize } else { 2 };
        let mut out = 0u128;
        for i in 0..(16 / size) {
            let x = lane_bits(a, i, size) as u128;
            let y = lane_bits(b, i, size) as u128;
            let r = ((x + y + 1) >> 1) as u64;
            out = set_lane(out, i, size, r);
        }
        return Some(out);
    }
    // q15mulr_sat_s: (a*b + 0x4000) >> 15, saturated to i16.
    if sub == 0x82 {
        let mut out = 0u128;
        for i in 0..8 {
            let x = lane_signed(a, i, 2);
            let y = lane_signed(b, i, 2);
            let product = x * y;
            let mut r = (product + 0x4000) >> 15;
            r = r.clamp(-32768, 32767);
            out = set_lane(out, i, 2, r as u64);
        }
        return Some(out);
    }
    // Narrowing: wider signed lanes -> narrower saturated lanes.
    let narrow = match sub {
        0x65 | 0x66 => Some((2usize, 1usize)),
        0x85 | 0x86 => Some((4usize, 2usize)),
        _ => None,
    };
    if let Some((in_size, out_size)) = narrow {
        let signed = matches!(sub, 0x65 | 0x85);
        let mut out = 0u128;
        let in_lanes = 16 / in_size;
        for i in 0..in_lanes {
            let r = narrow_clamp(lane_signed(a, i, in_size), signed, out_size);
            let s = narrow_clamp(lane_signed(b, i, in_size), signed, out_size);
            out = set_lane(out, i, out_size, r as u64);
            out = set_lane(out, i + in_lanes, out_size, s as u64);
        }
        return Some(out);
    }
    // min/max (signed/unsigned).
    let minmax = match sub {
        0x76..=0x79 => Some((1usize, (sub & 1) != 0)),
        0x96..=0x99 => Some((2usize, (sub & 1) != 0)),
        0xb6..=0xb9 => Some((4usize, (sub & 1) != 0)),
        _ => None,
    };
    if let Some((size, unsigned)) = minmax {
        let is_max = matches!(sub, 0x78 | 0x79 | 0x98 | 0x99 | 0xb8 | 0xb9);
        let mut out = 0u128;
        for i in 0..(16 / size) {
            let (x, y) = if unsigned {
                (lane_bits(a, i, size), lane_bits(b, i, size))
            } else {
                (
                    lane_signed(a, i, size) as u64,
                    lane_signed(b, i, size) as u64,
                )
            };
            let choose = if unsigned {
                if is_max { x >= y } else { x <= y }
            } else {
                let (xs, ys) = (x as i64, y as i64);
                if is_max { xs >= ys } else { xs <= ys }
            };
            out = set_lane(out, i, size, if choose { x } else { y });
        }
        return Some(out);
    }
    // Multiplies (wrapping, per lane width) for i16x8/i32x4/i64x2.
    let mul = match sub {
        0x95 => Some(2usize),
        0xb5 => Some(4),
        0xd5 => Some(8),
        _ => None,
    };
    if let Some(size) = mul {
        let mut out = 0u128;
        for i in 0..(16 / size) {
            let x = lane_signed(a, i, size);
            let y = lane_signed(b, i, size);
            out = set_lane(out, i, size, x.wrapping_mul(y) as u64);
        }
        return Some(out);
    }
    // Extending multiplies: low/high halves of the narrow operands,
    // sign/zero-extended and multiplied into the wide lanes.
    let extmul = match sub {
        0x9c..=0x9f => Some((1usize, 2usize)),
        0xbc..=0xbf => Some((2usize, 4usize)),
        0xdc..=0xdf => Some((4usize, 8usize)),
        _ => None,
    };
    if let Some((in_size, out_size)) = extmul {
        let signed = matches!(sub, 0x9c | 0x9d | 0xbc | 0xbd | 0xdc | 0xdd);
        let high = matches!(sub, 0x9d | 0x9f | 0xbd | 0xbf | 0xdd | 0xdf);
        let out_lanes = 16 / out_size;
        let mut out = 0u128;
        for i in 0..out_lanes {
            let j = if high { i + out_lanes } else { i };
            let x = widen(lane_bits(a, j, in_size), signed, in_size);
            let y = widen(lane_bits(b, j, in_size), signed, in_size);
            out = set_lane(out, i, out_size, x.wrapping_mul(y) as u64);
        }
        return Some(out);
    }
    // i32x4.dot_i16x8_s.
    if sub == 0xba {
        let mut out = 0u128;
        for i in 0..4 {
            let x0 = lane_signed(a, 2 * i, 2) * lane_signed(b, 2 * i, 2);
            let x1 = lane_signed(a, 2 * i + 1, 2) * lane_signed(b, 2 * i + 1, 2);
            out = set_lane(out, i, 4, (x0 + x1) as u64);
        }
        return Some(out);
    }
    None
}

/// Saturate a value into the narrower lane width (signed or unsigned).
fn narrow_clamp(x: i64, signed: bool, size: usize) -> i64 {
    match (signed, size) {
        (true, 1) => x.clamp(-128, 127),
        (false, 1) => x.clamp(0, 255),
        (true, 2) => x.clamp(-32768, 32767),
        (false, 2) => x.clamp(0, 65535),
        _ => x,
    }
}

/// Sign- or zero-extend a raw lane to i64 for a widening operation.
fn widen(bits: u64, signed: bool, size: usize) -> i64 {
    if signed {
        match size {
            1 => bits as u8 as i8 as i64,
            2 => bits as u16 as i16 as i64,
            _ => bits as u32 as i32 as i64,
        }
    } else {
        bits as i64
    }
}

/// One float comparison lane.
fn fp_cmp_lane(size: usize, op: FpCmp, bits_a: u64, bits_b: u64) -> bool {
    if size == 4 {
        fp_cmp_lane32(
            op,
            f32::from_bits(bits_a as u32),
            f32::from_bits(bits_b as u32),
        )
    } else {
        fp_cmp_lane64(op, f64::from_bits(bits_a), f64::from_bits(bits_b))
    }
}

/// `bits_a < bits_b` interpreted as floats of `size` bytes.
#[inline]
fn fp_lt(size: usize, bits_a: u64, bits_b: u64) -> bool {
    if size == 4 {
        f32::from_bits(bits_a as u32) < f32::from_bits(bits_b as u32)
    } else {
        f64::from_bits(bits_a) < f64::from_bits(bits_b)
    }
}

/// Floating-point arithmetic (spec 4.3.5): add/sub/mul/div/min/max and the
/// pseudo-min/max `pmin`/`pmax`. Arithmetic lanes reuse the scalar engine's
/// `values::exec_num` so NaN canonicalization matches the scalar semantics.
fn fp_arith_binop(sub: u16, a: u128, b: u128) -> Option<u128> {
    use crate::instr::NumOp;
    use crate::values::{Value, exec_num};
    // pmin = (b < a) ? b : a ; pmax = (a < b) ? b : a.
    if matches!(sub, 0xea | 0xeb | 0xf6 | 0xf7) {
        let size = if matches!(sub, 0xea | 0xeb) {
            4usize
        } else {
            8
        };
        let pmin = matches!(sub, 0xea | 0xf6);
        let mut out = 0u128;
        for i in 0..(16 / size) {
            let x = lane_bits(a, i, size);
            let y = lane_bits(b, i, size);
            let chosen = if pmin {
                if fp_lt(size, y, x) { y } else { x }
            } else if fp_lt(size, x, y) {
                y
            } else {
                x
            };
            out = set_lane(out, i, size, chosen);
        }
        return Some(out);
    }
    let (size, op) = match sub {
        0xe4 => (4usize, NumOp::F32Add),
        0xe5 => (4, NumOp::F32Sub),
        0xe6 => (4, NumOp::F32Mul),
        0xe7 => (4, NumOp::F32Div),
        0xe8 => (4, NumOp::F32Min),
        0xe9 => (4, NumOp::F32Max),
        0xf0 => (8, NumOp::F64Add),
        0xf1 => (8, NumOp::F64Sub),
        0xf2 => (8, NumOp::F64Mul),
        0xf3 => (8, NumOp::F64Div),
        0xf4 => (8, NumOp::F64Min),
        0xf5 => (8, NumOp::F64Max),
        _ => return None,
    };
    let mut out = 0u128;
    for i in 0..(16 / size) {
        let x = if size == 4 {
            Value::F32(lane_bits(a, i, size) as u32)
        } else {
            Value::F64(lane_bits(a, i, size))
        };
        let y = if size == 4 {
            Value::F32(lane_bits(b, i, size) as u32)
        } else {
            Value::F64(lane_bits(b, i, size))
        };
        let result = exec_num(op, &[x, y]).ok()?;
        let bits = match result {
            Value::F32(bits) => u64::from(bits),
            Value::F64(bits) => bits,
            _ => return None,
        };
        out = set_lane(out, i, size, bits);
    }
    Some(out)
}

#[inline]
fn fp_cmp_lane32(op: FpCmp, x: f32, y: f32) -> bool {
    use FpCmp::*;
    match op {
        Eq => x == y,
        Ne => x != y,
        Lt => x < y,
        Gt => x > y,
        Le => x <= y,
        Ge => x >= y,
    }
}

#[inline]
fn fp_cmp_lane64(op: FpCmp, x: f64, y: f64) -> bool {
    use FpCmp::*;
    match op {
        Eq => x == y,
        Ne => x != y,
        Lt => x < y,
        Gt => x > y,
        Le => x <= y,
        Ge => x >= y,
    }
}

/// `v128.bitselect`: per bit, `(a & c) | (b & !c)`.
pub fn exec_bitselect(a: u128, b: u128, c: u128) -> u128 {
    (a & c) | (b & !c)
}

/// Integer lane shift (logical left/right or arithmetic right), masking the
/// count to the lane width.
pub fn exec_shift(sub: u16, v: u128, count: u32) -> Option<u128> {
    let (size, shr_s) = int_shift(sub)?;
    let lane_bits_n = (size * 8) as u32;
    let shift = count % lane_bits_n;
    let right = shr_s || matches!(sub, 0x6d | 0x8d | 0xad | 0xcd);
    let mut out = 0u128;
    for i in 0..(16 / size) {
        let bits = lane_bits(v, i, size);
        let r = if !right {
            (bits << shift) & ((1u128 << (size * 8)) - 1) as u64
        } else if shr_s {
            // Arithmetic shift: propagate the sign bit.
            let signed = lane_signed(v, i, size);
            let shifted = signed >> shift;
            shifted as u64
        } else {
            bits >> shift
        };
        out = set_lane(out, i, size, r);
    }
    Some(out)
}

/// `v128.not`.
pub fn exec_not(v: u128) -> u128 {
    !v
}

/// Splat a scalar lane's bits across a shape.
pub fn exec_splat(sub: u16, bits: u64) -> Option<u128> {
    let kind = match sub {
        0x0f => LaneKind::I8,
        0x10 => LaneKind::I16,
        0x11 => LaneKind::I32,
        0x12 => LaneKind::I64,
        0x13 => LaneKind::F32,
        0x14 => LaneKind::F64,
        _ => return None,
    };
    let size = lane_bytes(kind);
    let count = lane_count(kind);
    let mask = (1u128 << (size * 8)) - 1;
    let lane = (bits as u128) & mask;
    let mut out = 0u128;
    for i in 0..count {
        out |= lane << (i * size * 8);
    }
    Some(out)
}

/// Extract lane `lane` of a v128 as a scalar value.
pub fn exec_extract(sub: u16, v: u128, lane: usize) -> Option<crate::values::Value> {
    use crate::values::Value;
    let kind = lane_kind_of(sub)?;
    if lane >= lane_count(kind) {
        return None;
    }
    Some(match sub {
        0x15 => Value::I32(lane_bits(v, lane, 1) as u8 as i8 as i32),
        0x16 => Value::I32(lane_bits(v, lane, 1) as u8 as i32),
        0x18 => Value::I32(lane_bits(v, lane, 2) as u16 as i16 as i32),
        0x19 => Value::I32(lane_bits(v, lane, 2) as u16 as i32),
        0x1b => Value::I32(lane_bits(v, lane, 4) as u32 as i32),
        0x1d => Value::I64(lane_bits(v, lane, 8) as i64),
        0x1f => Value::F32(lane_bits(v, lane, 4) as u32),
        0x21 => Value::F64(lane_bits(v, lane, 8)),
        _ => return None,
    })
}

/// Replace lane `lane` of `v` with the scalar `bits`.
pub fn exec_replace(sub: u16, v: u128, lane: usize, bits: u64) -> Option<u128> {
    let kind = lane_kind_of(sub)?;
    let size = lane_bytes(kind);
    if lane >= lane_count(kind) {
        return None;
    }
    Some(set_lane(v, lane, size, bits))
}

/// `v128.any_true` and the per-shape `*.all_true`: nonzero lanes.
pub fn exec_any_all_true(sub: u16, v: u128) -> Option<i32> {
    if sub == 0x53 {
        return Some(if v == 0 { 0 } else { 1 });
    }
    let size = match sub {
        0x63 => 1usize,
        0x83 => 2,
        0xa3 => 4,
        0xc3 => 8,
        _ => return None,
    };
    let mut all = true;
    for i in 0..(16 / size) {
        all &= lane_bits(v, i, size) != 0;
    }
    Some(if all { 1 } else { 0 })
}

/// Per-shape `*.bitmask`: bit `i` is the sign bit of lane `i`.
pub fn exec_bitmask(sub: u16, v: u128) -> Option<i32> {
    let size = match sub {
        0x64 => 1usize,
        0x84 => 2,
        0xa4 => 4,
        0xc4 => 8,
        _ => return None,
    };
    let mut mask = 0u32;
    for i in 0..(16 / size) {
        if lane_signed(v, i, size) < 0 {
            mask |= 1 << i;
        }
    }
    Some(mask as i32)
}

// ---- unary / converting ops (Cut 7b) ----

/// Integer abs/neg/popcnt (wrapping within the lane width).
pub fn exec_int_unop(sub: u16, v: u128) -> Option<u128> {
    let (size, kind) = match sub {
        0x60 => (1usize, 0u8), // abs
        0x61 => (1, 1),        // neg
        0x62 => (1, 2),        // popcnt
        0x80 => (2, 0),
        0x81 => (2, 1),
        0xa0 => (4, 0),
        0xa1 => (4, 1),
        0xc0 => (8, 0),
        0xc1 => (8, 1),
        _ => return None,
    };
    let mut out = 0u128;
    for i in 0..(16 / size) {
        let bits = lane_bits(v, i, size);
        let r = match kind {
            0 => lane_signed(v, i, size).wrapping_abs() as u64,
            1 => lane_signed(v, i, size).wrapping_neg() as u64,
            _ => bits.count_ones() as u64,
        };
        out = set_lane(out, i, size, r);
    }
    Some(out)
}

/// Sign/zero-extend the low or high half of a vector into twice-as-wide
/// lanes (spec 2.4.8 extend ops). `signed` selects the extension; `high`
/// selects the upper half of the source lanes.
fn exec_extend(v: u128, in_size: usize, signed: bool, high: bool) -> u128 {
    let out_size = 2 * in_size;
    let out_lanes = 16 / out_size;
    let mut out = 0u128;
    for i in 0..out_lanes {
        let j = if high { i + out_lanes } else { i };
        let widened = widen(lane_bits(v, j, in_size), signed, in_size);
        out = set_lane(out, i, out_size, widened as u64);
    }
    out
}

/// Pairwise widening add (spec 2.4.8): adjacent narrow lanes are summed into
/// the half-as-many wide lanes.
pub fn exec_extadd_pairwise(sub: u16, v: u128) -> Option<u128> {
    let (in_size, signed) = match sub {
        0x7c => (1usize, true),
        0x7d => (1, false),
        0x7e => (2, true),
        0x7f => (2, false),
        _ => return None,
    };
    let out_size = 2 * in_size;
    let out_lanes = 16 / out_size;
    let mut out = 0u128;
    for i in 0..out_lanes {
        let a = widen(lane_bits(v, 2 * i, in_size), signed, in_size);
        let b = widen(lane_bits(v, 2 * i + 1, in_size), signed, in_size);
        out = set_lane(out, i, out_size, (a + b) as u64);
    }
    Some(out)
}

/// Execute a v128 unary vector op (one v128 operand, one v128 result).
pub fn exec_unop(sub: u16, v: u128) -> Option<u128> {
    use crate::instr::NumOp;
    use crate::values::{Value, exec_num};
    // Integer abs/neg/popcnt.
    if matches!(sub, 0x60..=0x62 | 0x80 | 0x81 | 0xa0 | 0xa1 | 0xc0 | 0xc1) {
        return exec_int_unop(sub, v);
    }
    // Widening extends (i8->i16, i16->i32, i32->i64), low/high, s/u.
    if let Some((in_size, signed, high)) = match sub {
        0x87 => Some((1usize, true, false)),
        0x88 => Some((1, true, true)),
        0x89 => Some((1, false, false)),
        0x8a => Some((1, false, true)),
        0xa7 => Some((2, true, false)),
        0xa8 => Some((2, true, true)),
        0xa9 => Some((2, false, false)),
        0xaa => Some((2, false, true)),
        0xc7 => Some((4, true, false)),
        0xc8 => Some((4, true, true)),
        0xc9 => Some((4, false, false)),
        0xca => Some((4, false, true)),
        _ => None,
    } {
        return Some(exec_extend(v, in_size, signed, high));
    }
    // Pairwise widening adds.
    if let Some(out) = exec_extadd_pairwise(sub, v) {
        return Some(out);
    }
    // Floating-point unary ops per lane (spec 4.3.5), through the scalar
    // engine for consistent NaN handling.
    let (size, op) = match sub {
        0xe0 => (4usize, NumOp::F32Abs),
        0xe1 => (4, NumOp::F32Neg),
        0xe3 => (4, NumOp::F32Sqrt),
        0x67 => (4, NumOp::F32Ceil),
        0x68 => (4, NumOp::F32Floor),
        0x69 => (4, NumOp::F32Trunc),
        0x6a => (4, NumOp::F32Nearest),
        0xec => (8, NumOp::F64Abs),
        0xed => (8, NumOp::F64Neg),
        0xef => (8, NumOp::F64Sqrt),
        0x74 => (8, NumOp::F64Ceil),
        0x75 => (8, NumOp::F64Floor),
        0x7a => (8, NumOp::F64Trunc),
        0x94 => (8, NumOp::F64Nearest),
        _ => {
            // Conversions (see below).
            return exec_conversion(sub, v);
        }
    };
    let mut out = 0u128;
    for i in 0..(16 / size) {
        let x = if size == 4 {
            Value::F32(lane_bits(v, i, size) as u32)
        } else {
            Value::F64(lane_bits(v, i, size))
        };
        let result = exec_num(op, &[x]).ok()?;
        let bits = match result {
            Value::F32(bits) => u64::from(bits),
            Value::F64(bits) => bits,
            _ => return None,
        };
        out = set_lane(out, i, size, bits);
    }
    Some(out)
}

/// Shape/lane conversions: trunc_sat, int<->float, demote/promote.
fn exec_conversion(sub: u16, v: u128) -> Option<u128> {
    use crate::instr::NumOp;
    use crate::values::{Value, exec_num};
    match sub {
        // i32x4.trunc_sat_f32x4_s/u: four f32 lanes -> four i32 lanes.
        0xf8 | 0xf9 => {
            let op = if sub == 0xf8 {
                NumOp::I32TruncSatF32S
            } else {
                NumOp::I32TruncSatF32U
            };
            let mut out = 0u128;
            for i in 0..4 {
                let input = Value::F32(lane_bits(v, i, 4) as u32);
                let result = exec_num(op, &[input]).ok()?;
                let Value::I32(x) = result else { return None };
                out = set_lane(out, i, 4, x as u64);
            }
            Some(out)
        }
        // i32x4.trunc_sat_f64x2_s/u_zero: two f64 lanes -> low two i32 lanes.
        0xfc | 0xfd => {
            let op = if sub == 0xfc {
                NumOp::I32TruncSatF64S
            } else {
                NumOp::I32TruncSatF64U
            };
            let mut out = 0u128;
            for i in 0..2 {
                let input = Value::F64(lane_bits(v, i, 8));
                let result = exec_num(op, &[input]).ok()?;
                let Value::I32(x) = result else { return None };
                out = set_lane(out, i, 4, x as u64);
            }
            Some(out)
        }
        // f32x4.convert_i32x4_s/u: four i32 lanes -> four f32 lanes.
        0xfa | 0xfb => {
            let op = if sub == 0xfa {
                NumOp::F32ConvertI32S
            } else {
                NumOp::F32ConvertI32U
            };
            let mut out = 0u128;
            for i in 0..4 {
                let input = Value::I32(lane_signed(v, i, 4) as i32);
                let result = exec_num(op, &[input]).ok()?;
                let Value::F32(bits) = result else {
                    return None;
                };
                out = set_lane(out, i, 4, u64::from(bits));
            }
            Some(out)
        }
        // f64x2.convert_low_i32x4_s/u: two i32 lanes -> two f64 lanes.
        0xfe | 0xff => {
            let op = if sub == 0xfe {
                NumOp::F64ConvertI32S
            } else {
                NumOp::F64ConvertI32U
            };
            let mut out = 0u128;
            for i in 0..2 {
                let input = Value::I32(lane_signed(v, i, 4) as i32);
                let result = exec_num(op, &[input]).ok()?;
                let Value::F64(bits) = result else {
                    return None;
                };
                out = set_lane(out, i, 8, bits);
            }
            Some(out)
        }
        // f32x4.demote_f64x2_zero: two f64 lanes -> low two f32 lanes.
        0x5e => {
            let mut out = 0u128;
            for i in 0..2 {
                let input = Value::F64(lane_bits(v, i, 8));
                let result = exec_num(NumOp::F32DemoteF64, &[input]).ok()?;
                let Value::F32(bits) = result else {
                    return None;
                };
                out = set_lane(out, i, 4, u64::from(bits));
            }
            Some(out)
        }
        // f64x2.promote_low_f32x4: two f32 lanes -> two f64 lanes.
        0x5f => {
            let mut out = 0u128;
            for i in 0..2 {
                let input = Value::F32(lane_bits(v, i, 4) as u32);
                let result = exec_num(NumOp::F64PromoteF32, &[input]).ok()?;
                let Value::F64(bits) = result else {
                    return None;
                };
                out = set_lane(out, i, 8, bits);
            }
            Some(out)
        }
        _ => None,
    }
}
