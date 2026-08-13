//! Type conversion abstract operations (spec 7.1).

use num_bigint::BigInt as NumBigInt;

use crate::bigint::{self, BigInt};
use crate::error::{ErrorKind, JsError};
use crate::number;
use crate::property::PropertyKey;
use crate::string::{JsString, intern};
use crate::value::{Value, is_callable};
use unicode::{is_line_terminator, is_white_space};

/// The hint argument of `ToPrimitive` (spec 7.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToPrimitiveHint {
    Default,
    Number,
    String,
}

/// StrWhiteSpaceChar (spec 11.2): a WhiteSpace or LineTerminator code point.
fn is_str_white_space(unit: u16) -> bool {
    is_white_space(unit as u32) || is_line_terminator(unit as u32)
}

/// spec 11.2: strip leading and trailing StrWhiteSpaceChar code units.
fn trimmed(units: &[u16]) -> &[u16] {
    let mut start = 0;
    let mut end = units.len();
    while start < end && is_str_white_space(units[start]) {
        start += 1;
    }
    while end > start && is_str_white_space(units[end - 1]) {
        end -= 1;
    }
    &units[start..end]
}

/// ToPrimitive (spec 7.1.1): primitives pass through unchanged; objects
/// convert via OrdinaryToPrimitive (the @@toPrimitive symbol joins with the
/// well-known symbol table in Phase 15).
pub fn to_primitive(value: &Value, hint: ToPrimitiveHint) -> Result<Value, JsError> {
    // spec 7.1.1 step 1.a: the @@toPrimitive method runs before the
    // valueOf/toString loop, and its abrupt completion wins.
    match value {
        Value::Object(obj) => {
            let key = crate::property::PropertyKey::Symbol(
                crate::symbol::well_known("toPrimitive").as_ref().clone(),
            );
            let method = obj.get_key(&key)?;
            if !matches!(method, Value::Undefined | Value::Null) {
                if !is_callable(&method) {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Symbol.toPrimitive is not a function".into(),
                    ));
                }
                return call_exotic_to_primitive(&method, value.clone(), hint);
            }
            ordinary_to_primitive(|name| obj.get(name), value.clone(), hint)
        }
        Value::Function(function) => {
            let key = crate::property::PropertyKey::Symbol(
                crate::symbol::well_known("toPrimitive").as_ref().clone(),
            );
            let method = function.get_key(&key)?;
            if !matches!(method, Value::Undefined | Value::Null) {
                if !is_callable(&method) {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "Symbol.toPrimitive is not a function".into(),
                    ));
                }
                return call_exotic_to_primitive(&method, value.clone(), hint);
            }
            ordinary_to_primitive(|name| function.get(name), value.clone(), hint)
        }
        _ => Ok(value.clone()),
    }
}

/// Call `method` (the @@toPrimitive hook) with the hint and reject an object
/// result (spec 7.1.1 steps 1.a.i-iii).
fn call_exotic_to_primitive(
    method: &Value,
    receiver: Value,
    hint: ToPrimitiveHint,
) -> Result<Value, JsError> {
    let hint_text = match hint {
        ToPrimitiveHint::String => "string",
        ToPrimitiveHint::Default => "default",
        ToPrimitiveHint::Number => "number",
    };
    let result = crate::function::call(
        method,
        receiver,
        &[Value::String(crate::handle::Handle::new(
            JsString::from_utf8(hint_text),
        ))],
    )?;
    if matches!(result, Value::Object(_) | Value::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot convert object to primitive value".into(),
        ));
    }
    Ok(result)
}

/// OrdinaryToPrimitive (spec 7.1.1.1): look up `toString`/`valueOf` on the
/// object and call the first callable one with the object as receiver.
/// A primitive wrapper object converts directly through its wrapped value.
fn ordinary_to_primitive(
    get: impl Fn(&JsString) -> Result<Value, JsError>,
    receiver: Value,
    hint: ToPrimitiveHint,
) -> Result<Value, JsError> {
    if let Value::Object(obj) = &receiver {
        // A String exotic object converts directly through its wrapped
        // value (its toString/valueOf return [[StringData]]), like the
        // boxed Number/BigInt/Boolean wrappers below.
        if let crate::object::ObjectKind::String(text) = &obj.kind {
            return Ok(Value::String(text.clone()));
        }
        if let Some(boxed) = &*obj.boxed.borrow() {
            return Ok(match boxed {
                crate::object::BoxedPrimitive::Number(n) => Value::Number(*n),
                crate::object::BoxedPrimitive::BigInt(b) => {
                    Value::BigInt(crate::handle::Handle::new(b.clone()))
                }
                crate::object::BoxedPrimitive::Boolean(b) => Value::Boolean(*b),
            });
        }
    }
    let (first, second) = match hint {
        ToPrimitiveHint::String => ("toString", "valueOf"),
        ToPrimitiveHint::Default | ToPrimitiveHint::Number => ("valueOf", "toString"),
    };
    for name in [first, second] {
        let key = JsString::from_utf8(name);
        let method = get(&key)?;
        if is_callable(&method) {
            // spec step 2.b.ii: a non-object result is the primitive; an
            // object result falls through to the next method name.
            let result = crate::function::call(&method, receiver.clone(), &[])?;
            if !matches!(result, Value::Object(_) | Value::Function(_)) {
                return Ok(result);
            }
        }
    }
    Err(JsError::new(
        ErrorKind::TypeError,
        "Cannot convert object to primitive value".into(),
    ))
}

/// ToBoolean (spec 7.1.2).
pub fn to_boolean(value: &Value) -> bool {
    match value {
        Value::Undefined | Value::Null => false,
        Value::Boolean(b) => *b,
        Value::Number(n) => !n.is_nan() && *n != 0.0,
        Value::String(s) => !s.is_empty(),
        Value::BigInt(b) => !b.is_zero(),
        Value::Symbol(_) => true,
        Value::Object(_) | Value::Function(_) => true,
    }
}

/// ToNumber (spec 7.1.4) for the primitive types.
pub fn to_number(value: &Value) -> Result<f64, JsError> {
    match value {
        Value::Number(n) => Ok(*n),
        Value::Undefined => Ok(f64::NAN),
        Value::Null => Ok(0.0),
        Value::Boolean(true) => Ok(1.0),
        Value::Boolean(false) => Ok(0.0),
        Value::String(s) => Ok(string_numeric_literal(s.as_slice())),
        Value::BigInt(_) => Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot convert a BigInt value to a number".into(),
        )),
        Value::Symbol(_) => Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot convert a Symbol value to a number".into(),
        )),
        Value::Object(_) | Value::Function(_) => {
            let prim = to_primitive(value, ToPrimitiveHint::Number)?;
            to_number(&prim)
        }
    }
}

/// The Number value of a StringNumericLiteral (spec 7.1.4.1), or NaN when
/// `text` is not a StringNumericLiteral. Whitespace-only strings are +0.
pub(crate) fn string_numeric_literal(text: &[u16]) -> f64 {
    let body = trimmed(text);
    if body.is_empty() {
        return 0.0;
    }
    decimal_literal(body)
        .or_else(|| hex_literal(body))
        .or_else(|| binary_literal(body))
        .or_else(|| octal_literal(body))
        .unwrap_or(f64::NAN)
}

/// A StrBinaryIntegerLiteral covering the whole slice (spec 7.1.4.1), or
/// None. A leading sign is not part of the literal (as with hex).
fn binary_literal(body: &[u16]) -> Option<f64> {
    if body.len() < 3 || body[0] != b'0' as u16 {
        return None;
    }
    if body[1] != b'b' as u16 && body[1] != b'B' as u16 {
        return None;
    }
    let mut value = 0.0;
    for &unit in &body[2..] {
        value = match unit {
            0x30 => value * 2.0,
            0x31 => value * 2.0 + 1.0,
            _ => return None,
        };
    }
    Some(value)
}

/// A StrOctalIntegerLiteral covering the whole slice (spec 7.1.4.1), or
/// None.
fn octal_literal(body: &[u16]) -> Option<f64> {
    if body.len() < 3 || body[0] != b'0' as u16 {
        return None;
    }
    if body[1] != b'o' as u16 && body[1] != b'O' as u16 {
        return None;
    }
    let mut value = 0.0;
    for &unit in &body[2..] {
        let digit = match unit {
            0x30..=0x37 => unit - 0x30,
            _ => return None,
        };
        value = value * 8.0 + digit as f64;
    }
    Some(value)
}

/// spec 7.1.18 StringToBigInt: the BigInt of a StringIntegerLiteral, or None
/// when the string is not a valid integer literal. Whitespace-only strings are
/// 0n.
pub fn string_to_bigint(text: &JsString) -> Option<crate::BigInt> {
    let body = trimmed(text.as_slice());
    if body.is_empty() {
        return Some(crate::BigInt::zero());
    }
    let (negative, rest) = match body.first() {
        Some(&u) if u == b'-' as u16 => (true, &body[1..]),
        Some(&u) if u == b'+' as u16 => (false, &body[1..]),
        _ => (false, body),
    };
    let (radix, digits) = match rest {
        [hi, lo, tail @ ..] if *hi == b'0' as u16 && (*lo == b'x' as u16 || *lo == b'X' as u16) => {
            (16, tail)
        }
        [hi, lo, tail @ ..] if *hi == b'0' as u16 && (*lo == b'o' as u16 || *lo == b'O' as u16) => {
            (8, tail)
        }
        [hi, lo, tail @ ..] if *hi == b'0' as u16 && (*lo == b'b' as u16 || *lo == b'B' as u16) => {
            (2, tail)
        }
        _ => (10, rest),
    };
    if digits.is_empty() {
        return None;
    }
    let mut value = crate::BigInt::zero();
    let radix = radix as i64;
    for unit in digits {
        let digit = match unit {
            u if (0x30..=0x39).contains(u) => *u - 0x30,
            u if (0x61..=0x7A).contains(u) => *u - 0x61 + 10,
            u if (0x41..=0x5A).contains(u) => *u - 0x41 + 10,
            _ => return None,
        };
        if digit >= radix as u16 {
            return None;
        }
        value = crate::bigint::add(
            &crate::bigint::multiply(&value, &crate::BigInt::from(radix)),
            &crate::BigInt::from(digit as i64),
        );
    }
    if negative {
        Some(crate::bigint::unary_minus(&value))
    } else {
        Some(value)
    }
}

/// A StrDecimalLiteral covering the whole slice, or None.
fn decimal_literal(body: &[u16]) -> Option<f64> {
    let (value, matched) = decimal_prefix(body)?;
    (matched == body.len()).then_some(value)
}

const INFINITY_TEXT: &[u16] = &[
    b'I' as u16,
    b'n' as u16,
    b'f' as u16,
    b'i' as u16,
    b'n' as u16,
    b'i' as u16,
    b't' as u16,
    b'y' as u16,
];

/// The longest StrDecimalLiteral prefix of `body`, as `(value, matched length)`.
fn decimal_prefix(body: &[u16]) -> Option<(f64, usize)> {
    let (sign, i) = match body.first() {
        Some(&u) if u == b'-' as u16 => (-1.0, 1),
        Some(&u) if u == b'+' as u16 => (1.0, 1),
        _ => (1.0, 0),
    };
    let rest = &body[i..];
    if rest.starts_with(INFINITY_TEXT) {
        return Some((sign * f64::INFINITY, i + INFINITY_TEXT.len()));
    }
    let mut digits_before = 0usize;
    while rest
        .get(digits_before)
        .is_some_and(|u| (0x30..=0x39).contains(u))
    {
        digits_before += 1;
    }
    let mut digits_after = 0usize;
    let mut j = digits_before;
    if rest.get(j) == Some(&(b'.' as u16)) {
        j += 1;
        while rest.get(j).is_some_and(|u| (0x30..=0x39).contains(u)) {
            digits_after += 1;
            j += 1;
        }
    }
    if digits_before == 0 && digits_after == 0 {
        return None;
    }
    let mut end = j;
    if rest
        .get(end)
        .is_some_and(|u| *u == b'e' as u16 || *u == b'E' as u16)
    {
        let mut k = end + 1;
        if rest
            .get(k)
            .is_some_and(|u| *u == b'+' as u16 || *u == b'-' as u16)
        {
            k += 1;
        }
        let exp_start = k;
        while rest.get(k).is_some_and(|u| (0x30..=0x39).contains(u)) {
            k += 1;
        }
        if k > exp_start {
            end = k;
        }
    }
    let matched = &rest[..end];
    if matched.is_empty() {
        return None;
    }
    let text: String = matched.iter().map(|&u| u as u8 as char).collect();
    let value: f64 = text.parse().ok()?;
    Some((sign * value, i + end))
}

/// A StrHexIntegerLiteral covering the whole slice (spec 7.1.4.1), or None.
fn hex_literal(body: &[u16]) -> Option<f64> {
    if body.len() < 3 || body[0] != b'0' as u16 {
        return None;
    }
    if body[1] != b'x' as u16 && body[1] != b'X' as u16 {
        return None;
    }
    let mut value = NumBigInt::ZERO;
    for &unit in &body[2..] {
        value = value * 16 + hex_digit(unit)?;
    }
    Some(bigint_to_f64(&value))
}

fn hex_digit(unit: u16) -> Option<u64> {
    match unit {
        0x30..=0x39 => Some((unit - 0x30) as u64),
        0x61..=0x66 => Some((unit - 0x61 + 10) as u64),
        0x41..=0x46 => Some((unit - 0x41 + 10) as u64),
        _ => None,
    }
}

/// Correctly rounded conversion; the exact decimal expansion is a valid float
/// literal that Rust parses with correct rounding.
fn bigint_to_f64(value: &NumBigInt) -> f64 {
    value.to_str_radix(10).parse().unwrap_or(f64::NAN)
}

/// parseFloat (spec 20.1.2.12): the Number value of the longest
/// StrDecimalLiteral prefix, or NaN.
pub fn parse_float(text: &JsString) -> f64 {
    let body = trimmed(text.as_slice());
    if body.is_empty() {
        return f64::NAN;
    }
    decimal_prefix(body).map_or(f64::NAN, |(value, _)| value)
}

/// parseInt (spec 20.1.2.13). `radix` is the already-`ToInt32`-converted
/// second argument, or 0 when absent.
pub fn parse_int(text: &JsString, radix: i32) -> f64 {
    let body = trimmed(text.as_slice());
    if body.is_empty() {
        return f64::NAN;
    }
    let (sign, rest) = match body.first() {
        Some(&u) if u == b'-' as u16 => (-1.0, &body[1..]),
        Some(&u) if u == b'+' as u16 => (1.0, &body[1..]),
        _ => (1.0, body),
    };
    if rest.is_empty() {
        return f64::NAN;
    }
    let has_hex_prefix = rest.len() >= 2
        && rest[0] == b'0' as u16
        && (rest[1] == b'x' as u16 || rest[1] == b'X' as u16);
    let radix = if radix == 0 {
        if has_hex_prefix { 16 } else { 10 }
    } else {
        radix
    };
    if !(2..=36).contains(&radix) {
        return f64::NAN;
    }
    let rest = if radix == 16 && has_hex_prefix {
        &rest[2..]
    } else {
        rest
    };
    let mut value = NumBigInt::ZERO;
    let mut any = false;
    for &unit in rest {
        let Some(d) = digit_value(unit, radix) else {
            break;
        };
        value = value * radix + d;
        any = true;
    }
    if !any {
        return f64::NAN;
    }
    if value.sign() == num_bigint::Sign::NoSign {
        return 0.0;
    }
    sign * bigint_to_f64(&value)
}

fn digit_value(unit: u16, radix: i32) -> Option<u64> {
    let d = match unit {
        0x30..=0x39 => (unit - 0x30) as u64,
        0x61..=0x7A => (unit - 0x61 + 10) as u64,
        0x41..=0x5A => (unit - 0x41 + 10) as u64,
        _ => return None,
    };
    (d < radix as u64).then_some(d)
}

/// ToNumeric (spec 7.1.3).
pub fn to_numeric(value: &Value) -> Result<Value, JsError> {
    let prim = to_primitive(value, ToPrimitiveHint::Number)?;
    if matches!(prim, Value::BigInt(_)) {
        Ok(prim)
    } else {
        Ok(Value::Number(to_number(&prim)?))
    }
}

/// ToString (spec 7.1.13) for the primitive types.
pub fn to_string(value: &Value) -> Result<JsString, JsError> {
    match value {
        Value::String(s) => Ok(s.as_ref().clone()),
        Value::Undefined => Ok(JsString::from_utf8("undefined")),
        Value::Null => Ok(JsString::from_utf8("null")),
        Value::Boolean(true) => Ok(JsString::from_utf8("true")),
        Value::Boolean(false) => Ok(JsString::from_utf8("false")),
        Value::Number(n) => Ok(number::to_string(*n)),
        Value::BigInt(b) => Ok(JsString::from_utf8(&bigint::to_string(b, 10))),
        Value::Symbol(_) => Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot convert a Symbol value to a string".into(),
        )),
        Value::Object(_) | Value::Function(_) => {
            let prim = to_primitive(value, ToPrimitiveHint::String)?;
            to_string(&prim)
        }
    }
}

/// ToBigInt (spec 7.1.16) for the primitive types.
pub fn to_big_int(value: &Value) -> Result<BigInt, JsError> {
    let prim = to_primitive(value, ToPrimitiveHint::Number)?;
    match prim {
        Value::Undefined | Value::Null => Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot convert undefined or null to a BigInt".into(),
        )),
        Value::Boolean(true) => Ok(BigInt::from(1u64)),
        Value::Boolean(false) => Ok(BigInt::from(0u64)),
        Value::BigInt(b) => Ok(b.as_ref().clone()),
        Value::String(s) => {
            // ToBigInt('') is 0n (a whitespace-only StringIntegerLiteral);
            // anything else must be a strict decimal integer literal.
            if trimmed(s.as_slice()).is_empty() {
                return Ok(crate::BigInt::zero());
            }
            match string_to_big_int(&s) {
                Some(n) => Ok(n),
                None => Err(JsError::new(
                    ErrorKind::SyntaxError,
                    "Cannot convert the string to a BigInt".into(),
                )),
            }
        }
        Value::Number(n) => match BigInt::from_f64_exact(n) {
            Some(b) => Ok(b),
            None => Err(JsError::new(
                ErrorKind::RangeError,
                "The number cannot be converted to a BigInt because it is not an integer".into(),
            )),
        },
        Value::Symbol(_) => Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot convert a Symbol to a BigInt".into(),
        )),
        // to_primitive above rejects objects, but keep the match exhaustive.
        Value::Object(_) | Value::Function(_) => Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot convert an object to a BigInt".into(),
        )),
    }
}

/// StringToBigInt (spec 7.1.17): `None` when `text` is not a
/// StringIntegerLiteral.
pub fn string_to_big_int(text: &JsString) -> Option<BigInt> {
    let body = trimmed(text.as_slice());
    if body.is_empty() {
        return None;
    }
    let (negative, rest) = match body.first() {
        Some(&u) if u == b'-' as u16 => (true, &body[1..]),
        Some(&u) if u == b'+' as u16 => (false, &body[1..]),
        _ => (false, body),
    };
    if rest.is_empty() {
        return None;
    }
    // StrDecimalIntegerLiteral: 0 | NonZeroDigit DecimalDigits_opt.
    let mut value = NumBigInt::ZERO;
    for (idx, &unit) in rest.iter().enumerate() {
        if !(0x30..=0x39).contains(&unit) {
            return None;
        }
        let d = (unit - 0x30) as u64;
        if idx == 0 && d == 0 && rest.len() > 1 {
            return None; // leading zeros are not allowed
        }
        value = value * 10 + d;
    }
    if negative {
        value = -value;
    }
    Some(BigInt(value))
}

/// ToBigInt64 (spec 7.1.18): wrap modulo 2^64 as a signed value.
pub fn to_big_int64(value: &Value) -> Result<BigInt, JsError> {
    let n = to_big_int(value)?;
    Ok(BigInt(wrap_64(&n.0, true)))
}

/// ToBigUint64 (spec 7.1.19): wrap modulo 2^64 as an unsigned value.
pub fn to_big_uint64(value: &Value) -> Result<BigInt, JsError> {
    let n = to_big_int(value)?;
    Ok(BigInt(wrap_64(&n.0, false)))
}

fn wrap_64(n: &NumBigInt, signed: bool) -> NumBigInt {
    let two_64 = NumBigInt::from(2u64).pow(64);
    let two_63 = NumBigInt::from(2u64).pow(63);
    let mut r = ((n % &two_64) + &two_64) % &two_64;
    if signed && r >= two_63 {
        r -= &two_64;
    }
    r
}

/// ToIntegerOrInfinity (spec 7.1.5) on an already-converted Number.
pub fn to_integer_or_infinity(number: f64) -> f64 {
    if number.is_nan() || number == 0.0 {
        0.0
    } else if number.is_infinite() {
        number
    } else {
        number.trunc()
    }
}

/// ToLength (spec 7.1.17) on an already-converted Number.
pub fn to_length(number: f64) -> u64 {
    let int = to_integer_or_infinity(number);
    if int <= 0.0 {
        0
    } else if int >= 9007199254740991.0 {
        9007199254740991
    } else {
        int as u64
    }
}

/// ToIndex (spec 7.1.18).
pub fn to_index(value: &Value) -> Result<u64, JsError> {
    if matches!(value, Value::Undefined) {
        return Ok(0);
    }
    let number = to_number(value)?;
    let integer = to_integer_or_infinity(number);
    if integer < 0.0 || integer >= 9007199254740991.0 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "Index out of range".into(),
        ));
    }
    Ok(integer as u64)
}

/// ToUint32 (spec 7.1.7) on an already-converted Number.
pub fn to_uint32(number: f64) -> u32 {
    if !number.is_finite() || number == 0.0 {
        return 0;
    }
    number.trunc().rem_euclid(4294967296.0) as u32
}

/// ToInt32 (spec 7.1.6) on an already-converted Number.
pub fn to_int32(number: f64) -> i32 {
    if !number.is_finite() || number == 0.0 {
        return 0;
    }
    let m = number.trunc().rem_euclid(4294967296.0);
    let m = if m >= 2147483648.0 {
        m - 4294967296.0
    } else {
        m
    };
    m as i32
}

/// ToUint8Clamp (spec 7.1.9) on an already-converted Number.
pub fn to_uint8_clamp(number: f64) -> u8 {
    if number.is_nan() || number <= 0.0 {
        return 0;
    }
    if number >= 255.0 {
        return 255;
    }
    let f = number.floor();
    if f + 0.5 < number {
        return (f + 1.0) as u8;
    }
    if number < f + 0.5 {
        return f as u8;
    }
    if f % 2.0 == 1.0 {
        (f + 1.0) as u8
    } else {
        f as u8
    }
}

/// ToPropertyKey (spec 7.1.14) — object handling joins in Phase 5.
pub fn to_property_key(value: &Value) -> Result<PropertyKey, JsError> {
    let key = to_primitive(value, ToPrimitiveHint::String)?;
    match key {
        Value::Symbol(sym) => Ok(PropertyKey::Symbol(sym.as_ref().clone())),
        other => {
            let text = to_string(&other)?;
            Ok(PropertyKey::String(intern(text.as_slice())))
        }
    }
}

/// RequireObjectCoercible (spec 7.1.10).
pub fn require_object_coercible(value: &Value) -> Result<(), JsError> {
    match value {
        Value::Undefined | Value::Null => Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot convert undefined or null to object".into(),
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::Handle;

    fn str(s: &str) -> Value {
        Value::String(Handle::new(JsString::from_utf8(s)))
    }

    fn num(n: f64) -> Value {
        Value::Number(n)
    }

    fn big(v: i64) -> Value {
        Value::BigInt(Handle::new(BigInt::from(v)))
    }

    #[test]
    fn to_primitive_is_identity_for_primitives() {
        for v in [
            Value::Undefined,
            Value::Null,
            Value::Boolean(true),
            num(1.5),
            str("x"),
        ] {
            assert_eq!(to_primitive(&v, ToPrimitiveHint::Default).unwrap(), v);
        }
    }

    #[test]
    fn to_boolean_table() {
        assert!(!to_boolean(&Value::Undefined));
        assert!(!to_boolean(&Value::Null));
        assert!(!to_boolean(&Value::Boolean(false)));
        assert!(to_boolean(&Value::Boolean(true)));
        assert!(!to_boolean(&num(0.0)));
        assert!(!to_boolean(&num(-0.0)));
        assert!(!to_boolean(&num(f64::NAN)));
        assert!(to_boolean(&num(0.1)));
        assert!(!to_boolean(&str("")));
        assert!(to_boolean(&str("x")));
        assert!(!to_boolean(&big(0)));
        assert!(to_boolean(&big(-1)));
        assert!(to_boolean(&Value::Symbol(Handle::new(
            crate::symbol::Symbol::new(None)
        ))));
    }

    #[test]
    fn to_number_primitives() {
        assert!(to_number(&Value::Undefined).unwrap().is_nan());
        assert_eq!(to_number(&Value::Null).unwrap(), 0.0);
        assert_eq!(to_number(&Value::Boolean(true)).unwrap(), 1.0);
        assert_eq!(to_number(&Value::Boolean(false)).unwrap(), 0.0);
        assert_eq!(to_number(&num(42.5)).unwrap(), 42.5);
        assert_eq!(to_number(&str("12.5")).unwrap(), 12.5);
        assert!(to_number(&big(1)).is_err());
        assert!(
            to_number(&Value::Symbol(Handle::new(crate::symbol::Symbol::new(
                None
            ))))
            .is_err()
        );
    }

    #[test]
    fn to_number_string_cases() {
        assert_eq!(to_number(&str("")).unwrap(), 0.0);
        assert_eq!(to_number(&str("   ")).unwrap(), 0.0);
        assert_eq!(to_number(&str("0x10")).unwrap(), 16.0);
        assert_eq!(to_number(&str("0X1f")).unwrap(), 31.0);
        assert_eq!(to_number(&str("1.5e3")).unwrap(), 1500.0);
        assert_eq!(to_number(&str("  -12.5  ")).unwrap(), -12.5);
        assert_eq!(to_number(&str("Infinity")).unwrap(), f64::INFINITY);
        assert!(to_number(&str("-0x10")).unwrap().is_nan());
        assert_eq!(to_number(&str("0b10")).unwrap(), 2.0);
        assert_eq!(to_number(&str("0o10")).unwrap(), 8.0);
        assert_eq!(to_number(&str("0B11")).unwrap(), 3.0);
        assert_eq!(to_number(&str("0O17")).unwrap(), 15.0);
        assert!(to_number(&str("-0b10")).unwrap().is_nan());
        assert!(to_number(&str("0b2")).unwrap().is_nan());
        assert!(to_number(&str("0o8")).unwrap().is_nan());
        assert!(to_number(&str("0b")).unwrap().is_nan());
        assert!(to_number(&str("0o")).unwrap().is_nan());
        assert!(to_number(&str("abc")).unwrap().is_nan());
        assert!(to_number(&str("10e")).unwrap().is_nan());
        assert!(to_number(&str("0x")).unwrap().is_nan());
        assert!(to_number(&str("1.5.5")).unwrap().is_nan());
    }

    #[test]
    fn parse_float_cases() {
        assert_eq!(parse_float(&JsString::from_utf8("123.456e-2")), 1.23456);
        assert_eq!(parse_float(&JsString::from_utf8("0x10")), 0.0);
        assert_eq!(parse_float(&JsString::from_utf8("Infinity")), f64::INFINITY);
        assert!(parse_float(&JsString::from_utf8("infinity")).is_nan());
        assert_eq!(parse_float(&JsString::from_utf8("  -12.5  ")), -12.5);
        assert_eq!(parse_float(&JsString::from_utf8("123abc")), 123.0);
        assert_eq!(parse_float(&JsString::from_utf8("1e999")), f64::INFINITY);
        assert!(parse_float(&JsString::from_utf8("")).is_nan());
        assert!(parse_float(&JsString::from_utf8("  ")).is_nan());
        assert_eq!(parse_float(&JsString::from_utf8("+5.5")), 5.5);
        assert_eq!(parse_float(&JsString::from_utf8(".5")), 0.5);
    }

    #[test]
    fn parse_int_cases() {
        assert_eq!(parse_int(&JsString::from_utf8("0x10"), 0), 16.0);
        assert_eq!(parse_int(&JsString::from_utf8("010"), 0), 10.0);
        assert_eq!(parse_int(&JsString::from_utf8("0b11"), 0), 0.0);
        assert_eq!(parse_int(&JsString::from_utf8("  -123  "), 0), -123.0);
        assert_eq!(parse_int(&JsString::from_utf8("1e5"), 0), 1.0);
        assert_eq!(parse_int(&JsString::from_utf8("ff"), 16), 255.0);
        assert_eq!(parse_int(&JsString::from_utf8("101"), 2), 5.0);
        assert_eq!(parse_int(&JsString::from_utf8("-0x10"), 0), -16.0);
        assert!(parse_int(&JsString::from_utf8(""), 0).is_nan());
        assert!(parse_int(&JsString::from_utf8("   "), 0).is_nan());
        assert!(parse_int(&JsString::from_utf8("0x"), 0).is_nan());
        assert!(parse_int(&JsString::from_utf8("10"), 1).is_nan());
        assert!(parse_int(&JsString::from_utf8("10"), 37).is_nan());
        assert!(parse_int(&JsString::from_utf8("z"), 10).is_nan());
    }

    #[test]
    fn to_numeric_selects_number_for_strings() {
        let n = to_numeric(&str("1.5")).unwrap();
        assert!(matches!(n, Value::Number(v) if v == 1.5));
        let b = to_numeric(&big(7)).unwrap();
        assert!(matches!(b, Value::BigInt(_)));
    }

    #[test]
    fn to_string_conversions() {
        assert_eq!(
            to_string(&Value::Undefined).unwrap().to_string_lossy(),
            "undefined"
        );
        assert_eq!(to_string(&Value::Null).unwrap().to_string_lossy(), "null");
        assert_eq!(
            to_string(&Value::Boolean(true)).unwrap().to_string_lossy(),
            "true"
        );
        assert_eq!(to_string(&num(1e21)).unwrap().to_string_lossy(), "1e+21");
        assert_eq!(to_string(&big(-42)).unwrap().to_string_lossy(), "-42");
        assert!(
            to_string(&Value::Symbol(Handle::new(crate::symbol::Symbol::new(
                None
            ))))
            .is_err()
        );
    }

    #[test]
    fn to_big_int_cases() {
        assert_eq!(to_big_int(&str("123")).unwrap(), BigInt::from(123));
        assert_eq!(to_big_int(&str("  -42 ")).unwrap(), BigInt::from(-42));
        assert_eq!(to_big_int(&Value::Boolean(true)).unwrap(), BigInt::from(1));
        assert_eq!(to_big_int(&big(9)).unwrap(), BigInt::from(9));
        assert_eq!(to_big_int(&str("")).unwrap(), BigInt::from(0));
        assert!(to_big_int(&str("1.5")).is_err());
        assert!(to_big_int(&str("0x10")).is_err());
        assert!(to_big_int(&str("07")).is_err());
        assert!(to_big_int(&Value::Null).is_err());
        assert!(to_big_int(&Value::Undefined).is_err());
        assert!(to_big_int(&num(1.5)).is_err());
        assert!(
            to_big_int(&Value::Symbol(Handle::new(crate::symbol::Symbol::new(
                None
            ))))
            .is_err()
        );
    }

    #[test]
    fn string_to_big_int_accepts_zero_and_signs() {
        assert_eq!(
            string_to_big_int(&JsString::from_utf8("0")).unwrap(),
            BigInt::from(0)
        );
        assert_eq!(
            string_to_big_int(&JsString::from_utf8("+7")).unwrap(),
            BigInt::from(7)
        );
        assert_eq!(
            string_to_big_int(&JsString::from_utf8("-7")).unwrap(),
            BigInt::from(-7)
        );
        assert!(string_to_big_int(&JsString::from_utf8("00")).is_none());
    }

    #[test]
    fn to_big_int64_wraps_signed() {
        assert_eq!(to_big_int64(&big(-1)).unwrap(), BigInt::from(-1));
        // 2^63 wraps to -2^63.
        let two_63 = BigInt(NumBigInt::from(2u64).pow(63));
        let wrapped = to_big_int64(&Value::BigInt(Handle::new(two_63))).unwrap();
        assert_eq!(wrapped, BigInt(-NumBigInt::from(2u64).pow(63)));
    }

    #[test]
    fn to_big_uint64_wraps_unsigned() {
        assert_eq!(
            to_big_uint64(&big(-1)).unwrap(),
            BigInt(NumBigInt::from(2u64).pow(64) - 1)
        );
        assert_eq!(to_big_uint64(&big(0)).unwrap(), BigInt::from(0));
    }

    #[test]
    fn to_integer_or_infinity_cases() {
        assert_eq!(to_integer_or_infinity(1.5), 1.0);
        assert_eq!(to_integer_or_infinity(-1.5), -1.0);
        assert_eq!(to_integer_or_infinity(f64::NAN), 0.0);
        assert_eq!(to_integer_or_infinity(-0.0), 0.0);
        assert_eq!(to_integer_or_infinity(f64::INFINITY), f64::INFINITY);
        assert_eq!(to_integer_or_infinity(f64::NEG_INFINITY), f64::NEG_INFINITY);
    }

    #[test]
    fn to_length_cases() {
        assert_eq!(to_length(-1.0), 0);
        assert_eq!(to_length(f64::NAN), 0);
        assert_eq!(to_length(3.9), 3);
        assert_eq!(to_length(9007199254740991.0), 9007199254740991);
        assert_eq!(to_length(f64::INFINITY), 9007199254740991);
    }

    #[test]
    fn to_index_cases() {
        assert_eq!(to_index(&Value::Undefined).unwrap(), 0);
        assert_eq!(to_index(&num(5.9)).unwrap(), 5);
        assert!(to_index(&num(-1.0)).is_err());
        assert!(to_index(&num(9007199254740992.0)).is_err());
        assert!(to_index(&num(f64::INFINITY)).is_err());
    }

    #[test]
    fn to_uint32_cases() {
        assert_eq!(to_uint32(4294967296.0), 0);
        assert_eq!(to_uint32(-1.0), 4294967295);
        assert_eq!(to_uint32(1.9), 1);
        assert_eq!(to_uint32(f64::NAN), 0);
        assert_eq!(to_uint32(f64::INFINITY), 0);
        assert_eq!(to_uint32(-0.0), 0);
    }

    #[test]
    fn to_int32_cases() {
        assert_eq!(to_int32(2147483648.0), -2147483648);
        assert_eq!(to_int32(4294967296.0), 0);
        assert_eq!(to_int32(-1.0), -1);
        assert_eq!(to_int32(1.9), 1);
        assert_eq!(to_int32(f64::NAN), 0);
    }

    #[test]
    fn to_uint8_clamp_table() {
        assert_eq!(to_uint8_clamp(f64::NAN), 0);
        assert_eq!(to_uint8_clamp(-1.0), 0);
        assert_eq!(to_uint8_clamp(0.0), 0);
        assert_eq!(to_uint8_clamp(255.0), 255);
        assert_eq!(to_uint8_clamp(255.5), 255);
        assert_eq!(to_uint8_clamp(0.5), 0);
        assert_eq!(to_uint8_clamp(1.5), 2);
        assert_eq!(to_uint8_clamp(2.5), 2);
        assert_eq!(to_uint8_clamp(3.5), 4);
        assert_eq!(to_uint8_clamp(254.5), 254);
        assert_eq!(to_uint8_clamp(100.0), 100);
    }

    #[test]
    fn to_property_key_cases() {
        assert_eq!(
            to_property_key(&str("foo")).unwrap(),
            PropertyKey::from_utf8("foo")
        );
        assert_eq!(
            to_property_key(&num(5.0)).unwrap(),
            PropertyKey::from_utf8("5")
        );
        let sym = crate::symbol::Symbol::new(Some(JsString::from_utf8("k")));
        assert_eq!(
            to_property_key(&Value::Symbol(Handle::new(sym.clone()))).unwrap(),
            PropertyKey::Symbol(sym)
        );
    }

    #[test]
    fn require_object_coercible_cases() {
        assert!(require_object_coercible(&Value::Undefined).is_err());
        assert!(require_object_coercible(&Value::Null).is_err());
        assert!(require_object_coercible(&str("x")).is_ok());
        assert!(require_object_coercible(&num(0.0)).is_ok());
    }
}
