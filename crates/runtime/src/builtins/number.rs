//! The `%Number%` intrinsic (spec 21.1): the constructor, the statics, and
//! `%Number.prototype%` (toString with radix, toFixed/toExponential/toPrecision
//! with exact digit generation, valueOf, toLocaleString). The formatting
//! algorithms live in `crux::number`; the methods dispatch by intrinsic
//! identity (the %eval% pattern) because ThisNumberValue and the constructor's
//! ToPrimitive need the agent.

use crux::convert::{to_integer_or_infinity, to_number, to_string};
use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::Value;

use crate::agent::Agent;
use crate::context::as_object;
use crate::realm::Realm;

const NUMBER: &str = "%Number%";
const NUMBER_PROTO: &str = "%Number.prototype%";
const TO_EXPONENTIAL: &str = "%Number.prototype.toExponential%";
const TO_FIXED: &str = "%Number.prototype.toFixed%";
const TO_PRECISION: &str = "%Number.prototype.toPrecision%";
const TO_STRING: &str = "%Number.prototype.toString%";
const VALUE_OF: &str = "%Number.prototype.valueOf%";
const TO_LOCALE_STRING: &str = "%Number.prototype.toLocaleString%";

fn placeholder(name: &'static str) -> NativeFn {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

/// spec 21.1.3.1 ThisNumberValue: a Number or a Number wrapper object.
fn this_number_value(agent: &Agent, this: &Value) -> Result<f64, JsError> {
    match this {
        Value::Number(n) => Ok(*n),
        Value::Object(obj) => match agent.number_data.get(&obj.id()) {
            Some(n) => Ok(*n),
            None => Err(JsError::new(
                ErrorKind::TypeError,
                "Number.prototype method called on an incompatible receiver".into(),
            )),
        },
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "Number.prototype method called on an incompatible receiver".into(),
        )),
    }
}

/// GetPrototypeFromConstructor (spec 10.1.14) for the Number wrapper.
fn instance_proto(
    agent: &mut Agent,
    new_target: &Value,
) -> Result<Option<Handle<JsObject>>, JsError> {
    let proto = crate::context::get_property(
        agent,
        new_target,
        &JsString::from_utf8("prototype"),
        new_target.clone(),
    )?;
    as_object(&proto).map(Some).ok_or_else(|| {
        JsError::new(
            ErrorKind::TypeError,
            "new.target.prototype is not an object".into(),
        )
    })
}

/// `Number(value)` / `new Number(value)` (spec 21.1.1.1): ToNumber the
/// argument, returning the bare Number for a call and a wrapper object for a
/// construct.
fn number_construct(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    let proto = instance_proto(agent, new_target)?;
    let object = JsObject::ordinary_object_create(proto);
    let value = match args.first() {
        Some(value) => to_number(value)?,
        None => 0.0,
    };
    agent.number_data.insert(object.id(), value);
    Ok(Value::Object(object))
}

fn number_call(args: &[Value]) -> Result<Value, JsError> {
    let value = match args.first() {
        Some(value) => to_number(value)?,
        None => 0.0,
    };
    Ok(Value::Number(value))
}

/// spec 21.1.3.4 Number.prototype.toFixed(fractionDigits): exact digits with
/// half-up rounding at 10^f (spec 21.1.3.4 steps 9-15).
fn to_fixed(agent: &Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let number = this_number_value(agent, this)?;
    let fraction = args.first().cloned().unwrap_or(Value::Undefined);
    let fraction_count = if matches!(fraction, Value::Undefined) {
        0.0
    } else {
        to_integer_or_infinity(to_number(&fraction)?)
    };
    if !fraction_count.is_finite() || fraction_count < 0.0 || fraction_count > 100.0 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "toFixed() digits argument must be between 0 and 100".into(),
        ));
    }
    if !number.is_finite() {
        return Ok(number_to_string(number));
    }
    if number.abs() >= 1e21 {
        return Ok(number_to_string(number));
    }
    let f = fraction_count as u32;
    let negative = number < 0.0 || (number == 0.0 && number.is_sign_negative());
    let int_value = crux::number::to_fixed_scale(number.abs(), f);
    let mut digits = int_value.to_decimal();
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    let digit_count = digits.len();
    if digit_count <= f as usize {
        digits = format!("{}{}", "0".repeat(f as usize + 1 - digit_count), digits);
    }
    if f == 0 {
        out.push_str(&digits);
    } else {
        let split = digits.len() - f as usize;
        out.push_str(&digits[..split]);
        out.push('.');
        out.push_str(&digits[split..]);
    }
    Ok(Value::String(Handle::new(JsString::from_utf8(&out))))
}

/// The shortest round-trip digits of `x` as `(digits, exponent)` with the
/// value `d1.d2... × 10^exponent`, parsed from `Number::toString`.
fn shortest_digits(x: f64) -> (String, i32) {
    let text = crux::number::to_string(x).to_string_lossy();
    let (mantissa, e_notation) = match text.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i32>().unwrap_or(0)),
        None => (text.as_str(), 0),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mantissa, ""),
    };
    let combined = format!("{int_part}{frac_part}");
    let lead = combined
        .find(|c| c != '0')
        .unwrap_or(combined.len().saturating_sub(1));
    let digits = combined[lead..].to_string();
    let exponent = int_part.len() as i32 - lead as i32 - 1 + e_notation;
    (digits, exponent)
}

/// spec 21.1.3.2 Number.prototype.toExponential(fractionDigits).
fn to_exponential(agent: &Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let number = this_number_value(agent, this)?;
    let fraction = args.first().cloned().unwrap_or(Value::Undefined);
    let f_is_undefined = matches!(fraction, Value::Undefined);
    let fraction_count = if f_is_undefined {
        0.0
    } else {
        to_integer_or_infinity(to_number(&fraction)?)
    };
    if !number.is_finite() {
        return Ok(number_to_string(number));
    }
    if fraction_count < 0.0 || fraction_count > 100.0 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "toExponential() digits argument must be between 0 and 100".into(),
        ));
    }
    let negative = number < 0.0 || (number == 0.0 && number.is_sign_negative());
    let abs = number.abs();
    let (exponent, digits) = if abs == 0.0 {
        (0i32, "0".repeat(fraction_count as usize + 1))
    } else if f_is_undefined {
        let (digits, exponent) = shortest_digits(abs);
        (exponent, digits)
    } else {
        crux::number::round_significant(abs, fraction_count as u32 + 1)
    };
    let significand = if digits.len() == 1 {
        digits
    } else {
        format!("{}.{}", &digits[..1], &digits[1..])
    };
    let exponent_sign = if exponent >= 0 { "+" } else { "-" };
    Ok(Value::String(Handle::new(JsString::from_utf8(&format!(
        "{}{significand}e{exponent_sign}{}",
        if negative { "-" } else { "" },
        exponent.abs()
    )))))
}

/// spec 21.1.3.5 Number.prototype.toPrecision(precision).
fn to_precision(agent: &Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let number = this_number_value(agent, this)?;
    let precision = args.first().cloned().unwrap_or(Value::Undefined);
    if matches!(precision, Value::Undefined) {
        return Ok(number_to_string(number));
    }
    let precision_count = to_integer_or_infinity(to_number(&precision)?);
    if !number.is_finite() {
        return Ok(number_to_string(number));
    }
    if precision_count < 1.0 || precision_count > 100.0 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "toPrecision() precision argument must be between 1 and 100".into(),
        ));
    }
    let p = precision_count as u32;
    let negative = number < 0.0 || (number == 0.0 && number.is_sign_negative());
    let abs = number.abs();
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if abs == 0.0 {
        let significand = "0".repeat(p as usize);
        if p == 1 {
            out.push_str(&significand);
        } else {
            out.push_str(&significand[..1]);
            out.push('.');
            out.push_str(&significand[1..]);
        }
        return Ok(Value::String(Handle::new(JsString::from_utf8(&out))));
    }
    let (exponent, digits) = crux::number::round_significant(abs, p);
    if exponent < -6 || exponent >= p as i32 {
        // Exponential notation (spec steps 15-23).
        let significand = if p == 1 {
            digits
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        let exponent_sign = if exponent >= 0 { "+" } else { "-" };
        out.push_str(&format!("{significand}e{exponent_sign}{}", exponent.abs()));
    } else if exponent == p as i32 - 1 {
        out.push_str(&digits);
    } else if exponent >= 0 {
        let split = exponent as usize + 1;
        out.push_str(&digits[..split]);
        out.push('.');
        out.push_str(&digits[split..]);
    } else {
        out.push_str("0.");
        out.push_str(&"0".repeat((-exponent - 1) as usize));
        out.push_str(&digits);
    }
    Ok(Value::String(Handle::new(JsString::from_utf8(&out))))
}

/// spec 21.1.3.6 Number.prototype.toString(radix).
fn to_string_method(agent: &Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let number = this_number_value(agent, this)?;
    let radix = args.first().cloned().unwrap_or(Value::Undefined);
    let radix_value = if matches!(radix, Value::Undefined) {
        10.0
    } else {
        to_integer_or_infinity(to_number(&radix)?)
    };
    if radix_value < 2.0 || radix_value > 36.0 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "toString() radix argument must be between 2 and 36".into(),
        ));
    }
    let text = crux::number::to_string_radix(number, radix_value as u32);
    Ok(Value::String(Handle::new(JsString::from_utf8(&text))))
}

/// Number::toString(x, 10) as a value.
fn number_to_string(x: f64) -> Value {
    let text = to_string(&Value::Number(x)).unwrap_or_else(|e| JsString::from_utf8(&e.message));
    Value::String(Handle::new(text))
}

/// A pure Number static: `(this, args) -> value`.
type StaticFn = fn(&Value, &[Value]) -> Result<Value, JsError>;

/// spec 21.1.2.4 Number.isFinite: only Number values, no coercion.
fn is_finite(_this: &Value, args: &[Value]) -> Result<Value, JsError> {
    Ok(Value::Boolean(
        matches!(args.first(), Some(Value::Number(n)) if n.is_finite()),
    ))
}

/// spec 21.1.2.6 Number.isNaN.
fn is_nan(_this: &Value, args: &[Value]) -> Result<Value, JsError> {
    Ok(Value::Boolean(
        matches!(args.first(), Some(Value::Number(n)) if n.is_nan()),
    ))
}

/// spec 21.1.2.5 Number.isInteger.
fn is_integer(_this: &Value, args: &[Value]) -> Result<Value, JsError> {
    Ok(Value::Boolean(
        matches!(args.first(), Some(Value::Number(n)) if n.is_finite() && n.trunc() == *n),
    ))
}

/// spec 21.1.2.7 Number.isSafeInteger.
fn is_safe_integer(_this: &Value, args: &[Value]) -> Result<Value, JsError> {
    Ok(Value::Boolean(
        matches!(args.first(), Some(Value::Number(n)) if n.is_finite()
        && n.trunc() == *n
        && n.abs() <= 9007199254740991.0),
    ))
}

/// Install the Number intrinsics and the global `Number` binding (spec
/// 21.1.1-21.1.3) during SetDefaultGlobalBindings.
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let number_proto = JsObject::ordinary_object_create(object_proto);
    let number_proto_value = Value::Object(number_proto.clone());

    let number_ctor = Function::create_builtin(
        Some(JsString::from_utf8("Number")),
        1,
        placeholder("Number"),
        Some(Box::new(placeholder("Number"))),
        None,
    )?;
    let number_ctor_value = Value::Function(number_ctor.clone());

    realm.intrinsics.define(NUMBER, number_ctor_value.clone());
    realm
        .intrinsics
        .define(NUMBER_PROTO, number_proto_value.clone());

    number_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(number_proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    number_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(number_ctor_value.clone()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // spec 21.1.2: constants have { W: false, E: false, C: false }.
    for (name, value) in [
        ("EPSILON", 2.220446049250313e-16),
        ("MAX_SAFE_INTEGER", 9007199254740991.0),
        ("MAX_VALUE", f64::MAX),
        ("MIN_SAFE_INTEGER", -9007199254740991.0),
        ("MIN_VALUE", f64::from_bits(1)),
        ("NaN", f64::NAN),
        ("NEGATIVE_INFINITY", f64::NEG_INFINITY),
        ("POSITIVE_INFINITY", f64::INFINITY),
    ] {
        number_ctor.define_property(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: Some(Value::Number(value)),
                writable: Some(false),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(false),
            },
        )?;
    }

    // spec 21.1.2: function statics have { W: true, E: false, C: true }.
    let statics: [(&str, u64, StaticFn); 6] = [
        ("parseFloat", 1, crate::builtins::global::parse_float),
        ("parseInt", 2, crate::builtins::global::parse_int),
        ("isFinite", 1, is_finite),
        ("isInteger", 1, is_integer),
        ("isNaN", 1, is_nan),
        ("isSafeInteger", 1, is_safe_integer),
    ];
    for (name, length, body) in statics {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            Box::new(body),
            None,
            None,
        )?;
        number_ctor.define_property(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: Some(Value::Function(func)),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
    }

    // spec 21.1.3: prototype methods, all agent-dispatched.
    for (name, key, length) in [
        ("toExponential", TO_EXPONENTIAL, 1),
        ("toFixed", TO_FIXED, 1),
        ("toPrecision", TO_PRECISION, 1),
        ("toString", TO_STRING, 1),
        ("valueOf", VALUE_OF, 0),
        ("toLocaleString", TO_LOCALE_STRING, 0),
    ] {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            placeholder(name),
            None,
            None,
        )?;
        realm.intrinsics.define(key, Value::Function(func.clone()));
        number_proto.define_property(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: Some(Value::Function(func)),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
    }

    // spec 21.1.3: Number.prototype[@@toStringTag] = "Number".
    number_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8("Number")))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("Number"),
        &PropertyDescriptor {
            value: Some(number_ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// The Number members that need the agent, dispatched by intrinsic identity
/// from `runtime::function::call`/`construct`.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(NUMBER).as_ref() == Some(callee) {
        return Some(number_call(args));
    }
    if intrinsics.get(TO_STRING).as_ref() == Some(callee) {
        return Some(to_string_method(agent, this, args));
    }
    if intrinsics.get(TO_FIXED).as_ref() == Some(callee) {
        return Some(to_fixed(agent, this, args));
    }
    if intrinsics.get(TO_EXPONENTIAL).as_ref() == Some(callee) {
        return Some(to_exponential(agent, this, args));
    }
    if intrinsics.get(TO_PRECISION).as_ref() == Some(callee) {
        return Some(to_precision(agent, this, args));
    }
    if intrinsics.get(VALUE_OF).as_ref() == Some(callee) {
        return Some(this_number_value(agent, this).map(Value::Number));
    }
    if intrinsics.get(TO_LOCALE_STRING).as_ref() == Some(callee) {
        return match this_number_value(agent, this) {
            Ok(n) => Some(Ok(number_to_string(n))),
            Err(e) => Some(Err(e)),
        };
    }
    None
}

pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    if realm.intrinsics.get(NUMBER).as_ref() == Some(callee) {
        return Some(number_construct(agent, args, new_target));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;

    fn run(source: &str) -> Result<Value, JsError> {
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm()?;
        agent.run_script(source)
    }

    fn text(source: &str) -> String {
        match run(source).unwrap() {
            Value::String(s) => s.to_string_lossy(),
            other => panic!("expected a string, got {other:?}"),
        }
    }

    fn number(source: &str) -> f64 {
        match run(source).unwrap() {
            Value::Number(n) => n,
            other => panic!("expected a number, got {other:?}"),
        }
    }

    fn bool(source: &str) -> bool {
        match run(source).unwrap() {
            Value::Boolean(b) => b,
            other => panic!("expected a boolean, got {other:?}"),
        }
    }

    #[test]
    fn constructor_forms() {
        assert_eq!(number("Number()"), 0.0);
        assert_eq!(number("Number('42')"), 42.0);
        assert_eq!(number("Number(null)"), 0.0);
        assert_eq!(number("Number(true)"), 1.0);
        assert!(number("Number('abc')").is_nan());
        assert_eq!(number("new Number('7').valueOf()"), 7.0);
        assert_eq!(run("typeof Number").unwrap().to_string(), "function");
    }

    #[test]
    fn wrapper_and_value_of() {
        assert_eq!(number("new Number(5).valueOf()"), 5.0);
        assert_eq!(number("Number.prototype.valueOf.call(42)"), 42.0);
        assert_eq!(number("(5).valueOf()"), 5.0);
        assert_eq!(text("new Number(5).toString()"), "5");
        assert_eq!(text("(255).toString(16)"), "ff");
        assert_eq!(text("(0.5).toString(2)"), "0.1");
        assert_eq!(text("(10).toString(3)"), "101");
        assert_eq!(text("Number.prototype.toString.call(255, 16)"), "ff");
        assert!(matches!(
            run("Number.prototype.valueOf.call('x')"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
    }

    #[test]
    #[allow(clippy::approx_constant)] // parseFloat('3.14') round-trips the literal
    fn statics() {
        assert_eq!(number("Number.EPSILON"), 2.220446049250313e-16);
        assert_eq!(number("Number.MAX_SAFE_INTEGER"), 9007199254740991.0);
        assert_eq!(number("Number.MAX_VALUE"), f64::MAX);
        assert_eq!(number("Number.MIN_VALUE"), f64::from_bits(1));
        assert!(number("Number.NaN").is_nan());
        assert!(bool("Number.isFinite(5)"));
        assert!(!bool("Number.isFinite('5')"));
        assert!(!bool("Number.isFinite(Infinity)"));
        assert!(bool("Number.isNaN(NaN)"));
        assert!(!bool("Number.isNaN('NaN')"));
        assert!(bool("Number.isInteger(5)"));
        assert!(!bool("Number.isInteger(5.5)"));
        assert!(bool("Number.isSafeInteger(9007199254740991)"));
        assert!(!bool("Number.isSafeInteger(9007199254740992)"));
        assert_eq!(number("Number.parseFloat('3.14')"), 3.14);
        assert_eq!(number("Number.parseInt('0x1F')"), 31.0);
    }

    #[test]
    fn to_fixed_examples() {
        assert_eq!(text("(0.1).toFixed(20)"), "0.10000000000000000555");
        assert_eq!(text("(1.5).toFixed(0)"), "2");
        assert_eq!(text("(2.5).toFixed(0)"), "3");
        assert_eq!(text("(-0).toFixed(2)"), "-0.00");
        assert_eq!(
            text("(1000000000000000128).toFixed(0)"),
            "1000000000000000128"
        );
        assert_eq!(text("(1e21).toFixed(0)"), "1e+21");
        assert_eq!(text("(NaN).toFixed(2)"), "NaN");
        assert_eq!(text("(Infinity).toFixed(2)"), "Infinity");
        assert_eq!(text("(0).toFixed(2)"), "0.00");
        assert_eq!(text("(1).toFixed(2)"), "1.00");
        assert!(matches!(
            run("(1).toFixed(101)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        assert!(matches!(
            run("(1).toFixed(-1)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
    }

    #[test]
    fn to_exponential_examples() {
        assert_eq!(text("(123.456).toExponential(2)"), "1.23e+2");
        assert_eq!(text("(123.456).toExponential()"), "1.23456e+2");
        assert_eq!(text("(0.0001).toExponential()"), "1e-4");
        assert_eq!(text("(0).toExponential(2)"), "0.00e+0");
        assert_eq!(text("(1.5).toExponential(1)"), "1.5e+0");
        assert_eq!(text("(-5).toExponential(0)"), "-5e+0");
    }

    #[test]
    fn to_precision_examples() {
        assert_eq!(text("(123.456).toPrecision(4)"), "123.5");
        assert_eq!(text("(123.456).toPrecision(2)"), "1.2e+2");
        assert_eq!(text("(0.0001).toPrecision(2)"), "0.00010");
        assert_eq!(text("(0).toPrecision(3)"), "0.00");
        assert_eq!(text("(1.5).toPrecision(2)"), "1.5");
        assert_eq!(text("(0.5).toPrecision(1)"), "0.5");
        assert_eq!(text("(123.456).toPrecision()"), "123.456");
        assert!(matches!(
            run("(1).toPrecision(0)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
    }

    #[test]
    fn global_binding_and_prototype() {
        assert_eq!(run("typeof Number").unwrap().to_string(), "function");
        assert_eq!(
            text("Object.prototype.toString.call(new Number(5))"),
            "[object Number]"
        );
        assert_eq!(
            text("Object.prototype.toString.call(Number.prototype)"),
            "[object Number]"
        );
    }

    #[test]
    fn to_string_radix_and_scientific_edges() {
        assert_eq!(text("(255).toString(2)"), "11111111");
        assert_eq!(text("(255).toString(36)"), "73");
        assert_eq!(text("(-0).toString()"), "0");
        assert_eq!(text("(1e21).toString()"), "1e+21");
        assert_eq!(text("(1e-7).toString()"), "1e-7");
        assert_eq!(text("(123.456).toString()"), "123.456");
        assert!(matches!(
            run("(10).toString(1)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        assert!(matches!(
            run("(10).toString(37)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
    }

    #[test]
    fn to_fixed_and_to_precision_boundaries() {
        // 2.55 as a double is 2.54999..., so one decimal place rounds to 2.5.
        assert_eq!(text("(2.55).toFixed(1)"), "2.5");
        assert_eq!(text("(0.5).toFixed(0)"), "1");
        // digits == 100 is allowed (spec: 0..=100); only 101 throws.
        assert_eq!(text("(1).toFixed(100)"), format!("1.{}", "0".repeat(100)));
        assert_eq!(text("(1.5).toPrecision(1)"), "2");
        assert_eq!(text("(123.456).toPrecision(5)"), "123.46");
        assert!(matches!(
            run("(1.5).toPrecision(101)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
    }

    #[test]
    fn parse_int_and_parse_float_quirks() {
        // 0x7fff_ffff_ffff_ffff = 2^63 - 1; the nearest double is 2^63.
        assert_eq!(number("parseInt('0x7fffffffffffffff', 16)"), 2f64.powi(63));
        assert_eq!(number("parseInt('0x7fffffffffffffff')"), 2f64.powi(63));
        // 0xffff_ffff_ffff_ffff = 2^64 - 1; the nearest double is 2^64.
        assert_eq!(number("parseInt('0xffffffffffffffff', 16)"), 2f64.powi(64));
        assert_eq!(number("parseInt('-0x1')"), -1.0);
        assert_eq!(number("parseInt('  -0x10', 16)"), -16.0);
        assert!(number("parseInt('0x1', 37)").is_nan());
        assert_eq!(number("parseInt('1', 36)"), 1.0);
        assert_eq!(number("parseInt('z', 36)"), 35.0);
        assert_eq!(number("parseFloat('-Infinity')"), f64::NEG_INFINITY);
        assert_eq!(number("parseFloat('1.2.3')"), 1.2);
        assert!(number("parseFloat('   ')").is_nan());
        assert_eq!(number("Number('0x1F')"), 31.0);
        assert!(number("Number('  +0x1 ')").is_nan());
        assert!(number("Number('-0b1')").is_nan());
    }

    #[test]
    fn number_boundary_edges() {
        assert_eq!(number("Number.MIN_SAFE_INTEGER"), -9007199254740991.0);
        assert!(number("Number.MIN_VALUE") > 0.0);
        assert!(!bool("Number.isSafeInteger(2 ** 53)"));
        assert!(bool("Number.isSafeInteger(2 ** 53 - 1)"));
        assert!(bool("Number.isSafeInteger(-0)"));
        assert!(bool("Number.isInteger(-0)"));
        assert!(!bool("Number.isInteger(0.5)"));
        assert!(!bool("Number.isInteger(NaN)"));
    }
}
