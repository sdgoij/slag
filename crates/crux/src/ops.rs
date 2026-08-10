//! Identity, equality, and ordering abstract operations (spec 7.2).

use num_bigint::BigInt as NumBigInt;

use crate::bigint::BigInt;
use crate::convert::{string_to_big_int, to_number};
use crate::error::JsError;
use crate::handle::Handle;
use crate::value::Value;

/// SameValue (spec 7.2.12).
pub fn same_value(x: &Value, y: &Value) -> bool {
    match (x, y) {
        (Value::Number(a), Value::Number(b)) => {
            if a.is_nan() && b.is_nan() {
                true
            } else if *a == 0.0 && *b == 0.0 {
                a.is_sign_negative() == b.is_sign_negative()
            } else {
                a == b
            }
        }
        _ => x == y,
    }
}

/// SameValueZero (spec 7.2.13): like SameValue, but +0 and -0 are equal.
pub fn same_value_zero(x: &Value, y: &Value) -> bool {
    match (x, y) {
        (Value::Number(a), Value::Number(b)) => a == b || (a.is_nan() && b.is_nan()),
        _ => same_value(x, y),
    }
}

/// IsStrictlyEqual (spec 7.2.16).
pub fn is_strictly_equal(x: &Value, y: &Value) -> bool {
    match (x, y) {
        // Number::equal: NaN ≠ NaN, +0 = -0.
        (Value::Number(a), Value::Number(b)) => a == b,
        _ => same_value(x, y),
    }
}

/// IsIntegralNumber (spec 7.2.7).
pub fn is_integral_number(number: f64) -> bool {
    !number.is_nan() && !number.is_infinite() && number.trunc() == number
}

/// IsLooselyEqual (spec 7.2.15) for primitive operands. The object branches
/// (steps 1, 2, 11) join in Phase 5 with the object model.
pub fn is_loosely_equal(x: &Value, y: &Value) -> Result<bool, JsError> {
    if matches!(x, Value::Null) && matches!(y, Value::Undefined)
        || matches!(x, Value::Undefined) && matches!(y, Value::Null)
    {
        return Ok(true);
    }
    if matches!(x, Value::Number(_)) && matches!(y, Value::String(_)) {
        return is_loosely_equal(x, &Value::Number(to_number(y)?));
    }
    if matches!(x, Value::String(_)) && matches!(y, Value::Number(_)) {
        return is_loosely_equal(&Value::Number(to_number(x)?), y);
    }
    if matches!(x, Value::BigInt(_))
        && let Value::String(s) = y
    {
        let Some(n) = string_to_big_int(s) else {
            return Ok(false);
        };
        return is_loosely_equal(x, &Value::BigInt(Handle::new(n)));
    }
    if matches!(x, Value::String(_)) && matches!(y, Value::BigInt(_)) {
        return is_loosely_equal(y, x);
    }
    if matches!(x, Value::Boolean(_)) {
        return is_loosely_equal(&Value::Number(to_number(x)?), y);
    }
    if matches!(y, Value::Boolean(_)) {
        return is_loosely_equal(x, &Value::Number(to_number(y)?));
    }
    if let (Value::BigInt(a), Value::Number(b)) = (x, y) {
        return Ok(bigint_number_equal(a, *b));
    }
    if let (Value::Number(a), Value::BigInt(b)) = (x, y) {
        return Ok(bigint_number_equal(b, *a));
    }
    match (x, y) {
        // Number::equal for the same-type case.
        (Value::Number(a), Value::Number(b)) => Ok(a == b),
        _ => Ok(x == y),
    }
}

/// spec 7.2.15 step 12: loose equality between a BigInt and a Number.
fn bigint_number_equal(b: &BigInt, n: f64) -> bool {
    if n.is_nan() || n.is_infinite() {
        return false;
    }
    let int_n = n.trunc();
    if int_n != n {
        return false;
    }
    b.0 == f64_to_bigint_exact(int_n)
}

/// Exact conversion of an integer-valued `f64` to a BigInt.
fn f64_to_bigint_exact(n: f64) -> NumBigInt {
    debug_assert!(n.is_finite() && n.trunc() == n);
    let bits = n.to_bits();
    let sign = if bits >> 63 == 1 { -1 } else { 1 };
    let exponent = ((bits >> 52) & 0x7FF) as i32 - 1023;
    if exponent < 0 {
        // |n| < 1: with n integral this is only ±0.
        return NumBigInt::ZERO;
    }
    let mantissa = bits & ((1u64 << 52) - 1);
    let mut value = NumBigInt::from((1u64 << 52) | mantissa);
    let shift = exponent - 52;
    if shift >= 0 {
        value *= NumBigInt::from(2u64).pow(shift as u32);
    } else {
        value /= NumBigInt::from(2u64).pow((-shift) as u32);
    }
    if sign < 0 {
        value = -value;
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bigint::BigInt;
    use crate::string::JsString;
    use crate::symbol::Symbol;

    fn num(n: f64) -> Value {
        Value::Number(n)
    }

    fn str(s: &str) -> Value {
        Value::String(Handle::new(JsString::from_utf8(s)))
    }

    fn big(v: i64) -> Value {
        Value::BigInt(Handle::new(BigInt::from(v)))
    }

    #[test]
    fn same_value_nan_and_signed_zero() {
        assert!(same_value(&num(f64::NAN), &num(f64::NAN)));
        assert!(!same_value(&num(0.0), &num(-0.0)));
        assert!(same_value(&num(1.0), &num(1.0)));
        assert!(!same_value(&num(1.0), &num(1.5)));
        assert!(same_value(&str("a"), &str("a")));
        assert!(!same_value(&str("a"), &str("b")));
        assert!(same_value(&big(1), &big(1)));
        assert!(!same_value(&big(1), &big(2)));
        let s1 = Symbol::new(None);
        let s2 = s1.clone();
        let s3 = Symbol::new(None);
        assert!(same_value(
            &Value::Symbol(Handle::new(s1.clone())),
            &Value::Symbol(Handle::new(s2))
        ));
        assert!(!same_value(
            &Value::Symbol(Handle::new(s1)),
            &Value::Symbol(Handle::new(s3))
        ));
        assert!(!same_value(&Value::Undefined, &Value::Null));
    }

    #[test]
    fn same_value_zero_treats_signed_zeros_equally() {
        assert!(same_value_zero(&num(0.0), &num(-0.0)));
        assert!(same_value_zero(&num(f64::NAN), &num(f64::NAN)));
        assert!(!same_value_zero(&num(1.0), &num(2.0)));
    }

    #[test]
    fn strict_equality() {
        assert!(is_strictly_equal(&num(1.0), &num(1.0)));
        assert!(is_strictly_equal(&num(0.0), &num(-0.0)));
        assert!(!is_strictly_equal(&num(f64::NAN), &num(f64::NAN)));
        assert!(is_strictly_equal(&str("a"), &str("a")));
        assert!(is_strictly_equal(&big(1), &big(1)));
        assert!(!is_strictly_equal(&num(1.0), &str("1")));
        assert!(!is_strictly_equal(&Value::Undefined, &Value::Null));
    }

    #[test]
    fn is_integral_number_cases() {
        assert!(is_integral_number(5.0));
        assert!(is_integral_number(-0.0));
        assert!(!is_integral_number(5.5));
        assert!(!is_integral_number(f64::NAN));
        assert!(!is_integral_number(f64::INFINITY));
    }

    #[test]
    fn loose_equality_matrix() {
        assert!(is_loosely_equal(&Value::Null, &Value::Undefined).unwrap());
        assert!(is_loosely_equal(&num(1.0), &str("1")).unwrap());
        assert!(is_loosely_equal(&Value::Boolean(true), &num(1.0)).unwrap());
        assert!(is_loosely_equal(&Value::Boolean(false), &str("0")).unwrap());
        assert!(is_loosely_equal(&big(1), &num(1.0)).unwrap());
        assert!(is_loosely_equal(&big(1), &str("1")).unwrap());
        assert!(!is_loosely_equal(&big(1), &num(1.5)).unwrap());
        assert!(!is_loosely_equal(&big(1), &str("abc")).unwrap());
        assert!(!is_loosely_equal(&num(f64::NAN), &num(f64::NAN)).unwrap());
        assert!(!is_loosely_equal(&num(f64::NAN), &big(0)).unwrap());
        assert!(!is_loosely_equal(&Value::Null, &num(0.0)).unwrap());
        assert!(!is_loosely_equal(&num(0.0), &Value::Null).unwrap());
        assert!(is_loosely_equal(&str(""), &num(0.0)).unwrap());
        assert!(is_loosely_equal(&num(5.0), &num(5.0)).unwrap());
        assert!(is_loosely_equal(&str("a"), &str("a")).unwrap());
        assert!(!is_loosely_equal(&str("a"), &str("b")).unwrap());
        assert!(is_loosely_equal(&big(7), &big(7)).unwrap());
    }
}
