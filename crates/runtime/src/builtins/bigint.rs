//! The `%BigInt%` intrinsic (spec 21.2): the constructor (ToBigInt coercion,
//! no implicit Number conversion), the `asIntN`/`asUintN` statics, and
//! `%BigInt.prototype%` (toString with radix, valueOf, toLocaleString).
//! The arithmetic operators landed with the evaluator (Phase 6); this module
//! adds the built-in object surface. ToBigInt's ToPrimitive on objects needs
//! the agent, so the constructor and the statics dispatch by intrinsic
//! identity (the %eval% pattern).

use crux::convert::{to_integer_or_infinity, to_number};
use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use crate::agent::Agent;
use crate::context::as_object;
use crate::realm::Realm;

const BIGINT: &str = "%BigInt%";
const BIGINT_PROTO: &str = "%BigInt.prototype%";
const AS_INT_N: &str = "%BigInt.asIntN%";
const AS_UINT_N: &str = "%BigInt.asUintN%";
const PROTO_TO_STRING: &str = "%BigInt.prototype.toString%";
const PROTO_VALUE_OF: &str = "%BigInt.prototype.valueOf%";
const PROTO_TO_LOCALE_STRING: &str = "%BigInt.prototype.toLocaleString%";

fn placeholder(name: &'static str) -> NativeFn {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

/// spec 21.2.3.1 ThisBigIntValue: a BigInt or a BigInt wrapper object.
fn this_bigint_value(agent: &Agent, this: &Value) -> Result<crux::BigInt, JsError> {
    match this.kind() {
        ValueKind::BigInt(b) => Ok(b.as_ref().clone()),
        ValueKind::Object(obj) => match agent.bigint_data.get(&obj.id()) {
            Some(b) => Ok(b.clone()),
            None => Err(JsError::new(
                ErrorKind::TypeError,
                "BigInt.prototype method called on an incompatible receiver".into(),
            )),
        },
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "BigInt.prototype method called on an incompatible receiver".into(),
        )),
    }
}

/// spec 7.1.17 ToBigInt (strict): the abstract operation behind asIntN and
/// asUintN. Objects coerce through the agent, and Numbers throw a TypeError
/// (the constructor's integral-Number case is separate, 21.2.1.1).
fn to_big_int(agent: &mut Agent, value: &Value) -> Result<crux::BigInt, JsError> {
    let prim = crate::context::to_primitive(agent, value, crux::convert::ToPrimitiveHint::Number)?;
    match prim.kind() {
        ValueKind::BigInt(b) => Ok(b.as_ref().clone()),
        ValueKind::Boolean(b) => Ok(crux::BigInt::from(b as i64)),
        ValueKind::String(s) => match crux::convert::string_to_bigint(&s) {
            Some(b) => Ok(b),
            None => Err(JsError::new(
                ErrorKind::SyntaxError,
                "Cannot convert string to a BigInt".into(),
            )),
        },
        ValueKind::Undefined | ValueKind::Null => Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot convert undefined or null to a BigInt".into(),
        )),
        ValueKind::Number(_) => Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot convert a Number to a BigInt".into(),
        )),
        ValueKind::Symbol(_) => Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot convert a Symbol to a BigInt".into(),
        )),
        // ToPrimitive never returns an object; keep the match exhaustive.
        ValueKind::Object(_) | ValueKind::Function(_) => Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot convert an object to a BigInt".into(),
        )),
    }
}

/// `BigInt(value)` (spec 21.2.1.1): ToPrimitive, then the integral-Number
/// case converts exactly and everything else follows ToBigInt.
fn bigint_construct(agent: &mut Agent, args: &[Value]) -> Result<Value, JsError> {
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    let prim = crate::context::to_primitive(agent, &value, crux::convert::ToPrimitiveHint::Number)?;
    match prim.kind() {
        ValueKind::Number(n) => match crux::BigInt::from_f64_exact(n) {
            Some(b) => Ok(Value::BigInt(Handle::new(b))),
            None => Err(JsError::new(
                ErrorKind::RangeError,
                "The number cannot be converted to a BigInt because it is not an integer".into(),
            )),
        },
        _ => Ok(Value::BigInt(Handle::new(to_big_int(agent, &prim)?))),
    }
}

/// spec 21.2.2.3 BigInt.asIntN(bits, bigint).
fn as_int_n(agent: &mut Agent, args: &[Value]) -> Result<Value, JsError> {
    let bits = crate::context::to_index(agent, args.first().unwrap_or(&Value::Undefined))?;
    let int = to_big_int(agent, args.get(1).unwrap_or(&Value::Undefined))?;
    Ok(Value::BigInt(Handle::new(crux::bigint::as_int_n(
        &int, bits,
    ))))
}

/// spec 21.2.2.4 BigInt.asUintN(bits, bigint).
fn as_uint_n(agent: &mut Agent, args: &[Value]) -> Result<Value, JsError> {
    let bits = crate::context::to_index(agent, args.first().unwrap_or(&Value::Undefined))?;
    let int = to_big_int(agent, args.get(1).unwrap_or(&Value::Undefined))?;
    Ok(Value::BigInt(Handle::new(crux::bigint::as_uint_n(
        &int, bits,
    ))))
}

/// spec 21.2.3.3 BigInt.prototype.toString(radix).
fn to_string_method(agent: &Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let value = this_bigint_value(agent, this)?;
    let radix = args.first().cloned().unwrap_or(Value::Undefined);
    let radix_value = if matches!(radix.kind(), ValueKind::Undefined) {
        10.0
    } else {
        to_integer_or_infinity(to_number(&radix)?)
    };
    if !(2.0..=36.0).contains(&radix_value) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "toString() radix argument must be between 2 and 36".into(),
        ));
    }
    let text = crux::bigint::to_string(&value, radix_value as u32);
    Ok(Value::String(Handle::new(JsString::from_utf8(&text))))
}

/// Install the BigInt intrinsics and the global `BigInt` binding (spec
/// 21.2.1-21.2.3) during SetDefaultGlobalBindings.
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let bigint_proto = JsObject::ordinary_object_create(object_proto);
    let bigint_proto_value = Value::Object(bigint_proto.clone());

    let bigint_ctor = Function::create_builtin(
        Some(JsString::from_utf8("BigInt")),
        1,
        placeholder("BigInt"),
        Some(Box::new(placeholder("BigInt"))),
        None,
    )?;
    let bigint_ctor_value = Value::Function(bigint_ctor.clone());

    realm.intrinsics.define(BIGINT, bigint_ctor_value.clone());
    realm
        .intrinsics
        .define(BIGINT_PROTO, bigint_proto_value.clone());

    bigint_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(bigint_proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    bigint_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(bigint_ctor_value.clone()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // spec 21.2.2: asIntN/asUintN, W: true, E: false, C: true.
    for (name, key, length) in [("asIntN", AS_INT_N, 2), ("asUintN", AS_UINT_N, 2)] {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            placeholder(name),
            None,
            None,
        )?;
        realm.intrinsics.define(key, Value::Function(func.clone()));
        bigint_ctor.define_property(
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

    // spec 21.2.3: prototype methods. toString's length is 0 (the radix is
    // read from the arguments list, not a declared parameter).
    for (name, key, length) in [
        ("toString", PROTO_TO_STRING, 0),
        ("valueOf", PROTO_VALUE_OF, 0),
        ("toLocaleString", PROTO_TO_LOCALE_STRING, 0),
    ] {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            placeholder(name),
            None,
            None,
        )?;
        realm.intrinsics.define(key, Value::Function(func.clone()));
        bigint_proto.define_property(
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

    // spec 21.2.3: BigInt.prototype[@@toStringTag] = "BigInt".
    bigint_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8("BigInt")))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("BigInt"),
        &PropertyDescriptor {
            value: Some(bigint_ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// The BigInt members that need the agent, dispatched by intrinsic identity
/// from `runtime::function::call`/`construct`.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(BIGINT).as_ref() == Some(callee) {
        return Some(bigint_construct(agent, args));
    }
    if intrinsics.get(AS_INT_N).as_ref() == Some(callee) {
        return Some(as_int_n(agent, args));
    }
    if intrinsics.get(AS_UINT_N).as_ref() == Some(callee) {
        return Some(as_uint_n(agent, args));
    }
    if intrinsics.get(PROTO_TO_STRING).as_ref() == Some(callee) {
        return Some(to_string_method(agent, this, args));
    }
    if intrinsics.get(PROTO_TO_LOCALE_STRING).as_ref() == Some(callee) {
        return Some(to_string_method(agent, this, &[]));
    }
    if intrinsics.get(PROTO_VALUE_OF).as_ref() == Some(callee) {
        return match this_bigint_value(agent, this) {
            Ok(b) => Some(Ok(Value::BigInt(Handle::new(b)))),
            Err(e) => Some(Err(e)),
        };
    }
    None
}

pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    _args: &[Value],
    _new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    if realm.intrinsics.get(BIGINT).as_ref() == Some(callee) {
        // BigInt is not constructible (spec 21.2.1.1: new BigInt throws).
        return Some(Err(JsError::new(
            ErrorKind::TypeError,
            "BigInt is not a constructor".into(),
        )));
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
        match run(source).unwrap().kind() {
            ValueKind::String(s) => s.to_string_lossy(),
            other => panic!("expected a string, got {other:?}"),
        }
    }

    fn big(source: &str) -> String {
        match run(source).unwrap().kind() {
            ValueKind::BigInt(b) => b.0.to_string(),
            other => panic!("expected a BigInt, got {other:?}"),
        }
    }

    #[test]
    fn constructor_coercions() {
        assert_eq!(big("BigInt('123')"), "123");
        assert_eq!(big("BigInt('0x1F')"), "31");
        assert_eq!(big("BigInt('  42  ')"), "42");
        assert_eq!(big("BigInt('')"), "0");
        assert_eq!(big("BigInt(true)"), "1");
        assert_eq!(big("BigInt(false)"), "0");
        assert_eq!(big("BigInt(5n)"), "5");
        assert_eq!(big("BigInt(-3n)"), "-3");
        // NumberToBigInt: integral numbers convert exactly, the rest throw a
        // RangeError (spec 7.1.16).
        assert_eq!(big("BigInt(5)"), "5");
        assert_eq!(big("BigInt(-3)"), "-3");
        assert_eq!(big("BigInt(0)"), "0");
        assert_eq!(big("BigInt(Number.MAX_SAFE_INTEGER)"), "9007199254740991");
        assert_eq!(
            big("BigInt(Number.MAX_SAFE_INTEGER + 2)"),
            "9007199254740992"
        );
        assert!(matches!(
            run("BigInt(1.5)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        assert!(matches!(
            run("BigInt(NaN)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        assert!(matches!(
            run("BigInt(Infinity)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        assert!(matches!(
            run("BigInt('abc')"),
            Err(e) if e.kind == ErrorKind::SyntaxError
        ));
        assert!(matches!(
            run("BigInt()"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
        assert!(matches!(
            run("new BigInt(5n)"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
    }

    #[test]
    fn as_int_n_and_uint_n() {
        assert_eq!(big("BigInt.asIntN(4, 0xFn)"), "-1");
        assert_eq!(big("BigInt.asIntN(4, 7n)"), "7");
        assert_eq!(big("BigInt.asUintN(4, 0xFn)"), "15");
        assert_eq!(big("BigInt.asUintN(4, -1n)"), "15");
        assert_eq!(big("BigInt.asIntN(0, 5n)"), "0");
        assert_eq!(big("BigInt.asUintN(64, -1n)"), "18446744073709551615");
        assert_eq!(big("BigInt.asIntN(8, 255n)"), "-1");
        assert_eq!(big("BigInt.asIntN(8, 256n)"), "0");
        assert!(matches!(
            run("BigInt.asIntN(-1, 5n)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
    }

    #[test]
    fn prototype_methods() {
        assert_eq!(text("(255n).toString(16)"), "ff");
        assert_eq!(text("(255n).toString(2)"), "11111111");
        assert_eq!(text("(255n).toString()"), "255");
        assert_eq!(text("(-15n).toString(16)"), "-f");
        assert_eq!(text("(10n).toLocaleString()"), "10");
        assert_eq!(big("(5n).valueOf()"), "5");
        assert_eq!(big("BigInt.prototype.valueOf.call(42n)"), "42");
        assert_eq!(
            text("Object.prototype.toString.call(5n)"),
            "[object BigInt]"
        );
        assert!(matches!(
            run("(5n).toString(1)"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        assert!(matches!(
            run("BigInt.prototype.valueOf.call(5)"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
    }

    #[test]
    fn operators_and_boxing() {
        assert_eq!(big("2n + 3n"), "5");
        assert_eq!(big("2n ** 100n"), "1267650600228229401496703205376");
        assert!(matches!(
            run("2n + 1"),
            Err(e) if e.kind == ErrorKind::TypeError
        ));
        assert_eq!(text("typeof 5n"), "bigint");
        assert_eq!(text("(5n).toString()"), "5");
    }

    #[test]
    fn literal_and_arithmetic_edges() {
        assert_eq!(big("0n"), "0");
        assert_eq!(text("String(-0n === 0n)"), "true");
        assert_eq!(big("1n + 2n"), "3");
        assert_eq!(big("2n ** 3n"), "8");
        assert_eq!(big("5n / 2n"), "2");
        assert_eq!(big("-5n / 2n"), "-2");
        assert_eq!(big("5n % 2n"), "1");
        assert_eq!(big("1n << 100n"), "1267650600228229401496703205376");
    }

    #[test]
    fn comparison_with_numbers() {
        assert_eq!(text("String(0n === 0)"), "false");
        assert_eq!(text("String(0n == 0)"), "true");
        assert_eq!(text("String(0n < 1)"), "true");
    }

    #[test]
    fn as_uint_n_more() {
        assert_eq!(big("BigInt.asUintN(8, 255n)"), "255");
        assert_eq!(big("BigInt.asUintN(8, -1n)"), "255");
    }

    #[test]
    fn as_int_n_strict_to_bigint() {
        // ToBigInt rejects Numbers outright (spec 7.1.17 step 8); the
        // BigInt constructor's integral-Number case does not apply here.
        for src in [
            "BigInt.asIntN(0, 0)",
            "BigInt.asIntN(0, 1.5)",
            "BigInt.asIntN(0, NaN)",
            "BigInt.asIntN(0, Infinity)",
            "BigInt.asIntN(0, Object(0))",
            "BigInt.asIntN(0, { valueOf: function () { return 0; } })",
            "BigInt.asIntN(0, { toString: function () { return 0; } })",
            "BigInt.asIntN(0, { [Symbol.toPrimitive]: function () { return 0; } })",
            "BigInt.asIntN(0, Symbol('x'))",
            "BigInt.asIntN(0, Object(Symbol('x')))",
        ] {
            assert!(
                matches!(run(src), Err(e) if e.kind == ErrorKind::TypeError),
                "{src} should throw a TypeError"
            );
        }
        // Strings, booleans, and BigInts convert; objects unbox through the
        // agent's valueOf/toString dispatch.
        assert_eq!(big("BigInt.asIntN(2, '3')"), "-1");
        assert_eq!(big("BigInt.asIntN(2, true)"), "1");
        assert_eq!(big("BigInt.asIntN(2, Object(3n))"), "-1");
        assert_eq!(
            big("BigInt.asIntN(2, { valueOf: function () { return 3n; } })"),
            "-1"
        );
        // Strings that are not integer literals throw a SyntaxError.
        assert!(matches!(
            run("BigInt.asIntN(0, '0b2')"),
            Err(e) if e.kind == ErrorKind::SyntaxError
        ));
        assert!(matches!(
            run("BigInt.asIntN(0, '1n')"),
            Err(e) if e.kind == ErrorKind::SyntaxError
        ));
    }

    #[test]
    fn as_int_n_bits_to_index() {
        // bits goes through ToIndex: truncation towards 0, NaN => 0,
        // strings parse, objects unbox via the agent.
        assert_eq!(big("BigInt.asIntN(2.9, 3n)"), "-1");
        assert_eq!(big("BigInt.asIntN(-0.9, 1n)"), "0");
        assert_eq!(big("BigInt.asIntN('4', 3n)"), "3");
        assert_eq!(big("BigInt.asIntN(NaN, 3n)"), "0");
        assert_eq!(big("BigInt.asIntN(undefined, 3n)"), "0");
        assert_eq!(big("BigInt.asIntN([1], 1n)"), "-1");
        assert_eq!(
            big("BigInt.asIntN({ valueOf: function () { return 4; } }, 3n)"),
            "3"
        );
        // Negative, infinite, and overlarge bits throw a RangeError; BigInt
        // and Symbol bits throw a TypeError.
        for src in [
            "BigInt.asIntN(-1, 3n)",
            "BigInt.asIntN(-2.5, 3n)",
            "BigInt.asIntN(Infinity, 3n)",
            "BigInt.asIntN(9007199254740992, 3n)",
        ] {
            assert!(
                matches!(run(src), Err(e) if e.kind == ErrorKind::RangeError),
                "{src} should throw a RangeError"
            );
        }
        for src in ["BigInt.asIntN(2n, 3n)", "BigInt.asIntN(Symbol(), 3n)"] {
            assert!(
                matches!(run(src), Err(e) if e.kind == ErrorKind::TypeError),
                "{src} should throw a TypeError"
            );
        }
    }

    #[test]
    fn constructor_object_inputs() {
        // The constructor's ToPrimitive runs through the agent too, and its
        // integral-Number case converts while ToBigInt rejects Numbers.
        assert_eq!(big("BigInt(Object(5))"), "5");
        assert_eq!(big("BigInt(Object('3'))"), "3");
        assert_eq!(big("BigInt(Object(1n))"), "1");
        assert_eq!(big("BigInt({ valueOf: function () { return 5; } })"), "5");
        assert_eq!(
            big("BigInt({ [Symbol.toPrimitive]: function () { return '7'; } })"),
            "7"
        );
        assert!(matches!(
            run("BigInt({})"),
            Err(e) if e.kind == ErrorKind::SyntaxError
        ));
        assert!(matches!(
            run("BigInt({ valueOf: function () { return 1.5; } })"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
    }

    #[test]
    fn constructor_string_edges() {
        // Signed non-decimal strings and other malformed literals are not
        // StringIntegerLiterals (7.1.17.1); the binary-string fixture also
        // exercises long 0b/0B strings.
        for src in [
            "BigInt('-0x1')",
            "BigInt('00o')",
            "BigInt('0oa')",
            "BigInt('-0XFFab')",
        ] {
            assert!(
                matches!(run(src), Err(e) if e.kind == ErrorKind::SyntaxError),
                "{src} should throw a SyntaxError"
            );
        }
        assert_eq!(big("BigInt('0b1111')"), "15");
        assert_eq!(big("BigInt('0B10')"), "2");
        assert_eq!(big("BigInt('-0')"), "0");
    }

    #[test]
    fn to_string_length_is_zero() {
        assert_eq!(text("String(BigInt.prototype.toString.length)"), "0");
        assert_eq!(text("String(BigInt.prototype.valueOf.length)"), "0");
    }

    #[test]
    fn loose_equality_unboxes_through_the_agent() {
        // IsLooselyEqual must call an overridden valueOf via the agent rather
        // than reading the boxed value directly (spec 7.2.15 step 1).
        assert_eq!(
            text("var o = Object(1n); o.valueOf = function () { return 2n; }; String(o == 2n)"),
            "true"
        );
        assert_eq!(
            text("var o = Object(1n); o.valueOf = function () { return 2n; }; String(o != 1n)"),
            "true"
        );
    }
}
