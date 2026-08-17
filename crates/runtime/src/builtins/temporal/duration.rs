//! `Temporal.Duration` (spec 7): the constructor, statics, and prototype.
//! Includes the relative-duration rounding machinery (7.5.29-7.5.41) shared
//! by the PlainDate/PlainDateTime difference paths.

use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::property::PropertyDescriptor;
use crux::string::JsString;
use crux::value::Value;

use crate::agent::Agent;
use crate::realm::Realm;

use super::iso::{self, FracPrecision, RoundingMode, Unit};
use super::{
    RecordKind, TemporalRecord, UnitOption, create_temporal_duration, duration_sign,
    get_fractional_second_digits, get_options_object, get_rounding_increment, get_rounding_mode,
    get_temporal_unit, install_constructor, negate_duration, placeholder, require_record,
    to_integer_if_integral, to_temporal_duration,
};

const DURATION: &str = "%Temporal.Duration%";
const DURATION_PROTO: &str = "%Temporal.Duration.prototype%";
const DURATION_FROM: &str = "%Temporal.Duration.from%";
const DURATION_COMPARE: &str = "%Temporal.Duration.compare%";

const P_YEARS: &str = "%Temporal.Duration.prototype.years%";
const P_MONTHS: &str = "%Temporal.Duration.prototype.months%";
const P_WEEKS: &str = "%Temporal.Duration.prototype.weeks%";
const P_DAYS: &str = "%Temporal.Duration.prototype.days%";
const P_HOURS: &str = "%Temporal.Duration.prototype.hours%";
const P_MINUTES: &str = "%Temporal.Duration.prototype.minutes%";
const P_SECONDS: &str = "%Temporal.Duration.prototype.seconds%";
const P_MILLISECONDS: &str = "%Temporal.Duration.prototype.milliseconds%";
const P_MICROSECONDS: &str = "%Temporal.Duration.prototype.microseconds%";
const P_NANOSECONDS: &str = "%Temporal.Duration.prototype.nanoseconds%";
const P_SIGN: &str = "%Temporal.Duration.prototype.sign%";
const P_BLANK: &str = "%Temporal.Duration.prototype.blank%";
const P_WITH: &str = "%Temporal.Duration.prototype.with%";
const P_NEGATED: &str = "%Temporal.Duration.prototype.negated%";
const P_ABS: &str = "%Temporal.Duration.prototype.abs%";
const P_ADD: &str = "%Temporal.Duration.prototype.add%";
const P_SUBTRACT: &str = "%Temporal.Duration.prototype.subtract%";
const P_ROUND: &str = "%Temporal.Duration.prototype.round%";
const P_TOTAL: &str = "%Temporal.Duration.prototype.total%";
const P_TO_STRING: &str = "%Temporal.Duration.prototype.toString%";
const P_TO_JSON: &str = "%Temporal.Duration.prototype.toJSON%";
const P_TO_LOCALE: &str = "%Temporal.Duration.prototype.toLocaleString%";
const P_VALUE_OF: &str = "%Temporal.Duration.prototype.valueOf%";

/// Install `Temporal.Duration` (spec 7.1-7.3).
pub fn install(
    parent: &Handle<crux::object::JsObject>,
    realm: &Handle<Realm>,
) -> Result<(), JsError> {
    let (ctor, proto) = install_constructor(
        realm,
        parent,
        "Duration",
        DURATION,
        DURATION_PROTO,
        0,
        "Temporal.Duration",
    )?;

    for (name, intrinsic, length) in [("from", DURATION_FROM, 1), ("compare", DURATION_COMPARE, 2)]
    {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            placeholder(name),
            None,
            None,
        )?;
        realm
            .intrinsics
            .define(intrinsic, Value::Function(func.clone()));
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

    // Prototype getters (accessor properties, spec 7.3.3-7.3.14).
    for (name, intrinsic) in [
        ("years", P_YEARS),
        ("months", P_MONTHS),
        ("weeks", P_WEEKS),
        ("days", P_DAYS),
        ("hours", P_HOURS),
        ("minutes", P_MINUTES),
        ("seconds", P_SECONDS),
        ("milliseconds", P_MILLISECONDS),
        ("microseconds", P_MICROSECONDS),
        ("nanoseconds", P_NANOSECONDS),
        ("sign", P_SIGN),
        ("blank", P_BLANK),
    ] {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(&format!("get {name}"))),
            0,
            placeholder(name),
            None,
            None,
        )?;
        realm
            .intrinsics
            .define(intrinsic, Value::Function(func.clone()));
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
        ("with", P_WITH, 1),
        ("negated", P_NEGATED, 0),
        ("abs", P_ABS, 0),
        ("add", P_ADD, 1),
        ("subtract", P_SUBTRACT, 1),
        ("round", P_ROUND, 1),
        ("total", P_TOTAL, 1),
        ("toString", P_TO_STRING, 0),
        ("toJSON", P_TO_JSON, 0),
        ("toLocaleString", P_TO_LOCALE, 0),
        ("valueOf", P_VALUE_OF, 0),
    ] {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            placeholder(name),
            None,
            None,
        )?;
        realm
            .intrinsics
            .define(intrinsic, Value::Function(func.clone()));
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
    if intrinsics.get(DURATION).as_ref() == Some(callee) {
        // The constructor is not callable.
        return Some(Err(JsError::new(
            ErrorKind::TypeError,
            "Temporal.Duration cannot be called as a function".into(),
        )));
    }
    if intrinsics.get(DURATION_FROM).as_ref() == Some(callee) {
        let item = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(
            to_temporal_duration(agent, &item)
                .and_then(|fields| create_temporal_duration(agent, &fields, &Value::Undefined)),
        );
    }
    if intrinsics.get(DURATION_COMPARE).as_ref() == Some(callee) {
        return Some(duration_compare(agent, args));
    }
    if intrinsics.get(P_YEARS).as_ref() == Some(callee) {
        return Some(duration_field(agent, this, 0));
    }
    if intrinsics.get(P_MONTHS).as_ref() == Some(callee) {
        return Some(duration_field(agent, this, 1));
    }
    if intrinsics.get(P_WEEKS).as_ref() == Some(callee) {
        return Some(duration_field(agent, this, 2));
    }
    if intrinsics.get(P_DAYS).as_ref() == Some(callee) {
        return Some(duration_field(agent, this, 3));
    }
    if intrinsics.get(P_HOURS).as_ref() == Some(callee) {
        return Some(duration_field(agent, this, 4));
    }
    if intrinsics.get(P_MINUTES).as_ref() == Some(callee) {
        return Some(duration_field(agent, this, 5));
    }
    if intrinsics.get(P_SECONDS).as_ref() == Some(callee) {
        return Some(duration_field(agent, this, 6));
    }
    if intrinsics.get(P_MILLISECONDS).as_ref() == Some(callee) {
        return Some(duration_field(agent, this, 7));
    }
    if intrinsics.get(P_MICROSECONDS).as_ref() == Some(callee) {
        return Some(duration_field(agent, this, 8));
    }
    if intrinsics.get(P_NANOSECONDS).as_ref() == Some(callee) {
        return Some(duration_field(agent, this, 9));
    }
    if intrinsics.get(P_SIGN).as_ref() == Some(callee) {
        return Some(sign(agent, this));
    }
    if intrinsics.get(P_BLANK).as_ref() == Some(callee) {
        return Some(blank(agent, this));
    }
    if intrinsics.get(P_WITH).as_ref() == Some(callee) {
        let item = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(with(agent, this, &item));
    }
    if intrinsics.get(P_NEGATED).as_ref() == Some(callee) {
        return Some(negated(agent, this));
    }
    if intrinsics.get(P_ABS).as_ref() == Some(callee) {
        return Some(abs(agent, this));
    }
    if intrinsics.get(P_ADD).as_ref() == Some(callee) {
        let other = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(match require_duration(agent, this) {
            Ok(duration) => super::add_durations(agent, &duration, &other, false),
            Err(e) => Err(e),
        });
    }
    if intrinsics.get(P_SUBTRACT).as_ref() == Some(callee) {
        let other = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(match require_duration(agent, this) {
            Ok(duration) => super::add_durations(agent, &duration, &other, true),
            Err(e) => Err(e),
        });
    }
    if intrinsics.get(P_ROUND).as_ref() == Some(callee) {
        let round_to = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(round(agent, this, &round_to));
    }
    if intrinsics.get(P_TOTAL).as_ref() == Some(callee) {
        let total_of = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(total(agent, this, &total_of));
    }
    if intrinsics.get(P_TO_STRING).as_ref() == Some(callee) {
        let options = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(to_string_impl(agent, this, &options));
    }
    if intrinsics.get(P_TO_JSON).as_ref() == Some(callee) {
        return Some(to_json(agent, this));
    }
    if intrinsics.get(P_TO_LOCALE).as_ref() == Some(callee) {
        return Some(to_json(agent, this));
    }
    if intrinsics.get(P_VALUE_OF).as_ref() == Some(callee) {
        return Some(Err(JsError::new(
            ErrorKind::TypeError,
            "Temporal.Duration.prototype.valueOf throws".into(),
        )));
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
    if realm.intrinsics.get(DURATION).as_ref() == Some(callee) {
        return Some(construct(agent, args, new_target));
    }
    None
}

fn require_duration(agent: &Agent, this: &Value) -> Result<[f64; 10], JsError> {
    match require_record(agent, this, RecordKind::Duration)? {
        TemporalRecord::Duration(fields) => Ok(fields),
        _ => unreachable!(),
    }
}

/// spec 7.1.1.
fn construct(agent: &mut Agent, args: &[Value], new_target: &Value) -> Result<Value, JsError> {
    let mut fields = [0f64; 10];
    for (i, value) in args.iter().take(10).enumerate() {
        if !matches!(value, Value::Undefined) {
            fields[i] = to_integer_if_integral(agent, value)?;
        }
    }
    create_temporal_duration(agent, &fields, new_target)
}

fn duration_field(agent: &Agent, this: &Value, index: usize) -> Result<Value, JsError> {
    Ok(Value::Number(require_duration(agent, this)?[index]))
}

fn sign(agent: &Agent, this: &Value) -> Result<Value, JsError> {
    Ok(Value::Number(
        duration_sign(&require_duration(agent, this)?) as f64,
    ))
}

fn blank(agent: &Agent, this: &Value) -> Result<Value, JsError> {
    Ok(Value::Boolean(
        duration_sign(&require_duration(agent, this)?) == 0,
    ))
}

/// spec 7.3.15 `with`.
fn with(agent: &mut Agent, this: &Value, item: &Value) -> Result<Value, JsError> {
    let duration = require_duration(agent, this)?;
    if !matches!(item, Value::Object(_) | Value::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "temporalDurationLike must be an object".into(),
        ));
    }
    let partial = super::read_duration_fields(agent, item)?;
    let mut fields = duration;
    for (i, value) in partial.iter().enumerate() {
        if let Some(v) = value {
            fields[i] = *v;
        }
    }
    create_temporal_duration(agent, &fields, &Value::Undefined)
}

fn negated(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let fields = require_duration(agent, this)?;
    create_temporal_duration(agent, &negate_duration(&fields), &Value::Undefined)
}

fn abs(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let fields = require_duration(agent, this)?;
    let fields: [f64; 10] = fields.map(f64::abs);
    create_temporal_duration(agent, &fields, &Value::Undefined)
}

/// spec 7.2.3 `compare`.
fn duration_compare(agent: &mut Agent, args: &[Value]) -> Result<Value, JsError> {
    let one = to_temporal_duration(agent, &args.first().cloned().unwrap_or(Value::Undefined))?;
    let two = to_temporal_duration(agent, &args.get(1).cloned().unwrap_or(Value::Undefined))?;
    // spec 7.2.3: GetOptionsObject and GetTemporalRelativeToOption both run
    // before the equal-durations early return (a bad options argument or a
    // bad relativeTo throws even for identical durations).
    let options = args.get(2).cloned().unwrap_or(Value::Undefined);
    let resolved = get_options_object(&options)?;
    let relative = super::get_temporal_relative_to(agent, &resolved)?;
    if one == two {
        return Ok(Value::Number(0.0));
    }
    let largest_unit1 = super::default_temporal_largest_unit(&one);
    let largest_unit2 = super::default_temporal_largest_unit(&two);
    let d1 = super::to_internal_duration_record(&one);
    let d2 = super::to_internal_duration_record(&two);
    match relative {
        super::RelativeTo::Zoned(ns, tz) => {
            if largest_unit1.category() == iso::Category::Date
                || largest_unit2.category() == iso::Category::Date
            {
                let after1 = add_zoned_date_time(ns, &tz, d1)?;
                let after2 = add_zoned_date_time(ns, &tz, d2)?;
                return Ok(Value::Number(cmp_i128(after1, after2) as f64));
            }
        }
        super::RelativeTo::Plain(date) => {
            if iso::is_calendar_unit(largest_unit1) || iso::is_calendar_unit(largest_unit2) {
                let days1 = super::date_duration_days(d1.date, date)?;
                let days2 = super::date_duration_days(d2.date, date)?;
                let t1 = super::add_24_hour_days_to_time_duration(d1.time, days1 as i128)?;
                let t2 = super::add_24_hour_days_to_time_duration(d2.time, days2 as i128)?;
                return Ok(Value::Number(cmp_i128(t1, t2) as f64));
            }
        }
        super::RelativeTo::None => {
            if iso::is_calendar_unit(largest_unit1) || iso::is_calendar_unit(largest_unit2) {
                return Err(JsError::new(
                    ErrorKind::RangeError,
                    "relativeTo is required for durations with calendar units".into(),
                ));
            }
        }
    }
    let days1 = one[3] as i128;
    let days2 = two[3] as i128;
    let t1 = super::add_24_hour_days_to_time_duration(d1.time, days1)?;
    let t2 = super::add_24_hour_days_to_time_duration(d2.time, days2)?;
    Ok(Value::Number(cmp_i128(t1, t2) as f64))
}

fn cmp_i128(a: i128, b: i128) -> i64 {
    if a > b {
        1
    } else if a < b {
        -1
    } else {
        0
    }
}

/// Add an internal duration to an epoch in a fixed-offset time zone
/// (spec 6.5.5 AddZonedDateTime for offset zones).
pub fn add_zoned_date_time(
    epoch_ns: i128,
    tz: &str,
    duration: super::InternalDuration,
) -> Result<i128, JsError> {
    let offset = super::offset_time_zone_offset_ns(tz)
        .ok_or_else(|| JsError::new(ErrorKind::RangeError, "unsupported time zone".into()))?;
    let (y, m, d, h, min, s, ms, us, ns) = iso::iso_parts_from_epoch(epoch_ns + offset);
    if duration.date[0] != 0.0
        || duration.date[1] != 0.0
        || duration.date[2] != 0.0
        || duration.date[3] != 0.0
    {
        let (y, m, d) = iso::calendar_date_add(
            y,
            m,
            d,
            duration.date[0] as i64,
            duration.date[1] as i64,
            duration.date[2] as i64,
            duration.date[3] as i64,
            true,
        )
        .ok_or_else(|| JsError::new(ErrorKind::RangeError, "date out of range".into()))?;
        let intermediate = iso::get_utc_epoch_nanoseconds(y, m, d, h, min, s, ms, us, ns) - offset;
        let result = intermediate + duration.time;
        if !(iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(&result) {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "result out of range".into(),
            ));
        }
        Ok(result)
    } else {
        let result = epoch_ns + duration.time;
        if !(iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(&result) {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "result out of range".into(),
            ));
        }
        Ok(result)
    }
}

/// spec 7.3.20 `round`.
fn round(agent: &mut Agent, this: &Value, round_to: &Value) -> Result<Value, JsError> {
    let duration = require_duration(agent, this)?;
    if matches!(round_to, Value::Undefined) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "roundTo is required".into(),
        ));
    }
    let round_to = if let Value::String(text) = round_to {
        let obj = crux::object::JsObject::ordinary_object_create(None);
        obj.create_data_property_or_throw(
            &JsString::from_utf8("smallestUnit"),
            Value::String(text.clone()),
        )?;
        Value::Object(obj)
    } else {
        get_options_object(round_to)?
    };

    let largest_unit = get_temporal_unit(agent, &round_to, "largestUnit", None)?;
    let relative = super::get_temporal_relative_to(agent, &round_to)?;
    let rounding_increment = get_rounding_increment(agent, &round_to)?;
    let rounding_mode = get_rounding_mode(agent, &round_to, RoundingMode::HalfExpand)?;
    let smallest_unit = get_temporal_unit(agent, &round_to, "smallestUnit", None)?;

    let mut smallest_present = true;
    let smallest = match smallest_unit {
        UnitOption::Unset => {
            smallest_present = false;
            Unit::Nanosecond
        }
        UnitOption::Auto => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "smallestUnit cannot be auto".into(),
            ));
        }
        UnitOption::Unit(u) => u,
    };
    validate_unit_value(smallest, false)?;

    let existing_largest = super::default_temporal_largest_unit(&duration);
    let default_largest = iso::larger_of_two_units(existing_largest, smallest);
    let mut largest_present = true;
    let largest = match largest_unit {
        UnitOption::Unset => {
            largest_present = false;
            default_largest
        }
        UnitOption::Auto => default_largest,
        UnitOption::Unit(u) => u,
    };
    if !smallest_present && !largest_present {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "at least one of smallestUnit or largestUnit is required".into(),
        ));
    }
    if iso::larger_of_two_units(largest, smallest) != largest {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "largestUnit must be larger than smallestUnit".into(),
        ));
    }
    if let Some(maximum) = smallest.max_rounding_increment() {
        validate_rounding_increment(rounding_increment, maximum, false)?;
    }
    if rounding_increment > 1 && largest != smallest && smallest.category() == iso::Category::Date {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "roundingIncrement must be 1 for calendar units".into(),
        ));
    }

    let internal = super::to_internal_duration_record(&duration);
    match relative {
        super::RelativeTo::Zoned(ns, tz) => {
            let target = add_zoned_date_time(ns, &tz, internal)?;
            let internal = difference_zoned_date_time_with_rounding(
                ns,
                target,
                &tz,
                largest,
                rounding_increment,
                smallest,
                rounding_mode,
            )?;
            let largest_out = if largest.category() == iso::Category::Date {
                Unit::Hour
            } else {
                largest
            };
            let result =
                super::temporal_duration_from_internal(internal.date, internal.time, largest_out)?;
            create_temporal_duration(agent, &result, &Value::Undefined)
        }
        super::RelativeTo::Plain(date) => {
            let internal24 = super::to_internal_duration_record_with_24_hour_days(&duration)?;
            let (target_time, target_date) = add_time_to_midnight(date, internal24)?;
            let origin = (date.date.0, date.date.1, date.date.2, 0, 0, 0, 0, 0, 0);
            let target = (
                target_date.0,
                target_date.1,
                target_date.2,
                target_time[0],
                target_time[1],
                target_time[2],
                target_time[3],
                target_time[4],
                target_time[5],
            );
            let internal = difference_plain_date_time_with_rounding(
                origin,
                target,
                largest,
                rounding_increment,
                smallest,
                rounding_mode,
            )?;
            let result =
                super::temporal_duration_from_internal(internal.date, internal.time, largest)?;
            create_temporal_duration(agent, &result, &Value::Undefined)
        }
        super::RelativeTo::None => {
            if iso::is_calendar_unit(existing_largest) || iso::is_calendar_unit(largest) {
                return Err(JsError::new(
                    ErrorKind::RangeError,
                    "relativeTo is required for durations with calendar units".into(),
                ));
            }
            let internal24 = super::to_internal_duration_record_with_24_hour_days(&duration)?;
            let (date, time) = if smallest == Unit::Day {
                let days = super::round_time_duration(
                    internal24.time,
                    rounding_increment,
                    Unit::Day,
                    rounding_mode,
                )? / iso::NS_PER_DAY;
                ([0.0, 0.0, 0.0, days as f64], 0i128)
            } else {
                let time = super::round_time_duration(
                    internal24.time,
                    rounding_increment,
                    smallest,
                    rounding_mode,
                )?;
                ([0.0, 0.0, 0.0, 0.0], time)
            };
            let result = super::temporal_duration_from_internal(date, time, largest)?;
            create_temporal_duration(agent, &result, &Value::Undefined)
        }
    }
}

fn validate_unit_value(unit: Unit, allow_auto: bool) -> Result<(), JsError> {
    let _ = allow_auto;
    match unit {
        Unit::Year
        | Unit::Month
        | Unit::Week
        | Unit::Day
        | Unit::Hour
        | Unit::Minute
        | Unit::Second
        | Unit::Millisecond
        | Unit::Microsecond
        | Unit::Nanosecond => Ok(()),
    }
}

/// spec 13.14 ValidateTemporalRoundingIncrement.
pub fn validate_rounding_increment(
    increment: i64,
    dividend: i64,
    inclusive: bool,
) -> Result<(), JsError> {
    let maximum = if inclusive { dividend } else { dividend - 1 };
    if increment > maximum || dividend % increment != 0 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "roundingIncrement must evenly divide the unit".into(),
        ));
    }
    Ok(())
}

/// spec 7.3.21 `total`.
fn total(agent: &mut Agent, this: &Value, total_of: &Value) -> Result<Value, JsError> {
    let duration = require_duration(agent, this)?;
    if matches!(total_of, Value::Undefined) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "totalOf is required".into(),
        ));
    }
    let total_of = if let Value::String(text) = total_of {
        let obj = crux::object::JsObject::ordinary_object_create(None);
        obj.create_data_property_or_throw(
            &JsString::from_utf8("unit"),
            Value::String(text.clone()),
        )?;
        Value::Object(obj)
    } else {
        get_options_object(total_of)?
    };
    let relative = super::get_temporal_relative_to(agent, &total_of)?;
    let unit = match get_temporal_unit(agent, &total_of, "unit", None)? {
        UnitOption::Unit(u) => u,
        UnitOption::Auto | UnitOption::Unset => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "unit is required".into(),
            ));
        }
    };
    validate_unit_value(unit, false)?;
    let internal = super::to_internal_duration_record(&duration);
    let total = match relative {
        super::RelativeTo::Zoned(ns, tz) => {
            let target = add_zoned_date_time(ns, &tz, internal)?;
            // Fixed-offset zones only: a day is always 24 h, so the total in
            // days is the exact epoch difference (spec 6.5.8 for time units;
            // the NudgeToCalendarUnit day path applies to DST zones).
            if iso::is_calendar_unit(unit) {
                let offset = super::offset_time_zone_offset_ns(&tz).unwrap_or(0);
                let dt = iso::iso_parts_from_epoch(ns + offset);
                nudge_to_calendar_unit_total(
                    internal_duration_sign(&internal),
                    internal,
                    ns,
                    target,
                    dt,
                    Some(&tz),
                    1,
                    unit,
                )?
            } else if unit == iso::Unit::Day {
                // spec 7.5.39: a zoned day total goes through
                // NudgeToCalendarUnit, whose ComputeNudgeWindow materializes
                // the ±1-day end boundary; the boundary's exact time must be
                // a valid Instant (test262 relativeto-date-limits).
                let offset = super::offset_time_zone_offset_ns(&tz).unwrap_or(0);
                let dt = iso::iso_parts_from_epoch(ns + offset);
                let sign = if target - ns < 0 { -1i64 } else { 1i64 };
                let (ey, em, ed) = iso::add_days_to_iso_date(dt.0, dt.1, dt.2, sign);
                if iso::iso_date_to_epoch_days(ey, em - 1, ed).abs() > 100_000_000 {
                    return Err(JsError::new(
                        ErrorKind::RangeError,
                        "result is out of range".into(),
                    ));
                }
                let end_epoch =
                    iso::get_utc_epoch_nanoseconds(ey, em, ed, dt.3, dt.4, dt.5, dt.6, dt.7, dt.8)
                        - offset;
                if !(iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(&end_epoch) {
                    return Err(JsError::new(
                        ErrorKind::RangeError,
                        "result is out of range".into(),
                    ));
                }
                super::total_time_duration(target - ns, unit)
            } else {
                super::total_time_duration(target - ns, unit)
            }
        }
        super::RelativeTo::Plain(date) => {
            let internal24 = super::to_internal_duration_record_with_24_hour_days(&duration)?;
            let (target_time, target_date) = add_time_to_midnight(date, internal24)?;
            let origin = (date.date.0, date.date.1, date.date.2, 0, 0, 0, 0, 0, 0);
            let target = (
                target_date.0,
                target_date.1,
                target_date.2,
                target_time[0],
                target_time[1],
                target_time[2],
                target_time[3],
                target_time[4],
                target_time[5],
            );
            let origin_ns =
                iso::get_utc_epoch_nanoseconds(origin.0, origin.1, origin.2, 0, 0, 0, 0, 0, 0);
            let dest_ns = iso::get_utc_epoch_nanoseconds(
                target.0, target.1, target.2, target.3, target.4, target.5, target.6, target.7,
                target.8,
            );
            // spec 5.5.14: DifferencePlainDateTimeWithTotal rejects either
            // datetime outside ISODateTimeWithinLimits (after the
            // zero-difference early return). The midnight origin on
            // -271821-04-19 sits exactly at nsMin - nsPerDay.
            if dest_ns != origin_ns
                && (!iso::iso_date_time_within_limits(
                    origin.0, origin.1, origin.2, 0, 0, 0, 0, 0, 0,
                ) || !iso::iso_date_time_within_limits(
                    target.0, target.1, target.2, target.3, target.4, target.5, target.6, target.7,
                    target.8,
                ))
            {
                return Err(JsError::new(
                    ErrorKind::RangeError,
                    "relativeTo is outside the representable range".into(),
                ));
            }
            if iso::is_calendar_unit(unit) {
                nudge_to_calendar_unit_total(
                    internal_duration_sign(&internal24),
                    internal24,
                    origin_ns,
                    dest_ns,
                    origin,
                    None,
                    1,
                    unit,
                )?
            } else {
                super::total_time_duration(dest_ns - origin_ns, unit)
            }
        }
        super::RelativeTo::None => {
            let largest = super::default_temporal_largest_unit(&duration);
            if iso::is_calendar_unit(largest) || iso::is_calendar_unit(unit) {
                return Err(JsError::new(
                    ErrorKind::RangeError,
                    "relativeTo is required for calendar units".into(),
                ));
            }
            let internal24 = super::to_internal_duration_record_with_24_hour_days(&duration)?;
            super::total_time_duration(internal24.time, unit)
        }
    };
    Ok(Value::Number(total))
}

/// spec 7.3.22 `toString`.
fn to_string_impl(agent: &mut Agent, this: &Value, options: &Value) -> Result<Value, JsError> {
    let duration = require_duration(agent, this)?;
    let resolved = get_options_object(options)?;
    let digits = get_fractional_second_digits(agent, &resolved)?;
    let rounding_mode = get_rounding_mode(agent, &resolved, RoundingMode::Trunc)?;
    let smallest_unit = get_temporal_unit(agent, &resolved, "smallestUnit", None)?;
    let smallest = match smallest_unit {
        // spec 7.3.22: ValidateTemporalUnitValue(smallestUnit, time) first,
        // then hour/minute are rejected as too coarse.
        UnitOption::Unit(Unit::Year)
        | UnitOption::Unit(Unit::Month)
        | UnitOption::Unit(Unit::Week)
        | UnitOption::Unit(Unit::Day) => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "smallestUnit must be a time unit".into(),
            ));
        }
        UnitOption::Unit(Unit::Hour) | UnitOption::Unit(Unit::Minute) => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "smallestUnit must be second or smaller".into(),
            ));
        }
        UnitOption::Unit(u) => u,
        _ => Unit::Nanosecond,
    };
    validate_unit_value(smallest, false)?;
    let smallest_for_precision = match smallest_unit {
        UnitOption::Unit(u) => Some(u),
        _ => None,
    };
    let (precision, unit, increment) =
        super::to_seconds_string_precision(smallest_for_precision, digits);
    let text = if unit == Unit::Nanosecond && increment == 1 {
        super::temporal_duration_to_string(&duration, precision)
    } else {
        let largest = super::default_temporal_largest_unit(&duration);
        let internal = super::to_internal_duration_record(&duration);
        let time = super::round_time_duration(internal.time, increment, unit, rounding_mode)?;
        let rounded_largest = iso::larger_of_two_units(largest, Unit::Second);
        let rounded = super::temporal_duration_from_internal(internal.date, time, rounded_largest)?;
        super::temporal_duration_to_string(&rounded, precision)
    };
    Ok(Value::String(Handle::new(JsString::from_utf8(&text))))
}

fn to_json(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let duration = require_duration(agent, this)?;
    let text = super::temporal_duration_to_string(&duration, FracPrecision::Auto);
    Ok(Value::String(Handle::new(JsString::from_utf8(&text))))
}

// ---------------------------------------------------------------------------
// Relative-duration rounding (spec 7.5.29-7.5.39)
// ---------------------------------------------------------------------------

/// A 9-field ISO date-time: (y, m, d, h, min, s, ms, us, ns).
pub type IsoDateTime = (i64, i64, i64, i64, i64, i64, i64, i64, i64);

/// Add a time duration to midnight at a date, returning the balanced time
/// and date (spec 7.3.20 plain-relativeTo path).
#[allow(clippy::type_complexity)]
fn add_time_to_midnight(
    relative: super::PlainRelativeTo,
    internal: super::InternalDuration,
) -> Result<([i64; 6], (i64, i64, i64)), JsError> {
    let (y, m, d) = relative.date;
    let total = internal.time;
    let days = total.div_euclid(iso::NS_PER_DAY);
    let mut rem = total.rem_euclid(iso::NS_PER_DAY);
    let h = (rem / iso::NS_PER_HOUR) as i64;
    rem %= iso::NS_PER_HOUR;
    let min = (rem / iso::NS_PER_MINUTE) as i64;
    rem %= iso::NS_PER_MINUTE;
    let s = (rem / iso::NS_PER_SECOND) as i64;
    rem %= iso::NS_PER_SECOND;
    let ms = (rem / 1_000_000) as i64;
    rem %= 1_000_000;
    let us = (rem / 1_000) as i64;
    let ns = (rem % 1_000) as i64;
    let mut date = (y, m, d);
    if days != 0 {
        let date_dur = [
            internal.date[0],
            internal.date[1],
            internal.date[2],
            internal.date[3] + days as f64,
        ];
        date = iso::calendar_date_add(
            y,
            m,
            d,
            date_dur[0] as i64,
            date_dur[1] as i64,
            date_dur[2] as i64,
            date_dur[3] as i64,
            true,
        )
        .ok_or_else(|| JsError::new(ErrorKind::RangeError, "date out of range".into()))?;
    } else if internal.date[0] != 0.0
        || internal.date[1] != 0.0
        || internal.date[2] != 0.0
        || internal.date[3] != 0.0
    {
        date = iso::calendar_date_add(
            y,
            m,
            d,
            internal.date[0] as i64,
            internal.date[1] as i64,
            internal.date[2] as i64,
            internal.date[3] as i64,
            true,
        )
        .ok_or_else(|| JsError::new(ErrorKind::RangeError, "date out of range".into()))?;
    }
    Ok(([h, min, s, ms, us, ns], date))
}

/// spec 5.5.12 DifferenceISODateTime (plain, no time zone).
pub fn difference_iso_date_time(
    one: IsoDateTime,
    two: IsoDateTime,
    largest_unit: Unit,
) -> super::InternalDuration {
    let t1 = time_record_ns((one.3, one.4, one.5, one.6, one.7, one.8));
    let t2 = time_record_ns((two.3, two.4, two.5, two.6, two.7, two.8));
    let mut time_duration = t2 - t1;
    let time_sign = time_duration.signum();
    let date_sign = iso::compare_iso_date((one.0, one.1, one.2), (two.0, two.1, two.2));
    let mut adjusted_date = (two.0, two.1, two.2);
    if time_sign != 0 && time_sign == date_sign as i128 {
        adjusted_date = iso::add_days_to_iso_date(
            adjusted_date.0,
            adjusted_date.1,
            adjusted_date.2,
            time_sign as i64,
        );
        time_duration -= time_sign * iso::NS_PER_DAY;
    }
    let date_largest = iso::larger_of_two_units(Unit::Day, largest_unit);
    let date_difference =
        iso::calendar_date_until((one.0, one.1, one.2), adjusted_date, date_largest);
    let mut date_difference = [
        date_difference.0 as f64,
        date_difference.1 as f64,
        date_difference.2 as f64,
        date_difference.3 as f64,
    ];
    if largest_unit != date_largest {
        time_duration += date_difference[3] as i128 * iso::NS_PER_DAY;
        date_difference[3] = 0.0;
    }
    super::InternalDuration {
        date: date_difference,
        time: time_duration,
    }
}

fn time_record_ns(t: (i64, i64, i64, i64, i64, i64)) -> i128 {
    let (h, m, s, ms, us, ns) = t;
    ((((h as i128 * 60 + m as i128) * 60 + s as i128) * 1000 + ms as i128) * 1000 + us as i128)
        * 1000
        + ns as i128
}

/// spec 5.5.13 DifferencePlainDateTimeWithRounding.
#[allow(clippy::too_many_arguments)]
pub fn difference_plain_date_time_with_rounding(
    one: IsoDateTime,
    two: IsoDateTime,
    largest_unit: Unit,
    rounding_increment: i64,
    smallest_unit: Unit,
    rounding_mode: RoundingMode,
) -> Result<super::InternalDuration, JsError> {
    if iso::compare_iso_date((one.0, one.1, one.2), (two.0, two.1, two.2)) == 0
        && time_record_ns((one.3, one.4, one.5, one.6, one.7, one.8))
            == time_record_ns((two.3, two.4, two.5, two.6, two.7, two.8))
    {
        return Ok(super::InternalDuration {
            date: [0.0, 0.0, 0.0, 0.0],
            time: 0,
        });
    }
    if !iso::iso_date_time_within_limits(
        one.0, one.1, one.2, one.3, one.4, one.5, one.6, one.7, one.8,
    ) || !iso::iso_date_time_within_limits(
        two.0, two.1, two.2, two.3, two.4, two.5, two.6, two.7, two.8,
    ) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "date-time out of range".into(),
        ));
    }
    let mut diff = difference_iso_date_time(one, two, largest_unit);
    if smallest_unit == Unit::Nanosecond && rounding_increment == 1 {
        return Ok(diff);
    }
    let origin_ns = iso::get_utc_epoch_nanoseconds(
        one.0, one.1, one.2, one.3, one.4, one.5, one.6, one.7, one.8,
    );
    let dest_ns = iso::get_utc_epoch_nanoseconds(
        two.0, two.1, two.2, two.3, two.4, two.5, two.6, two.7, two.8,
    );
    round_relative_duration(
        &mut diff,
        origin_ns,
        dest_ns,
        one,
        None,
        largest_unit,
        rounding_increment,
        smallest_unit,
        rounding_mode,
    )?;
    Ok(diff)
}

/// spec 6.5.7 DifferenceZonedDateTimeWithRounding for fixed-offset zones.
pub fn difference_zoned_date_time_with_rounding(
    ns1: i128,
    ns2: i128,
    tz: &str,
    largest_unit: Unit,
    rounding_increment: i64,
    smallest_unit: Unit,
    rounding_mode: RoundingMode,
) -> Result<super::InternalDuration, JsError> {
    if largest_unit.category() == iso::Category::Time {
        let time = ns2 - ns1;
        let time =
            super::round_time_duration(time, rounding_increment, smallest_unit, rounding_mode)?;
        return Ok(super::InternalDuration {
            date: [0.0, 0.0, 0.0, 0.0],
            time,
        });
    }
    let offset = super::offset_time_zone_offset_ns(tz)
        .ok_or_else(|| JsError::new(ErrorKind::RangeError, "unsupported time zone".into()))?;
    let (y1, m1, d1, h1, min1, s1, ms1, us1, ns1_rest) = iso::iso_parts_from_epoch(ns1 + offset);
    let (y2, m2, d2, h2, min2, s2, ms2, us2, ns2_rest) = iso::iso_parts_from_epoch(ns2 + offset);
    let mut diff = difference_iso_date_time(
        (y1, m1, d1, h1, min1, s1, ms1, us1, ns1_rest),
        (y2, m2, d2, h2, min2, s2, ms2, us2, ns2_rest),
        largest_unit,
    );
    if smallest_unit == Unit::Nanosecond && rounding_increment == 1 {
        return Ok(diff);
    }
    let date_time = (y1, m1, d1, h1, min1, s1, ms1, us1, ns1_rest);
    round_relative_duration(
        &mut diff,
        ns1,
        ns2,
        date_time,
        Some(tz),
        largest_unit,
        rounding_increment,
        smallest_unit,
        rounding_mode,
    )?;
    Ok(diff)
}

/// spec 7.5.38 RoundRelativeDuration.
#[allow(clippy::too_many_arguments)]
pub fn round_relative_duration(
    duration: &mut super::InternalDuration,
    origin_epoch_ns: i128,
    dest_epoch_ns: i128,
    iso_date_time: IsoDateTime,
    time_zone: Option<&str>,
    largest_unit: Unit,
    increment: i64,
    smallest_unit: Unit,
    rounding_mode: RoundingMode,
) -> Result<(), JsError> {
    let irregular =
        iso::is_calendar_unit(smallest_unit) || (time_zone.is_some() && smallest_unit == Unit::Day);
    let sign = internal_duration_sign(duration);
    let (nudged, did_expand, nudged_ns) = if irregular {
        nudge_to_calendar_unit(
            sign,
            *duration,
            origin_epoch_ns,
            dest_epoch_ns,
            iso_date_time,
            time_zone,
            increment,
            smallest_unit,
            rounding_mode,
        )?
    } else if let Some(tz) = time_zone {
        nudge_to_zoned_time(
            sign,
            *duration,
            iso_date_time,
            tz,
            increment,
            smallest_unit,
            rounding_mode,
        )?
    } else {
        nudge_to_day_or_time(
            *duration,
            dest_epoch_ns,
            largest_unit,
            increment,
            smallest_unit,
            rounding_mode,
        )?
    };
    *duration = nudged;
    if did_expand && smallest_unit != Unit::Week {
        let start_unit = iso::larger_of_two_units(smallest_unit, Unit::Day);
        bubble_relative_duration(
            sign,
            duration,
            nudged_ns,
            iso_date_time,
            time_zone,
            largest_unit,
            start_unit,
        )?;
    }
    Ok(())
}

fn internal_duration_sign(d: &super::InternalDuration) -> i64 {
    for v in d.date {
        if v < 0.0 {
            return -1;
        }
        if v > 0.0 {
            return 1;
        }
    }
    if d.time < 0 {
        -1
    } else if d.time > 0 {
        1
    } else {
        0
    }
}

/// spec 7.5.36 NudgeToDayOrTime.
fn nudge_to_day_or_time(
    duration: super::InternalDuration,
    dest_epoch_ns: i128,
    largest_unit: Unit,
    increment: i64,
    smallest_unit: Unit,
    rounding_mode: RoundingMode,
) -> Result<(super::InternalDuration, bool, i128), JsError> {
    let time_duration =
        super::add_24_hour_days_to_time_duration(duration.time, duration.date[3] as i128)?;
    let unit_length = smallest_unit.length_ns().unwrap();
    let rounded_time = super::round_time_duration(
        time_duration,
        increment * unit_length as i64,
        Unit::Nanosecond,
        rounding_mode,
    )?;
    let diff_time = super::add_time_duration(rounded_time, -time_duration)?;
    let whole_days = time_duration / iso::NS_PER_DAY;
    let rounded_whole_days = rounded_time / iso::NS_PER_DAY;
    let day_delta = rounded_whole_days - whole_days;
    let day_delta_sign = day_delta.signum();
    let did_expand = day_delta_sign == time_duration.signum() && time_duration != 0;
    let nudged_ns = dest_epoch_ns + diff_time;
    let mut days = 0f64;
    let mut remainder = rounded_time;
    if largest_unit.category() == iso::Category::Date {
        days = rounded_whole_days as f64;
        remainder -= rounded_whole_days * iso::NS_PER_DAY;
    }
    let date = [duration.date[0], duration.date[1], duration.date[2], days];
    Ok((
        super::InternalDuration {
            date,
            time: remainder,
        },
        did_expand,
        nudged_ns,
    ))
}

/// spec 7.5.33 ComputeNudgeWindow.
#[allow(clippy::too_many_arguments)]
fn compute_nudge_window(
    sign: i64,
    duration: super::InternalDuration,
    origin_epoch_ns: i128,
    iso_date_time: IsoDateTime,
    time_zone: Option<&str>,
    increment: i64,
    unit: Unit,
    additional_shift: bool,
) -> Result<Window, JsError> {
    let (r1, r2, start_date, end_date) = match unit {
        Unit::Year => {
            let years = iso::round_number_to_increment(
                duration.date[0] as i128,
                increment as i128,
                RoundingMode::Trunc,
            );
            let r1 = if additional_shift {
                years + increment as i128 * sign as i128
            } else {
                years
            };
            let r2 = r1 + increment as i128 * sign as i128;
            (
                r1,
                r2,
                [r1 as f64, 0.0, 0.0, 0.0],
                [r2 as f64, 0.0, 0.0, 0.0],
            )
        }
        Unit::Month => {
            let months = iso::round_number_to_increment(
                duration.date[1] as i128,
                increment as i128,
                RoundingMode::Trunc,
            );
            let r1 = if additional_shift {
                months + increment as i128 * sign as i128
            } else {
                months
            };
            let r2 = r1 + increment as i128 * sign as i128;
            (
                r1,
                r2,
                [duration.date[0], r1 as f64, 0.0, 0.0],
                [duration.date[0], r2 as f64, 0.0, 0.0],
            )
        }
        Unit::Week => {
            let years_months = [duration.date[0], duration.date[1], 0.0, 0.0];
            let weeks_start = iso::calendar_date_add(
                iso_date_time.0,
                iso_date_time.1,
                iso_date_time.2,
                years_months[0] as i64,
                years_months[1] as i64,
                0,
                0,
                true,
            )
            .ok_or_else(|| JsError::new(ErrorKind::RangeError, "date out of range".into()))?;
            let weeks_end = iso::add_days_to_iso_date(
                weeks_start.0,
                weeks_start.1,
                weeks_start.2,
                duration.date[3] as i64,
            );
            let until = iso::calendar_date_until(weeks_start, weeks_end, Unit::Week);
            let weeks = iso::round_number_to_increment(
                duration.date[2] as i128 + until.2 as i128,
                increment as i128,
                RoundingMode::Trunc,
            );
            let r1 = weeks;
            let r2 = weeks + increment as i128 * sign as i128;
            (
                r1,
                r2,
                [duration.date[0], duration.date[1], r1 as f64, 0.0],
                [duration.date[0], duration.date[1], r2 as f64, 0.0],
            )
        }
        _ => {
            let days = iso::round_number_to_increment(
                duration.date[3] as i128,
                increment as i128,
                RoundingMode::Trunc,
            );
            let r1 = days;
            let r2 = days + increment as i128 * sign as i128;
            (
                r1,
                r2,
                [
                    duration.date[0],
                    duration.date[1],
                    duration.date[2],
                    r1 as f64,
                ],
                [
                    duration.date[0],
                    duration.date[1],
                    duration.date[2],
                    r2 as f64,
                ],
            )
        }
    };
    let start_epoch_ns = if r1 == 0 {
        origin_epoch_ns
    } else {
        let start = iso::calendar_date_add(
            iso_date_time.0,
            iso_date_time.1,
            iso_date_time.2,
            start_date[0] as i64,
            start_date[1] as i64,
            start_date[2] as i64,
            start_date[3] as i64,
            true,
        )
        .ok_or_else(|| JsError::new(ErrorKind::RangeError, "date out of range".into()))?;
        let dt = (
            start.0,
            start.1,
            start.2,
            iso_date_time.3,
            iso_date_time.4,
            iso_date_time.5,
            iso_date_time.6,
            iso_date_time.7,
            iso_date_time.8,
        );
        date_time_epoch(dt, time_zone)?
    };
    let end = iso::calendar_date_add(
        iso_date_time.0,
        iso_date_time.1,
        iso_date_time.2,
        end_date[0] as i64,
        end_date[1] as i64,
        end_date[2] as i64,
        end_date[3] as i64,
        true,
    )
    .ok_or_else(|| JsError::new(ErrorKind::RangeError, "date out of range".into()))?;
    let end_dt = (
        end.0,
        end.1,
        end.2,
        iso_date_time.3,
        iso_date_time.4,
        iso_date_time.5,
        iso_date_time.6,
        iso_date_time.7,
        iso_date_time.8,
    );
    let end_epoch_ns = date_time_epoch(end_dt, time_zone)?;
    Ok(Window {
        r1,
        r2,
        start_epoch_ns,
        end_epoch_ns,
        start_duration: super::InternalDuration {
            date: start_date,
            time: 0,
        },
        end_duration: super::InternalDuration {
            date: end_date,
            time: 0,
        },
    })
}

struct Window {
    r1: i128,
    r2: i128,
    start_epoch_ns: i128,
    end_epoch_ns: i128,
    start_duration: super::InternalDuration,
    end_duration: super::InternalDuration,
}

/// The epoch nanoseconds of a wall-clock date-time in a fixed-offset zone
/// (`None` = UTC).
fn date_time_epoch(dt: IsoDateTime, time_zone: Option<&str>) -> Result<i128, JsError> {
    let utc = iso::get_utc_epoch_nanoseconds(dt.0, dt.1, dt.2, dt.3, dt.4, dt.5, dt.6, dt.7, dt.8);
    match time_zone {
        Some(tz) => {
            let offset = super::offset_time_zone_offset_ns(tz).ok_or_else(|| {
                JsError::new(ErrorKind::RangeError, "unsupported time zone".into())
            })?;
            Ok(utc - offset)
        }
        None => Ok(utc),
    }
}

/// spec 7.5.34 NudgeToCalendarUnit.
#[allow(clippy::too_many_arguments)]
fn nudge_to_calendar_unit(
    sign: i64,
    duration: super::InternalDuration,
    origin_epoch_ns: i128,
    dest_epoch_ns: i128,
    iso_date_time: IsoDateTime,
    time_zone: Option<&str>,
    increment: i64,
    unit: Unit,
    rounding_mode: RoundingMode,
) -> Result<(super::InternalDuration, bool, i128), JsError> {
    let mut did_expand = false;
    let mut window = compute_nudge_window(
        sign,
        duration,
        origin_epoch_ns,
        iso_date_time,
        time_zone,
        increment,
        unit,
        false,
    )?;
    let inside = if sign == 1 {
        window.start_epoch_ns <= dest_epoch_ns && dest_epoch_ns <= window.end_epoch_ns
    } else {
        window.end_epoch_ns <= dest_epoch_ns && dest_epoch_ns <= window.start_epoch_ns
    };
    if !inside {
        window = compute_nudge_window(
            sign,
            duration,
            origin_epoch_ns,
            iso_date_time,
            time_zone,
            increment,
            unit,
            true,
        )?;
        did_expand = true;
    }
    let r1 = window.r1;
    let r2 = window.r2;
    let start_epoch_ns = window.start_epoch_ns;
    let end_epoch_ns = window.end_epoch_ns;
    let start_duration = window.start_duration;
    let end_duration = window.end_duration;
    // progress = (dest - start) / (end - start) as an exact rational.
    let (num, den) = if end_epoch_ns != start_epoch_ns {
        (
            (dest_epoch_ns - start_epoch_ns),
            (end_epoch_ns - start_epoch_ns),
        )
    } else {
        (0, 1)
    };
    // total = r1 + progress × increment × sign  (a rational; the fixtures
    // compare exact values, so keep numerator/denominator).
    let total_num = r1 * den + num * (increment as i128) * sign as i128;
    let total_den = den;
    let is_negative = sign < 0;
    let unsigned = unsigned_rounding_mode(rounding_mode, is_negative);
    // roundedUnit = |total| rounded to |r1| or |r2|.
    let (abs_r1, abs_r2) = (r1.abs(), r2.abs());
    let total_abs_num = total_num.abs();
    let total_abs_den = total_den;
    let cmp = total_abs_num.cmp(&(abs_r2 * total_abs_den));
    let progress_cmp = match cmp {
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
        std::cmp::Ordering::Less => -1,
    };
    let rounded_unit = if progress_cmp == 0 {
        abs_r2
    } else {
        let (d1_num, d1_den) = diff_frac(total_abs_num, total_abs_den, abs_r1);
        let (d2_num, d2_den) = diff_frac2(abs_r2, total_abs_num, total_abs_den);
        // apply_unsigned_frac returns 1 when r2 wins, 0 when r1 wins.
        if apply_unsigned_frac(d1_num, d1_den, d2_num, d2_den, unsigned) == 1 {
            abs_r2
        } else {
            abs_r1
        }
    };
    if rounded_unit == abs_r2 {
        did_expand = true;
        Ok((end_duration, did_expand, end_epoch_ns))
    } else {
        Ok((start_duration, did_expand, start_epoch_ns))
    }
}

/// |a/b - c| and |c - a/b| as fractions.
fn diff_frac(a_num: i128, a_den: i128, c: i128) -> (i128, i128) {
    // |a/b - c| = |a - c*b| / b
    ((a_num - c * a_den).abs(), a_den)
}

fn diff_frac2(a: i128, b_num: i128, b_den: i128) -> (i128, i128) {
    // |a - b_num/b_den| = |a*b_den - b_num| / b_den
    ((a * b_den - b_num).abs(), b_den)
}

fn apply_unsigned_frac(
    d1_num: i128,
    d1_den: i128,
    d2_num: i128,
    d2_den: i128,
    mode: Unsigned,
) -> i128 {
    // Returns 1 when r2 wins, 0 when r1 wins.
    match mode {
        Unsigned::Zero => 0,
        Unsigned::Infinity => 1,
        Unsigned::HalfZero | Unsigned::HalfInfinity | Unsigned::HalfEven => {
            let cross = d1_num * d2_den;
            let cross2 = d2_num * d1_den;
            if cross < cross2 {
                0
            } else if cross2 < cross {
                1
            } else {
                match mode {
                    Unsigned::HalfInfinity => 1,
                    _ => 0,
                }
            }
        }
    }
}

enum Unsigned {
    Zero,
    Infinity,
    HalfZero,
    HalfInfinity,
    HalfEven,
}

fn unsigned_rounding_mode(mode: RoundingMode, negative: bool) -> Unsigned {
    match mode {
        RoundingMode::Ceil => {
            if negative {
                Unsigned::Zero
            } else {
                Unsigned::Infinity
            }
        }
        RoundingMode::Floor => {
            if negative {
                Unsigned::Infinity
            } else {
                Unsigned::Zero
            }
        }
        RoundingMode::Expand => Unsigned::Infinity,
        RoundingMode::Trunc => Unsigned::Zero,
        RoundingMode::HalfCeil => {
            if negative {
                Unsigned::HalfZero
            } else {
                Unsigned::HalfInfinity
            }
        }
        RoundingMode::HalfFloor => {
            if negative {
                Unsigned::HalfInfinity
            } else {
                Unsigned::HalfZero
            }
        }
        RoundingMode::HalfExpand => Unsigned::HalfInfinity,
        RoundingMode::HalfTrunc => Unsigned::HalfZero,
        RoundingMode::HalfEven => Unsigned::HalfEven,
    }
}

/// spec 7.5.35 NudgeToZonedTime (fixed-offset zones).
fn nudge_to_zoned_time(
    sign: i64,
    duration: super::InternalDuration,
    iso_date_time: IsoDateTime,
    tz: &str,
    increment: i64,
    unit: Unit,
    rounding_mode: RoundingMode,
) -> Result<(super::InternalDuration, bool, i128), JsError> {
    let offset = super::offset_time_zone_offset_ns(tz)
        .ok_or_else(|| JsError::new(ErrorKind::RangeError, "unsupported time zone".into()))?;
    let start = iso::calendar_date_add(
        iso_date_time.0,
        iso_date_time.1,
        iso_date_time.2,
        duration.date[0] as i64,
        duration.date[1] as i64,
        duration.date[2] as i64,
        duration.date[3] as i64,
        true,
    )
    .ok_or_else(|| JsError::new(ErrorKind::RangeError, "date out of range".into()))?;
    let start_epoch_ns = iso::get_utc_epoch_nanoseconds(
        start.0,
        start.1,
        start.2,
        iso_date_time.3,
        iso_date_time.4,
        iso_date_time.5,
        iso_date_time.6,
        iso_date_time.7,
        iso_date_time.8,
    ) - offset;
    let end_date = iso::add_days_to_iso_date(start.0, start.1, start.2, sign);
    let end_epoch_ns = iso::get_utc_epoch_nanoseconds(
        end_date.0,
        end_date.1,
        end_date.2,
        iso_date_time.3,
        iso_date_time.4,
        iso_date_time.5,
        iso_date_time.6,
        iso_date_time.7,
        iso_date_time.8,
    ) - offset;
    let day_span = end_epoch_ns - start_epoch_ns;
    let unit_length = unit.length_ns().unwrap();
    let mut rounded = super::round_time_duration(
        duration.time,
        increment * unit_length as i64,
        Unit::Nanosecond,
        rounding_mode,
    )?;
    let beyond = rounded - day_span;
    let (did_round_beyond_day, day_delta, nudged) = if beyond.signum() != -(sign as i128) {
        let day_delta = sign;
        rounded = super::round_time_duration(
            beyond,
            increment * unit_length as i64,
            Unit::Nanosecond,
            rounding_mode,
        )?;
        (true, day_delta, end_epoch_ns + rounded)
    } else {
        (false, 0, start_epoch_ns + rounded)
    };
    let date = [
        duration.date[0],
        duration.date[1],
        duration.date[2],
        duration.date[3] + day_delta as f64,
    ];
    Ok((
        super::InternalDuration {
            date,
            time: rounded,
        },
        did_round_beyond_day,
        nudged,
    ))
}

/// spec 7.5.37 BubbleRelativeDuration.
fn bubble_relative_duration(
    sign: i64,
    duration: &mut super::InternalDuration,
    nudged_epoch_ns: i128,
    iso_date_time: IsoDateTime,
    time_zone: Option<&str>,
    largest_unit: Unit,
    smallest_unit: Unit,
) -> Result<(), JsError> {
    if smallest_unit == largest_unit {
        return Ok(());
    }
    let largest_idx = largest_unit.ordinal();
    let mut unit_idx = smallest_unit.ordinal() - 1;
    let mut done = false;
    while unit_idx >= largest_idx && !done {
        let unit = match unit_idx {
            0 => Unit::Year,
            1 => Unit::Month,
            2 => Unit::Week,
            _ => break,
        };
        if unit != Unit::Week || largest_unit == Unit::Week {
            let end_duration = match unit {
                Unit::Year => [duration.date[0] + sign as f64, 0.0, 0.0, 0.0],
                Unit::Month => [duration.date[0], duration.date[1] + sign as f64, 0.0, 0.0],
                _ => [
                    duration.date[0],
                    duration.date[1],
                    duration.date[2] + sign as f64,
                    0.0,
                ],
            };
            let end = iso::calendar_date_add(
                iso_date_time.0,
                iso_date_time.1,
                iso_date_time.2,
                end_duration[0] as i64,
                end_duration[1] as i64,
                end_duration[2] as i64,
                end_duration[3] as i64,
                true,
            )
            .ok_or_else(|| JsError::new(ErrorKind::RangeError, "date out of range".into()))?;
            let end_epoch = date_time_epoch(
                (
                    end.0,
                    end.1,
                    end.2,
                    iso_date_time.3,
                    iso_date_time.4,
                    iso_date_time.5,
                    iso_date_time.6,
                    iso_date_time.7,
                    iso_date_time.8,
                ),
                time_zone,
            )?;
            let beyond = nudged_epoch_ns - end_epoch;
            let beyond_sign = beyond.signum();
            if beyond_sign != -(sign as i128) {
                *duration = super::InternalDuration {
                    date: end_duration,
                    time: 0,
                };
            } else {
                done = true;
            }
        }
        if unit_idx == 0 {
            break;
        }
        unit_idx -= 1;
    }
    Ok(())
}

/// spec 7.5.39 TotalRelativeDuration for calendar units: NudgeToCalendarUnit
/// with roundingMode trunc, returning the `Total` (a mathematical value).
#[allow(clippy::too_many_arguments)]
pub fn nudge_to_calendar_unit_total(
    sign: i64,
    duration: super::InternalDuration,
    origin_epoch_ns: i128,
    dest_epoch_ns: i128,
    iso_date_time: IsoDateTime,
    time_zone: Option<&str>,
    increment: i64,
    unit: Unit,
) -> Result<f64, JsError> {
    let window = compute_nudge_window(
        sign,
        duration,
        origin_epoch_ns,
        iso_date_time,
        time_zone,
        increment,
        unit,
        false,
    )?;
    let inside = if sign == 1 {
        window.start_epoch_ns <= dest_epoch_ns && dest_epoch_ns <= window.end_epoch_ns
    } else {
        window.end_epoch_ns <= dest_epoch_ns && dest_epoch_ns <= window.start_epoch_ns
    };
    let window = if inside {
        window
    } else {
        compute_nudge_window(
            sign,
            duration,
            origin_epoch_ns,
            iso_date_time,
            time_zone,
            increment,
            unit,
            true,
        )?
    };
    let r1 = window.r1;
    let total = if window.end_epoch_ns != window.start_epoch_ns {
        // spec 7.5.34 note: express total as one quotient of exact integers
        // (r1 + progress × increment × sign = num / den) and divide once,
        // avoiding the double-rounding of a float progress.
        let den = window.end_epoch_ns - window.start_epoch_ns;
        let num =
            r1 * den + (dest_epoch_ns - window.start_epoch_ns) * increment as i128 * sign as i128;
        // The quotient's sign is num XOR den (den is negative for backward
        // windows).
        let negative = (num < 0) != (den < 0);
        let magnitude = super::divide_rounded(num.unsigned_abs(), den.unsigned_abs());
        if negative { -magnitude } else { magnitude }
    } else {
        r1 as f64
    };
    Ok(total)
}
