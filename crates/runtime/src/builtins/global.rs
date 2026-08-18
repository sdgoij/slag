//! Global function properties (spec 19.2): `isFinite`, `isNaN`, `parseFloat`,
//! `parseInt`, and the URI handling functions (`encodeURI[Component]` /
//! `decodeURI[Component]`). Their ToString/ToNumber arguments can be objects,
//! so the conversions run through the agent (recovered from the crux
//! `with_agent` window) instead of the pure crux helpers.

use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use crate::realm::Realm;

fn arg(args: &[Value], index: usize) -> Value {
    args.get(index).cloned().unwrap_or(Value::Undefined)
}

fn str(value: &str) -> Value {
    Value::String(Handle::new(JsString::from_utf8(value)))
}

fn uri_error() -> JsError {
    JsError::new(ErrorKind::UriError, "URI malformed".into())
}

/// The agent recorded by the innermost `crux::function::with_agent` window;
/// the global functions are plain crux closures and cannot take it as a
/// parameter, but they only run inside those windows.
fn current_agent_mut() -> Result<&'static mut crate::agent::Agent, JsError> {
    let agent = crux::function::current_agent();
    if agent.is_null() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "host global called outside an agent window".into(),
        ));
    }
    // SAFETY: `with_agent` guarantees a live `&mut Agent` for the duration of
    // the enclosing call.
    Ok(unsafe { &mut *(agent as *mut crate::agent::Agent) })
}

/// ToString with the agent's ToPrimitive dispatch (the pure crux `to_string`
/// cannot call the %Object.prototype.toString%/valueOf builtins).
fn to_string_agent(value: &Value) -> Result<JsString, JsError> {
    crate::context::to_string(current_agent_mut()?, value)
}

/// ToNumber with the agent's ToPrimitive dispatch.
fn to_number_agent(value: &Value) -> Result<f64, JsError> {
    crate::context::to_number(current_agent_mut()?, value)
}

/// Install the eight global function properties during
/// SetDefaultGlobalBindings (spec 19.2).
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    // CreateBuiltinFunction (spec 10.2.3 step 1): the [[Prototype]] defaults
    // to %Function.prototype%. The realm's post-pass only links intrinsics,
    // and these functions live on the global object, so set it here.
    let function_proto = realm
        .intrinsics
        .get("%Function.prototype%")
        .and_then(|value| match value.kind() {
            ValueKind::Function(function) => function.object.handle(),
            _ => None,
        });
    for (name, length, call) in [
        (
            "isFinite",
            1,
            is_finite as fn(&Value, &[Value]) -> Result<Value, JsError>,
        ),
        ("isNaN", 1, is_nan),
        ("parseFloat", 1, parse_float),
        ("parseInt", 2, parse_int),
        ("encodeURI", 1, |_, args| encode(args, false)),
        ("encodeURIComponent", 1, |_, args| encode(args, true)),
        ("decodeURI", 1, |_, args| decode(args, false)),
        ("decodeURIComponent", 1, |_, args| decode(args, true)),
        ("escape", 1, escape),
        ("unescape", 1, unescape),
    ] {
        let function = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            Box::new(call),
            None,
            None,
        )?;
        if let Some(function_proto) = &function_proto {
            function
                .object
                .set_prototype_of(Some(function_proto.clone()))?;
        }
        realm.global_object.define_property_or_throw(
            &JsString::from_utf8(name),
            &crux::property::PropertyDescriptor {
                value: Some(Value::Function(function)),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
    }
    Ok(())
}

/// escape (spec B.2.1.1): keep the URL-safe set verbatim, percent-encode
/// code units below 256 as `%XX` and the rest as `%uXXXX` (uppercase hex).
fn escape(_this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let text = to_string_agent(&arg(args, 0))?;
    let mut out = String::new();
    for &unit in text.as_slice() {
        match unit {
            0x41..=0x5A
            | 0x61..=0x7A
            | 0x30..=0x39
            | 0x40
            | 0x2A
            | 0x5F
            | 0x2B
            | 0x2D
            | 0x2E
            | 0x2F => {
                out.push(char::from_u32(unit as u32).unwrap());
            }
            0..=0xFF => out.push_str(&format!("%{unit:02X}")),
            _ => out.push_str(&format!("%u{unit:04X}")),
        }
    }
    Ok(str(&out))
}

/// unescape (spec B.2.1.2): decode `%uXXXX` (four hex digits, case
/// insensitive) and `%XX` (two); a `%` that does not start a valid escape is
/// kept literally and the scan continues after it.
fn unescape(_this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let text = to_string_agent(&arg(args, 0))?;
    let units = text.as_slice();
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let mut index = 0;
    while index < units.len() {
        if units[index] == b'%' as u16 {
            if index + 5 < units.len()
                && units[index + 1] == b'u' as u16
                && let Some(value) = hex4(&units[index + 2..index + 6])
            {
                out.push(value);
                index += 6;
                continue;
            }
            if index + 2 < units.len()
                && let Some(value) = hex2(&units[index + 1..index + 3])
            {
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(units[index]);
        index += 1;
    }
    Ok(Value::String(Handle::new(JsString::from_utf16(&out))))
}

fn hex2(units: &[u16]) -> Option<u16> {
    Some(hex_value(units[0])? as u16 * 16 + hex_value(units[1])? as u16)
}

fn hex4(units: &[u16]) -> Option<u16> {
    Some(
        hex_value(units[0])? as u16 * 4096
            + hex_value(units[1])? as u16 * 256
            + hex_value(units[2])? as u16 * 16
            + hex_value(units[3])? as u16,
    )
}

/// isFinite (spec 19.2.2): ToNumber (abrupt completions propagate), then
/// false for NaN and infinities.
fn is_finite(_this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let number = to_number_agent(&arg(args, 0))?;
    Ok(Value::Boolean(number.is_finite()))
}

/// isNaN (spec 19.2.3).
fn is_nan(_this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let number = to_number_agent(&arg(args, 0))?;
    Ok(Value::Boolean(number.is_nan()))
}

/// The code points of the `uriReserved` set (`decodeURI`'s keep-set).
const URI_RESERVED: &[u8] = b";/?:@&=+$,#";
/// The code points of the `uriUnescaped` set (`encodeURIComponent`'s
/// keep-set).
const URI_UNESCAPED: &[u8] = b"-_.!~*'()";
/// `encodeURI`'s keep-set: `uriReserved` plus `uriUnescaped`.
const URI_UNESCAPED_RESERVED: &[u8] = b";/?:@&=+$,#-_.!~*'()";

fn in_set(cp: u32, set: &[u8]) -> bool {
    cp < 0x80 && set.contains(&(cp as u8))
}

/// Encode (spec 19.2.6.5): percent-encode every code point outside the
/// unescaped set as UTF-8, rejecting lone surrogates.
fn encode(args: &[Value], component: bool) -> Result<Value, JsError> {
    let string = to_string_agent(&arg(args, 0))?;
    let unescaped = if component {
        URI_UNESCAPED
    } else {
        URI_UNESCAPED_RESERVED
    };
    let units = string.as_slice();
    let mut out = String::new();
    let mut index = 0;
    while index < units.len() {
        let (cp, unpaired, count) = string.code_point_at(index).unwrap();
        // The unescaped set plus the always-safe ASCII alphanumerics.
        let alphanumeric = matches!(cp, 0x30..=0x39 | 0x41..=0x5A | 0x61..=0x7A);
        if in_set(cp, unescaped) || alphanumeric {
            out.push(cp as u8 as char);
        } else if unpaired {
            return Err(uri_error());
        } else {
            let Some(ch) = char::from_u32(cp) else {
                return Err(uri_error());
            };
            let mut buffer = [0u8; 4];
            for &byte in ch.encode_utf8(&mut buffer).as_bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        }
        index += count;
    }
    Ok(str(&out))
}

/// The value of a hex digit code unit, or `None`.
fn hex_value(unit: u16) -> Option<u8> {
    match unit {
        0x30..=0x39 => Some((unit - b'0' as u16) as u8),
        0x61..=0x66 => Some((unit - b'a' as u16 + 10) as u8),
        0x41..=0x46 => Some((unit - b'A' as u16 + 10) as u8),
        _ => None,
    }
}

/// Decode (spec 19.2.6.6): un-percent-encode `%XX` escapes, validating the
/// UTF-8 of multi-byte sequences; the reserved set keeps its escapes literal.
fn decode(args: &[Value], component: bool) -> Result<Value, JsError> {
    let string = to_string_agent(&arg(args, 0))?;
    let reserved = if component { b"" } else { URI_RESERVED };
    let units = string.as_slice();
    let mut out = Vec::<u16>::new();
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        if unit != b'%' as u16 {
            out.push(unit);
            index += 1;
            continue;
        }
        let (Some(&hi), Some(&lo)) = (units.get(index + 1), units.get(index + 2)) else {
            return Err(uri_error());
        };
        let (Some(high), Some(low)) = (hex_value(hi), hex_value(lo)) else {
            return Err(uri_error());
        };
        let byte = (high << 4) | low;
        index += 3;
        if byte < 0x80 {
            if reserved.contains(&byte) {
                out.push(b'%' as u16);
                out.push(hi);
                out.push(lo);
            } else {
                out.push(byte as u16);
            }
            continue;
        }
        // A multi-byte UTF-8 sequence: the first byte fixes the length.
        let byte_count = if byte & 0xE0 == 0xC0 {
            2
        } else if byte & 0xF0 == 0xE0 {
            3
        } else if byte & 0xF8 == 0xF0 {
            4
        } else {
            return Err(uri_error());
        };
        let mut octets = vec![byte];
        for _ in 1..byte_count {
            let Some(&percent) = units.get(index) else {
                return Err(uri_error());
            };
            if percent != b'%' as u16 {
                return Err(uri_error());
            }
            let (Some(&hi), Some(&lo)) = (units.get(index + 1), units.get(index + 2)) else {
                return Err(uri_error());
            };
            let (Some(high), Some(low)) = (hex_value(hi), hex_value(lo)) else {
                return Err(uri_error());
            };
            let next = (high << 4) | low;
            if !(0x80..=0xBF).contains(&next) {
                return Err(uri_error());
            }
            octets.push(next);
            index += 3;
        }
        // str::from_utf8 rejects overlong encodings, surrogates, and code
        // points above U+10FFFF — the Decode malformed cases.
        let text = std::str::from_utf8(&octets).map_err(|_| uri_error())?;
        let mut chars = text.chars();
        let (Some(ch), None) = (chars.next(), chars.next()) else {
            return Err(uri_error());
        };
        let cp = ch as u32;
        if cp <= 0xFFFF {
            out.push(cp as u16);
        } else {
            let value = cp - 0x10000;
            out.push(0xD800 + (value >> 10) as u16);
            out.push(0xDC00 + (value & 0x3FF) as u16);
        }
    }
    Ok(Value::String(Handle::new(JsString::from_utf16(&out))))
}

/// The integer value of a digit code unit in `radix`, or `None`.
fn digit_value(unit: u16) -> Option<u32> {
    match unit {
        0x30..=0x39 => Some((unit - b'0' as u16) as u32),
        0x61..=0x7A => Some((unit - b'a' as u16 + 10) as u32),
        0x41..=0x5A => Some((unit - b'A' as u16 + 10) as u32),
        _ => None,
    }
}

/// parseInt (spec 19.2.5): optional sign and radix, with the 0x/0o/0b
/// inference when the radix is 0 or 16.
pub(crate) fn parse_int(_this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let string = to_string_agent(&arg(args, 0))?;
    // spec step 6: R = ℝ(? ToInt32(radix)); NaN and infinities map to 0.
    let radix_value = to_number_agent(&arg(args, 1))?;
    let units = string.as_slice();
    let mut index = 0;
    // Leading white space and line terminators (spec step 5).
    while index < units.len() && is_whitespace(units[index]) {
        index += 1;
    }
    if index >= units.len() {
        return Ok(Value::Number(f64::NAN));
    }
    let mut sign = 1.0;
    match units[index] {
        0x2D => {
            sign = -1.0;
            index += 1;
        }
        0x2B => index += 1,
        _ => {}
    }
    // spec steps 10-13: R = ToInt32(radix); the 0x/0o/0b prefixes are only
    // stripped when R is 0 or 16.
    let r = crux::convert::to_int32(radix_value);
    let strip_prefix = r == 0 || r == 16;
    if r != 0 && !(2..=36).contains(&r) {
        return Ok(Value::Number(f64::NAN));
    }
    let mut effective = if r == 0 { 10 } else { r as u32 };
    if strip_prefix && index + 1 < units.len() && units[index] == b'0' as u16 {
        match units[index + 1] {
            0x78 | 0x58 => {
                effective = 16;
                index += 2;
            }
            0x6F | 0x4F => {
                effective = 8;
                index += 2;
            }
            0x62 | 0x42 => {
                effective = 2;
                index += 2;
            }
            _ => {}
        }
    }
    if index >= units.len() {
        return Ok(Value::Number(f64::NAN));
    }
    let mut value: f64 = 0.0;
    let mut saw_digit = false;
    while index < units.len() {
        let Some(digit) = digit_value(units[index]) else {
            break;
        };
        if digit >= effective {
            break;
        }
        value = value * effective as f64 + digit as f64;
        saw_digit = true;
        index += 1;
    }
    if !saw_digit {
        return Ok(Value::Number(f64::NAN));
    }
    Ok(Value::Number(sign * value))
}

/// parseFloat (spec 19.2.4): the longest prefix matching the decimal-literal
/// grammar, or NaN.
pub(crate) fn parse_float(_this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let string = to_string_agent(&arg(args, 0))?;
    let units = string.as_slice();
    let mut index = 0;
    while index < units.len() && is_whitespace(units[index]) {
        index += 1;
    }
    let mut sign = 1.0;
    match units.get(index).copied() {
        Some(0x2D) => {
            sign = -1.0;
            index += 1;
        }
        Some(0x2B) => index += 1,
        _ => {}
    }
    if index >= units.len() {
        return Ok(Value::Number(f64::NAN));
    }
    let start = index;
    // Infinity (spec step 6).
    if units.len() - index >= 8
        && units[index..index + 8] == *"Infinity".encode_utf16().collect::<Vec<u16>>()
    {
        return Ok(Value::Number(sign * f64::INFINITY));
    }
    let mut saw_digit = false;
    while index < units.len() && is_ascii_digit(units[index]) {
        index += 1;
        saw_digit = true;
    }
    if index < units.len() && units[index] == b'.' as u16 {
        index += 1;
        while index < units.len() && is_ascii_digit(units[index]) {
            index += 1;
            saw_digit = true;
        }
    }
    // Exponent: only when a digit follows the e/E (and optional sign).
    if saw_digit && index < units.len() && matches!(units[index], 0x65 | 0x45) {
        let mut lookahead = index + 1;
        if lookahead < units.len() && matches!(units[lookahead], 0x2B | 0x2D) {
            lookahead += 1;
        }
        if lookahead < units.len() && is_ascii_digit(units[lookahead]) {
            index = lookahead;
            while index < units.len() && is_ascii_digit(units[index]) {
                index += 1;
            }
        }
    }
    if !saw_digit {
        return Ok(Value::Number(f64::NAN));
    }
    let text = JsString::from_utf16(&units[start..index]).to_string_lossy();
    match text.parse::<f64>() {
        Ok(value) => Ok(Value::Number(sign * value)),
        Err(_) => Ok(Value::Number(f64::NAN)),
    }
}

fn is_ascii_digit(unit: u16) -> bool {
    matches!(unit, 0x30..=0x39)
}

/// White Space or LineTerminator code points (spec 19.2.5 step 5).
fn is_whitespace(unit: u16) -> bool {
    matches!(
        unit,
        0x0009 | 0x000A | 0x000B | 0x000C | 0x000D | 0x0020 | 0x00A0 | 0x1680 | 0x2000
            ..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000 | 0xFEFF
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::evaluate;

    fn run(source: &str) -> Result<Value, JsError> {
        evaluate(source)
    }

    fn str(value: &str) -> Value {
        Value::String(Handle::new(JsString::from_utf8(value)))
    }

    fn is_nan_value(value: &Value) -> bool {
        matches!(value.kind(), ValueKind::Number(n) if n.is_nan())
    }

    #[test]
    fn is_finite_and_is_nan() {
        assert_eq!(run("isFinite(42)").unwrap(), Value::Boolean(true));
        assert_eq!(run("isFinite(Infinity)").unwrap(), Value::Boolean(false));
        assert_eq!(run("isFinite(NaN)").unwrap(), Value::Boolean(false));
        assert_eq!(run("isFinite('42')").unwrap(), Value::Boolean(true));
        assert_eq!(run("isNaN(NaN)").unwrap(), Value::Boolean(true));
        assert_eq!(run("isNaN('x')").unwrap(), Value::Boolean(true));
        assert_eq!(run("isNaN(5)").unwrap(), Value::Boolean(false));
    }

    #[test]
    fn is_finite_is_nan_coercion_and_abrupts() {
        // Objects unbox through the agent's valueOf/toString (spec 19.2.2/3).
        assert_eq!(run("isFinite([1])").unwrap(), Value::Boolean(true));
        assert_eq!(run("isFinite([Infinity])").unwrap(), Value::Boolean(false));
        assert_eq!(
            run("isFinite({ valueOf: function () { return 5; } })").unwrap(),
            Value::Boolean(true)
        );
        // ToNumber abrupt completions propagate instead of collapsing to a
        // boolean (spec step 1 uses ? ToNumber).
        assert!(matches!(
            run("isFinite(Symbol())"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
        assert!(matches!(
            run("isNaN(Symbol())"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
        // A user valueOf throw propagates as a wrapped TypeError (the thrown
        // value is attached), not a collapsed false/true.
        assert!(matches!(
            run("isFinite({ valueOf: function () { throw 'boom'; } })"),
            Err(e) if e.kind == ErrorKind::TypeError && e.value.is_some()
        ));
    }

    #[test]
    fn parse_int_radix_to_int32() {
        // ToInt32 semantics for the radix (spec 19.2.5 step 6): NaN and the
        // infinities map to 0, and out-of-range values wrap modulo 2^32.
        assert_eq!(
            run("parseInt('11', NaN)").unwrap(),
            run("parseInt('11', 10)").unwrap()
        );
        assert_eq!(
            run("parseInt('11', Infinity)").unwrap(),
            run("parseInt('11', 10)").unwrap()
        );
        assert_eq!(
            run("parseInt('11', 4294967298)").unwrap(),
            run("parseInt('11', 2)").unwrap()
        );
        assert_eq!(
            run("parseInt('11', 4294967296)").unwrap(),
            run("parseInt('11', 10)").unwrap()
        );
        assert!(is_nan_value(&run("parseInt('11', -2147483650)").unwrap()));
        // An object radix unboxes through the agent, and a throwing valueOf
        // propagates.
        assert_eq!(
            run("parseInt('11', { valueOf: function () { return 2; } })").unwrap(),
            Value::Number(3.0)
        );
        assert!(matches!(
            run("parseInt('11', { valueOf: function () { throw 'e'; } })"),
            Err(e) if e.kind == ErrorKind::TypeError && e.value.is_some()
        ));
    }

    #[test]
    fn parse_float_and_int_unbox_objects() {
        assert_eq!(
            run("parseFloat({ toString: function () { return '3.5'; } })").unwrap(),
            Value::Number(3.5)
        );
        assert!(is_nan_value(
            &run("parseFloat({ valueOf: function () { return 1; } })").unwrap()
        ));
        assert_eq!(
            run("parseInt({ toString: function () { return '11'; } }, { valueOf: function () { return 2; } })")
                .unwrap(),
            Value::Number(3.0)
        );
    }

    #[test]
    fn global_functions_inherit_function_prototype() {
        // CreateBuiltinFunction (spec 10.2.3): the [[Prototype]] is
        // %Function.prototype%, so hasOwnProperty/call/apply resolve.
        assert_eq!(
            run("encodeURI.hasOwnProperty('length')").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("Object.getPrototypeOf(parseInt) === Function.prototype").unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            run("parseInt.call(null, '10', 2)").unwrap(),
            Value::Number(2.0)
        );
        // After deleting its own length, the lookup reaches
        // %Function.prototype%'s length (0), so it is not undefined.
        assert_eq!(
            run("delete encodeURI.length; String(encodeURI.length)").unwrap(),
            str("0")
        );
    }

    #[test]
    fn parse_int_forms() {
        assert_eq!(run("parseInt('42')").unwrap(), Value::Number(42.0));
        assert_eq!(run("parseInt('  -42')").unwrap(), Value::Number(-42.0));
        assert_eq!(run("parseInt('0x1F')").unwrap(), Value::Number(31.0));
        assert_eq!(run("parseInt('0b101')").unwrap(), Value::Number(5.0));
        assert_eq!(run("parseInt('0o17')").unwrap(), Value::Number(15.0));
        assert_eq!(run("parseInt('123', 8)").unwrap(), Value::Number(83.0));
        assert_eq!(run("parseInt('12abc')").unwrap(), Value::Number(12.0));
        assert!(is_nan_value(&run("parseInt('abc')").unwrap()));
        assert_eq!(run("parseInt('0x1F', 10)").unwrap(), Value::Number(0.0));
        assert!(is_nan_value(&run("parseInt('7', 2)").unwrap()));
    }

    #[test]
    // parseFloat("3.14") must round-trip to the literal 3.14; clippy sees the
    // literal as an approximation of PI and would rather we used a constant.
    #[allow(clippy::approx_constant)]
    fn parse_float_forms() {
        assert_eq!(run("parseFloat('3.14')").unwrap(), Value::Number(3.14));
        assert_eq!(
            run("parseFloat('  -2.5e2')").unwrap(),
            Value::Number(-250.0)
        );
        assert_eq!(run("parseFloat('.5')").unwrap(), Value::Number(0.5));
        assert_eq!(
            run("parseFloat('Infinity')").unwrap(),
            Value::Number(f64::INFINITY)
        );
        assert_eq!(run("parseFloat('12px')").unwrap(), Value::Number(12.0));
        assert_eq!(run("parseFloat('1e')").unwrap(), Value::Number(1.0));
        assert!(is_nan_value(&run("parseFloat('abc')").unwrap()));
    }

    #[test]
    fn encode_uri_functions() {
        assert_eq!(run("encodeURIComponent('a b&')").unwrap(), str("a%20b%26"));
        assert_eq!(run("encodeURIComponent('a/b')").unwrap(), str("a%2Fb"));
        // encodeURI keeps reserved characters.
        assert_eq!(run("encodeURI('a/b?c=d')").unwrap(), str("a/b?c=d"));
        assert_eq!(run("encodeURI('a b')").unwrap(), str("a%20b"));
        // Unicode encodes as UTF-8 percent sequences.
        assert_eq!(
            run("encodeURIComponent('\u{00E9}')").unwrap(),
            str("%C3%A9")
        );
        // Lone surrogates throw URIError.
        assert!(run("encodeURIComponent('\\uD800')").is_err());
    }

    #[test]
    fn decode_uri_functions() {
        assert_eq!(run("decodeURIComponent('a%20b%26')").unwrap(), str("a b&"));
        assert_eq!(
            run("decodeURIComponent('%C3%A9')").unwrap(),
            str("\u{00E9}")
        );
        // decodeURI keeps the reserved %XX escapes.
        assert_eq!(run("decodeURI('%2F')").unwrap(), str("%2F"));
        assert_eq!(run("decodeURIComponent('%2F')").unwrap(), str("/"));
        // Malformed escapes throw URIError.
        assert!(run("decodeURIComponent('%')").is_err());
        assert!(run("decodeURIComponent('%GG')").is_err());
        assert!(run("decodeURIComponent('%C3')").is_err());
    }
}
