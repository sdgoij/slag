//! The BigInt language type: arbitrary-precision integers (spec 6.1.6.2).

use num_bigint::BigInt as NumBigInt;

use crate::error::{ErrorKind, JsError};
use crate::heap::Trace;

/// A BigInt value. Equality follows the mathematical value, per
/// `BigInt::equal` (spec 6.1.6.2.6).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BigInt(pub NumBigInt);

impl Trace for BigInt {
    fn trace(&self, _visit: &mut dyn FnMut(crate::heap::GcAny)) {
        // An arbitrary-precision integer: no heap edges.
    }
}

impl From<i32> for BigInt {
    fn from(v: i32) -> Self {
        Self(NumBigInt::from(v))
    }
}

impl From<i64> for BigInt {
    fn from(v: i64) -> Self {
        Self(NumBigInt::from(v))
    }
}

impl From<u64> for BigInt {
    fn from(v: u64) -> Self {
        Self(NumBigInt::from(v))
    }
}

impl BigInt {
    pub fn zero() -> BigInt {
        Self(NumBigInt::ZERO)
    }

    /// Parses `text` in the given radix (2-36); None on invalid input.
    pub fn parse_str(text: &str, radix: u32) -> Option<BigInt> {
        NumBigInt::parse_bytes(text.as_bytes(), radix).map(BigInt)
    }

    pub fn is_zero(&self) -> bool {
        self.0.sign() == num_bigint::Sign::NoSign
    }

    /// Correctly rounded `f64` conversion; the exact decimal expansion is a
    /// valid, correctly parsed float literal.
    pub fn to_f64(&self) -> f64 {
        self.0.to_str_radix(10).parse().unwrap_or(f64::NAN)
    }

    /// NumberToBigInt (spec 7.1.16): the exact integer value of an integral
    /// double, or None when `number` is NaN, +Infinity, -Infinity, or not an
    /// integral number. The mantissa/exponent decomposition is exact (the
    /// shortest decimal round-trip of a large double can round to a different
    /// mathematical value than the double itself).
    pub fn from_f64_exact(number: f64) -> Option<BigInt> {
        if !number.is_finite() || number.fract() != 0.0 {
            return None;
        }
        let bits = number.to_bits();
        let negative = bits >> 63 == 1;
        let exponent_bits = ((bits >> 52) & 0x7ff) as i32;
        let fraction = bits & ((1u64 << 52) - 1);
        let (mantissa, shift) = if exponent_bits == 0 {
            // Subnormal: value = fraction * 2^-1074.
            (fraction, -1074)
        } else {
            // Normal: value = (2^52 + fraction) * 2^(exponent_bits - 1023 - 52).
            ((1u64 << 52) + fraction, exponent_bits - 1023 - 52)
        };
        let mut value = NumBigInt::from(mantissa);
        if shift > 0 {
            value <<= shift as usize;
        } else if shift < 0 {
            // `number` is integral, so the division is exact.
            value >>= (-shift) as usize;
        }
        if negative {
            value = -value;
        }
        Some(BigInt(value))
    }
}

/// spec 6.1.6.2.1
pub fn add(a: &BigInt, b: &BigInt) -> BigInt {
    BigInt(&a.0 + &b.0)
}

/// spec 6.1.6.2.2
pub fn subtract(a: &BigInt, b: &BigInt) -> BigInt {
    BigInt(&a.0 - &b.0)
}

/// spec 6.1.6.2.3
pub fn multiply(a: &BigInt, b: &BigInt) -> BigInt {
    BigInt(&a.0 * &b.0)
}

/// Truncating division toward zero (spec 6.1.6.2.8).
pub fn divide(a: &BigInt, b: &BigInt) -> BigInt {
    BigInt(&a.0 / &b.0)
}

/// Remainder with the sign of the dividend (spec 6.1.6.2.9).
pub fn remainder(a: &BigInt, b: &BigInt) -> BigInt {
    BigInt(&a.0 % &b.0)
}

/// Left shift by a non-negative count (spec 6.1.6.2.11).
/// Exponentiation; a negative exponent is a RangeError (spec 6.1.6.2.15).
pub fn exponentiate(base: &BigInt, exponent: &BigInt) -> Result<BigInt, JsError> {
    if exponent.0.sign() == num_bigint::Sign::Minus {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "Exponent must be non-negative".into(),
        ));
    }
    let exp: u64 = exponent.0.to_str_radix(10).parse().unwrap_or(u64::MAX);
    let exp = u32::try_from(exp)
        .map_err(|_| JsError::new(ErrorKind::RangeError, "Exponent too large".into()))?;
    Ok(BigInt(base.0.pow(exp)))
}

/// Left shift: `x × 2^shift`, or truncating division for negative counts
/// (spec 6.1.6.2.10).
pub fn left_shift(x: &BigInt, shift: i64) -> BigInt {
    if shift >= 0 {
        BigInt(&x.0 * NumBigInt::from(2u64).pow(shift as u32))
    } else {
        BigInt(&x.0 / NumBigInt::from(2u64).pow((-shift) as u32))
    }
}

/// Arithmetic right shift (spec 6.1.6.2.12): `leftShift(x, -y)`.
pub fn right_shift(x: &BigInt, shift: i64) -> BigInt {
    left_shift(x, -shift)
}

/// `>>>` on BigInt is a TypeError (spec 6.1.6.2.13).
pub fn unsigned_right_shift(_x: &BigInt, _shift: i64) -> Result<BigInt, JsError> {
    Err(JsError::new(
        ErrorKind::TypeError,
        "BigInts have no unsigned right shift".into(),
    ))
}

/// spec 6.1.6.2.4
pub fn bitwise_and(a: &BigInt, b: &BigInt) -> BigInt {
    BigInt(&a.0 & &b.0)
}

/// spec 6.1.6.2.5
pub fn bitwise_or(a: &BigInt, b: &BigInt) -> BigInt {
    BigInt(&a.0 | &b.0)
}

/// spec 6.1.6.2.7
pub fn bitwise_xor(a: &BigInt, b: &BigInt) -> BigInt {
    BigInt(&a.0 ^ &b.0)
}

/// Bitwise NOT: `-x - 1` (spec 6.1.6.2.14).
pub fn bitwise_not(x: &BigInt) -> BigInt {
    BigInt(-&x.0 - 1)
}

pub fn unary_minus(x: &BigInt) -> BigInt {
    BigInt(-&x.0)
}

/// spec 6.1.6.2.16 BigInt::toString.
pub fn to_string(x: &BigInt, radix: u32) -> String {
    x.0.to_str_radix(radix)
}

/// spec 6.1.6.2.6 BigInt::equal.
pub fn equal(a: &BigInt, b: &BigInt) -> bool {
    a == b
}

/// spec 6.1.6.2.11 BigInt::lessThan.
pub fn less_than(a: &BigInt, b: &BigInt) -> bool {
    a.0 < b.0
}

/// spec 21.2.2.4 BigInt.asUintN: `int mod 2^bits` as a non-negative value.
/// `bits` comes from ToIndex, so it can exceed u32 and the value's magnitude.
pub fn as_uint_n(int: &BigInt, bits: u64) -> BigInt {
    if bits == 0 || int.is_zero() {
        return BigInt::zero();
    }
    // A non-negative value that fits in `bits` bits is unchanged; skipping the
    // modulus avoids allocating 2^bits when ToIndex yields a huge width.
    if int.0.sign() != num_bigint::Sign::Minus && int.0.bits() < bits {
        return BigInt(int.0.clone());
    }
    let modulus = BigInt(NumBigInt::from(1u64) << (bits as usize));
    let mut result = BigInt(&int.0 % &modulus.0);
    if result.0.sign() == num_bigint::Sign::Minus {
        result = BigInt(&result.0 + &modulus.0);
    }
    result
}

/// spec 21.2.2.3 BigInt.asIntN: the signed two's-complement truncation to
/// `bits` bits.
pub fn as_int_n(int: &BigInt, bits: u64) -> BigInt {
    if bits == 0 || int.is_zero() {
        return BigInt::zero();
    }
    if int.0.sign() != num_bigint::Sign::Minus && int.0.bits() < bits {
        return BigInt(int.0.clone());
    }
    let modulus = BigInt(NumBigInt::from(1u64) << (bits as usize));
    let half = BigInt(NumBigInt::from(1u64) << (bits as usize - 1));
    let mut result = as_uint_n(int, bits);
    if result.0 >= half.0 {
        result = BigInt(&result.0 - &modulus.0);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big(v: i64) -> BigInt {
        BigInt::from(v)
    }

    #[test]
    fn arithmetic() {
        assert_eq!(add(&big(5), &big(3)), big(8));
        assert_eq!(subtract(&big(5), &big(8)), big(-3));
        assert_eq!(multiply(&big(-4), &big(3)), big(-12));
        assert_eq!(divide(&big(7), &big(2)), big(3));
        assert_eq!(divide(&big(-7), &big(2)), big(-3));
        assert_eq!(remainder(&big(7), &big(-2)), big(1));
        assert_eq!(remainder(&big(-7), &big(2)), big(-1));
    }

    #[test]
    fn exponentiation() {
        assert_eq!(exponentiate(&big(2), &big(10)).unwrap(), big(1024));
        assert_eq!(exponentiate(&big(-2), &big(3)).unwrap(), big(-8));
        let err = exponentiate(&big(2), &big(-1)).unwrap_err();
        assert_eq!(err.kind, ErrorKind::RangeError);
    }

    #[test]
    fn shifts() {
        assert_eq!(left_shift(&big(1), 10), big(1024));
        assert_eq!(left_shift(&big(-8), 1), big(-16));
        assert_eq!(right_shift(&big(8), 2), big(2));
        assert_eq!(right_shift(&big(-8), 2), big(-2));
        assert_eq!(right_shift(&big(1), -10), big(1024));
        assert!(unsigned_right_shift(&big(1), 1).is_err());
    }

    #[test]
    fn bitwise_ops() {
        assert_eq!(bitwise_and(&big(0b1100), &big(0b1010)), big(0b1000));
        assert_eq!(bitwise_or(&big(0b1100), &big(0b1010)), big(0b1110));
        assert_eq!(bitwise_xor(&big(0b1100), &big(0b1010)), big(0b0110));
        assert_eq!(bitwise_not(&big(0)), big(-1));
        assert_eq!(bitwise_not(&big(5)), big(-6));
        assert_eq!(unary_minus(&big(5)), big(-5));
    }

    #[test]
    fn to_string_handles_sign_and_radix() {
        assert_eq!(to_string(&big(-123), 10), "-123");
        assert_eq!(to_string(&big(255), 16), "ff");
        assert_eq!(to_string(&big(0), 10), "0");
    }

    #[test]
    fn comparison() {
        assert!(equal(&big(7), &big(7)));
        assert!(!equal(&big(7), &big(-7)));
        assert!(less_than(&big(-1), &big(1)));
        assert!(!less_than(&big(1), &big(1)));
    }

    #[test]
    fn zero_and_f64_conversion() {
        assert!(BigInt::zero().is_zero());
        assert!(big(0).is_zero());
        assert!(!big(1).is_zero());
        assert_eq!(big(123456789).to_f64(), 123456789.0);
        assert_eq!(big(-2).to_f64(), -2.0);
        assert_eq!(BigInt::from(1u64 << 63).to_f64(), 9223372036854775808.0);
    }

    #[test]
    fn parse_str_handles_radices_and_sign() {
        assert_eq!(BigInt::parse_str("ff", 16).unwrap(), big(255));
        assert_eq!(BigInt::parse_str("101", 2).unwrap(), big(5));
        assert_eq!(BigInt::parse_str("17", 8).unwrap(), big(15));
        assert_eq!(BigInt::parse_str("-12", 10).unwrap(), big(-12));
        assert_eq!(BigInt::parse_str("0", 10).unwrap(), big(0));
        assert!(BigInt::parse_str("", 10).is_none());
        assert!(BigInt::parse_str("zz", 16).is_none());
    }
}
