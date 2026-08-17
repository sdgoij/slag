//! Minimal `PlainDate`, `PlainTime`, `PlainDateTime`, and `ZonedDateTime`
//! (spec 3-6): constructors, basic field getters, `from`/`compare`, and
//! `toString`. The full clusters are out of scope; these shells serve the
//! `Temporal.Now`, `Instant.prototype.toZonedDateTimeISO`, and the
//! relative-to machinery of `Duration`.

use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::property::PropertyDescriptor;
use crux::string::JsString;
use crux::value::Value;

use crate::agent::Agent;
use crate::realm::Realm;

use super::iso::{self, FracPrecision};
use super::{
    RecordKind, TemporalRecord, create_temporal_object, install_constructor, placeholder,
    require_record,
};

const PLAIN_DATE: &str = "%Temporal.PlainDate%";
const PLAIN_DATE_PROTO: &str = "%Temporal.PlainDate.prototype%";
const PLAIN_TIME: &str = "%Temporal.PlainTime%";
const PLAIN_TIME_PROTO: &str = "%Temporal.PlainTime.prototype%";
const PLAIN_DATE_TIME: &str = "%Temporal.PlainDateTime%";
const PLAIN_DATE_TIME_PROTO: &str = "%Temporal.PlainDateTime.prototype%";
const ZONED: &str = "%Temporal.ZonedDateTime%";
const ZONED_PROTO: &str = "%Temporal.ZonedDateTime.prototype%";

/// Install the four Temporal type shells on `parent` (the Temporal object).
pub fn install(
    parent: &Handle<crux::object::JsObject>,
    realm: &Handle<Realm>,
) -> Result<(), JsError> {
    install_plain_date(parent, realm)?;
    install_plain_time(parent, realm)?;
    install_plain_date_time(parent, realm)?;
    install_zoned_date_time(parent, realm)?;
    Ok(())
}

fn statics(
    realm: &Handle<Realm>,
    ctor: &Handle<Function>,
    methods: &'static [(&'static str, &'static str, u64)],
) -> Result<(), JsError> {
    for (name, intrinsic, length) in methods {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            *length,
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
    Ok(())
}

fn proto_methods(
    realm: &Handle<Realm>,
    proto: &Handle<crux::object::JsObject>,
    methods: &'static [(&'static str, &'static str, u64)],
) -> Result<(), JsError> {
    for (name, intrinsic, length) in methods {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            *length,
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

fn proto_getters(
    realm: &Handle<Realm>,
    proto: &Handle<crux::object::JsObject>,
    getters: &'static [(&'static str, &'static str)],
) -> Result<(), JsError> {
    for (name, intrinsic) in getters {
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
    Ok(())
}

fn install_plain_date(
    parent: &Handle<crux::object::JsObject>,
    realm: &Handle<Realm>,
) -> Result<(), JsError> {
    let (ctor, proto) = install_constructor(
        realm,
        parent,
        "PlainDate",
        PLAIN_DATE,
        PLAIN_DATE_PROTO,
        3,
        "Temporal.PlainDate",
    )?;
    statics(
        realm,
        &ctor,
        &[
            ("from", "%Temporal.PlainDate.from%", 1),
            ("compare", "%Temporal.PlainDate.compare%", 2),
        ],
    )?;
    proto_getters(
        realm,
        &proto,
        &[
            ("year", "%Temporal.PlainDate.prototype.year%"),
            ("month", "%Temporal.PlainDate.prototype.month%"),
            ("monthCode", "%Temporal.PlainDate.prototype.monthCode%"),
            ("day", "%Temporal.PlainDate.prototype.day%"),
            ("dayOfWeek", "%Temporal.PlainDate.prototype.dayOfWeek%"),
            ("calendarId", "%Temporal.PlainDate.prototype.calendarId%"),
        ],
    )?;
    proto_methods(
        realm,
        &proto,
        &[
            ("add", "%Temporal.PlainDate.prototype.add%", 1),
            ("until", "%Temporal.PlainDate.prototype.until%", 1),
            ("equals", "%Temporal.PlainDate.prototype.equals%", 1),
            ("toString", "%Temporal.PlainDate.prototype.toString%", 0),
            ("toJSON", "%Temporal.PlainDate.prototype.toJSON%", 0),
            ("valueOf", "%Temporal.PlainDate.prototype.valueOf%", 0),
        ],
    )?;
    Ok(())
}

fn install_plain_time(
    parent: &Handle<crux::object::JsObject>,
    realm: &Handle<Realm>,
) -> Result<(), JsError> {
    let (ctor, proto) = install_constructor(
        realm,
        parent,
        "PlainTime",
        PLAIN_TIME,
        PLAIN_TIME_PROTO,
        0,
        "Temporal.PlainTime",
    )?;
    statics(
        realm,
        &ctor,
        &[
            ("from", "%Temporal.PlainTime.from%", 1),
            ("compare", "%Temporal.PlainTime.compare%", 2),
        ],
    )?;
    proto_getters(
        realm,
        &proto,
        &[
            ("hour", "%Temporal.PlainTime.prototype.hour%"),
            ("minute", "%Temporal.PlainTime.prototype.minute%"),
            ("second", "%Temporal.PlainTime.prototype.second%"),
            ("millisecond", "%Temporal.PlainTime.prototype.millisecond%"),
            ("microsecond", "%Temporal.PlainTime.prototype.microsecond%"),
            ("nanosecond", "%Temporal.PlainTime.prototype.nanosecond%"),
        ],
    )?;
    proto_methods(
        realm,
        &proto,
        &[
            ("add", "%Temporal.PlainTime.prototype.add%", 1),
            ("toString", "%Temporal.PlainTime.prototype.toString%", 0),
            ("toJSON", "%Temporal.PlainTime.prototype.toJSON%", 0),
            ("valueOf", "%Temporal.PlainTime.prototype.valueOf%", 0),
        ],
    )?;
    Ok(())
}

fn install_plain_date_time(
    parent: &Handle<crux::object::JsObject>,
    realm: &Handle<Realm>,
) -> Result<(), JsError> {
    let (ctor, proto) = install_constructor(
        realm,
        parent,
        "PlainDateTime",
        PLAIN_DATE_TIME,
        PLAIN_DATE_TIME_PROTO,
        3,
        "Temporal.PlainDateTime",
    )?;
    statics(
        realm,
        &ctor,
        &[
            ("from", "%Temporal.PlainDateTime.from%", 1),
            ("compare", "%Temporal.PlainDateTime.compare%", 2),
        ],
    )?;
    proto_getters(
        realm,
        &proto,
        &[
            ("year", "%Temporal.PlainDateTime.prototype.year%"),
            ("month", "%Temporal.PlainDateTime.prototype.month%"),
            ("monthCode", "%Temporal.PlainDateTime.prototype.monthCode%"),
            ("day", "%Temporal.PlainDateTime.prototype.day%"),
            ("hour", "%Temporal.PlainDateTime.prototype.hour%"),
            ("minute", "%Temporal.PlainDateTime.prototype.minute%"),
            ("second", "%Temporal.PlainDateTime.prototype.second%"),
            (
                "millisecond",
                "%Temporal.PlainDateTime.prototype.millisecond%",
            ),
            (
                "microsecond",
                "%Temporal.PlainDateTime.prototype.microsecond%",
            ),
            (
                "nanosecond",
                "%Temporal.PlainDateTime.prototype.nanosecond%",
            ),
            (
                "calendarId",
                "%Temporal.PlainDateTime.prototype.calendarId%",
            ),
        ],
    )?;
    proto_methods(
        realm,
        &proto,
        &[
            ("toString", "%Temporal.PlainDateTime.prototype.toString%", 0),
            ("toJSON", "%Temporal.PlainDateTime.prototype.toJSON%", 0),
            ("valueOf", "%Temporal.PlainDateTime.prototype.valueOf%", 0),
        ],
    )?;
    Ok(())
}

fn install_zoned_date_time(
    parent: &Handle<crux::object::JsObject>,
    realm: &Handle<Realm>,
) -> Result<(), JsError> {
    let (ctor, proto) = install_constructor(
        realm,
        parent,
        "ZonedDateTime",
        ZONED,
        ZONED_PROTO,
        2,
        "Temporal.ZonedDateTime",
    )?;
    statics(
        realm,
        &ctor,
        &[
            ("from", "%Temporal.ZonedDateTime.from%", 1),
            ("compare", "%Temporal.ZonedDateTime.compare%", 2),
        ],
    )?;
    proto_getters(
        realm,
        &proto,
        &[
            (
                "epochNanoseconds",
                "%Temporal.ZonedDateTime.prototype.epochNanoseconds%",
            ),
            (
                "epochMilliseconds",
                "%Temporal.ZonedDateTime.prototype.epochMilliseconds%",
            ),
            (
                "timeZoneId",
                "%Temporal.ZonedDateTime.prototype.timeZoneId%",
            ),
            (
                "calendarId",
                "%Temporal.ZonedDateTime.prototype.calendarId%",
            ),
            ("year", "%Temporal.ZonedDateTime.prototype.year%"),
            ("month", "%Temporal.ZonedDateTime.prototype.month%"),
            ("day", "%Temporal.ZonedDateTime.prototype.day%"),
            ("hour", "%Temporal.ZonedDateTime.prototype.hour%"),
            ("minute", "%Temporal.ZonedDateTime.prototype.minute%"),
            ("second", "%Temporal.ZonedDateTime.prototype.second%"),
            (
                "millisecond",
                "%Temporal.ZonedDateTime.prototype.millisecond%",
            ),
            (
                "microsecond",
                "%Temporal.ZonedDateTime.prototype.microsecond%",
            ),
            (
                "nanosecond",
                "%Temporal.ZonedDateTime.prototype.nanosecond%",
            ),
            ("offset", "%Temporal.ZonedDateTime.prototype.offset%"),
            (
                "offsetNanoseconds",
                "%Temporal.ZonedDateTime.prototype.offsetNanoseconds%",
            ),
        ],
    )?;
    proto_methods(
        realm,
        &proto,
        &[
            (
                "toInstant",
                "%Temporal.ZonedDateTime.prototype.toInstant%",
                0,
            ),
            ("toString", "%Temporal.ZonedDateTime.prototype.toString%", 0),
            ("toJSON", "%Temporal.ZonedDateTime.prototype.toJSON%", 0),
            ("valueOf", "%Temporal.ZonedDateTime.prototype.valueOf%", 0),
        ],
    )?;
    Ok(())
}

/// A field-getter body helper: reads the record field and returns it as a
/// Number (or the given string for the string fields).
fn field_number(
    agent: &Agent,
    this: &Value,
    kind: RecordKind,
    index: usize,
) -> Result<Value, JsError> {
    let record = require_record(agent, this, kind)?;
    let value = match (&record, kind) {
        (TemporalRecord::PlainDate(d), RecordKind::PlainDate) => d[index],
        (TemporalRecord::PlainTime(t), RecordKind::PlainTime) => t[index],
        (TemporalRecord::PlainDateTime(dt), RecordKind::PlainDateTime) => dt[index],
        _ => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "brand check failed".into(),
            ));
        }
    };
    Ok(Value::Number(value as f64))
}

pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    // Constructors are not callable.
    for (name, key) in [
        ("PlainDate", PLAIN_DATE),
        ("PlainTime", PLAIN_TIME),
        ("PlainDateTime", PLAIN_DATE_TIME),
        ("ZonedDateTime", ZONED),
    ] {
        if intrinsics.get(key).as_ref() == Some(callee) {
            return Some(Err(JsError::new(
                ErrorKind::TypeError,
                format!("Temporal.{name} cannot be called as a function"),
            )));
        }
    }
    // Field getters.
    let field = |name: &str| intrinsics.get(name).as_ref() == Some(callee);
    if field("%Temporal.PlainDate.prototype.year%") {
        return Some(field_number(agent, this, RecordKind::PlainDate, 0));
    }
    if field("%Temporal.PlainDate.prototype.month%") {
        return Some(field_number(agent, this, RecordKind::PlainDate, 1));
    }
    if field("%Temporal.PlainDate.prototype.day%") {
        return Some(field_number(agent, this, RecordKind::PlainDate, 2));
    }
    if field("%Temporal.PlainDate.prototype.dayOfWeek%") {
        return Some(plain_date_day_of_week(agent, this));
    }
    if field("%Temporal.PlainDate.prototype.monthCode%") {
        return Some(month_code(agent, this, RecordKind::PlainDate, 1));
    }
    if field("%Temporal.PlainDateTime.prototype.monthCode%") {
        return Some(month_code(agent, this, RecordKind::PlainDateTime, 1));
    }
    if field("%Temporal.PlainTime.prototype.hour%") {
        return Some(field_number(agent, this, RecordKind::PlainTime, 0));
    }
    if field("%Temporal.PlainTime.prototype.minute%") {
        return Some(field_number(agent, this, RecordKind::PlainTime, 1));
    }
    if field("%Temporal.PlainTime.prototype.second%") {
        return Some(field_number(agent, this, RecordKind::PlainTime, 2));
    }
    if field("%Temporal.PlainTime.prototype.millisecond%") {
        return Some(field_number(agent, this, RecordKind::PlainTime, 3));
    }
    if field("%Temporal.PlainTime.prototype.microsecond%") {
        return Some(field_number(agent, this, RecordKind::PlainTime, 4));
    }
    if field("%Temporal.PlainTime.prototype.nanosecond%") {
        return Some(field_number(agent, this, RecordKind::PlainTime, 5));
    }
    if field("%Temporal.PlainDateTime.prototype.calendarId%") {
        return Some(calendar_id(agent, this, RecordKind::PlainDateTime));
    }
    if field("%Temporal.PlainDate.prototype.calendarId%") {
        return Some(calendar_id(agent, this, RecordKind::PlainDate));
    }
    if field("%Temporal.ZonedDateTime.prototype.calendarId%") {
        return Some(calendar_id(agent, this, RecordKind::ZonedDateTime));
    }
    if field("%Temporal.ZonedDateTime.prototype.timeZoneId%") {
        return Some(zoned_time_zone_id(agent, this));
    }
    if field("%Temporal.ZonedDateTime.prototype.epochNanoseconds%") {
        return Some(zoned_epoch_ns(agent, this));
    }
    if field("%Temporal.ZonedDateTime.prototype.epochMilliseconds%") {
        return Some(zoned_epoch_ms(agent, this));
    }
    for (idx, name) in [
        (0, "year"),
        (1, "month"),
        (2, "day"),
        (3, "hour"),
        (4, "minute"),
        (5, "second"),
        (6, "millisecond"),
        (7, "microsecond"),
        (8, "nanosecond"),
    ] {
        if field(&format!("%Temporal.ZonedDateTime.prototype.{name}%")) {
            return Some(zoned_field(agent, this, idx));
        }
    }
    if field("%Temporal.ZonedDateTime.prototype.offset%") {
        return Some(zoned_offset(agent, this));
    }
    if field("%Temporal.ZonedDateTime.prototype.offsetNanoseconds%") {
        return Some(zoned_offset_ns(agent, this));
    }
    for (idx, name) in [
        (0, "year"),
        (1, "month"),
        (2, "day"),
        (3, "hour"),
        (4, "minute"),
        (5, "second"),
        (6, "millisecond"),
        (7, "microsecond"),
        (8, "nanosecond"),
    ] {
        if field(&format!("%Temporal.PlainDateTime.prototype.{name}%")) {
            return Some(field_number(agent, this, RecordKind::PlainDateTime, idx));
        }
    }
    // Statics.
    if intrinsics.get("%Temporal.PlainDate.from%").as_ref() == Some(callee) {
        let item = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(to_plain_date(agent, &item));
    }
    if intrinsics.get("%Temporal.PlainTime.from%").as_ref() == Some(callee) {
        let item = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(to_plain_time(agent, &item));
    }
    if intrinsics.get("%Temporal.PlainDateTime.from%").as_ref() == Some(callee) {
        let item = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(to_plain_date_time(agent, &item));
    }
    if intrinsics.get("%Temporal.ZonedDateTime.from%").as_ref() == Some(callee) {
        let item = args.first().cloned().unwrap_or(Value::Undefined);
        return Some(to_zoned(agent, &item));
    }
    if intrinsics.get("%Temporal.PlainDate.compare%").as_ref() == Some(callee) {
        return Some(compare_records(
            agent,
            args,
            RecordKind::PlainDate,
            plain_compare_key,
        ));
    }
    if intrinsics.get("%Temporal.PlainTime.compare%").as_ref() == Some(callee) {
        return Some(compare_records(
            agent,
            args,
            RecordKind::PlainTime,
            time_compare_key,
        ));
    }
    if intrinsics.get("%Temporal.PlainDateTime.compare%").as_ref() == Some(callee) {
        return Some(compare_records(
            agent,
            args,
            RecordKind::PlainDateTime,
            date_time_compare_key,
        ));
    }
    if intrinsics.get("%Temporal.ZonedDateTime.compare%").as_ref() == Some(callee) {
        return Some(compare_records(
            agent,
            args,
            RecordKind::ZonedDateTime,
            zoned_compare_key,
        ));
    }
    // Prototype methods.
    if field("%Temporal.PlainDate.prototype.toString%") {
        return Some(plain_date_to_string(agent, this));
    }
    if field("%Temporal.PlainDate.prototype.toJSON%") {
        return Some(plain_date_to_string(agent, this));
    }
    if field("%Temporal.PlainTime.prototype.toString%") {
        return Some(plain_time_to_string(agent, this));
    }
    if field("%Temporal.PlainTime.prototype.toJSON%") {
        return Some(plain_time_to_string(agent, this));
    }
    if field("%Temporal.PlainDateTime.prototype.toString%") {
        return Some(plain_date_time_to_string(agent, this));
    }
    if field("%Temporal.PlainDateTime.prototype.toJSON%") {
        return Some(plain_date_time_to_string(agent, this));
    }
    if field("%Temporal.ZonedDateTime.prototype.toString%") {
        return Some(zoned_to_string(agent, this));
    }
    if field("%Temporal.ZonedDateTime.prototype.toJSON%") {
        return Some(zoned_to_string(agent, this));
    }
    if field("%Temporal.ZonedDateTime.prototype.toInstant%") {
        return Some(zoned_to_instant(agent, this));
    }
    for (name, key) in [
        ("PlainDate", "%Temporal.PlainDate.prototype.valueOf%"),
        ("PlainTime", "%Temporal.PlainTime.prototype.valueOf%"),
        (
            "PlainDateTime",
            "%Temporal.PlainDateTime.prototype.valueOf%",
        ),
        (
            "ZonedDateTime",
            "%Temporal.ZonedDateTime.prototype.valueOf%",
        ),
    ] {
        if intrinsics.get(key).as_ref() == Some(callee) {
            return Some(Err(JsError::new(
                ErrorKind::TypeError,
                format!("Temporal.{name}.prototype.valueOf throws"),
            )));
        }
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
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(PLAIN_DATE).as_ref() == Some(callee) {
        return Some(construct_plain_date(agent, args, new_target));
    }
    if intrinsics.get(PLAIN_TIME).as_ref() == Some(callee) {
        return Some(construct_plain_time(agent, args, new_target));
    }
    if intrinsics.get(PLAIN_DATE_TIME).as_ref() == Some(callee) {
        return Some(construct_plain_date_time(agent, args, new_target));
    }
    if intrinsics.get(ZONED).as_ref() == Some(callee) {
        return Some(construct_zoned(agent, args, new_target));
    }
    None
}

fn check_calendar(agent: &mut Agent, value: &Value) -> Result<(), JsError> {
    if matches!(value, Value::Undefined) {
        return Ok(());
    }
    let text = crate::context::to_string(agent, value)?;
    if !text.to_string_lossy().eq_ignore_ascii_case("iso8601") {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "only the iso8601 calendar is supported".into(),
        ));
    }
    Ok(())
}

/// spec 3.1.1 Temporal.PlainDate.
fn construct_plain_date(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    let y = super::to_integer_with_truncation(agent, args.first().unwrap_or(&Value::Undefined))?;
    let m = super::to_integer_with_truncation(agent, args.get(1).unwrap_or(&Value::Undefined))?;
    let d = super::to_integer_with_truncation(agent, args.get(2).unwrap_or(&Value::Undefined))?;
    check_calendar(agent, args.get(3).unwrap_or(&Value::Undefined))?;
    if !iso::is_valid_iso_date(y, m, d) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "invalid ISO date".into(),
        ));
    }
    create_plain_date(agent, (y, m, d), new_target)
}

pub fn create_plain_date(
    agent: &mut Agent,
    date: (i64, i64, i64),
    new_target: &Value,
) -> Result<Value, JsError> {
    let days = iso::iso_date_to_epoch_days(date.0, date.1 - 1, date.2);
    if days.abs() > 100_000_001 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "date out of range".into(),
        ));
    }
    create_temporal_object(
        agent,
        new_target,
        PLAIN_DATE_PROTO,
        TemporalRecord::PlainDate([date.0, date.1, date.2]),
    )
}

/// spec 4.1.1 Temporal.PlainTime.
fn construct_plain_time(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    let mut t = [0i64; 6];
    for (i, value) in args.iter().take(6).enumerate() {
        if !matches!(value, Value::Undefined) {
            t[i] = super::to_integer_with_truncation(agent, value)?;
        }
    }
    if !is_valid_time(t) {
        return Err(JsError::new(ErrorKind::RangeError, "invalid time".into()));
    }
    create_temporal_object(
        agent,
        new_target,
        PLAIN_TIME_PROTO,
        TemporalRecord::PlainTime(t),
    )
}

pub fn create_plain_time(
    agent: &mut Agent,
    t: [i64; 6],
    new_target: &Value,
) -> Result<Value, JsError> {
    create_temporal_object(
        agent,
        new_target,
        PLAIN_TIME_PROTO,
        TemporalRecord::PlainTime(t),
    )
}

/// spec 5.1.1 Temporal.PlainDateTime.
fn construct_plain_date_time(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    let y = super::to_integer_with_truncation(agent, args.first().unwrap_or(&Value::Undefined))?;
    let m = super::to_integer_with_truncation(agent, args.get(1).unwrap_or(&Value::Undefined))?;
    let d = super::to_integer_with_truncation(agent, args.get(2).unwrap_or(&Value::Undefined))?;
    let mut t = [0i64; 6];
    for (i, value) in args.iter().skip(3).take(6).enumerate() {
        if !matches!(value, Value::Undefined) {
            t[i] = super::to_integer_with_truncation(agent, value)?;
        }
    }
    check_calendar(agent, args.get(9).unwrap_or(&Value::Undefined))?;
    if !iso::is_valid_iso_date(y, m, d) || !is_valid_time(t) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "invalid date-time".into(),
        ));
    }
    create_temporal_object(
        agent,
        new_target,
        PLAIN_DATE_TIME_PROTO,
        TemporalRecord::PlainDateTime([y, m, d, t[0], t[1], t[2], t[3], t[4], t[5]]),
    )
}

/// spec 6.1.1 Temporal.ZonedDateTime.
fn construct_zoned(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    let ns_value = args.first().cloned().unwrap_or(Value::Undefined);
    let bigint = crate::context::to_big_int(agent, &ns_value)?;
    let ns = iso::bigint_to_epoch_ns(&bigint)
        .filter(|ns| (iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(ns))
        .ok_or_else(|| {
            JsError::new(
                ErrorKind::RangeError,
                "epoch nanoseconds out of range".into(),
            )
        })?;
    let tz_value = args.get(1).cloned().unwrap_or(Value::Undefined);
    let tz = super::instant::to_temporal_time_zone_identifier(agent, &tz_value)?;
    check_calendar(agent, args.get(2).unwrap_or(&Value::Undefined))?;
    create_temporal_object(
        agent,
        new_target,
        ZONED_PROTO,
        TemporalRecord::ZonedDateTime(ns, JsString::from_utf8(&tz)),
    )
}

fn is_valid_time(t: [i64; 6]) -> bool {
    let [h, m, s, ms, us, ns] = t;
    (0..=23).contains(&h)
        && (0..=59).contains(&m)
        && (0..=59).contains(&s)
        && (0..=999).contains(&ms)
        && (0..=999).contains(&us)
        && (0..=999).contains(&ns)
}

fn plain_date_day_of_week(agent: &Agent, this: &Value) -> Result<Value, JsError> {
    let [y, m, d, ..] = match require_record(agent, this, RecordKind::PlainDate)? {
        TemporalRecord::PlainDate(date) => date,
        _ => unreachable!(),
    };
    Ok(Value::Number(iso::iso_day_of_week(y, m, d) as f64))
}

fn month_code(
    agent: &Agent,
    this: &Value,
    kind: RecordKind,
    month_index: usize,
) -> Result<Value, JsError> {
    let month = match require_record(agent, this, kind)? {
        TemporalRecord::PlainDate(d) => d[month_index],
        TemporalRecord::PlainDateTime(dt) => dt[month_index],
        _ => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "brand check failed".into(),
            ));
        }
    };
    Ok(Value::String(Handle::new(JsString::from_utf8(&format!(
        "M{month:02}"
    )))))
}

fn calendar_id(agent: &Agent, this: &Value, kind: RecordKind) -> Result<Value, JsError> {
    require_record(agent, this, kind)?;
    Ok(Value::String(Handle::new(JsString::from_utf8("iso8601"))))
}

fn zoned_time_zone_id(agent: &Agent, this: &Value) -> Result<Value, JsError> {
    let (_, tz) = match require_record(agent, this, RecordKind::ZonedDateTime)? {
        TemporalRecord::ZonedDateTime(ns, tz) => (ns, tz),
        _ => unreachable!(),
    };
    Ok(Value::String(Handle::new(JsString::from_utf8(
        &tz.to_string_lossy(),
    ))))
}

fn zoned_epoch_ns(agent: &Agent, this: &Value) -> Result<Value, JsError> {
    let (ns, _) = match require_record(agent, this, RecordKind::ZonedDateTime)? {
        TemporalRecord::ZonedDateTime(ns, tz) => (ns, tz),
        _ => unreachable!(),
    };
    Ok(Value::BigInt(Handle::new(iso::epoch_ns_to_bigint(ns))))
}

fn zoned_epoch_ms(agent: &Agent, this: &Value) -> Result<Value, JsError> {
    let (ns, _) = match require_record(agent, this, RecordKind::ZonedDateTime)? {
        TemporalRecord::ZonedDateTime(ns, tz) => (ns, tz),
        _ => unreachable!(),
    };
    Ok(Value::Number(ns.div_euclid(1_000_000) as f64))
}

/// The local (y, m, d, h, min, s, ms, us, ns) of a ZonedDateTime.
#[allow(clippy::type_complexity)]
fn zoned_local(
    _agent: &Agent,
    ns: i128,
    tz: &str,
) -> Result<(i64, i64, i64, i64, i64, i64, i64, i64, i64), JsError> {
    let offset = super::offset_time_zone_offset_ns(tz)
        .ok_or_else(|| JsError::new(ErrorKind::RangeError, "unsupported time zone".into()))?;
    let (y, m, d, h, min, s, ms, us, n) = iso::iso_parts_from_epoch(ns);
    Ok(super::instant::balance_iso_date_time(
        y,
        m,
        d,
        h,
        min,
        s,
        ms,
        us,
        (n as i128 + offset) as i64,
    ))
}

fn zoned_field(agent: &mut Agent, this: &Value, index: usize) -> Result<Value, JsError> {
    let (ns, tz) = match require_record(agent, this, RecordKind::ZonedDateTime)? {
        TemporalRecord::ZonedDateTime(ns, tz) => (ns, tz.to_string_lossy()),
        _ => unreachable!(),
    };
    let local = zoned_local(agent, ns, &tz)?;
    let value = match index {
        0 => local.0,
        1 => local.1,
        2 => local.2,
        3 => local.3,
        4 => local.4,
        5 => local.5,
        6 => local.6,
        7 => local.7,
        _ => local.8,
    };
    Ok(Value::Number(value as f64))
}

fn zoned_offset(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let (_, tz) = match require_record(agent, this, RecordKind::ZonedDateTime)? {
        TemporalRecord::ZonedDateTime(ns, tz) => (ns, tz.to_string_lossy()),
        _ => unreachable!(),
    };
    let offset = super::offset_time_zone_offset_ns(&tz).unwrap_or(0);
    Ok(Value::String(Handle::new(JsString::from_utf8(
        &iso::format_offset_nanoseconds(offset),
    ))))
}

fn zoned_offset_ns(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let (_, tz) = match require_record(agent, this, RecordKind::ZonedDateTime)? {
        TemporalRecord::ZonedDateTime(ns, tz) => (ns, tz.to_string_lossy()),
        _ => unreachable!(),
    };
    let offset = super::offset_time_zone_offset_ns(&tz).unwrap_or(0);
    Ok(Value::Number(offset as f64))
}

fn zoned_to_instant(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let (ns, _) = match require_record(agent, this, RecordKind::ZonedDateTime)? {
        TemporalRecord::ZonedDateTime(ns, tz) => (ns, tz),
        _ => unreachable!(),
    };
    super::instant::create_instant(agent, ns, &Value::Undefined)
}

/// spec 3.5.4 ToTemporalDate (minimal: our records, strings, and plain
/// property bags).
pub fn to_plain_date(agent: &mut Agent, item: &Value) -> Result<Value, JsError> {
    if let Value::Object(obj) = item {
        if let Some(record) = agent.temporal_data.get(&obj.id()) {
            return match record {
                TemporalRecord::PlainDate(d) => {
                    create_plain_date(agent, (d[0], d[1], d[2]), &Value::Undefined)
                }
                TemporalRecord::PlainDateTime(dt) => {
                    create_plain_date(agent, (dt[0], dt[1], dt[2]), &Value::Undefined)
                }
                TemporalRecord::ZonedDateTime(ns, tz) => {
                    let local = zoned_local(agent, *ns, &tz.to_string_lossy())?;
                    create_plain_date(agent, (local.0, local.1, local.2), &Value::Undefined)
                }
                _ => Err(JsError::new(
                    ErrorKind::TypeError,
                    "value is not convertible to a PlainDate".into(),
                )),
            };
        }
        // Property bag.
        let mut year = None;
        let mut month = None;
        let mut day = None;
        for key in ["day", "month", "year"] {
            let value =
                crate::context::get_property(agent, item, &JsString::from_utf8(key), item.clone())?;
            if matches!(value, Value::Undefined) {
                continue;
            }
            match key {
                "day" => day = Some(super::to_positive_integer_with_truncation(agent, &value)?),
                "month" => month = Some(super::to_positive_integer_with_truncation(agent, &value)?),
                _ => year = Some(super::to_integer_with_truncation(agent, &value)?),
            }
        }
        let (Some(year), Some(month), Some(day)) = (year, month, day) else {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "year, month, and day are required".into(),
            ));
        };
        if !iso::is_valid_iso_date(year, month, day) {
            return Err(JsError::new(ErrorKind::RangeError, "invalid date".into()));
        }
        return create_plain_date(agent, (year, month, day), &Value::Undefined);
    }
    if !matches!(item, Value::String(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "value must be a string or object".into(),
        ));
    }
    let text = crate::context::to_string(agent, item)?;
    let parsed = iso::parse_iso_date_time(text.as_slice(), iso::Format::DateTimePlain)
        .map_err(|_| JsError::new(ErrorKind::RangeError, "invalid date string".into()))?;
    create_plain_date(
        agent,
        (parsed.year, parsed.month, parsed.day),
        &Value::Undefined,
    )
}

/// spec 4.5.6 ToTemporalTime (minimal).
pub fn to_plain_time(agent: &mut Agent, item: &Value) -> Result<Value, JsError> {
    if let Value::Object(obj) = item {
        if let Some(record) = agent.temporal_data.get(&obj.id()) {
            return match record {
                TemporalRecord::PlainTime(t) => create_plain_time(agent, *t, &Value::Undefined),
                TemporalRecord::PlainDateTime(dt) => create_plain_time(
                    agent,
                    [dt[3], dt[4], dt[5], dt[6], dt[7], dt[8]],
                    &Value::Undefined,
                ),
                TemporalRecord::ZonedDateTime(ns, tz) => {
                    let local = zoned_local(agent, *ns, &tz.to_string_lossy())?;
                    create_plain_time(
                        agent,
                        [local.3, local.4, local.5, local.6, local.7, local.8],
                        &Value::Undefined,
                    )
                }
                _ => Err(JsError::new(
                    ErrorKind::TypeError,
                    "value is not convertible to a PlainTime".into(),
                )),
            };
        }
        let mut t = [0i64; 6];
        for key in [
            "hour",
            "microsecond",
            "millisecond",
            "minute",
            "nanosecond",
            "second",
        ] {
            let value =
                crate::context::get_property(agent, item, &JsString::from_utf8(key), item.clone())?;
            if matches!(value, Value::Undefined) {
                continue;
            }
            let idx = match key {
                "hour" => 0,
                "minute" => 1,
                "second" => 2,
                "millisecond" => 3,
                "microsecond" => 4,
                _ => 5,
            };
            t[idx] = super::to_integer_with_truncation(agent, &value)?;
        }
        if !is_valid_time(t) {
            return Err(JsError::new(ErrorKind::RangeError, "invalid time".into()));
        }
        return create_plain_time(agent, t, &Value::Undefined);
    }
    if !matches!(item, Value::String(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "value must be a string or object".into(),
        ));
    }
    let text = crate::context::to_string(agent, item)?;
    let parsed = iso::parse_iso_date_time(text.as_slice(), iso::Format::DateTimePlain)
        .map_err(|_| JsError::new(ErrorKind::RangeError, "invalid time string".into()))?;
    let t = parsed.time.unwrap_or([0, 0, 0, 0, 0, 0]);
    create_plain_time(agent, t, &Value::Undefined)
}

/// spec 5.5.6 ToTemporalDateTime (minimal).
pub fn to_plain_date_time(agent: &mut Agent, item: &Value) -> Result<Value, JsError> {
    if let Value::Object(obj) = item
        && let Some(record) = agent.temporal_data.get(&obj.id())
    {
        return match record {
            TemporalRecord::PlainDateTime(dt) => create_temporal_object(
                agent,
                &Value::Undefined,
                PLAIN_DATE_TIME_PROTO,
                TemporalRecord::PlainDateTime(*dt),
            ),
            TemporalRecord::PlainDate(d) => create_temporal_object(
                agent,
                &Value::Undefined,
                PLAIN_DATE_TIME_PROTO,
                TemporalRecord::PlainDateTime([d[0], d[1], d[2], 0, 0, 0, 0, 0, 0]),
            ),
            TemporalRecord::ZonedDateTime(ns, tz) => {
                let local = zoned_local(agent, *ns, &tz.to_string_lossy())?;
                create_temporal_object(
                    agent,
                    &Value::Undefined,
                    PLAIN_DATE_TIME_PROTO,
                    TemporalRecord::PlainDateTime([
                        local.0, local.1, local.2, local.3, local.4, local.5, local.6, local.7,
                        local.8,
                    ]),
                )
            }
            _ => Err(JsError::new(
                ErrorKind::TypeError,
                "value is not convertible to a PlainDateTime".into(),
            )),
        };
    }
    if !matches!(item, Value::String(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "value must be a string or object".into(),
        ));
    }
    let text = crate::context::to_string(agent, item)?;
    let parsed = iso::parse_iso_date_time(text.as_slice(), iso::Format::DateTimePlain)
        .map_err(|_| JsError::new(ErrorKind::RangeError, "invalid date-time string".into()))?;
    let t = parsed.time.unwrap_or([0, 0, 0, 0, 0, 0]);
    create_temporal_object(
        agent,
        &Value::Undefined,
        PLAIN_DATE_TIME_PROTO,
        TemporalRecord::PlainDateTime([
            parsed.year,
            parsed.month,
            parsed.day,
            t[0],
            t[1],
            t[2],
            t[3],
            t[4],
            t[5],
        ]),
    )
}

/// spec 6.5.2 ToTemporalZonedDateTime (minimal: records, strings).
fn to_zoned(agent: &mut Agent, item: &Value) -> Result<Value, JsError> {
    if let Value::Object(obj) = item
        && let Some(TemporalRecord::ZonedDateTime(ns, tz)) = agent.temporal_data.get(&obj.id())
    {
        return create_temporal_object(
            agent,
            &Value::Undefined,
            ZONED_PROTO,
            TemporalRecord::ZonedDateTime(*ns, tz.clone()),
        );
    }
    if !matches!(item, Value::String(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "value must be a string or ZonedDateTime".into(),
        ));
    }
    let text = crate::context::to_string(agent, item)?;
    let parsed =
        iso::parse_iso_date_time(text.as_slice(), iso::Format::DateTimeZoned).map_err(|_| {
            JsError::new(
                ErrorKind::RangeError,
                "invalid zoned date-time string".into(),
            )
        })?;
    let tz = super::instant::to_temporal_time_zone_identifier(
        agent,
        &Value::String(Handle::new(JsString::from_utf8(&parsed.tz.annotation))),
    )?;
    let offset = super::offset_time_zone_offset_ns(&tz).unwrap_or(0);
    let [h, min, s, ms, us, ns] = parsed.time.unwrap_or([0, 0, 0, 0, 0, 0]);
    let utc = iso::get_utc_epoch_nanoseconds(
        parsed.year,
        parsed.month,
        parsed.day,
        h,
        min,
        s,
        ms,
        us,
        ns,
    );
    let epoch = if parsed.tz.z {
        utc
    } else if !parsed.tz.offset_string.is_empty() {
        let given = iso::parse_date_time_utc_offset(&parsed.tz.offset_string)
            .map_err(|_| JsError::new(ErrorKind::RangeError, "invalid offset".into()))?;
        if given != offset {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "offset does not match the time zone".into(),
            ));
        }
        utc - given
    } else {
        utc - offset
    };
    create_temporal_object(
        agent,
        &Value::Undefined,
        ZONED_PROTO,
        TemporalRecord::ZonedDateTime(epoch, JsString::from_utf8(&tz)),
    )
}

fn plain_compare_key(record: &TemporalRecord) -> Option<(i64, i64, i64)> {
    match record {
        TemporalRecord::PlainDate(d) => Some((d[0], d[1], d[2])),
        _ => None,
    }
}

fn time_compare_key(record: &TemporalRecord) -> Option<[i64; 6]> {
    match record {
        TemporalRecord::PlainTime(t) => Some(*t),
        _ => None,
    }
}

fn date_time_compare_key(record: &TemporalRecord) -> Option<[i64; 9]> {
    match record {
        TemporalRecord::PlainDateTime(dt) => Some(*dt),
        _ => None,
    }
}

fn zoned_compare_key(record: &TemporalRecord) -> Option<i128> {
    match record {
        TemporalRecord::ZonedDateTime(ns, _) => Some(*ns),
        _ => None,
    }
}

fn compare_records<T: Ord>(
    agent: &mut Agent,
    args: &[Value],
    kind: RecordKind,
    key_of: fn(&TemporalRecord) -> Option<T>,
) -> Result<Value, JsError> {
    let one = to_compare_value(
        agent,
        args.first().cloned().unwrap_or(Value::Undefined),
        kind,
    )?;
    let two = to_compare_value(
        agent,
        args.get(1).cloned().unwrap_or(Value::Undefined),
        kind,
    )?;
    let a = key_of(&one).unwrap();
    let b = key_of(&two).unwrap();
    Ok(Value::Number(if a > b {
        1.0
    } else if a < b {
        -1.0
    } else {
        0.0
    }))
}

fn to_compare_value(
    agent: &mut Agent,
    item: Value,
    kind: RecordKind,
) -> Result<TemporalRecord, JsError> {
    match kind {
        RecordKind::PlainDate => match to_plain_date(agent, &item)? {
            Value::Object(obj) => agent
                .temporal_data
                .get(&obj.id())
                .cloned()
                .ok_or_else(|| JsError::new(ErrorKind::TypeError, "brand check failed".into())),
            _ => unreachable!(),
        },
        RecordKind::PlainTime => match to_plain_time(agent, &item)? {
            Value::Object(obj) => agent
                .temporal_data
                .get(&obj.id())
                .cloned()
                .ok_or_else(|| JsError::new(ErrorKind::TypeError, "brand check failed".into())),
            _ => unreachable!(),
        },
        RecordKind::PlainDateTime => match to_plain_date_time(agent, &item)? {
            Value::Object(obj) => agent
                .temporal_data
                .get(&obj.id())
                .cloned()
                .ok_or_else(|| JsError::new(ErrorKind::TypeError, "brand check failed".into())),
            _ => unreachable!(),
        },
        _ => match to_zoned(agent, &item)? {
            Value::Object(obj) => agent
                .temporal_data
                .get(&obj.id())
                .cloned()
                .ok_or_else(|| JsError::new(ErrorKind::TypeError, "brand check failed".into())),
            _ => unreachable!(),
        },
    }
}

fn plain_date_to_string(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let [y, m, d, ..] = match require_record(agent, this, RecordKind::PlainDate)? {
        TemporalRecord::PlainDate(date) => date,
        _ => unreachable!(),
    };
    Ok(Value::String(Handle::new(JsString::from_utf8(&format!(
        "{}-{:02}-{:02}",
        iso::pad_iso_year(y),
        m,
        d
    )))))
}

fn plain_time_to_string(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let t = match require_record(agent, this, RecordKind::PlainTime)? {
        TemporalRecord::PlainTime(t) => t,
        _ => unreachable!(),
    };
    let sub = t[3] * 1_000_000 + t[4] * 1_000 + t[5];
    Ok(Value::String(Handle::new(JsString::from_utf8(
        &iso::format_time_string(t[0], t[1], t[2], sub, FracPrecision::Auto),
    ))))
}

fn plain_date_time_to_string(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let dt = match require_record(agent, this, RecordKind::PlainDateTime)? {
        TemporalRecord::PlainDateTime(dt) => dt,
        _ => unreachable!(),
    };
    let sub = dt[6] * 1_000_000 + dt[7] * 1_000 + dt[8];
    let time = iso::format_time_string(dt[3], dt[4], dt[5], sub, FracPrecision::Auto);
    Ok(Value::String(Handle::new(JsString::from_utf8(&format!(
        "{}-{:02}-{:02}T{}",
        iso::pad_iso_year(dt[0]),
        dt[1],
        dt[2],
        time
    )))))
}

/// spec 6.5.4 TemporalZonedDateTimeToString (auto precision, showOffset auto,
/// showTimeZone auto, showCalendar auto).
fn zoned_to_string(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let (ns, tz) = match require_record(agent, this, RecordKind::ZonedDateTime)? {
        TemporalRecord::ZonedDateTime(ns, tz) => (ns, tz.to_string_lossy()),
        _ => unreachable!(),
    };
    let offset = super::offset_time_zone_offset_ns(&tz).unwrap_or(0);
    let (y, m, d, h, min, s, ms, us, n) = iso::iso_parts_from_epoch(ns);
    let balanced = super::instant::balance_iso_date_time(
        y,
        m,
        d,
        h,
        min,
        s,
        ms,
        us,
        (n as i128 + offset) as i64,
    );
    let sub = balanced.6 * 1_000_000 + balanced.7 * 1_000 + balanced.8;
    let time =
        iso::format_time_string(balanced.3, balanced.4, balanced.5, sub, FracPrecision::Auto);
    let offset_str = iso::format_date_time_utc_offset_rounded(offset);
    Ok(Value::String(Handle::new(JsString::from_utf8(&format!(
        "{}-{:02}-{:02}T{}{}[{}]",
        iso::pad_iso_year(balanced.0),
        balanced.1,
        balanced.2,
        time,
        offset_str,
        tz
    )))))
}
