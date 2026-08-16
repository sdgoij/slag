//! Number algorithms: `Number::toString` and arithmetic (spec 6.1.6.1).

use crate::convert::{to_int32, to_uint32};
use crate::string::JsString;

/// The shortest round-trip decimal digits of a finite non-zero `x`, as
/// `(digits, n)` with `x = s × 10^(n − k)` and `k = digits.len()`, matching
/// the `(n, k, s)` triple of spec `Number::toString`.
fn shortest_digits(x: f64) -> (Vec<u8>, i32) {
    debug_assert!(x.is_finite() && x != 0.0);
    let mut buffer = ryu::Buffer::new();
    let text = buffer.format(x.abs());
    parse_shortest(text)
}

/// Parses ryu's shortest representation (Rust Display style) into `(digits, n)`.
fn parse_shortest(text: &str) -> (Vec<u8>, i32) {
    let (mantissa, exponent) = match text.split_once('e').or_else(|| text.split_once('E')) {
        Some((m, e)) => (m, e.parse::<i32>().unwrap_or(0)),
        None => (text, 0),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mantissa, ""),
    };
    let digits = format!("{int_part}{frac_part}");
    let Some(first_nonzero) = digits.find(|c| c != '0') else {
        return (vec![b'0'], 1);
    };
    let last_nonzero = digits.rfind(|c| c != '0').unwrap_or(first_nonzero);
    // Trailing zeros are ryu's "0.0" formatting artifact, never significant.
    let significant = &digits[first_nonzero..=last_nonzero];
    let n = int_part.len() as i32 - first_nonzero as i32 + exponent;
    (significant.as_bytes().to_vec(), n)
}

/// spec 6.1.6.1.20 Number::toString(x).
pub fn to_string(x: f64) -> JsString {
    if x.is_nan() {
        return JsString::from_utf8("NaN");
    }
    if x == 0.0 {
        return JsString::from_utf8("0");
    }
    if x.is_infinite() {
        return JsString::from_utf8(if x < 0.0 { "-Infinity" } else { "Infinity" });
    }
    let mut out = String::new();
    if x < 0.0 {
        out.push('-');
    }
    let (digits, n) = shortest_digits(x);
    let s: String = digits.iter().map(|d| char::from(*d)).collect();
    let k = digits.len() as i32;
    if k <= n && n <= 21 {
        // Step 7: integer-valued, no fractional part in decimal notation.
        out.push_str(&s);
        out.extend(std::iter::repeat_n('0', (n - k) as usize));
    } else if 0 < n && n <= 21 {
        // Step 8: decimal point inside the digits.
        let n = n as usize;
        out.push_str(&s[..n]);
        out.push('.');
        out.push_str(&s[n..]);
    } else if -6 < n && n <= 0 {
        // Step 9: leading fractional zeros.
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-n) as usize));
        out.push_str(&s);
    } else {
        // Steps 10-11: exponential notation with an explicit sign.
        let exponent = n - 1;
        if k == 1 {
            out.push_str(&s);
        } else {
            out.push_str(&s[..1]);
            out.push('.');
            out.push_str(&s[1..]);
        }
        out.push('e');
        out.push(if exponent < 0 { '-' } else { '+' });
        out.push_str(&exponent.abs().to_string());
    }
    JsString::from_utf8(&out)
}

/// spec 6.1.6.1.1 Number::add.
pub fn add(a: f64, b: f64) -> f64 {
    a + b
}

/// spec 6.1.6.1.2 Number::subtract.
pub fn subtract(a: f64, b: f64) -> f64 {
    a - b
}

/// spec 6.1.6.1.3 Number::multiply.
pub fn multiply(a: f64, b: f64) -> f64 {
    a * b
}

/// spec 6.1.6.1.4 Number::divide.
pub fn divide(a: f64, b: f64) -> f64 {
    a / b
}

/// spec 6.1.6.1.5 Number::remainder — sign of the dividend; `x % 0` is NaN.
pub fn remainder(a: f64, b: f64) -> f64 {
    a % b
}

/// spec 6.1.6.1.6 Number::exponentiate, with the NaN/±0/infinite special cases.
pub fn exponentiate(base: f64, exponent: f64) -> f64 {
    if exponent.is_nan() {
        return f64::NAN;
    }
    if exponent == 0.0 {
        return 1.0;
    }
    if (base == 1.0 || base == -1.0) && exponent.is_infinite() {
        return f64::NAN;
    }
    base.powf(exponent)
}

/// spec 6.1.6.1.7 Number::unaryMinus.
pub fn unary_minus(x: f64) -> f64 {
    -x
}

/// spec 6.1.6.1.11 Number::bitwiseNOT.
pub fn bitwise_not(x: f64) -> f64 {
    (!to_int32(x)) as f64
}

/// spec 6.1.6.1.8 Number::leftShift.
pub fn left_shift(x: f64, shift: f64) -> f64 {
    (to_int32(x).wrapping_shl(to_uint32(shift) & 0x1F)) as f64
}

/// spec 6.1.6.1.9 Number::signedRightShift.
pub fn signed_right_shift(x: f64, shift: f64) -> f64 {
    (to_int32(x).wrapping_shr(to_uint32(shift) & 0x1F)) as f64
}

/// spec 6.1.6.1.10 Number::unsignedRightShift.
pub fn unsigned_right_shift(x: f64, shift: f64) -> f64 {
    (to_uint32(x).wrapping_shr(to_uint32(shift) & 0x1F)) as f64
}

/// spec 6.1.6.1.12 Number::bitwiseAND.
pub fn bitwise_and(x: f64, y: f64) -> f64 {
    (to_int32(x) & to_int32(y)) as f64
}

/// spec 6.1.6.1.13 Number::bitwiseOR.
pub fn bitwise_or(x: f64, y: f64) -> f64 {
    (to_int32(x) | to_int32(y)) as f64
}

/// spec 6.1.6.1.14 Number::bitwiseXOR.
pub fn bitwise_xor(x: f64, y: f64) -> f64 {
    (to_int32(x) ^ to_int32(y)) as f64
}

/// spec 6.1.6.1.15 Number::equal — NaN is not equal to itself.
pub fn equal(a: f64, b: f64) -> bool {
    a == b
}

/// spec 6.1.6.1.16 Number::lessThan — `undefined` (None) when either operand
/// is NaN.
pub fn less_than(a: f64, b: f64) -> Option<bool> {
    if a.is_nan() || b.is_nan() {
        None
    } else {
        Some(a < b)
    }
}

/// A minimal little-endian big unsigned integer, used for exact digit
/// generation in the Number formatting algorithms (every f64 is a dyadic
/// rational, and its expansions in base 10 / powers of 2 are computable
/// exactly).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BigU {
    limbs: Vec<u64>,
}

impl BigU {
    pub fn zero() -> Self {
        Self::default()
    }

    pub fn from_u64(value: u64) -> Self {
        if value == 0 {
            Self::zero()
        } else {
            Self { limbs: vec![value] }
        }
    }

    pub fn is_zero(&self) -> bool {
        self.limbs.iter().all(|limb| *limb == 0)
    }

    // The bignum's own comparison; the name matches `Ord::cmp` but this is
    // not a trait implementation.
    #[allow(clippy::should_implement_trait)]
    pub fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let a_top = self.limbs.iter().rposition(|l| *l != 0);
        let b_top = other.limbs.iter().rposition(|l| *l != 0);
        match (a_top, b_top) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (Some(i), Some(j)) if i != j => i.cmp(&j),
            (Some(i), Some(_)) => {
                for k in (0..=i).rev() {
                    let a_k = self.limbs.get(k).copied().unwrap_or(0);
                    let b_k = other.limbs.get(k).copied().unwrap_or(0);
                    match a_k.cmp(&b_k) {
                        std::cmp::Ordering::Equal => continue,
                        ord => return ord,
                    }
                }
                std::cmp::Ordering::Equal
            }
        }
    }

    pub fn shl(&self, bits: u32) -> Self {
        let mut out = vec![0u64; self.limbs.len()];
        let limb_shift = (bits / 64) as usize;
        let bit_shift = bits % 64;
        for (i, limb) in self.limbs.iter().enumerate() {
            let mut v = *limb;
            if bit_shift != 0 {
                v <<= bit_shift;
            }
            let target = i + limb_shift;
            if out.len() <= target {
                out.resize(target + 1, 0);
            }
            out[target] |= v;
            if bit_shift != 0 {
                let high = limb >> (64 - bit_shift);
                if high != 0 {
                    if out.len() <= target + 1 {
                        out.resize(target + 2, 0);
                    }
                    out[target + 1] |= high;
                }
            }
        }
        Self { limbs: out }
    }

    pub fn shr(&self, bits: u32) -> Self {
        if self.is_zero() {
            return Self::zero();
        }
        let limb_shift = (bits / 64) as usize;
        let bit_shift = bits % 64;
        let mut out = Vec::with_capacity(self.limbs.len().saturating_sub(limb_shift));
        for i in limb_shift..self.limbs.len() {
            let mut v = self.limbs[i] >> bit_shift;
            if bit_shift != 0 && i + 1 < self.limbs.len() {
                v |= self.limbs[i + 1] << (64 - bit_shift);
            }
            out.push(v);
        }
        Self { limbs: out }
    }

    /// The low `bits` bits (used as `mod 2^bits`).
    pub fn mask(&self, bits: u32) -> Self {
        let mut out = self.clone();
        let limb = (bits / 64) as usize;
        let bit = bits % 64;
        if out.limbs.len() > limb {
            if bit == 0 {
                out.limbs.truncate(limb);
            } else {
                out.limbs[limb] &= (1u64 << bit) - 1;
                out.limbs.truncate(limb + 1);
            }
        }
        while out.limbs.last() == Some(&0) {
            out.limbs.pop();
        }
        out
    }

    pub fn add(&self, other: &Self) -> Self {
        let mut out = Vec::new();
        let mut carry = 0u64;
        let len = self.limbs.len().max(other.limbs.len());
        for i in 0..len {
            let a = self.limbs.get(i).copied().unwrap_or(0);
            let b = other.limbs.get(i).copied().unwrap_or(0);
            let (sum, c1) = a.overflowing_add(b);
            let (sum, c2) = sum.overflowing_add(carry);
            out.push(sum);
            carry = u64::from(c1 || c2);
        }
        if carry != 0 {
            out.push(carry);
        }
        Self { limbs: out }
    }

    /// `self - other` (self ≥ other).
    pub fn sub(&self, other: &Self) -> Self {
        let mut out = Vec::with_capacity(self.limbs.len());
        let mut borrow = 0u64;
        for i in 0..self.limbs.len() {
            let b = other.limbs.get(i).copied().unwrap_or(0);
            let (diff, b1) = self.limbs[i].overflowing_sub(b);
            let (diff, b2) = diff.overflowing_sub(borrow);
            out.push(diff);
            borrow = u64::from(b1 || b2);
        }
        while out.last() == Some(&0) {
            out.pop();
        }
        Self { limbs: out }
    }

    pub fn add_small(&self, n: u64) -> Self {
        if n == 0 {
            return self.clone();
        }
        self.add(&Self::from_u64(n))
    }

    /// Multiply by a single limb (≤ 36 for radix digit generation, but any
    /// u64 works).
    pub fn mul_small(&self, n: u64) -> Self {
        if n == 0 || self.is_zero() {
            return Self::zero();
        }
        let mut out = Vec::with_capacity(self.limbs.len() + 1);
        let mut carry = 0u128;
        for limb in &self.limbs {
            let product = (*limb as u128) * (n as u128) + carry;
            out.push(product as u64);
            carry = product >> 64;
        }
        if carry != 0 {
            out.push(carry as u64);
        }
        Self { limbs: out }
    }

    /// Schoolbook multiply by another bignum.
    pub fn mul(&self, other: &Self) -> Self {
        if self.is_zero() || other.is_zero() {
            return Self::zero();
        }
        let mut out = vec![0u64; self.limbs.len() + other.limbs.len()];
        for (i, a) in self.limbs.iter().enumerate() {
            let mut carry = 0u128;
            for (j, b) in other.limbs.iter().enumerate() {
                let idx = i + j;
                let product = (*a as u128) * (*b as u128) + (out[idx] as u128) + carry;
                out[idx] = product as u64;
                carry = product >> 64;
            }
            let mut k = i + other.limbs.len();
            while carry != 0 {
                let (sum, overflow) = out[k].overflowing_add(carry as u64);
                out[k] = sum;
                carry = u64::from(overflow) as u128;
                k += 1;
            }
        }
        while out.last() == Some(&0) {
            out.pop();
        }
        Self { limbs: out }
    }

    /// `(self / n, self % n)` for a single-limb divisor.
    pub fn divmod_small(&self, n: u64) -> (Self, u64) {
        let mut quotient = vec![0u64; self.limbs.len()];
        let mut remainder = 0u128;
        for i in (0..self.limbs.len()).rev() {
            let current = (remainder << 64) | (self.limbs[i] as u128);
            quotient[i] = (current / n as u128) as u64;
            remainder = current % n as u128;
        }
        while quotient.last() == Some(&0) {
            quotient.pop();
        }
        (Self { limbs: quotient }, remainder as u64)
    }

    /// The position of the top bit + 1 (0 for zero).
    pub fn bit_len(&self) -> u64 {
        match self.limbs.iter().rposition(|l| *l != 0) {
            Some(top) => top as u64 * 64 + (64 - self.limbs[top].leading_zeros() as u64),
            None => 0,
        }
    }

    /// The decimal digits of the integer, most significant first.
    pub fn to_decimal(&self) -> String {
        self.to_base(10)
    }

    /// The digits of the integer in the given radix (2-36), most significant
    /// first; `"0"` for zero.
    pub fn to_base(&self, radix: u32) -> String {
        debug_assert!((2..=36).contains(&radix));
        if self.is_zero() {
            return "0".into();
        }
        let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let mut value = self.clone();
        let mut out = Vec::new();
        while !value.is_zero() {
            let (quotient, remainder) = value.divmod_small(radix as u64);
            out.push(digits[remainder as usize] as char);
            value = quotient;
        }
        out.iter().rev().collect()
    }
}

/// The exact dyadic decomposition of a finite value: `x = mantissa × 2^exp`
/// with `mantissa` in [1, 2^53) (subnormal-aware).
fn decompose(x: f64) -> (u64, i32) {
    let bits = x.to_bits();
    let biased = ((bits >> 52) & 0x7FF) as i32;
    let fraction = bits & 0xF_FFFF_FFFF_FFFF;
    if biased == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1 << 52), biased - 1075)
    }
}

/// The exact value of a finite nonzero `x` as `n / 2^p` (n a bignum, p ≥ 0).
fn exact_fraction(x: f64) -> (BigU, u32) {
    let (mantissa, exp) = decompose(x);
    if exp >= 0 {
        (BigU::from_u64(mantissa).shl(exp as u32), 0)
    } else {
        (BigU::from_u64(mantissa), (-exp) as u32)
    }
}

/// Round-half-up the exact value `n / 2^p` at `10^k`: the integer nearest to
/// (n / 2^p) × 10^k, with ties rounded up. This is `toFixed`'s intValue and
/// the exact digit source for the other formatting methods.
fn round_scale10(n: &BigU, p: u32, k: u32) -> BigU {
    let mut scaled = n.clone();
    for _ in 0..k {
        scaled = scaled.mul_small(10);
    }
    let quotient = scaled.shr(p);
    let remainder = scaled.mask(p);
    let half = if p == 0 {
        false
    } else {
        // The remainder's top bit: remainder ≥ 2^(p-1) rounds up.
        remainder.bit_len() > (p - 1) as u64
    };
    if half {
        quotient.add_small(1)
    } else {
        quotient
    }
}

/// The exact terminating decimal expansion of `|x|` (finite, nonzero):
/// `(digits, point)` with the digits in order and `point` the number of
/// digits before the decimal point (so `|x| = 0.<digits> × 10^point`; `point`
/// may be ≤ 0). Trailing zeros are dropped.
fn exact_decimal(x: f64) -> (String, i32) {
    let (n, p) = exact_fraction(x);
    let integer = n.shr(p);
    let mut fraction = n.mask(p);
    let int_digits = if integer.is_zero() {
        String::new()
    } else {
        integer.to_decimal()
    };
    let mut frac_digits = String::new();
    let mut guard = 0;
    while !fraction.is_zero() && guard < 2000 {
        let scaled = fraction.mul_small(10);
        let digit = scaled.shr(p).limbs.first().copied().unwrap_or(0);
        frac_digits.push(char::from(b'0' + digit as u8));
        fraction = scaled.mask(p);
        guard += 1;
    }
    let point = int_digits.len() as i32;
    let digits = format!("{int_digits}{frac_digits}");
    // Trim trailing zeros (not significant in the exact expansion).
    let digits = digits.trim_end_matches('0').to_string();
    (digits, point)
}

/// spec 6.1.6.1.21 Number::toString(x, radix) for radix ≠ 10, exact for
/// terminating expansions and shortest round-trip otherwise.
pub fn to_string_radix(x: f64, radix: u32) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x == 0.0 {
        return "0".into();
    }
    if x < 0.0 {
        return format!("-{}", to_string_radix(-x, radix));
    }
    if x.is_infinite() {
        return "Infinity".into();
    }
    if radix == 10 {
        return to_string(x).to_string_lossy();
    }
    // Fast path: an integral value below 2^53 converts by plain division —
    // the general radix paths allocate big integers even for byte-sized
    // values (the Sputnik decodeURI fixtures call `toString(16)` millions
    // of times).
    if x.fract() == 0.0 && x.abs() < 9007199254740992.0 {
        return integer_radix(x as i64, radix);
    }
    if radix.is_power_of_two() {
        return radix_power_of_two(x, radix);
    }
    // Dyadic rationals terminate in any even radix; odd radixes need the
    // shortest round-trip search.
    if radix.is_multiple_of(2) {
        return radix_exact(x, radix);
    }
    radix_round_trip(x, radix)
}

/// The radix digits for the integer fast path.
const RADIX_DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// Convert a non-negative integral value to `radix` by repeated division;
/// the caller has already handled the sign (the `x < 0` branch recurses).
fn integer_radix(mut x: i64, radix: u32) -> String {
    let mut buf = [0u8; 64];
    let mut i = buf.len();
    while x > 0 {
        i -= 1;
        buf[i] = RADIX_DIGITS[(x % radix as i64) as usize];
        x /= radix as i64;
    }
    if i == buf.len() {
        return "0".into();
    }
    String::from_utf8_lossy(&buf[i..]).into_owned()
}

/// Exact conversion for radix = 2^p (p ∈ 1..5): group the binary digits of
/// the dyadic value. The exact representation is also the shortest.
fn radix_power_of_two(x: f64, radix: u32) -> String {
    let p = radix.trailing_zeros();
    let (mantissa, exp) = decompose(x);
    // Binary digits of x = mantissa × 2^exp, then group p at a time from the
    // binary point outward.
    let shift = if exp >= 0 { exp as u32 } else { (-exp) as u32 };
    let integer = BigU::from_u64(mantissa).shl(if exp >= 0 { shift } else { 0 });
    let integer = integer.shr(if exp >= 0 { 0 } else { shift });
    let mask = radix as u64 - 1;
    let mut digits: Vec<u8> = Vec::new();
    if integer.is_zero() {
        digits.push(0);
    } else {
        let mut value = integer;
        let mut int_digits = Vec::new();
        while !value.is_zero() {
            let (q, r) = value.divmod_small(radix as u64);
            int_digits.push(r as u8);
            value = q;
        }
        int_digits.reverse();
        digits.extend(int_digits);
    }
    if exp < 0 {
        let frac_bits = (-exp) as u32;
        let fraction = BigU::from_u64(mantissa).mask(frac_bits);
        let mut frac_digits: Vec<u8> = Vec::new();
        let mut position = frac_bits;
        let mut remaining = fraction.clone();
        while position > 0 && !remaining.is_zero() {
            let start = position.saturating_sub(p);
            let group = remaining.mask(position).shr(start);
            let digit = group.limbs.first().copied().unwrap_or(0) & mask;
            frac_digits.push(digit as u8);
            remaining = remaining.mask(start);
            position = start;
        }
        // Drop trailing zero fractional digits (shortest representation).
        while frac_digits.last() == Some(&0) {
            frac_digits.pop();
        }
        if !frac_digits.is_empty() {
            digits.push(b'.');
            digits.extend(frac_digits);
        }
    }
    let chars = b"0123456789abcdefghijklmnopqrstuvwxyz";
    digits
        .iter()
        .map(|d| {
            if *d == b'.' {
                '.'
            } else {
                chars[*d as usize] as char
            }
        })
        .collect()
}

/// The exact terminating expansion in an even radix.
fn radix_exact(x: f64, radix: u32) -> String {
    let (n, p) = exact_fraction(x);
    let integer = n.shr(p);
    let mut fraction = n.mask(p);
    let int_digits = integer.to_base(radix);
    let mut frac_digits: Vec<u8> = Vec::new();
    let mut guard = 0;
    while !fraction.is_zero() && guard < 4000 {
        let scaled = fraction.mul_small(radix as u64);
        // scaled < 2^p × radix, so the shifted digit is already < radix.
        let digit = scaled.shr(p).limbs.first().copied().unwrap_or(0);
        frac_digits.push(digit as u8);
        fraction = scaled.mask(p);
        guard += 1;
    }
    while frac_digits.last() == Some(&0) {
        frac_digits.pop();
    }
    let mut out = int_digits;
    if !frac_digits.is_empty() {
        out.push('.');
        for d in frac_digits {
            out.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[d as usize] as char);
        }
    }
    out
}

/// The rounding interval of a finite positive value as `(low, high, even)`, in
/// units of 2^-1075 (exact integers). `even` is the parity of the significand:
/// ties at the boundaries round to the value iff the significand is even.
fn rounding_bounds(mantissa: u64, exp: i32) -> (BigU, BigU, bool) {
    // x = mantissa × 2^exp; the top bit sits at position 52 + exp, so in
    // units of 2^-1075 the value is mantissa << (exp + 1075) and the ulp is
    // 2^(exp + 1074).
    let value = BigU::from_u64(mantissa).shl((exp + 1075) as u32);
    let even = mantissa.is_multiple_of(2);
    if mantissa < (1 << 52) {
        // Subnormal: uniform grid with spacing 2^-1074 (half = 1 in these
        // units).
        let half = BigU::from_u64(1);
        return (value.sub(&half), value.add(&half), even);
    }
    let half_up = BigU::from_u64(1).shl((exp + 1074) as u32);
    let half_down = if mantissa.is_power_of_two() && exp > -1074 {
        // Half spacing below a power of two (except the smallest normal,
        // which sits on the subnormal grid).
        BigU::from_u64(1).shl((exp + 1073) as u32)
    } else {
        half_up.clone()
    };
    (value.sub(&half_down), value.add(&half_up), even)
}

/// `n` such that b^(n-1) ≤ x < b^n (the decimal-point position), for a
/// positive finite x = mantissa × 2^exp.
fn floor_log_base(mantissa: u64, exp: i32, b: u32) -> i32 {
    // ln(x) = ln(mantissa) + exp·ln(2): the equivalent `mantissa as f64 *
    // 2f64.powi(exp)` returns 0 for subnormal exponents — powi(2, -1074)
    // overflows 2^1074 to inf and 1/inf flushes to zero — turning ln(x)
    // into -inf and the estimate into i32::MIN + 1.
    let ln_x = (mantissa as f64).ln() + (exp as f64) * std::f64::consts::LN_2;
    let mut n = (ln_x / (b as f64).ln()).floor() as i32 + 1;
    // Adjust with exact comparisons: x ≥ b^n? (in units of 2^-1074).
    let x_scaled = BigU::from_u64(mantissa).shl((exp + 1074) as u32);
    loop {
        let mut b_pow = BigU::from_u64(1);
        for _ in 0..n {
            b_pow = b_pow.mul_small(b as u64);
        }
        let b_pow_scaled = b_pow.shl(1074);
        if x_scaled.cmp(&b_pow_scaled) == std::cmp::Ordering::Greater {
            n += 1;
            continue;
        }
        if n > 1 {
            let mut b_prev = BigU::from_u64(1);
            for _ in 0..(n - 1) {
                b_prev = b_prev.mul_small(b as u64);
            }
            if x_scaled.cmp(&b_prev.shl(1074)) == std::cmp::Ordering::Less {
                n -= 1;
                continue;
            }
        }
        break;
    }
    n
}

/// Shortest round-trip conversion for odd radixes (spec 6.1.6.1.21 step 5):
/// find the smallest k and a k-digit s with 𝔽(s × b^(n-k)) = x, choosing the
/// s closest to the true value.
fn radix_round_trip(x: f64, radix: u32) -> String {
    let (mantissa, exp) = decompose(x);
    let (low, high, even) = rounding_bounds(mantissa, exp);
    let value = BigU::from_u64(mantissa).shl((exp + 1075) as u32);
    let n = floor_log_base(mantissa, exp, radix);
    let b = radix as u64;
    let scale = 1075u32;
    // Incremental state: low×b^m, high×b^m, value×b^m, b^(k-1), b^k for
    // m = k - n ≥ 0 (k starts at max(n, 1)).
    let mut low_scaled = low;
    let mut high_scaled = high;
    let mut value_scaled = value;
    for _ in 0..(n.max(1) - n) {
        low_scaled = low_scaled.mul_small(b);
        high_scaled = high_scaled.mul_small(b);
        value_scaled = value_scaled.mul_small(b);
    }
    let mut b_pow_minus = BigU::from_u64(1); // b^(k-1)
    for _ in 1..n.max(1) {
        b_pow_minus = b_pow_minus.mul_small(b);
    }
    let mut b_pow = b_pow_minus.mul_small(b); // b^k
    for k in n.max(1)..6000 {
        // s must satisfy: low ≤ s×b^(n-k)×2^1075 ≤ high, i.e.
        // low_scaled ≤ s × 2^1075 ≤ high_scaled (m = k - n ≥ 0).
        let low_exact = low_scaled.mask(scale).is_zero();
        let high_exact = high_scaled.mask(scale).is_zero();
        let mut s_min = low_scaled.shr(scale);
        if !low_exact {
            s_min = s_min.add_small(1); // ceil
        }
        let mut s_max = high_scaled.shr(scale);
        if !even && high_exact {
            s_max = s_max.sub(&BigU::from_u64(1)); // open upper boundary
        }
        if !even && low_exact {
            s_min = s_min.add_small(1); // open lower boundary
        }
        // s must have exactly k digits.
        if s_min.cmp(&b_pow_minus) == std::cmp::Ordering::Less {
            s_min = b_pow_minus.clone();
        }
        if s_max.cmp(&b_pow.sub(&BigU::from_u64(1))) == std::cmp::Ordering::Greater {
            s_max = b_pow.sub(&BigU::from_u64(1));
        }
        if s_min.cmp(&s_max) != std::cmp::Ordering::Greater {
            // Choose the candidate closest to the true value (round-half-even
            // of value_scaled / 2^1075).
            let mut target = value_scaled.shr(scale);
            let remainder = value_scaled.mask(scale);
            let half = BigU::from_u64(1).shl(scale - 1);
            if remainder.cmp(&half) == std::cmp::Ordering::Greater
                || (remainder.cmp(&half) == std::cmp::Ordering::Equal
                    && target.limbs.first().copied().unwrap_or(0) % 2 == 1)
            {
                target = target.add_small(1);
            }
            let s = if target.cmp(&s_min) == std::cmp::Ordering::Less {
                s_min
            } else if target.cmp(&s_max) == std::cmp::Ordering::Greater {
                s_max
            } else {
                target
            };
            return format_fixed_digits(&s, radix, k, n);
        }
        low_scaled = low_scaled.mul_small(b);
        high_scaled = high_scaled.mul_small(b);
        value_scaled = value_scaled.mul_small(b);
        b_pow_minus = b_pow_minus.mul_small(b);
        b_pow = b_pow.mul_small(b);
    }
    unreachable!("round-trip digit search must terminate for {x} radix {radix}")
}

/// Format a k-digit significand s with the decimal point after n digits
/// (spec 6.1.6.1.21 steps 6-12).
fn format_fixed_digits(s: &BigU, radix: u32, k: i32, n: i32) -> String {
    let digits = s.to_base(radix);
    if n >= k {
        let zeros = "0".repeat((n - k) as usize);
        format!("{digits}{zeros}")
    } else if n > 0 {
        let split = n as usize;
        format!("{}.{}", &digits[..split], &digits[split..])
    } else {
        let zeros = "0".repeat((-n) as usize);
        format!("0.{zeros}{digits}")
    }
}

/// The integer nearest to `|x| × 10^f`, ties rounded up (spec 20.1.3.3
/// Number.prototype.toFixed).
pub fn to_fixed_scale(x: f64, f: u32) -> BigU {
    let (n, p) = exact_fraction(x);
    round_scale10(&n, p, f)
}

/// The exact digits of `|x|` rounded to `len` significant digits (half-up at
/// the cut): `(exponent, digits)` with the value `0.digits × 10^exponent` and
/// `digits` of length `len` (or `len + 1` after a 999... carry, with the
/// exponent bumped by the caller). Leading zeros below the first significant
/// digit are skipped.
pub fn round_significant(x: f64, len: u32) -> (i32, String) {
    let (digits, point) = exact_decimal(x);
    let lead = digits.find(|c| c != '0').unwrap_or(0);
    let mut exponent = point - 1 - lead as i32;
    let stripped = &digits[lead..];
    let mut significand = round_digits(stripped, len as usize);
    if significand.len() as u32 > len {
        // 999... rounded up to 1000...: the exponent bumps.
        exponent += 1;
        significand = significand[..len as usize].to_string();
    }
    (exponent, significand)
}

/// Keep the first `len` digits, rounding the cut half-up; carries propagate
/// and may produce `len + 1` digits.
fn round_digits(digits: &str, len: usize) -> String {
    if digits.len() <= len {
        return format!("{digits:0<width$}", width = len);
    }
    let cut = &digits[..len];
    let next = digits.as_bytes()[len] - b'0';
    let mut out: Vec<u8> = cut.bytes().collect();
    if next >= 5 {
        let mut i = out.len();
        loop {
            if i == 0 {
                out.insert(0, b'1');
                break;
            }
            i -= 1;
            if out[i] == b'9' {
                out[i] = b'0';
            } else {
                out[i] += 1;
                break;
            }
        }
    }
    out.iter().map(|b| *b as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: f64) -> String {
        to_string(x).to_string_lossy()
    }

    #[test]
    fn to_string_special_values() {
        assert_eq!(s(f64::NAN), "NaN");
        assert_eq!(s(0.0), "0");
        assert_eq!(s(-0.0), "0");
        assert_eq!(s(f64::INFINITY), "Infinity");
        assert_eq!(s(f64::NEG_INFINITY), "-Infinity");
    }

    #[test]
    fn to_string_decimal_cases() {
        assert_eq!(s(123.456), "123.456");
        assert_eq!(s(0.5), "0.5");
        assert_eq!(s(0.1), "0.1");
        assert_eq!(s(1.0 / 3.0), "0.3333333333333333");
        assert_eq!(s(-42.0), "-42");
        assert_eq!(s(100000000000000000000.0), "100000000000000000000");
        assert_eq!(s(123456789012345680000.0), "123456789012345680000");
    }

    #[test]
    fn to_string_exponent_thresholds() {
        assert_eq!(s(1e-6), "0.000001");
        assert_eq!(s(1e-7), "1e-7");
        assert_eq!(s(1e21), "1e+21");
        assert_eq!(s(1234567890123456800000.0), "1.2345678901234568e+21");
        assert_eq!(s(1.5e300), "1.5e+300");
        assert_eq!(s(5e-324), "5e-324");
        assert_eq!(s(1.7976931348623157e308), "1.7976931348623157e+308");
        assert_eq!(s(1.23e-5), "0.0000123");
        assert_eq!(s(123e-20), "1.23e-18");
    }

    #[test]
    fn to_string_precise_integers() {
        assert_eq!(s(9007199254740992.0), "9007199254740992");
        assert_eq!(s(9007199254740994.0), "9007199254740994");
        assert_eq!(s(-0.00000123), "-0.00000123");
    }

    #[test]
    fn radix_power_of_two_conversions() {
        assert_eq!(to_string_radix(255.0, 16), "ff");
        assert_eq!(to_string_radix(255.0, 2), "11111111");
        assert_eq!(to_string_radix(255.0, 8), "377");
        assert_eq!(to_string_radix(0.5, 2), "0.1");
        assert_eq!(to_string_radix(0.5, 16), "0.8");
        assert_eq!(to_string_radix(1.5, 16), "1.8");
        assert_eq!(to_string_radix(10.0, 2), "1010");
        assert_eq!(to_string_radix(10.0, 16), "a");
        assert_eq!(to_string_radix(-1.0, 2), "-1");
        assert_eq!(to_string_radix(f64::NAN, 2), "NaN");
        assert_eq!(to_string_radix(0.0, 16), "0");
        assert_eq!(to_string_radix(f64::INFINITY, 2), "Infinity");
    }

    #[test]
    fn radix_even_and_odd_conversions() {
        assert_eq!(to_string_radix(255.0, 10), "255");
        assert_eq!(to_string_radix(255.0, 36), "73");
        assert_eq!(to_string_radix(10.0, 3), "101");
        assert_eq!(to_string_radix(10.0, 5), "20");
        assert_eq!(to_string_radix(0.5, 6), "0.3"); // 3/6 = 1/2
        // 1.5 = 1.111...₃ never terminates; the shortest round-trip is
        // (3^34 - 1)/2 × 3^-33, i.e. 34 ones.
        assert_eq!(to_string_radix(1.5, 3), format!("1.{}", "1".repeat(33)));
        // Round trips in every radix.
        for radix in 2..=36 {
            for x in [
                0.5,
                1.0,
                10.0,
                123.456,
                1.0 / 3.0,
                1e21,
                5e-324,
                9007199254740993.0,
            ] {
                let text = to_string_radix(x, radix);
                let parsed = parse_radix(&text, radix);
                assert!(
                    (parsed - x).abs() / x.abs().max(1.0) < 1e-9 || parsed == x,
                    "round trip failed for {x} radix {radix}: {text} -> {parsed}"
                );
            }
        }
    }

    /// A small radix parser for the round-trip check (handles `e±N`
    /// exponential notation, which radix-10 toString uses for large values).
    fn parse_radix(text: &str, radix: u32) -> f64 {
        let negative = text.starts_with('-');
        let text = text.trim_start_matches(['-', '+']);
        let (mantissa_text, exponent) = if radix == 10 {
            match text.find(['e', 'E']) {
                Some(at) => (&text[..at], text[at + 1..].parse::<i32>().unwrap_or(0)),
                None => (text, 0),
            }
        } else {
            (text, 0)
        };
        let (int_part, frac_part) = match mantissa_text.split_once('.') {
            Some((i, f)) => (i, f),
            None => (mantissa_text, ""),
        };
        let mut value = 0.0;
        for c in int_part.chars() {
            value = value * radix as f64 + c.to_digit(36).unwrap_or(0) as f64;
        }
        let mut scale = 1.0 / radix as f64;
        for c in frac_part.chars() {
            value += c.to_digit(36).unwrap_or(0) as f64 * scale;
            scale /= radix as f64;
        }
        value *= (radix as f64).powi(exponent);
        if negative { -value } else { value }
    }

    #[test]
    fn to_fixed_exact_digits() {
        let f = |x: f64, digits: u32| to_fixed_scale(x, digits).to_decimal();
        assert_eq!(f(1.0, 0), "1");
        assert_eq!(f(1.5, 0), "2"); // half-up
        assert_eq!(f(2.5, 0), "3");
        assert_eq!(f(0.1, 1), "1");
        assert_eq!(f(0.1, 20), "10000000000000000555");
        assert_eq!(f(123.456, 2), "12346");
        assert_eq!(f(1000000000000000128.0, 0), "1000000000000000128");
    }

    #[test]
    fn round_significant_digits() {
        let (e, d) = round_significant(123.456, 4);
        assert_eq!((e, d.as_str()), (2, "1235"));
        let (e, d) = round_significant(0.5, 1);
        assert_eq!((e, d.as_str()), (-1, "5"));
        let (e, d) = round_significant(0.05, 1);
        assert_eq!((e, d.as_str()), (-2, "5"));
        let (e, d) = round_significant(0.0005, 1);
        assert_eq!((e, d.as_str()), (-4, "5"));
        let (e, d) = round_significant(0.9999, 3);
        assert_eq!((e, d.as_str()), (0, "100"));
        let (e, d) = round_significant(1.0 / 3.0, 5);
        assert_eq!((e, d.as_str()), (-1, "33333"));
    }

    #[test]
    fn bignum_arithmetic() {
        let a = BigU::from_u64(123456789);
        let b = BigU::from_u64(987654321);
        assert_eq!(a.add(&b).to_decimal(), "1111111110");
        assert_eq!(b.sub(&a).to_decimal(), "864197532");
        assert_eq!(a.mul_small(1000).to_decimal(), "123456789000");
        assert_eq!(a.mul(&b).to_decimal(), "121932631112635269");
        assert_eq!(a.to_base(16), "75bcd15");
        assert_eq!(BigU::from_u64(255).to_base(2), "11111111");
        let (q, r) = a.divmod_small(7);
        assert_eq!(q.to_decimal(), "17636684");
        assert_eq!(r, 1);
        assert_eq!(BigU::from_u64(1).shl(1074).to_base(2).len(), 1075);
        assert_eq!(BigU::from_u64(1).shl(2000).shr(1075).bit_len(), 926);
    }

    proptest::proptest! {
        #[test]
        fn to_string_round_trips(x: f64) {
            let text = s(x);
            let back: f64 = text.parse().unwrap_or(f64::NAN);
            if x.is_nan() {
                assert_eq!(text, "NaN");
            } else if x == 0.0 {
                assert_eq!(text, "0");
                assert_eq!(back, 0.0);
            } else {
                assert_eq!(back, x, "round trip failed for {x}: {text}");
            }
        }
    }

    #[test]
    fn arithmetic_ops() {
        assert_eq!(add(1.5, 2.5), 4.0);
        assert_eq!(subtract(2.0, 5.0), -3.0);
        assert_eq!(multiply(-2.0, 3.0), -6.0);
        assert_eq!(divide(1.0, 4.0), 0.25);
        assert_eq!(divide(1.0, 0.0), f64::INFINITY);
    }

    #[test]
    fn remainder_semantics() {
        assert_eq!(remainder(5.0, 2.0), 1.0);
        assert_eq!(remainder(-5.0, 2.0), -1.0);
        assert_eq!(remainder(5.0, -2.0), 1.0);
        assert!(remainder(5.0, 0.0).is_nan());
    }

    #[test]
    fn exponentiate_special_cases() {
        assert_eq!(exponentiate(2.0, 3.0), 8.0);
        assert_eq!(exponentiate(2.0, 0.0), 1.0);
        assert!(exponentiate(f64::NAN, 0.0) == 1.0);
        assert!(exponentiate(1.0, f64::INFINITY).is_nan());
        assert!(exponentiate(-1.0, f64::INFINITY).is_nan());
        assert!(exponentiate(0.5, f64::INFINITY) == 0.0);
        assert!(exponentiate(2.0, f64::INFINITY).is_infinite());
        assert!(exponentiate(-2.0, 2.5).is_nan());
        assert!(exponentiate(f64::NAN, 5.0).is_nan());
    }

    #[test]
    fn unary_minus_preserves_signed_zero() {
        assert_eq!(unary_minus(0.0).to_bits(), (-0.0f64).to_bits());
        assert_eq!(unary_minus(-0.0).to_bits(), 0.0f64.to_bits());
        assert_eq!(unary_minus(5.0), -5.0);
    }

    #[test]
    fn bitwise_ops_on_int32() {
        assert_eq!(bitwise_not(5.0), -6.0);
        assert_eq!(bitwise_not(-1.0), 0.0);
        assert_eq!(bitwise_and(12.0, 10.0), 8.0);
        assert_eq!(bitwise_or(12.0, 10.0), 14.0);
        assert_eq!(bitwise_xor(12.0, 10.0), 6.0);
    }

    #[test]
    fn shift_ops() {
        assert_eq!(left_shift(1.0, 10.0), 1024.0);
        assert_eq!(left_shift(1.0, 32.0), 1.0); // shift count mod 32
        assert_eq!(signed_right_shift(-8.0, 1.0), -4.0);
        assert_eq!(unsigned_right_shift(-1.0, 0.0), 4294967295.0);
        assert_eq!(unsigned_right_shift(2147483648.0, 0.0), 2147483648.0);
    }

    #[test]
    fn comparisons() {
        assert!(equal(1.0, 1.0));
        assert!(!equal(f64::NAN, f64::NAN));
        assert!(equal(0.0, -0.0));
        assert_eq!(less_than(1.0, 2.0), Some(true));
        assert_eq!(less_than(2.0, 1.0), Some(false));
        assert_eq!(less_than(0.0, -0.0), Some(false));
        assert_eq!(less_than(f64::NAN, 1.0), None);
    }
}
