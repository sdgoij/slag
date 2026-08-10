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
