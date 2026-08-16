//! The `%Math%` intrinsic (spec 21.3): the Math object with its constants and
//! function properties. Almost every method is a pure crux closure (ToNumber
//! the arguments, compute, return); `Math.sumPrecise` alone needs the agent
//! for the iterator protocol and dispatches by intrinsic identity (the %eval%
//! pattern).

use crux::convert::{to_number, to_uint32};
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

const MATH_SUM_PRECISE: &str = "%Math.sumPrecise%";

/// spec 21.3.2.28 Math.round: round half toward +infinity, with the exact
/// `-0` semantics for the [-0.5, -0] range (fixtures check `1 / Math.round(-0.5)`
/// is `-Infinity`).
fn math_round(x: f64) -> f64 {
    if x.is_nan() || x == 0.0 || x.is_infinite() {
        return x;
    }
    let floor = x.floor();
    let rounded = if x - floor < 0.5 { floor } else { floor + 1.0 };
    if rounded == 0.0 {
        if x < 0.0 { -0.0 } else { 0.0 }
    } else {
        rounded
    }
}

/// spec 21.3.2.17 Math.imul: (ToUint32(a) × ToUint32(b)) mod 2^32, as int32.
fn imul(a: f64, b: f64) -> f64 {
    let a = to_uint32(a) as u32;
    let b = to_uint32(b) as u32;
    (a.wrapping_mul(b) as i32) as f64
}

/// spec 21.3.2.9 Math.clz32: count leading zeroes of ToUint32(x).
fn clz32(x: f64) -> f64 {
    to_uint32(x).leading_zeros() as f64
}

/// spec 21.3.2.10 Math.max / 21.3.2.24 Math.min: NaN propagates, +0 beats -0
/// (and vice versa for min), no arguments give -inf/+inf.
fn math_max(values: &[f64]) -> f64 {
    let mut max = f64::NEG_INFINITY;
    for &n in values {
        if n.is_nan() {
            return f64::NAN;
        }
        if n == 0.0 && max == 0.0 {
            max = if n.is_sign_negative() && max.is_sign_negative() {
                -0.0
            } else {
                0.0
            };
        } else if n > max {
            max = n;
        }
    }
    max
}

fn math_min(values: &[f64]) -> f64 {
    let mut min = f64::INFINITY;
    for &n in values {
        if n.is_nan() {
            return f64::NAN;
        }
        if n == 0.0 && min == 0.0 {
            min = if n.is_sign_negative() || min.is_sign_negative() {
                -0.0
            } else {
                0.0
            };
        } else if n < min {
            min = n;
        }
    }
    min
}

/// spec 21.3.2.18 Math.hypot: rescale by the largest |arg| so the sum of
/// squares neither overflows nor underflows.
fn hypot(values: &[f64]) -> f64 {
    let mut max = 0.0f64;
    let mut has_nan = false;
    for &n in values {
        if n.is_infinite() {
            return f64::INFINITY;
        }
        if n.is_nan() {
            has_nan = true;
        }
        if n.abs() > max {
            max = n.abs();
        }
    }
    if has_nan {
        return f64::NAN;
    }
    if max == 0.0 {
        return 0.0;
    }
    let mut sum = 0.0f64;
    for &n in values {
        let scaled = n / max;
        sum += scaled * scaled;
    }
    max * sum.sqrt()
}

/// Binary16 (IEEE 754 half precision) round-trip, used by Math.f16round
/// (spec 21.3.2.15). The encode side rounds directly from the full 53-bit
/// f64 mantissa (see `crux::typed_array::f16_from_f64`), so the spec's
/// double-rounding trap (going through binary32 first) cannot occur.
fn f16_from_f64(x: f64) -> u16 {
    crux::typed_array::f16_from_f64(x)
}

/// Convert the half-precision bit pattern back to f64.
fn f16_to_f64(bits: u16) -> f64 {
    if bits == 0 || bits == 0x8000 {
        return if bits == 0x8000 { -0.0 } else { 0.0 };
    }
    let sign = ((bits >> 15) as u64) << 63;
    let biased = (bits >> 10) & 0x1F;
    let fraction = (bits & 0x3FF) as u64;
    if biased == 0x1F {
        if fraction == 0 {
            return f64::from_bits(sign | 0x7FF0_0000_0000_0000);
        }
        return f64::NAN;
    }
    if biased == 0 {
        // Subnormal: fraction × 2^-24, exactly representable in f64 (a
        // power-of-two scaling of a small integer).
        let magnitude = (fraction as f64) * 2f64.powi(-24);
        return if sign != 0 { -magnitude } else { magnitude };
    }
    // Normal: (1024 + fraction) × 2^(biased - 25).
    let biased64 = (biased as u64) + (1023 - 15);
    f64::from_bits(sign | (biased64 << 52) | (fraction << 42))
}

fn math_f16round(x: f64) -> f64 {
    f16_to_f64(f16_from_f64(x))
}

/// A simple xorshift64* PRNG for Math.random (spec 21.3.2.27: host-defined,
/// uniform in [0, 1)). Re-exported as the embedding API's default random
/// source when no host callback overrides it.
pub(crate) fn default_random() -> f64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut x = STATE.load(Ordering::Relaxed);
    if x == 0 {
        x = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x1234_5678_9ABC_DEF0)
            | 1;
    }
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    STATE.store(x, Ordering::Relaxed);
    let r = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
    ((r >> 11) as f64) * (1.0 / (1u64 << 53) as f64)
}

/// Exact summation accumulator for `Math.sumPrecise` (spec 21.3.2.39): every
/// f64 input is a dyadic rational m·2^e, so the exact sum is an integer in
/// units of 2^-1074 (the smallest subnormal). The sum is accumulated as a
/// signed big unsigned integer and rounded to f64 once at the end.
#[derive(Default)]
struct ExactSum {
    /// Little-endian base-2^64 limbs of the magnitude.
    limbs: Vec<u64>,
    /// Whether the exact sum is negative.
    negative: bool,
}

impl ExactSum {
    /// Add `mantissa × 2^shift` (in units of 2^-1074, shift ≥ 0) to the
    /// magnitude. The 53-bit mantissa spans at most two limbs.
    fn add_magnitude(&mut self, mantissa: u64, shift: u32) {
        let index = (shift / 64) as usize;
        let bit = shift % 64;
        if bit == 0 {
            self.add_limb(index, mantissa);
        } else {
            let low = mantissa << bit;
            let high = mantissa >> (64 - bit);
            if low != 0 {
                self.add_limb(index, low);
            }
            if high != 0 {
                self.add_limb(index + 1, high);
            }
        }
    }

    /// Add a full limb at `index` with carry propagation.
    fn add_limb(&mut self, index: usize, mut carry: u64) {
        let mut index = index;
        while carry != 0 {
            if self.limbs.len() <= index {
                self.limbs.resize(index + 1, 0);
            }
            let (sum, overflow) = self.limbs[index].overflowing_add(carry);
            self.limbs[index] = sum;
            carry = u64::from(overflow);
            index += 1;
        }
    }

    fn add(&mut self, value: f64) {
        let bits = value.to_bits();
        let negative = (bits >> 63) == 1;
        let biased = ((bits >> 52) & 0x7FF) as i32;
        let fraction = bits & 0xF_FFFF_FFFF_FFFF;
        let (mantissa, shift) = if biased == 0 {
            // Subnormal: fraction × 2^-1074, i.e. shift 0.
            (fraction, 0u32)
        } else {
            // Normal: (2^52 + fraction) × 2^(biased - 1075); in units of
            // 2^-1074 the shift is biased - 1.
            (fraction | (1 << 52), (biased - 1) as u32)
        };
        if negative == self.negative {
            self.add_magnitude(mantissa, shift);
        } else {
            self.subtract_magnitude(mantissa, shift);
        }
    }

    /// Subtract `mantissa × 2^shift` from the magnitude (opposite sign).
    fn subtract_magnitude(&mut self, mantissa: u64, shift: u32) {
        let mut other = vec![0u64; (shift / 64) as usize + 1];
        other[shift as usize / 64] = mantissa << (shift % 64);
        if !shift.is_multiple_of(64) {
            let carry = mantissa >> (64 - (shift % 64));
            if carry != 0 {
                other.push(carry);
            }
        }
        // Trim trailing zero limbs so the comparison is meaningful.
        while other.last() == Some(&0) {
            other.pop();
        }
        match compare(&self.limbs, &other) {
            std::cmp::Ordering::Equal => {
                self.limbs.clear();
                self.negative = false;
            }
            std::cmp::Ordering::Greater => sub_magnitude(&mut self.limbs, &other),
            std::cmp::Ordering::Less => {
                let mut result = other;
                sub_magnitude(&mut result, &self.limbs);
                self.limbs = result;
                self.negative = !self.negative;
            }
        }
    }

    /// The bit length of the magnitude (position of the top bit + 1).
    fn bit_len(&self) -> u64 {
        match self.limbs.iter().rposition(|limb| *limb != 0) {
            Some(top) => top as u64 * 64 + (64 - self.limbs[top].leading_zeros() as u64),
            None => 0,
        }
    }

    /// The top 53 bits and the round/sticky information of the bits dropped
    /// below them.
    fn top_bits(&self, drop: u64) -> (u64, bool, bool) {
        if drop == 0 {
            let top = self.limbs.first().copied().unwrap_or(0) & ((1 << 53) - 1);
            return (top, false, false);
        }
        let limb = (drop / 64) as usize;
        let bit = (drop % 64) as u32;
        let mut top = if limb < self.limbs.len() {
            self.limbs[limb] >> bit
        } else {
            0
        };
        if bit != 0 && limb + 1 < self.limbs.len() {
            top |= self.limbs[limb + 1] << (64 - bit);
        }
        let half = if bit == 0 {
            limb >= 1 && (self.limbs[limb - 1] >> 63) == 1
        } else {
            limb < self.limbs.len() && (self.limbs[limb] >> (bit - 1)) & 1 == 1
        };
        let mut sticky = self.limbs[..limb].iter().any(|l| *l != 0);
        if bit == 0 {
            // The half bit is limbs[limb-1] bit 63; the bits below it stay.
            sticky = limb >= 1 && self.limbs[limb - 1] & ((1 << 63) - 1) != 0;
            sticky |= self.limbs[..limb.saturating_sub(1)].iter().any(|l| *l != 0);
        } else if limb < self.limbs.len() && bit >= 1 {
            sticky |= self.limbs[limb] & ((1u64 << (bit - 1)) - 1) != 0;
        }
        (top & ((1 << 53) - 1), half, sticky)
    }

    /// Round the exact magnitude to the nearest f64 (ties-to-even).
    fn to_f64(&self) -> f64 {
        let bits = self.bit_len();
        if bits == 0 {
            return if self.negative { -0.0 } else { 0.0 };
        }
        let magnitude = if self.negative { -1.0f64 } else { 1.0f64 };
        if bits <= 53 {
            // Exactly representable: significand is the value itself at 2^-1074.
            let (top, _, _) = self.top_bits(0);
            return (top as f64) * f64::from_bits(1) * magnitude;
        }
        let drop = bits - 53;
        let (mut mantissa, half, sticky) = self.top_bits(drop);
        let mut exponent = bits as i64 - 1075;
        if half && (sticky || mantissa & 1 == 1) {
            mantissa += 1;
            if mantissa == 1 << 53 {
                mantissa = 1 << 52;
                exponent += 1;
            }
        }
        if exponent >= 1024 {
            return if self.negative {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            };
        }
        let biased = (exponent + 1023) as u64;
        let result = f64::from_bits((biased << 52) | (mantissa & 0xF_FFFF_FFFF_FFFF));
        result * magnitude
    }
}

/// Compare two little-endian magnitudes.
fn compare(a: &[u64], b: &[u64]) -> std::cmp::Ordering {
    let a_top = a.iter().rposition(|l| *l != 0);
    let b_top = b.iter().rposition(|l| *l != 0);
    match (a_top, b_top) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(i), Some(j)) if i != j => i.cmp(&j),
        (Some(i), Some(_)) => {
            for k in (0..=i).rev() {
                let a_k = a.get(k).copied().unwrap_or(0);
                let b_k = b.get(k).copied().unwrap_or(0);
                match a_k.cmp(&b_k) {
                    std::cmp::Ordering::Equal => continue,
                    ord => return ord,
                }
            }
            std::cmp::Ordering::Equal
        }
    }
}

/// `a -= b` where `a >= b` (same length after trim).
fn sub_magnitude(a: &mut Vec<u64>, b: &[u64]) {
    let mut borrow = 0u64;
    for i in 0..a.len() {
        let b_i = if i < b.len() { b[i] } else { 0 };
        let (diff, b1) = a[i].overflowing_sub(b_i);
        let (diff, b2) = diff.overflowing_sub(borrow);
        a[i] = diff;
        borrow = u64::from(b1 || b2);
    }
    while a.last() == Some(&0) {
        a.pop();
    }
}

/// spec 21.3.2.39 Math.sumPrecise(items): the exact sum of an iterable of
/// Numbers, with the spec's plus/minus-infinity and minus-zero state machine.
fn sum_precise(agent: &mut Agent, args: &[Value]) -> Result<Value, JsError> {
    let items = args.first().cloned().unwrap_or(Value::Undefined);
    if matches!(items, Value::Undefined | Value::Null) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Cannot convert undefined or null to object".into(),
        ));
    }
    let iterator = crate::expr::get_iterator(agent, &items)?;
    #[derive(PartialEq)]
    enum State {
        MinusZero,
        Finite,
        PlusInfinity,
        MinusInfinity,
        NotANumber,
    }
    let mut state = State::MinusZero;
    let mut sum = ExactSum::default();
    let mut count: u64 = 0;
    loop {
        let next = crate::expr::iterator_step(agent, &iterator)?;
        let Some(value) = next else { break };
        if count >= (1u64 << 53) - 1 {
            let err = JsError::new(
                ErrorKind::RangeError,
                "Math.sumPrecise received too many values".into(),
            );
            let _ = crate::expr::iterator_close(agent, &iterator);
            return Err(err);
        }
        let Value::Number(n) = value else {
            let err = JsError::new(
                ErrorKind::TypeError,
                "Math.sumPrecise requires Number values".into(),
            );
            let _ = crate::expr::iterator_close(agent, &iterator);
            return Err(err);
        };
        if state != State::NotANumber {
            if n.is_nan() {
                state = State::NotANumber;
            } else if n == f64::INFINITY {
                state = if state == State::MinusInfinity {
                    State::NotANumber
                } else {
                    State::PlusInfinity
                };
            } else if n == f64::NEG_INFINITY {
                state = if state == State::PlusInfinity {
                    State::NotANumber
                } else {
                    State::MinusInfinity
                };
            } else if n != 0.0 || n.is_sign_positive() {
                // Not -0: accumulate when the state is minus-zero or finite.
                if state == State::MinusZero || state == State::Finite {
                    state = State::Finite;
                    sum.add(n);
                    count += 1;
                }
            }
        }
    }
    match state {
        State::NotANumber => Ok(Value::Number(f64::NAN)),
        State::PlusInfinity => Ok(Value::Number(f64::INFINITY)),
        State::MinusInfinity => Ok(Value::Number(f64::NEG_INFINITY)),
        State::MinusZero => Ok(Value::Number(-0.0)),
        State::Finite => Ok(Value::Number(sum.to_f64())),
    }
}

fn placeholder(name: &'static str) -> NativeFn {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

/// ToNumber the i-th argument (spec's "Let n be ? ToNumber(x)").
fn arg(args: &[Value], i: usize) -> Result<f64, JsError> {
    to_number(args.get(i).unwrap_or(&Value::Undefined))
}

/// A Math method: ToNumber its arguments, run a pure computation.
fn method(body: fn(&[f64]) -> f64) -> NativeFn {
    Box::new(move |_, args| {
        let values: Result<Vec<f64>, JsError> = args.iter().map(to_number).collect();
        Ok(Value::Number(body(&values?)))
    })
}

fn unary(body: fn(f64) -> f64) -> NativeFn {
    Box::new(move |_, args| Ok(Value::Number(body(arg(args, 0)?))))
}

/// Install the `Math` intrinsic and the global `Math` binding (spec 21.3.1)
/// during SetDefaultGlobalBindings.
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let math = JsObject::ordinary_object_create(object_proto);

    for (name, value) in [
        ("E", std::f64::consts::E),
        ("LN10", std::f64::consts::LN_10),
        ("LN2", std::f64::consts::LN_2),
        ("LOG10E", std::f64::consts::LOG10_E),
        ("LOG2E", std::f64::consts::LOG2_E),
        ("PI", std::f64::consts::PI),
        ("SQRT1_2", std::f64::consts::FRAC_1_SQRT_2),
        ("SQRT2", std::f64::consts::SQRT_2),
    ] {
        // spec 21.3.1: constants have { [[Writable]]: false, [[Enumerable]]:
        // false, [[Configurable]]: false }.
        math.define_property(
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

    let mut functions: Vec<(&str, u64, NativeFn)> = vec![
        ("abs", 1, unary(f64::abs)),
        ("acos", 1, unary(f64::acos)),
        ("acosh", 1, unary(f64::acosh)),
        ("asin", 1, unary(f64::asin)),
        ("asinh", 1, unary(f64::asinh)),
        ("atan", 1, unary(f64::atan)),
        ("atanh", 1, unary(f64::atanh)),
        ("cbrt", 1, unary(f64::cbrt)),
        ("ceil", 1, unary(f64::ceil)),
        ("clz32", 1, unary(clz32)),
        ("cos", 1, unary(f64::cos)),
        ("cosh", 1, unary(f64::cosh)),
        ("exp", 1, unary(f64::exp)),
        ("expm1", 1, unary(f64::exp_m1)),
        ("f16round", 1, unary(math_f16round)),
        ("floor", 1, unary(f64::floor)),
        ("fround", 1, unary(|x| x as f32 as f64)),
        (
            "imul",
            2,
            Box::new(|_: &Value, args: &[Value]| {
                Ok(Value::Number(imul(arg(args, 0)?, arg(args, 1)?)))
            }),
        ),
        ("log", 1, unary(f64::ln)),
        ("log10", 1, unary(f64::log10)),
        ("log1p", 1, unary(f64::ln_1p)),
        ("log2", 1, unary(f64::log2)),
        ("max", 2, method(math_max)),
        ("min", 2, method(math_min)),
        (
            "pow",
            2,
            Box::new(|_: &Value, args: &[Value]| {
                Ok(Value::Number(crux::number::exponentiate(
                    arg(args, 0)?,
                    arg(args, 1)?,
                )))
            }),
        ),
        (
            "random",
            0,
            Box::new(|_: &Value, _: &[Value]| Ok(Value::Number(default_random()))),
        ),
        ("round", 1, unary(math_round)),
        (
            "sign",
            1,
            unary(|x| {
                if x.is_nan() || x == 0.0 {
                    x
                } else if x < 0.0 {
                    -1.0
                } else {
                    1.0
                }
            }),
        ),
        ("sin", 1, unary(f64::sin)),
        ("sinh", 1, unary(f64::sinh)),
        ("sqrt", 1, unary(f64::sqrt)),
        ("tan", 1, unary(f64::tan)),
        ("tanh", 1, unary(f64::tanh)),
        ("trunc", 1, unary(f64::trunc)),
    ];
    for (name, length, body) in [
        (
            "atan2",
            2,
            Box::new(|_: &Value, args: &[Value]| {
                Ok(Value::Number(arg(args, 0)?.atan2(arg(args, 1)?)))
            }) as NativeFn,
        ),
        ("hypot", 2, method(hypot)),
    ] {
        functions.push((name, length, body));
    }

    // spec 21.3.1: function properties have { [[Writable]]: true,
    // [[Enumerable]]: false, [[Configurable]]: true }. The realm's
    // post-pass only links intrinsic-registered functions, so the
    // [[Prototype]] defaults to %Function.prototype% here
    // (CreateBuiltinFunction, spec 10.2.3 step 1).
    let function_proto = realm
        .intrinsics
        .get("%Function.prototype%")
        .and_then(|value| as_object(&value));
    for (name, length, body) in functions {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            body,
            None,
            function_proto.clone(),
        )?;
        math.define_property(
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

    // Math.sumPrecise needs the agent for the iterator protocol.
    let sum_precise = Function::create_builtin(
        Some(JsString::from_utf8("sumPrecise")),
        1,
        placeholder("Math.sumPrecise"),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(MATH_SUM_PRECISE, Value::Function(sum_precise.clone()));
    math.define_property(
        &JsString::from_utf8("sumPrecise"),
        &PropertyDescriptor {
            value: Some(Value::Function(sum_precise)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // spec 21.3.1: Math[@@toStringTag] = "Math".
    math.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8("Math")))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    let math_value = Value::Object(math);
    realm.intrinsics.define("%Math%", math_value.clone());
    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("Math"),
        &PropertyDescriptor {
            value: Some(math_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// The agent-dependent Math members, dispatched by intrinsic identity from
/// `runtime::function::call`.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    _this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    if realm.intrinsics.get(MATH_SUM_PRECISE).as_ref() == Some(callee) {
        return Some(sum_precise(agent, args));
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

    fn number(source: &str) -> f64 {
        match run(source).unwrap() {
            Value::Number(n) => n,
            other => panic!("expected a number, got {other:?}"),
        }
    }

    fn is_nan(value: f64) -> bool {
        value.is_nan()
    }

    #[test]
    fn constants_are_installed() {
        assert_eq!(number("Math.E"), std::f64::consts::E);
        assert_eq!(number("Math.PI"), std::f64::consts::PI);
        assert_eq!(number("Math.SQRT2"), std::f64::consts::SQRT_2);
        assert_eq!(number("Math.LN10"), std::f64::consts::LN_10);
    }

    #[test]
    fn round_half_toward_infinity_with_minus_zero() {
        assert_eq!(number("Math.round(0.5)"), 1.0);
        assert_eq!(number("Math.round(0.4)"), 0.0);
        assert_eq!(number("Math.round(-0.4)"), -0.0);
        assert_eq!(number("Math.round(-0.5)"), -0.0);
        assert_eq!(number("1 / Math.round(-0.5)"), f64::NEG_INFINITY);
        assert_eq!(number("1 / Math.round(0.49999999999999994)"), f64::INFINITY);
        assert_eq!(number("Math.round(1.5)"), 2.0);
        assert_eq!(number("Math.round(-1.5)"), -1.0);
    }

    #[test]
    fn max_min_signed_zero_and_nan() {
        assert_eq!(number("Math.max()"), f64::NEG_INFINITY);
        assert_eq!(number("Math.min()"), f64::INFINITY);
        assert_eq!(number("1 / Math.max(0, -0)"), f64::INFINITY);
        assert_eq!(number("1 / Math.min(0, -0)"), f64::NEG_INFINITY);
        assert_eq!(number("Math.max(1, 2, 3)"), 3.0);
        assert!(is_nan(number("Math.max(1, NaN)")));
        assert!(is_nan(number("Math.min(1, NaN)")));
    }

    #[test]
    fn pow_special_cases() {
        assert!(is_nan(number("Math.pow(2, NaN)")));
        assert_eq!(number("Math.pow(2, 0)"), 1.0);
        assert!(is_nan(number("Math.pow(1, Infinity)")));
        assert!(is_nan(number("Math.pow(-1, Infinity)")));
        assert_eq!(number("Math.pow(2, 10)"), 1024.0);
        assert_eq!(number("Math.pow(-2, 3)"), -8.0);
    }

    #[test]
    fn clz32_and_imul() {
        assert_eq!(number("Math.clz32(0)"), 32.0);
        assert_eq!(number("Math.clz32(1)"), 31.0);
        assert_eq!(number("Math.clz32(0x80000000)"), 0.0);
        assert_eq!(number("Math.clz32(-1)"), 0.0);
        assert_eq!(number("Math.imul(3, 4)"), 12.0);
        assert_eq!(number("Math.imul(0xffffffff, 5)"), -5.0);
        assert_eq!(number("Math.imul(-1, -1)"), 1.0);
    }

    #[test]
    fn fround_and_f16round() {
        assert_eq!(number("Math.fround(1.5)"), 1.5);
        assert_eq!(number("Math.fround(0.1)"), 0.10000000149011612);
        assert!(is_nan(number("Math.fround(NaN)")));
        assert_eq!(number("Math.f16round(1.5)"), 1.5);
        assert_eq!(number("Math.f16round(0.1)"), 0.0999755859375);
        assert_eq!(number("Math.f16round(65504)"), 65504.0);
        assert_eq!(number("Math.f16round(65520)"), f64::INFINITY);
        assert_eq!(number("1 / Math.f16round(-1e-10)"), f64::NEG_INFINITY);
        assert_eq!(number("Math.f16round(6.1e-5)"), 0.00006097555160522461);
        // Subnormal boundary: the f64 one ULP above 2^-25 rounds up to the
        // smallest subnormal, not to 0 (a via-f32 conversion or premature
        // 11-bit rounding flattens it onto the tie); the tie itself rounds
        // to even (0), and the largest subnormal round-trips exactly.
        assert_eq!(
            number("Math.f16round(2.980232238769532e-8)"),
            5.960464477539063e-8
        );
        assert_eq!(number("Math.f16round(2.9802322387695312e-8)"), 0.0);
        assert_eq!(
            number("Math.f16round(5.960464477539063e-8)"),
            5.960464477539063e-8
        );
        assert_eq!(
            number("Math.f16round(0.00006097555160522461)"),
            0.00006097555160522461
        );
        assert_eq!(
            number("Math.f16round(0.000061005353927612305)"),
            0.00006103515625
        );
        assert_eq!(
            number("Math.f16round(0.0000610053539276123)"),
            0.00006097555160522461
        );
    }

    #[test]
    fn hypot_rescales() {
        assert_eq!(number("Math.hypot(3, 4)"), 5.0);
        assert_eq!(number("Math.hypot(1e308, 1e308)"), 1.4142135623730951e308);
        assert_eq!(
            number("Math.hypot(1e-300, 1e-300)"),
            1.4142135623730952e-300
        );
        assert_eq!(number("Math.hypot()"), 0.0);
        assert_eq!(number("1 / Math.hypot(0, -0)"), f64::INFINITY);
        assert_eq!(number("Math.hypot(1, Infinity)"), f64::INFINITY);
        assert!(is_nan(number("Math.hypot(1, NaN)")));
    }

    #[test]
    fn random_is_in_unit_interval() {
        for _ in 0..100 {
            let r = number("Math.random()");
            assert!((0.0..1.0).contains(&r));
        }
    }

    #[test]
    fn sign_and_trunc() {
        assert_eq!(number("Math.sign(-5)"), -1.0);
        assert_eq!(number("Math.sign(5)"), 1.0);
        assert_eq!(number("Math.sign(0)"), 0.0);
        assert_eq!(number("1 / Math.sign(-0)"), f64::NEG_INFINITY);
        assert_eq!(number("Math.trunc(-0.5)"), -0.0);
        assert_eq!(number("Math.trunc(0.5)"), 0.0);
    }

    #[test]
    fn exact_sum_accumulator() {
        let mut sum = ExactSum::default();
        sum.add(1.0);
        sum.add(2.0);
        sum.add(3.0);
        assert_eq!(sum.to_f64(), 6.0);
        let mut sum = ExactSum::default();
        sum.add(0.1);
        sum.add(0.2);
        assert_eq!(sum.to_f64(), 0.30000000000000004);
        let mut sum = ExactSum::default();
        sum.add(1e308);
        sum.add(-1e308);
        assert_eq!(sum.to_f64(), 0.0);
        let mut sum = ExactSum::default();
        sum.add(1e30);
        sum.add(0.1);
        sum.add(-1e30);
        assert_eq!(sum.to_f64(), 0.1);
        let mut sum = ExactSum::default();
        sum.add(1.7976931348623157e308);
        sum.add(1.7976931348623157e308);
        assert_eq!(sum.to_f64(), f64::INFINITY);
        // Smallest subnormal stays exact.
        let mut sum = ExactSum::default();
        sum.add(f64::from_bits(1));
        sum.add(f64::from_bits(1));
        assert_eq!(sum.to_f64(), f64::from_bits(2));
        // Boundary: 2^53 exactly representable as the smallest normal.
        let mut sum = ExactSum::default();
        sum.add(4.450147717014403e-308); // 2^-1022
        assert_eq!(sum.to_f64(), 2.2250738585072014e-308 * 2.0);
    }

    #[test]
    fn sum_precise_state_machine() {
        // Arrays are Phase 12; drive sumPrecise with a native iterable.
        let mut agent = Agent::new();
        agent.initialize_host_defined_realm().unwrap();
        let mut sum = |values: &[f64]| {
            let index = std::cell::Cell::new(0);
            let values = values.to_vec();
            let next = Function::create_builtin(
                Some(JsString::from_utf8("next")),
                0,
                Box::new(move |_, _| {
                    let i = index.get();
                    if i < values.len() {
                        index.set(i + 1);
                        let result = JsObject::ordinary_object_create(None);
                        result
                            .create_data_property(
                                &JsString::from_utf8("value"),
                                Value::Number(values[i]),
                            )
                            .unwrap();
                        result
                            .create_data_property(
                                &JsString::from_utf8("done"),
                                Value::Boolean(false),
                            )
                            .unwrap();
                        Ok(Value::Object(result))
                    } else {
                        let result = JsObject::ordinary_object_create(None);
                        result
                            .create_data_property(
                                &JsString::from_utf8("done"),
                                Value::Boolean(true),
                            )
                            .unwrap();
                        Ok(Value::Object(result))
                    }
                }),
                None,
                None,
            )
            .unwrap();
            let iterable = JsObject::ordinary_object_create(None);
            iterable
                .create_data_property(&JsString::from_utf8("next"), Value::Function(next))
                .unwrap();
            let iterator_method = Function::create_builtin(
                Some(JsString::from_utf8("[Symbol.iterator]")),
                0,
                Box::new(|this, _| Ok(this.clone())),
                None,
                None,
            )
            .unwrap();
            iterable
                .create_data_property_key(
                    &PropertyKey::Symbol(crux::symbol::well_known("iterator").as_ref().clone()),
                    Value::Function(iterator_method),
                )
                .unwrap();
            match sum_precise(&mut agent, &[Value::Object(iterable)]).unwrap() {
                Value::Number(n) => n,
                other => panic!("expected a number, got {other:?}"),
            }
        };
        assert_eq!(sum(&[1.0, 2.0, 3.0]), 6.0);
        assert_eq!(sum(&[1e30, 0.1, -1e30]), 0.1);
        assert_eq!(
            sum(&[1e308, 1e308, 0.1, 0.1, 1e30, 0.1, -1e30, -1e308, -1e308]),
            0.30000000000000004
        );
        assert_eq!(1.0 / sum(&[-0.0]), f64::NEG_INFINITY);
        assert_eq!(sum(&[1.0, -0.0]), 1.0);
        assert_eq!(sum(&[f64::INFINITY]), f64::INFINITY);
        assert!(sum(&[f64::INFINITY, f64::NEG_INFINITY]).is_nan());
        assert_eq!(sum(&[f64::INFINITY, 1.0]), f64::INFINITY);
    }

    #[test]
    fn sqrt_pow_and_roots() {
        assert!(is_nan(number("Math.sqrt(-1)")));
        assert_eq!(number("Math.sqrt(0)"), 0.0);
        assert_eq!(number("1 / Math.sqrt(-0)"), f64::NEG_INFINITY);
        assert_eq!(number("Math.pow(0, 0)"), 1.0);
        assert!(is_nan(number("Math.pow(-1, 0.5)")));
        assert!(is_nan(number("Math.pow(-8, 1 / 3)")));
        assert_eq!(number("Math.cbrt(-27)"), -3.0);
    }

    #[test]
    fn log_exp_and_trig() {
        assert!(is_nan(number("Math.log(-1)")));
        assert_eq!(number("Math.log(1)"), 0.0);
        assert_eq!(number("Math.log2(8)"), 3.0);
        assert_eq!(number("Math.log10(1000)"), 3.0);
        assert_eq!(number("Math.exp(0)"), 1.0);
        assert_eq!(number("Math.exp(1)"), std::f64::consts::E);
        assert_eq!(number("Math.sin(0)"), 0.0);
        assert_eq!(number("Math.cos(0)"), 1.0);
        assert_eq!(number("Math.tan(0)"), 0.0);
    }

    #[test]
    fn round_floor_ceil_and_abs_signed_zero() {
        assert_eq!(number("Math.round(2.5)"), 3.0);
        assert_eq!(number("Math.floor(-0.5)"), -1.0);
        assert_eq!(number("1 / Math.ceil(-0.5)"), f64::NEG_INFINITY);
        assert_eq!(number("1 / Math.abs(-0)"), f64::INFINITY);
        assert!(is_nan(number("Math.sign(NaN)")));
    }

    #[test]
    fn hypot_prefers_infinity_over_nan() {
        assert_eq!(number("Math.hypot(Infinity, NaN)"), f64::INFINITY);
    }
}
