//! The `Temporal` object (proposal-temporal, stage 4): records shared by the
//! Temporal types, the common option/rounding machinery, and the namespace
//! object with `Temporal.Now`. Bodies dispatch by intrinsic identity from
//! `runtime::function::call`/`construct` (the %eval% pattern).

pub mod calendar;
pub mod duration;
pub mod instant;
pub mod iso;
pub mod lunar_tables;
pub mod shell;

use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::heap::{GcAny, Trace};
use crux::object::JsObject;
use crux::property::PropertyDescriptor;
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use crate::agent::Agent;
use crate::context::as_object;
use crate::realm::Realm;

use iso::{Category, FracPrecision, RoundingMode, Unit};

const TEMPORAL: &str = "%Temporal%";
const NOW: &str = "%Temporal.Now%";
const NOW_TZ: &str = "%Temporal.Now.timeZoneId%";
const NOW_INSTANT: &str = "%Temporal.Now.instant%";
const NOW_PLAIN_DATE: &str = "%Temporal.Now.plainDateISO%";
const NOW_PLAIN_TIME: &str = "%Temporal.Now.plainTimeISO%";
const NOW_PLAIN_DATE_TIME: &str = "%Temporal.Now.plainDateTimeISO%";
const NOW_ZONED_DATE_TIME: &str = "%Temporal.Now.zonedDateTimeISO%";

/// The record behind a Temporal object, keyed by object identity in
/// `Agent::temporal_data` (the [[InitializedTemporal*]] internal slots).
#[derive(Debug, Clone)]
pub enum TemporalRecord {
    Instant(i128),
    Duration([f64; 10]),
    ZonedDateTime(i128, JsString),
    PlainDate([i64; 3]),
    PlainTime([i64; 6]),
    PlainDateTime([i64; 9]),
    YearMonth([i64; 3]),
    MonthDay([i64; 3]),
}

impl Trace for TemporalRecord {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        if let TemporalRecord::ZonedDateTime(_, tz) = self {
            tz.trace(visit);
        }
    }
}

pub fn placeholder(name: &'static str) -> NativeFn {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

/// Define an own data property with the standard builtin attributes.
pub fn define_data(obj: &Handle<JsObject>, name: &str, value: Value) -> Result<bool, JsError> {
    obj.define_property(
        &JsString::from_utf8(name),
        &PropertyDescriptor {
            value: Some(value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )
}

/// Define the shared constructor boilerplate: the `prototype` property, the
/// `constructor` back-reference, `@@toStringTag`, and the constructor on its
/// `parent` object.
pub fn install_constructor(
    realm: &Handle<Realm>,
    parent: &Handle<JsObject>,
    name: &'static str,
    intrinsic: &str,
    proto_intrinsic: &str,
    length: u64,
    tag: &str,
) -> Result<(Handle<Function>, Handle<JsObject>), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let proto = JsObject::ordinary_object_create(object_proto);
    let proto_value = Value::Object(proto);

    let ctor = Function::create_builtin(
        Some(JsString::from_utf8(name)),
        length,
        placeholder(name),
        Some(Box::new(placeholder(name))),
        None,
    )?;
    let ctor_value = Value::Function(ctor);

    realm.intrinsics.define(intrinsic, ctor_value);
    realm
        .intrinsics
        .define(proto_intrinsic, proto_value);

    ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(proto_value),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // `[@@toStringTag]` is a non-writable data property (spec 7.3.2).
    proto.define_property_key(
        &crux::property::PropertyKey::Symbol(
            crux::symbol::well_known("toStringTag")
        ),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8(tag)))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    parent.define_property(
        &JsString::from_utf8(name),
        &PropertyDescriptor {
            value: Some(ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok((ctor, proto))
}

/// Create a new Temporal instance with the given prototype intrinsic, using
/// `newTarget`'s prototype when subclassing (GetPrototypeFromConstructor).
pub fn create_temporal_object(
    agent: &mut Agent,
    new_target: &Value,
    proto_intrinsic: &str,
    record: TemporalRecord,
) -> Result<Value, JsError> {
    let proto = if matches!(new_target.kind(), ValueKind::Undefined) {
        None
    } else {
        let proto = crate::context::get_property(
            agent,
            new_target,
            &JsString::from_utf8("prototype"),
            *new_target,
        )?;
        as_object(&proto)
    };
    let object = match proto {
        Some(obj) => JsObject::ordinary_object_create(Some(obj)),
        None => {
            let realm = agent.current_realm()?;
            let fallback = realm
                .intrinsics
                .get(proto_intrinsic)
                .and_then(|value| as_object(&value))
                .ok_or_else(|| {
                    JsError::new(ErrorKind::TypeError, format!("{proto_intrinsic} missing"))
                })?;
            JsObject::ordinary_object_create(Some(fallback))
        }
    };
    agent.temporal_data.insert(object.id(), record);
    Ok(Value::Object(object))
}

/// Whether the calendar identifier is in the supported Temporal set (the
/// era-monthcode available-calendars list): the from/islamic.js and
/// future-calendar.js fixtures pin the RangeError for "islamic",
/// "islamic-rgsa", and the not-yet-supported calendars.
pub fn calendar_id_supported(calendar: &str) -> bool {
    matches!(
        calendar,
        "buddhist"
            | "chinese"
            | "coptic"
            | "dangi"
            | "ethioaa"
            | "ethiopic"
            | "gregory"
            | "hebrew"
            | "indian"
            | "islamic-civil"
            | "islamic-tbla"
            | "islamic-umalqura"
            | "iso8601"
            | "japanese"
            | "persian"
            | "roc"
    )
}

/// CanonicalizeCalendar (ECMA-402 §6.9.2): lowercase the identifier, apply
/// the two alias mappings (islamicc → islamic-civil, ethiopic-amete-alem →
/// ethioaa), validate the well-formed type-identifier form (each subtag 3-8
/// ASCII alphanumerics, `-`-separated), and require the supported set (the
/// era-monthcode available-calendars list). `None` when malformed or
/// unsupported.
pub fn canonicalize_calendar_id(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let canonical = match lower.as_str() {
        "islamicc" => "islamic-civil".to_string(),
        "ethiopic-amete-alem" => "ethioaa".to_string(),
        other => other.to_string(),
    };
    if canonical.is_empty() {
        return None;
    }
    if canonical.split('-').all(|subtag| {
        (3..=8).contains(&subtag.len()) && subtag.bytes().all(|b| b.is_ascii_alphanumeric())
    }) && calendar_id_supported(&canonical)
    {
        Some(canonical)
    } else {
        None
    }
}

/// Record the [[Calendar]] internal slot of a Temporal instance (default
/// "iso8601" when `calendar` is `None`); overwrites any existing value.
pub fn set_temporal_calendar(agent: &mut Agent, value: &Value, calendar: Option<&str>) {
    if let ValueKind::Object(obj) = value.kind() {
        match calendar {
            Some(calendar) => {
                agent
                    .temporal_calendars
                    .insert(obj.id(), JsString::from_utf8(calendar));
            }
            None => {
                agent
                    .temporal_calendars
                    .insert(obj.id(), JsString::from_utf8("iso8601"));
            }
        }
    }
}

/// The [[Calendar]] of a Temporal instance (default "iso8601").
pub fn temporal_calendar_id(agent: &Agent, this: &Value) -> JsString {
    if let ValueKind::Object(obj) = this.kind()
        && let Some(calendar) = agent.temporal_calendars.get(&obj.id())
    {
        return calendar.clone();
    }
    JsString::from_utf8("iso8601")
}

/// RequireInternalSlot: the receiver must be a Temporal object of `kind`.
pub fn require_record(
    agent: &Agent,
    this: &Value,
    kind: RecordKind,
) -> Result<TemporalRecord, JsError> {
    let ValueKind::Object(obj) = this.kind() else {
        return Err(temporal_type_error(kind));
    };
    let Some(record) = agent.temporal_data.get(&obj.id()) else {
        return Err(temporal_type_error(kind));
    };
    if record_kind(record) != kind {
        return Err(temporal_type_error(kind));
    }
    Ok(record.clone())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    Instant,
    Duration,
    ZonedDateTime,
    PlainDate,
    PlainTime,
    PlainDateTime,
    YearMonth,
    MonthDay,
}

fn record_kind(record: &TemporalRecord) -> RecordKind {
    match record {
        TemporalRecord::Instant(_) => RecordKind::Instant,
        TemporalRecord::Duration(_) => RecordKind::Duration,
        TemporalRecord::ZonedDateTime(..) => RecordKind::ZonedDateTime,
        TemporalRecord::PlainDate(_) => RecordKind::PlainDate,
        TemporalRecord::PlainTime(_) => RecordKind::PlainTime,
        TemporalRecord::PlainDateTime(_) => RecordKind::PlainDateTime,
        TemporalRecord::YearMonth(_) => RecordKind::YearMonth,
        TemporalRecord::MonthDay(_) => RecordKind::MonthDay,
    }
}

fn temporal_type_error(kind: RecordKind) -> JsError {
    JsError::new(
        ErrorKind::TypeError,
        format!(
            "received a value that is not a Temporal.{}",
            kind_name(kind)
        ),
    )
}

pub fn kind_name(kind: RecordKind) -> &'static str {
    match kind {
        RecordKind::Instant => "Instant",
        RecordKind::Duration => "Duration",
        RecordKind::ZonedDateTime => "ZonedDateTime",
        RecordKind::PlainDate => "PlainDate",
        RecordKind::PlainTime => "PlainTime",
        RecordKind::PlainDateTime => "PlainDateTime",
        RecordKind::YearMonth => "PlainYearMonth",
        RecordKind::MonthDay => "PlainMonthDay",
    }
}

// ---------------------------------------------------------------------------
// Options (spec 14.5.2)
// ---------------------------------------------------------------------------

/// GetOptionsObject (spec 14.5.2.1 in ECMA-262): `undefined` becomes a fresh
/// ordinary object; objects pass through; every other value is a TypeError
/// (never a ToObject box).
pub fn get_options_object(options: &Value) -> Result<Value, JsError> {
    match options.kind() {
        ValueKind::Undefined => Ok(Value::Object(JsObject::ordinary_object_create(None))),
        ValueKind::Object(_) | ValueKind::Function(_) => Ok(*options),
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "options must be an object".into(),
        )),
    }
}

/// GetOption (spec 14.5.2.1): reads `key`, validates against `values`.
pub fn get_option(
    agent: &mut Agent,
    options: &Value,
    key: &str,
    values: &[&str],
    default: Option<&str>,
) -> Result<Option<String>, JsError> {
    let value =
        crate::context::get_property(agent, options, &JsString::from_utf8(key), *options)?;
    if matches!(value.kind(), ValueKind::Undefined) {
        return Ok(default.map(str::to_string));
    }
    let text = crate::context::to_string(agent, &value)?;
    let text = text.to_string_lossy();
    if !values.is_empty() && !values.contains(&text.as_str()) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            format!("{text} is not a permitted value for {key}"),
        ));
    }
    Ok(Some(text))
}

/// GetRoundingModeOption (spec 14.5.2.2).
pub fn get_rounding_mode(
    agent: &mut Agent,
    options: &Value,
    fallback: RoundingMode,
) -> Result<RoundingMode, JsError> {
    let fallback_str = match fallback {
        RoundingMode::Ceil => "ceil",
        RoundingMode::Floor => "floor",
        RoundingMode::Expand => "expand",
        RoundingMode::Trunc => "trunc",
        RoundingMode::HalfCeil => "halfCeil",
        RoundingMode::HalfFloor => "halfFloor",
        RoundingMode::HalfExpand => "halfExpand",
        RoundingMode::HalfTrunc => "halfTrunc",
        RoundingMode::HalfEven => "halfEven",
    };
    let value = get_option(
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
        Some(fallback_str),
    )?;
    Ok(RoundingMode::parse(&value.unwrap()).unwrap())
}

/// GetTemporalRoundingIncrementOption (spec 14.5.2.3).
pub fn get_rounding_increment(agent: &mut Agent, options: &Value) -> Result<i64, JsError> {
    let value = crate::context::get_property(
        agent,
        options,
        &JsString::from_utf8("roundingIncrement"),
        *options,
    )?;
    if matches!(value.kind(), ValueKind::Undefined) {
        return Ok(1);
    }
    let integer = to_integer_with_truncation(agent, &value)?;
    if !(1..=1_000_000_000).contains(&integer) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "roundingIncrement must be between 1 and 10^9".into(),
        ));
    }
    Ok(integer)
}

/// GetTemporalOverflowOption (spec 13.18): "constrain" (default) or "reject".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overflow {
    Constrain,
    Reject,
}

pub fn get_temporal_overflow_option(
    agent: &mut Agent,
    options: &Value,
) -> Result<Overflow, JsError> {
    let value = get_option(
        agent,
        options,
        "overflow",
        &["constrain", "reject"],
        Some("constrain"),
    )?;
    Ok(if value.as_deref() == Some("reject") {
        Overflow::Reject
    } else {
        Overflow::Constrain
    })
}

/// spec 13.18 GetTemporalDisambiguationOption: "compatible" (default),
/// "earlier", "later", or "reject".
pub fn get_temporal_disambiguation_option(
    agent: &mut Agent,
    options: &Value,
) -> Result<String, JsError> {
    let value = get_option(
        agent,
        options,
        "disambiguation",
        &["compatible", "earlier", "later", "reject"],
        Some("compatible"),
    )?;
    Ok(value.unwrap_or_else(|| "compatible".to_string()))
}

/// spec 13.19 GetTemporalOffsetOption: prefer/use/ignore/reject with the
/// given fallback.
pub fn get_temporal_offset_option(
    agent: &mut Agent,
    options: &Value,
    fallback: &str,
) -> Result<String, JsError> {
    let value = get_option(
        agent,
        options,
        "offset",
        &["prefer", "use", "ignore", "reject"],
        Some(fallback),
    )?;
    Ok(value.unwrap_or_else(|| fallback.to_string()))
}

/// The unit groups of ValidateTemporalUnitValue (spec 13.21).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitGroup {
    Date,
    Time,
    DateTime,
}

/// ValidateTemporalUnitValue (spec 13.21) at the call sites, after all options
/// are read (the read-before-validate order of the fixtures).
pub fn validate_unit_group(unit: Unit, group: UnitGroup) -> Result<(), JsError> {
    let allowed = match group {
        UnitGroup::Date => matches!(unit, Unit::Year | Unit::Month | Unit::Week | Unit::Day),
        UnitGroup::Time => matches!(
            unit,
            Unit::Hour
                | Unit::Minute
                | Unit::Second
                | Unit::Millisecond
                | Unit::Microsecond
                | Unit::Nanosecond
        ),
        UnitGroup::DateTime => true,
    };
    if allowed {
        return Ok(());
    }
    let group = match group {
        UnitGroup::Date => "date",
        UnitGroup::Time => "time",
        UnitGroup::DateTime => "datetime",
    };
    Err(JsError::new(
        ErrorKind::RangeError,
        format!("{} is not allowed as a {group} unit", unit_to_string(unit)),
    ))
}

/// GetTemporalFractionalSecondDigitsOption (spec 13.15).
pub fn get_fractional_second_digits(
    agent: &mut Agent,
    options: &Value,
) -> Result<FracPrecision, JsError> {
    let value = crate::context::get_property(
        agent,
        options,
        &JsString::from_utf8("fractionalSecondDigits"),
        *options,
    )?;
    if matches!(value.kind(), ValueKind::Undefined) {
        return Ok(FracPrecision::Auto);
    }
    if let ValueKind::Number(n) = value.kind() {
        if n.is_nan() || n.is_infinite() {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "fractionalSecondDigits must be between 0 and 9".into(),
            ));
        }
        let digits = n.floor() as i64;
        if !(0..=9).contains(&digits) {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "fractionalSecondDigits must be between 0 and 9".into(),
            ));
        }
        return Ok(FracPrecision::Digits(digits as u8));
    }
    let text = crate::context::to_string(agent, &value)?;
    if text.to_string_lossy() != "auto" {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "fractionalSecondDigits must be \"auto\" or a number".into(),
        ));
    }
    Ok(FracPrecision::Auto)
}

/// The result of GetTemporalUnitValuedOption (spec 13.17).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitOption {
    Unset,
    Auto,
    Unit(Unit),
}

/// GetTemporalUnitValuedOption (spec 13.17): `required`/`unset`/`auto`
/// defaults; accepts singular and plural names. The unit group is validated
/// separately by the callers (ValidateTemporalUnitValue) so that all options
/// are read before any validation (spec 13.43's read-order fixtures).
pub fn get_temporal_unit(
    agent: &mut Agent,
    options: &Value,
    key: &str,
    default: Option<UnitOption>,
) -> Result<UnitOption, JsError> {
    let allowed: &[&str] = &[
        "year",
        "years",
        "month",
        "months",
        "week",
        "weeks",
        "day",
        "days",
        "hour",
        "hours",
        "minute",
        "minutes",
        "second",
        "seconds",
        "millisecond",
        "milliseconds",
        "microsecond",
        "microseconds",
        "nanosecond",
        "nanoseconds",
        "auto",
    ];
    let default_str = default.and_then(|d| match d {
        UnitOption::Unset => None,
        UnitOption::Auto => Some("auto".to_string()),
        UnitOption::Unit(u) => Some(unit_to_string(u).to_string()),
    });
    let value = get_option(agent, options, key, allowed, default_str.as_deref())?;
    let Some(value) = value else {
        return Ok(UnitOption::Unset);
    };
    if value == "auto" {
        return Ok(UnitOption::Auto);
    }
    Ok(UnitOption::Unit(Unit::from_string(&value).unwrap()))
}

pub fn unit_to_string(unit: Unit) -> &'static str {
    match unit {
        Unit::Year => "year",
        Unit::Month => "month",
        Unit::Week => "week",
        Unit::Day => "day",
        Unit::Hour => "hour",
        Unit::Minute => "minute",
        Unit::Second => "second",
        Unit::Millisecond => "millisecond",
        Unit::Microsecond => "microsecond",
        Unit::Nanosecond => "nanosecond",
    }
}

/// spec 13.16 ToSecondsStringPrecisionRecord.
pub fn to_seconds_string_precision(
    smallest_unit: Option<Unit>,
    digits: FracPrecision,
) -> (FracPrecision, Unit, i64) {
    match smallest_unit {
        Some(Unit::Minute) => (FracPrecision::Minute, Unit::Minute, 1),
        Some(Unit::Second) => (FracPrecision::Digits(0), Unit::Second, 1),
        Some(Unit::Millisecond) => (FracPrecision::Digits(3), Unit::Millisecond, 1),
        Some(Unit::Microsecond) => (FracPrecision::Digits(6), Unit::Microsecond, 1),
        Some(Unit::Nanosecond) => (FracPrecision::Digits(9), Unit::Nanosecond, 1),
        _ => match digits {
            FracPrecision::Auto => (FracPrecision::Auto, Unit::Nanosecond, 1),
            FracPrecision::Digits(0) => (FracPrecision::Digits(0), Unit::Second, 1),
            FracPrecision::Digits(n) if (1..=3).contains(&n) => (
                FracPrecision::Digits(n),
                Unit::Millisecond,
                10i64.pow(3 - n as u32),
            ),
            FracPrecision::Digits(n) if (4..=6).contains(&n) => (
                FracPrecision::Digits(n),
                Unit::Microsecond,
                10i64.pow(6 - n as u32),
            ),
            FracPrecision::Digits(n) => (
                FracPrecision::Digits(n),
                Unit::Nanosecond,
                10i64.pow(9 - n as u32),
            ),
            FracPrecision::Minute => unreachable!(),
        },
    }
}

// ---------------------------------------------------------------------------
// Conversions (spec 13.39-13.40, 14.5.1.1)
// ---------------------------------------------------------------------------

/// ToIntegerWithTruncation (spec 13.40).
pub fn to_integer_with_truncation(agent: &mut Agent, value: &Value) -> Result<i64, JsError> {
    let number = crate::context::to_number(agent, value)?;
    if number.is_nan() || number.is_infinite() {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "value must be a finite number".into(),
        ));
    }
    Ok(number.trunc() as i64)
}

/// ToIntegerIfIntegral (spec 14.5.1.1).
pub fn to_integer_if_integral(agent: &mut Agent, value: &Value) -> Result<f64, JsError> {
    let number = crate::context::to_number(agent, value)?;
    if !number.is_finite() || number.fract() != 0.0 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "value must be an integral Number".into(),
        ));
    }
    Ok(number)
}

/// ToPositiveIntegerWithTruncation (spec 13.39).
pub fn to_positive_integer_with_truncation(
    agent: &mut Agent,
    value: &Value,
) -> Result<i64, JsError> {
    let integer = to_integer_with_truncation(agent, value)?;
    if integer <= 0 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "value must be a positive integer".into(),
        ));
    }
    Ok(integer)
}

// ---------------------------------------------------------------------------
// Duration records and math (spec 7.5)
// ---------------------------------------------------------------------------

/// The internal form of a duration: the date part plus the time part as total
/// nanoseconds (spec 7.5.3).
#[derive(Debug, Clone, Copy)]
pub struct InternalDuration {
    pub date: [f64; 4],
    pub time: i128,
}

/// spec 7.5.21 TimeDurationFromComponents.
pub fn time_duration_from_components(fields: &[f64; 10]) -> i128 {
    let [_, _, _, _, h, m, s, ms, us, ns] = *fields;
    ((((h as i128 * 60 + m as i128) * 60 + s as i128) * 1000 + ms as i128) * 1000 + us as i128)
        * 1000
        + ns as i128
}

/// spec 7.5.16 IsValidDuration.
pub fn is_valid_duration(fields: &[f64; 10]) -> bool {
    let mut sign = 0i64;
    for v in fields {
        if !v.is_finite() {
            return false;
        }
        if *v < 0.0 {
            if sign > 0 {
                return false;
            }
            sign = -1;
        } else if *v > 0.0 {
            if sign < 0 {
                return false;
            }
            sign = 1;
        }
    }
    if fields[0].abs() >= 2f64.powi(32)
        || fields[1].abs() >= 2f64.powi(32)
        || fields[2].abs() >= 2f64.powi(32)
    {
        return false;
    }
    // Normalize days..nanoseconds to total nanoseconds. The fields are
    // float64-representable but may exceed any 128-bit range; saturating
    // arithmetic still decides the threshold correctly (anything beyond
    // u128 is certainly >= 2^53 * 10^9).
    let mut normalized = 0u128;
    for (field, unit) in [
        (3usize, iso::NS_PER_DAY),
        (4, iso::NS_PER_HOUR),
        (5, iso::NS_PER_MINUTE),
        (6, iso::NS_PER_SECOND),
        (7, 1_000_000i128),
        (8, 1_000),
        (9, 1),
    ] {
        let value = fields[field];
        if value != 0.0 {
            let abs = value.abs() as i128;
            normalized = normalized.saturating_add((abs as u128).saturating_mul(unit as u128));
        }
    }
    // spec 7.5.16: invalid iff abs(normalizedNanoseconds) >= 10^9 × 2^53.
    normalized <= iso::MAX_TIME_DURATION as u128
}

/// spec 7.5.13 DurationSign.
pub fn duration_sign(fields: &[f64; 10]) -> i64 {
    for v in fields {
        if *v < 0.0 {
            return -1;
        }
        if *v > 0.0 {
            return 1;
        }
    }
    0
}

/// spec 7.5.17 DefaultTemporalLargestUnit.
pub fn default_temporal_largest_unit(fields: &[f64; 10]) -> Unit {
    const UNITS: [Unit; 10] = [
        Unit::Year,
        Unit::Month,
        Unit::Week,
        Unit::Day,
        Unit::Hour,
        Unit::Minute,
        Unit::Second,
        Unit::Millisecond,
        Unit::Microsecond,
        Unit::Nanosecond,
    ];
    for (field, unit) in fields.iter().zip(UNITS.iter()) {
        if *field != 0.0 {
            return *unit;
        }
    }
    Unit::Nanosecond
}

/// spec 7.5.8 TemporalDurationFromInternal.
pub fn temporal_duration_from_internal(
    date: [f64; 4],
    time: i128,
    largest_unit: Unit,
) -> Result<[f64; 10], JsError> {
    let sign = time.signum();
    let mut ns = time.abs();
    let mut days = 0i128;
    let mut hours = 0i128;
    let mut minutes = 0i128;
    let mut seconds = 0i128;
    let mut ms = 0i128;
    let mut us = 0i128;
    if largest_unit.category() == Category::Date {
        us = ns / 1_000;
        ns %= 1_000;
        ms = us / 1_000;
        us %= 1_000;
        seconds = ms / 1_000;
        ms %= 1_000;
        minutes = seconds / 60;
        seconds %= 60;
        hours = minutes / 60;
        minutes %= 60;
        days = hours / 24;
        hours %= 24;
    } else if largest_unit == Unit::Hour {
        us = ns / 1_000;
        ns %= 1_000;
        ms = us / 1_000;
        us %= 1_000;
        seconds = ms / 1_000;
        ms %= 1_000;
        minutes = seconds / 60;
        seconds %= 60;
        hours = minutes / 60;
        minutes %= 60;
    } else if largest_unit == Unit::Minute {
        us = ns / 1_000;
        ns %= 1_000;
        ms = us / 1_000;
        us %= 1_000;
        seconds = ms / 1_000;
        ms %= 1_000;
        minutes = seconds / 60;
        seconds %= 60;
    } else if largest_unit == Unit::Second {
        us = ns / 1_000;
        ns %= 1_000;
        ms = us / 1_000;
        us %= 1_000;
        seconds = ms / 1_000;
        ms %= 1_000;
    } else if largest_unit == Unit::Millisecond {
        us = ns / 1_000;
        ns %= 1_000;
        ms = us / 1_000;
        us %= 1_000;
    } else if largest_unit == Unit::Microsecond {
        us = ns / 1_000;
        ns %= 1_000;
    }
    let s = sign as f64;
    // spec 7.5.8 ends with CreateTemporalDuration: the decomposed result must
    // still be a valid duration (rounding can push the total past the max).
    create_duration_record(&[
        date[0],
        date[1],
        date[2],
        date[3] + days as f64 * s,
        hours as f64 * s,
        minutes as f64 * s,
        seconds as f64 * s,
        ms as f64 * s,
        us as f64 * s,
        ns as f64 * s,
    ])
}

/// spec 7.5.30 RoundTimeDuration.
pub fn round_time_duration(
    time: i128,
    increment: i64,
    unit: Unit,
    mode: RoundingMode,
) -> Result<i128, JsError> {
    let divisor = unit.length_ns().unwrap();
    let rounded = iso::round_number_to_increment(time, divisor * increment as i128, mode);
    if rounded.abs() > iso::MAX_TIME_DURATION {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "result is out of range".into(),
        ));
    }
    Ok(rounded)
}

/// spec 7.5.31 TotalTimeDuration.
///
/// The division must round the exact mathematical quotient once (spec note:
/// implementing it as `𝔽(time) / 𝔽(divisor)` double-rounds when the time is
/// not a safe integer). `divide_rounded` extracts 54 quotient bits with a
/// sticky tail and rounds once, which matches the fixtures' decimal-string
/// expectations exactly.
pub fn total_time_duration(time: i128, unit: Unit) -> f64 {
    let divisor = unit.length_ns().unwrap();
    if time == 0 {
        return 0.0;
    }
    let negative = time < 0;
    let magnitude = divide_rounded(time.unsigned_abs(), divisor as u128);
    if negative { -magnitude } else { magnitude }
}

/// Correctly rounded (nearest-even) `a / b` for `a < 2^84` and `b < 2^53`
/// (the time-duration range and unit lengths). The result is always in the
/// normal f64 range (≥ 2^-54).
fn divide_rounded(a: u128, b: u128) -> f64 {
    let la = 128 - a.leading_zeros() as i32;
    let lb = 128 - b.leading_zeros() as i32;
    // e = MSB position of a/b (0-based): 2^e <= a/b < 2^(e+1). The
    // bit-length difference can be one too high (a,b leading mantissa bits
    // differ), and the check must work for negative e too.
    let cand = la - lb;
    let e = if cand >= 0 {
        if (b << (cand as u32)) > a {
            cand - 1
        } else {
            cand
        }
    } else if (a << ((-cand) as u32)) < b {
        cand - 1
    } else {
        cand
    };
    // 54-bit significand (53 mantissa bits + a guard bit):
    // m54 = floor(a/b × 2^(53 - e)); the remainder is the sticky tail.
    let shift = 53 - e;
    let (m54, r) = if shift >= 0 {
        let num = a << shift;
        (num / b, num % b)
    } else {
        let den = b << (-shift);
        (a / den, a % den)
    };
    let mant = m54 >> 1;
    // Round half-even on the guard bit with the remainder as sticky.
    let round_up = (m54 & 1) == 1 && (r != 0 || (mant & 1) == 1);
    let mant = mant + round_up as u128;
    mant as f64 * 2f64.powi(e - 52)
}

/// spec 7.5.40 TemporalDurationToString.
pub fn temporal_duration_to_string(fields: &[f64; 10], precision: FracPrecision) -> String {
    let sign = duration_sign(fields);
    let mut date_part = String::new();
    for (value, suffix) in [
        (fields[0], "Y"),
        (fields[1], "M"),
        (fields[2], "W"),
        (fields[3], "D"),
    ] {
        if value != 0.0 {
            date_part.push_str(&format!("{}{}", value.abs() as i64, suffix));
        }
    }
    let mut time_part = String::new();
    if fields[4] != 0.0 {
        time_part.push_str(&format!("{}{}", fields[4].abs() as i64, "H"));
    }
    if fields[5] != 0.0 {
        time_part.push_str(&format!("{}{}", fields[5].abs() as i64, "M"));
    }
    let zero_minutes_and_higher = matches!(
        default_temporal_largest_unit(fields),
        Unit::Second | Unit::Millisecond | Unit::Microsecond | Unit::Nanosecond
    );
    let seconds_duration = time_duration_from_components(&[
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, fields[6], fields[7], fields[8], fields[9],
    ]);
    if seconds_duration != 0 || zero_minutes_and_higher || precision != FracPrecision::Auto {
        let seconds = (seconds_duration.abs() / iso::NS_PER_SECOND) as i64;
        let sub = (seconds_duration.abs() % iso::NS_PER_SECOND) as i64;
        time_part.push_str(&format!(
            "{}{}S",
            seconds,
            iso::format_fractional_seconds(sub, precision)
        ));
    }
    let sign_part = if sign < 0 { "-" } else { "" };
    let mut result = format!("{sign_part}P{date_part}");
    if !time_part.is_empty() {
        result.push('T');
        result.push_str(&time_part);
    }
    result
}

/// spec 7.5.5 ToInternalDurationRecord.
pub fn to_internal_duration_record(fields: &[f64; 10]) -> InternalDuration {
    InternalDuration {
        date: [fields[0], fields[1], fields[2], fields[3]],
        time: time_duration_from_components(fields),
    }
}

/// spec 7.5.6 ToInternalDurationRecordWith24HourDays.
pub fn to_internal_duration_record_with_24_hour_days(
    fields: &[f64; 10],
) -> Result<InternalDuration, JsError> {
    let mut time = time_duration_from_components(fields);
    time = add_24_hour_days_to_time_duration(time, fields[3] as i128)?;
    Ok(InternalDuration {
        date: [fields[0], fields[1], fields[2], 0.0],
        time,
    })
}

/// spec 7.5.23 Add24HourDaysToTimeDuration.
pub fn add_24_hour_days_to_time_duration(d: i128, days: i128) -> Result<i128, JsError> {
    let result = d + days * iso::NS_PER_DAY;
    if result.abs() > iso::MAX_TIME_DURATION {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "duration is out of range".into(),
        ));
    }
    Ok(result)
}

/// spec 7.5.22 AddTimeDuration.
pub fn add_time_duration(a: i128, b: i128) -> Result<i128, JsError> {
    let result = a + b;
    if result.abs() > iso::MAX_TIME_DURATION {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "duration is out of range".into(),
        ));
    }
    Ok(result)
}

/// spec 7.5.41 AddDurations.
pub fn add_durations(
    agent: &mut Agent,
    duration: &[f64; 10],
    other: &Value,
    subtract: bool,
) -> Result<Value, JsError> {
    let mut other = to_temporal_duration(agent, other)?;
    if subtract {
        for f in other.iter_mut() {
            *f = -*f;
        }
    }
    let largest_unit = iso::larger_of_two_units(
        default_temporal_largest_unit(duration),
        default_temporal_largest_unit(&other),
    );
    // spec 7.5.41: only calendar units require relativeTo — days are folded
    // into the 24-hour time duration.
    if iso::is_calendar_unit(largest_unit) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "cannot add durations with calendar units".into(),
        ));
    }
    let d1 = to_internal_duration_record_with_24_hour_days(duration)?;
    let d2 = to_internal_duration_record_with_24_hour_days(&other)?;
    let time = add_time_duration(d1.time, d2.time)?;
    let result = temporal_duration_from_internal([0.0, 0.0, 0.0, 0.0], time, largest_unit)?;
    create_temporal_duration(agent, &result, &Value::Undefined)
}

/// spec 7.5.12 ToTemporalDuration.
pub fn to_temporal_duration(agent: &mut Agent, item: &Value) -> Result<[f64; 10], JsError> {
    if let ValueKind::Object(obj) = item.kind()
        && let Some(TemporalRecord::Duration(fields)) = agent.temporal_data.get(&obj.id())
    {
        return Ok(*fields);
    }
    if !matches!(item.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
        if !matches!(item.kind(), ValueKind::String(_)) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "value must be a Duration, string, or object".into(),
            ));
        }
        let text = crate::context::to_string(agent, item)?;
        return parse_duration_text(&text);
    }
    // Property bag: fields default to 0, read in alphabetical order.
    let mut fields = [0f64; 10];
    let partial = read_duration_fields(agent, item)?;
    for (i, value) in partial.iter().enumerate() {
        if let Some(v) = value {
            fields[i] = *v;
        }
    }
    create_duration_record(&fields)
}

/// Parse a duration string into its components via ToTemporalDuration's
/// string branch.
pub(crate) fn parse_duration_text(text: &JsString) -> Result<[f64; 10], JsError> {
    let fields = iso::parse_duration_string(text.as_slice())
        .map_err(|_| JsError::new(ErrorKind::RangeError, "invalid duration string".into()))?;
    let fields: [f64; 10] = fields.map(|v| v as f64);
    create_duration_record(&fields)
}

/// spec 7.5.18 ToTemporalPartialDurationRecord: reads fields alphabetically,
/// throwing when none are present.
fn read_duration_fields(agent: &mut Agent, item: &Value) -> Result<[Option<f64>; 10], JsError> {
    let keys = [
        ("days", 3),
        ("hours", 4),
        ("microseconds", 8),
        ("milliseconds", 7),
        ("minutes", 5),
        ("months", 1),
        ("nanoseconds", 9),
        ("seconds", 6),
        ("weeks", 2),
        ("years", 0),
    ];
    let mut result = [None; 10];
    let mut any = false;
    for (key, idx) in keys {
        let value =
            crate::context::get_property(agent, item, &JsString::from_utf8(key), *item)?;
        if !matches!(value.kind(), ValueKind::Undefined) {
            any = true;
            result[idx] = Some(to_integer_if_integral(agent, &value)?);
        }
    }
    if !any {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "at least one duration field is required".into(),
        ));
    }
    Ok(result)
}

/// CreateTemporalDuration (spec 7.5.19) without creating an object: validates
/// and returns the record.
pub fn create_duration_record(fields: &[f64; 10]) -> Result<[f64; 10], JsError> {
    if !is_valid_duration(fields) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "invalid duration".into(),
        ));
    }
    // Zero fields are stored as +0 (spec 7.5.19 stores ℝ(𝔽(v)); SameValue
    // distinguishes -0 from +0).
    Ok(fields.map(|v| if v == 0.0 { 0.0 } else { v }))
}

/// CreateTemporalDuration (spec 7.5.19): object form.
pub fn create_temporal_duration(
    agent: &mut Agent,
    fields: &[f64; 10],
    new_target: &Value,
) -> Result<Value, JsError> {
    let fields = create_duration_record(fields)?;
    create_temporal_object(
        agent,
        new_target,
        "%Temporal.Duration.prototype%",
        TemporalRecord::Duration(fields),
    )
}

/// spec 7.5.20 CreateNegatedTemporalDuration.
pub fn negate_duration(fields: &[f64; 10]) -> [f64; 10] {
    let mut out = *fields;
    for f in out.iter_mut() {
        // Keep +0 for zero fields (SameValue distinguishes -0 from +0).
        *f = if *f == 0.0 { 0.0 } else { -*f };
    }
    out
}

/// A relativeTo that resolved to a plain date (the ISO calendar only).
#[derive(Debug, Clone, Copy)]
pub struct PlainRelativeTo {
    pub date: (i64, i64, i64),
}

/// spec 7.5.29 DateDurationDays.
pub fn date_duration_days(date: [f64; 4], relative: PlainRelativeTo) -> Result<i64, JsError> {
    let [years, months, weeks, days] = date;
    let years_months_weeks = [years, months, weeks, 0.0];
    if years == 0.0 && months == 0.0 && weeks == 0.0 {
        return Ok(days as i64);
    }
    let later = iso::calendar_date_add(
        relative.date.0,
        relative.date.1,
        relative.date.2,
        years_months_weeks[0] as i64,
        years_months_weeks[1] as i64,
        years_months_weeks[2] as i64,
        years_months_weeks[3] as i64,
        true,
    )
    .ok_or_else(|| JsError::new(ErrorKind::RangeError, "date out of range".into()))?;
    let d1 = iso::iso_date_to_epoch_days(relative.date.0, relative.date.1 - 1, relative.date.2);
    let d2 = iso::iso_date_to_epoch_days(later.0, later.1 - 1, later.2);
    Ok(days as i64 + (d2 - d1))
}

/// The result of GetTemporalRelativeToOption (spec 13.19). Zoned relative
/// values are (epoch ns, time zone identifier).
#[derive(Debug, Clone)]
pub enum RelativeTo {
    None,
    Plain(PlainRelativeTo),
    Zoned(i128, String),
}

/// spec 13.19 GetTemporalRelativeToOption.
pub fn get_temporal_relative_to(agent: &mut Agent, options: &Value) -> Result<RelativeTo, JsError> {
    let value = crate::context::get_property(
        agent,
        options,
        &JsString::from_utf8("relativeTo"),
        *options,
    )?;
    if matches!(value.kind(), ValueKind::Undefined) {
        return Ok(RelativeTo::None);
    }
    if let ValueKind::Object(obj) = value.kind() {
        if let Some(record) = agent.temporal_data.get(&obj.id()) {
            return match record {
                TemporalRecord::ZonedDateTime(ns, tz) => {
                    Ok(RelativeTo::Zoned(*ns, tz.to_string_lossy()))
                }
                TemporalRecord::PlainDate([y, m, d]) => {
                    Ok(RelativeTo::Plain(PlainRelativeTo { date: (*y, *m, *d) }))
                }
                TemporalRecord::PlainDateTime([y, m, d, ..]) => {
                    Ok(RelativeTo::Plain(PlainRelativeTo { date: (*y, *m, *d) }))
                }
                _ => Err(JsError::new(
                    ErrorKind::TypeError,
                    "relativeTo must not be a Temporal value of this type".into(),
                )),
            };
        }
        return relative_to_object(agent, &value);
    }
    if matches!(value.kind(), ValueKind::String(_)) {
        return relative_to_string(agent, &value);
    }
    Err(JsError::new(
        ErrorKind::TypeError,
        "relativeTo must be a string or object".into(),
    ))
}

/// The property-bag branch of GetTemporalRelativeToOption (spec 13.19).
fn relative_to_object(agent: &mut Agent, item: &Value) -> Result<RelativeTo, JsError> {
    // spec 12.3.10 ToTemporalCalendarIdentifier: a Temporal object passes its
    // own calendar; any other non-String is a TypeError, an unsupported
    // String a RangeError.
    let calendar =
        crate::context::get_property(agent, item, &JsString::from_utf8("calendar"), *item)?;
    if !matches!(calendar.kind(), ValueKind::Undefined) {
        if let ValueKind::Object(obj) = calendar.kind()
            && agent.temporal_data.contains_key(&obj.id())
        {
            // A Temporal object's own calendar (validated elsewhere).
        } else if let ValueKind::String(text) = calendar.kind() {
            if canonicalize_calendar_id(&text.to_string_lossy()).is_none() {
                return Err(JsError::new(
                    ErrorKind::RangeError,
                    "invalid calendar identifier".into(),
                ));
            }
        } else {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "calendar must be a string".into(),
            ));
        }
    }
    // The canonical calendar id (None for the iso8601 default) drives the
    // era-field resolution.
    let calendar_id = if matches!(calendar.kind(), ValueKind::Undefined) {
        None
    } else if let ValueKind::String(text) = calendar.kind() {
        canonicalize_calendar_id(&text.to_string_lossy())
    } else {
        Some(temporal_calendar_id(agent, &calendar).to_string_lossy())
    };
    // Fields read in sorted property-name order (PrepareCalendarFields).
    let mut year: Option<i64> = None;
    let mut month: Option<i64> = None;
    let mut month_code: Option<String> = None;
    let mut day: Option<i64> = None;
    let mut time = [0i64; 6];
    let mut offset: Option<String> = None;
    let mut time_zone: Option<String> = None;
    for key in [
        "day",
        "hour",
        "microsecond",
        "millisecond",
        "minute",
        "month",
        "monthCode",
        "nanosecond",
        "offset",
        "second",
        "timeZone",
        "year",
    ] {
        let value =
            crate::context::get_property(agent, item, &JsString::from_utf8(key), *item)?;
        if matches!(value.kind(), ValueKind::Undefined) {
            continue;
        }
        match key {
            "day" => day = Some(to_positive_integer_with_truncation(agent, &value)?),
            "hour" => time[0] = to_integer_with_truncation(agent, &value)?,
            "minute" => time[1] = to_integer_with_truncation(agent, &value)?,
            "second" => time[2] = to_integer_with_truncation(agent, &value)?,
            "millisecond" => time[3] = to_integer_with_truncation(agent, &value)?,
            "microsecond" => time[4] = to_integer_with_truncation(agent, &value)?,
            "nanosecond" => time[5] = to_integer_with_truncation(agent, &value)?,
            "month" => month = Some(to_positive_integer_with_truncation(agent, &value)?),
            "monthCode" => {
                month_code = Some(crate::context::to_string(agent, &value)?.to_string_lossy())
            }
            "offset" => {
                // spec 13.41 ToOffsetString: ToPrimitive with the string
                // hint; a non-string result is a TypeError, an invalid
                // string a RangeError.
                let prim = crate::context::to_primitive(
                    agent,
                    &value,
                    crux::convert::ToPrimitiveHint::String,
                )?;
                let ValueKind::String(text) = prim.kind() else {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "offset must be a string".into(),
                    ));
                };
                let text = text.to_string_lossy();
                iso::parse_date_time_utc_offset(&text).map_err(|_| {
                    JsError::new(ErrorKind::RangeError, "invalid offset string".into())
                })?;
                offset = Some(text);
            }
            "timeZone" => {
                time_zone = Some(
                    crate::builtins::temporal::instant::to_temporal_time_zone_identifier(
                        agent, &value,
                    )?,
                );
            }
            "year" => year = Some(to_integer_with_truncation(agent, &value)?),
            _ => {}
        }
    }
    // Resolve monthCode for the ISO calendar (spec 12.3.31).
    let month = resolve_iso_month(month, month_code)?;
    let year = crate::builtins::temporal::shell::read_era_fields(
        agent,
        item,
        calendar_id.as_deref(),
        year,
    )?;
    let (Some(year), Some(month), Some(day)) = (year, month, day) else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "year, month, and day are required for relativeTo".into(),
        ));
    };
    if !iso::is_valid_iso_date(year, month, day) {
        return Err(JsError::new(ErrorKind::RangeError, "invalid date".into()));
    }
    let Some(time_zone) = time_zone else {
        // spec 13.19: a plain relativeTo must be within the PlainDate limits
        // (ISODateWithinLimits, evaluated at noon).
        if !iso::iso_date_time_within_limits(year, month, day, 12, 0, 0, 0, 0, 0) {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "relativeTo is outside the representable range".into(),
            ));
        }
        return Ok(RelativeTo::Plain(PlainRelativeTo {
            date: (year, month, day),
        }));
    };
    // spec 13.19: InterpretISODateTimeOffset runs CheckISODaysRange on the
    // wall date (the epoch day count must be within 10^8).
    if iso::iso_date_to_epoch_days(year, month - 1, day).abs() > 100_000_000 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "relativeTo is outside the representable range".into(),
        ));
    }
    let offset_ns = match &offset {
        Some(text) => Some(
            iso::parse_date_time_utc_offset(text)
                .map_err(|_| JsError::new(ErrorKind::RangeError, "invalid offset".into()))?,
        ),
        None => None,
    };
    let epoch = shell::interpret_iso_date_time_offset(
        [
            year, month, day, time[0], time[1], time[2], time[3], time[4], time[5],
        ],
        &time_zone,
        offset_ns,
        false,
        "reject",
        "compatible",
        false,
    )?;
    // spec 13.19: InterpretISODateTimeOffset validates the resulting exact
    // time against the Instant range.
    if !(iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(&epoch) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "relativeTo is outside the representable range".into(),
        ));
    }
    Ok(RelativeTo::Zoned(epoch, time_zone))
}

/// Resolve an ISO month number from an optional `month` and `monthCode`
/// (the resolveNonLunisolarMonth step for iso8601): a monthCode is validated
/// against the `M01`-`M12` forms (no leap months) and checked against a
/// concurrent `month`.
pub fn resolve_iso_month(
    month: Option<i64>,
    month_code: Option<String>,
) -> Result<Option<i64>, JsError> {
    let Some(code) = month_code else {
        return Ok(month);
    };
    let parsed = if let Some(rest) = code.strip_prefix('M') {
        let leap = rest.ends_with('L');
        let digits = if leap { &rest[..rest.len() - 1] } else { rest };
        if digits.len() == 2 && digits.chars().all(|c| c.is_ascii_digit()) {
            let value: i64 = digits.parse().unwrap();
            if leap || value == 0 {
                None
            } else {
                Some(value)
            }
        } else {
            None
        }
    } else {
        None
    };
    let Some(parsed) = parsed else {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "invalid monthCode".into(),
        ));
    };
    if parsed > 12 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "invalid monthCode".into(),
        ));
    }
    match month {
        Some(m) if m != parsed => Err(JsError::new(
            ErrorKind::RangeError,
            "month and monthCode conflict".into(),
        )),
        _ => Ok(Some(parsed)),
    }
}

/// The string branch of GetTemporalRelativeToOption (spec 13.19).
fn relative_to_string(agent: &mut Agent, item: &Value) -> Result<RelativeTo, JsError> {
    let text = crate::context::to_string(agent, item)?;
    let parsed = iso::parse_iso_date_time(text.as_slice(), iso::Format::DateTimeZoned)
        .or_else(|_| iso::parse_iso_date_time(text.as_slice(), iso::Format::DateTimePlain))
        .map_err(|_| JsError::new(ErrorKind::RangeError, "invalid date-time string".into()))?;
    if let Some(calendar) = &parsed.calendar
        && canonicalize_calendar_id(calendar).is_none()
    {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "invalid calendar identifier".into(),
        ));
    }
    let date = (parsed.year, parsed.month, parsed.day);
    if parsed.tz.annotation.is_empty() {
        // spec 13.19: a plain relativeTo must be within the PlainDate limits
        // (ISODateWithinLimits, evaluated at noon).
        if !iso::iso_date_time_within_limits(date.0, date.1, date.2, 12, 0, 0, 0, 0, 0) {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "relativeTo is outside the representable range".into(),
            ));
        }
        return Ok(RelativeTo::Plain(PlainRelativeTo { date }));
    }
    let time_zone = crate::builtins::temporal::instant::to_temporal_time_zone_identifier(
        agent,
        &Value::String(Handle::new(JsString::from_utf8(&parsed.tz.annotation))),
    )?;
    // spec 13.19: InterpretISODateTimeOffset runs CheckISODaysRange on the
    // wall date (the epoch day count must be within 10^8).
    if iso::iso_date_to_epoch_days(date.0, date.1 - 1, date.2).abs() > 100_000_000 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "relativeTo is outside the representable range".into(),
        ));
    }
    if parsed.time.is_none() {
        // Start-of-day (spec 6.5.1): a date-only relativeTo resolves through
        // GetStartOfDay, not the disambiguation of 00:00.
        let epoch = shell::zoned_start_of_day_ns(agent, &time_zone, date.0, date.1, date.2)?;
        return Ok(RelativeTo::Zoned(epoch, time_zone));
    }
    let t = parsed.time.unwrap_or([0, 0, 0, 0, 0, 0]);
    let offset_ns = if parsed.tz.z {
        None
    } else if !parsed.tz.offset_string.is_empty() {
        Some(
            iso::parse_date_time_utc_offset(&parsed.tz.offset_string)
                .map_err(|_| JsError::new(ErrorKind::RangeError, "invalid offset".into()))?,
        )
    } else {
        None
    };
    let epoch = shell::interpret_iso_date_time_offset(
        [date.0, date.1, date.2, t[0], t[1], t[2], t[3], t[4], t[5]],
        &time_zone,
        offset_ns,
        parsed.tz.z,
        "reject",
        "compatible",
        !shell::offset_string_has_seconds(&parsed.tz.offset_string),
    )?;
    // spec 13.19: InterpretISODateTimeOffset validates the resulting exact
    // time against the Instant range.
    if !(iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(&epoch) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "relativeTo is outside the representable range".into(),
        ));
    }
    Ok(RelativeTo::Zoned(epoch, time_zone))
}

// ---------------------------------------------------------------------------
// The Temporal namespace + Temporal.Now
// ---------------------------------------------------------------------------

/// spec 1.3.1: the `Temporal` namespace object.
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let temporal = JsObject::ordinary_object_create(object_proto);
    let temporal_value = Value::Object(temporal);
    realm.intrinsics.define(TEMPORAL, temporal_value);

    install_now(&temporal, realm)?;
    duration::install(&temporal, realm)?;
    instant::install(&temporal, realm)?;
    shell::install(&temporal, realm)?;

    // Temporal[@@toStringTag] = "Temporal" (spec 1.1.1): a non-writable
    // data property.
    temporal.define_property_key(
        &crux::property::PropertyKey::Symbol(
            crux::symbol::well_known("toStringTag")
        ),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8("Temporal")))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("Temporal"),
        &PropertyDescriptor {
            value: Some(temporal_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// spec 2: `Temporal.Now`.
fn install_now(temporal: &Handle<JsObject>, realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let now = JsObject::ordinary_object_create(object_proto);
    let now_value = Value::Object(now);
    realm.intrinsics.define(NOW, now_value);
    for (name, intrinsic, length) in [
        ("timeZoneId", NOW_TZ, 0),
        ("instant", NOW_INSTANT, 0),
        ("plainDateISO", NOW_PLAIN_DATE, 0),
        ("plainTimeISO", NOW_PLAIN_TIME, 0),
        ("plainDateTimeISO", NOW_PLAIN_DATE_TIME, 0),
        ("zonedDateTimeISO", NOW_ZONED_DATE_TIME, 0),
    ] {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            placeholder(name),
            None,
            None,
        )?;
        realm.intrinsics.define(intrinsic, Value::Function(func));
        now.define_property(
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
    temporal.define_property(
        &JsString::from_utf8("Now"),
        &PropertyDescriptor {
            value: Some(now_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    // Temporal.Now[@@toStringTag] = "Temporal.Now" (spec 2.1.1).
    now.define_property_key(
        &crux::property::PropertyKey::Symbol(
            crux::symbol::well_known("toStringTag")
        ),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8(
                "Temporal.Now",
            )))),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// The current time as epoch nanoseconds (spec 2.3.3 SystemUTCEpochNanoseconds).
pub fn system_utc_epoch_nanoseconds() -> i128 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0);
    now.clamp(iso::NS_MIN_INSTANT, iso::NS_MAX_INSTANT)
}

fn now_dispatch(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    let ns = system_utc_epoch_nanoseconds();
    if intrinsics.get(NOW_TZ).as_ref() == Some(callee) {
        return Some(Ok(Value::String(Handle::new(JsString::from_utf8("UTC")))));
    }
    if intrinsics.get(NOW_INSTANT).as_ref() == Some(callee) {
        return Some(instant::create_instant(agent, ns, &Value::Undefined));
    }
    // Only the four time-zone-taking methods reach past this point; verify
    // the callee first so unrelated builtins never see their arguments here.
    let is_tz_method = intrinsics.get(NOW_PLAIN_DATE).as_ref() == Some(callee)
        || intrinsics.get(NOW_PLAIN_TIME).as_ref() == Some(callee)
        || intrinsics.get(NOW_PLAIN_DATE_TIME).as_ref() == Some(callee)
        || intrinsics.get(NOW_ZONED_DATE_TIME).as_ref() == Some(callee);
    if !is_tz_method {
        return None;
    }
    // SystemDateTime (spec 2.3.4): validate the time zone and return the
    // local wall-clock date-time.
    let tz_arg = args.first().cloned().unwrap_or(Value::Undefined);
    let time_zone = if matches!(tz_arg.kind(), ValueKind::Undefined) {
        "UTC".to_string()
    } else {
        match instant::to_temporal_time_zone_identifier(agent, &tz_arg) {
            Ok(tz) => tz,
            Err(e) => return Some(Err(e)),
        }
    };
    let offset = offset_ns_at(&time_zone, ns).unwrap_or(0);
    let (y, m, d, h, min, s, ms, us, n) = iso::iso_parts_from_epoch(ns);
    let (y, m, d, h, min, s, ms, us, ns_local) =
        instant::balance_iso_date_time(y, m, d, h, min, s, ms, us, (n as i128 + offset) as i64);
    if intrinsics.get(NOW_PLAIN_DATE).as_ref() == Some(callee) {
        let record = TemporalRecord::PlainDate([y, m, d]);
        return Some(create_temporal_object(
            agent,
            &Value::Undefined,
            "%Temporal.PlainDate.prototype%",
            record,
        ));
    }
    if intrinsics.get(NOW_PLAIN_TIME).as_ref() == Some(callee) {
        let record = TemporalRecord::PlainTime([h, min, s, ms, us, ns_local]);
        return Some(create_temporal_object(
            agent,
            &Value::Undefined,
            "%Temporal.PlainTime.prototype%",
            record,
        ));
    }
    if intrinsics.get(NOW_PLAIN_DATE_TIME).as_ref() == Some(callee) {
        let record = TemporalRecord::PlainDateTime([y, m, d, h, min, s, ms, us, ns_local]);
        return Some(create_temporal_object(
            agent,
            &Value::Undefined,
            "%Temporal.PlainDateTime.prototype%",
            record,
        ));
    }
    if intrinsics.get(NOW_ZONED_DATE_TIME).as_ref() == Some(callee) {
        let record = TemporalRecord::ZonedDateTime(ns, JsString::from_utf8(&time_zone));
        return Some(create_temporal_object(
            agent,
            &Value::Undefined,
            "%Temporal.ZonedDateTime.prototype%",
            record,
        ));
    }
    None
}

/// Dispatch Temporal method calls by intrinsic identity.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    if let Some(result) = now_dispatch(agent, callee, args) {
        return Some(result);
    }
    if let Some(result) = duration::dispatch_call(agent, callee, this, args) {
        return Some(result);
    }
    if let Some(result) = instant::dispatch_call(agent, callee, this, args) {
        return Some(result);
    }
    shell::dispatch_call(agent, callee, this, args)
}

/// Dispatch Temporal construction.
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    if let Some(result) = duration::dispatch_construct(agent, callee, args, new_target) {
        return Some(result);
    }
    if let Some(result) = instant::dispatch_construct(agent, callee, args, new_target) {
        return Some(result);
    }
    shell::dispatch_construct(agent, callee, args, new_target)
}

/// The offset (in nanoseconds) of a time-zone identifier: the UTC zone, a
/// numeric `±HH:MM` offset, or a named zone resolved through the generated
/// IANA tables (`None` when unsupported).
pub fn offset_time_zone_offset_ns(tz: &str) -> Option<i128> {
    if tz == "UTC" {
        return Some(0);
    }
    iso::parse_date_time_utc_offset(tz).ok()
}

/// The offset (in nanoseconds) of a named/offset zone at a specific instant
/// (GetOffsetNanosecondsFor for the generated IANA tables; numeric offset
/// zones are constant). `None` when the zone is unsupported.
pub fn offset_ns_at(tz: &str, epoch_ns: i128) -> Option<i128> {
    if tz == "UTC" {
        return Some(0);
    }
    if let Ok(offset) = iso::parse_date_time_utc_offset(tz) {
        return Some(offset);
    }
    let zone = unicode::tz::resolve_zone(tz)?;
    let (offset_secs, ..) = unicode::tz::offset_info_at(zone, epoch_ns);
    Some(offset_secs as i128 * 1_000_000_000)
}

/// The next/previous transition of a named zone at an instant
/// (GetIANATimeZoneNextTransition / PreviousTransition): the transition
/// record with its instant and the new offset, or `None` when the zone is
/// unsupported or has no such transition.
pub fn tz_transition(tz: &str, epoch_ns: i128, next: bool) -> Option<(i64, i64, bool)> {
    if tz == "UTC" {
        return None;
    }
    if iso::parse_date_time_utc_offset(tz).is_ok() {
        return None;
    }
    let zone = unicode::tz::resolve_zone(tz)?;
    let transition = if next {
        unicode::tz::next_transition(zone, epoch_ns)
    } else {
        unicode::tz::previous_transition(zone, epoch_ns)
    }?;
    Some((
        transition.at_secs,
        transition.offset_secs as i64,
        transition.dst,
    ))
}
