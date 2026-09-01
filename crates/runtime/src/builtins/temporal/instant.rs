//! `Temporal.Instant` (spec 8): the constructor, statics, and prototype.

use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::property::PropertyDescriptor;
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use crate::agent::Agent;
use crate::realm::Realm;

use super::iso::{self, FracPrecision, RoundingMode, Unit};
use super::{
    RecordKind, TemporalRecord, UnitOption, create_temporal_object, get_fractional_second_digits,
    get_options_object, get_rounding_increment, get_rounding_mode, get_temporal_unit,
    install_constructor, placeholder, require_record,
};

const INSTANT: &str = "%Temporal.Instant%";
const INSTANT_PROTO: &str = "%Temporal.Instant.prototype%";
const INSTANT_FROM: &str = "%Temporal.Instant.from%";
const INSTANT_FROM_MS: &str = "%Temporal.Instant.fromEpochMilliseconds%";
const INSTANT_FROM_NS: &str = "%Temporal.Instant.fromEpochNanoseconds%";
const INSTANT_COMPARE: &str = "%Temporal.Instant.compare%";

const P_EPOCH_MS: &str = "%Temporal.Instant.prototype.epochMilliseconds%";
const P_EPOCH_NS: &str = "%Temporal.Instant.prototype.epochNanoseconds%";
const P_ADD: &str = "%Temporal.Instant.prototype.add%";
const P_SUBTRACT: &str = "%Temporal.Instant.prototype.subtract%";
const P_UNTIL: &str = "%Temporal.Instant.prototype.until%";
const P_SINCE: &str = "%Temporal.Instant.prototype.since%";
const P_ROUND: &str = "%Temporal.Instant.prototype.round%";
const P_EQUALS: &str = "%Temporal.Instant.prototype.equals%";
const P_TO_STRING: &str = "%Temporal.Instant.prototype.toString%";
const P_TO_LOCALE: &str = "%Temporal.Instant.prototype.toLocaleString%";
const P_TO_JSON: &str = "%Temporal.Instant.prototype.toJSON%";
const P_VALUE_OF: &str = "%Temporal.Instant.prototype.valueOf%";
const P_TO_ZDT: &str = "%Temporal.Instant.prototype.toZonedDateTimeISO%";

/// Install `Temporal.Instant` (spec 8.1-8.3).
pub fn install(
    parent: &Handle<crux::object::JsObject>,
    realm: &Handle<Realm>,
) -> Result<(), JsError> {
    let (ctor, proto) = install_constructor(
        realm,
        parent,
        "Instant",
        INSTANT,
        INSTANT_PROTO,
        1,
        "Temporal.Instant",
    )?;

    for (name, intrinsic, length) in [
        ("from", INSTANT_FROM, 1),
        ("fromEpochMilliseconds", INSTANT_FROM_MS, 1),
        ("fromEpochNanoseconds", INSTANT_FROM_NS, 1),
        ("compare", INSTANT_COMPARE, 2),
    ] {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            placeholder(name),
            None,
            None,
        )?;
        realm.intrinsics.define(intrinsic, Value::Function(func));
        ctor.define_property(
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

    for (name, intrinsic) in [
        ("epochMilliseconds", P_EPOCH_MS),
        ("epochNanoseconds", P_EPOCH_NS),
    ] {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(&format!("get {name}"))),
            0,
            placeholder(name),
            None,
            None,
        )?;
        realm.intrinsics.define(intrinsic, Value::Function(func));
        proto.define_property(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: None,
                writable: None,
                get: Some(Value::Function(func)),
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
    }

    for (name, intrinsic, length) in [
        ("add", P_ADD, 1),
        ("subtract", P_SUBTRACT, 1),
        ("until", P_UNTIL, 1),
        ("since", P_SINCE, 1),
        ("round", P_ROUND, 1),
        ("equals", P_EQUALS, 1),
        ("toString", P_TO_STRING, 0),
        ("toLocaleString", P_TO_LOCALE, 0),
        ("toJSON", P_TO_JSON, 0),
        ("valueOf", P_VALUE_OF, 0),
        ("toZonedDateTimeISO", P_TO_ZDT, 1),
    ] {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            placeholder(name),
            None,
            None,
        )?;
        realm.intrinsics.define(intrinsic, Value::Function(func));
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
    Ok(())
}

pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(INSTANT).as_ref() == Some(callee) {
        return Some(Err(JsError::new(
            ErrorKind::TypeError,
            "Temporal.Instant cannot be called as a function".into(),
        )));
    }
    if intrinsics.get(INSTANT_FROM).as_ref() == Some(callee) {
        let item = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(
            to_temporal_instant(agent, &item)
                .and_then(|ns| create_instant(agent, ns, &Value::Undefined)),
        );
    }
    if intrinsics.get(INSTANT_FROM_MS).as_ref() == Some(callee) {
        let value = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(from_epoch_milliseconds(agent, &value));
    }
    if intrinsics.get(INSTANT_FROM_NS).as_ref() == Some(callee) {
        let value = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(from_epoch_nanoseconds(agent, &value));
    }
    if intrinsics.get(INSTANT_COMPARE).as_ref() == Some(callee) {
        let one = args.first().cloned().unwrap_or(Value::Undefined);
        let two = args.get(1).cloned().unwrap_or(Value::Undefined);
        return Some(to_temporal_instant(agent, &one).and_then(|a| {
            to_temporal_instant(agent, &two).map(|b| Value::Number(cmp(a, b) as f64))
        }));
    }
    if intrinsics.get(P_EPOCH_MS).as_ref() == Some(callee) {
        return Some(epoch_ms(agent, this));
    }
    if intrinsics.get(P_EPOCH_NS).as_ref() == Some(callee) {
        return Some(epoch_ns(agent, this));
    }
    if intrinsics.get(P_ADD).as_ref() == Some(callee) {
        let d = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(add_subtract(agent, this, &d, false));
    }
    if intrinsics.get(P_SUBTRACT).as_ref() == Some(callee) {
        let d = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(add_subtract(agent, this, &d, true));
    }
    if intrinsics.get(P_UNTIL).as_ref() == Some(callee) {
        return Some(difference(agent, this, args, false));
    }
    if intrinsics.get(P_SINCE).as_ref() == Some(callee) {
        return Some(difference(agent, this, args, true));
    }
    if intrinsics.get(P_ROUND).as_ref() == Some(callee) {
        let round_to = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(round(agent, this, &round_to));
    }
    if intrinsics.get(P_EQUALS).as_ref() == Some(callee) {
        let other = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(to_temporal_instant(agent, &other).and_then(|other_ns| {
            require_instant(agent, this).map(|ns| Value::Boolean(ns == other_ns))
        }));
    }
    if intrinsics.get(P_TO_STRING).as_ref() == Some(callee) {
        let options = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(to_string_impl(agent, this, &options));
    }
    if intrinsics.get(P_TO_LOCALE).as_ref() == Some(callee) {
        return Some(match require_instant(agent, this) {
            Ok(ns) => {
                let locales = args.first().cloned().unwrap_or(Value::Undefined);
                let options = args.get(1).cloned().unwrap_or(Value::Undefined);
                crate::builtins::intl::date_time_format::to_locale_string(
                    agent,
                    &locales,
                    &options,
                    ns as f64 / 1_000_000.0,
                    "any",
                    "all",
                )
                .map(|text| Value::String(Handle::new(JsString::from_utf8(&text))))
            }
            Err(error) => Err(error),
        });
    }
    if intrinsics.get(P_TO_JSON).as_ref() == Some(callee) {
        return Some(to_string_impl(agent, this, &Value::Undefined));
    }
    if intrinsics.get(P_VALUE_OF).as_ref() == Some(callee) {
        return Some(Err(JsError::new(
            ErrorKind::TypeError,
            "Temporal.Instant.prototype.valueOf throws".into(),
        )));
    }
    if intrinsics.get(P_TO_ZDT).as_ref() == Some(callee) {
        let tz = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(to_zoned_date_time_iso(agent, this, &tz));
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
    if realm.intrinsics.get(INSTANT).as_ref() == Some(callee) {
        return Some(construct(agent, args, new_target));
    }
    None
}

fn require_instant(agent: &Agent, this: &Value) -> Result<i128, JsError> {
    match require_record(agent, this, RecordKind::Instant)? {
        TemporalRecord::Instant(ns) => Ok(ns),
        _ => unreachable!(),
    }
}

/// spec 8.5.2 CreateTemporalInstant.
pub fn create_instant(agent: &mut Agent, ns: i128, new_target: &Value) -> Result<Value, JsError> {
    if !(iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(&ns) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "epoch nanoseconds out of range".into(),
        ));
    }
    create_temporal_object(
        agent,
        new_target,
        INSTANT_PROTO,
        TemporalRecord::Instant(ns),
    )
}

/// spec 8.1.1.
fn construct(agent: &mut Agent, args: &[Value], new_target: &Value) -> Result<Value, JsError> {
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    let bigint = crate::context::to_big_int(agent, &value)?;
    let ns = iso::bigint_to_epoch_ns(&bigint).ok_or_else(|| {
        JsError::new(
            ErrorKind::RangeError,
            "epoch nanoseconds out of range".into(),
        )
    })?;
    create_instant(agent, ns, new_target)
}

/// spec 8.2.3 fromEpochMilliseconds.
fn from_epoch_milliseconds(agent: &mut Agent, value: &Value) -> Result<Value, JsError> {
    let number = crate::context::to_number(agent, value)?;
    if !number.is_finite() || number.fract() != 0.0 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "epoch milliseconds must be an integral Number".into(),
        ));
    }
    let bigint = crux::BigInt::from_f64_exact(number)
        .ok_or_else(|| JsError::new(ErrorKind::RangeError, "invalid epoch milliseconds".into()))?;
    let ns = iso::bigint_to_epoch_ns(&bigint)
        .and_then(|ms| ms.checked_mul(1_000_000))
        .filter(|ns| (iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(ns))
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::RangeError,
                "epoch nanoseconds out of range".into(),
            )
        })?;
    create_instant(agent, ns, &Value::Undefined)
}

/// spec 8.2.4 fromEpochNanoseconds.
fn from_epoch_nanoseconds(agent: &mut Agent, value: &Value) -> Result<Value, JsError> {
    let bigint = crate::context::to_big_int(agent, value)?;
    let ns = iso::bigint_to_epoch_ns(&bigint)
        .filter(|ns| (iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(ns))
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::RangeError,
                "epoch nanoseconds out of range".into(),
            )
        })?;
    create_instant(agent, ns, &Value::Undefined)
}

fn cmp(a: i128, b: i128) -> i64 {
    if a > b {
        1
    } else if a < b {
        -1
    } else {
        0
    }
}

/// spec 8.5.3 ToTemporalInstant.
pub fn to_temporal_instant(agent: &mut Agent, item: &Value) -> Result<i128, JsError> {
    if matches!(item.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
        if let ValueKind::Object(obj) = item.kind()
            && let Some(record) = agent.temporal_data.get(&obj.id())
        {
            match record {
                TemporalRecord::Instant(ns) => return Ok(*ns),
                TemporalRecord::ZonedDateTime(ns, _) => return Ok(*ns),
                _ => {}
            }
        }
        // spec 8.5.3: ToPrimitive with the string hint; a function's
        // toString yields a source string that then fails to parse.
        let prim =
            crate::context::to_primitive(agent, item, crux::convert::ToPrimitiveHint::String)?;
        if !matches!(prim.kind(), ValueKind::String(_)) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "value must be a string or Temporal.Instant".into(),
            ));
        }
        return instant_from_string(agent, &prim);
    }
    if !matches!(item.kind(), ValueKind::String(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "value must be a string or Temporal.Instant".into(),
        ));
    }
    instant_from_string(agent, item)
}

fn instant_from_string(agent: &mut Agent, item: &Value) -> Result<i128, JsError> {
    let text = crate::context::to_string(agent, item)?;
    let parsed = iso::parse_iso_date_time(text.as_slice(), iso::Format::InstantString)
        .map_err(|_| JsError::new(ErrorKind::RangeError, "invalid ISO string".into()))?;
    let time = parsed.time.ok_or_else(|| {
        JsError::new(
            ErrorKind::RangeError,
            "time is required for an Instant".into(),
        )
    })?;
    let offset_ns = if parsed.tz.z {
        0
    } else {
        iso::parse_date_time_utc_offset(&parsed.tz.offset_string)
            .map_err(|_| JsError::new(ErrorKind::RangeError, "invalid offset".into()))?
    };
    let [h, min, s, ms, us, ns] = time;
    let balanced = balance_iso_date_time(
        parsed.year,
        parsed.month,
        parsed.day,
        h,
        min,
        s,
        ms,
        us,
        (ns as i128 - offset_ns) as i64,
    );
    let days = iso::iso_date_to_epoch_days(balanced.0, balanced.1 - 1, balanced.2);
    if days.abs() > 100_000_000 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "date out of range".into(),
        ));
    }
    let epoch_ns = iso::get_utc_epoch_nanoseconds(
        balanced.0, balanced.1, balanced.2, balanced.3, balanced.4, balanced.5, balanced.6,
        balanced.7, balanced.8,
    );
    if !(iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(&epoch_ns) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "epoch nanoseconds out of range".into(),
        ));
    }
    Ok(epoch_ns)
}

/// spec 5.5.7 BalanceISODateTime.
#[allow(clippy::too_many_arguments)]
pub fn balance_iso_date_time(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    ms: i64,
    us: i64,
    ns: i64,
) -> (i64, i64, i64, i64, i64, i64, i64, i64, i64) {
    let mut us = us + ns.div_euclid(1_000);
    let ns = ns.rem_euclid(1_000);
    let mut ms = ms + us.div_euclid(1_000);
    us = us.rem_euclid(1_000);
    let mut second = second + ms.div_euclid(1_000);
    ms = ms.rem_euclid(1_000);
    let mut minute = minute + second.div_euclid(60);
    second = second.rem_euclid(60);
    let mut hour = hour + minute.div_euclid(60);
    minute = minute.rem_euclid(60);
    let days = hour.div_euclid(24);
    hour = hour.rem_euclid(24);
    let date = iso::add_days_to_iso_date(year, month, day, days);
    (date.0, date.1, date.2, hour, minute, second, ms, us, ns)
}

fn epoch_ms(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let ns = require_instant(agent, this)?;
    // spec 8.3.3: floor, not truncation toward zero (negative instants).
    Ok(Value::Number(ns.div_euclid(1_000_000) as f64))
}

fn epoch_ns(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let ns = require_instant(agent, this)?;
    Ok(Value::BigInt(Handle::new(iso::epoch_ns_to_bigint(ns))))
}

/// spec 8.5.10 AddDurationToInstant.
fn add_subtract(
    agent: &mut Agent,
    this: &Value,
    duration_like: &Value,
    subtract: bool,
) -> Result<Value, JsError> {
    let ns = require_instant(agent, this)?;
    let mut fields = super::to_temporal_duration(agent, duration_like)?;
    if subtract {
        for f in fields.iter_mut() {
            *f = -*f;
        }
    }
    let largest = super::default_temporal_largest_unit(&fields);
    if largest.category() == iso::Category::Date {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "cannot add date units to an Instant".into(),
        ));
    }
    let internal = super::to_internal_duration_record_with_24_hour_days(&fields)?;
    let result = ns + internal.time;
    if !(iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(&result) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "result out of range".into(),
        ));
    }
    create_instant(agent, result, &Value::Undefined)
}

/// spec 8.3.9 `round`.
fn round(agent: &mut Agent, this: &Value, round_to: &Value) -> Result<Value, JsError> {
    let ns = require_instant(agent, this)?;
    if matches!(round_to.kind(), ValueKind::Undefined) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "roundTo is required".into(),
        ));
    }
    let round_to = if let ValueKind::String(text) = round_to.kind() {
        let obj = crux::object::JsObject::ordinary_object_create(None);
        obj.create_data_property_or_throw(
            &JsString::from_utf8("smallestUnit"),
            Value::String(text),
        )?;
        Value::Object(obj)
    } else {
        get_options_object(round_to)?
    };
    let increment = get_rounding_increment(agent, &round_to)?;
    let rounding_mode = get_rounding_mode(agent, &round_to, RoundingMode::HalfExpand)?;
    let smallest = match get_temporal_unit(agent, &round_to, "smallestUnit", None)? {
        // spec 8.3.9: ValidateTemporalUnitValue(smallestUnit, time).
        UnitOption::Unit(Unit::Year)
        | UnitOption::Unit(Unit::Month)
        | UnitOption::Unit(Unit::Week)
        | UnitOption::Unit(Unit::Day) => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "smallestUnit must be a time unit".into(),
            ));
        }
        UnitOption::Unit(u) => u,
        _ => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "smallestUnit is required".into(),
            ));
        }
    };
    if smallest.category() != iso::Category::Time || smallest == Unit::Day {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "smallestUnit must be a time unit".into(),
        ));
    }
    let maximum = match smallest {
        Unit::Hour => 24,
        Unit::Minute => 1_440,
        Unit::Second => 86_400,
        Unit::Millisecond => 86_400_000,
        Unit::Microsecond => 86_400_000_000,
        _ => 86_400_000_000_000,
    };
    super::duration::validate_rounding_increment(increment, maximum, true)?;
    let unit_length = smallest.length_ns().unwrap();
    let rounded = iso::round_number_to_increment_as_if_positive(
        ns,
        unit_length * increment as i128,
        rounding_mode,
    );
    create_instant(agent, rounded, &Value::Undefined)
}

/// spec 8.5.9 DifferenceTemporalInstant.
fn difference(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    since: bool,
) -> Result<Value, JsError> {
    let ns1 = require_instant(agent, this)?;
    let other = args.first().cloned().unwrap_or(Value::Undefined);
    let ns2 = to_temporal_instant(agent, &other)?;
    let options = args.get(1).cloned().unwrap_or(Value::Undefined);
    let resolved = get_options_object(&options)?;
    let settings = get_difference_settings(agent, &resolved, since)?;
    let time = ns2 - ns1;
    let time =
        super::round_time_duration(time, settings.increment, settings.smallest, settings.mode)?;
    let fields =
        super::temporal_duration_from_internal([0.0, 0.0, 0.0, 0.0], time, settings.largest)?;
    let fields = if since {
        super::negate_duration(&fields)
    } else {
        fields
    };
    super::create_temporal_duration(agent, &fields, &Value::Undefined)
}

struct DifferenceSettings {
    largest: Unit,
    increment: i64,
    smallest: Unit,
    mode: RoundingMode,
}

/// spec 13.43 GetDifferenceSettings (unitGroup = time).
fn get_difference_settings(
    agent: &mut Agent,
    options: &Value,
    since: bool,
) -> Result<DifferenceSettings, JsError> {
    let largest_option = get_temporal_unit(agent, options, "largestUnit", None)?;
    let increment = get_rounding_increment(agent, options)?;
    let mut mode = get_rounding_mode(agent, options, RoundingMode::Trunc)?;
    let smallest = get_temporal_unit(agent, options, "smallestUnit", None)?;
    let largest = match &largest_option {
        UnitOption::Unit(u) if u.category() == iso::Category::Time => *u,
        UnitOption::Auto => Unit::Second,
        UnitOption::Unset => Unit::Second,
        UnitOption::Unit(_) => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "largestUnit must be a time unit".into(),
            ));
        }
    };
    let smallest = match smallest {
        UnitOption::Unset => Unit::Nanosecond,
        UnitOption::Unit(u) if u.category() == iso::Category::Time => u,
        _ => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "smallestUnit must be a time unit".into(),
            ));
        }
    };
    let default_largest = iso::larger_of_two_units(Unit::Second, smallest);
    // Reuse the single largestUnit read: an explicit "auto" (or its absence)
    // defaults to the larger of second and smallestUnit (spec 13.43).
    let largest = if matches!(largest_option, UnitOption::Auto | UnitOption::Unset) {
        default_largest
    } else {
        largest
    };
    if iso::larger_of_two_units(largest, smallest) != largest {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "largestUnit must be larger than smallestUnit".into(),
        ));
    }
    if let Some(maximum) = smallest.max_rounding_increment() {
        super::duration::validate_rounding_increment(increment, maximum, false)?;
    }
    if since {
        mode = iso::negate_rounding_mode(mode);
    }
    Ok(DifferenceSettings {
        largest,
        increment,
        smallest,
        mode,
    })
}

/// spec 8.3.11 `toString`.
fn to_string_impl(agent: &mut Agent, this: &Value, options: &Value) -> Result<Value, JsError> {
    let ns = require_instant(agent, this)?;
    let resolved = get_options_object(options)?;
    let digits = get_fractional_second_digits(agent, &resolved)?;
    let rounding_mode = get_rounding_mode(agent, &resolved, RoundingMode::Trunc)?;
    let smallest_unit = get_temporal_unit(agent, &resolved, "smallestUnit", None)?;
    // spec 8.3.11: the timeZone property is read before validation.
    let time_zone =
        crate::context::get_property(agent, &resolved, &JsString::from_utf8("timeZone"), resolved)?;
    let time_zone = if matches!(time_zone.kind(), ValueKind::Undefined) {
        None
    } else {
        Some(to_temporal_time_zone_identifier(agent, &time_zone)?)
    };
    // spec 8.3.11: ValidateTemporalUnitValue(smallestUnit, time), then hour
    // is rejected as too coarse.
    match smallest_unit {
        UnitOption::Unit(Unit::Year)
        | UnitOption::Unit(Unit::Month)
        | UnitOption::Unit(Unit::Week)
        | UnitOption::Unit(Unit::Day) => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "smallestUnit must be a time unit".into(),
            ));
        }
        UnitOption::Unit(Unit::Hour) => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "smallestUnit cannot be hour".into(),
            ));
        }
        _ => {}
    }
    let smallest_for_precision = match smallest_unit {
        UnitOption::Unit(u) => Some(u),
        _ => None,
    };
    let (precision, unit, increment) =
        super::to_seconds_string_precision(smallest_for_precision, digits);
    let unit_length = unit.length_ns().unwrap_or(iso::NS_PER_SECOND);
    let rounded = iso::round_number_to_increment_as_if_positive(
        ns,
        unit_length * increment as i128,
        rounding_mode,
    );
    let text = temporal_instant_to_string(rounded, time_zone.as_deref(), precision)?;
    Ok(Value::String(Handle::new(JsString::from_utf8(&text))))
}

/// spec 8.5.8 TemporalInstantToString.
pub fn temporal_instant_to_string(
    ns: i128,
    time_zone: Option<&str>,
    precision: FracPrecision,
) -> Result<String, JsError> {
    let output_time_zone = time_zone.unwrap_or("UTC");
    let offset = super::offset_ns_at(output_time_zone, ns)
        .ok_or_else(|| JsError::new(ErrorKind::RangeError, "unsupported time zone".into()))?;
    let (y, m, d, h, min, s, ms, us, ns_rest) = iso::iso_parts_from_epoch(ns);
    let balanced = balance_iso_date_time(
        y,
        m,
        d,
        h,
        min,
        s,
        ms,
        us,
        (ns_rest as i128 + offset) as i64,
    );
    let date_time = format!(
        "{}-{:02}-{:02}T{}",
        iso::pad_iso_year(balanced.0),
        balanced.1,
        balanced.2,
        iso::format_time_string(
            balanced.3,
            balanced.4,
            balanced.5,
            balanced.6 * 1_000_000 + balanced.7 * 1_000 + balanced.8,
            precision,
        )
    );
    let zone = if time_zone.is_none() {
        "Z".to_string()
    } else {
        iso::format_date_time_utc_offset_rounded(offset)
    };
    Ok(format!("{date_time}{zone}"))
}

/// spec 11.1.8 ToTemporalTimeZoneIdentifier (UTC + fixed offsets only).
/// spec 11.1.8 ToTemporalTimeZoneIdentifier (UTC + fixed offsets + the
/// ISO-string time zone forms of 13.38 ParseTemporalTimeZoneString).
pub fn to_temporal_time_zone_identifier(
    agent: &mut Agent,
    value: &Value,
) -> Result<String, JsError> {
    if let ValueKind::Object(obj) = value.kind()
        && let Some(TemporalRecord::ZonedDateTime(_, tz)) = agent.temporal_data.get(&obj.id())
    {
        return Ok(tz.to_string_lossy());
    }
    if !matches!(value.kind(), ValueKind::String(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "time zone must be a string".into(),
        ));
    }
    let text = crate::context::to_string(agent, value)?;
    let text = text.to_string_lossy();
    // ParseTimeZoneIdentifier on the whole string: an offset time zone
    // (minute precision) or a named zone.
    if let Ok(parsed) = iso::parse_time_zone_identifier(&text) {
        return match parsed {
            Some(offset_ns) => Ok(iso::format_offset_time_zone_identifier(
                (offset_ns / iso::NS_PER_MINUTE) as i64,
            )),
            None => lookup_named_time_zone(&text),
        };
    }
    // spec 13.38: the string may be an ISO date-time; the annotation, the Z
    // designator, or the numeric offset supplies the identifier.
    let units: Vec<u16> = text.encode_utf16().collect();
    let parsed = iso::parse_iso_date_time(&units, iso::Format::DateTimeZoned)
        .or_else(|_| iso::parse_iso_date_time(&units, iso::Format::DateTimePlain))
        .or_else(|_| iso::parse_iso_date_time(&units, iso::Format::InstantString))
        .or_else(|_| iso::parse_iso_date_time(&units, iso::Format::TimeString))
        .or_else(|_| iso::parse_iso_date_time(&units, iso::Format::MonthDayString))
        .or_else(|_| iso::parse_iso_date_time(&units, iso::Format::YearMonthString))
        .map_err(|_| {
            JsError::new(
                ErrorKind::RangeError,
                format!("unsupported time zone identifier: {text}"),
            )
        })?;
    if !parsed.tz.annotation.is_empty() {
        return match iso::parse_time_zone_identifier(&parsed.tz.annotation).map_err(|_| {
            JsError::new(ErrorKind::RangeError, "invalid time zone annotation".into())
        })? {
            Some(offset_ns) => Ok(iso::format_offset_time_zone_identifier(
                (offset_ns / iso::NS_PER_MINUTE) as i64,
            )),
            None => lookup_named_time_zone(&parsed.tz.annotation),
        };
    }
    if parsed.tz.z {
        return Ok("UTC".to_string());
    }
    if !parsed.tz.offset_string.is_empty() {
        // An offset time zone identifier is minute precision only; a
        // sub-minute offset (with seconds) is not a valid time zone.
        return match iso::parse_time_zone_identifier(&parsed.tz.offset_string)
            .map_err(|_| JsError::new(ErrorKind::RangeError, "invalid offset time zone".into()))?
        {
            Some(offset_ns) => Ok(iso::format_offset_time_zone_identifier(
                (offset_ns / iso::NS_PER_MINUTE) as i64,
            )),
            None => unreachable!("an offset string cannot be a named zone"),
        };
    }
    Err(JsError::new(
        ErrorKind::RangeError,
        format!("unsupported time zone identifier: {text}"),
    ))
}

/// GetAvailableNamedTimeZoneIdentifier: the UTC zone and its IANA links,
/// ASCII-case-insensitive (spec 11.1.1 + 14.6.2). Returns the canonical
/// primary identifier.
fn lookup_named_time_zone(text: &str) -> Result<String, JsError> {
    let upper = text.to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "UTC"
            | "ETC/UTC"
            | "GMT"
            | "ETC/GMT"
            | "UNIVERSAL"
            | "ETC/UNIVERSAL"
            | "ZULU"
            | "ETC/ZULU"
            | "UCT"
            | "ETC/UCT"
            | "GREENWICH"
            | "ETC/GREENWICH"
    ) {
        return Ok("UTC".to_string());
    }
    match unicode::tz::resolve_zone(text) {
        Some(zone) => Ok(unicode::tz::primary_identifier(zone).to_string()),
        None => Err(JsError::new(
            ErrorKind::RangeError,
            format!("unsupported time zone identifier: {text}"),
        )),
    }
}

/// spec 6.5.1 step 5: the constructor accepts only a TimeZoneIdentifier (an
/// offset or a named zone), never an ISO date-time string, and only a String.
pub fn to_constructor_time_zone_identifier(
    agent: &mut Agent,
    value: &Value,
) -> Result<String, JsError> {
    if !matches!(value.kind(), ValueKind::String(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "time zone must be a string".into(),
        ));
    }
    let text = crate::context::to_string(agent, value)?;
    let text = text.to_string_lossy();
    let parsed = iso::parse_time_zone_identifier(&text).map_err(|_| {
        JsError::new(
            ErrorKind::RangeError,
            format!("invalid time zone identifier: {text}"),
        )
    })?;
    match parsed {
        Some(offset_ns) => Ok(iso::format_offset_time_zone_identifier(
            (offset_ns / iso::NS_PER_MINUTE) as i64,
        )),
        None => lookup_named_time_zone(&text),
    }
}

/// spec 8.3.15 toZonedDateTimeISO.
fn to_zoned_date_time_iso(
    agent: &mut Agent,
    this: &Value,
    time_zone: &Value,
) -> Result<Value, JsError> {
    let ns = require_instant(agent, this)?;
    let tz = to_temporal_time_zone_identifier(agent, time_zone)?;
    create_temporal_object(
        agent,
        &Value::Undefined,
        "%Temporal.ZonedDateTime.prototype%",
        TemporalRecord::ZonedDateTime(ns, JsString::from_utf8(&tz)),
    )
}
