//! `Intl.NumberFormat` (ECMA-402 §16): the constructor (locale resolution,
//! the unit/digit/sign options), the prototype (`format` accessor returning a
//! bound function, `formatToParts`, `resolvedOptions`, `formatRange`,
//! `formatRangeToParts`), and the abstract operations — exact decimal
//! rounding (`ToRawFixed`/`ToRawPrecision`), grouping, transliteration, and
//! the pattern application. Instances store their resolved options in the
//! agent's `intl_number_format_data` map (the [[InitializedNumberFormat]]
//! internal slot); the locale data tables live in `number_data.rs`.

use crux::BigInt;
use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use crate::agent::Agent;
use crate::builtins::intl::number_data::{
    HANIDEC_DIGITS, NUMBERING_SYSTEM_DIGITS, NumberLocaleData, UnitDisplay, locale_data,
};
use crate::context::{as_object, get_property, to_object, to_string};
use crate::realm::Realm;

pub const NUMBER_FORMAT: &str = "%Intl.NumberFormat%";
pub const NUMBER_FORMAT_PROTO: &str = "%Intl.NumberFormat.prototype%";
pub const NF_SUPPORTED_LOCALES_OF: &str = "%Intl.NumberFormat.supportedLocalesOf%";
pub const NF_RESOLVED_OPTIONS: &str = "%Intl.NumberFormat.prototype.resolvedOptions%";
pub const NF_FORMAT_GETTER: &str = "%Intl.NumberFormat.prototype.format%";
pub const NF_FORMAT_RANGE: &str = "%Intl.NumberFormat.prototype.formatRange%";
pub const NF_FORMAT_RANGE_TO_PARTS: &str = "%Intl.NumberFormat.prototype.formatRangeToParts%";
pub const NF_FORMAT_TO_PARTS: &str = "%Intl.NumberFormat.prototype.formatToParts%";

fn range_error(message: &str) -> JsError {
    JsError::new(ErrorKind::RangeError, message.into())
}

fn type_error(message: &str) -> JsError {
    JsError::new(ErrorKind::TypeError, message.into())
}

/// The style enums (kept as small ints in the record).
pub const STYLE_DECIMAL: u8 = 0;
pub const STYLE_PERCENT: u8 = 1;
pub const STYLE_CURRENCY: u8 = 2;
pub const STYLE_UNIT: u8 = 3;
pub const DISPLAY_CODE: u8 = 0;
pub const DISPLAY_SYMBOL: u8 = 1;
pub const DISPLAY_NARROW: u8 = 2;
pub const DISPLAY_NAME: u8 = 3;
pub const SIGN_STANDARD: u8 = 0;
pub const SIGN_ACCOUNTING: u8 = 1;
pub const DISPLAY_SHORT: u8 = 0;
pub const DISPLAY_NARROW_UNIT: u8 = 1;
pub const DISPLAY_LONG: u8 = 2;
pub const ROUNDING_FRACTION: u8 = 0;
pub const ROUNDING_SIGNIFICANT: u8 = 1;
pub const ROUNDING_MORE: u8 = 2;
pub const ROUNDING_LESS: u8 = 3;
pub const NOTATION_STANDARD: u8 = 0;
pub const NOTATION_SCIENTIFIC: u8 = 1;
pub const NOTATION_ENGINEERING: u8 = 2;
pub const NOTATION_COMPACT: u8 = 3;
pub const GROUPING_FALSE: u8 = 0;
pub const GROUPING_ALWAYS: u8 = 1;
pub const GROUPING_MIN2: u8 = 2;
pub const GROUPING_AUTO: u8 = 3;
pub const SIGN_AUTO: u8 = 0;
pub const SIGN_NEVER: u8 = 1;
pub const SIGN_ALWAYS: u8 = 2;
pub const SIGN_EXCEPT_ZERO: u8 = 3;
pub const SIGN_NEGATIVE: u8 = 4;
pub const ROUNDING_MODE_CEIL: u8 = 0;
pub const ROUNDING_MODE_FLOOR: u8 = 1;
pub const ROUNDING_MODE_EXPAND: u8 = 2;
pub const ROUNDING_MODE_TRUNC: u8 = 3;
pub const ROUNDING_MODE_HALF_CEIL: u8 = 4;
pub const ROUNDING_MODE_HALF_FLOOR: u8 = 5;
pub const ROUNDING_MODE_HALF_EXPAND: u8 = 6;
pub const ROUNDING_MODE_HALF_TRUNC: u8 = 7;
pub const ROUNDING_MODE_HALF_EVEN: u8 = 8;
pub const TZD_AUTO: u8 = 0;
pub const TZD_STRIP: u8 = 1;

/// The [[InitializedNumberFormat]] record: every resolved option the
/// prototype members and the formatting algorithms read.
#[derive(Debug, Clone)]
pub struct NumberFormatRecord {
    pub locale: String,
    pub numbering_system: String,
    pub style: u8,
    pub currency: Option<String>,
    pub currency_display: u8,
    pub currency_sign: u8,
    pub unit: Option<String>,
    pub unit_display: u8,
    pub minimum_integer_digits: u32,
    pub minimum_fraction_digits: u32,
    pub maximum_fraction_digits: u32,
    pub minimum_significant_digits: u32,
    pub maximum_significant_digits: u32,
    pub rounding_type: u8,
    pub notation: u8,
    pub compact_display: u8,
    pub use_grouping: u8,
    pub sign_display: u8,
    pub rounding_increment: u32,
    pub rounding_mode: u8,
    pub computed_rounding_priority: &'static str,
    pub trailing_zero_display: u8,
    /// The cached [[BoundFormat]] function value.
    pub bound_format: Option<Value>,
}

/// An Intl mathematical value (ECMA-402 §16.5.16): a mathematical value
/// `±mant × 10^exp10` (mant ≥ 0) or one of the special values.
#[derive(Debug, Clone)]
pub enum IntlMv {
    Nan,
    PosInf,
    NegInf,
    NegZero,
    Value {
        negative: bool,
        mant: BigInt,
        exp10: i64,
    },
}

impl IntlMv {
    fn value(negative: bool, mant: BigInt, exp10: i64) -> IntlMv {
        if mant.is_zero() && !negative {
            IntlMv::Value {
                negative: false,
                mant,
                exp10,
            }
        } else {
            IntlMv::Value {
                negative,
                mant,
                exp10,
            }
        }
    }

    fn is_zero(&self) -> bool {
        matches!(self, IntlMv::Value { mant, .. } if mant.is_zero())
    }

    /// The value with the sign stripped (non-negative).
    fn abs(&self) -> IntlMv {
        match self {
            IntlMv::Value {
                negative: _,
                mant,
                exp10,
            } => IntlMv::Value {
                negative: false,
                mant: mant.clone(),
                exp10: *exp10,
            },
            other => other.clone(),
        }
    }

    fn negate(&self) -> IntlMv {
        match self {
            IntlMv::Value {
                negative,
                mant,
                exp10,
            } => IntlMv::value(!negative, mant.clone(), *exp10),
            IntlMv::PosInf => IntlMv::NegInf,
            IntlMv::NegInf => IntlMv::PosInf,
            IntlMv::NegZero => IntlMv::value(false, BigInt::zero(), 0),
            IntlMv::Nan => IntlMv::Nan,
        }
    }

    /// The mathematical value × 10^e.
    pub(crate) fn scale_pow10(&self, e: i64) -> IntlMv {
        match self {
            IntlMv::Value {
                negative,
                mant,
                exp10,
            } => IntlMv::Value {
                negative: *negative,
                mant: mant.clone(),
                exp10: exp10 + e,
            },
            other => other.clone(),
        }
    }

    /// `floor(log10(|v|))` for a non-zero Value (the magnitude).
    fn magnitude(&self) -> i64 {
        let IntlMv::Value { mant, exp10, .. } = self else {
            return 0;
        };
        let digits = crux::bigint::to_string(mant, 10);
        (digits.len() as i64 - 1) + exp10
    }

    /// Whether the value is an integer (`v mod 1 == 0`).
    fn is_integer(&self) -> bool {
        let IntlMv::Value { mant, exp10, .. } = self else {
            return false;
        };
        if mant.is_zero() {
            return true;
        }
        *exp10 >= 0
    }

    /// The exact `mant × 10^exp10` form (for non-negative values).
    fn integer_form(&self) -> Option<(BigInt, i64)> {
        let IntlMv::Value {
            negative: false,
            mant,
            exp10,
        } = self
        else {
            return None;
        };
        Some((mant.clone(), *exp10))
    }
}

/// The unsigned rounding modes (Table 31's Unsigned Rounding Mode column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnsignedRoundingMode {
    Zero,
    Infinity,
    HalfZero,
    HalfInfinity,
    HalfEven,
}

/// GetUnsignedRoundingMode (ECMA-402 §16.5.17, Table 31).
fn get_unsigned_rounding_mode(mode: u8, negative: bool) -> UnsignedRoundingMode {
    use UnsignedRoundingMode::*;
    match (mode, negative) {
        (ROUNDING_MODE_CEIL, false) | (ROUNDING_MODE_EXPAND, _) => Infinity,
        (ROUNDING_MODE_CEIL, true) => Zero,
        (ROUNDING_MODE_FLOOR, false) | (ROUNDING_MODE_TRUNC, _) => Zero,
        (ROUNDING_MODE_FLOOR, true) => Infinity,
        (ROUNDING_MODE_HALF_CEIL, false) | (ROUNDING_MODE_HALF_EXPAND, _) => HalfInfinity,
        (ROUNDING_MODE_HALF_CEIL, true) => HalfZero,
        (ROUNDING_MODE_HALF_FLOOR, true) => HalfInfinity,
        (ROUNDING_MODE_HALF_FLOOR, false) | (ROUNDING_MODE_HALF_TRUNC, _) => HalfZero,
        (ROUNDING_MODE_HALF_EVEN, _) => HalfEven,
        _ => HalfInfinity,
    }
}

fn bigint_cmp(a: &BigInt, b: &BigInt) -> std::cmp::Ordering {
    a.0.cmp(&b.0)
}

/// Compare two non-negative rational values `(mant, exp10)` exactly: align
/// to the finest scale (the minimum exponent).
fn cmp_scaled(a: &BigInt, ae: i64, b: &BigInt, be: i64) -> std::cmp::Ordering {
    let common = ae.min(be);
    let a_scaled = if ae > common {
        multiply_pow10(a, (ae - common) as u32)
    } else {
        a.clone()
    };
    let b_scaled = if be > common {
        multiply_pow10(b, (be - common) as u32)
    } else {
        b.clone()
    };
    bigint_cmp(&a_scaled, &b_scaled)
}

pub(crate) fn multiply_pow10(base: &BigInt, n: u32) -> BigInt {
    if n == 0 {
        return base.clone();
    }
    let ten = BigInt::parse_str("10", 10).expect("10");
    let exp = BigInt::parse_str(&n.to_string(), 10).expect("n");
    let power = crux::bigint::exponentiate(&ten, &exp).expect("10^n");
    crux::bigint::multiply(base, &power)
}

/// ApplyUnsignedRoundingMode (ECMA-402 §16.5.18): pick `r1` or `r2`
/// (both non-negative rational `(mant, exp10)` pairs) for `x` and the mode.
fn apply_unsigned_rounding_mode(
    x: &IntlMv,
    r1: &(BigInt, i64),
    r2: &(BigInt, i64),
    mode: UnsignedRoundingMode,
) -> (BigInt, i64) {
    use UnsignedRoundingMode::*;
    let Some((xm, xe)) = x.integer_form() else {
        return r1.clone();
    };
    if cmp_scaled(&xm, xe, &r1.0, r1.1) == std::cmp::Ordering::Equal {
        return r1.clone();
    }
    match mode {
        Zero => return r1.clone(),
        Infinity => return r2.clone(),
        _ => {}
    }
    // d1 = x - r1, d2 = r2 - x at the finest common scale.
    let common = xe.min(r1.1).min(r2.1);
    let x_al = if xe > common {
        multiply_pow10(&xm, (xe - common) as u32)
    } else {
        xm.clone()
    };
    let r1_al = if r1.1 > common {
        multiply_pow10(&r1.0, (r1.1 - common) as u32)
    } else {
        r1.0.clone()
    };
    let r2_al = if r2.1 > common {
        multiply_pow10(&r2.0, (r2.1 - common) as u32)
    } else {
        r2.0.clone()
    };
    let d1 = crux::bigint::subtract(&x_al, &r1_al);
    let d2 = crux::bigint::subtract(&r2_al, &x_al);
    match bigint_cmp(&d1, &d2) {
        std::cmp::Ordering::Less => r1.clone(),
        std::cmp::Ordering::Greater => r2.clone(),
        std::cmp::Ordering::Equal => match mode {
            HalfZero => r1.clone(),
            HalfInfinity => r2.clone(),
            _ => {
                // half-even: cardinality = (r1 / (r2 - r1)) mod 2.
                let step = crux::bigint::subtract(&r2_al, &r1_al);
                let quotient = if step.is_zero() {
                    BigInt::zero()
                } else {
                    crux::bigint::divide(&r1_al, &step)
                };
                if crux::bigint::remainder(&quotient, &BigInt::parse_str("2", 10).expect("2"))
                    .is_zero()
                {
                    r1.clone()
                } else {
                    r2.clone()
                }
            }
        },
    }
}

/// The ToRawPrecision / ToRawFixed result.
struct RawResult {
    formatted: String,
    rounded: IntlMv,
    int_digits: u32,
    rounding_magnitude: i64,
}

/// ToRawFixed (ECMA-402 §16.5.9): `x` is a non-negative Value.
fn to_raw_fixed(
    x: &IntlMv,
    min_fraction: u32,
    max_fraction: u32,
    rounding_increment: u32,
    mode: UnsignedRoundingMode,
) -> RawResult {
    let f = max_fraction as i64;
    let Some((xm, xe)) = x.integer_form() else {
        // x == 0.
        let (n, _) =
            apply_unsigned_rounding_mode(x, &(BigInt::zero(), -f), &(BigInt::zero(), -f), mode);
        let m = "0".to_string();
        return RawResult {
            formatted: format_raw_fixed(&m, f, min_fraction),
            rounded: IntlMv::value(false, n, -f),
            int_digits: 1,
            rounding_magnitude: -f,
        };
    };
    // The exact scaled value x × 10^f = mant × 10^(xe+f). n1/n2 are the
    // multiples of the increment bracketing the exact scaled value.
    let inc = BigInt::parse_str(&rounding_increment.to_string(), 10).expect("inc");
    let (n1, n2) = scaled_bracket(&xm, xe + f, &inc);
    let r1 = (n1.clone(), -f);
    let r2 = (n2.clone(), -f);
    let (n, _) = apply_unsigned_rounding_mode(x, &r1, &r2, mode);
    let digits = crux::bigint::to_string(&n, 10);
    let m = if n.is_zero() { "0".to_string() } else { digits };
    let formatted = format_raw_fixed(&m, f, min_fraction);
    let int_digits = if f != 0 {
        (m.len() as i64 - f).max(1) as u32
    } else {
        m.len() as u32
    };
    RawResult {
        formatted,
        rounded: IntlMv::value(false, n, -f),
        int_digits,
        rounding_magnitude: -f,
    }
}

/// The multiples of `inc` bracketing `mant × 10^e` (mant ≥ 0): `(floor, ceil)`
/// of the exact rational value, scaled back to multiples of `inc`.
fn scaled_bracket(mant: &BigInt, e: i64, inc: &BigInt) -> (BigInt, BigInt) {
    if e >= 0 {
        let n = multiply_pow10(mant, e as u32);
        (
            crux::bigint::multiply(&div_floor_bigint(&n, inc), inc),
            crux::bigint::multiply(&ceil_div_bigint(&n, inc), inc),
        )
    } else {
        // floor/ceil of mant / (inc × 10^-e).
        let d = multiply_pow10(inc, (-e) as u32);
        (
            crux::bigint::multiply(&div_floor_bigint(mant, &d), inc),
            crux::bigint::multiply(&ceil_div_bigint(mant, &d), inc),
        )
    }
}

/// Place the decimal point into the raw integer digits at `f` fraction
/// digits and strip trailing zeros down to `min_fraction`.
fn format_raw_fixed(m: &str, f: i64, min_fraction: u32) -> String {
    let mut out;
    if f != 0 {
        let k = m.len() as i64;
        if k <= f {
            let zeros = "0".repeat((f + 1 - k) as usize);
            out = format!("{zeros}{m}");
        } else {
            out = m.to_string();
        }
        let k = out.len() as i64;
        let split = (k - f) as usize;
        out = format!("{}.{}", &out[..split], &out[split..]);
    } else {
        out = m.to_string();
    }
    // cut = maxFraction - minFraction (the zero-padding slack).
    let mut cut = f - min_fraction as i64;
    while cut > 0 && out.ends_with('0') {
        out.pop();
        cut -= 1;
    }
    if out.ends_with('.') {
        out.pop();
    }
    out
}

/// ToRawPrecision (ECMA-402 §16.5.8): `x` is a non-negative Value.
fn to_raw_precision(
    x: &IntlMv,
    min_precision: u32,
    max_precision: u32,
    mode: UnsignedRoundingMode,
) -> RawResult {
    let p = max_precision as i64;
    if x.is_zero() {
        // x == 0: m = p zeros, e = 0.
        let m = "0".repeat(p as usize);
        return RawResult {
            formatted: format_raw_precision(&m, 0, p, min_precision),
            rounded: IntlMv::value(false, BigInt::zero(), 0),
            int_digits: 1,
            rounding_magnitude: 1 - p,
        };
    }
    let Some((xm, xe)) = x.integer_form() else {
        return RawResult {
            formatted: "0".to_string(),
            rounded: IntlMv::value(false, BigInt::zero(), 0),
            int_digits: 1,
            rounding_magnitude: 1 - p,
        };
    };
    let magnitude = x.magnitude();
    let scale_exp = magnitude - p + 1;
    // scaled = x / 10^scale_exp = mant × 10^(xe - scale_exp); n1 = floor,
    // n2 = ceil of the exact rational.
    let e = xe - scale_exp;
    let n1 = if e >= 0 {
        multiply_pow10(&xm, e as u32)
    } else {
        divide_pow10(&xm, (-e) as u32)
    };
    let n2 = if e < 0 && !divides_pow10(&xm, (-e) as u32) {
        crux::bigint::add(&n1, &BigInt::parse_str("1", 10).expect("1"))
    } else {
        n1.clone()
    };
    let r1 = (n1.clone(), scale_exp);
    let r2 = (n2.clone(), scale_exp);
    let (n, _) = apply_unsigned_rounding_mode(x, &r1, &r2, mode);
    // The grid exponent for the mantissa string: n × 10^(e - p + 1) = r.
    let e = scale_exp + p - 1;
    let digits = crux::bigint::to_string(&n, 10);
    let m = if n.is_zero() { "0".to_string() } else { digits };
    let formatted = format_raw_precision(&m, e, p, min_precision);
    let int_digits = if e >= 0 { (e + 1) as u32 } else { 1 };
    RawResult {
        formatted,
        rounded: IntlMv::value(false, n, scale_exp),
        int_digits,
        rounding_magnitude: e - p + 1,
    }
}

/// Whether `mant` is divisible by 10^n.
fn divides_pow10(mant: &BigInt, n: u32) -> bool {
    if n == 0 {
        return true;
    }
    let digits = crux::bigint::to_string(mant, 10);
    if (digits.len() as u32) < n {
        return mant.is_zero();
    }
    digits[digits.len() - n as usize..]
        .chars()
        .all(|c| c == '0')
}

/// Position the decimal point for the p-digit mantissa with grid exponent `e`.
fn format_raw_precision(m: &str, e: i64, p: i64, min_precision: u32) -> String {
    let mut out;
    if e >= p - 1 {
        out = format!("{m}{}", "0".repeat((e - p + 1) as usize));
    } else if e >= 0 {
        let first = (e + 1) as usize;
        out = format!("{}.{}", &m[..first], &m[first..]);
    } else {
        out = format!("0.{}{m}", "0".repeat((-e - 1) as usize));
    }
    if out.contains('.') && p > min_precision as i64 {
        let mut cut = p - min_precision as i64;
        while cut > 0 && out.ends_with('0') {
            out.pop();
            cut -= 1;
        }
        if out.ends_with('.') {
            out.pop();
        }
    }
    out
}

/// `floor(mant / 10^n)` for mant ≥ 0 (truncation equals floor).
fn divide_pow10(mant: &BigInt, n: u32) -> BigInt {
    if n == 0 {
        return mant.clone();
    }
    let digits = crux::bigint::to_string(mant, 10);
    if digits.len() <= n as usize {
        BigInt::zero()
    } else {
        BigInt::parse_str(&digits[..digits.len() - n as usize], 10).expect("digit slice")
    }
}

/// Floor division of two non-negative BigInts (truncation equals floor).
fn div_floor_bigint(a: &BigInt, b: &BigInt) -> BigInt {
    crux::bigint::divide(a, b)
}

/// Ceiling division of two non-negative BigInts.
fn ceil_div_bigint(a: &BigInt, b: &BigInt) -> BigInt {
    let q = crux::bigint::divide(a, b);
    let r = crux::bigint::remainder(a, b);
    if r.is_zero() {
        q
    } else {
        crux::bigint::add(&q, &BigInt::parse_str("1", 10).expect("1"))
    }
}

/// FormatNumericToString (ECMA-402 §16.5.3).
pub(crate) fn format_numeric_to_string(
    record: &NumberFormatRecord,
    x: &IntlMv,
) -> (IntlMv, String) {
    let (sign_negative, x) = match x {
        IntlMv::NegZero => (true, IntlMv::value(false, BigInt::zero(), 0)),
        IntlMv::Value { negative, .. } => (*negative, x.abs()),
        other => (false, other.clone()),
    };
    let mode = get_unsigned_rounding_mode(record.rounding_mode, sign_negative);
    let result = match record.rounding_type {
        ROUNDING_SIGNIFICANT => to_raw_precision(
            &x,
            record.minimum_significant_digits,
            record.maximum_significant_digits,
            mode,
        ),
        ROUNDING_FRACTION => to_raw_fixed(
            &x,
            record.minimum_fraction_digits,
            record.maximum_fraction_digits,
            record.rounding_increment,
            mode,
        ),
        _ => {
            let significant = to_raw_precision(
                &x,
                record.minimum_significant_digits,
                record.maximum_significant_digits,
                mode,
            );
            let fraction = to_raw_fixed(
                &x,
                record.minimum_fraction_digits,
                record.maximum_fraction_digits,
                record.rounding_increment,
                mode,
            );
            let fixed_is_more_precise =
                fraction.rounding_magnitude < significant.rounding_magnitude;
            if (record.rounding_type == ROUNDING_MORE && fixed_is_more_precise)
                || (record.rounding_type == ROUNDING_LESS && !fixed_is_more_precise)
            {
                fraction
            } else {
                significant
            }
        }
    };
    let mut rounded = result.rounded;
    let mut string = result.formatted;
    // trailingZeroDisplay: stripIfInteger and the rounded value is an integer.
    if record.trailing_zero_display == TZD_STRIP
        && rounded.is_integer()
        && let Some(dot) = string.find('.')
    {
        string.truncate(dot);
    }
    let int = result.int_digits;
    if int < record.minimum_integer_digits {
        let zeros = "0".repeat((record.minimum_integer_digits - int) as usize);
        string = format!("{zeros}{string}");
    }
    if sign_negative {
        if rounded.is_zero() {
            rounded = IntlMv::NegZero;
        } else {
            rounded = rounded.negate();
        }
    }
    (rounded, string)
}

/// ComputeExponentForMagnitude (ECMA-402 §16.5.14).
fn compute_exponent_for_magnitude(
    record: &NumberFormatRecord,
    data: &NumberLocaleData,
    magnitude: i64,
) -> i64 {
    match record.notation {
        NOTATION_STANDARD => 0,
        NOTATION_SCIENTIFIC => magnitude,
        NOTATION_ENGINEERING => magnitude.div_euclid(3) * 3,
        _ => {
            // compact: the largest table power ≤ magnitude with a non-empty
            // affix for the chosen display (an empty affix collapses the
            // exponent to 0 — the German short 10^3..10^5 gaps).
            let short = record.compact_display == DISPLAY_SHORT;
            let mut exponent = 0;
            for entry in data.compact {
                if i64::from(entry.power) <= magnitude {
                    let affix = if short {
                        entry.short_affix
                    } else {
                        entry.long_affix
                    };
                    if !affix.is_empty() {
                        exponent = entry.power as i64;
                    }
                }
            }
            exponent
        }
    }
}

/// ComputeExponent (ECMA-402 §16.5.13).
pub(crate) fn compute_exponent(
    record: &NumberFormatRecord,
    data: &NumberLocaleData,
    x: &IntlMv,
) -> i64 {
    if x.is_zero() {
        return 0;
    }
    let ax = x.abs();
    let magnitude = ax.magnitude();
    let exponent = compute_exponent_for_magnitude(record, data, magnitude);
    let scaled = ax.scale_pow10(-exponent);
    let (rounded, _) = format_numeric_to_string(record, &scaled);
    if rounded.is_zero() {
        return exponent;
    }
    let new_magnitude = rounded.abs().magnitude();
    if new_magnitude == magnitude - exponent {
        return exponent;
    }
    compute_exponent_for_magnitude(record, data, magnitude + 1)
}

/// GetNotationSubPattern (ECMA-402 §16.5.12).
fn get_notation_sub_pattern(
    record: &NumberFormatRecord,
    data: &NumberLocaleData,
    exponent: i64,
) -> String {
    match record.notation {
        NOTATION_SCIENTIFIC | NOTATION_ENGINEERING => {
            "{number}{scientificSeparator}{scientificExponent}".to_string()
        }
        _ if exponent != 0 => {
            // compact: the entry's pattern carries the separator literals
            // around {number} and the {compactSymbol}/{compactName}
            // placeholder.
            let short = record.compact_display == DISPLAY_SHORT;
            let mut pattern = "";
            for entry in data.compact {
                if entry.power as i64 == exponent {
                    pattern = if short { entry.short } else { entry.long };
                }
            }
            if pattern.is_empty() {
                "{number}".to_string()
            } else {
                pattern.to_string()
            }
        }
        _ => "{number}".to_string(),
    }
}

/// A pattern part (ECMA-402 §9.2.15 PartitionPattern).
#[derive(Debug, Clone)]
enum PatternPart {
    Literal(String),
    Number,
    PlusSign,
    MinusSign,
    PercentSign,
    CurrencyCode,
    CurrencyPrefix,
    CurrencySuffix,
    UnitPrefix,
    UnitSuffix,
    CompactSymbol,
    CompactName,
    ScientificSeparator,
    ScientificExponent,
}

/// PartitionPattern: split a pattern string into its placeholder and literal
/// parts.
fn partition_pattern(pattern: &str) -> Vec<PatternPart> {
    let mut parts = Vec::new();
    let mut literal = String::new();
    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        if let Some(close_rel) = rest[open..].find('}') {
            literal.push_str(&rest[..open]);
            if !literal.is_empty() {
                parts.push(PatternPart::Literal(std::mem::take(&mut literal)));
            }
            let name = &rest[open + 1..open + close_rel];
            let part = match name {
                "number" => PatternPart::Number,
                "plusSign" => PatternPart::PlusSign,
                "minusSign" => PatternPart::MinusSign,
                "percentSign" => PatternPart::PercentSign,
                "currencyCode" => PatternPart::CurrencyCode,
                "currencyPrefix" => PatternPart::CurrencyPrefix,
                "currencySuffix" => PatternPart::CurrencySuffix,
                "unitPrefix" => PatternPart::UnitPrefix,
                "unitSuffix" => PatternPart::UnitSuffix,
                "compactSymbol" => PatternPart::CompactSymbol,
                "compactName" => PatternPart::CompactName,
                "scientificSeparator" => PatternPart::ScientificSeparator,
                "scientificExponent" => PatternPart::ScientificExponent,
                _ => PatternPart::Literal(String::new()),
            };
            parts.push(part);
            rest = &rest[open + close_rel + 1..];
        } else {
            break;
        }
    }
    literal.push_str(rest);
    if !literal.is_empty() {
        parts.push(PatternPart::Literal(literal));
    }
    parts
}

/// A formatted part: type + value (+ source for ranges).
#[derive(Debug, Clone)]
/// A formatted-number part (ECMA-402 §16.5.7 FormatNumericToParts).
pub(crate) struct Part {
    pub(crate) part_type: &'static str,
    pub(crate) value: String,
    pub(crate) source: Option<String>,
}

impl Part {
    fn new(part_type: &'static str, value: String) -> Part {
        Part {
            part_type,
            value,
            source: None,
        }
    }
}

/// The transliterated digit string for the numbering system (Table 30).
pub(crate) fn transliterate(numbering_system: &str, digits_text: &str) -> String {
    if numbering_system == "latn" {
        return digits_text.to_string();
    }
    let digits: Vec<char> = if numbering_system == "hanidec" || numbering_system == "jpanfin" {
        HANIDEC_DIGITS.to_vec()
    } else {
        let mut found = None;
        for &(name, start) in NUMBERING_SYSTEM_DIGITS {
            if name == numbering_system {
                found = Some(start);
                break;
            }
        }
        let Some(start) = found else {
            return digits_text.to_string();
        };
        (0..10)
            .map(|i| char::from_u32(start + i).unwrap_or('0'))
            .collect()
    };
    digits_text
        .chars()
        .map(|c| {
            if c.is_ascii_digit() {
                digits[(c as u8 - b'0') as usize]
            } else {
                c
            }
        })
        .collect()
}

/// Split the formatted string into integer and fraction digit parts. The
/// string may contain astral (multi-byte) digits before the '.', so scan by
/// char boundaries.
fn split_int_fraction(formatted: &str) -> (String, Option<String>) {
    for (i, c) in formatted.char_indices() {
        if c == '.' && i > 0 {
            return (
                formatted[..i].to_string(),
                Some(formatted[i + 1..].to_string()),
            );
        }
    }
    (formatted.to_string(), None)
}

/// The compact affix for the exponent (the pattern's symbol/name value).
fn compact_affix(record: &NumberFormatRecord, data: &NumberLocaleData, exponent: i64) -> String {
    let short = record.compact_display == DISPLAY_SHORT;
    for entry in data.compact {
        if entry.power as i64 == exponent {
            return (if short {
                entry.short_affix
            } else {
                entry.long_affix
            })
            .to_string();
        }
    }
    String::new()
}

/// PartitionNotationSubPattern (ECMA-402 §16.5.5).
fn partition_notation_sub_pattern(
    record: &NumberFormatRecord,
    data: &NumberLocaleData,
    x: &IntlMv,
    formatted_string: &str,
    exponent: i64,
) -> Vec<Part> {
    let mut result = Vec::new();
    if matches!(x, IntlMv::Nan) {
        result.push(Part::new("nan", formatted_string.to_string()));
        return result;
    }
    if matches!(x, IntlMv::PosInf | IntlMv::NegInf) {
        result.push(Part::new("infinity", formatted_string.to_string()));
        return result;
    }
    let sub_pattern = get_notation_sub_pattern(record, data, exponent);
    let mut formatted = formatted_string.to_string();
    for part in partition_pattern(&sub_pattern) {
        match part {
            PatternPart::Literal(text) => result.push(Part::new("literal", text)),
            PatternPart::Number => {
                formatted = transliterate(&record.numbering_system, &formatted);
                let (int_digits, fraction_digits) = split_int_fraction(&formatted);
                if record.use_grouping == GROUPING_FALSE {
                    result.push(Part::new("integer", int_digits));
                } else {
                    let groups = grouping_groups(
                        &int_digits,
                        data.primary_group,
                        data.secondary_group,
                        record.use_grouping == GROUPING_MIN2,
                    );
                    for (i, group) in groups.iter().enumerate() {
                        result.push(Part::new("integer", group.clone()));
                        if i + 1 < groups.len() {
                            result.push(Part::new("group", data.group.to_string()));
                        }
                    }
                }
                if let Some(fraction) = fraction_digits {
                    result.push(Part::new("decimal", data.decimal.to_string()));
                    result.push(Part::new("fraction", fraction));
                }
            }
            PatternPart::CompactSymbol => {
                result.push(Part::new("compact", compact_affix(record, data, exponent)));
            }
            PatternPart::CompactName => {
                result.push(Part::new("compact", compact_affix(record, data, exponent)));
            }
            PatternPart::ScientificSeparator => {
                result.push(Part::new("exponentSeparator", "E".to_string()));
            }
            PatternPart::ScientificExponent => {
                let mut exp = exponent;
                if exp < 0 {
                    result.push(Part::new("exponentMinusSign", "-".to_string()));
                    exp = -exp;
                }
                result.push(Part::new(
                    "exponentInteger",
                    if exp == 0 {
                        "0".to_string()
                    } else {
                        exp.to_string()
                    },
                ));
            }
            _ => {}
        }
    }
    result
}

/// The integer-digit groups: right-to-left with the primary group first
/// (least significant) and the secondary group repeating. `min2` disables
/// grouping when the leading group would have fewer than 2 digits. The input
/// may contain astral (multi-byte) digits, so group by char count.
fn grouping_groups(int_digits: &str, primary: u32, secondary: u32, min2: bool) -> Vec<String> {
    let chars: Vec<char> = int_digits.chars().collect();
    let len = chars.len() as u32;
    if len <= primary {
        return vec![int_digits.to_string()];
    }
    let mut groups: Vec<String> = Vec::new();
    let mut pos = len;
    let mut size = primary;
    while pos > 0 {
        let take = size.min(pos);
        let start = (pos - take) as usize;
        groups.push(chars[start..pos as usize].iter().collect());
        pos -= take;
        size = secondary;
    }
    groups.reverse();
    // min2: the first secondary group (the one left of the primary) must
    // have at least 2 digits — 1000 → "1000", 10000 → "10,000", en-IN
    // 100000 → "1,00,000" (the "00" middle group).
    if min2 && groups.len() >= 2 && groups[groups.len() - 2].chars().count() < 2 {
        return vec![int_digits.to_string()];
    }
    groups
}

/// The unit display entry for the record's unit (or the fallback: the raw
/// unit id as the suffix). `is_one` picks the plural form for the count
/// (en unit patterns: "1 day" / "2 days").
fn unit_display_for(
    record: &NumberFormatRecord,
    data: &NumberLocaleData,
    unit: &str,
    is_one: bool,
) -> (String, String, String) {
    for entry in data.units {
        if entry.unit == unit {
            let display: &UnitDisplay = match record.unit_display {
                DISPLAY_NARROW_UNIT => &entry.narrow,
                DISPLAY_LONG => &entry.long,
                _ => &entry.short,
            };
            let suffix = if is_one {
                display.suffix
            } else {
                display.plural_suffix
            };
            return (
                display.pattern.to_string(),
                display.prefix.to_string(),
                suffix.to_string(),
            );
        }
    }
    // Fallback: "{number} {unitSuffix}" with the raw unit identifier.
    (
        "{number} {unitSuffix}".to_string(),
        String::new(),
        unit.to_string(),
    )
}

/// The currency prefix/suffix strings for the record.
fn currency_display_string(record: &NumberFormatRecord, data: &NumberLocaleData) -> String {
    let code = record.currency.as_deref().unwrap_or("XXX");
    let mut found = None;
    for &(c, sym, name) in data.currencies {
        if c == code {
            found = Some((sym, name));
            break;
        }
    }
    match record.currency_display {
        DISPLAY_CODE => code.to_string(),
        DISPLAY_NAME => found.map(|(_, name)| name).unwrap_or(code).to_string(),
        _ => found.map(|(sym, _)| sym).unwrap_or(code).to_string(),
    }
}

/// GetNumberFormatPattern (ECMA-402 §16.5.11): the sign-dependent pattern
/// for the value's category.
fn get_number_format_pattern(
    record: &NumberFormatRecord,
    data: &NumberLocaleData,
    x: &IntlMv,
) -> String {
    // The sign category (ECMA-402 §16.5.11): 0 = negative-non-zero,
    // 1 = negative-zero, 2 = positive-non-zero (positive-infinity included),
    // 3 = positive-zero (NaN).
    let category = match x {
        IntlMv::NegInf | IntlMv::Value { negative: true, .. } => 0,
        IntlMv::NegZero => 1,
        IntlMv::PosInf => 2,
        IntlMv::Value {
            negative: false,
            mant,
            ..
        } if !mant.is_zero() => 2,
        _ => 3,
    };
    let base: (String, String, String) = match record.style {
        STYLE_PERCENT => {
            // The percent pattern with the sign placeholders substituted.
            let pattern = data.percent_pattern;
            (
                pattern.replace("{number}", "{plusSign}{number}"),
                pattern.to_string(),
                pattern.replace("{number}", "{minusSign}{number}"),
            )
        }
        STYLE_CURRENCY => {
            let (cpos, cneg, cacct) = currency_templates(record, data);
            let neg = if record.currency_sign == SIGN_ACCOUNTING {
                cacct
            } else {
                cneg
            };
            (format!("{{plusSign}}{cpos}"), cpos, neg)
        }
        STYLE_UNIT => {
            let (upos, uneg) = unit_templates(record, data);
            (format!("{{plusSign}}{upos}"), upos, uneg)
        }
        _ => (
            "{plusSign}{number}".to_string(),
            "{number}".to_string(),
            "{minusSign}{number}".to_string(),
        ),
    };
    let (pos, zero, neg) = base;
    match record.sign_display {
        SIGN_NEVER => zero,
        SIGN_AUTO => {
            if category == 2 || category == 3 {
                zero
            } else {
                neg
            }
        }
        SIGN_ALWAYS => {
            if category == 2 || category == 3 {
                pos
            } else {
                neg
            }
        }
        SIGN_EXCEPT_ZERO => match category {
            3 | 1 => zero,
            2 => pos,
            _ => neg,
        },
        _ => {
            if category == 0 {
                neg
            } else {
                zero
            }
        }
    }
}

/// The currency positive/negative/accounting templates for the display.
fn currency_templates(
    record: &NumberFormatRecord,
    data: &NumberLocaleData,
) -> (String, String, String) {
    let patterns = &data.currency_patterns;
    match record.currency_display {
        DISPLAY_CODE => (
            patterns[3].to_string(),
            patterns[4].to_string(),
            patterns[4].to_string(),
        ),
        DISPLAY_NAME => (
            patterns[5].to_string(),
            patterns[6].to_string(),
            patterns[6].to_string(),
        ),
        _ => (
            patterns[0].to_string(),
            patterns[1].to_string(),
            patterns[2].to_string(),
        ),
    }
}

/// The unit positive/negative templates for the unit/display.
fn unit_templates(record: &NumberFormatRecord, data: &NumberLocaleData) -> (String, String) {
    let unit = record.unit.as_deref().unwrap_or("fallback");
    let (pattern, _, _) = unit_display_for(record, data, unit, false);
    (
        pattern.clone(),
        pattern.replace("{number}", "{minusSign}{number}"),
    )
}

/// PartitionNumberPattern (ECMA-402 §16.5.4).
pub(crate) fn partition_number_pattern(
    record: &NumberFormatRecord,
    data: &NumberLocaleData,
    x: &IntlMv,
) -> Vec<Part> {
    let mut exponent = 0;
    let formatted_string: String;
    let mut value = x.clone();
    if matches!(value, IntlMv::Nan) {
        formatted_string = data.nan.to_string();
    } else if matches!(value, IntlMv::PosInf | IntlMv::NegInf) {
        formatted_string = data.infinity.to_string();
    } else {
        if record.style == STYLE_PERCENT {
            value = value.scale_pow10(2);
        }
        exponent = compute_exponent(record, data, &value);
        value = value.scale_pow10(-exponent);
        let (rounded, formatted) = format_numeric_to_string(record, &value);
        value = rounded;
        formatted_string = formatted;
    }
    let pattern = get_number_format_pattern(record, data, &value);
    // The en plural unit suffix: "1 day" / "2 days" (1 and -1 both use the
    // singular form; the rounded value keeps its fraction scaling, e.g.
    // 1.0 → mant 1000 exp10 -3 under maxFrac 3).
    let unit_is_one = unit_magnitude_is_one(&value);
    let mut result: Vec<Part> = Vec::new();
    for part in partition_pattern(&pattern) {
        match part {
            PatternPart::Literal(text) => result.push(Part::new("literal", text)),
            PatternPart::Number => {
                result.extend(partition_notation_sub_pattern(
                    record,
                    data,
                    &value,
                    &formatted_string,
                    exponent,
                ));
            }
            PatternPart::PlusSign => result.push(Part::new("plusSign", "+".to_string())),
            PatternPart::MinusSign => result.push(Part::new("minusSign", "-".to_string())),
            PatternPart::PercentSign => {
                result.push(Part::new("percentSign", data.percent.to_string()));
            }
            PatternPart::CurrencyCode => {
                result.push(Part::new(
                    "currency",
                    record.currency.clone().unwrap_or_default(),
                ));
            }
            PatternPart::CurrencyPrefix | PatternPart::CurrencySuffix => {
                result.push(Part::new("currency", currency_display_string(record, data)));
            }
            PatternPart::UnitPrefix | PatternPart::UnitSuffix => {
                let unit = record.unit.as_deref().unwrap_or("fallback");
                let (_, prefix, suffix) = unit_display_for(record, data, unit, unit_is_one);
                result.push(Part::new(
                    "unit",
                    if matches!(part, PatternPart::UnitPrefix) {
                        prefix
                    } else {
                        suffix
                    },
                ));
            }
            _ => {}
        }
    }
    result
}

/// Whether an IntlMv has magnitude 1 (the en unit plural "one" form; -1 is
/// also one, the sign is carried separately).
fn unit_magnitude_is_one(value: &IntlMv) -> bool {
    let IntlMv::Value { mant, exp10, .. } = value else {
        return false;
    };
    if *exp10 >= 0 {
        return crux::bigint::to_string(mant, 10) == "1" && *exp10 == 0;
    }
    let digits = crux::bigint::to_string(mant, 10);
    let zeros = (-exp10) as usize;
    digits.len() == zeros + 1
        && digits.starts_with('1')
        && digits.bytes().skip(1).all(|b| b == b'0')
}

/// CollapseNumberRange (ECMA-402 §16.5.21, ICU-shaped): drop the redundant
/// currency affix — the end-range prefix for prefix patterns (+$2.90–3.10),
/// the start-range suffix (with its separator literal) for suffix patterns
/// (3 € – 5 € → 3 – 5 €). A prefix+auto range keeps both currencies
/// ($3 – $5).
fn collapse_number_range(
    record: &NumberFormatRecord,
    data: &NumberLocaleData,
    result: Vec<Part>,
) -> Vec<Part> {
    if record.style != STYLE_CURRENCY {
        return result;
    }
    let prefix = data.currency_patterns[0].ends_with("{number}");
    if prefix && record.sign_display == SIGN_AUTO {
        // The pinned $3 – $5 case: both currencies stay.
        return result;
    }
    if prefix {
        // Drop the end-range affix (its leading sign/literal/currency parts
        // before the first number part).
        let mut out = Vec::new();
        let mut in_end_prefix = false;
        for part in result {
            if part.source.as_deref() == Some("endRange") && !in_end_prefix {
                match part.part_type {
                    "integer" | "decimal" | "fraction" | "group" | "infinity" => {
                        in_end_prefix = true;
                        out.push(part);
                    }
                    "currency" | "plusSign" | "minusSign" | "literal" => {}
                    _ => {
                        in_end_prefix = true;
                        out.push(part);
                    }
                }
            } else {
                out.push(part);
            }
        }
        out
    } else {
        // Suffix pattern: drop the start-range suffix (the currency and the
        // separator literal after the last number part) and the signs of
        // both halves (+2,90 - +3,10 € → 2,90 - 3,10 €).
        let mut last_number = 0usize;
        for (i, part) in result.iter().enumerate() {
            if part.source.as_deref() == Some("startRange")
                && matches!(part.part_type, "integer" | "decimal" | "fraction" | "group")
            {
                last_number = i;
            }
        }
        result
            .into_iter()
            .enumerate()
            .filter(|(i, part)| {
                if part.source.as_deref() == Some("startRange") && *i > last_number {
                    matches!(part.part_type, "plusSign" | "minusSign")
                        || !matches!(part.part_type, "currency" | "literal")
                } else if part.source.as_deref() == Some("endRange") {
                    !matches!(part.part_type, "plusSign" | "minusSign")
                } else {
                    true
                }
            })
            .map(|(_, part)| part)
            .collect()
    }
}

/// PartitionNumberRangePattern (ECMA-402 §16.5.19).
fn partition_number_range_pattern(
    record: &NumberFormatRecord,
    data: &NumberLocaleData,
    x: &IntlMv,
    y: &IntlMv,
) -> Result<Vec<Part>, JsError> {
    if matches!(x, IntlMv::Nan) || matches!(y, IntlMv::Nan) {
        return Err(range_error("Cannot format a range with NaN"));
    }
    let x_result = partition_number_pattern(record, data, x);
    let y_result = partition_number_pattern(record, data, y);
    let x_text: String = x_result.iter().map(|p| p.value.clone()).collect();
    let y_text: String = y_result.iter().map(|p| p.value.clone()).collect();
    if x_text == y_text {
        // FormatApproximately: insert the approximately sign first.
        let mut approx = x_result;
        if !data.approximately.is_empty() {
            approx.insert(
                0,
                Part::new("approximatelySign", data.approximately.to_string()),
            );
        }
        for part in &mut approx {
            part.source = Some("shared".to_string());
        }
        return Ok(approx);
    }
    let mut result: Vec<Part> = Vec::new();
    let mut collapsible = record.style != STYLE_CURRENCY;
    if record.style == STYLE_CURRENCY {
        let prefix = data.currency_patterns[0].ends_with("{number}");
        collapsible = !prefix || record.sign_display != SIGN_AUTO;
    }
    for mut part in x_result {
        part.source = Some("startRange".to_string());
        result.push(part);
    }
    result.push(Part::new(
        "literal",
        if collapsible {
            data.range_separator_collapsed.to_string()
        } else {
            data.range_separator.to_string()
        },
    ));
    if let Some(last) = result.last_mut() {
        last.source = Some("shared".to_string());
    }
    for mut part in y_result {
        part.source = Some("endRange".to_string());
        result.push(part);
    }
    Ok(collapse_number_range(record, data, result))
}

/// FormatNumeric (ECMA-402 §16.5.6).
pub fn format_numeric(record: &NumberFormatRecord, data: &NumberLocaleData, x: &IntlMv) -> String {
    partition_number_pattern(record, data, x)
        .into_iter()
        .map(|part| part.value)
        .collect()
}

/// FormatNumericToParts (ECMA-402 §16.5.7): the array of {type, value} parts.
pub fn format_numeric_to_parts(
    agent: &mut Agent,
    record: &NumberFormatRecord,
    data: &NumberLocaleData,
    x: &IntlMv,
) -> Result<Value, JsError> {
    let parts = partition_number_pattern(record, data, x);
    let object_proto = agent
        .current_realm()?
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let mut array = Vec::new();
    for part in parts {
        let obj = JsObject::ordinary_object_create(object_proto.clone());
        obj.define_property(
            &JsString::from_utf8("type"),
            &PropertyDescriptor {
                value: Some(Value::String(Handle::new(JsString::from_utf8(
                    part.part_type,
                )))),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(true),
                configurable: Some(true),
            },
        )?;
        obj.define_property(
            &JsString::from_utf8("value"),
            &PropertyDescriptor {
                value: Some(Value::String(Handle::new(JsString::from_utf8(&part.value)))),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(true),
                configurable: Some(true),
            },
        )?;
        if let Some(source) = part.source {
            obj.define_property(
                &JsString::from_utf8("source"),
                &PropertyDescriptor {
                    value: Some(Value::String(Handle::new(JsString::from_utf8(&source)))),
                    writable: Some(true),
                    get: None,
                    set: None,
                    enumerable: Some(true),
                    configurable: Some(true),
                },
            )?;
        }
        array.push(Value::Object(obj));
    }
    crate::builtins::array::array_from_values(agent, &array)
}

/// ToIntlMathematicalValue (ECMA-402 §16.5.16).
pub fn to_intl_mathematical_value(agent: &mut Agent, value: &Value) -> Result<IntlMv, JsError> {
    let prim = crate::context::to_primitive(agent, value, crux::convert::ToPrimitiveHint::Number)?;
    if let ValueKind::BigInt(b) = prim.kind() {
        return Ok(bigint_to_mv(b.as_ref().clone()));
    }
    let str_text: String = if let ValueKind::String(s) = prim.kind() {
        s.to_string_lossy()
    } else {
        let number = crate::context::to_number(agent, &prim)?;
        if number == 0.0 && number.is_sign_negative() {
            return Ok(IntlMv::NegZero);
        }
        crux::convert::to_string(&Value::Number(number))
            .map(|s| s.to_string_lossy())
            .unwrap_or_default()
    };
    Ok(parse_string_intl_mv(&str_text))
}

/// BigInt → IntlMV (the exact mathematical value; ECMA-402 §16.5.16 step 2).
pub fn bigint_to_intl_mv(b: crux::BigInt) -> IntlMv {
    bigint_to_mv(b)
}

/// BigInt → IntlMV (the exact mathematical value).
fn bigint_to_mv(b: crux::BigInt) -> IntlMv {
    if b.is_zero() {
        return IntlMv::value(false, BigInt::zero(), 0);
    }
    let digits = crux::bigint::to_string(&b, 10);
    let negative = digits.starts_with('-');
    let abs = if negative { &digits[1..] } else { &digits };
    let mant = BigInt::parse_str(abs, 10).unwrap_or_else(BigInt::zero);
    IntlMv::value(negative, mant, 0)
}

/// Parse a StringNumericLiteral (ECMA-402 §16.5.15 StringIntlMV) into an
/// IntlMV: optional whitespace, optional sign, `Infinity`, decimal digits
/// with an optional fraction and exponent, or a non-decimal integer literal.
pub fn parse_string_intl_mv(text: &str) -> IntlMv {
    let t = text.trim();
    if t.is_empty() {
        return IntlMv::value(false, BigInt::zero(), 0);
    }
    let (negative, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if rest == "Infinity" {
        return if negative {
            IntlMv::NegInf
        } else {
            IntlMv::PosInf
        };
    }
    // Non-decimal integer literal (0x/0b/0o).
    if rest.len() > 2 {
        let (radix, digits) = match &rest[..2] {
            "0x" | "0X" => (16, &rest[2..]),
            "0b" | "0B" => (2, &rest[2..]),
            "0o" | "0O" => (8, &rest[2..]),
            _ => (10, rest),
        };
        if radix != 10
            && !digits.is_empty()
            && digits.bytes().all(|c| c.is_ascii_alphanumeric())
            && let Some(big) = BigInt::parse_str(digits, radix)
        {
            return IntlMv::value(negative, big, 0);
        }
    }
    // Decimal: digits [. digits] [e exponent].
    let mut mantissa_digits = String::new();
    let mut exp = 0i64;
    let mut chars = rest.chars().peekable();
    let mut seen_digit = false;
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            mantissa_digits.push(c);
            seen_digit = true;
            chars.next();
        } else {
            break;
        }
    }
    let mut frac_digits = 0u32;
    if chars.peek() == Some(&'.') {
        chars.next();
        while let Some(&c) = chars.peek() {
            if c.is_ascii_digit() {
                mantissa_digits.push(c);
                frac_digits += 1;
                seen_digit = true;
                chars.next();
            } else {
                break;
            }
        }
    }
    if chars.peek().is_some_and(|c| *c == 'e' || *c == 'E') {
        chars.next();
        let (exp_negative, exp_digits) = match chars.peek() {
            Some('-') => {
                chars.next();
                (true, collect_digits(&mut chars))
            }
            Some('+') => {
                chars.next();
                (false, collect_digits(&mut chars))
            }
            _ => (false, collect_digits(&mut chars)),
        };
        if !exp_digits.is_empty() {
            let e: i64 = exp_digits.parse().unwrap_or(i64::MAX);
            exp = if exp_negative { -e } else { e };
        }
    }
    if !seen_digit {
        return IntlMv::Nan;
    }
    let trimmed = mantissa_digits.trim_start_matches('0');
    let mantissa_digits = if trimmed.is_empty() { "0" } else { trimmed };
    let mant = BigInt::parse_str(mantissa_digits, 10).unwrap_or_else(BigInt::zero);
    let exp10 = exp - frac_digits as i64;
    if mant.is_zero() {
        return if negative {
            IntlMv::NegZero
        } else {
            IntlMv::value(false, BigInt::zero(), 0)
        };
    }
    // The overflow/underflow checks (RoundMVResult): |mv| must fit a Number.
    let approx: f64 = match mantissa_digits.parse::<f64>() {
        Ok(v) => v * 10f64.powi(exp10.clamp(-400, 400) as i32),
        Err(_) => f64::INFINITY,
    };
    if approx.is_infinite() {
        return if negative {
            IntlMv::NegInf
        } else {
            IntlMv::PosInf
        };
    }
    if approx == 0.0 {
        return if negative {
            IntlMv::NegZero
        } else {
            IntlMv::value(false, BigInt::zero(), 0)
        };
    }
    IntlMv::value(negative, mant, exp10)
}

fn collect_digits(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut out = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            out.push(c);
            chars.next();
        } else {
            break;
        }
    }
    out
}

/// The default locale (ECMA-402 §6.2.3).
pub fn default_locale() -> &'static str {
    "en-US"
}

/// CanonicalizeLocaleList is shared with `%Intl%` (mod.rs).
fn canonicalize_locale_list(agent: &mut Agent, locales: &Value) -> Result<Vec<String>, JsError> {
    crate::builtins::intl::canonicalize_locale_list(agent, locales)
}

/// GetOption (ECMA-402 §9.2.11): `values` empty → any string.
pub(crate) fn get_option(
    agent: &mut Agent,
    options: &Value,
    name: &str,
    values: &[&str],
    fallback: Option<&str>,
) -> Result<Option<String>, JsError> {
    let value = get_property(agent, options, &JsString::from_utf8(name), options.clone())?;
    if value.is_undefined() {
        return Ok(fallback.map(|s| s.to_string()));
    }
    let text = to_string(agent, &value)?.to_string_lossy();
    if !values.is_empty() && !values.contains(&text.as_str()) {
        return Err(range_error(&format!(
            "Value {text} out of range for option {name}"
        )));
    }
    Ok(Some(text))
}

/// CoerceOptionsToObject (ECMA-402 §9.2.10): undefined → a null-prototype
/// object; otherwise ToObject.
pub(crate) fn coerce_options_to_object(
    agent: &mut Agent,
    options: &Value,
) -> Result<Value, JsError> {
    if options.is_undefined() {
        Ok(Value::Object(JsObject::ordinary_object_create(None)))
    } else {
        to_object(agent, options)
    }
}

/// GetBooleanOrStringNumberFormatOption (ECMA-402 §9.2.12).
fn get_boolean_or_string_number_format_option(
    agent: &mut Agent,
    options: &Value,
    name: &str,
    string_values: &[&str],
    fallback: &str,
) -> Result<Value, JsError> {
    let value = get_property(agent, options, &JsString::from_utf8(name), options.clone())?;
    if value.is_undefined() {
        return Ok(Value::String(Handle::new(JsString::from_utf8(fallback))));
    }
    if matches!(value.kind(), ValueKind::Boolean(_)) {
        return Ok(value);
    }
    if !crux::convert::to_boolean(&value) {
        return Ok(Value::Boolean(false));
    }
    let text = to_string(agent, &value)?.to_string_lossy();
    if !string_values.contains(&text.as_str()) {
        return Err(range_error(&format!(
            "Value {text} out of range for option {name}"
        )));
    }
    Ok(Value::String(Handle::new(JsString::from_utf8(&text))))
}

/// GetNumberOption (ECMA-402 §9.2.14).
pub(crate) fn get_number_option(
    agent: &mut Agent,
    options: &Value,
    name: &str,
    minimum: f64,
    maximum: f64,
    fallback: f64,
) -> Result<f64, JsError> {
    let value = get_property(agent, options, &JsString::from_utf8(name), options.clone())?;
    if value.is_undefined() {
        return Ok(fallback);
    }
    let number = crate::context::to_number(agent, &value)?;
    if !number.is_finite() || number < minimum || number > maximum {
        return Err(range_error(&format!(
            "Value {number} out of range for option {name}"
        )));
    }
    Ok(number)
}

/// Strip the `-u-...` extension (other extensions and private use stay).
pub(crate) fn strip_unicode_extension(locale: &str) -> String {
    let Some(parts) = crate::builtins::intl::bcp47::parse_locale_id(locale) else {
        return locale.to_string();
    };
    let mut out = parts.base_name();
    for ext in &parts.extensions {
        if !ext.starts_with("u-") {
            out.push('-');
            out.push_str(ext);
        }
    }
    if !parts.privateuse.is_empty() {
        out.push_str("-x");
        for subtag in &parts.privateuse {
            out.push('-');
            out.push_str(subtag);
        }
    }
    out
}

/// Read the value of a keyword inside a `-u-` extension sequence (the
/// value is the following type tokens joined with `-`, e.g. the `ca` value
/// of `islamic-civil`).
pub(crate) fn unicode_extension_keyword_value(extension: &str, key: &str) -> Option<String> {
    let parts: Vec<&str> = extension.split('-').collect();
    let mut i = 0;
    while i < parts.len() {
        if parts[i] == key {
            // The value: the following type tokens (3-8 chars each).
            let mut j = i + 1;
            let mut tokens: Vec<&str> = Vec::new();
            while j < parts.len() && (3..=8).contains(&parts[j].len()) {
                tokens.push(parts[j]);
                j += 1;
            }
            if tokens.is_empty() {
                return None;
            }
            return Some(tokens.join("-"));
        }
        i += 1;
    }
    None
}

/// Insert the `-u-key-value` extension and canonicalize (spec 9.2.6).
pub(crate) fn insert_unicode_extension(locale: &str, key: &str, value: &str) -> String {
    let mut tag = strip_unicode_extension(locale);
    let extension = format!("-u-{key}-{value}");
    if let Some(private_index) = tag.find("-x-") {
        let (pre, post) = tag.split_at(private_index);
        tag = format!("{pre}{extension}{post}");
    } else {
        tag = format!("{tag}{extension}");
    }
    crate::builtins::intl::bcp47::canonicalize(&tag).unwrap_or(tag)
}

/// The best-fit matcher: the requested locale (or its longest available
/// prefix).
pub(crate) fn best_fit(available: &[&str], requested: &str) -> Option<String> {
    if available.contains(&requested) {
        return Some(requested.to_string());
    }
    let subtags: Vec<&str> = requested.split('-').collect();
    let mut end = subtags.len();
    while end > 1 {
        end -= 1;
        let prefix = subtags[..end].join("-");
        if available.contains(&prefix.as_str()) {
            return Some(prefix);
        }
    }
    None
}

/// ResolveLocale (ECMA-402 §9.2.7) with the single `nu` extension key.
/// Returns (resolved_locale, numbering_system).
pub(crate) fn resolve_locale(
    _agent: &mut Agent,
    requested: &[String],
    numbering_system: Option<&str>,
) -> Result<(String, String), JsError> {
    let available = crate::builtins::intl::number_data::NUMBER_FORMAT_LOCALES;
    let mut found: Option<String> = None;
    let mut extension: Option<String> = None;
    for locale in requested {
        let base = strip_unicode_extension(locale);
        if let Some(matched) = best_fit(available, &base) {
            found = Some(matched);
            extension = if base == *locale {
                None
            } else {
                Some(locale.clone())
            };
            break;
        }
    }
    let mut found_locale = found.unwrap_or_else(|| default_locale().to_string());
    let mut nu: String = "latn".to_string();
    let mut supported_keyword: Option<(String, String)> = None;
    if let Some(ext) = extension
        && let Some(value) = unicode_extension_keyword_value(&ext, "nu")
        && !value.is_empty()
    {
        let value = crate::builtins::intl::bcp47::canonicalize_uvalue("nu", &value);
        if supported_numbering_systems().contains(&value.as_str()) {
            nu = value.clone();
            supported_keyword = Some(("nu".to_string(), value));
        }
    }
    if let Some(options_value) = numbering_system {
        let mut options_value =
            crate::builtins::intl::bcp47::canonicalize_uvalue("nu", options_value);
        if options_value.is_empty() {
            options_value = "true".to_string();
        }
        if options_value != nu && supported_numbering_systems().contains(&options_value.as_str()) {
            nu = options_value;
            supported_keyword = None;
        }
    }
    if let Some((key, value)) = supported_keyword {
        found_locale = insert_unicode_extension(&found_locale, &key, &value);
    }
    Ok((found_locale, nu))
}

/// ResolveLocale for a component with no relevant extension keys
/// (PluralRules: `[[RelevantExtensionKeys]]` is « »): the u-extension is
/// dropped from the matched locale entirely.
pub(crate) fn resolve_locale_simple(requested: &[String]) -> Result<String, JsError> {
    let available = crate::builtins::intl::number_data::NUMBER_FORMAT_LOCALES;
    let mut found: Option<String> = None;
    for locale in requested {
        let base = strip_unicode_extension(locale);
        if let Some(matched) = best_fit(available, &base) {
            found = Some(matched);
            break;
        }
    }
    Ok(found.unwrap_or_else(|| default_locale().to_string()))
}

fn supported_numbering_systems() -> &'static [&'static str] {
    crate::builtins::intl::number_data::SUPPORTED_NUMBERING_SYSTEMS
}

/// The `type` Unicode locale nonterminal: alphanumeric subtags of 3-8.
pub(crate) fn is_type_identifier(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    value.split('-').all(|subtag| {
        (3..=8).contains(&subtag.len()) && subtag.bytes().all(|c| c.is_ascii_alphanumeric())
    })
}

/// ResolveOptions (ECMA-402 §9.2.8) for NumberFormat: reads localeMatcher
/// and numberingSystem from the coerced options, then ResolveLocale.
pub(crate) fn resolve_options(
    agent: &mut Agent,
    locales: &Value,
    options: &Value,
) -> Result<(String, String, Value), JsError> {
    let requested = canonicalize_locale_list(agent, locales)?;
    let options = coerce_options_to_object(agent, options)?;
    get_option(
        agent,
        &options,
        "localeMatcher",
        &["lookup", "best fit"],
        Some("best fit"),
    )?;
    let numbering_system = get_option(agent, &options, "numberingSystem", &[], None)?;
    if let Some(value) = &numbering_system
        && !is_type_identifier(value)
    {
        return Err(range_error(
            "Value cannot be matched by the type Unicode locale nonterminal",
        ));
    }
    let (locale, nu) = resolve_locale(agent, &requested, numbering_system.as_deref())?;
    Ok((locale, nu, options))
}

/// IsWellFormedCurrencyCode (ECMA-402 §6.3.1).
fn is_well_formed_currency_code(currency: &str) -> bool {
    currency.len() == 3 && currency.bytes().all(|c| c.is_ascii_alphabetic())
}

/// IsWellFormedUnitIdentifier (ECMA-402 §6.6.1): a sanctioned simple unit,
/// or `simple-per-simple`.
fn is_well_formed_unit_identifier(unit: &str) -> bool {
    let units = crate::builtins::intl::number_data::SANCTIONED_UNITS;
    if units.contains(&unit) {
        return true;
    }
    if let Some((a, b)) = unit.split_once("-per-") {
        return units.contains(&a) && units.contains(&b);
    }
    false
}

/// SetNumberFormatUnitOptions (ECMA-402 §16.1.3).
fn set_number_format_unit_options(
    agent: &mut Agent,
    record: &mut NumberFormatRecord,
    options: &Value,
) -> Result<(), JsError> {
    let style = get_option(
        agent,
        options,
        "style",
        &["decimal", "percent", "currency", "unit"],
        Some("decimal"),
    )?;
    record.style = match style.as_deref() {
        Some("percent") => STYLE_PERCENT,
        Some("currency") => STYLE_CURRENCY,
        Some("unit") => STYLE_UNIT,
        _ => STYLE_DECIMAL,
    };
    let currency = get_option(agent, options, "currency", &[], None)?;
    if let Some(currency) = &currency {
        if !is_well_formed_currency_code(currency) {
            return Err(range_error("Currency code is not well-formed"));
        }
    } else if record.style == STYLE_CURRENCY {
        return Err(type_error("Currency code is required with currency style"));
    }
    let currency_display = get_option(
        agent,
        options,
        "currencyDisplay",
        &["code", "symbol", "narrowSymbol", "name"],
        Some("symbol"),
    )?;
    let currency_sign = get_option(
        agent,
        options,
        "currencySign",
        &["standard", "accounting"],
        Some("standard"),
    )?;
    let unit = get_option(agent, options, "unit", &[], None)?;
    if let Some(unit) = &unit {
        if !is_well_formed_unit_identifier(unit) {
            return Err(range_error("Unit identifier is not well-formed"));
        }
    } else if record.style == STYLE_UNIT {
        return Err(type_error("Unit identifier is required with unit style"));
    }
    let unit_display = get_option(
        agent,
        options,
        "unitDisplay",
        &["short", "narrow", "long"],
        Some("short"),
    )?;
    if record.style == STYLE_CURRENCY {
        record.currency = Some(currency.unwrap_or_default().to_ascii_uppercase());
        record.currency_display = match currency_display.as_deref() {
            Some("code") => DISPLAY_CODE,
            Some("narrowSymbol") => DISPLAY_NARROW,
            Some("name") => DISPLAY_NAME,
            _ => DISPLAY_SYMBOL,
        };
        record.currency_sign = if currency_sign.as_deref() == Some("accounting") {
            SIGN_ACCOUNTING
        } else {
            SIGN_STANDARD
        };
    }
    if record.style == STYLE_UNIT {
        record.unit = unit;
        record.unit_display = match unit_display.as_deref() {
            Some("narrow") => DISPLAY_NARROW_UNIT,
            Some("long") => DISPLAY_LONG,
            _ => DISPLAY_SHORT,
        };
    }
    Ok(())
}

/// DefaultNumberOption on an already-read value (ECMA-402 §9.2.13 applied
/// to the captured Get result, so the option getter runs exactly once).
fn default_number_option_value(
    agent: &mut Agent,
    value: &Value,
    name: &str,
    minimum: f64,
    maximum: f64,
    fallback: Option<f64>,
) -> Result<f64, JsError> {
    if value.is_undefined() {
        return Ok(fallback.unwrap_or(f64::NAN));
    }
    let number = crate::context::to_number(agent, value)?;
    if !number.is_finite() || number < minimum || number > maximum {
        return Err(range_error(&format!(
            "Value {number} out of range for option {name}"
        )));
    }
    Ok(number)
}

/// SetNumberFormatDigitOptions (ECMA-402 §16.1.2).
pub(crate) fn set_number_format_digit_options(
    agent: &mut Agent,
    record: &mut NumberFormatRecord,
    options: &Value,
    mnfd_default: u32,
    mxfd_default: u32,
    notation: &str,
) -> Result<(), JsError> {
    let mnid = get_number_option(agent, options, "minimumIntegerDigits", 1.0, 21.0, 1.0)?;
    let mnfd = get_property(
        agent,
        options,
        &JsString::from_utf8("minimumFractionDigits"),
        options.clone(),
    )?;
    let mxfd = get_property(
        agent,
        options,
        &JsString::from_utf8("maximumFractionDigits"),
        options.clone(),
    )?;
    let mnsd = get_property(
        agent,
        options,
        &JsString::from_utf8("minimumSignificantDigits"),
        options.clone(),
    )?;
    let mxsd = get_property(
        agent,
        options,
        &JsString::from_utf8("maximumSignificantDigits"),
        options.clone(),
    )?;
    record.minimum_integer_digits = mnid as u32;
    let rounding_increment =
        get_number_option(agent, options, "roundingIncrement", 1.0, 5000.0, 1.0)?;
    let rounding_increment = rounding_increment as u32;
    if !matches!(
        rounding_increment,
        1 | 2 | 5 | 10 | 20 | 25 | 50 | 100 | 200 | 250 | 500 | 1000 | 2000 | 2500 | 5000
    ) {
        return Err(range_error(
            "roundingIncrement is not one of the allowed values",
        ));
    }
    let rounding_mode = get_option(
        agent,
        options,
        "roundingMode",
        &[
            "ceil",
            "floor",
            "expand",
            "trunc",
            "halfCeil",
            "halfFloor",
            "halfExpand",
            "halfTrunc",
            "halfEven",
        ],
        Some("halfExpand"),
    )?;
    let rounding_priority = get_option(
        agent,
        options,
        "roundingPriority",
        &["auto", "morePrecision", "lessPrecision"],
        Some("auto"),
    )?;
    let trailing_zero_display = get_option(
        agent,
        options,
        "trailingZeroDisplay",
        &["auto", "stripIfInteger"],
        Some("auto"),
    )?;
    // All option reads are done.
    let mut mxfd_default = mxfd_default;
    if rounding_increment != 1 {
        // The rounding-increment path forces the fraction defaults equal.
        mxfd_default = mnfd_default;
    }
    record.rounding_increment = rounding_increment;
    record.rounding_mode = match rounding_mode.as_deref() {
        Some("ceil") => ROUNDING_MODE_CEIL,
        Some("floor") => ROUNDING_MODE_FLOOR,
        Some("expand") => ROUNDING_MODE_EXPAND,
        Some("trunc") => ROUNDING_MODE_TRUNC,
        Some("halfCeil") => ROUNDING_MODE_HALF_CEIL,
        Some("halfFloor") => ROUNDING_MODE_HALF_FLOOR,
        Some("halfTrunc") => ROUNDING_MODE_HALF_TRUNC,
        Some("halfEven") => ROUNDING_MODE_HALF_EVEN,
        _ => ROUNDING_MODE_HALF_EXPAND,
    };
    record.trailing_zero_display = if trailing_zero_display.as_deref() == Some("stripIfInteger") {
        TZD_STRIP
    } else {
        TZD_AUTO
    };
    let has_sd = !mnsd.is_undefined() || !mxsd.is_undefined();
    let has_fd = !mnfd.is_undefined() || !mxfd.is_undefined();
    let rounding_priority = rounding_priority.unwrap_or_else(|| "auto".to_string());
    let mut need_sd = true;
    let mut need_fd = true;
    if rounding_priority == "auto" {
        need_sd = has_sd;
        if need_sd || (!has_fd && notation == "compact") {
            need_fd = false;
        }
    }
    if need_sd {
        if has_sd {
            // DefaultNumberOption on the already-read values (the option
            // getters run exactly once — significant-digits-options-get-
            // sequence.js).
            record.minimum_significant_digits = default_number_option_value(
                agent,
                &mnsd,
                "minimumSignificantDigits",
                1.0,
                21.0,
                Some(1.0),
            )? as u32;
            record.maximum_significant_digits = default_number_option_value(
                agent,
                &mxsd,
                "maximumSignificantDigits",
                record.minimum_significant_digits as f64,
                21.0,
                Some(21.0),
            )? as u32;
        } else {
            record.minimum_significant_digits = 1;
            record.maximum_significant_digits = 21;
        }
    }
    if need_fd {
        if has_fd {
            let mut mnfd = default_number_option_value(
                agent,
                &mnfd,
                "minimumFractionDigits",
                0.0,
                100.0,
                None,
            )?;
            let mut mxfd = default_number_option_value(
                agent,
                &mxfd,
                "maximumFractionDigits",
                0.0,
                100.0,
                None,
            )?;
            if mnfd.is_nan() {
                mnfd = (mnfd_default as f64).min(mxfd);
            } else if mxfd.is_nan() {
                mxfd = (mxfd_default as f64).max(mnfd);
            } else if mnfd > mxfd {
                return Err(range_error(
                    "minimumFractionDigits exceeds maximumFractionDigits",
                ));
            }
            record.minimum_fraction_digits = mnfd as u32;
            record.maximum_fraction_digits = mxfd as u32;
        } else {
            record.minimum_fraction_digits = mnfd_default;
            record.maximum_fraction_digits = mxfd_default;
        }
    }
    if !need_sd && !need_fd {
        record.minimum_fraction_digits = 0;
        record.maximum_fraction_digits = 0;
        record.minimum_significant_digits = 1;
        record.maximum_significant_digits = 2;
        record.rounding_type = ROUNDING_MORE;
        record.computed_rounding_priority = "morePrecision";
    } else if rounding_priority == "morePrecision" {
        record.rounding_type = ROUNDING_MORE;
        record.computed_rounding_priority = "morePrecision";
    } else if rounding_priority == "lessPrecision" {
        record.rounding_type = ROUNDING_LESS;
        record.computed_rounding_priority = "lessPrecision";
    } else if has_sd {
        record.rounding_type = ROUNDING_SIGNIFICANT;
        record.computed_rounding_priority = "auto";
    } else {
        record.rounding_type = ROUNDING_FRACTION;
        record.computed_rounding_priority = "auto";
    }
    if record.rounding_increment != 1 {
        if record.rounding_type != ROUNDING_FRACTION {
            return Err(type_error(
                "roundingIncrement requires fraction-digit rounding",
            ));
        }
        if record.maximum_fraction_digits != record.minimum_fraction_digits {
            return Err(range_error(
                "roundingIncrement requires equal min/max fraction digits",
            ));
        }
    }
    Ok(())
}

/// Intl.NumberFormat (ECMA-402 §16.1.1): the shared initialization pipeline.
pub fn initialize(
    agent: &mut Agent,
    locales: &Value,
    options: &Value,
) -> Result<NumberFormatRecord, JsError> {
    let (locale, numbering_system, options) = resolve_options(agent, locales, options)?;
    let mut record = NumberFormatRecord {
        locale,
        numbering_system,
        style: STYLE_DECIMAL,
        currency: None,
        currency_display: DISPLAY_SYMBOL,
        currency_sign: SIGN_STANDARD,
        unit: None,
        unit_display: DISPLAY_SHORT,
        minimum_integer_digits: 1,
        minimum_fraction_digits: 0,
        maximum_fraction_digits: 3,
        minimum_significant_digits: 1,
        maximum_significant_digits: 21,
        rounding_type: ROUNDING_FRACTION,
        notation: NOTATION_STANDARD,
        compact_display: DISPLAY_SHORT,
        use_grouping: GROUPING_AUTO,
        sign_display: SIGN_AUTO,
        rounding_increment: 1,
        rounding_mode: ROUNDING_MODE_HALF_EXPAND,
        computed_rounding_priority: "auto",
        trailing_zero_display: TZD_AUTO,
        bound_format: None,
    };
    set_number_format_unit_options(agent, &mut record, &options)?;
    let style = record.style;
    let notation = get_option(
        agent,
        &options,
        "notation",
        &["standard", "scientific", "engineering", "compact"],
        Some("standard"),
    )?;
    record.notation = match notation.as_deref() {
        Some("scientific") => NOTATION_SCIENTIFIC,
        Some("engineering") => NOTATION_ENGINEERING,
        Some("compact") => NOTATION_COMPACT,
        _ => NOTATION_STANDARD,
    };
    let (mnfd_default, mxfd_default) =
        if style == STYLE_CURRENCY && record.notation == NOTATION_STANDARD {
            let currency = record.currency.clone().unwrap_or_default();
            let digits = crate::builtins::intl::number_data::currency_digits(&currency);
            (digits, digits)
        } else {
            let mxfd = if style == STYLE_PERCENT { 0 } else { 3 };
            (0, mxfd)
        };
    set_number_format_digit_options(
        agent,
        &mut record,
        &options,
        mnfd_default,
        mxfd_default,
        notation.as_deref().unwrap_or("standard"),
    )?;
    let compact_display = get_option(
        agent,
        &options,
        "compactDisplay",
        &["short", "long"],
        Some("short"),
    )?;
    let mut default_use_grouping = "auto";
    if record.notation == NOTATION_COMPACT {
        record.compact_display = if compact_display.as_deref() == Some("long") {
            DISPLAY_LONG
        } else {
            DISPLAY_SHORT
        };
        default_use_grouping = "min2";
    }
    let use_grouping = get_boolean_or_string_number_format_option(
        agent,
        &options,
        "useGrouping",
        &["min2", "auto", "always", "true", "false"],
        default_use_grouping,
    )?;
    let use_grouping = match use_grouping.kind() {
        ValueKind::String(s) => {
            let s = s.to_string_lossy();
            if s == "true" || s == "false" {
                default_use_grouping.to_string()
            } else {
                s
            }
        }
        ValueKind::Boolean(true) => "always".to_string(),
        _ => "false".to_string(),
    };
    record.use_grouping = match use_grouping.as_str() {
        "always" => GROUPING_ALWAYS,
        "min2" => GROUPING_MIN2,
        "false" => GROUPING_FALSE,
        _ => GROUPING_AUTO,
    };
    let sign_display = get_option(
        agent,
        &options,
        "signDisplay",
        &["auto", "never", "always", "exceptZero", "negative"],
        Some("auto"),
    )?;
    record.sign_display = match sign_display.as_deref() {
        Some("never") => SIGN_NEVER,
        Some("always") => SIGN_ALWAYS,
        Some("exceptZero") => SIGN_EXCEPT_ZERO,
        Some("negative") => SIGN_NEGATIVE,
        _ => SIGN_AUTO,
    };
    Ok(record)
}

/// Get the NumberFormat record of `this` (RequireInternalSlot).
fn number_format_record(agent: &Agent, this: &Value) -> Result<NumberFormatRecord, JsError> {
    let Some(obj) = as_object(this) else {
        return Err(type_error(
            "Intl.NumberFormat method called on a non-object",
        ));
    };
    agent
        .intl_number_format_data
        .get(&obj.id())
        .cloned()
        .ok_or_else(|| type_error("Intl.NumberFormat method called on an uninitialized object"))
}

/// OrdinaryHasInstance(%Intl.NumberFormat%, value): a prototype-chain walk
/// that honors the exotic [[GetPrototypeOf]] (proxy getPrototypeOf trap).
fn ordinary_has_instance_number_format(agent: &mut Agent, value: &Value) -> bool {
    let Some(mut current) = as_object(value) else {
        return false;
    };
    let Some(proto) = agent
        .current_realm()
        .ok()
        .and_then(|realm| realm.intrinsics.get(NUMBER_FORMAT_PROTO))
        .and_then(|v| as_object(&v))
    else {
        return false;
    };
    loop {
        let Some(parent) = current.get_prototype_of().ok().flatten() else {
            return false;
        };
        if parent.id() == proto.id() {
            return true;
        }
        current = parent;
    }
}

/// UnwrapNumberFormat (ECMA-402 §16.5.10): the legacy-constructor path
/// reads the [[FallbackSymbol]] property (a proxy `get` trap must fire —
/// intl-legacy-constructed-symbol-on-unwrap.js).
fn unwrap_number_format(agent: &mut Agent, nf: &Value) -> Result<Value, JsError> {
    let Some(obj) = as_object(nf) else {
        return Err(type_error(
            "Intl.NumberFormat method called on a non-object",
        ));
    };
    if !agent.intl_number_format_data.contains_key(&obj.id())
        && ordinary_has_instance_number_format(agent, nf)
    {
        let key = PropertyKey::Symbol(fallback_symbol(agent)?);
        let value = crate::context::get_property_key(agent, nf, &key, nf.clone())?;
        if value.is_undefined() {
            return Err(type_error(
                "Intl.NumberFormat method called on an incompatible receiver",
            ));
        }
        return Ok(value);
    }
    Ok(nf.clone())
}

/// The %Intl%.[[FallbackSymbol]] of the current realm: a per-realm private
/// symbol with the description "IntlLegacyConstructedSymbol", cached in the
/// realm intrinsics (the FallbackSymbol/per-realm fixture pins that two
/// realms get distinct symbols).
pub(crate) fn fallback_symbol(agent: &Agent) -> Result<crux::symbol::Symbol, JsError> {
    const INTL_FALLBACK_SYMBOL: &str = "%Intl.FallbackSymbol%";
    let realm = agent.current_realm()?;
    if let Some(value) = realm.intrinsics.get(INTL_FALLBACK_SYMBOL)
        && let ValueKind::Symbol(sym) = value.kind()
    {
        return Ok(sym.as_ref().clone());
    }
    let symbol =
        crux::symbol::Symbol::new(Some(JsString::from_utf8("IntlLegacyConstructedSymbol")));
    realm.intrinsics.define(
        INTL_FALLBACK_SYMBOL,
        Value::Symbol(Handle::new(symbol.clone())),
    );
    Ok(symbol)
}

/// Install `Intl.NumberFormat` and its prototype (ECMA-402 §16).
pub fn install(realm: &Handle<Realm>, intl_value: &Value) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let function_proto = realm
        .intrinsics
        .get("%Function.prototype%")
        .and_then(|value| as_object(&value));
    let proto = JsObject::ordinary_object_create(object_proto);
    let ctor = Function::create_builtin(
        Some(JsString::from_utf8("NumberFormat")),
        0,
        placeholder("Intl.NumberFormat"),
        Some(placeholder_ctor("Intl.NumberFormat")),
        function_proto.clone(),
    )?;
    proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(Value::Function(ctor.clone())),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    // The prototype methods.
    let methods: &[(&str, &str, u64)] = &[
        ("resolvedOptions", NF_RESOLVED_OPTIONS, 0),
        ("formatRange", NF_FORMAT_RANGE, 2),
        ("formatRangeToParts", NF_FORMAT_RANGE_TO_PARTS, 2),
        ("formatToParts", NF_FORMAT_TO_PARTS, 1),
    ];
    for (name, key, length) in methods {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            *length,
            placeholder(name),
            None,
            function_proto.clone(),
        )?;
        realm.intrinsics.define(key, Value::Function(func.clone()));
        proto.define_property(
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
    // The format accessor.
    let format_getter = Function::create_builtin(
        Some(JsString::from_utf8("get format")),
        0,
        placeholder("format getter"),
        None,
        function_proto.clone(),
    )?;
    realm
        .intrinsics
        .define(NF_FORMAT_GETTER, Value::Function(format_getter.clone()));
    proto.define_property(
        &JsString::from_utf8("format"),
        &PropertyDescriptor {
            value: None,
            writable: None,
            get: Some(Value::Function(format_getter)),
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    // %Intl.NumberFormat.prototype%[@@toStringTag] = "Intl.NumberFormat".
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8(
                "Intl.NumberFormat",
            )))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    let proto_value = Value::Object(proto.clone());
    ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    // Intl.NumberFormat.supportedLocalesOf.
    let supported = Function::create_builtin(
        Some(JsString::from_utf8("supportedLocalesOf")),
        1,
        placeholder("supportedLocalesOf"),
        None,
        function_proto.clone(),
    )?;
    realm
        .intrinsics
        .define(NF_SUPPORTED_LOCALES_OF, Value::Function(supported.clone()));
    ctor.define_property(
        &JsString::from_utf8("supportedLocalesOf"),
        &PropertyDescriptor {
            value: Some(Value::Function(supported)),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    realm.intrinsics.define(NUMBER_FORMAT_PROTO, proto_value);
    realm
        .intrinsics
        .define(NUMBER_FORMAT, Value::Function(ctor.clone()));
    if let Some(obj) = as_object(intl_value) {
        obj.define_property(
            &JsString::from_utf8("NumberFormat"),
            &PropertyDescriptor {
                value: Some(Value::Function(ctor)),
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

fn placeholder(name: &str) -> NativeFn {
    let name = name.to_string();
    Box::new(move |_, _| Err(type_error(&format!("{name} must be dispatched"))))
}

fn placeholder_ctor(name: &str) -> NativeFn {
    let name = name.to_string();
    Box::new(move |_, _| Err(type_error(&format!("{name} must be dispatched"))))
}

/// Create the NumberFormat instance object for the record.
fn create_instance(
    agent: &mut Agent,
    proto: Option<&Handle<JsObject>>,
    record: NumberFormatRecord,
) -> Result<Value, JsError> {
    let realm = agent.current_realm()?;
    let proto = match proto {
        Some(proto) => proto.clone(),
        None => realm
            .intrinsics
            .get(NUMBER_FORMAT_PROTO)
            .and_then(|value| as_object(&value))
            .ok_or_else(|| type_error("%Intl.NumberFormat.prototype% missing"))?,
    };
    let instance = JsObject::ordinary_object_create(Some(proto));
    agent.intl_number_format_data.insert(instance.id(), record);
    Ok(Value::Object(instance))
}

/// Intl.NumberFormat (ECMA-402 §16.1.1) with ChainNumberFormat: `this` is
/// the call-path this value (undefined for `new`), `new_target_undefined`
/// tells whether the original NewTarget was undefined.
fn construct_inner(
    agent: &mut Agent,
    new_target: &Value,
    this: &Value,
    new_target_was_undefined: bool,
    args: &[Value],
) -> Result<Value, JsError> {
    // GetPrototypeFromConstructor: the newTarget's `prototype`, falling back
    // to %Intl.NumberFormat.prototype% (the subclassing path).
    let proto = crate::context::get_property(
        agent,
        new_target,
        &JsString::from_utf8("prototype"),
        new_target.clone(),
    )?;
    let proto = if let Some(obj) = as_object(&proto) {
        obj
    } else {
        crate::context::get_function_realm(agent, new_target)?
            .intrinsics
            .get(NUMBER_FORMAT_PROTO)
            .and_then(|value| as_object(&value))
            .ok_or_else(|| type_error("%Intl.NumberFormat.prototype% missing"))?
    };
    let locales = args.first().cloned().unwrap_or(Value::Undefined);
    let options = args.get(1).cloned().unwrap_or(Value::Undefined);
    let record = initialize(agent, &locales, &options)?;
    // ChainNumberFormat (ECMA-402 §16.1.1.1): the legacy constructor mode.
    if new_target_was_undefined && let Some(this_obj) = as_object(this) {
        let is_instance = agent.intl_number_format_data.contains_key(&this_obj.id())
            || ordinary_has_instance_number_format(agent, this);
        if is_instance {
            let inner = create_instance(agent, Some(&proto), record)?;
            this_obj.define_property_key(
                &PropertyKey::Symbol(fallback_symbol(agent)?),
                &PropertyDescriptor {
                    value: Some(inner.clone()),
                    writable: Some(false),
                    get: None,
                    set: None,
                    enumerable: Some(false),
                    configurable: Some(false),
                },
            )?;
            return Ok(this.clone());
        }
    }
    create_instance(agent, Some(&proto), record)
}

/// dispatch_call: the NumberFormat constructor (as a function), the
/// prototype members, and the per-instance bound format functions.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(NUMBER_FORMAT).as_ref() == Some(callee) {
        // Called as a function: NewTarget is undefined → the ctor itself.
        return Some(construct_inner(agent, callee, this, true, args));
    }
    if intrinsics.get(NF_SUPPORTED_LOCALES_OF).as_ref() == Some(callee) {
        return Some(supported_locales_of(
            agent,
            args.first().cloned().unwrap_or(Value::Undefined),
            args.get(1).cloned().unwrap_or(Value::Undefined),
        ));
    }
    if intrinsics.get(NF_RESOLVED_OPTIONS).as_ref() == Some(callee) {
        return Some(resolved_options_method(agent, this));
    }
    if intrinsics.get(NF_FORMAT_GETTER).as_ref() == Some(callee) {
        return Some(format_getter(agent, this));
    }
    if intrinsics.get(NF_FORMAT_TO_PARTS).as_ref() == Some(callee) {
        return Some(format_to_parts(agent, this, args));
    }
    if intrinsics.get(NF_FORMAT_RANGE).as_ref() == Some(callee) {
        return Some(format_range(agent, this, args, false));
    }
    if intrinsics.get(NF_FORMAT_RANGE_TO_PARTS).as_ref() == Some(callee) {
        return Some(format_range(agent, this, args, true));
    }
    // The per-instance bound format functions.
    if let ValueKind::Function(function) = callee.kind()
        && let Some(nf_id) = agent.intl_format_functions.get(&function.id()).copied()
    {
        return Some(format_bound(agent, nf_id, args));
    }
    None
}

/// dispatch_construct: `new Intl.NumberFormat(...)`.
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    if realm.intrinsics.get(NUMBER_FORMAT).as_ref() == Some(callee) {
        return Some(construct_inner(
            agent,
            new_target,
            &Value::Undefined,
            false,
            args,
        ));
    }
    None
}

/// The format accessor (ECMA-402 §16.3.3): returns the cached bound
/// function.
fn format_getter(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let nf = unwrap_number_format(agent, this)?;
    let mut record = number_format_record(agent, &nf)?;
    if let Some(bound) = &record.bound_format {
        return Ok(bound.clone());
    }
    let Some(obj) = as_object(&nf) else {
        return Err(type_error(
            "Intl.NumberFormat method called on a non-object",
        ));
    };
    let nf_id = obj.id();
    let function_proto = agent
        .current_realm()?
        .intrinsics
        .get("%Function.prototype%")
        .and_then(|value| as_object(&value));
    let func = Function::create_builtin(
        Some(JsString::from_utf8("")),
        1,
        placeholder("bound format"),
        None,
        function_proto,
    )?;
    agent.intl_format_functions.insert(func.id(), nf_id);
    let bound = Value::Function(func);
    record.bound_format = Some(bound.clone());
    agent.intl_number_format_data.insert(nf_id, record);
    Ok(bound)
}

/// The bound format function body: format the argument.
fn format_bound(agent: &mut Agent, nf_id: u64, args: &[Value]) -> Result<Value, JsError> {
    let record = agent
        .intl_number_format_data
        .get(&nf_id)
        .cloned()
        .ok_or_else(|| type_error("Intl.NumberFormat method called on an incompatible receiver"))?;
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    let x = to_intl_mathematical_value(agent, &value)?;
    let data = locale_data(&record.locale);
    let text = format_numeric(&record, data, &x);
    Ok(Value::String(Handle::new(JsString::from_utf8(&text))))
}

/// Intl.NumberFormat.supportedLocalesOf (ECMA-402 §16.2.2).
fn supported_locales_of(
    agent: &mut Agent,
    locales: Value,
    options: Value,
) -> Result<Value, JsError> {
    let requested = canonicalize_locale_list(agent, &locales)?;
    let options = coerce_options_to_object(agent, &options)?;
    get_option(
        agent,
        &options,
        "localeMatcher",
        &["lookup", "best fit"],
        Some("best fit"),
    )?;
    let available = crate::builtins::intl::number_data::NUMBER_FORMAT_LOCALES;
    let mut subset = Vec::new();
    for locale in &requested {
        let base = strip_unicode_extension(locale);
        if best_fit(available, &base).is_some() {
            subset.push(Value::String(Handle::new(JsString::from_utf8(locale))));
        }
    }
    crate::builtins::array::array_from_values(agent, &subset)
}

/// Intl.NumberFormat.prototype.resolvedOptions (ECMA-402 §16.3.2).
fn resolved_options_method(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let nf = unwrap_number_format(agent, this)?;
    let record = number_format_record(agent, &nf)?;
    let object_proto = agent
        .current_realm()?
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let options = JsObject::ordinary_object_create(object_proto);
    let define = |name: &str, value: Option<Value>| -> Result<(), JsError> {
        if let Some(value) = value {
            options.define_property(
                &JsString::from_utf8(name),
                &PropertyDescriptor {
                    value: Some(value),
                    writable: Some(true),
                    get: None,
                    set: None,
                    enumerable: Some(true),
                    configurable: Some(true),
                },
            )?;
        }
        Ok(())
    };
    let str = |s: String| Value::String(Handle::new(JsString::from_utf8(&s)));
    define("locale", Some(str(record.locale.clone())))?;
    define(
        "numberingSystem",
        Some(str(record.numbering_system.clone())),
    )?;
    define("style", Some(str(style_name(record.style).to_string())))?;
    if record.style == STYLE_CURRENCY {
        define("currency", record.currency.clone().map(str))?;
        define(
            "currencyDisplay",
            Some(str(
                currency_display_name(record.currency_display).to_string()
            )),
        )?;
        define(
            "currencySign",
            Some(str(if record.currency_sign == SIGN_ACCOUNTING {
                "accounting"
            } else {
                "standard"
            }
            .to_string())),
        )?;
    } else if record.style == STYLE_UNIT {
        define("unit", record.unit.clone().map(str))?;
        define(
            "unitDisplay",
            Some(str(unit_display_name(record.unit_display).to_string())),
        )?;
    }
    define(
        "minimumIntegerDigits",
        Some(Value::Number(record.minimum_integer_digits as f64)),
    )?;
    if record.rounding_type != ROUNDING_SIGNIFICANT {
        define(
            "minimumFractionDigits",
            Some(Value::Number(record.minimum_fraction_digits as f64)),
        )?;
        define(
            "maximumFractionDigits",
            Some(Value::Number(record.maximum_fraction_digits as f64)),
        )?;
    }
    if record.rounding_type != ROUNDING_FRACTION {
        define(
            "minimumSignificantDigits",
            Some(Value::Number(record.minimum_significant_digits as f64)),
        )?;
        define(
            "maximumSignificantDigits",
            Some(Value::Number(record.maximum_significant_digits as f64)),
        )?;
    }
    define(
        "useGrouping",
        Some(if record.use_grouping == GROUPING_FALSE {
            Value::Boolean(false)
        } else {
            str(use_grouping_name(record.use_grouping).to_string())
        }),
    )?;
    define(
        "notation",
        Some(str(notation_name(record.notation).to_string())),
    )?;
    if record.notation == NOTATION_COMPACT {
        define(
            "compactDisplay",
            Some(str(if record.compact_display == DISPLAY_LONG {
                "long"
            } else {
                "short"
            }
            .to_string())),
        )?;
    }
    define(
        "signDisplay",
        Some(str(sign_display_name(record.sign_display).to_string())),
    )?;
    define(
        "roundingIncrement",
        Some(Value::Number(record.rounding_increment as f64)),
    )?;
    define(
        "roundingMode",
        Some(str(rounding_mode_name(record.rounding_mode).to_string())),
    )?;
    define(
        "roundingPriority",
        Some(str(record.computed_rounding_priority.to_string())),
    )?;
    define(
        "trailingZeroDisplay",
        Some(str(if record.trailing_zero_display == TZD_STRIP {
            "stripIfInteger"
        } else {
            "auto"
        }
        .to_string())),
    )?;
    Ok(Value::Object(options))
}

fn style_name(style: u8) -> &'static str {
    match style {
        STYLE_PERCENT => "percent",
        STYLE_CURRENCY => "currency",
        STYLE_UNIT => "unit",
        _ => "decimal",
    }
}

fn currency_display_name(display: u8) -> &'static str {
    match display {
        DISPLAY_CODE => "code",
        DISPLAY_NARROW => "narrowSymbol",
        DISPLAY_NAME => "name",
        _ => "symbol",
    }
}

fn unit_display_name(display: u8) -> &'static str {
    match display {
        DISPLAY_NARROW_UNIT => "narrow",
        DISPLAY_LONG => "long",
        _ => "short",
    }
}

fn use_grouping_name(grouping: u8) -> &'static str {
    match grouping {
        GROUPING_ALWAYS => "always",
        GROUPING_MIN2 => "min2",
        GROUPING_FALSE => "false",
        _ => "auto",
    }
}

fn notation_name(notation: u8) -> &'static str {
    match notation {
        NOTATION_SCIENTIFIC => "scientific",
        NOTATION_ENGINEERING => "engineering",
        NOTATION_COMPACT => "compact",
        _ => "standard",
    }
}

fn sign_display_name(sign: u8) -> &'static str {
    match sign {
        SIGN_NEVER => "never",
        SIGN_ALWAYS => "always",
        SIGN_EXCEPT_ZERO => "exceptZero",
        SIGN_NEGATIVE => "negative",
        _ => "auto",
    }
}

fn rounding_mode_name(mode: u8) -> &'static str {
    match mode {
        ROUNDING_MODE_CEIL => "ceil",
        ROUNDING_MODE_FLOOR => "floor",
        ROUNDING_MODE_EXPAND => "expand",
        ROUNDING_MODE_TRUNC => "trunc",
        ROUNDING_MODE_HALF_CEIL => "halfCeil",
        ROUNDING_MODE_HALF_FLOOR => "halfFloor",
        ROUNDING_MODE_HALF_TRUNC => "halfTrunc",
        ROUNDING_MODE_HALF_EVEN => "halfEven",
        _ => "halfExpand",
    }
}

/// Intl.NumberFormat.prototype.formatToParts (ECMA-402 §16.3.6).
fn format_to_parts(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let record = number_format_record(agent, this)?;
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    let x = to_intl_mathematical_value(agent, &value)?;
    let data = locale_data(&record.locale);
    format_numeric_to_parts(agent, &record, data, &x)
}

/// Intl.NumberFormat.prototype.formatRange / formatRangeToParts
/// (ECMA-402 §16.3.4/16.3.5).
fn format_range(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    to_parts: bool,
) -> Result<Value, JsError> {
    let record = number_format_record(agent, this)?;
    let start = args.first().cloned().unwrap_or(Value::Undefined);
    let end = args.get(1).cloned().unwrap_or(Value::Undefined);
    if start.is_undefined() || end.is_undefined() {
        return Err(type_error(
            "formatRange requires both start and end arguments",
        ));
    }
    let x = to_intl_mathematical_value(agent, &start)?;
    let y = to_intl_mathematical_value(agent, &end)?;
    let data = locale_data(&record.locale);
    let parts = partition_number_range_pattern(&record, data, &x, &y)?;
    if to_parts {
        let object_proto = agent
            .current_realm()?
            .intrinsics
            .get("%Object.prototype%")
            .and_then(|value| as_object(&value));
        let mut array = Vec::new();
        for part in parts {
            let obj = JsObject::ordinary_object_create(object_proto.clone());
            obj.define_property(
                &JsString::from_utf8("type"),
                &PropertyDescriptor {
                    value: Some(Value::String(Handle::new(JsString::from_utf8(
                        part.part_type,
                    )))),
                    writable: Some(true),
                    get: None,
                    set: None,
                    enumerable: Some(true),
                    configurable: Some(true),
                },
            )?;
            obj.define_property(
                &JsString::from_utf8("value"),
                &PropertyDescriptor {
                    value: Some(Value::String(Handle::new(JsString::from_utf8(&part.value)))),
                    writable: Some(true),
                    get: None,
                    set: None,
                    enumerable: Some(true),
                    configurable: Some(true),
                },
            )?;
            if let Some(source) = part.source {
                obj.define_property(
                    &JsString::from_utf8("source"),
                    &PropertyDescriptor {
                        value: Some(Value::String(Handle::new(JsString::from_utf8(&source)))),
                        writable: Some(true),
                        get: None,
                        set: None,
                        enumerable: Some(true),
                        configurable: Some(true),
                    },
                )?;
            }
            array.push(Value::Object(obj));
        }
        crate::builtins::array::array_from_values(agent, &array)
    } else {
        let text: String = parts.into_iter().map(|p| p.value).collect();
        Ok(Value::String(Handle::new(JsString::from_utf8(&text))))
    }
}

/// Number.prototype.toLocaleString / BigInt.prototype.toLocaleString:
/// construct a NumberFormat for (locales, options) and format the value.
pub fn to_locale_string(
    agent: &mut Agent,
    locales: &Value,
    options: &Value,
    x: &IntlMv,
) -> Result<String, JsError> {
    let record = initialize(agent, locales, options)?;
    let data = locale_data(&record.locale);
    Ok(format_numeric(&record, data, x))
}
