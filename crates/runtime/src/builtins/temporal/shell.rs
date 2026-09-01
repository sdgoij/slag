//! `PlainDate`, `PlainTime`, `PlainDateTime`, `ZonedDateTime`, and the
//! `PlainYearMonth`/`PlainMonthDay` shells (spec 3-6): constructors, field
//! getters, `from`/`compare`, and `toString`. The PlainDate cluster — `with`,
//! `add`/`subtract`, `until`/`since`, `toZonedDateTime`, `toPlainDateTime`,
//! `withCalendar`, `toPlainYearMonth`/`toPlainMonthDay`, and the calendar
//! getters — is implemented against the ISO calendar; the remaining clusters
//! are in progress.

use crux::error::{ErrorKind, JsError};
use crux::function::Function;
use crux::handle::Handle;
use crux::property::PropertyDescriptor;
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use crate::builtins::intl::date_time_format::PlainKind;

use crate::agent::Agent;
use crate::realm::Realm;

use super::calendar;
use super::iso::{self, FracPrecision, RoundingMode, Unit};
use super::{
    Overflow, RecordKind, TemporalRecord, UnitGroup, UnitOption, create_temporal_object,
    install_constructor, placeholder, require_record,
};

const PLAIN_DATE: &str = "%Temporal.PlainDate%";
const PLAIN_DATE_PROTO: &str = "%Temporal.PlainDate.prototype%";
const PLAIN_TIME: &str = "%Temporal.PlainTime%";
const PLAIN_TIME_PROTO: &str = "%Temporal.PlainTime.prototype%";
const PLAIN_DATE_TIME: &str = "%Temporal.PlainDateTime%";
const PLAIN_DATE_TIME_PROTO: &str = "%Temporal.PlainDateTime.prototype%";
const ZONED: &str = "%Temporal.ZonedDateTime%";
const ZONED_PROTO: &str = "%Temporal.ZonedDateTime.prototype%";
const PLAIN_YEAR_MONTH: &str = "%Temporal.PlainYearMonth%";
const PLAIN_YEAR_MONTH_PROTO: &str = "%Temporal.PlainYearMonth.prototype%";
const PLAIN_MONTH_DAY: &str = "%Temporal.PlainMonthDay%";
const PLAIN_MONTH_DAY_PROTO: &str = "%Temporal.PlainMonthDay.prototype%";

/// Install the four Temporal type shells on `parent` (the Temporal object).
pub fn install(
    parent: &Handle<crux::object::JsObject>,
    realm: &Handle<Realm>,
) -> Result<(), JsError> {
    install_plain_date(parent, realm)?;
    install_plain_time(parent, realm)?;
    install_plain_date_time(parent, realm)?;
    install_zoned_date_time(parent, realm)?;
    install_plain_year_month(parent, realm)?;
    install_plain_month_day(parent, realm)?;
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
            ("era", "%Temporal.PlainDate.prototype.era%"),
            ("eraYear", "%Temporal.PlainDate.prototype.eraYear%"),
            ("dayOfYear", "%Temporal.PlainDate.prototype.dayOfYear%"),
            ("weekOfYear", "%Temporal.PlainDate.prototype.weekOfYear%"),
            ("yearOfWeek", "%Temporal.PlainDate.prototype.yearOfWeek%"),
            (
                "monthsInYear",
                "%Temporal.PlainDate.prototype.monthsInYear%",
            ),
            ("daysInWeek", "%Temporal.PlainDate.prototype.daysInWeek%"),
            ("daysInMonth", "%Temporal.PlainDate.prototype.daysInMonth%"),
            ("daysInYear", "%Temporal.PlainDate.prototype.daysInYear%"),
            ("inLeapYear", "%Temporal.PlainDate.prototype.inLeapYear%"),
            ("calendarId", "%Temporal.PlainDate.prototype.calendarId%"),
        ],
    )?;
    proto_methods(
        realm,
        &proto,
        &[
            ("with", "%Temporal.PlainDate.prototype.with%", 1),
            (
                "withCalendar",
                "%Temporal.PlainDate.prototype.withCalendar%",
                1,
            ),
            ("add", "%Temporal.PlainDate.prototype.add%", 1),
            ("subtract", "%Temporal.PlainDate.prototype.subtract%", 1),
            ("until", "%Temporal.PlainDate.prototype.until%", 1),
            ("since", "%Temporal.PlainDate.prototype.since%", 1),
            ("equals", "%Temporal.PlainDate.prototype.equals%", 1),
            (
                "toPlainDateTime",
                "%Temporal.PlainDate.prototype.toPlainDateTime%",
                0,
            ),
            (
                "toZonedDateTime",
                "%Temporal.PlainDate.prototype.toZonedDateTime%",
                1,
            ),
            (
                "toPlainYearMonth",
                "%Temporal.PlainDate.prototype.toPlainYearMonth%",
                0,
            ),
            (
                "toPlainMonthDay",
                "%Temporal.PlainDate.prototype.toPlainMonthDay%",
                0,
            ),
            ("toString", "%Temporal.PlainDate.prototype.toString%", 0),
            ("toJSON", "%Temporal.PlainDate.prototype.toJSON%", 0),
            (
                "toLocaleString",
                "%Temporal.PlainDate.prototype.toLocaleString%",
                0,
            ),
            ("valueOf", "%Temporal.PlainDate.prototype.valueOf%", 0),
        ],
    )?;
    Ok(())
}

fn install_plain_year_month(
    parent: &Handle<crux::object::JsObject>,
    realm: &Handle<Realm>,
) -> Result<(), JsError> {
    let (ctor, proto) = install_constructor(
        realm,
        parent,
        "PlainYearMonth",
        PLAIN_YEAR_MONTH,
        PLAIN_YEAR_MONTH_PROTO,
        2,
        "Temporal.PlainYearMonth",
    )?;
    statics(
        realm,
        &ctor,
        &[
            ("from", "%Temporal.PlainYearMonth.from%", 1),
            ("compare", "%Temporal.PlainYearMonth.compare%", 2),
        ],
    )?;
    proto_getters(
        realm,
        &proto,
        &[
            ("year", "%Temporal.PlainYearMonth.prototype.year%"),
            ("month", "%Temporal.PlainYearMonth.prototype.month%"),
            ("monthCode", "%Temporal.PlainYearMonth.prototype.monthCode%"),
            ("day", "%Temporal.PlainYearMonth.prototype.day%"),
            (
                "calendarId",
                "%Temporal.PlainYearMonth.prototype.calendarId%",
            ),
            ("era", "%Temporal.PlainYearMonth.prototype.era%"),
            ("eraYear", "%Temporal.PlainYearMonth.prototype.eraYear%"),
            (
                "daysInMonth",
                "%Temporal.PlainYearMonth.prototype.daysInMonth%",
            ),
            (
                "daysInYear",
                "%Temporal.PlainYearMonth.prototype.daysInYear%",
            ),
            (
                "monthsInYear",
                "%Temporal.PlainYearMonth.prototype.monthsInYear%",
            ),
            (
                "inLeapYear",
                "%Temporal.PlainYearMonth.prototype.inLeapYear%",
            ),
        ],
    )?;
    proto_methods(
        realm,
        &proto,
        &[
            ("with", "%Temporal.PlainYearMonth.prototype.with%", 1),
            ("add", "%Temporal.PlainYearMonth.prototype.add%", 1),
            (
                "subtract",
                "%Temporal.PlainYearMonth.prototype.subtract%",
                1,
            ),
            ("until", "%Temporal.PlainYearMonth.prototype.until%", 1),
            ("since", "%Temporal.PlainYearMonth.prototype.since%", 1),
            ("equals", "%Temporal.PlainYearMonth.prototype.equals%", 1),
            (
                "toPlainDate",
                "%Temporal.PlainYearMonth.prototype.toPlainDate%",
                1,
            ),
            (
                "toString",
                "%Temporal.PlainYearMonth.prototype.toString%",
                0,
            ),
            ("toJSON", "%Temporal.PlainYearMonth.prototype.toJSON%", 0),
            (
                "toLocaleString",
                "%Temporal.PlainYearMonth.prototype.toLocaleString%",
                0,
            ),
            ("valueOf", "%Temporal.PlainYearMonth.prototype.valueOf%", 0),
        ],
    )?;
    Ok(())
}

fn install_plain_month_day(
    parent: &Handle<crux::object::JsObject>,
    realm: &Handle<Realm>,
) -> Result<(), JsError> {
    let (ctor, proto) = install_constructor(
        realm,
        parent,
        "PlainMonthDay",
        PLAIN_MONTH_DAY,
        PLAIN_MONTH_DAY_PROTO,
        2,
        "Temporal.PlainMonthDay",
    )?;
    statics(
        realm,
        &ctor,
        &[
            ("from", "%Temporal.PlainMonthDay.from%", 1),
            ("compare", "%Temporal.PlainMonthDay.compare%", 2),
        ],
    )?;
    proto_getters(
        realm,
        &proto,
        &[
            ("monthCode", "%Temporal.PlainMonthDay.prototype.monthCode%"),
            ("day", "%Temporal.PlainMonthDay.prototype.day%"),
            (
                "calendarId",
                "%Temporal.PlainMonthDay.prototype.calendarId%",
            ),
        ],
    )?;
    proto_methods(
        realm,
        &proto,
        &[
            ("with", "%Temporal.PlainMonthDay.prototype.with%", 1),
            ("equals", "%Temporal.PlainMonthDay.prototype.equals%", 1),
            (
                "toPlainDate",
                "%Temporal.PlainMonthDay.prototype.toPlainDate%",
                1,
            ),
            ("toString", "%Temporal.PlainMonthDay.prototype.toString%", 0),
            ("toJSON", "%Temporal.PlainMonthDay.prototype.toJSON%", 0),
            (
                "toLocaleString",
                "%Temporal.PlainMonthDay.prototype.toLocaleString%",
                0,
            ),
            ("valueOf", "%Temporal.PlainMonthDay.prototype.valueOf%", 0),
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
            ("with", "%Temporal.PlainTime.prototype.with%", 1),
            ("add", "%Temporal.PlainTime.prototype.add%", 1),
            ("subtract", "%Temporal.PlainTime.prototype.subtract%", 1),
            ("round", "%Temporal.PlainTime.prototype.round%", 1),
            ("until", "%Temporal.PlainTime.prototype.until%", 1),
            ("since", "%Temporal.PlainTime.prototype.since%", 1),
            ("equals", "%Temporal.PlainTime.prototype.equals%", 1),
            ("toString", "%Temporal.PlainTime.prototype.toString%", 0),
            ("toJSON", "%Temporal.PlainTime.prototype.toJSON%", 0),
            (
                "toLocaleString",
                "%Temporal.PlainTime.prototype.toLocaleString%",
                0,
            ),
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
            ("era", "%Temporal.PlainDateTime.prototype.era%"),
            ("eraYear", "%Temporal.PlainDateTime.prototype.eraYear%"),
            ("dayOfWeek", "%Temporal.PlainDateTime.prototype.dayOfWeek%"),
            ("dayOfYear", "%Temporal.PlainDateTime.prototype.dayOfYear%"),
            (
                "weekOfYear",
                "%Temporal.PlainDateTime.prototype.weekOfYear%",
            ),
            (
                "yearOfWeek",
                "%Temporal.PlainDateTime.prototype.yearOfWeek%",
            ),
            (
                "monthsInYear",
                "%Temporal.PlainDateTime.prototype.monthsInYear%",
            ),
            (
                "daysInWeek",
                "%Temporal.PlainDateTime.prototype.daysInWeek%",
            ),
            (
                "daysInMonth",
                "%Temporal.PlainDateTime.prototype.daysInMonth%",
            ),
            (
                "daysInYear",
                "%Temporal.PlainDateTime.prototype.daysInYear%",
            ),
            (
                "inLeapYear",
                "%Temporal.PlainDateTime.prototype.inLeapYear%",
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
            ("with", "%Temporal.PlainDateTime.prototype.with%", 1),
            (
                "withPlainTime",
                "%Temporal.PlainDateTime.prototype.withPlainTime%",
                0,
            ),
            (
                "withCalendar",
                "%Temporal.PlainDateTime.prototype.withCalendar%",
                1,
            ),
            ("add", "%Temporal.PlainDateTime.prototype.add%", 1),
            ("subtract", "%Temporal.PlainDateTime.prototype.subtract%", 1),
            ("round", "%Temporal.PlainDateTime.prototype.round%", 1),
            ("until", "%Temporal.PlainDateTime.prototype.until%", 1),
            ("since", "%Temporal.PlainDateTime.prototype.since%", 1),
            ("equals", "%Temporal.PlainDateTime.prototype.equals%", 1),
            (
                "toPlainDate",
                "%Temporal.PlainDateTime.prototype.toPlainDate%",
                0,
            ),
            (
                "toPlainTime",
                "%Temporal.PlainDateTime.prototype.toPlainTime%",
                0,
            ),
            (
                "toZonedDateTime",
                "%Temporal.PlainDateTime.prototype.toZonedDateTime%",
                1,
            ),
            ("toString", "%Temporal.PlainDateTime.prototype.toString%", 0),
            ("toJSON", "%Temporal.PlainDateTime.prototype.toJSON%", 0),
            (
                "toLocaleString",
                "%Temporal.PlainDateTime.prototype.toLocaleString%",
                0,
            ),
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
            ("monthCode", "%Temporal.ZonedDateTime.prototype.monthCode%"),
            ("era", "%Temporal.ZonedDateTime.prototype.era%"),
            ("eraYear", "%Temporal.ZonedDateTime.prototype.eraYear%"),
            ("dayOfWeek", "%Temporal.ZonedDateTime.prototype.dayOfWeek%"),
            ("dayOfYear", "%Temporal.ZonedDateTime.prototype.dayOfYear%"),
            (
                "weekOfYear",
                "%Temporal.ZonedDateTime.prototype.weekOfYear%",
            ),
            (
                "yearOfWeek",
                "%Temporal.ZonedDateTime.prototype.yearOfWeek%",
            ),
            (
                "hoursInDay",
                "%Temporal.ZonedDateTime.prototype.hoursInDay%",
            ),
            (
                "daysInWeek",
                "%Temporal.ZonedDateTime.prototype.daysInWeek%",
            ),
            (
                "daysInMonth",
                "%Temporal.ZonedDateTime.prototype.daysInMonth%",
            ),
            (
                "daysInYear",
                "%Temporal.ZonedDateTime.prototype.daysInYear%",
            ),
            (
                "monthsInYear",
                "%Temporal.ZonedDateTime.prototype.monthsInYear%",
            ),
            (
                "inLeapYear",
                "%Temporal.ZonedDateTime.prototype.inLeapYear%",
            ),
        ],
    )?;
    proto_methods(
        realm,
        &proto,
        &[
            ("equals", "%Temporal.ZonedDateTime.prototype.equals%", 1),
            (
                "toInstant",
                "%Temporal.ZonedDateTime.prototype.toInstant%",
                0,
            ),
            ("toString", "%Temporal.ZonedDateTime.prototype.toString%", 0),
            ("toJSON", "%Temporal.ZonedDateTime.prototype.toJSON%", 0),
            (
                "toLocaleString",
                "%Temporal.ZonedDateTime.prototype.toLocaleString%",
                0,
            ),
            ("valueOf", "%Temporal.ZonedDateTime.prototype.valueOf%", 0),
            ("with", "%Temporal.ZonedDateTime.prototype.with%", 1),
            (
                "withPlainTime",
                "%Temporal.ZonedDateTime.prototype.withPlainTime%",
                0,
            ),
            (
                "withTimeZone",
                "%Temporal.ZonedDateTime.prototype.withTimeZone%",
                1,
            ),
            (
                "withCalendar",
                "%Temporal.ZonedDateTime.prototype.withCalendar%",
                1,
            ),
            ("add", "%Temporal.ZonedDateTime.prototype.add%", 1),
            ("subtract", "%Temporal.ZonedDateTime.prototype.subtract%", 1),
            ("round", "%Temporal.ZonedDateTime.prototype.round%", 1),
            ("until", "%Temporal.ZonedDateTime.prototype.until%", 1),
            ("since", "%Temporal.ZonedDateTime.prototype.since%", 1),
            (
                "startOfDay",
                "%Temporal.ZonedDateTime.prototype.startOfDay%",
                0,
            ),
            (
                "getTimeZoneTransition",
                "%Temporal.ZonedDateTime.prototype.getTimeZoneTransition%",
                1,
            ),
            (
                "toPlainDate",
                "%Temporal.ZonedDateTime.prototype.toPlainDate%",
                0,
            ),
            (
                "toPlainTime",
                "%Temporal.ZonedDateTime.prototype.toPlainTime%",
                0,
            ),
            (
                "toPlainDateTime",
                "%Temporal.ZonedDateTime.prototype.toPlainDateTime%",
                0,
            ),
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
        (TemporalRecord::YearMonth(ym), RecordKind::YearMonth) => ym[index],
        (TemporalRecord::MonthDay(md), RecordKind::MonthDay) => md[index],
        _ => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "brand check failed".into(),
            ));
        }
    };
    Ok(Value::Number(value as f64))
}

/// The calendar-aware `year` getter: the linear calendars the corpus
/// exercises convert the ISO year (roc offsets by 1911; gregory and
/// japanese keep it), the rest return the ISO year.
fn calendar_year_getter(
    agent: &mut Agent,
    this: &Value,
    kind: RecordKind,
) -> Result<Value, JsError> {
    let record = require_record(agent, this, kind)?;
    let (iso_year, iso_month, iso_day) = match (&record, kind) {
        (TemporalRecord::PlainDate(d), RecordKind::PlainDate) => (d[0], d[1], d[2]),
        (TemporalRecord::PlainDateTime(dt), RecordKind::PlainDateTime) => (dt[0], dt[1], dt[2]),
        (TemporalRecord::YearMonth(ym), RecordKind::YearMonth) => (ym[0], ym[1], ym[2]),
        (TemporalRecord::ZonedDateTime(ns, tz), RecordKind::ZonedDateTime) => {
            let local = zoned_local(agent, *ns, &tz.to_string_lossy())?;
            (local.0, local.1, local.2)
        }
        _ => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "brand check failed".into(),
            ));
        }
    };
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    Ok(Value::Number(
        calendar_year_fields(&calendar, iso_year, iso_month, iso_day).0 as f64,
    ))
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
        ("PlainYearMonth", PLAIN_YEAR_MONTH),
        ("PlainMonthDay", PLAIN_MONTH_DAY),
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
        return Some(calendar_year_getter(agent, this, RecordKind::PlainDate));
    }
    if field("%Temporal.PlainDate.prototype.month%") {
        return Some(plain_date_calendar_field(agent, this, "month"));
    }
    if field("%Temporal.PlainDate.prototype.day%") {
        return Some(plain_date_calendar_field(agent, this, "day"));
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
    if field("%Temporal.ZonedDateTime.prototype.year%") {
        return Some(calendar_year_getter(agent, this, RecordKind::ZonedDateTime));
    }
    for (idx, name) in [
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
    if field("%Temporal.ZonedDateTime.prototype.monthCode%") {
        return Some(zoned_month_code(agent, this));
    }
    // ZonedDateTime calendar getters (computed from the local date part).
    for (name, key) in [
        ("era", "%Temporal.ZonedDateTime.prototype.era%"),
        ("eraYear", "%Temporal.ZonedDateTime.prototype.eraYear%"),
        ("month", "%Temporal.ZonedDateTime.prototype.month%"),
        ("day", "%Temporal.ZonedDateTime.prototype.day%"),
        ("dayOfWeek", "%Temporal.ZonedDateTime.prototype.dayOfWeek%"),
        ("dayOfYear", "%Temporal.ZonedDateTime.prototype.dayOfYear%"),
        (
            "weekOfYear",
            "%Temporal.ZonedDateTime.prototype.weekOfYear%",
        ),
        (
            "yearOfWeek",
            "%Temporal.ZonedDateTime.prototype.yearOfWeek%",
        ),
        (
            "hoursInDay",
            "%Temporal.ZonedDateTime.prototype.hoursInDay%",
        ),
        (
            "daysInWeek",
            "%Temporal.ZonedDateTime.prototype.daysInWeek%",
        ),
        (
            "daysInMonth",
            "%Temporal.ZonedDateTime.prototype.daysInMonth%",
        ),
        (
            "daysInYear",
            "%Temporal.ZonedDateTime.prototype.daysInYear%",
        ),
        (
            "monthsInYear",
            "%Temporal.ZonedDateTime.prototype.monthsInYear%",
        ),
        (
            "inLeapYear",
            "%Temporal.ZonedDateTime.prototype.inLeapYear%",
        ),
    ] {
        if field(key) {
            return Some(zoned_calendar_field(agent, this, name));
        }
    }
    if field("%Temporal.PlainDateTime.prototype.year%") {
        return Some(calendar_year_getter(agent, this, RecordKind::PlainDateTime));
    }
    // The PlainDateTime time fields (the date part's month/day route through
    // the calendar getters above; year through calendar_year_getter).
    for (idx, name) in [
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
    // PlainDate calendar getters.
    for (name, key) in [
        ("era", "%Temporal.PlainDate.prototype.era%"),
        ("eraYear", "%Temporal.PlainDate.prototype.eraYear%"),
        ("dayOfYear", "%Temporal.PlainDate.prototype.dayOfYear%"),
        ("weekOfYear", "%Temporal.PlainDate.prototype.weekOfYear%"),
        ("yearOfWeek", "%Temporal.PlainDate.prototype.yearOfWeek%"),
        (
            "monthsInYear",
            "%Temporal.PlainDate.prototype.monthsInYear%",
        ),
        ("daysInWeek", "%Temporal.PlainDate.prototype.daysInWeek%"),
        ("daysInMonth", "%Temporal.PlainDate.prototype.daysInMonth%"),
        ("daysInYear", "%Temporal.PlainDate.prototype.daysInYear%"),
        ("inLeapYear", "%Temporal.PlainDate.prototype.inLeapYear%"),
    ] {
        if field(key) {
            return Some(plain_date_calendar_field(agent, this, name));
        }
    }
    // PlainDateTime calendar getters (computed from the date part).
    for (name, key) in [
        ("era", "%Temporal.PlainDateTime.prototype.era%"),
        ("eraYear", "%Temporal.PlainDateTime.prototype.eraYear%"),
        ("month", "%Temporal.PlainDateTime.prototype.month%"),
        ("day", "%Temporal.PlainDateTime.prototype.day%"),
        ("dayOfWeek", "%Temporal.PlainDateTime.prototype.dayOfWeek%"),
        ("dayOfYear", "%Temporal.PlainDateTime.prototype.dayOfYear%"),
        (
            "weekOfYear",
            "%Temporal.PlainDateTime.prototype.weekOfYear%",
        ),
        (
            "yearOfWeek",
            "%Temporal.PlainDateTime.prototype.yearOfWeek%",
        ),
        (
            "monthsInYear",
            "%Temporal.PlainDateTime.prototype.monthsInYear%",
        ),
        (
            "daysInWeek",
            "%Temporal.PlainDateTime.prototype.daysInWeek%",
        ),
        (
            "daysInMonth",
            "%Temporal.PlainDateTime.prototype.daysInMonth%",
        ),
        (
            "daysInYear",
            "%Temporal.PlainDateTime.prototype.daysInYear%",
        ),
        (
            "inLeapYear",
            "%Temporal.PlainDateTime.prototype.inLeapYear%",
        ),
    ] {
        if field(key) {
            return Some(plain_date_time_calendar_field(agent, this, name));
        }
    }
    // PlainYearMonth getters.
    if field("%Temporal.PlainYearMonth.prototype.year%") {
        return Some(calendar_year_getter(agent, this, RecordKind::YearMonth));
    }
    if field("%Temporal.PlainYearMonth.prototype.month%") {
        return Some(year_month_calendar_field(agent, this, "month"));
    }
    if field("%Temporal.PlainYearMonth.prototype.monthCode%") {
        return Some(month_code(agent, this, RecordKind::YearMonth, 1));
    }
    if field("%Temporal.PlainYearMonth.prototype.day%") {
        return Some(year_month_calendar_field(agent, this, "day"));
    }
    if field("%Temporal.PlainYearMonth.prototype.calendarId%") {
        return Some(calendar_id(agent, this, RecordKind::YearMonth));
    }
    for (name, key) in [
        ("era", "%Temporal.PlainYearMonth.prototype.era%"),
        ("eraYear", "%Temporal.PlainYearMonth.prototype.eraYear%"),
        (
            "daysInMonth",
            "%Temporal.PlainYearMonth.prototype.daysInMonth%",
        ),
        (
            "daysInYear",
            "%Temporal.PlainYearMonth.prototype.daysInYear%",
        ),
        (
            "monthsInYear",
            "%Temporal.PlainYearMonth.prototype.monthsInYear%",
        ),
        (
            "inLeapYear",
            "%Temporal.PlainYearMonth.prototype.inLeapYear%",
        ),
    ] {
        if field(key) {
            return Some(year_month_calendar_field(agent, this, name));
        }
    }
    // PlainMonthDay getters.
    if field("%Temporal.PlainMonthDay.prototype.monthCode%") {
        return Some(month_code(agent, this, RecordKind::MonthDay, 1));
    }
    if field("%Temporal.PlainMonthDay.prototype.day%") {
        return Some(plain_month_day_day(agent, this));
    }
    if field("%Temporal.PlainMonthDay.prototype.calendarId%") {
        return Some(calendar_id(agent, this, RecordKind::MonthDay));
    }
    // Statics.
    if intrinsics.get("%Temporal.PlainDate.from%").as_ref() == Some(callee) {
        let item = args.first().cloned().unwrap_or(Value::Undefined);
        let options = args.get(1).cloned().unwrap_or(Value::Undefined);
        return Some(to_plain_date_with_options(agent, &item, &options));
    }
    if intrinsics.get("%Temporal.PlainTime.from%").as_ref() == Some(callee) {
        let item = args.first().cloned().unwrap_or(Value::Undefined);
        let options = args.get(1).cloned().unwrap_or(Value::Undefined);
        return Some(to_plain_time_with_options(agent, &item, &options));
    }
    if intrinsics.get("%Temporal.PlainDateTime.from%").as_ref() == Some(callee) {
        let item = args.first().cloned().unwrap_or(Value::Undefined);
        let options = args.get(1).cloned().unwrap_or(Value::Undefined);
        return Some(to_plain_date_time(agent, &item, &options));
    }
    if intrinsics.get("%Temporal.ZonedDateTime.from%").as_ref() == Some(callee) {
        let item = args.first().cloned().unwrap_or(Value::Undefined);
        let options = args.get(1).cloned().unwrap_or(Value::Undefined);
        return Some(to_zoned(agent, &item, &options));
    }
    if intrinsics.get("%Temporal.PlainYearMonth.from%").as_ref() == Some(callee) {
        let item = args.first().cloned().unwrap_or(Value::Undefined);
        let options = args.get(1).cloned().unwrap_or(Value::Undefined);
        return Some(to_plain_year_month(agent, &item, &options));
    }
    if intrinsics.get("%Temporal.PlainMonthDay.from%").as_ref() == Some(callee) {
        let item = args.first().cloned().unwrap_or(Value::Undefined);
        let options = args.get(1).cloned().unwrap_or(Value::Undefined);
        return Some(to_plain_month_day(agent, &item, &options));
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
    if intrinsics.get("%Temporal.PlainYearMonth.compare%").as_ref() == Some(callee) {
        return Some(compare_records(
            agent,
            args,
            RecordKind::YearMonth,
            year_month_compare_key,
        ));
    }
    if intrinsics.get("%Temporal.PlainMonthDay.compare%").as_ref() == Some(callee) {
        return Some(compare_records(
            agent,
            args,
            RecordKind::MonthDay,
            month_day_compare_key,
        ));
    }
    // Prototype methods.
    if field("%Temporal.PlainDate.prototype.toString%") {
        return Some(plain_date_to_string_impl(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.PlainDate.prototype.toJSON%") {
        return Some(plain_date_to_string_impl(agent, this, Value::Undefined));
    }
    if field("%Temporal.PlainDate.prototype.toLocaleString%") {
        return Some(plain_to_locale_string_dispatch(
            agent,
            this,
            args,
            PlainKind::Date,
            "date",
            "date",
        ));
    }
    if field("%Temporal.PlainDate.prototype.with%") {
        return Some(plain_date_with(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
            args.get(1).cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.PlainDate.prototype.withCalendar%") {
        return Some(plain_date_with_calendar(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.PlainDate.prototype.add%") {
        return Some(plain_date_add_subtract(agent, this, args, false));
    }
    if field("%Temporal.PlainDate.prototype.subtract%") {
        return Some(plain_date_add_subtract(agent, this, args, true));
    }
    if field("%Temporal.PlainDate.prototype.until%") {
        return Some(plain_date_until_since(agent, this, args, false));
    }
    if field("%Temporal.PlainDate.prototype.since%") {
        return Some(plain_date_until_since(agent, this, args, true));
    }
    if field("%Temporal.PlainDate.prototype.equals%") {
        return Some(plain_date_equals(agent, this, args));
    }
    if field("%Temporal.PlainDate.prototype.toPlainDateTime%") {
        return Some(plain_date_to_plain_date_time(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.PlainDate.prototype.toZonedDateTime%") {
        return Some(plain_date_to_zoned_date_time(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.PlainDate.prototype.toPlainYearMonth%") {
        return Some(plain_date_to_plain_year_month(agent, this));
    }
    if field("%Temporal.PlainDate.prototype.toPlainMonthDay%") {
        return Some(plain_date_to_plain_month_day(agent, this));
    }
    if field("%Temporal.PlainTime.prototype.toString%") {
        return Some(plain_time_to_string_impl(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.PlainTime.prototype.toJSON%") {
        return Some(plain_time_to_string_impl(agent, this, Value::Undefined));
    }
    if field("%Temporal.PlainTime.prototype.toLocaleString%") {
        return Some(plain_to_locale_string_dispatch(
            agent,
            this,
            args,
            PlainKind::Time,
            "time",
            "time",
        ));
    }
    if field("%Temporal.PlainTime.prototype.with%") {
        return Some(plain_time_with(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
            args.get(1).cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.PlainTime.prototype.add%") {
        return Some(plain_time_add_subtract(agent, this, args, false));
    }
    if field("%Temporal.PlainTime.prototype.subtract%") {
        return Some(plain_time_add_subtract(agent, this, args, true));
    }
    if field("%Temporal.PlainTime.prototype.round%") {
        return Some(plain_time_round(agent, this, args));
    }
    if field("%Temporal.PlainTime.prototype.until%") {
        return Some(plain_time_until_since(agent, this, args, false));
    }
    if field("%Temporal.PlainTime.prototype.since%") {
        return Some(plain_time_until_since(agent, this, args, true));
    }
    if field("%Temporal.PlainTime.prototype.equals%") {
        return Some(plain_time_equals(agent, this, args));
    }
    if field("%Temporal.PlainDateTime.prototype.toString%") {
        return Some(plain_date_time_to_string_impl(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.PlainDateTime.prototype.toJSON%") {
        return Some(plain_date_time_to_string_impl(
            agent,
            this,
            Value::Undefined,
        ));
    }
    if field("%Temporal.PlainDateTime.prototype.toLocaleString%") {
        return Some(plain_to_locale_string_dispatch(
            agent,
            this,
            args,
            PlainKind::DateTime,
            "any",
            "all",
        ));
    }
    if field("%Temporal.PlainDateTime.prototype.equals%") {
        return Some(plain_date_time_equals(agent, this, args));
    }
    if field("%Temporal.PlainDateTime.prototype.with%") {
        return Some(plain_date_time_with(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
            args.get(1).cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.PlainDateTime.prototype.withPlainTime%") {
        return Some(plain_date_time_with_plain_time(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.PlainDateTime.prototype.withCalendar%") {
        return Some(plain_date_time_with_calendar(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.PlainDateTime.prototype.add%") {
        return Some(plain_date_time_add_subtract(agent, this, args, false));
    }
    if field("%Temporal.PlainDateTime.prototype.subtract%") {
        return Some(plain_date_time_add_subtract(agent, this, args, true));
    }
    if field("%Temporal.PlainDateTime.prototype.round%") {
        return Some(plain_date_time_round(agent, this, args));
    }
    if field("%Temporal.PlainDateTime.prototype.until%") {
        return Some(plain_date_time_until_since(agent, this, args, false));
    }
    if field("%Temporal.PlainDateTime.prototype.since%") {
        return Some(plain_date_time_until_since(agent, this, args, true));
    }
    if field("%Temporal.PlainDateTime.prototype.toPlainDate%") {
        return Some(plain_date_time_to_plain_date(agent, this));
    }
    if field("%Temporal.PlainDateTime.prototype.toPlainTime%") {
        return Some(plain_date_time_to_plain_time(agent, this));
    }
    if field("%Temporal.PlainDateTime.prototype.toZonedDateTime%") {
        return Some(plain_date_time_to_zoned_date_time(agent, this, args));
    }
    if field("%Temporal.ZonedDateTime.prototype.toString%") {
        return Some(zoned_to_string_impl(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.ZonedDateTime.prototype.toJSON%") {
        return Some(zoned_to_string(agent, this));
    }
    if field("%Temporal.ZonedDateTime.prototype.toLocaleString%") {
        return Some(zoned_to_locale_string_dispatch(agent, this, args));
    }
    if field("%Temporal.ZonedDateTime.prototype.toInstant%") {
        return Some(zoned_to_instant(agent, this));
    }
    if field("%Temporal.ZonedDateTime.prototype.equals%") {
        return Some(zoned_equals(agent, this, args));
    }
    if field("%Temporal.ZonedDateTime.prototype.with%") {
        return Some(zoned_with(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
            args.get(1).cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.ZonedDateTime.prototype.withPlainTime%") {
        return Some(zoned_with_plain_time(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.ZonedDateTime.prototype.withTimeZone%") {
        return Some(zoned_with_time_zone(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.ZonedDateTime.prototype.withCalendar%") {
        return Some(zoned_with_calendar(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.ZonedDateTime.prototype.add%") {
        return Some(zoned_add_subtract(agent, this, args, false));
    }
    if field("%Temporal.ZonedDateTime.prototype.subtract%") {
        return Some(zoned_add_subtract(agent, this, args, true));
    }
    if field("%Temporal.ZonedDateTime.prototype.round%") {
        return Some(zoned_round(agent, this, args));
    }
    if field("%Temporal.ZonedDateTime.prototype.until%") {
        return Some(zoned_until_since(agent, this, args, false));
    }
    if field("%Temporal.ZonedDateTime.prototype.since%") {
        return Some(zoned_until_since(agent, this, args, true));
    }
    if field("%Temporal.ZonedDateTime.prototype.startOfDay%") {
        return Some(zoned_start_of_day(agent, this));
    }
    if field("%Temporal.ZonedDateTime.prototype.getTimeZoneTransition%") {
        return Some(zoned_get_time_zone_transition(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.ZonedDateTime.prototype.toPlainDate%") {
        return Some(zoned_to_plain_date(agent, this));
    }
    if field("%Temporal.ZonedDateTime.prototype.toPlainTime%") {
        return Some(zoned_to_plain_time(agent, this));
    }
    if field("%Temporal.ZonedDateTime.prototype.toPlainDateTime%") {
        return Some(zoned_to_plain_date_time(agent, this));
    }
    if field("%Temporal.PlainYearMonth.prototype.toString%") {
        return Some(year_month_to_string_impl(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.PlainYearMonth.prototype.toJSON%") {
        return Some(year_month_to_string_impl(agent, this, Value::Undefined));
    }
    if field("%Temporal.PlainYearMonth.prototype.toLocaleString%") {
        return Some(plain_to_locale_string_dispatch(
            agent,
            this,
            args,
            PlainKind::YearMonth,
            "date",
            "date",
        ));
    }
    if field("%Temporal.PlainYearMonth.prototype.with%") {
        return Some(plain_year_month_with(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
            args.get(1).cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.PlainYearMonth.prototype.add%") {
        return Some(plain_year_month_add_subtract(agent, this, args, false));
    }
    if field("%Temporal.PlainYearMonth.prototype.subtract%") {
        return Some(plain_year_month_add_subtract(agent, this, args, true));
    }
    if field("%Temporal.PlainYearMonth.prototype.until%") {
        return Some(plain_year_month_until_since(agent, this, args, false));
    }
    if field("%Temporal.PlainYearMonth.prototype.since%") {
        return Some(plain_year_month_until_since(agent, this, args, true));
    }
    if field("%Temporal.PlainYearMonth.prototype.equals%") {
        return Some(plain_year_month_equals(agent, this, args));
    }
    if field("%Temporal.PlainYearMonth.prototype.toPlainDate%") {
        return Some(plain_year_month_to_plain_date(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.PlainMonthDay.prototype.with%") {
        return Some(plain_month_day_with(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
            args.get(1).cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.PlainMonthDay.prototype.equals%") {
        return Some(plain_month_day_equals(agent, this, args));
    }
    if field("%Temporal.PlainMonthDay.prototype.toPlainDate%") {
        return Some(plain_month_day_to_plain_date(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.PlainMonthDay.prototype.toString%") {
        return Some(month_day_to_string_impl(
            agent,
            this,
            args.first().cloned().unwrap_or(Value::Undefined),
        ));
    }
    if field("%Temporal.PlainMonthDay.prototype.toJSON%") {
        return Some(month_day_to_string_impl(agent, this, Value::Undefined));
    }
    if field("%Temporal.PlainMonthDay.prototype.toLocaleString%") {
        return Some(plain_to_locale_string_dispatch(
            agent,
            this,
            args,
            PlainKind::MonthDay,
            "date",
            "date",
        ));
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
        (
            "PlainYearMonth",
            "%Temporal.PlainYearMonth.prototype.valueOf%",
        ),
        (
            "PlainMonthDay",
            "%Temporal.PlainMonthDay.prototype.valueOf%",
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

/// Temporal.Plain* toLocaleString dispatch: build the DateTimeFormat and
/// format the plain wall-clock fields (the per-type required/defaults).
fn plain_to_locale_string_dispatch(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    kind: PlainKind,
    required: &str,
    defaults: &str,
) -> Result<Value, JsError> {
    let locales = args.first().cloned().unwrap_or(Value::Undefined);
    let options = args.get(1).cloned().unwrap_or(Value::Undefined);
    crate::builtins::intl::date_time_format::plain_to_locale_string(
        agent, &locales, &options, this, kind, required, defaults,
    )
    .map(|text| Value::String(Handle::new(JsString::from_utf8(&text))))
}

/// Temporal.ZonedDateTime.prototype.toLocaleString dispatch: the instance's
/// own time zone is used and a timeZone option is rejected.
fn zoned_to_locale_string_dispatch(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let (ns, tz) = zoned_parts(agent, this)?;
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    let locales = args.first().cloned().unwrap_or(Value::Undefined);
    let options = args.get(1).cloned().unwrap_or(Value::Undefined);
    crate::builtins::intl::date_time_format::zoned_to_locale_string(
        agent, &locales, &options, ns, &tz, &calendar,
    )
    .map(|text| Value::String(Handle::new(JsString::from_utf8(&text))))
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
    if intrinsics.get(PLAIN_YEAR_MONTH).as_ref() == Some(callee) {
        return Some(construct_year_month(agent, args, new_target));
    }
    if intrinsics.get(PLAIN_MONTH_DAY).as_ref() == Some(callee) {
        return Some(construct_month_day(agent, args, new_target));
    }
    None
}

fn check_calendar(agent: &mut Agent, value: &Value) -> Result<Option<String>, JsError> {
    if matches!(value.kind(), ValueKind::Undefined) {
        return Ok(None);
    }
    // The constructors take a bare calendar identifier (CanonicalizeCalendar),
    // not the ISO-string forms a property-bag calendar accepts.
    let ValueKind::String(text) = value.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "calendar must be a string".into(),
        ));
    };
    let text = text.to_string_lossy();
    let Some(calendar) = super::canonicalize_calendar_id(&text) else {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "invalid calendar identifier".into(),
        ));
    };
    let _ = agent;
    Ok(Some(calendar))
}

/// Attach a [[Calendar]] slot to a freshly created Temporal instance and
/// return it unchanged.
fn with_calendar(
    agent: &mut Agent,
    value: Value,
    calendar: Option<&str>,
) -> Result<Value, JsError> {
    super::set_temporal_calendar(agent, &value, calendar);
    Ok(value)
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
    let calendar = check_calendar(agent, args.get(3).unwrap_or(&Value::Undefined))?;
    if !iso::is_valid_iso_date(y, m, d) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "invalid ISO date".into(),
        ));
    }
    let value = create_plain_date(agent, (y, m, d), new_target)?;
    super::set_temporal_calendar(agent, &value, calendar.as_deref());
    Ok(value)
}

pub fn create_plain_date(
    agent: &mut Agent,
    date: (i64, i64, i64),
    new_target: &Value,
) -> Result<Value, JsError> {
    let days = iso::iso_date_to_epoch_days(date.0, date.1 - 1, date.2);
    // ISODateWithinLimits: -271821-04-19 (epoch day -100_000_001) through
    // +275760-09-13 (epoch day +100_000_000).
    if !(-100_000_001..=100_000_000).contains(&days) {
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
        if !matches!(value.kind(), ValueKind::Undefined) {
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
        if !matches!(value.kind(), ValueKind::Undefined) {
            t[i] = super::to_integer_with_truncation(agent, value)?;
        }
    }
    let calendar = check_calendar(agent, args.get(9).unwrap_or(&Value::Undefined))?;
    if !iso::is_valid_iso_date(y, m, d) || !is_valid_time(t) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "invalid date-time".into(),
        ));
    }
    // CreateTemporalDateTimeSlots: RejectDateTimeRange.
    if !iso::iso_date_time_within_limits(y, m, d, t[0], t[1], t[2], t[3], t[4], t[5]) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "date-time out of range".into(),
        ));
    }
    let value = create_temporal_object(
        agent,
        new_target,
        PLAIN_DATE_TIME_PROTO,
        TemporalRecord::PlainDateTime([y, m, d, t[0], t[1], t[2], t[3], t[4], t[5]]),
    )?;
    super::set_temporal_calendar(agent, &value, calendar.as_deref());
    Ok(value)
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
    let tz = super::instant::to_constructor_time_zone_identifier(agent, &tz_value)?;
    let calendar = check_calendar(agent, args.get(2).unwrap_or(&Value::Undefined))?;
    let value = create_temporal_object(
        agent,
        new_target,
        ZONED_PROTO,
        TemporalRecord::ZonedDateTime(ns, JsString::from_utf8(&tz)),
    )?;
    super::set_temporal_calendar(agent, &value, calendar.as_deref());
    Ok(value)
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
    let record = require_record(agent, this, kind)?;
    let (year, month, day) = match &record {
        TemporalRecord::PlainDate(d) => (d[0], d[month_index], d[2]),
        TemporalRecord::PlainDateTime(dt) => (dt[0], dt[month_index], dt[2]),
        TemporalRecord::YearMonth(ym) => (ym[0], ym[month_index], ym[2]),
        TemporalRecord::MonthDay(md) => (md[0], md[month_index], md[2]),
        _ => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "brand check failed".into(),
            ));
        }
    };
    // The calendar month code of the date (hebrew M05L/shifted months,
    // the chinese/dangi leap months, and the non-ISO-month calendars — the
    // islamic and solar calendars' months are not the ISO months); the
    // ISO-month calendars fall back to the ISO month number.
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    if let Some((cy, cm, _)) = calendar::calendar_iso_to_date(&calendar, year, month, day) {
        return Ok(Value::String(Handle::new(JsString::from_utf8(
            &calendar::calendar_month_code(&calendar, cy, cm),
        ))));
    }
    Ok(Value::String(Handle::new(JsString::from_utf8(&format!(
        "M{month:02}"
    )))))
}

fn calendar_id(agent: &Agent, this: &Value, kind: RecordKind) -> Result<Value, JsError> {
    require_record(agent, this, kind)?;
    Ok(Value::String(Handle::new(super::temporal_calendar_id(
        agent, this,
    ))))
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
    let offset = super::offset_ns_at(tz, ns)
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
    let (ns, tz) = match require_record(agent, this, RecordKind::ZonedDateTime)? {
        TemporalRecord::ZonedDateTime(ns, tz) => (ns, tz.to_string_lossy()),
        _ => unreachable!(),
    };
    let offset = super::offset_ns_at(&tz, ns).unwrap_or(0);
    Ok(Value::String(Handle::new(JsString::from_utf8(
        &iso::format_offset_nanoseconds(offset),
    ))))
}

fn zoned_offset_ns(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let (ns, tz) = match require_record(agent, this, RecordKind::ZonedDateTime)? {
        TemporalRecord::ZonedDateTime(ns, tz) => (ns, tz.to_string_lossy()),
        _ => unreachable!(),
    };
    let offset = super::offset_ns_at(&tz, ns).unwrap_or(0);
    Ok(Value::Number(offset as f64))
}

/// The local monthCode getter (the calendar month code of the local date;
/// the ISO-month calendars fall back to the ISO month number).
fn zoned_month_code(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let (ns, tz) = zoned_parts(agent, this)?;
    let local = zoned_local(agent, ns, &tz)?;
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    if let Some((cy, cm, _)) = calendar::calendar_iso_to_date(&calendar, local.0, local.1, local.2)
    {
        return Ok(Value::String(Handle::new(JsString::from_utf8(
            &calendar::calendar_month_code(&calendar, cy, cm),
        ))));
    }
    Ok(Value::String(Handle::new(JsString::from_utf8(&format!(
        "M{:02}",
        local.1
    )))))
}

/// The first instant of the local day (y, m, d) in the zone (GetStartOfDay):
/// the earliest epoch whose local date is that day. The candidates come from
/// the two offsets in effect near the day's boundary (a transition can sit at
/// or near midnight), validated against the actual local time at the
/// candidate — the `00:00` local time may be skipped (a midnight gap) or
/// occur twice (a midnight overlap).
pub(super) fn zoned_start_of_day_ns(
    agent: &Agent,
    tz: &str,
    y: i64,
    m: i64,
    d: i64,
) -> Result<i128, JsError> {
    let wall = iso::get_utc_epoch_nanoseconds(y, m, d, 0, 0, 0, 0, 0, 0);
    if tz == "UTC" || iso::parse_date_time_utc_offset(tz).is_ok() {
        let start = wall - super::offset_time_zone_offset_ns(tz).unwrap_or(0);
        // GetStartOfDay: the start instant must be within the
        // epoch-nanosecond range (test262 get-start-of-day-throws.js: the
        // extreme dates' midnight sits at or beyond the Instant bounds).
        if !(iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(&start) {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "start of day is out of range".into(),
            ));
        }
        return Ok(start);
    }
    let zone = unicode::tz::resolve_zone(tz)
        .ok_or_else(|| JsError::new(ErrorKind::RangeError, "unsupported time zone".into()))?;
    let day = 86_400_000_000_000i128;
    let (o1, ..) = unicode::tz::offset_info_at(zone, wall - day);
    let (o2, ..) = unicode::tz::offset_info_at(zone, wall + day);
    let mut best: Option<i128> = None;
    for offset_secs in [o1, o2] {
        let epoch = wall - offset_secs as i128 * 1_000_000_000;
        let local = zoned_local(agent, epoch, tz)?;
        // The candidate is the local midnight iff its local time is exactly
        // 00:00:00.000000000 (a gap can make the offset at the candidate
        // differ from the one that produced it, landing on 01:00 — the
        // Toronto 1919 midnight gap).
        let midnight = local.3 == 0
            && local.4 == 0
            && local.5 == 0
            && local.6 == 0
            && local.7 == 0
            && local.8 == 0;
        if midnight
            && local.0 == y
            && local.1 == m
            && local.2 == d
            && best.is_none_or(|b| epoch < b)
        {
            best = Some(epoch);
        }
    }
    // A midnight gap (a spring-forward at or before midnight skips 00:00 —
    // test262 dst-skipped-cross-midnight.js): the day starts at the
    // transition instant, the first valid local time (GetStartOfDay falls
    // back to the earliest local time after the transition; the compatible
    // mapping of 00:00 would land half an hour late).
    let start = best.unwrap_or_else(|| {
        let e_before = wall - o1 as i128 * 1_000_000_000;
        let e_after = wall - o2 as i128 * 1_000_000_000;
        let lo = e_before.min(e_after);
        let hi = e_before.max(e_after);
        unicode::tz::next_transition(zone, lo)
            .filter(|t| {
                let at = t.at_secs as i128 * 1_000_000_000;
                at > lo && at <= hi
            })
            .map(|t| t.at_secs as i128 * 1_000_000_000)
            .unwrap_or_else(|| {
                let (offset_secs, ..) = unicode::tz::offset_info_at(zone, wall);
                wall - offset_secs as i128 * 1_000_000_000
            })
    });
    // GetStartOfDay: the start instant must be within the epoch-nanosecond
    // range (test262 get-start-of-day-throws.js: the extreme dates' midnight
    // sits at or beyond the Instant bounds).
    if !(iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(&start) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "start of day is out of range".into(),
        ));
    }
    Ok(start)
}

/// GetPossibleInstantsFor (spec 6.5.2): the instants whose local date-time in
/// the zone is the wall date-time — 0 (a skipped interval), 1, or 2 (an
/// overlap). The candidate offsets are those in effect at the wall instant
/// and a day either side (a transition can shift the wall clock by up to a
/// day — the Apia dateline jump); each candidate is validated against the
/// zone's offset at the candidate, which pins which side of the transition
/// it sits on.
#[allow(clippy::too_many_arguments)]
fn possible_instants_for_wall(
    tz: &str,
    y: i64,
    m: i64,
    d: i64,
    h: i64,
    min: i64,
    s: i64,
    ms: i64,
    us: i64,
    ns: i64,
) -> Result<Vec<i128>, JsError> {
    let utc_wall = iso::get_utc_epoch_nanoseconds(y, m, d, h, min, s, ms, us, ns);
    if tz == "UTC" || iso::parse_date_time_utc_offset(tz).is_ok() {
        let offset = super::offset_time_zone_offset_ns(tz).unwrap_or(0);
        return Ok(vec![utc_wall - offset]);
    }
    let zone = unicode::tz::resolve_zone(tz)
        .ok_or_else(|| JsError::new(ErrorKind::RangeError, "unsupported time zone".into()))?;
    let day = 86_400_000_000_000i128;
    let mut offsets: Vec<i32> = [utc_wall - day, utc_wall, utc_wall + day]
        .iter()
        .map(|t| unicode::tz::offset_info_at(zone, *t).0)
        .collect();
    offsets.sort_unstable();
    offsets.dedup();
    let mut possible = Vec::with_capacity(2);
    for offset_secs in offsets {
        let epoch = utc_wall - offset_secs as i128 * 1_000_000_000;
        // The candidate is a real local time iff the zone's offset at the
        // candidate is the offset that produced it.
        if unicode::tz::offset_info_at(zone, epoch).0 == offset_secs {
            possible.push(epoch);
        }
    }
    possible.sort_unstable();
    Ok(possible)
}

/// DisambiguatePossibleInstants (spec 6.5.3): a single instant wins; an
/// overlap picks the first for compatible/earlier and the last for later
/// (reject throws); a skipped interval resolves to the wall time with the
/// offset in effect just before the gap (compatible/later — the legacy-Date
/// mapping) or just after (earlier), and reject throws.
fn disambiguate_possible_instants(
    possible: &[i128],
    tz: &str,
    utc_wall: i128,
    disambiguation: &str,
) -> Result<i128, JsError> {
    if !possible.is_empty() {
        if disambiguation == "reject" && possible.len() > 1 {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "no such local time exists".into(),
            ));
        }
        return if disambiguation == "later" {
            Ok(possible[possible.len() - 1])
        } else {
            Ok(possible[0])
        };
    }
    if disambiguation == "reject" {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "no such local time exists".into(),
        ));
    }
    // The gap: the offsets in effect a day either side of the wall instant
    // bracket the transition (the wall time is always within the offset
    // difference of it, under a day). Compatible/later apply the offset
    // before the gap, earlier the offset after.
    let zone = unicode::tz::resolve_zone(tz)
        .ok_or_else(|| JsError::new(ErrorKind::RangeError, "unsupported time zone".into()))?;
    let day = 86_400_000_000_000i128;
    let before = unicode::tz::offset_info_at(zone, utc_wall - day).0 as i128 * 1_000_000_000;
    let after = unicode::tz::offset_info_at(zone, utc_wall + day).0 as i128 * 1_000_000_000;
    let e_before = utc_wall - before;
    let e_after = utc_wall - after;
    Ok(if disambiguation == "earlier" {
        e_after.min(e_before)
    } else {
        e_after.max(e_before)
    })
}

/// GetEpochNanosecondsFor (spec 6.5.3): the possible instants for the wall
/// date-time, disambiguated. The shared primitive behind `from`, `with`,
/// `round`, `withPlainTime`, the AddZonedDateTime intermediate, and the
/// Duration relativeTo machinery.
#[allow(clippy::too_many_arguments)]
pub(super) fn wall_to_epoch_ns(
    tz: &str,
    y: i64,
    m: i64,
    d: i64,
    h: i64,
    min: i64,
    s: i64,
    ms: i64,
    us: i64,
    ns: i64,
    disambiguation: &str,
) -> Result<i128, JsError> {
    let utc_wall = iso::get_utc_epoch_nanoseconds(y, m, d, h, min, s, ms, us, ns);
    let possible = possible_instants_for_wall(tz, y, m, d, h, min, s, ms, us, ns)?;
    disambiguate_possible_instants(&possible, tz, utc_wall, disambiguation)
}

/// The ISO calendar's derived ZonedDateTime getters (computed from the local
/// date part; hoursInDay diffs the two GetStartOfDay instants).
fn zoned_calendar_field(agent: &mut Agent, this: &Value, name: &str) -> Result<Value, JsError> {
    let (ns, tz) = zoned_parts(agent, this)?;
    let local = zoned_local(agent, ns, &tz)?;
    if name == "hoursInDay" {
        // The local day length: the difference between the start instants of
        // the local date and the next date (a DST-transition day differs
        // from 24h).
        let today = zoned_start_of_day_ns(agent, &tz, local.0, local.1, local.2)?;
        let next = iso::add_days_to_iso_date(local.0, local.1, local.2, 1);
        let tomorrow = zoned_start_of_day_ns(agent, &tz, next.0, next.1, next.2)?;
        if !(iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(&today)
            || !(iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(&tomorrow)
        {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "day boundary is out of range".into(),
            ));
        }
        return Ok(Value::Number(
            (tomorrow - today) as f64 / iso::NS_PER_HOUR as f64,
        ));
    }
    calendar_field_value(
        &super::temporal_calendar_id(agent, this).to_string_lossy(),
        local.0,
        local.1,
        local.2,
        name,
    )
}

fn zoned_to_instant(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let (ns, _) = match require_record(agent, this, RecordKind::ZonedDateTime)? {
        TemporalRecord::ZonedDateTime(ns, tz) => (ns, tz),
        _ => unreachable!(),
    };
    super::instant::create_instant(agent, ns, &Value::Undefined)
}

/// spec 3.5.4 ToTemporalDate (no options — used by compare/equals/until/since).
pub fn to_plain_date(agent: &mut Agent, item: &Value) -> Result<Value, JsError> {
    to_plain_date_with_options(agent, item, &Value::Undefined)
}

/// spec 3.5.4 ToTemporalDate: records, strings, and plain property bags, with
/// the `from` options (the overflow option is read after the fields).
pub fn to_plain_date_with_options(
    agent: &mut Agent,
    item: &Value,
    options: &Value,
) -> Result<Value, JsError> {
    if let ValueKind::Object(obj) = item.kind() {
        if let Some(record) = agent.temporal_data.get(&obj.id()).cloned() {
            let calendar = super::temporal_calendar_id(agent, item);
            return match &record {
                TemporalRecord::PlainDate(d) => {
                    let opts = super::get_options_object(options)?;
                    super::get_temporal_overflow_option(agent, &opts)?;
                    let value = create_plain_date(agent, (d[0], d[1], d[2]), &Value::Undefined)?;
                    with_calendar(agent, value, Some(&calendar.to_string_lossy()))
                }
                TemporalRecord::PlainDateTime(dt) => {
                    let opts = super::get_options_object(options)?;
                    super::get_temporal_overflow_option(agent, &opts)?;
                    let value = create_plain_date(agent, (dt[0], dt[1], dt[2]), &Value::Undefined)?;
                    with_calendar(agent, value, Some(&calendar.to_string_lossy()))
                }
                TemporalRecord::ZonedDateTime(ns, tz) => {
                    let opts = super::get_options_object(options)?;
                    super::get_temporal_overflow_option(agent, &opts)?;
                    let local = zoned_local(agent, *ns, &tz.to_string_lossy())?;
                    let value =
                        create_plain_date(agent, (local.0, local.1, local.2), &Value::Undefined)?;
                    with_calendar(agent, value, Some(&calendar.to_string_lossy()))
                }
                _ => Err(JsError::new(
                    ErrorKind::TypeError,
                    "value is not convertible to a PlainDate".into(),
                )),
            };
        }
        // Property bag: the calendar is read first (GetTemporalCalendar
        // IdentifierWithISODefault), then the fields in ascending code point
        // order, then the options, then the algorithmic validation.
        let calendar = read_bag_calendar(agent, item)?;
        let mut year = None;
        let mut month = None;
        let mut month_code = None;
        let mut day = None;
        for key in ["day", "month", "monthCode", "year"] {
            let value =
                crate::context::get_property(agent, item, &JsString::from_utf8(key), *item)?;
            if matches!(value.kind(), ValueKind::Undefined) {
                continue;
            }
            match key {
                "day" => day = Some(super::to_positive_integer_with_truncation(agent, &value)?),
                "month" => month = Some(super::to_positive_integer_with_truncation(agent, &value)?),
                "monthCode" => month_code = Some(read_month_code(agent, &value)?),
                _ => year = Some(super::to_integer_with_truncation(agent, &value)?),
            }
        }
        let year = read_era_fields(agent, item, calendar.as_deref(), year)?;
        let options = super::get_options_object(options)?;
        let constrain =
            super::get_temporal_overflow_option(agent, &options)? == Overflow::Constrain;
        return date_from_merged_fields(agent, year, month, month_code, day, constrain, calendar);
    }
    if !matches!(item.kind(), ValueKind::String(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "value must be a string or object".into(),
        ));
    }
    let text = crate::context::to_string(agent, item)?;
    let parsed = iso::parse_iso_date_time(text.as_slice(), iso::Format::DateTimePlain)
        .map_err(|_| JsError::new(ErrorKind::RangeError, "invalid date string".into()))?;
    if parsed.tz.z {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "Z designator not supported for PlainDate".into(),
        ));
    }
    // The u-ca annotation must name a supported calendar (the future-
    // calendar fixtures: a not-yet-supported or unknown id is a RangeError).
    let calendar = match parsed.calendar.as_deref() {
        Some(c) => Some(super::canonicalize_calendar_id(c).ok_or_else(|| {
            JsError::new(ErrorKind::RangeError, "invalid calendar identifier".into())
        })?),
        None => None,
    };
    let options = super::get_options_object(options)?;
    super::get_temporal_overflow_option(agent, &options)?;
    let value = create_plain_date(
        agent,
        (parsed.year, parsed.month, parsed.day),
        &Value::Undefined,
    )?;
    with_calendar(agent, value, calendar.as_deref())
}

/// spec 4.5.6 ToTemporalTime (no options — used by toPlainDateTime /
/// toZonedDateTime).
pub fn to_plain_time(agent: &mut Agent, item: &Value) -> Result<Value, JsError> {
    to_plain_time_with_options(agent, item, &Value::Undefined)
}

/// spec 4.5.6 ToTemporalTime: records, strings (TemporalTimeString), and
/// plain property bags (regulated with the overflow option).
pub fn to_plain_time_with_options(
    agent: &mut Agent,
    item: &Value,
    options: &Value,
) -> Result<Value, JsError> {
    if let ValueKind::Object(obj) = item.kind() {
        if let Some(record) = agent.temporal_data.get(&obj.id()).cloned() {
            let opts = super::get_options_object(options)?;
            super::get_temporal_overflow_option(agent, &opts)?;
            return match &record {
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
        // Property bag: at least one field must be present (ToTemporalTime-
        // Record), and the values are regulated with the overflow option.
        let partial = read_partial_time_fields(agent, item)?;
        let mut t = [0i64; 6];
        for (i, value) in partial.iter().enumerate() {
            if let Some(v) = value {
                t[i] = *v;
            }
        }
        let options = super::get_options_object(options)?;
        let constrain =
            super::get_temporal_overflow_option(agent, &options)? == Overflow::Constrain;
        regulate_time(&mut t, constrain)?;
        return create_plain_time(agent, t, &Value::Undefined);
    }
    if !matches!(item.kind(), ValueKind::String(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "value must be a string or object".into(),
        ));
    }
    let text = crate::context::to_string(agent, item)?;
    let text = text.to_string_lossy();
    let units: Vec<u16> = text.encode_utf16().collect();
    let parsed = match iso::parse_iso_date_time(&units, iso::Format::TimeString) {
        Ok(parsed) => parsed,
        Err(_) => iso::parse_iso_date_time(&units, iso::Format::DateTimePlain)
            .map_err(|_| JsError::new(ErrorKind::RangeError, "invalid time string".into()))?,
    };
    let options = super::get_options_object(options)?;
    super::get_temporal_overflow_option(agent, &options)?;
    let t = parsed
        .time
        .ok_or_else(|| JsError::new(ErrorKind::RangeError, "time is missing in string".into()))?;
    // spec 13.34: a time string lacking the T/space designator is rejected
    // when it would also parse as a PlainMonthDay or PlainYearMonth string
    // (calendar annotations are ignored by the time conversion).
    if !has_time_designator(&text) {
        let core = text.split_once('[').map_or(text.as_ref(), |(core, _)| core);
        if loose_year_month(core) || loose_month_day(core) {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "ambiguous time string; use a T prefix".into(),
            ));
        }
    }
    create_plain_time(agent, t, &Value::Undefined)
}

/// spec 5.5.6 ToTemporalDateTime: records, strings (TemporalDateTimeString),
/// and property bags (the calendar, then the date/time fields, regulated with
/// the overflow option).
pub fn to_plain_date_time(
    agent: &mut Agent,
    item: &Value,
    options: &Value,
) -> Result<Value, JsError> {
    if let ValueKind::Object(obj) = item.kind()
        && let Some(record) = agent.temporal_data.get(&obj.id()).cloned()
    {
        let opts = super::get_options_object(options)?;
        super::get_temporal_overflow_option(agent, &opts)?;
        let calendar = super::temporal_calendar_id(agent, item);
        let value = match record {
            TemporalRecord::PlainDateTime(dt) => create_temporal_object(
                agent,
                &Value::Undefined,
                PLAIN_DATE_TIME_PROTO,
                TemporalRecord::PlainDateTime(dt),
            ),
            TemporalRecord::PlainDate(d) => create_temporal_object(
                agent,
                &Value::Undefined,
                PLAIN_DATE_TIME_PROTO,
                TemporalRecord::PlainDateTime([d[0], d[1], d[2], 0, 0, 0, 0, 0, 0]),
            ),
            TemporalRecord::ZonedDateTime(ns, tz) => {
                let local = zoned_local(agent, ns, &tz.to_string_lossy())?;
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
        }?;
        return with_calendar(agent, value, Some(&calendar.to_string_lossy()));
    }
    if matches!(item.kind(), ValueKind::String(_)) {
        let text = crate::context::to_string(agent, item)?;
        let parsed = iso::parse_iso_date_time(text.as_slice(), iso::Format::DateTimePlain)
            .map_err(|_| JsError::new(ErrorKind::RangeError, "invalid date-time string".into()))?;
        if parsed.tz.z {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "Z designator not supported for PlainDateTime".into(),
            ));
        }
        let calendar = match parsed.calendar.as_deref() {
            Some(c) => Some(super::canonicalize_calendar_id(c).ok_or_else(|| {
                JsError::new(ErrorKind::RangeError, "invalid calendar identifier".into())
            })?),
            None => None,
        };
        let opts = super::get_options_object(options)?;
        super::get_temporal_overflow_option(agent, &opts)?;
        let t = parsed.time.unwrap_or([0, 0, 0, 0, 0, 0]);
        // CreateTemporalDateTime: RejectDateTimeRange.
        if !iso::iso_date_time_within_limits(
            parsed.year,
            parsed.month,
            parsed.day,
            t[0],
            t[1],
            t[2],
            t[3],
            t[4],
            t[5],
        ) {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "date-time out of range".into(),
            ));
        }
        let value = create_temporal_object(
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
        )?;
        return with_calendar(agent, value, calendar.as_deref());
    }
    if let ValueKind::Object(_) = item.kind() {
        // Property bag: the calendar (iso8601 default), then all ten fields
        // read in ascending code point order (PrepareCalendarFields complete).
        let calendar = read_bag_calendar(agent, item)?;
        let calendar_id = calendar.as_deref().unwrap_or("iso8601");
        let (year, month, month_code, day, t) =
            read_date_time_fields(agent, item, false, calendar_id)?;
        let year = read_era_fields(agent, item, calendar.as_deref(), year)?;
        let opts = super::get_options_object(options)?;
        let constrain = super::get_temporal_overflow_option(agent, &opts)? == Overflow::Constrain;
        let (y, m, d) =
            resolve_date_fields(year, month, month_code, day, constrain, calendar.as_deref())?;
        // CalendarDateFromFields for the tabular Islamic calendars.
        let (y, m, d) = if let Some(calendar) = calendar.as_deref()
            && let Some((cy, cm, cd)) = calendar::calendar_date_to_iso(calendar, y, m, d)
        {
            (cy, cm, cd)
        } else {
            (y, m, d)
        };
        // Missing time fields default to midnight (PrepareCalendarFields
        // complete mode).
        let mut time = t.map(|v| v.unwrap_or(0));
        regulate_time(&mut time, constrain)?;
        if !iso::iso_date_time_within_limits(
            y, m, d, time[0], time[1], time[2], time[3], time[4], time[5],
        ) {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "date-time out of range".into(),
            ));
        }
        let value = create_temporal_object(
            agent,
            &Value::Undefined,
            PLAIN_DATE_TIME_PROTO,
            TemporalRecord::PlainDateTime([
                y, m, d, time[0], time[1], time[2], time[3], time[4], time[5],
            ]),
        )?;
        return with_calendar(agent, value, calendar.as_deref());
    }
    Err(JsError::new(
        ErrorKind::TypeError,
        "value must be a string or object".into(),
    ))
}

/// spec 6.5.2 ToTemporalZonedDateTime (minimal: records, strings).
/// PrepareCalendarFields(iso8601, bag, «year, month, monthCode, day», «hour,
/// minute, second, millisecond, microsecond, nanosecond, offset, timeZone»,
/// «timeZone»): read in ascending code point order with the casts; the time
/// zone is required.
struct ZonedFields {
    year: Option<i64>,
    month: Option<i64>,
    month_code: Option<String>,
    day: Option<i64>,
    time: [Option<i64>; 6],
    offset: Option<String>,
    time_zone: Option<String>,
}

fn read_zoned_fields(agent: &mut Agent, bag: &Value) -> Result<ZonedFields, JsError> {
    let mut fields = ZonedFields {
        year: None,
        month: None,
        month_code: None,
        day: None,
        time: [None; 6],
        offset: None,
        time_zone: None,
    };
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
        let value = crate::context::get_property(agent, bag, &JsString::from_utf8(key), *bag)?;
        if matches!(value.kind(), ValueKind::Undefined) {
            continue;
        }
        match key {
            "day" => fields.day = Some(super::to_positive_integer_with_truncation(agent, &value)?),
            "hour" => fields.time[0] = Some(super::to_integer_with_truncation(agent, &value)?),
            "microsecond" => {
                fields.time[4] = Some(super::to_integer_with_truncation(agent, &value)?)
            }
            "millisecond" => {
                fields.time[3] = Some(super::to_integer_with_truncation(agent, &value)?)
            }
            "minute" => fields.time[1] = Some(super::to_integer_with_truncation(agent, &value)?),
            "month" => {
                fields.month = Some(super::to_positive_integer_with_truncation(agent, &value)?)
            }
            "monthCode" => fields.month_code = Some(read_month_code(agent, &value)?),
            "nanosecond" => {
                fields.time[5] = Some(super::to_integer_with_truncation(agent, &value)?)
            }
            "offset" => {
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
                iso::parse_date_time_utc_offset(&text)
                    .map_err(|_| JsError::new(ErrorKind::RangeError, "invalid offset".into()))?;
                fields.offset = Some(text);
            }
            "second" => fields.time[2] = Some(super::to_integer_with_truncation(agent, &value)?),
            "timeZone" => {
                fields.time_zone = Some(super::instant::to_temporal_time_zone_identifier(
                    agent, &value,
                )?)
            }
            _ => fields.year = Some(super::to_integer_with_truncation(agent, &value)?),
        }
    }
    Ok(fields)
}

/// PrepareCalendarFields(iso8601, like, «year, month, monthCode, day», «hour,
/// minute, second, millisecond, microsecond, nanosecond, offset», partial):
/// the `with` field read in ascending code point order (no timeZone/calendar).
#[allow(clippy::type_complexity)]
fn read_zoned_with_fields(
    agent: &mut Agent,
    bag: &Value,
    calendar: &str,
) -> Result<
    (
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<i64>,
        [Option<i64>; 6],
        Option<String>,
    ),
    JsError,
> {
    let mut any = false;
    let mut year = None;
    let mut month = None;
    let mut month_code = None;
    let mut day = None;
    let mut t = [None; 6];
    let mut offset = None;
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
        "year",
    ] {
        let value = crate::context::get_property(agent, bag, &JsString::from_utf8(key), *bag)?;
        if matches!(value.kind(), ValueKind::Undefined) {
            continue;
        }
        any = true;
        match key {
            "day" => day = Some(super::to_positive_integer_with_truncation(agent, &value)?),
            "hour" => t[0] = Some(super::to_integer_with_truncation(agent, &value)?),
            "microsecond" => t[4] = Some(super::to_integer_with_truncation(agent, &value)?),
            "millisecond" => t[3] = Some(super::to_integer_with_truncation(agent, &value)?),
            "minute" => t[1] = Some(super::to_integer_with_truncation(agent, &value)?),
            "month" => month = Some(super::to_positive_integer_with_truncation(agent, &value)?),
            "monthCode" => month_code = Some(read_month_code(agent, &value)?),
            "nanosecond" => t[5] = Some(super::to_integer_with_truncation(agent, &value)?),
            "offset" => {
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
                iso::parse_date_time_utc_offset(&text)
                    .map_err(|_| JsError::new(ErrorKind::RangeError, "invalid offset".into()))?;
                offset = Some(text);
            }
            "second" => t[2] = Some(super::to_integer_with_truncation(agent, &value)?),
            _ => year = Some(super::to_integer_with_truncation(agent, &value)?),
        }
    }
    if !any && !bag_has_era_fields(agent, bag, calendar)? {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "no supported properties found".into(),
        ));
    }
    Ok((year, month, month_code, day, t, offset))
}

/// InterpretISODateTimeOffset (spec 6.5.10): the wall date-time plus the
/// offset behaviour resolve to an epoch nanosecond. The exact (Z) and "use"
/// behaviours apply the given offset directly; the wall and "ignore"
/// behaviours disambiguate the zone's possible instants; "prefer" returns
/// the given offset when it is one of the possible instants and otherwise
/// falls back to the disambiguation; "reject" throws when the given offset
/// is not a possible instant. `match_minutes` (a string parse, spec 6.5.1)
/// accepts a possible instant whose offset rounds to the given offset at
/// minute precision — a sub-minute zone offset like -06:36:36 matches the
/// string offset -06:36.
#[allow(clippy::too_many_arguments)]
pub(super) fn interpret_iso_date_time_offset(
    dt: [i64; 9],
    tz: &str,
    offset_ns: Option<i128>,
    has_z: bool,
    offset_opt: &str,
    disambiguation: &str,
    match_minutes: bool,
) -> Result<i128, JsError> {
    let utc_wall = iso::get_utc_epoch_nanoseconds(
        dt[0], dt[1], dt[2], dt[3], dt[4], dt[5], dt[6], dt[7], dt[8],
    );
    // offsetBehaviour: exact (Z), wall (no offset), or option (offset given).
    let exact = has_z;
    let wall = !has_z && offset_ns.is_none();
    let option = !has_z && offset_ns.is_some();
    if exact || (option && offset_opt == "use") {
        // BalanceISODateTime(wall - offset), CheckISODaysRange, then the
        // epoch range check. "use" applies the given offset regardless of
        // the zone (the result is displayed with the zone's own offset).
        let given = if exact { 0 } else { offset_ns.unwrap() };
        let epoch = utc_wall - given;
        check_balanced_range(epoch)?;
        return Ok(epoch);
    }
    if wall || offset_opt == "ignore" {
        // GetEpochNanosecondsFor without an offset: the possible instants,
        // disambiguated (test262 from/argument-string-dst-option-
        // disambiguation.js pins the gap/overlap choices).
        let epoch = wall_to_epoch_ns(
            tz,
            dt[0],
            dt[1],
            dt[2],
            dt[3],
            dt[4],
            dt[5],
            dt[6],
            dt[7],
            dt[8],
            disambiguation,
        )?;
        check_balanced_range(epoch)?;
        return Ok(epoch);
    }
    // offsetBehaviour option: the given offset wins when it matches one of
    // the possible instants (match-minutes accepts a minute-rounded offset
    // for a sub-minute zone); "prefer" then falls back to the disambiguation
    // and "reject" throws.
    check_iso_days_range(iso::iso_date_to_epoch_days(dt[0], dt[1] - 1, dt[2]))?;
    let given = offset_ns.unwrap();
    let possible = possible_instants_for_wall(
        tz, dt[0], dt[1], dt[2], dt[3], dt[4], dt[5], dt[6], dt[7], dt[8],
    )?;
    for p in &possible {
        let candidate_offset = utc_wall - p;
        if candidate_offset == given
            || (match_minutes
                && iso::round_number_to_increment(
                    candidate_offset,
                    60 * 1_000_000_000,
                    RoundingMode::HalfExpand,
                ) == given)
        {
            check_balanced_range(*p)?;
            return Ok(*p);
        }
    }
    if offset_opt == "reject" {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "offset does not match the time zone".into(),
        ));
    }
    let epoch = disambiguate_possible_instants(&possible, tz, utc_wall, disambiguation)?;
    check_balanced_range(epoch)?;
    Ok(epoch)
}

/// CheckISODaysRange (spec): the ISO date must be strictly within ±10^8 days
/// of the epoch.
fn check_iso_days_range(days: i64) -> Result<(), JsError> {
    if days.abs() > 100_000_000 {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "date is outside the representable range".into(),
        ));
    }
    Ok(())
}

/// The CheckISODaysRange + IsValidEpochNanoseconds pair over a balanced
/// wall-minus-offset instant (spec GetPossibleEpochNanoseconds).
fn check_balanced_range(epoch: i128) -> Result<(), JsError> {
    let (y, m, d, _, _, _, _, _, _) = iso::iso_parts_from_epoch(epoch);
    check_iso_days_range(iso::iso_date_to_epoch_days(y, m - 1, d))?;
    if !(iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(&epoch) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "result is out of range".into(),
        ));
    }
    Ok(())
}

/// Whether an offset string specifies seconds (spec 6.5.2: a second
/// MinuteSecond Parse Node makes the offset match-exactly rather than
/// match-minutes).
pub(super) fn offset_string_has_seconds(text: &str) -> bool {
    let digits = text.trim_start_matches(['+', '-']);
    if digits.contains(':') {
        digits.matches(':').count() >= 2
    } else {
        digits.len() > 4
    }
}

/// spec 6.5.2 ToTemporalZonedDateTime: records, strings (ZonedDateTimeString),
/// and property bags (the calendar, the date/time/offset/timeZone fields, and
/// the disambiguation/offset/overflow options). Only UTC and fixed-offset
/// zones are supported.
pub fn to_zoned(agent: &mut Agent, item: &Value, options: &Value) -> Result<Value, JsError> {
    if let ValueKind::Object(obj) = item.kind()
        && let Some(TemporalRecord::ZonedDateTime(ns, tz)) =
            agent.temporal_data.get(&obj.id()).cloned()
    {
        let opts = super::get_options_object(options)?;
        let _ = super::get_temporal_disambiguation_option(agent, &opts)?;
        super::get_temporal_offset_option(agent, &opts, "reject")?;
        super::get_temporal_overflow_option(agent, &opts)?;
        let calendar = super::temporal_calendar_id(agent, item);
        let value = create_temporal_object(
            agent,
            &Value::Undefined,
            ZONED_PROTO,
            TemporalRecord::ZonedDateTime(ns, tz),
        )?;
        return with_calendar(agent, value, Some(&calendar.to_string_lossy()));
    }
    if matches!(item.kind(), ValueKind::String(_)) {
        let text = crate::context::to_string(agent, item)?;
        let parsed = iso::parse_iso_date_time(text.as_slice(), iso::Format::DateTimeZoned)
            .map_err(|_| {
                JsError::new(
                    ErrorKind::RangeError,
                    "invalid zoned date-time string".into(),
                )
            })?;
        let calendar = match parsed.calendar.as_deref() {
            Some(c) => Some(super::canonicalize_calendar_id(c).ok_or_else(|| {
                JsError::new(ErrorKind::RangeError, "invalid calendar identifier".into())
            })?),
            None => None,
        };
        let tz = super::instant::to_temporal_time_zone_identifier(
            agent,
            &Value::String(Handle::new(JsString::from_utf8(&parsed.tz.annotation))),
        )?;
        let opts = super::get_options_object(options)?;
        let disambiguation = super::get_temporal_disambiguation_option(agent, &opts)?;
        let offset_opt = super::get_temporal_offset_option(agent, &opts, "reject")?;
        super::get_temporal_overflow_option(agent, &opts)?;
        if parsed.time.is_none() {
            // Start-of-day (spec 6.5.1 InterpretISODateTimeOffset): a string
            // with no time component resolves through GetStartOfDay, not the
            // disambiguation of 00:00 (test262 dst-skipped-cross-midnight).
            let epoch = zoned_start_of_day_ns(agent, &tz, parsed.year, parsed.month, parsed.day)?;
            let value = create_temporal_object(
                agent,
                &Value::Undefined,
                ZONED_PROTO,
                TemporalRecord::ZonedDateTime(epoch, JsString::from_utf8(&tz)),
            )?;
            return with_calendar(agent, value, calendar.as_deref());
        }
        let t = parsed.time.unwrap_or([0, 0, 0, 0, 0, 0]);
        let dt = [
            parsed.year,
            parsed.month,
            parsed.day,
            t[0],
            t[1],
            t[2],
            t[3],
            t[4],
            t[5],
        ];
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
        let epoch = interpret_iso_date_time_offset(
            dt,
            &tz,
            offset_ns,
            parsed.tz.z,
            &offset_opt,
            &disambiguation,
            !offset_string_has_seconds(&parsed.tz.offset_string),
        )?;
        let value = create_temporal_object(
            agent,
            &Value::Undefined,
            ZONED_PROTO,
            TemporalRecord::ZonedDateTime(epoch, JsString::from_utf8(&tz)),
        )?;
        return with_calendar(agent, value, calendar.as_deref());
    }
    if let ValueKind::Object(_) = item.kind() {
        // Property bag: the calendar, then the fields in ascending code point
        // order (the time zone is required), then the options.
        let calendar = read_bag_calendar(agent, item)?;
        let mut fields = read_zoned_fields(agent, item)?;
        fields.year = read_era_fields(agent, item, calendar.as_deref(), fields.year)?;
        let Some(tz) = fields.time_zone else {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "timeZone is required".into(),
            ));
        };
        let opts = super::get_options_object(options)?;
        let disambiguation = super::get_temporal_disambiguation_option(agent, &opts)?;
        let offset_opt = super::get_temporal_offset_option(agent, &opts, "reject")?;
        let constrain = super::get_temporal_overflow_option(agent, &opts)? == Overflow::Constrain;
        // InterpretTemporalDateTimeFields: resolve the date and regulate the
        // time with the overflow option.
        let (y, m, d) = resolve_date_fields(
            fields.year,
            fields.month,
            fields.month_code,
            fields.day,
            constrain,
            calendar.as_deref(),
        )?;
        // CalendarDateFromFields for the tabular Islamic calendars.
        let (y, m, d) = if let Some(calendar) = calendar.as_deref()
            && let Some((cy, cm, cd)) = calendar::calendar_date_to_iso(calendar, y, m, d)
        {
            (cy, cm, cd)
        } else {
            (y, m, d)
        };
        let mut time = fields.time.map(|v| v.unwrap_or(0));
        regulate_time(&mut time, constrain)?;
        if !iso::iso_date_time_within_limits(
            y, m, d, time[0], time[1], time[2], time[3], time[4], time[5],
        ) {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "date-time out of range".into(),
            ));
        }
        let dt = [
            y, m, d, time[0], time[1], time[2], time[3], time[4], time[5],
        ];
        let offset_ns = match &fields.offset {
            Some(text) => Some(
                iso::parse_date_time_utc_offset(text)
                    .map_err(|_| JsError::new(ErrorKind::RangeError, "invalid offset".into()))?,
            ),
            None => None,
        };
        let epoch = interpret_iso_date_time_offset(
            dt,
            &tz,
            offset_ns,
            false,
            &offset_opt,
            &disambiguation,
            false,
        )?;
        let value = create_temporal_object(
            agent,
            &Value::Undefined,
            ZONED_PROTO,
            TemporalRecord::ZonedDateTime(epoch, JsString::from_utf8(&tz)),
        )?;
        return with_calendar(agent, value, calendar.as_deref());
    }
    Err(JsError::new(
        ErrorKind::TypeError,
        "value must be a string or object".into(),
    ))
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

fn year_month_compare_key(record: &TemporalRecord) -> Option<[i64; 3]> {
    match record {
        TemporalRecord::YearMonth(ym) => Some(*ym),
        _ => None,
    }
}

fn month_day_compare_key(record: &TemporalRecord) -> Option<[i64; 3]> {
    match record {
        TemporalRecord::MonthDay(md) => Some(*md),
        _ => None,
    }
}

fn compare_records<T: Ord>(
    agent: &mut Agent,
    args: &[Value],
    kind: RecordKind,
    key_of: fn(&TemporalRecord) -> Option<T>,
) -> Result<Value, JsError> {
    // The corpus pins the current spec: compare does NOT take the calendar
    // into account (e.g. PlainYearMonth/compare/compare-calendar.js,
    // PlainDateTime/compare/calendar-ignored.js, ZonedDateTime/compare/*).
    let (one, _) = to_compare_value(
        agent,
        args.first().cloned().unwrap_or(Value::Undefined),
        kind,
    )?;
    let (two, _) = to_compare_value(
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
) -> Result<(TemporalRecord, JsString), JsError> {
    match kind {
        RecordKind::PlainDate => {
            let value = to_plain_date(agent, &item)?;
            let cal = super::temporal_calendar_id(agent, &value);
            let record = match value.kind() {
                ValueKind::Object(obj) => {
                    agent.temporal_data.get(&obj.id()).cloned().ok_or_else(|| {
                        JsError::new(ErrorKind::TypeError, "brand check failed".into())
                    })
                }
                _ => unreachable!(),
            }?;
            Ok((record, cal))
        }
        RecordKind::PlainTime => {
            let value = to_plain_time(agent, &item)?;
            let cal = super::temporal_calendar_id(agent, &value);
            let record = match value.kind() {
                ValueKind::Object(obj) => {
                    agent.temporal_data.get(&obj.id()).cloned().ok_or_else(|| {
                        JsError::new(ErrorKind::TypeError, "brand check failed".into())
                    })
                }
                _ => unreachable!(),
            }?;
            Ok((record, cal))
        }
        RecordKind::PlainDateTime => {
            let value = to_plain_date_time(agent, &item, &Value::Undefined)?;
            let cal = super::temporal_calendar_id(agent, &value);
            let record = match value.kind() {
                ValueKind::Object(obj) => {
                    agent.temporal_data.get(&obj.id()).cloned().ok_or_else(|| {
                        JsError::new(ErrorKind::TypeError, "brand check failed".into())
                    })
                }
                _ => unreachable!(),
            }?;
            Ok((record, cal))
        }
        RecordKind::YearMonth => {
            let value = to_plain_year_month(agent, &item, &Value::Undefined)?;
            let cal = super::temporal_calendar_id(agent, &value);
            let record = match value.kind() {
                ValueKind::Object(obj) => {
                    agent.temporal_data.get(&obj.id()).cloned().ok_or_else(|| {
                        JsError::new(ErrorKind::TypeError, "brand check failed".into())
                    })
                }
                _ => unreachable!(),
            }?;
            Ok((record, cal))
        }
        RecordKind::MonthDay => {
            let value = to_plain_month_day(agent, &item, &Value::Undefined)?;
            let cal = super::temporal_calendar_id(agent, &value);
            let record = match value.kind() {
                ValueKind::Object(obj) => {
                    agent.temporal_data.get(&obj.id()).cloned().ok_or_else(|| {
                        JsError::new(ErrorKind::TypeError, "brand check failed".into())
                    })
                }
                _ => unreachable!(),
            }?;
            Ok((record, cal))
        }
        _ => {
            let value = to_zoned(agent, &item, &Value::Undefined)?;
            let cal = super::temporal_calendar_id(agent, &value);
            let record = match value.kind() {
                ValueKind::Object(obj) => {
                    agent.temporal_data.get(&obj.id()).cloned().ok_or_else(|| {
                        JsError::new(ErrorKind::TypeError, "brand check failed".into())
                    })
                }
                _ => unreachable!(),
            }?;
            Ok((record, cal))
        }
    }
}

/// spec 3.5.9 TemporalDateToString (iso8601 only; the calendar annotation is
/// `[u-ca=iso8601]` / `[!u-ca=iso8601]` for always/critical).
fn plain_date_to_string_impl(
    agent: &mut Agent,
    this: &Value,
    options: Value,
) -> Result<Value, JsError> {
    let [y, m, d, ..] = match require_record(agent, this, RecordKind::PlainDate)? {
        TemporalRecord::PlainDate(date) => date,
        _ => unreachable!(),
    };
    let options = super::get_options_object(&options)?;
    let show = get_temporal_show_calendar_name_option(agent, &options)?;
    let mut result = format!("{}-{:02}-{:02}", iso::pad_iso_year(y), m, d);
    result.push_str(&calendar_name_annotation(agent, this, show));
    Ok(Value::String(Handle::new(JsString::from_utf8(&result))))
}

/// The `[u-ca=X]` suffix of the toString family: appended when the
/// calendarName option is "always"/"critical", or under "auto" when the
/// object's calendar is not iso8601 (spec ToTemporalCalendarNameRecord).
fn calendar_name_annotation(agent: &Agent, this: &Value, show: &str) -> String {
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    match show {
        "never" => String::new(),
        "critical" => format!("[!u-ca={calendar}]"),
        _ if calendar == "iso8601" && show == "auto" => String::new(),
        _ => format!("[u-ca={calendar}]"),
    }
}

/// spec 4.5.14 TimeRecordToString for PlainTime: the fractionalSecondDigits,
/// smallestUnit, and roundingMode options round the time before formatting.
fn plain_time_to_string_impl(
    agent: &mut Agent,
    this: &Value,
    options: Value,
) -> Result<Value, JsError> {
    let t = match require_record(agent, this, RecordKind::PlainTime)? {
        TemporalRecord::PlainTime(t) => t,
        _ => unreachable!(),
    };
    let options = super::get_options_object(&options)?;
    let digits = super::get_fractional_second_digits(agent, &options)?;
    let rounding_mode = super::get_rounding_mode(agent, &options, RoundingMode::Trunc)?;
    let smallest = super::get_temporal_unit(agent, &options, "smallestUnit", None)?;
    let smallest = match smallest {
        UnitOption::Unit(u) => {
            super::validate_unit_group(u, UnitGroup::Time)?;
            if u == Unit::Hour {
                return Err(JsError::new(
                    ErrorKind::RangeError,
                    "smallestUnit must be a time unit other than hour".into(),
                ));
            }
            Some(u)
        }
        UnitOption::Auto => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "smallestUnit cannot be auto".into(),
            ));
        }
        UnitOption::Unset => None,
    };
    let (precision, unit, increment) = super::to_seconds_string_precision(smallest, digits);
    let t = round_time(t, increment, unit, rounding_mode)?;
    let sub = t[3] * 1_000_000 + t[4] * 1_000 + t[5];
    Ok(Value::String(Handle::new(JsString::from_utf8(
        &iso::format_time_string(t[0], t[1], t[2], sub, precision),
    ))))
}

/// spec 4.5.2 `with` (RegulateTime with the overflow option).
fn plain_time_with(
    agent: &mut Agent,
    this: &Value,
    item: Value,
    options: Value,
) -> Result<Value, JsError> {
    let mut t = match require_record(agent, this, RecordKind::PlainTime)? {
        TemporalRecord::PlainTime(t) => t,
        _ => unreachable!(),
    };
    if !matches!(item.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "invalid argument".into(),
        ));
    }
    reject_temporal_like_object(agent, &item)?;
    let partial = read_partial_time_fields(agent, &item)?;
    for (i, value) in partial.iter().enumerate() {
        if let Some(v) = value {
            t[i] = *v;
        }
    }
    let options = super::get_options_object(&options)?;
    let constrain = super::get_temporal_overflow_option(agent, &options)? == Overflow::Constrain;
    regulate_time(&mut t, constrain)?;
    create_plain_time(agent, t, &Value::Undefined)
}

/// spec 4.5.3 `add` / 4.5.4 `subtract` (AddDurationToTime: the duration folds
/// into 24-hour days and the balanced time is regulated with reject).
fn plain_time_add_subtract(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    subtract: bool,
) -> Result<Value, JsError> {
    let t = match require_record(agent, this, RecordKind::PlainTime)? {
        TemporalRecord::PlainTime(t) => t,
        _ => unreachable!(),
    };
    let duration_like = args.first().cloned().unwrap_or(Value::Undefined);
    let mut duration = super::to_temporal_duration(agent, &duration_like)?;
    if subtract {
        duration = super::negate_duration(&duration);
    }
    let internal = super::to_internal_duration_record_with_24_hour_days(&duration)?;
    let total = time_record_ns(t) + internal.time;
    let balanced = balance_time_ns(total);
    if !is_valid_time(balanced) {
        return Err(JsError::new(ErrorKind::RangeError, "invalid time".into()));
    }
    create_plain_time(agent, balanced, &Value::Undefined)
}

/// spec 4.5.5 `round` (RoundTime with a required time smallestUnit).
fn plain_time_round(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let t = match require_record(agent, this, RecordKind::PlainTime)? {
        TemporalRecord::PlainTime(t) => t,
        _ => unreachable!(),
    };
    let round_to = args.first().cloned().unwrap_or(Value::Undefined);
    if matches!(round_to.kind(), ValueKind::Undefined) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "roundTo is required".into(),
        ));
    }
    let round_to = if let ValueKind::String(text) = &round_to.kind() {
        let obj = crux::object::JsObject::ordinary_object_create(None);
        obj.create_data_property_or_throw(
            &JsString::from_utf8("smallestUnit"),
            Value::String(*text),
        )?;
        Value::Object(obj)
    } else {
        super::get_options_object(&round_to)?
    };
    let rounding_increment = super::get_rounding_increment(agent, &round_to)?;
    let rounding_mode = super::get_rounding_mode(agent, &round_to, RoundingMode::HalfExpand)?;
    let smallest = match super::get_temporal_unit(agent, &round_to, "smallestUnit", None)? {
        UnitOption::Unit(u) => {
            super::validate_unit_group(u, UnitGroup::Time)?;
            u
        }
        UnitOption::Auto | UnitOption::Unset => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "smallestUnit is required".into(),
            ));
        }
    };
    if let Some(maximum) = smallest.max_rounding_increment() {
        super::duration::validate_rounding_increment(rounding_increment, maximum, false)?;
    }
    let rounded = round_time(t, rounding_increment, smallest, rounding_mode)?;
    create_plain_time(agent, rounded, &Value::Undefined)
}

/// spec 4.5.10 `until` / 4.5.11 `since` (DifferenceTemporalPlainTime).
fn plain_time_until_since(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    since: bool,
) -> Result<Value, JsError> {
    let t = match require_record(agent, this, RecordKind::PlainTime)? {
        TemporalRecord::PlainTime(t) => t,
        _ => unreachable!(),
    };
    let other = to_plain_time(agent, &args.first().cloned().unwrap_or(Value::Undefined))?;
    let o = match require_record(agent, &other, RecordKind::PlainTime)? {
        TemporalRecord::PlainTime(t) => t,
        _ => unreachable!(),
    };
    let options = super::get_options_object(args.get(1).unwrap_or(&Value::Undefined))?;
    // GetDifferenceSettings(operation, options, "time", [], "nanosecond",
    // "hour").
    let largest_option = super::get_temporal_unit(agent, &options, "largestUnit", None)?;
    let rounding_increment = super::get_rounding_increment(agent, &options)?;
    let mut rounding_mode = super::get_rounding_mode(agent, &options, RoundingMode::Trunc)?;
    let smallest_option = super::get_temporal_unit(agent, &options, "smallestUnit", None)?;
    if let UnitOption::Unit(u) = largest_option {
        super::validate_unit_group(u, UnitGroup::Time)?;
    }
    let smallest = match smallest_option {
        UnitOption::Unit(u) => {
            super::validate_unit_group(u, UnitGroup::Time)?;
            u
        }
        UnitOption::Auto => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "smallestUnit cannot be auto".into(),
            ));
        }
        UnitOption::Unset => Unit::Nanosecond,
    };
    if since {
        rounding_mode = iso::negate_rounding_mode(rounding_mode);
    }
    let default_largest = iso::larger_of_two_units(Unit::Hour, smallest);
    let largest = match largest_option {
        UnitOption::Unset | UnitOption::Auto => default_largest,
        UnitOption::Unit(u) => u,
    };
    if iso::larger_of_two_units(largest, smallest) != largest {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "largestUnit cannot be smaller than smallestUnit".into(),
        ));
    }
    if let Some(maximum) = smallest.max_rounding_increment() {
        super::duration::validate_rounding_increment(rounding_increment, maximum, false)?;
    }
    // DifferenceTime then RoundTimeDuration.
    let diff_ns = time_record_ns(o) - time_record_ns(t);
    let rounded = super::round_time_duration(diff_ns, rounding_increment, smallest, rounding_mode)?;
    let mut fields =
        super::temporal_duration_from_internal([0.0, 0.0, 0.0, 0.0], rounded, largest)?;
    if since {
        fields = super::negate_duration(&fields);
    }
    super::create_temporal_duration(agent, &fields, &Value::Undefined)
}

/// spec 4.5.9 `equals`.
fn plain_time_equals(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let t = match require_record(agent, this, RecordKind::PlainTime)? {
        TemporalRecord::PlainTime(t) => t,
        _ => unreachable!(),
    };
    let other = to_plain_time(agent, &args.first().cloned().unwrap_or(Value::Undefined))?;
    let o = match require_record(agent, &other, RecordKind::PlainTime)? {
        TemporalRecord::PlainTime(t) => t,
        _ => unreachable!(),
    };
    Ok(Value::Boolean(o == t))
}

/// ToTemporalTimeRecord(bag, "partial"): the six fields read in ascending
/// code point order, at least one required.
fn read_partial_time_fields(agent: &mut Agent, bag: &Value) -> Result<[Option<i64>; 6], JsError> {
    let mut any = false;
    let mut fields = [None; 6];
    for key in [
        "hour",
        "microsecond",
        "millisecond",
        "minute",
        "nanosecond",
        "second",
    ] {
        let value = crate::context::get_property(agent, bag, &JsString::from_utf8(key), *bag)?;
        if matches!(value.kind(), ValueKind::Undefined) {
            continue;
        }
        any = true;
        let idx = match key {
            "hour" => 0,
            "minute" => 1,
            "second" => 2,
            "millisecond" => 3,
            "microsecond" => 4,
            _ => 5,
        };
        fields[idx] = Some(super::to_integer_with_truncation(agent, &value)?);
    }
    if !any {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "invalid time-like".into(),
        ));
    }
    Ok(fields)
}

/// PrepareCalendarFields(iso8601, bag, «year, month, monthCode, day», «hour,
/// minute, second, millisecond, microsecond, nanosecond», completeness): the
/// ten fields are read in ascending code point order with their casts. With
/// `partial` at least one field must be present; complete mode leaves missing
/// fields as None (the callers default the time fields to 0).
#[allow(clippy::type_complexity)]
fn read_date_time_fields(
    agent: &mut Agent,
    bag: &Value,
    partial: bool,
    calendar: &str,
) -> Result<
    (
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<i64>,
        [Option<i64>; 6],
    ),
    JsError,
> {
    let mut any = false;
    let mut year = None;
    let mut month = None;
    let mut month_code = None;
    let mut day = None;
    let mut t = [None; 6];
    for key in [
        "day",
        "hour",
        "microsecond",
        "millisecond",
        "minute",
        "month",
        "monthCode",
        "nanosecond",
        "second",
        "year",
    ] {
        let value = crate::context::get_property(agent, bag, &JsString::from_utf8(key), *bag)?;
        if matches!(value.kind(), ValueKind::Undefined) {
            continue;
        }
        any = true;
        match key {
            "day" => day = Some(super::to_positive_integer_with_truncation(agent, &value)?),
            "hour" => t[0] = Some(super::to_integer_with_truncation(agent, &value)?),
            "microsecond" => t[4] = Some(super::to_integer_with_truncation(agent, &value)?),
            "millisecond" => t[3] = Some(super::to_integer_with_truncation(agent, &value)?),
            "minute" => t[1] = Some(super::to_integer_with_truncation(agent, &value)?),
            "month" => month = Some(super::to_positive_integer_with_truncation(agent, &value)?),
            "monthCode" => month_code = Some(read_month_code(agent, &value)?),
            "nanosecond" => t[5] = Some(super::to_integer_with_truncation(agent, &value)?),
            "second" => t[2] = Some(super::to_integer_with_truncation(agent, &value)?),
            _ => year = Some(super::to_integer_with_truncation(agent, &value)?),
        }
    }
    if partial && !any && !bag_has_era_fields(agent, bag, calendar)? {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "no supported properties found".into(),
        ));
    }
    Ok((year, month, month_code, day, t))
}

/// RegulateTime (spec 4.5.12): clamp with constrain, validate with reject.
fn regulate_time(t: &mut [i64; 6], constrain: bool) -> Result<(), JsError> {
    if constrain {
        t[0] = t[0].clamp(0, 23);
        t[1] = t[1].clamp(0, 59);
        t[2] = t[2].clamp(0, 59);
        t[3] = t[3].clamp(0, 999);
        t[4] = t[4].clamp(0, 999);
        t[5] = t[5].clamp(0, 999);
        Ok(())
    } else if is_valid_time(*t) {
        Ok(())
    } else {
        Err(JsError::new(ErrorKind::RangeError, "invalid time".into()))
    }
}

/// BalanceTime on the sub-day fields only (the day overflow is dropped, as
/// PlainTime carries no date).
fn balance_time_ns(total: i128) -> [i64; 6] {
    let rem = total.rem_euclid(iso::NS_PER_DAY);
    [
        (rem / iso::NS_PER_HOUR) as i64,
        ((rem / iso::NS_PER_MINUTE) % 60) as i64,
        ((rem / iso::NS_PER_SECOND) % 60) as i64,
        ((rem / 1_000_000) % 1000) as i64,
        ((rem / 1000) % 1000) as i64,
        (rem % 1000) as i64,
    ]
}

fn time_record_ns(t: [i64; 6]) -> i128 {
    let [h, m, s, ms, us, ns] = t;
    ((((h as i128 * 60 + m as i128) * 60 + s as i128) * 1000 + ms as i128) * 1000 + us as i128)
        * 1000
        + ns as i128
}

/// spec 13.33 RoundTime: round the time-of-day to the given unit/increment
/// and re-balance (the day overflow is dropped for PlainTime). The higher
/// fields are kept and only the rounded unit's field is replaced.
fn round_time(
    t: [i64; 6],
    increment: i64,
    unit: Unit,
    mode: RoundingMode,
) -> Result<[i64; 6], JsError> {
    let [h, m, s, ms, us, ns] = t;
    let quantity = match unit {
        Unit::Hour | Unit::Day => time_record_ns(t),
        Unit::Minute => time_record_ns([0, m, s, ms, us, ns]),
        Unit::Second => time_record_ns([0, 0, s, ms, us, ns]),
        Unit::Millisecond => time_record_ns([0, 0, 0, ms, us, ns]),
        Unit::Microsecond => time_record_ns([0, 0, 0, 0, us, ns]),
        _ => ns as i128,
    };
    let ns_per_unit = unit
        .length_ns()
        .ok_or_else(|| JsError::new(ErrorKind::RangeError, "unit has no fixed length".into()))?;
    let rounded = iso::round_number_to_increment(quantity, ns_per_unit * increment as i128, mode)
        / ns_per_unit;
    let rounded = rounded as i64;
    let balanced = match unit {
        Unit::Hour => balance_fields(rounded, 0, 0, 0, 0, 0),
        Unit::Minute => balance_fields(h, rounded, 0, 0, 0, 0),
        Unit::Second => balance_fields(h, m, rounded, 0, 0, 0),
        Unit::Millisecond => balance_fields(h, m, s, rounded, 0, 0),
        Unit::Microsecond => balance_fields(h, m, s, ms, rounded, 0),
        _ => balance_fields(h, m, s, ms, us, rounded),
    };
    Ok(balanced)
}

fn balance_fields(h: i64, m: i64, s: i64, ms: i64, us: i64, ns: i64) -> [i64; 6] {
    balance_time_ns(time_record_ns([h, m, s, ms, us, ns]))
}

/// spec 13.33 RoundISODateTime: round the time-of-day to the unit/increment
/// (keeping the higher fields and replacing only the rounded one) and fold
/// the day overflow back into the date.
fn round_iso_date_time(
    dt: [i64; 9],
    increment: i64,
    unit: Unit,
    mode: RoundingMode,
) -> Result<[i64; 9], JsError> {
    let [y, m, d, h, min, s, ms, us, ns] = dt;
    let quantity = match unit {
        Unit::Day | Unit::Hour => time_record_ns([h, min, s, ms, us, ns]),
        Unit::Minute => time_record_ns([0, min, s, ms, us, ns]),
        Unit::Second => time_record_ns([0, 0, s, ms, us, ns]),
        Unit::Millisecond => time_record_ns([0, 0, 0, ms, us, ns]),
        Unit::Microsecond => time_record_ns([0, 0, 0, 0, us, ns]),
        _ => ns as i128,
    };
    let ns_per_unit = unit
        .length_ns()
        .ok_or_else(|| JsError::new(ErrorKind::RangeError, "unit has no fixed length".into()))?;
    let rounded = iso::round_number_to_increment(quantity, ns_per_unit * increment as i128, mode)
        / ns_per_unit;
    let rounded = rounded as i64;
    let (t, delta_days) = if unit == Unit::Day {
        // RoundTime for the day unit returns only the day overflow.
        ([0i64; 6], rounded)
    } else {
        let fields = match unit {
            Unit::Hour => [rounded, 0, 0, 0, 0, 0],
            Unit::Minute => [h, rounded, 0, 0, 0, 0],
            Unit::Second => [h, min, rounded, 0, 0, 0],
            Unit::Millisecond => [h, min, s, rounded, 0, 0],
            Unit::Microsecond => [h, min, s, ms, rounded, 0],
            _ => [h, min, s, ms, us, rounded],
        };
        let total = time_record_ns(fields);
        (
            balance_time_ns(total),
            total.div_euclid(iso::NS_PER_DAY) as i64,
        )
    };
    let date = iso::calendar_date_add(y, m, d, 0, 0, 0, delta_days, true)
        .ok_or_else(|| JsError::new(ErrorKind::RangeError, "date out of range".into()))?;
    Ok([date.0, date.1, date.2, t[0], t[1], t[2], t[3], t[4], t[5]])
}

/// spec 5.5.12 TemporalDateTimeToString: the calendarName, fractionalSecond-
/// Digits, roundingMode, and smallestUnit options are read in that order, the
/// time is rounded (RoundISODateTime), and the result is re-checked against
/// the DateTime range.
fn plain_date_time_to_string_impl(
    agent: &mut Agent,
    this: &Value,
    options: Value,
) -> Result<Value, JsError> {
    let dt = match require_record(agent, this, RecordKind::PlainDateTime)? {
        TemporalRecord::PlainDateTime(dt) => dt,
        _ => unreachable!(),
    };
    let options = super::get_options_object(&options)?;
    let show = get_temporal_show_calendar_name_option(agent, &options)?;
    let digits = super::get_fractional_second_digits(agent, &options)?;
    let rounding_mode = super::get_rounding_mode(agent, &options, RoundingMode::Trunc)?;
    let smallest = super::get_temporal_unit(agent, &options, "smallestUnit", None)?;
    let smallest = match smallest {
        UnitOption::Unit(u) => {
            super::validate_unit_group(u, UnitGroup::Time)?;
            if u == Unit::Hour {
                return Err(JsError::new(
                    ErrorKind::RangeError,
                    "smallestUnit must be a time unit other than hour".into(),
                ));
            }
            Some(u)
        }
        UnitOption::Auto => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "smallestUnit cannot be auto".into(),
            ));
        }
        UnitOption::Unset => None,
    };
    let (precision, unit, increment) = super::to_seconds_string_precision(smallest, digits);
    let rounded = round_iso_date_time(dt, increment, unit, rounding_mode)?;
    if !iso::iso_date_time_within_limits(
        rounded[0], rounded[1], rounded[2], rounded[3], rounded[4], rounded[5], rounded[6],
        rounded[7], rounded[8],
    ) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "date-time out of range".into(),
        ));
    }
    let sub = rounded[6] * 1_000_000 + rounded[7] * 1_000 + rounded[8];
    let time = iso::format_time_string(rounded[3], rounded[4], rounded[5], sub, precision);
    let mut result = format!(
        "{}-{:02}-{:02}T{}",
        iso::pad_iso_year(rounded[0]),
        rounded[1],
        rounded[2],
        time
    );
    result.push_str(&calendar_name_annotation(agent, this, show));
    Ok(Value::String(Handle::new(JsString::from_utf8(&result))))
}

/// spec 5.5.2 `with` (CalendarMergeFields over the existing date and time
/// fields, then InterpretTemporalDateTimeFields with the overflow option).
fn plain_date_time_with(
    agent: &mut Agent,
    this: &Value,
    item: Value,
    options: Value,
) -> Result<Value, JsError> {
    let dt = match require_record(agent, this, RecordKind::PlainDateTime)? {
        TemporalRecord::PlainDateTime(dt) => dt,
        _ => unreachable!(),
    };
    if !matches!(item.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "invalid argument".into(),
        ));
    }
    reject_temporal_like_object(agent, &item)?;
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    let (py, pm, pmc, pd, pt) = read_date_time_fields(agent, &item, true, &calendar)?;
    let (p_era, p_era_year) = read_partial_era_fields(agent, &item, &calendar)?;
    let options = super::get_options_object(&options)?;
    let constrain = super::get_temporal_overflow_option(agent, &options)? == Overflow::Constrain;
    // CalendarMergeFields: the same month/monthCode dedup as PlainDate.with,
    // applied over the existing date fields in calendar terms.
    let (cy, cm, cd) = calendar::calendar_iso_to_date(&calendar, dt[0], dt[1], dt[2])
        .unwrap_or((dt[0], dt[1], dt[2]));
    let year = merge_era_year(&calendar, py, cy, p_era.as_deref(), p_era_year)?;
    let month = pm;
    let month_code = pmc.or(if pm.is_some() {
        None
    } else {
        Some(calendar::calendar_month_code(&calendar, cy, cm))
    });
    let day = pd.or(Some(cd));
    let mut t = [dt[3], dt[4], dt[5], dt[6], dt[7], dt[8]];
    for (i, value) in pt.iter().enumerate() {
        if let Some(v) = value {
            t[i] = *v;
        }
    }
    // InterpretTemporalDateTimeFields: the date resolves through the calendar
    // and the time is regulated with the overflow option.
    let (y, m, d) = resolve_date_fields(year, month, month_code, day, constrain, Some(&calendar))?;
    // CalendarDateFromFields: the resolved fields are calendar fields for the
    // non-ISO calendars and must convert back to the ISO date.
    let (y, m, d) = calendar::calendar_date_to_iso(&calendar, y, m, d).unwrap_or((y, m, d));
    regulate_time(&mut t, constrain)?;
    if !iso::iso_date_time_within_limits(y, m, d, t[0], t[1], t[2], t[3], t[4], t[5]) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "date-time out of range".into(),
        ));
    }
    let value = create_temporal_object(
        agent,
        &Value::Undefined,
        PLAIN_DATE_TIME_PROTO,
        TemporalRecord::PlainDateTime([y, m, d, t[0], t[1], t[2], t[3], t[4], t[5]]),
    )?;
    with_calendar(agent, value, Some(&calendar))
}

/// spec 5.5.3 `withPlainTime` (ToTimeRecordOrMidnight).
fn plain_date_time_with_plain_time(
    agent: &mut Agent,
    this: &Value,
    temporal_time: Value,
) -> Result<Value, JsError> {
    let dt = match require_record(agent, this, RecordKind::PlainDateTime)? {
        TemporalRecord::PlainDateTime(dt) => dt,
        _ => unreachable!(),
    };
    let t = if matches!(temporal_time.kind(), ValueKind::Undefined) {
        [0i64; 6]
    } else {
        let time_value = to_plain_time(agent, &temporal_time)?;
        match require_record(agent, &time_value, RecordKind::PlainTime)? {
            TemporalRecord::PlainTime(t) => t,
            _ => unreachable!(),
        }
    };
    if !iso::iso_date_time_within_limits(dt[0], dt[1], dt[2], t[0], t[1], t[2], t[3], t[4], t[5]) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "date-time out of range".into(),
        ));
    }
    let value = create_temporal_object(
        agent,
        &Value::Undefined,
        PLAIN_DATE_TIME_PROTO,
        TemporalRecord::PlainDateTime([dt[0], dt[1], dt[2], t[0], t[1], t[2], t[3], t[4], t[5]]),
    )?;
    with_calendar(
        agent,
        value,
        Some(&super::temporal_calendar_id(agent, this).to_string_lossy()),
    )
}

/// spec 5.5.4 `withCalendar` (only the iso8601 calendar is available).
fn plain_date_time_with_calendar(
    agent: &mut Agent,
    this: &Value,
    calendar: Value,
) -> Result<Value, JsError> {
    let dt = match require_record(agent, this, RecordKind::PlainDateTime)? {
        TemporalRecord::PlainDateTime(dt) => dt,
        _ => unreachable!(),
    };
    let calendar = to_temporal_calendar_identifier(agent, &calendar)?;
    let value = create_temporal_object(
        agent,
        &Value::Undefined,
        PLAIN_DATE_TIME_PROTO,
        TemporalRecord::PlainDateTime(dt),
    )?;
    with_calendar(agent, value, calendar.as_deref())
}

/// spec 5.5.9 `add` / 5.5.10 `subtract` (AddDurationToDateTime: the duration's
/// time part, with 24-hour days folded in, balances against the wall time and
/// its day overflow joins the calendar date part).
fn plain_date_time_add_subtract(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    subtract: bool,
) -> Result<Value, JsError> {
    let dt = match require_record(agent, this, RecordKind::PlainDateTime)? {
        TemporalRecord::PlainDateTime(dt) => dt,
        _ => unreachable!(),
    };
    let duration_like = args.first().cloned().unwrap_or(Value::Undefined);
    let mut duration = super::to_temporal_duration(agent, &duration_like)?;
    if subtract {
        duration = super::negate_duration(&duration);
    }
    let options = super::get_options_object(args.get(1).unwrap_or(&Value::Undefined))?;
    let constrain = super::get_temporal_overflow_option(agent, &options)? == Overflow::Constrain;
    let internal = super::to_internal_duration_record_with_24_hour_days(&duration)?;
    let total = time_record_ns([dt[3], dt[4], dt[5], dt[6], dt[7], dt[8]]) + internal.time;
    let delta_days = total.div_euclid(iso::NS_PER_DAY) as i64;
    let t = balance_time_ns(total);
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    let date = calendar::calendar_date_add(
        &calendar,
        (dt[0], dt[1], dt[2]),
        internal.date[0] as i64,
        internal.date[1] as i64,
        internal.date[2] as i64,
        delta_days,
        constrain,
    )
    .ok_or_else(|| JsError::new(ErrorKind::RangeError, "date out of range".into()))?;
    if !iso::iso_date_time_within_limits(date.0, date.1, date.2, t[0], t[1], t[2], t[3], t[4], t[5])
    {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "date-time out of range".into(),
        ));
    }
    create_temporal_object(
        agent,
        &Value::Undefined,
        PLAIN_DATE_TIME_PROTO,
        TemporalRecord::PlainDateTime([date.0, date.1, date.2, t[0], t[1], t[2], t[3], t[4], t[5]]),
    )
    .and_then(|value| with_calendar(agent, value, Some(&calendar)))
}

/// spec 5.5.5 `round` (RoundISODateTime; day is a valid smallestUnit with a
/// rounding increment capped at 1).
fn plain_date_time_round(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let dt = match require_record(agent, this, RecordKind::PlainDateTime)? {
        TemporalRecord::PlainDateTime(dt) => dt,
        _ => unreachable!(),
    };
    let round_to = args.first().cloned().unwrap_or(Value::Undefined);
    if matches!(round_to.kind(), ValueKind::Undefined) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "roundTo is required".into(),
        ));
    }
    let round_to = if let ValueKind::String(text) = &round_to.kind() {
        let obj = crux::object::JsObject::ordinary_object_create(None);
        obj.create_data_property_or_throw(
            &JsString::from_utf8("smallestUnit"),
            Value::String(*text),
        )?;
        Value::Object(obj)
    } else {
        super::get_options_object(&round_to)?
    };
    let rounding_increment = super::get_rounding_increment(agent, &round_to)?;
    let rounding_mode = super::get_rounding_mode(agent, &round_to, RoundingMode::HalfExpand)?;
    let smallest = match super::get_temporal_unit(agent, &round_to, "smallestUnit", None)? {
        UnitOption::Unit(u) => {
            if u != Unit::Day {
                super::validate_unit_group(u, UnitGroup::Time)?;
            }
            u
        }
        UnitOption::Auto | UnitOption::Unset => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "smallestUnit is required".into(),
            ));
        }
    };
    // ValidateTemporalRoundingIncrement: day is capped at 1 (inclusive), the
    // time units at their field maxima.
    let (maximum, inclusive) = match smallest {
        Unit::Day => (1, true),
        Unit::Hour => (24, false),
        Unit::Minute => (60, false),
        Unit::Second => (60, false),
        Unit::Millisecond => (1000, false),
        Unit::Microsecond => (1000, false),
        Unit::Nanosecond => (1000, false),
        _ => unreachable!(),
    };
    super::duration::validate_rounding_increment(rounding_increment, maximum, inclusive)?;
    if rounding_increment == 1 && smallest == Unit::Nanosecond {
        return create_temporal_object(
            agent,
            &Value::Undefined,
            PLAIN_DATE_TIME_PROTO,
            TemporalRecord::PlainDateTime(dt),
        );
    }
    let rounded = round_iso_date_time(dt, rounding_increment, smallest, rounding_mode)?;
    if !iso::iso_date_time_within_limits(
        rounded[0], rounded[1], rounded[2], rounded[3], rounded[4], rounded[5], rounded[6],
        rounded[7], rounded[8],
    ) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "date-time out of range".into(),
        ));
    }
    create_temporal_object(
        agent,
        &Value::Undefined,
        PLAIN_DATE_TIME_PROTO,
        TemporalRecord::PlainDateTime(rounded),
    )
}

/// spec 5.5.14 `until` / 5.5.15 `since` (DifferenceTemporalPlainDateTime).
fn plain_date_time_until_since(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    since: bool,
) -> Result<Value, JsError> {
    let dt = match require_record(agent, this, RecordKind::PlainDateTime)? {
        TemporalRecord::PlainDateTime(dt) => dt,
        _ => unreachable!(),
    };
    let other = to_plain_date_time(
        agent,
        &args.first().cloned().unwrap_or(Value::Undefined),
        &Value::Undefined,
    )?;
    if super::temporal_calendar_id(agent, this) != super::temporal_calendar_id(agent, &other) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "calendars must match".into(),
        ));
    }
    let o = match require_record(agent, &other, RecordKind::PlainDateTime)? {
        TemporalRecord::PlainDateTime(dt) => dt,
        _ => unreachable!(),
    };
    let options = super::get_options_object(args.get(1).unwrap_or(&Value::Undefined))?;
    // GetDifferenceSettings(operation, options, "datetime", [], "nanosecond",
    // "day").
    let largest_option = super::get_temporal_unit(agent, &options, "largestUnit", None)?;
    let rounding_increment = super::get_rounding_increment(agent, &options)?;
    let mut rounding_mode = super::get_rounding_mode(agent, &options, RoundingMode::Trunc)?;
    let smallest_option = super::get_temporal_unit(agent, &options, "smallestUnit", None)?;
    if let UnitOption::Unit(u) = largest_option {
        super::validate_unit_group(u, UnitGroup::DateTime)?;
    }
    let smallest = match smallest_option {
        UnitOption::Unit(u) => {
            super::validate_unit_group(u, UnitGroup::DateTime)?;
            u
        }
        UnitOption::Auto => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "smallestUnit cannot be auto".into(),
            ));
        }
        UnitOption::Unset => Unit::Nanosecond,
    };
    if since {
        rounding_mode = iso::negate_rounding_mode(rounding_mode);
    }
    let default_largest = iso::larger_of_two_units(Unit::Day, smallest);
    let largest = match largest_option {
        UnitOption::Unset | UnitOption::Auto => default_largest,
        UnitOption::Unit(u) => u,
    };
    if iso::larger_of_two_units(largest, smallest) != largest {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "largestUnit cannot be smaller than smallestUnit".into(),
        ));
    }
    if let Some(maximum) = smallest.max_rounding_increment() {
        super::duration::validate_rounding_increment(rounding_increment, maximum, false)?;
    }
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    let diff = super::duration::difference_plain_date_time_with_rounding(
        (
            dt[0], dt[1], dt[2], dt[3], dt[4], dt[5], dt[6], dt[7], dt[8],
        ),
        (o[0], o[1], o[2], o[3], o[4], o[5], o[6], o[7], o[8]),
        largest,
        rounding_increment,
        smallest,
        rounding_mode,
        Some(&calendar),
    )?;
    let mut fields = super::temporal_duration_from_internal(diff.date, diff.time, largest)?;
    if since {
        fields = super::negate_duration(&fields);
    }
    super::create_temporal_duration(agent, &fields, &Value::Undefined)
}

/// spec 5.5.7 `toPlainDate`.
fn plain_date_time_to_plain_date(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let dt = match require_record(agent, this, RecordKind::PlainDateTime)? {
        TemporalRecord::PlainDateTime(dt) => dt,
        _ => unreachable!(),
    };
    create_plain_date(agent, (dt[0], dt[1], dt[2]), &Value::Undefined)
}

/// spec 5.5.8 `toPlainTime`.
fn plain_date_time_to_plain_time(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let dt = match require_record(agent, this, RecordKind::PlainDateTime)? {
        TemporalRecord::PlainDateTime(dt) => dt,
        _ => unreachable!(),
    };
    create_plain_time(
        agent,
        [dt[3], dt[4], dt[5], dt[6], dt[7], dt[8]],
        &Value::Undefined,
    )
}

/// spec 5.5.11 `toZonedDateTime` (GetEpochNanosecondsFor: the wall date-time
/// with the disambiguation option).
fn plain_date_time_to_zoned_date_time(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let dt = match require_record(agent, this, RecordKind::PlainDateTime)? {
        TemporalRecord::PlainDateTime(dt) => dt,
        _ => unreachable!(),
    };
    let time_zone_like = args.first().cloned().unwrap_or(Value::Undefined);
    let tz = super::instant::to_temporal_time_zone_identifier(agent, &time_zone_like)?;
    let options = super::get_options_object(args.get(1).unwrap_or(&Value::Undefined))?;
    let disambiguation = super::get_temporal_disambiguation_option(agent, &options)?;
    let epoch = wall_to_epoch_ns(
        &tz,
        dt[0],
        dt[1],
        dt[2],
        dt[3],
        dt[4],
        dt[5],
        dt[6],
        dt[7],
        dt[8],
        &disambiguation,
    )?;
    if !(iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(&epoch) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "result is out of range".into(),
        ));
    }
    create_temporal_object(
        agent,
        &Value::Undefined,
        ZONED_PROTO,
        TemporalRecord::ZonedDateTime(epoch, JsString::from_utf8(&tz)),
    )
}

/// spec 6.5.3 `toString`: reads calendarName, fractionalSecondDigits, offset,
/// roundingMode, smallestUnit, and timeZoneName (in that order), rounds the
/// instant, then formats via TemporalZonedDateTimeToString.
fn zoned_to_string_impl(agent: &mut Agent, this: &Value, options: Value) -> Result<Value, JsError> {
    let (ns, tz) = match require_record(agent, this, RecordKind::ZonedDateTime)? {
        TemporalRecord::ZonedDateTime(ns, tz) => (ns, tz.to_string_lossy()),
        _ => unreachable!(),
    };
    let options = super::get_options_object(&options)?;
    let show = get_temporal_show_calendar_name_option(agent, &options)?;
    let digits = super::get_fractional_second_digits(agent, &options)?;
    let show_offset = get_temporal_show_offset_option(agent, &options)?;
    let rounding_mode = super::get_rounding_mode(agent, &options, RoundingMode::Trunc)?;
    let smallest = super::get_temporal_unit(agent, &options, "smallestUnit", None)?;
    let show_time_zone = get_temporal_show_time_zone_name_option(agent, &options)?;
    let smallest = match smallest {
        UnitOption::Unit(u) => {
            super::validate_unit_group(u, UnitGroup::Time)?;
            if u == Unit::Hour {
                return Err(JsError::new(
                    ErrorKind::RangeError,
                    "smallestUnit must be a time unit other than hour".into(),
                ));
            }
            Some(u)
        }
        UnitOption::Auto => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "smallestUnit cannot be auto".into(),
            ));
        }
        UnitOption::Unset => None,
    };
    let (precision, unit, increment) = super::to_seconds_string_precision(smallest, digits);
    // RoundTemporalInstant, then format the rounded instant.
    let rounded = iso::round_number_to_increment_as_if_positive(
        ns,
        unit.length_ns().unwrap() * increment as i128,
        rounding_mode,
    );
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    zoned_format_string(
        rounded,
        &tz,
        precision,
        show,
        show_offset,
        show_time_zone,
        &calendar,
    )
}

/// spec 6.5.4 TemporalZonedDateTimeToString: the local date-time in the zone,
/// the offset (unless never), the `[zone]` annotation (unless never), and the
/// calendar annotation (auto hides iso8601).
#[allow(clippy::too_many_arguments)]
fn zoned_format_string(
    ns: i128,
    tz: &str,
    precision: FracPrecision,
    show: &str,
    show_offset: &str,
    show_time_zone: &str,
    calendar: &str,
) -> Result<Value, JsError> {
    let offset = super::offset_ns_at(tz, ns).unwrap_or(0);
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
    let time = iso::format_time_string(balanced.3, balanced.4, balanced.5, sub, precision);
    let mut result = format!(
        "{}-{:02}-{:02}T{}",
        iso::pad_iso_year(balanced.0),
        balanced.1,
        balanced.2,
        time
    );
    if show_offset != "never" {
        result.push_str(&iso::format_date_time_utc_offset_rounded(offset));
    }
    if show_time_zone != "never" {
        let flag = if show_time_zone == "critical" {
            "!"
        } else {
            ""
        };
        result.push_str(&format!("[{flag}{tz}]"));
    }
    let flag = if show == "critical" { "!" } else { "" };
    if show == "auto" && calendar != "iso8601" || show == "always" || show == "critical" {
        result.push_str(&format!("[{flag}u-ca={calendar}]"));
    }
    Ok(Value::String(Handle::new(JsString::from_utf8(&result))))
}

/// spec 6.5.11 `toJSON` (TemporalZonedDateTimeToString with auto precision and
/// all auto show options).
fn zoned_to_string(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    zoned_to_string_impl(agent, this, Value::Undefined)
}

/// The (epoch ns, time zone identifier) of a ZonedDateTime.
fn zoned_parts(agent: &Agent, this: &Value) -> Result<(i128, String), JsError> {
    match require_record(agent, this, RecordKind::ZonedDateTime)? {
        TemporalRecord::ZonedDateTime(ns, tz) => Ok((ns, tz.to_string_lossy())),
        _ => unreachable!(),
    }
}

/// Create a ZonedDateTime record with the epoch-nanosecond range check
/// (spec 6.5.1 CreateTemporalZonedDateTime).
fn create_zoned(agent: &mut Agent, ns: i128, tz: &str) -> Result<Value, JsError> {
    if !(iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(&ns) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "result is out of range".into(),
        ));
    }
    create_temporal_object(
        agent,
        &Value::Undefined,
        ZONED_PROTO,
        TemporalRecord::ZonedDateTime(ns, JsString::from_utf8(tz)),
    )
}

/// spec 6.5.5 `with` (CalendarMergeFields over the existing local fields,
/// then InterpretISODateTimeOffset with the disambiguation/offset/overflow
/// options).
fn zoned_with(
    agent: &mut Agent,
    this: &Value,
    item: Value,
    options: Value,
) -> Result<Value, JsError> {
    let (ns, tz) = zoned_parts(agent, this)?;
    if !matches!(item.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "invalid argument".into(),
        ));
    }
    reject_temporal_like_object(agent, &item)?;
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    let (py, pm, pmc, pd, pt, poffset) = read_zoned_with_fields(agent, &item, &calendar)?;
    let (p_era, p_era_year) = read_partial_era_fields(agent, &item, &calendar)?;
    let local = zoned_local(agent, ns, &tz)?;
    let offset_ns = super::offset_ns_at(&tz, ns)
        .ok_or_else(|| JsError::new(ErrorKind::RangeError, "unsupported time zone".into()))?;
    let options = super::get_options_object(&options)?;
    let disambiguation = super::get_temporal_disambiguation_option(agent, &options)?;
    let offset_opt = super::get_temporal_offset_option(agent, &options, "prefer")?;
    let constrain = super::get_temporal_overflow_option(agent, &options)? == Overflow::Constrain;
    // CalendarMergeFields: the partial month/monthCode dedup of
    // PlainDateTime.with over the existing date fields in calendar terms.
    let (cy, cm, cd) = calendar::calendar_iso_to_date(&calendar, local.0, local.1, local.2)
        .unwrap_or((local.0, local.1, local.2));
    let year = merge_era_year(&calendar, py, cy, p_era.as_deref(), p_era_year)?;
    let month = pm;
    let month_code = pmc.or(if pm.is_some() {
        None
    } else {
        Some(calendar::calendar_month_code(&calendar, cy, cm))
    });
    let day = pd.or(Some(cd));
    let mut t = [local.3, local.4, local.5, local.6, local.7, local.8];
    for (i, value) in pt.iter().enumerate() {
        if let Some(v) = value {
            t[i] = *v;
        }
    }
    let offset_text = poffset
        .clone()
        .unwrap_or_else(|| iso::format_offset_nanoseconds(offset_ns));
    let (y, m, d) = resolve_date_fields(year, month, month_code, day, constrain, Some(&calendar))?;
    // CalendarDateFromFields: the resolved fields are calendar fields for the
    // non-ISO calendars and must convert back to the ISO date.
    let (y, m, d) = calendar::calendar_date_to_iso(&calendar, y, m, d).unwrap_or((y, m, d));
    regulate_time(&mut t, constrain)?;
    let dt = [y, m, d, t[0], t[1], t[2], t[3], t[4], t[5]];
    let new_offset_ns = iso::parse_date_time_utc_offset(&offset_text)
        .map_err(|_| JsError::new(ErrorKind::RangeError, "invalid offset".into()))?;
    let epoch = interpret_iso_date_time_offset(
        dt,
        &tz,
        Some(new_offset_ns),
        false,
        &offset_opt,
        &disambiguation,
        false,
    )?;
    let value = create_zoned(agent, epoch, &tz)?;
    with_calendar(agent, value, Some(&calendar))
}

/// spec 6.5.6 `withPlainTime` (ToTemporalTimeRecord over the instance's local
/// date; midnight when the argument is undefined).
fn zoned_with_plain_time(
    agent: &mut Agent,
    this: &Value,
    temporal_time: Value,
) -> Result<Value, JsError> {
    let (ns, tz) = zoned_parts(agent, this)?;
    let local = zoned_local(agent, ns, &tz)?;
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    if matches!(temporal_time.kind(), ValueKind::Undefined) {
        // GetStartOfDay (spec 6.3.32): a midnight gap skips 00:00
        // (test262 dst-skipped-cross-midnight.js).
        let epoch = zoned_start_of_day_ns(agent, &tz, local.0, local.1, local.2)?;
        let value = create_zoned(agent, epoch, &tz)?;
        return with_calendar(agent, value, Some(&calendar));
    }
    let time_value = to_plain_time(agent, &temporal_time)?;
    let t = match require_record(agent, &time_value, RecordKind::PlainTime)? {
        TemporalRecord::PlainTime(t) => t,
        _ => unreachable!(),
    };
    // GetEpochNanosecondsFor: the wall date-time with the compatible
    // disambiguation.
    let epoch = wall_to_epoch_ns(
        &tz,
        local.0,
        local.1,
        local.2,
        t[0],
        t[1],
        t[2],
        t[3],
        t[4],
        t[5],
        "compatible",
    )?;
    let value = create_zoned(agent, epoch, &tz)?;
    with_calendar(agent, value, Some(&calendar))
}

/// spec 6.5.7 `withTimeZone` (keeps the instant, changes the zone).
/// spec 6.5.7 `withTimeZone` (the result keeps the instance's calendar).
fn zoned_with_time_zone(
    agent: &mut Agent,
    this: &Value,
    time_zone: Value,
) -> Result<Value, JsError> {
    let (ns, _) = zoned_parts(agent, this)?;
    let tz = super::instant::to_temporal_time_zone_identifier(agent, &time_zone)?;
    let value = create_zoned(agent, ns, &tz)?;
    let calendar = super::temporal_calendar_id(agent, this);
    super::set_temporal_calendar(agent, &value, Some(&calendar.to_string_lossy()));
    Ok(value)
}

/// spec 6.5.8 `withCalendar`.
fn zoned_with_calendar(agent: &mut Agent, this: &Value, calendar: Value) -> Result<Value, JsError> {
    let (ns, tz) = zoned_parts(agent, this)?;
    let calendar = to_temporal_calendar_identifier(agent, &calendar)?;
    let value = create_zoned(agent, ns, &tz)?;
    with_calendar(agent, value, calendar.as_deref())
}

/// spec 6.5.9 `add` / 6.5.10 `subtract` (AddDurationToZonedDateTime: the
/// date part adds to the local date first, then the new wall time resolves
/// through GetEpochNanosecondsFor with the compatible disambiguation — a DST
/// transition between the wall times shifts the offset (test262 add/dst.js)
/// — then the time part adds to the resulting instant).
fn zoned_add_subtract(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    subtract: bool,
) -> Result<Value, JsError> {
    let (ns, tz) = zoned_parts(agent, this)?;
    let duration_like = args.first().cloned().unwrap_or(Value::Undefined);
    let mut duration = super::to_temporal_duration(agent, &duration_like)?;
    if subtract {
        duration = super::negate_duration(&duration);
    }
    let options = super::get_options_object(args.get(1).unwrap_or(&Value::Undefined))?;
    let constrain = super::get_temporal_overflow_option(agent, &options)? == Overflow::Constrain;
    // AddZonedDateTime splits the duration at the day: the date parts add to
    // the local (wall) date — not 24h each, which would cross a transition
    // at the wrong point (test262 subtract/dst.js) — and the time parts add
    // to the resulting instant.
    let internal = super::to_internal_duration_record(&duration);
    // spec 6.5.5 AddZonedDateTime step 1: a duration with no date units adds
    // directly to the instant (AddInstant). Wall arithmetic would re-resolve
    // the wall time across a transition — subtracting 1s from an instant at a
    // transition would land on the wrong side (test262 getTimeZoneTransition/
    // subtract-second-and-nanosecond-from-last-transition.js).
    if internal.date.iter().all(|&d| d == 0.0) {
        let end_ns = ns + internal.time;
        if !(iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(&end_ns) {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "result is out of range".into(),
            ));
        }
        return create_zoned(agent, end_ns, &tz).and_then(|value| {
            let calendar = super::temporal_calendar_id(agent, this);
            with_calendar(agent, value, Some(&calendar.to_string_lossy()))
        });
    }
    // AddZonedDateTime: CalendarDateAdd on the local date (the days fold into
    // the calendar add), then GetEpochNanosecondsFor for the new wall time.
    let local = zoned_local(agent, ns, &tz)?;
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    let date = calendar::calendar_date_add(
        &calendar,
        (local.0, local.1, local.2),
        internal.date[0] as i64,
        internal.date[1] as i64,
        internal.date[2] as i64,
        internal.date[3] as i64,
        constrain,
    )
    .ok_or_else(|| JsError::new(ErrorKind::RangeError, "date out of range".into()))?;
    let intermediate_ns = wall_to_epoch_ns(
        &tz,
        date.0,
        date.1,
        date.2,
        local.3,
        local.4,
        local.5,
        local.6,
        local.7,
        local.8,
        "compatible",
    )?;
    // AddInstant: the result must be a valid instant.
    let end_ns = intermediate_ns + internal.time;
    if !(iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(&end_ns) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "result is out of range".into(),
        ));
    }
    create_zoned(agent, end_ns, &tz).and_then(|value| {
        let calendar = super::temporal_calendar_id(agent, this);
        with_calendar(agent, value, Some(&calendar.to_string_lossy()))
    })
}

/// spec 6.5.12 `round` (day or time units; the day case rounds the progress
/// through the 24-hour day, the time case rounds the local fields and keeps
/// the current offset via InterpretISODateTimeOffset).
fn zoned_round(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let (ns, tz) = zoned_parts(agent, this)?;
    let round_to = args.first().cloned().unwrap_or(Value::Undefined);
    if matches!(round_to.kind(), ValueKind::Undefined) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "roundTo is required".into(),
        ));
    }
    let round_to = if let ValueKind::String(text) = &round_to.kind() {
        let obj = crux::object::JsObject::ordinary_object_create(None);
        obj.create_data_property_or_throw(
            &JsString::from_utf8("smallestUnit"),
            Value::String(*text),
        )?;
        Value::Object(obj)
    } else {
        super::get_options_object(&round_to)?
    };
    let rounding_increment = super::get_rounding_increment(agent, &round_to)?;
    let rounding_mode = super::get_rounding_mode(agent, &round_to, RoundingMode::HalfExpand)?;
    let smallest = match super::get_temporal_unit(agent, &round_to, "smallestUnit", None)? {
        UnitOption::Unit(Unit::Year)
        | UnitOption::Unit(Unit::Month)
        | UnitOption::Unit(Unit::Week) => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "smallestUnit must be a time unit or day".into(),
            ));
        }
        UnitOption::Unit(u) => u,
        UnitOption::Auto | UnitOption::Unset => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "smallestUnit is required".into(),
            ));
        }
    };
    let (maximum, inclusive) = match smallest {
        Unit::Day => (1, true),
        Unit::Hour => (24, false),
        Unit::Minute => (60, false),
        Unit::Second => (60, false),
        Unit::Millisecond => (1000, false),
        Unit::Microsecond => (1000, false),
        Unit::Nanosecond => (1000, false),
        _ => unreachable!(),
    };
    super::duration::validate_rounding_increment(rounding_increment, maximum, inclusive)?;
    if rounding_increment == 1 && smallest == Unit::Nanosecond {
        return create_zoned(agent, ns, &tz);
    }
    let offset = super::offset_ns_at(&tz, ns)
        .ok_or_else(|| JsError::new(ErrorKind::RangeError, "unsupported time zone".into()))?;
    let epoch = if smallest == Unit::Day {
        // Day rounding: the progress is the local wall time-of-day, rounded
        // against the span between the start instants of the local date and
        // the next date — a DST day is shorter or longer than 24h, and an
        // overlap can replay wall times past the start of the next day
        // (test262 round/dst-skipped-cross-midnight.js,
        // same-date-starts-twice.js).
        let local = zoned_local(agent, ns, &tz)?;
        let start_ns = zoned_start_of_day_ns(agent, &tz, local.0, local.1, local.2)?;
        let next = iso::add_days_to_iso_date(local.0, local.1, local.2, 1);
        let end_ns = zoned_start_of_day_ns(agent, &tz, next.0, next.1, next.2)?;
        if !(iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(&start_ns)
            || !(iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(&end_ns)
        {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "day boundary is out of range".into(),
            ));
        }
        let day_length = end_ns - start_ns;
        // The progress is the elapsed time from the day's start (the halfway
        // point of a 25-hour day is 11:30 local — test262 round-dst-
        // boundaries.js); an overlap can replay wall times past the start of
        // the next day, where the local wall time-of-day is used instead
        // (test262 same-date-starts-twice.js).
        let progress = if ns < end_ns {
            ns - start_ns
        } else {
            (local.3 as i128 * 3600 + local.4 as i128 * 60 + local.5 as i128) * 1_000_000_000
                + local.6 as i128 * 1_000_000
                + local.7 as i128 * 1_000
                + local.8 as i128
        };
        let rounded = iso::round_number_to_increment_as_if_positive(
            progress,
            day_length * rounding_increment as i128,
            rounding_mode,
        );
        start_ns + rounded
    } else {
        // Time-unit rounding of the local fields; the current offset is
        // retained when it still applies (offset option prefer, compatible
        // disambiguation otherwise).
        let local = zoned_local(agent, ns, &tz)?;
        let dt = [
            local.0, local.1, local.2, local.3, local.4, local.5, local.6, local.7, local.8,
        ];
        let rounded = round_iso_date_time(dt, rounding_increment, smallest, rounding_mode)?;
        interpret_iso_date_time_offset(
            rounded,
            &tz,
            Some(offset),
            false,
            "prefer",
            "compatible",
            false,
        )?
    };
    create_zoned(agent, epoch, &tz)
}

/// spec 6.5.13 `until` / 6.5.14 `since` (DifferenceTemporalZonedDateTime;
/// the time zones must be equal, then DifferenceZonedDateTimeWithRounding).
fn zoned_until_since(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    since: bool,
) -> Result<Value, JsError> {
    let (ns, tz) = zoned_parts(agent, this)?;
    let other = to_zoned(
        agent,
        &args.first().cloned().unwrap_or(Value::Undefined),
        &Value::Undefined,
    )?;
    if super::temporal_calendar_id(agent, this) != super::temporal_calendar_id(agent, &other) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "calendars must match".into(),
        ));
    }
    let (ons, ons_tz) = match require_record(agent, &other, RecordKind::ZonedDateTime)? {
        TemporalRecord::ZonedDateTime(ns, tz) => (ns, tz.to_string_lossy()),
        _ => unreachable!(),
    };
    let options = super::get_options_object(args.get(1).unwrap_or(&Value::Undefined))?;
    // GetDifferenceSettings(operation, options, "datetime", [], "nanosecond",
    // "hour").
    let largest_option = super::get_temporal_unit(agent, &options, "largestUnit", None)?;
    let rounding_increment = super::get_rounding_increment(agent, &options)?;
    let mut rounding_mode = super::get_rounding_mode(agent, &options, RoundingMode::Trunc)?;
    let smallest_option = super::get_temporal_unit(agent, &options, "smallestUnit", None)?;
    if let UnitOption::Unit(u) = largest_option {
        super::validate_unit_group(u, UnitGroup::DateTime)?;
    }
    let smallest = match smallest_option {
        UnitOption::Unit(u) => {
            super::validate_unit_group(u, UnitGroup::DateTime)?;
            u
        }
        UnitOption::Auto => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "smallestUnit cannot be auto".into(),
            ));
        }
        UnitOption::Unset => Unit::Nanosecond,
    };
    if since {
        rounding_mode = iso::negate_rounding_mode(rounding_mode);
    }
    let default_largest = iso::larger_of_two_units(Unit::Hour, smallest);
    let largest = match largest_option {
        UnitOption::Unset | UnitOption::Auto => default_largest,
        UnitOption::Unit(u) => u,
    };
    // spec 6.5.9: different time zones are only comparable in time units;
    // the identifiers are canonicalized at creation (spec 11.1.15
    // TimeZoneEquals — test262 canonicalize-iana-identifiers-before-
    // comparing).
    if largest.category() == iso::Category::Date && tz != ons_tz {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "time zones must match".into(),
        ));
    }
    if iso::larger_of_two_units(largest, smallest) != largest {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "largestUnit cannot be smaller than smallestUnit".into(),
        ));
    }
    if let Some(maximum) = smallest.max_rounding_increment() {
        super::duration::validate_rounding_increment(rounding_increment, maximum, false)?;
    }
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    let diff = super::duration::difference_zoned_date_time_with_rounding(
        ns,
        ons,
        &tz,
        largest,
        rounding_increment,
        smallest,
        rounding_mode,
        Some(&calendar),
    )?;
    // spec 6.5.9 step 9: a date-unit result balances up to hours, not the
    // settings largestUnit (24 hours does not become 1 day in a 25-hour day —
    // test262 dst-balancing-result.js). A time-unit largestUnit folds the
    // exact epoch difference into that unit (test262 subseconds.js:
    // milliseconds largestUnit yields 86400250 ms, not 24 h + 250 ms).
    let balance = if largest.category() == iso::Category::Date {
        Unit::Hour
    } else {
        largest
    };
    let mut fields = super::temporal_duration_from_internal(diff.date, diff.time, balance)?;
    if since {
        fields = super::negate_duration(&fields);
    }
    super::create_temporal_duration(agent, &fields, &Value::Undefined)
}

/// spec 6.5.15 `startOfDay` (GetStartOfDay on the local date).
fn zoned_start_of_day(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let (_, tz) = zoned_parts(agent, this)?;
    let (y, m, d, ..) = match require_record(agent, this, RecordKind::ZonedDateTime)? {
        TemporalRecord::ZonedDateTime(ns, _) => {
            let local = zoned_local(agent, ns, &tz)?;
            (local.0, local.1, local.2)
        }
        _ => unreachable!(),
    };
    let start = zoned_start_of_day_ns(agent, &tz, y, m, d)?;
    create_zoned(agent, start, &tz)
}

/// spec 6.5.16 `getTimeZoneTransition` (offset zones and UTC have no
/// transitions: always null after validating the direction option).
fn zoned_get_time_zone_transition(
    agent: &mut Agent,
    this: &Value,
    direction: Value,
) -> Result<Value, JsError> {
    if matches!(direction.kind(), ValueKind::Undefined) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "options parameter is required".into(),
        ));
    }
    let direction = if let ValueKind::String(text) = &direction.kind() {
        let obj = crux::object::JsObject::ordinary_object_create(None);
        obj.create_data_property_or_throw(&JsString::from_utf8("direction"), Value::String(*text))?;
        Value::Object(obj)
    } else {
        super::get_options_object(&direction)?
    };
    let value = super::get_option(agent, &direction, "direction", &["next", "previous"], None)?;
    if value.is_none() {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "direction is required".into(),
        ));
    }
    // The next/previous transition instant (UTC and offset zones have none).
    let (ns, tz) = zoned_parts(agent, this)?;
    let Some((at_secs, _, _)) = super::tz_transition(&tz, ns, value.unwrap() == "next") else {
        return Ok(Value::Null);
    };
    create_zoned(agent, at_secs as i128 * 1_000_000_000, &tz)
}

/// spec 6.5.17 `toPlainDate` (the result keeps the instance's calendar).
fn zoned_to_plain_date(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let (ns, tz) = zoned_parts(agent, this)?;
    let local = zoned_local(agent, ns, &tz)?;
    let value = create_plain_date(agent, (local.0, local.1, local.2), &Value::Undefined)?;
    let calendar = super::temporal_calendar_id(agent, this);
    super::set_temporal_calendar(agent, &value, Some(&calendar.to_string_lossy()));
    Ok(value)
}

/// spec 6.5.18 `toPlainTime`.
fn zoned_to_plain_time(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let (ns, tz) = zoned_parts(agent, this)?;
    let local = zoned_local(agent, ns, &tz)?;
    create_plain_time(
        agent,
        [local.3, local.4, local.5, local.6, local.7, local.8],
        &Value::Undefined,
    )
}

/// spec 6.5.19 `toPlainDateTime`.
fn zoned_to_plain_date_time(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let (ns, tz) = zoned_parts(agent, this)?;
    let local = zoned_local(agent, ns, &tz)?;
    let value = create_temporal_object(
        agent,
        &Value::Undefined,
        PLAIN_DATE_TIME_PROTO,
        TemporalRecord::PlainDateTime([
            local.0, local.1, local.2, local.3, local.4, local.5, local.6, local.7, local.8,
        ]),
    )?;
    let calendar = super::temporal_calendar_id(agent, this);
    super::set_temporal_calendar(agent, &value, Some(&calendar.to_string_lossy()));
    Ok(value)
}

// ---------------------------------------------------------------------------
// PlainDate cluster (spec 3.5)
// ---------------------------------------------------------------------------

fn require_date(agent: &Agent, this: &Value) -> Result<[i64; 3], JsError> {
    match require_record(agent, this, RecordKind::PlainDate)? {
        TemporalRecord::PlainDate(d) => Ok(d),
        _ => unreachable!(),
    }
}

/// GetTemporalShowCalendarNameOption (spec 13.22).
fn get_temporal_show_calendar_name_option(
    agent: &mut Agent,
    options: &Value,
) -> Result<&'static str, JsError> {
    let value = super::get_option(
        agent,
        options,
        "calendarName",
        &["auto", "always", "never", "critical"],
        Some("auto"),
    )?;
    Ok(match value.as_deref() {
        Some("always") => "always",
        Some("never") => "never",
        Some("critical") => "critical",
        _ => "auto",
    })
}

/// GetTemporalShowOffsetOption (spec 13.27): "auto" or "never".
fn get_temporal_show_offset_option(
    agent: &mut Agent,
    options: &Value,
) -> Result<&'static str, JsError> {
    let value = super::get_option(agent, options, "offset", &["auto", "never"], Some("auto"))?;
    Ok(match value.as_deref() {
        Some("never") => "never",
        _ => "auto",
    })
}

/// GetTemporalShowTimeZoneNameOption (spec 13.28): "auto", "never", or
/// "critical".
fn get_temporal_show_time_zone_name_option(
    agent: &mut Agent,
    options: &Value,
) -> Result<&'static str, JsError> {
    let value = super::get_option(
        agent,
        options,
        "timeZoneName",
        &["auto", "never", "critical"],
        Some("auto"),
    )?;
    Ok(match value.as_deref() {
        Some("never") => "never",
        Some("critical") => "critical",
        _ => "auto",
    })
}

/// Validate a parsed u-ca annotation, returning the canonical calendar.
fn calendar_from_annotation(calendar: Option<&str>) -> Result<Option<String>, JsError> {
    match calendar {
        None => Ok(None),
        Some(c) => super::canonicalize_calendar_id(c).map(Some).ok_or_else(|| {
            JsError::new(ErrorKind::RangeError, "invalid calendar identifier".into())
        }),
    }
}

/// A bare `YYYY-MM` / `YYYYMM` / `±YYYYYY-MM` core that would also parse as
/// a PlainYearMonth string (with a valid ISO date for the first of the
/// month).
fn loose_year_month(core: &str) -> bool {
    let (year, month) = if let Some((y, m)) = core.split_once('-') {
        (y, m)
    } else if core.len() == 6 {
        (&core[..4], &core[4..])
    } else {
        return false;
    };
    if month.len() != 2 || !month.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let Ok(month) = month.parse::<i64>() else {
        return false;
    };
    let (signed, unsigned) = if year.starts_with(['+', '-']) {
        (true, &year[1..])
    } else {
        (false, year)
    };
    if year.starts_with('-') && unsigned.bytes().all(|b| b == b'0') {
        return false; // "-000000" is not a valid year
    }
    let year_ok = (unsigned.len() == 6 && signed) || (unsigned.len() == 4 && !signed);
    if !year_ok || !unsigned.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let Ok(year) = year.parse::<i64>() else {
        return false;
    };
    iso::is_valid_iso_date(year, month, 1)
}

/// A bare `MM-DD` / `MMDD` core that would also parse as a PlainMonthDay
/// string (with a valid ISO date in the 1972 reference year).
fn loose_month_day(core: &str) -> bool {
    let (month, day) = if let Some((m, d)) = core.split_once('-') {
        (m, d)
    } else if core.len() == 4 {
        (&core[..2], &core[2..])
    } else {
        return false;
    };
    if month.len() != 2
        || day.len() != 2
        || !month.bytes().all(|b| b.is_ascii_digit())
        || !day.bytes().all(|b| b.is_ascii_digit())
    {
        return false;
    }
    let (Ok(month), Ok(day)) = (month.parse::<i64>(), day.parse::<i64>()) else {
        return false;
    };
    iso::is_valid_iso_date(1972, month, day)
}

/// Whether the string carries a T/t/space time designator directly before
/// two digits (spec 13.34's disambiguation check).
fn has_time_designator(text: &str) -> bool {
    let bytes = text.as_bytes();
    (0..bytes.len().saturating_sub(2)).any(|i| {
        matches!(bytes[i], b'T' | b't' | b' ')
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
    })
}

/// ToTemporalCalendarIdentifier (spec 12.3.10): a Temporal object with a
/// calendar slot passes its own calendar, any other non-String is a
/// TypeError, an unsupported String a RangeError. Returns the canonical
/// calendar identifier (None when the input carries no calendar).
fn to_temporal_calendar_identifier(
    agent: &mut Agent,
    value: &Value,
) -> Result<Option<String>, JsError> {
    if let ValueKind::Object(obj) = value.kind()
        && let Some(record) = agent.temporal_data.get(&obj.id())
    {
        return match record {
            TemporalRecord::Instant(_) | TemporalRecord::Duration(_) => Err(JsError::new(
                ErrorKind::TypeError,
                "calendar must be a string".into(),
            )),
            _ => Ok(Some(
                super::temporal_calendar_id(agent, value).to_string_lossy(),
            )),
        };
    }
    if !matches!(value.kind(), ValueKind::String(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "calendar must be a string".into(),
        ));
    }
    let text = crate::context::to_string(agent, value)?;
    let text = text.to_string_lossy();
    if let Some(calendar) = super::canonicalize_calendar_id(&text) {
        return Ok(Some(calendar));
    }
    // ISO date-time strings carry the calendar in a u-ca annotation.
    let units: Vec<u16> = text.encode_utf16().collect();
    if let Ok(parsed) = iso::parse_iso_date_time(&units, iso::Format::TimeString) {
        return calendar_from_annotation(parsed.calendar.as_deref());
    }
    if let Ok(parsed) = iso::parse_iso_date_time(&units, iso::Format::DateTimeZoned) {
        return calendar_from_annotation(parsed.calendar.as_deref());
    }
    if let Ok(parsed) = iso::parse_iso_date_time(&units, iso::Format::DateTimePlain) {
        return calendar_from_annotation(parsed.calendar.as_deref());
    }
    // Bare year-month and month-day forms ("2020-01", "01-01") with an
    // optional annotation. The rejected "-000000" extended year never parses.
    let (core, annotation) = match text.rfind('[') {
        Some(i) if text.ends_with(']') => (&text[..i], &text[i + 1..text.len() - 1]),
        _ => (&text[..], ""),
    };
    if !annotation.is_empty() {
        let value = annotation
            .strip_prefix("!u-ca=")
            .or_else(|| annotation.strip_prefix("u-ca="));
        match value {
            Some(value) if super::canonicalize_calendar_id(value).is_some() => {}
            _ => {
                return Err(JsError::new(
                    ErrorKind::RangeError,
                    "invalid calendar identifier".into(),
                ));
            }
        }
    }
    if loose_year_month(core) || loose_month_day(core) {
        return Ok(None);
    }
    Err(JsError::new(
        ErrorKind::RangeError,
        "invalid calendar identifier".into(),
    ))
}

/// GetTemporalCalendarIdentifierWithISODefault (spec 12.3.x) for a property
/// bag: reads the `calendar` property; `undefined` means iso8601.
fn read_bag_calendar(agent: &mut Agent, item: &Value) -> Result<Option<String>, JsError> {
    let calendar =
        crate::context::get_property(agent, item, &JsString::from_utf8("calendar"), *item)?;
    if matches!(calendar.kind(), ValueKind::Undefined) {
        return Ok(None);
    }
    to_temporal_calendar_identifier(agent, &calendar)
}

/// The monthCode cast of PrepareCalendarFields (spec 13.x ParseMonthCode):
/// ToPrimitive with the string hint, a TypeError for non-string results, and
/// a RangeError for malformed forms. The ISO suitability (M01-M12, no leap
/// months) is checked later by resolve_iso_month, after the year cast (the
/// fixture's "syntax before year type, suitability after" order).
fn read_month_code(agent: &mut Agent, value: &Value) -> Result<String, JsError> {
    let prim = crate::context::to_primitive(agent, value, crux::convert::ToPrimitiveHint::String)?;
    let ValueKind::String(text) = prim.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "month code must be a string".into(),
        ));
    };
    let code = text.to_string_lossy();
    let bytes = code.as_bytes();
    let digits_ok = bytes.len() >= 3 && bytes[1].is_ascii_digit() && bytes[2].is_ascii_digit();
    let well_formed = bytes.first() == Some(&b'M')
        && digits_ok
        && if bytes.len() == 4 {
            bytes[3] == b'L'
        } else {
            bytes.len() == 3 && !(bytes[1] == b'0' && bytes[2] == b'0')
        };
    if !well_formed {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "invalid monthCode".into(),
        ));
    }
    Ok(code)
}

/// Whether the calendar has eras (Intl.Era-monthcode): iso8601, chinese, and
/// dangi ignore era/eraYear fields entirely (test262
/// from/calendar-not-supporting-eras.js).
fn calendar_uses_eras(calendar: &str) -> bool {
    !matches!(calendar, "iso8601" | "chinese" | "dangi")
}

/// The canonical era code of an input era string for a calendar (the
/// temporalHelpers CalendarEras table; the ad/bc aliases fold to ce/bce).
/// `None` when the era is not valid for the calendar.
fn canonical_era(calendar: &str, era: &str) -> Option<&'static str> {
    let valid: &[&str] = match calendar {
        "buddhist" => &["be"],
        "coptic" => &["am"],
        "ethioaa" => &["aa"],
        "ethiopic" => &["aa", "am"],
        "gregory" => &["ce", "bce"],
        "hebrew" => &["am"],
        "indian" => &["shaka"],
        "islamic-civil" | "islamic-tbla" | "islamic-umalqura" => &["ah", "bh"],
        "japanese" => &["ce", "bce", "meiji", "taisho", "showa", "heisei", "reiwa"],
        "persian" => &["ap"],
        "roc" => &["roc", "broc"],
        _ => return None,
    };
    let folded = era.to_ascii_lowercase();
    let canonical = match folded.as_str() {
        "ad" => "ce",
        "bc" => "bce",
        other => other,
    };
    if valid.contains(&canonical) {
        Some(match canonical {
            "ce" => "ce",
            "bce" => "bce",
            "be" => "be",
            "am" => "am",
            "aa" => "aa",
            "shaka" => "shaka",
            "ah" => "ah",
            "bh" => "bh",
            "meiji" => "meiji",
            "taisho" => "taisho",
            "showa" => "showa",
            "heisei" => "heisei",
            "reiwa" => "reiwa",
            "ap" => "ap",
            "roc" => "roc",
            "broc" => "broc",
            _ => unreachable!(),
        })
    } else {
        None
    }
}

/// Read the `era` and `eraYear` bag fields (PrepareCalendarFields): both or
/// neither must be present (a TypeError otherwise), the era must be valid for
/// the calendar (a RangeError), and eraYear must be a finite integer (a
/// RangeError — the corpus's `eraYear: Infinity` fixtures). When the calendar
/// does not use eras the fields are ignored if `year` is present (a TypeError
/// when they would replace the absent `year` and the caller needs one — a
/// PlainMonthDay.from accepts a month-day without a year, so era/eraYear are
/// simply ignored there). Returns the resolved ISO year when era/eraYear
/// replace the absent `year`.
pub(super) fn read_era_fields(
    agent: &mut Agent,
    bag: &Value,
    calendar: Option<&str>,
    year: Option<i64>,
) -> Result<Option<i64>, JsError> {
    let cal = calendar.unwrap_or("iso8601");
    // PrepareCalendarFields reads only the fields the calendar's `fields()`
    // lists; an era-less calendar lists no era fields, so the bag's
    // era/eraYear are never observed (the year-missing error for a month-day
    // bag surfaces from the caller's resolve instead). The era-monthcode
    // intl402 fixtures check the results, not the reads, so ignoring the
    // fields matches.
    if !calendar_uses_eras(cal) {
        return Ok(year);
    }
    let mut era: Option<String> = None;
    let mut era_year: Option<i64> = None;
    for key in ["era", "eraYear"] {
        let value = crate::context::get_property(agent, bag, &JsString::from_utf8(key), *bag)?;
        if matches!(value.kind(), ValueKind::Undefined) {
            continue;
        }
        match key {
            "era" => era = Some(crate::context::to_string(agent, &value)?.to_string_lossy()),
            _ => era_year = Some(super::to_integer_with_truncation(agent, &value)?),
        }
    }
    match (era, era_year) {
        (None, None) => Ok(year),
        (Some(_), None) | (None, Some(_)) => Err(JsError::new(
            ErrorKind::TypeError,
            "era and eraYear must both be provided".into(),
        )),
        (Some(e), Some(ey)) => {
            let Some(canonical) = canonical_era(cal, &e) else {
                return Err(JsError::new(
                    ErrorKind::RangeError,
                    format!("invalid era {e} for calendar {cal}"),
                ));
            };
            // The era-year → calendar year conversion (the era-boundary and
            // remapping fixtures remap leniently: eraYear N maps linearly and
            // the resolved date's era is recomputed by the getters).
            let converted = era_year_to_year(cal, canonical, ey)?;
            if let Some(y) = year {
                // era/eraYear and year must agree when both are present
                // (fields-overspecified pins the RangeError).
                if y != converted {
                    return Err(JsError::new(
                        ErrorKind::RangeError,
                        "era and eraYear are inconsistent with year".into(),
                    ));
                }
                return Ok(Some(y));
            }
            // The era fields resolve a year even for a month-day bag (the
            // dont-calculate-month-info-for-out-of-range-year fixture pins
            // the eraYear ±999999 RangeError); a calendar without eras keeps
            // the ignore behavior above.
            Ok(Some(converted))
        }
    }
}

/// The calendar year an era/eraYear denotes (the from/with property-bag
/// paths feed the result to the calendar-year resolve, so the conversions
/// return the calendar YEAR FIELD: the era-year of the single-era calendars
/// (buddhist, coptic, hebrew, indian, persian, the islamic ah, roc) is the
/// year itself; the paired eras (bce, broc, the islamic bh, the ethiopic
/// aa) and the regnal japanese eras offset.
fn era_year_to_year(calendar: &str, canonical: &str, era_year: i64) -> Result<i64, JsError> {
    let year = match (calendar, canonical) {
        (_, "ce") => era_year,
        (_, "bce") => 1 - era_year,
        ("buddhist", "be") => era_year,
        ("coptic", "am") | ("hebrew", "am") | ("ethiopic", "am") | ("ethioaa", "aa") => era_year,
        ("ethiopic", "aa") => era_year - 5500,
        ("indian", "shaka") => era_year,
        ("islamic-civil" | "islamic-tbla" | "islamic-umalqura", "ah") => era_year,
        ("islamic-civil" | "islamic-tbla" | "islamic-umalqura", "bh") => 1 - era_year,
        ("japanese", "meiji") => era_year + 1867,
        ("japanese", "taisho") => era_year + 1911,
        ("japanese", "showa") => era_year + 1925,
        ("japanese", "heisei") => era_year + 1988,
        ("japanese", "reiwa") => era_year + 2018,
        ("persian", "ap") => era_year,
        ("roc", "roc") => era_year,
        ("roc", "broc") => 1 - era_year,
        _ => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                format!("{canonical} era-year conversion is not supported"),
            ));
        }
    };
    Ok(year)
}

/// Whether the bag has an era/eraYear pair (counts as a supported property
/// for the with() partial readers; the pair validation happens in the merge).
fn bag_has_era_fields(agent: &mut Agent, bag: &Value, calendar: &str) -> Result<bool, JsError> {
    let (era, era_year) = read_partial_era_fields(agent, bag, calendar)?;
    Ok(era.is_some() || era_year.is_some())
}

/// Read the `era`/`eraYear` pair from a with() partial bag (both or neither
/// present; the with calendarresolvefields-error-ordering fixtures pin the
/// TypeError when only one is present before the range errors). An era-less
/// calendar's partial field list has no era fields (spec PrepareCalendarFields
/// — the with/order-of-operations fixtures pin that they are not observed).
fn read_partial_era_fields(
    agent: &mut Agent,
    bag: &Value,
    calendar: &str,
) -> Result<(Option<String>, Option<i64>), JsError> {
    if !calendar_uses_eras(calendar) {
        return Ok((None, None));
    }
    let mut era = None;
    let mut era_year = None;
    for key in ["era", "eraYear"] {
        let value = crate::context::get_property(agent, bag, &JsString::from_utf8(key), *bag)?;
        if matches!(value.kind(), ValueKind::Undefined) {
            continue;
        }
        match key {
            "era" => era = Some(crate::context::to_string(agent, &value)?.to_string_lossy()),
            _ => era_year = Some(super::to_integer_with_truncation(agent, &value)?),
        }
    }
    Ok((era, era_year))
}

/// The merged year of a with() partial: the era/eraYear pair (when both are
/// present) converts to the year and excludes the provided/existing year
/// (mutually-exclusive-fields-gregory: "era and eraYear together exclude
/// year"); otherwise the provided year wins over the existing one ("year
/// excludes era and eraYear" — the result era is recomputed by the getters).
fn merge_era_year(
    calendar: &str,
    year: Option<i64>,
    existing_year: i64,
    era: Option<&str>,
    era_year: Option<i64>,
) -> Result<Option<i64>, JsError> {
    match (era, era_year) {
        (Some(e), Some(ey)) => {
            if !calendar_uses_eras(calendar) {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "era and eraYear are invalid for this calendar".into(),
                ));
            }
            let canonical = canonical_era(calendar, e).ok_or_else(|| {
                JsError::new(
                    ErrorKind::RangeError,
                    format!("invalid era {e} for calendar {calendar}"),
                )
            })?;
            era_year_to_year(calendar, canonical, ey).map(Some)
        }
        (Some(_), None) | (None, Some(_)) => Err(JsError::new(
            ErrorKind::TypeError,
            "era and eraYear must both be provided".into(),
        )),
        (None, None) => Ok(year.or(Some(existing_year))),
    }
}

/// PrepareCalendarFields(calendar, bag, «year, month, monthCode, day», [],
/// "partial") for iso8601: the fields are read in ascending code point order
/// and at least one of them (or the era/eraYear pair the with() merge reads
/// separately) must be present.
#[allow(clippy::type_complexity)]
fn prepare_partial_date_fields(
    agent: &mut Agent,
    bag: &Value,
    calendar: &str,
) -> Result<(Option<i64>, Option<i64>, Option<String>, Option<i64>), JsError> {
    let mut any = false;
    let mut year = None;
    let mut month = None;
    let mut month_code = None;
    let mut day = None;
    for key in ["day", "month", "monthCode", "year"] {
        let value = crate::context::get_property(agent, bag, &JsString::from_utf8(key), *bag)?;
        if matches!(value.kind(), ValueKind::Undefined) {
            continue;
        }
        any = true;
        match key {
            "day" => day = Some(super::to_positive_integer_with_truncation(agent, &value)?),
            "month" => month = Some(super::to_positive_integer_with_truncation(agent, &value)?),
            "monthCode" => month_code = Some(read_month_code(agent, &value)?),
            _ => year = Some(super::to_integer_with_truncation(agent, &value)?),
        }
    }
    // An era/eraYear pair also counts as a supported property (the
    // mutually-exclusive-fields fixtures with({era, eraYear}) have no other
    // fields); the pair validation happens in the with merge. For an
    // era-less calendar the era fields are not supported at all.
    if !any {
        let (era, era_year) = read_partial_era_fields(agent, bag, calendar)?;
        if era.is_none() && era_year.is_none() {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "no supported properties found".into(),
            ));
        }
    }
    Ok((year, month, month_code, day))
}

/// RejectTemporalLikeObject (spec 14.4): no calendar/timeZone properties and
/// not a Temporal object.
fn reject_temporal_like_object(agent: &mut Agent, item: &Value) -> Result<(), JsError> {
    if let ValueKind::Object(obj) = item.kind() {
        if agent.temporal_data.contains_key(&obj.id()) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "with() does not support a calendar or timeZone property".into(),
            ));
        }
        let calendar =
            crate::context::get_property(agent, item, &JsString::from_utf8("calendar"), *item)?;
        if !matches!(calendar.kind(), ValueKind::Undefined) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "with() does not support a calendar property".into(),
            ));
        }
        let time_zone =
            crate::context::get_property(agent, item, &JsString::from_utf8("timeZone"), *item)?;
        if !matches!(time_zone.kind(), ValueKind::Undefined) {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "with() does not support a timeZone property".into(),
            ));
        }
    }
    Ok(())
}

/// CalendarDateFromFields (spec 12.3.6) for iso8601 over a merged field
/// record: year and day are required, month/monthCode resolve to one month,
/// and the overflow option regulates the result.
fn resolve_date_fields(
    year: Option<i64>,
    month: Option<i64>,
    month_code: Option<String>,
    day: Option<i64>,
    constrain: bool,
    calendar: Option<&str>,
) -> Result<(i64, i64, i64), JsError> {
    let (Some(year), Some(day)) = (year, day) else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "year and day are required".into(),
        ));
    };
    let had_month = month.is_some() || month_code.is_some();
    let month = match calendar {
        Some(cal) => calendar::resolve_calendar_month_with_overflow(
            cal,
            year,
            month,
            month_code.as_deref(),
            constrain,
        ),
        None => super::resolve_iso_month(month, month_code)?,
    };
    let Some(mut month) = month else {
        return Err(JsError::new(
            if had_month {
                ErrorKind::RangeError
            } else {
                ErrorKind::TypeError
            },
            if had_month {
                "invalid month".into()
            } else {
                "month or monthCode is required".into()
            },
        ));
    };
    if let Some(cal) = calendar {
        // The month is bounded by the year's month count (constrain clamps
        // the 13th month of a common hebrew year to Elul).
        if let Some(max) = calendar::calendar_months_in_year(cal, year)
            && month > max
        {
            if !constrain {
                return Err(JsError::new(ErrorKind::RangeError, "invalid date".into()));
            }
            month = max;
        }
        // The calendar months with length data regulate the day against
        // their own length (the ISO regulation would mis-handle
        // hebrew/islamic).
        if let Some(max) = calendar::calendar_days_in_month(cal, year, month) {
            if day > max {
                if constrain {
                    return Ok((year, month, max));
                }
                return Err(JsError::new(ErrorKind::RangeError, "invalid date".into()));
            }
            return Ok((year, month, day));
        }
    }
    let (year, month, day) = iso::regulate_iso_date(year, month, day, constrain);
    if !iso::is_valid_iso_date(year, month, day) {
        return Err(JsError::new(ErrorKind::RangeError, "invalid date".into()));
    }
    Ok((year, month, day))
}

/// CalendarDateFromFields (spec 12.3.6) for iso8601 over a merged field
/// record: year and day are required, month/monthCode resolve to one month,
/// and the overflow option regulates the result.
fn date_from_merged_fields(
    agent: &mut Agent,
    year: Option<i64>,
    month: Option<i64>,
    month_code: Option<String>,
    day: Option<i64>,
    constrain: bool,
    calendar: Option<String>,
) -> Result<Value, JsError> {
    let (year, month, day) =
        resolve_date_fields(year, month, month_code, day, constrain, calendar.as_deref())?;
    // CalendarDateFromFields for the tabular Islamic calendars (the
    // canonicalize-calendar fixtures: 1445-12-25 → 2024-07-02).
    let (year, month, day) = if let Some(calendar) = calendar.as_deref()
        && let Some((y, m, d)) = calendar::calendar_date_to_iso(calendar, year, month, day)
    {
        (y, m, d)
    } else {
        (year, month, day)
    };
    let value = create_plain_date(agent, (year, month, day), &Value::Undefined)?;
    with_calendar(agent, value, calendar.as_deref())
}

/// spec 3.5.12 `with` (CalendarMergeFields over the existing ISO date).
fn plain_date_with(
    agent: &mut Agent,
    this: &Value,
    item: Value,
    options: Value,
) -> Result<Value, JsError> {
    let [y, m, d, ..] = require_date(agent, this)?;
    if !matches!(item.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "invalid argument".into(),
        ));
    }
    reject_temporal_like_object(agent, &item)?;
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    let (py, pm, pmc, pd) = prepare_partial_date_fields(agent, &item, &calendar)?;
    let (p_era, p_era_year) = read_partial_era_fields(agent, &item, &calendar)?;
    let options = super::get_options_object(&options)?;
    let constrain = super::get_temporal_overflow_option(agent, &options)? == Overflow::Constrain;
    // The existing fields in calendar terms (the ISO fields are not the
    // calendar fields of a non-ISO date). CalendarMergeFields: a provided
    // month drops the monthCode and a provided monthCode drops the month
    // (fieldKeysToIgnore); every other existing field is kept.
    let (cy, cm, cd) = calendar::calendar_iso_to_date(&calendar, y, m, d).unwrap_or((y, m, d));
    let year = merge_era_year(&calendar, py, cy, p_era.as_deref(), p_era_year)?;
    let month = pm;
    let month_code = pmc.or(if pm.is_some() {
        None
    } else {
        Some(calendar::calendar_month_code(&calendar, cy, cm))
    });
    let day = pd.or(Some(cd));
    date_from_merged_fields(
        agent,
        year,
        month,
        month_code,
        day,
        constrain,
        Some(calendar),
    )
}

/// spec 3.5.4 `withCalendar`.
fn plain_date_with_calendar(
    agent: &mut Agent,
    this: &Value,
    calendar: Value,
) -> Result<Value, JsError> {
    let [y, m, d, ..] = require_date(agent, this)?;
    let calendar = to_temporal_calendar_identifier(agent, &calendar)?;
    let value = create_plain_date(agent, (y, m, d), &Value::Undefined)?;
    with_calendar(agent, value, calendar.as_deref())
}

/// spec 3.5.2 `add` / 3.5.3 `subtract` (AddDurationToDate; the time part of
/// the duration folds into 24-hour days).
fn plain_date_add_subtract(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    subtract: bool,
) -> Result<Value, JsError> {
    let [y, m, d, ..] = require_date(agent, this)?;
    let duration_like = args.first().cloned().unwrap_or(Value::Undefined);
    let mut duration = super::to_temporal_duration(agent, &duration_like)?;
    if subtract {
        duration = super::negate_duration(&duration);
    }
    let options = super::get_options_object(args.get(1).unwrap_or(&Value::Undefined))?;
    let constrain = super::get_temporal_overflow_option(agent, &options)? == Overflow::Constrain;
    let internal = super::to_internal_duration_record_with_24_hour_days(&duration)?;
    let days = (internal.time / iso::NS_PER_DAY) as i64;
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    let result = calendar::calendar_date_add(
        &calendar,
        (y, m, d),
        internal.date[0] as i64,
        internal.date[1] as i64,
        internal.date[2] as i64,
        days,
        constrain,
    )
    .ok_or_else(|| JsError::new(ErrorKind::RangeError, "date out of range".into()))?;
    let value = create_plain_date(agent, result, &Value::Undefined)?;
    with_calendar(agent, value, Some(&calendar))
}

/// spec 3.5.14 `until` / 3.5.15 `since` (DifferenceTemporalPlainDate).
fn plain_date_until_since(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    since: bool,
) -> Result<Value, JsError> {
    let [y, m, d, ..] = require_date(agent, this)?;
    let other = to_plain_date(agent, &args.first().cloned().unwrap_or(Value::Undefined))?;
    if super::temporal_calendar_id(agent, this) != super::temporal_calendar_id(agent, &other) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "calendars must match".into(),
        ));
    }
    let o = match require_record(agent, &other, RecordKind::PlainDate)? {
        TemporalRecord::PlainDate(d) => d,
        _ => unreachable!(),
    };
    let options = super::get_options_object(args.get(1).unwrap_or(&Value::Undefined))?;
    // GetDifferenceSettings(operation, options, "date", [], "day", "day").
    let largest_option = super::get_temporal_unit(agent, &options, "largestUnit", None)?;
    let rounding_increment = super::get_rounding_increment(agent, &options)?;
    let mut rounding_mode = super::get_rounding_mode(agent, &options, RoundingMode::Trunc)?;
    let smallest_option = super::get_temporal_unit(agent, &options, "smallestUnit", None)?;
    if let UnitOption::Unit(u) = largest_option {
        super::validate_unit_group(u, UnitGroup::Date)?;
    }
    let smallest = match smallest_option {
        UnitOption::Unit(u) => {
            super::validate_unit_group(u, UnitGroup::Date)?;
            u
        }
        UnitOption::Auto => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "smallestUnit cannot be auto".into(),
            ));
        }
        UnitOption::Unset => Unit::Day,
    };
    if since {
        rounding_mode = iso::negate_rounding_mode(rounding_mode);
    }
    let default_largest = iso::larger_of_two_units(Unit::Day, smallest);
    let largest = match largest_option {
        UnitOption::Unset | UnitOption::Auto => default_largest,
        UnitOption::Unit(u) => u,
    };
    if iso::larger_of_two_units(largest, smallest) != largest {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "largestUnit cannot be smaller than smallestUnit".into(),
        ));
    }
    // DifferenceTemporalPlainDate (spec 3.5.16): CalendarDateUntil plus
    // rounding at midnight. No date-time range validation runs for the
    // no-rounding case, so the edge dates stay valid untils.
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    let diff = difference_plain_date(
        (y, m, d),
        (o[0], o[1], o[2]),
        largest,
        rounding_increment,
        smallest,
        rounding_mode,
        &calendar,
    )?;
    let mut fields = super::temporal_duration_from_internal(diff.date, diff.time, Unit::Day)?;
    if since {
        fields = super::negate_duration(&fields);
    }
    super::create_temporal_duration(agent, &fields, &Value::Undefined)
}

/// spec 3.5.16 DifferenceTemporalPlainDate's core: CalendarDateUntil (with
/// the instance's calendar) plus rounding at midnight.
#[allow(clippy::too_many_arguments)]
fn difference_plain_date(
    one: (i64, i64, i64),
    two: (i64, i64, i64),
    largest: Unit,
    increment: i64,
    smallest: Unit,
    mode: RoundingMode,
    calendar: &str,
) -> Result<super::InternalDuration, JsError> {
    if one == two {
        return Ok(super::InternalDuration {
            date: [0.0, 0.0, 0.0, 0.0],
            time: 0,
        });
    }
    let diff = calendar::calendar_date_until(calendar, one, two, largest);
    let mut duration = super::InternalDuration {
        date: [diff.0 as f64, diff.1 as f64, diff.2 as f64, diff.3 as f64],
        time: 0,
    };
    if smallest != Unit::Day || increment != 1 {
        let one_dt = (one.0, one.1, one.2, 0, 0, 0, 0, 0, 0);
        let origin = iso::get_utc_epoch_nanoseconds(one.0, one.1, one.2, 0, 0, 0, 0, 0, 0);
        let dest = iso::get_utc_epoch_nanoseconds(two.0, two.1, two.2, 0, 0, 0, 0, 0, 0);
        super::duration::round_relative_duration(
            &mut duration,
            origin,
            dest,
            one_dt,
            None,
            largest,
            increment,
            smallest,
            mode,
        )?;
    }
    Ok(duration)
}

/// spec 3.5.8 `toPlainDateTime` (ToTimeRecordOrMidnight).
fn plain_date_to_plain_date_time(
    agent: &mut Agent,
    this: &Value,
    temporal_time: Value,
) -> Result<Value, JsError> {
    let [y, m, d, ..] = require_date(agent, this)?;
    let t = if matches!(temporal_time.kind(), ValueKind::Undefined) {
        [0i64; 6]
    } else {
        let time_value = to_plain_time(agent, &temporal_time)?;
        match require_record(agent, &time_value, RecordKind::PlainTime)? {
            TemporalRecord::PlainTime(t) => t,
            _ => unreachable!(),
        }
    };
    // RejectDateTimeRange: the midnight edge of the PlainDate range sits one
    // nanosecond below the PlainDateTime minimum.
    if !iso::iso_date_time_within_limits(y, m, d, t[0], t[1], t[2], t[3], t[4], t[5]) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "date-time out of range".into(),
        ));
    }
    let value = create_temporal_object(
        agent,
        &Value::Undefined,
        PLAIN_DATE_TIME_PROTO,
        TemporalRecord::PlainDateTime([y, m, d, t[0], t[1], t[2], t[3], t[4], t[5]]),
    )?;
    with_calendar(
        agent,
        value,
        Some(&super::temporal_calendar_id(agent, this).to_string_lossy()),
    )
}

/// spec 3.5.11 `toZonedDateTime` (fixed-offset and UTC time zones only).
fn plain_date_to_zoned_date_time(
    agent: &mut Agent,
    this: &Value,
    item: Value,
) -> Result<Value, JsError> {
    let [y, m, d, ..] = require_date(agent, this)?;
    let (tz, time) = if matches!(item.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
        let time_zone_like =
            crate::context::get_property(agent, &item, &JsString::from_utf8("timeZone"), item)?;
        if matches!(time_zone_like.kind(), ValueKind::Undefined) {
            let tz = super::instant::to_temporal_time_zone_identifier(agent, &item)?;
            (tz, None)
        } else {
            let tz = super::instant::to_temporal_time_zone_identifier(agent, &time_zone_like)?;
            let plain_time = crate::context::get_property(
                agent,
                &item,
                &JsString::from_utf8("plainTime"),
                item,
            )?;
            let time = if matches!(plain_time.kind(), ValueKind::Undefined) {
                None
            } else {
                let time_value = to_plain_time(agent, &plain_time)?;
                match require_record(agent, &time_value, RecordKind::PlainTime)? {
                    TemporalRecord::PlainTime(t) => Some(t),
                    _ => unreachable!(),
                }
            };
            (tz, time)
        }
    } else {
        let tz = super::instant::to_temporal_time_zone_identifier(agent, &item)?;
        (tz, None)
    };
    // GetStartOfDay when no plainTime is given (spec 3.3.29: a midnight gap
    // skips 00:00 — test262 dst-skipped-cross-midnight.js); otherwise
    // GetEpochNanosecondsFor with the compatible disambiguation.
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    let value = if let Some(t) = time {
        let epoch = wall_to_epoch_ns(
            &tz,
            y,
            m,
            d,
            t[0],
            t[1],
            t[2],
            t[3],
            t[4],
            t[5],
            "compatible",
        )?;
        if !(iso::NS_MIN_INSTANT..=iso::NS_MAX_INSTANT).contains(&epoch) {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "result is out of range".into(),
            ));
        }
        create_temporal_object(
            agent,
            &Value::Undefined,
            ZONED_PROTO,
            TemporalRecord::ZonedDateTime(epoch, JsString::from_utf8(&tz)),
        )?
    } else {
        let epoch = zoned_start_of_day_ns(agent, &tz, y, m, d)?;
        create_temporal_object(
            agent,
            &Value::Undefined,
            ZONED_PROTO,
            TemporalRecord::ZonedDateTime(epoch, JsString::from_utf8(&tz)),
        )?
    };
    with_calendar(agent, value, Some(&calendar))
}

/// spec 3.5.5 `toPlainYearMonth` (the date's calendar year-month stored with
/// the first day of the calendar month as the reference; the result keeps the
/// instance's calendar).
fn plain_date_to_plain_year_month(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let [y, m, d, ..] = require_date(agent, this)?;
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    let (y, m, day) = calendar::calendar_iso_to_date(&calendar, y, m, d)
        .and_then(|(cy, cm, _)| calendar::calendar_date_to_iso(&calendar, cy, cm, 1))
        .unwrap_or((y, m, 1));
    let value = create_temporal_object(
        agent,
        &Value::Undefined,
        PLAIN_YEAR_MONTH_PROTO,
        TemporalRecord::YearMonth([y, m, day]),
    )?;
    with_calendar(agent, value, Some(&calendar))
}

/// spec 3.5.6 `toPlainMonthDay` (the date's calendar month-day stored as the
/// ISO reference date; the result keeps the instance's calendar).
fn plain_date_to_plain_month_day(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let [y, m, d, ..] = require_date(agent, this)?;
    let calendar = super::temporal_calendar_id(agent, this);
    let md = month_day_reference_date(&calendar.to_string_lossy(), (y, m, d), true)?;
    let value = create_temporal_object(
        agent,
        &Value::Undefined,
        PLAIN_MONTH_DAY_PROTO,
        TemporalRecord::MonthDay(md),
    )?;
    super::set_temporal_calendar(agent, &value, Some(&calendar.to_string_lossy()));
    Ok(value)
}

/// spec 3.5.10 `equals`.
fn plain_date_equals(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let [y, m, d, ..] = require_date(agent, this)?;
    let other = to_plain_date(agent, &args.first().cloned().unwrap_or(Value::Undefined))?;
    let o = match require_record(agent, &other, RecordKind::PlainDate)? {
        TemporalRecord::PlainDate(d) => d,
        _ => unreachable!(),
    };
    Ok(Value::Boolean(
        o == [y, m, d]
            && super::temporal_calendar_id(agent, this)
                == super::temporal_calendar_id(agent, &other),
    ))
}

/// The ISO calendar's derived PlainDate getters (spec 12.3.9 calendarDateTo
/// ISO): era/eraYear are undefined for iso8601.
fn plain_date_calendar_field(agent: &Agent, this: &Value, name: &str) -> Result<Value, JsError> {
    let [y, m, d, ..] = require_date(agent, this)?;
    calendar_field_value(
        &super::temporal_calendar_id(agent, this).to_string_lossy(),
        y,
        m,
        d,
        name,
    )
}

/// The ISO calendar's derived PlainDateTime getters (computed from the date
/// part).
fn plain_date_time_calendar_field(
    agent: &Agent,
    this: &Value,
    name: &str,
) -> Result<Value, JsError> {
    let dt = match require_record(agent, this, RecordKind::PlainDateTime)? {
        TemporalRecord::PlainDateTime(dt) => dt,
        _ => unreachable!(),
    };
    calendar_field_value(
        &super::temporal_calendar_id(agent, this).to_string_lossy(),
        dt[0],
        dt[1],
        dt[2],
        name,
    )
}

/// The era/eraYear of an ISO date for the Japanese calendar: the regnal
/// eras with their start dates (the corpus pins Meiji starting 1873-01-01,
/// the Gregorian reform date; the era-boundary-japanese fixture remaps the
/// earlier Meiji years to CE).
fn japanese_era_fields(y: i64, m: i64, d: i64) -> (Option<&'static str>, Option<i64>) {
    if (y, m, d) >= (2019, 5, 1) {
        (Some("reiwa"), Some(y - 2018))
    } else if (y, m, d) >= (1989, 1, 8) {
        (Some("heisei"), Some(y - 1988))
    } else if (y, m, d) >= (1926, 12, 25) {
        (Some("showa"), Some(y - 1925))
    } else if (y, m, d) >= (1912, 7, 30) {
        (Some("taisho"), Some(y - 1911))
    } else if (y, m, d) >= (1873, 1, 1) {
        (Some("meiji"), Some(y - 1867))
    } else if y >= 1 {
        (Some("ce"), Some(y))
    } else {
        (Some("bce"), Some(1 - y))
    }
}

/// The calendar year/era/eraYear of an ISO date: the linear-era calendars
/// (gregory, japanese, roc, buddhist) offset the ISO year with the ISO
/// months; the fixed-date calendars convert through their own arithmetic.
/// The corpus pins the anchors (roundtrip-from-property-bag: ISO 2000-01-01
/// is buddhist 2543 M01-01, coptic 1716 M04-22, indian 1921 M10-11, ...)
/// and the era boundaries (era-boundary-japanese, era-boundary-ethiopic,
/// non-positive-single-era-year).
fn calendar_year_fields(
    calendar: &str,
    y: i64,
    m: i64,
    d: i64,
) -> (i64, Option<&'static str>, Option<i64>) {
    let cal_year = match calendar {
        "roc" => y - 1911,
        "buddhist" => y + 543,
        _ => calendar::calendar_iso_to_date(calendar, y, m, d)
            .map(|(cy, _, _)| cy)
            .unwrap_or(y),
    };
    let (era, era_year) = match calendar {
        "roc" => {
            if cal_year < 1 {
                (Some("broc"), Some(1 - cal_year))
            } else {
                (Some("roc"), Some(cal_year))
            }
        }
        "gregory" => {
            if y < 1 {
                (Some("bce"), Some(1 - y))
            } else {
                (Some("ce"), Some(y))
            }
        }
        "japanese" => japanese_era_fields(y, m, d),
        "buddhist" => (Some("be"), Some(cal_year)),
        "coptic" => (Some("am"), Some(cal_year)),
        "ethioaa" => (Some("aa"), Some(cal_year)),
        // The ethiopic year field is the Amete Alem numbering; the era is
        // am from year 1 up, aa before (aa eraYear = year + 5500).
        "ethiopic" => {
            if cal_year < 1 {
                (Some("aa"), Some(cal_year + 5500))
            } else {
                (Some("am"), Some(cal_year))
            }
        }
        "indian" => (Some("shaka"), Some(cal_year)),
        "persian" => (Some("ap"), Some(cal_year)),
        "islamic-civil" | "islamic-tbla" | "islamic-umalqura" => {
            if cal_year < 1 {
                (Some("bh"), Some(1 - cal_year))
            } else {
                (Some("ah"), Some(cal_year))
            }
        }
        "hebrew" => (Some("am"), Some(cal_year)),
        // The chinese/dangi and iso8601 calendars report no era.
        _ => (None, None),
    };
    (cal_year, era, era_year)
}

/// The era code of the ISO date in the calendar (the DateTimeFormat era
/// field uses the same table as the calendar era getters).
pub fn calendar_date_era(calendar: &str, y: i64, m: i64, d: i64) -> Option<&'static str> {
    calendar_year_fields(calendar, y, m, d).1
}

fn calendar_field_value(
    calendar: &str,
    y: i64,
    m: i64,
    d: i64,
    name: &str,
) -> Result<Value, JsError> {
    let (cal_year, era, era_year) = calendar_year_fields(calendar, y, m, d);
    // The calendar month/day of the ISO date (hebrew and the tabular Islamic
    // calendars differ from the ISO fields; the rest pass through).
    let (cal_month, cal_day) = match calendar::calendar_iso_to_date(calendar, y, m, d) {
        Some((_, cm, cd)) => (cm, cd),
        None => (m, d),
    };
    let value = match name {
        "era" => match era {
            Some(era) => Value::String(Handle::new(JsString::from_utf8(era))),
            None => Value::Undefined,
        },
        "eraYear" => match era_year {
            Some(era_year) => Value::Number(era_year as f64),
            None => Value::Undefined,
        },
        "year" => Value::Number(cal_year as f64),
        "month" => Value::Number(cal_month as f64),
        "day" => Value::Number(cal_day as f64),
        "dayOfWeek" => Value::Number(iso::iso_day_of_week(y, m, d) as f64),
        "dayOfYear" => Value::Number(
            calendar::calendar_day_of_year(calendar, cal_year, cal_month, cal_day)
                .unwrap_or_else(|| iso::iso_day_of_year(y, m, d)) as f64,
        ),
        // The week fields are defined for the iso8601 calendar only; the
        // other calendars' weekOfYear/yearOfWeek are undefined (the corpus's
        // construct-non-utc-non-iso.js pins gregory's undefined).
        "weekOfYear" | "yearOfWeek" => {
            if calendar == "iso8601" {
                let (week, year_of_week) = iso_week_of_year(y, m, d);
                Value::Number(if name == "yearOfWeek" {
                    year_of_week as f64
                } else {
                    week as f64
                })
            } else {
                Value::Undefined
            }
        }
        "monthsInYear" => Value::Number(
            calendar::calendar_months_in_year(calendar, cal_year).unwrap_or(12) as f64,
        ),
        "daysInWeek" => Value::Number(7.0),
        "daysInMonth" => Value::Number(
            calendar::calendar_days_in_month(calendar, cal_year, cal_month)
                .unwrap_or_else(|| iso::days_in_month(y, m)) as f64,
        ),
        "daysInYear" => Value::Number(
            calendar::calendar_days_in_year(calendar, cal_year)
                .unwrap_or_else(|| if iso::is_leap_year(y) { 366 } else { 365 }) as f64,
        ),
        _ => Value::Boolean(
            calendar::calendar_in_leap_year(calendar, cal_year)
                .unwrap_or_else(|| iso::is_leap_year(y)),
        ),
    };
    Ok(value)
}

/// The calendar day of a PlainMonthDay's reference date (the stored ISO
/// date converts back through the calendar).
fn plain_month_day_day(agent: &Agent, this: &Value) -> Result<Value, JsError> {
    let [y, m, d, ..] = match require_record(agent, this, RecordKind::MonthDay)? {
        TemporalRecord::MonthDay(md) => md,
        _ => unreachable!(),
    };
    calendar_field_value(
        &super::temporal_calendar_id(agent, this).to_string_lossy(),
        y,
        m,
        d,
        "day",
    )
}

/// The derived PlainYearMonth getters (computed from the stored ISO
/// year-month and its reference day, so the calendar month of the first day
/// resolves for the non-ISO calendars).
fn year_month_calendar_field(agent: &Agent, this: &Value, name: &str) -> Result<Value, JsError> {
    let [y, m, d, ..] = match require_record(agent, this, RecordKind::YearMonth)? {
        TemporalRecord::YearMonth(ym) => ym,
        _ => unreachable!(),
    };
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    calendar_field_value(&calendar, y, m, d, name)
}

/// ISO-8601 week of year and its associated year (the polyfill's
/// calendarDateWeekOfYear for iso8601; the week containing the first
/// Thursday).
fn iso_week_of_year(year: i64, month: i64, day: i64) -> (i64, i64) {
    const FDOW: i64 = 1; // the week starts Monday
    const MDOW: i64 = 4; // the first week must contain Thursday
    let mut yow = year;
    let dow = iso::iso_day_of_week(year, month, day);
    let doy = iso::iso_day_of_year(year, month, day);
    let days_in_year = if iso::is_leap_year(year) { 366 } else { 365 };
    let rel_dow = (dow + 7 - FDOW) % 7;
    let rel_dow_jan1 = (dow - doy + 7001 - FDOW).rem_euclid(7);
    let mut woy = (doy - 1 + rel_dow_jan1) / 7;
    if 7 - rel_dow_jan1 >= MDOW {
        woy += 1;
    }
    if woy == 0 {
        // The date falls in the last week of the previous year.
        let prev_doy = doy
            + if iso::is_leap_year(year - 1) {
                366
            } else {
                365
            };
        woy = week_number(FDOW, MDOW, prev_doy, dow);
        yow -= 1;
    } else if doy >= days_in_year - 5 {
        // The date may fall in the first week of the next year.
        let last_rel_dow = (rel_dow + days_in_year - doy).rem_euclid(7);
        if 6 - last_rel_dow >= MDOW && doy + 7 - rel_dow > days_in_year {
            woy = 1;
            yow += 1;
        }
    }
    (woy, yow)
}

fn week_number(fdow: i64, mdow: i64, desired_day: i64, day_of_week: i64) -> i64 {
    let period_start = (day_of_week - fdow - desired_day + 1).rem_euclid(7);
    let mut week_no = (desired_day + period_start - 1) / 7;
    if 7 - period_start >= mdow {
        week_no += 1;
    }
    week_no
}

/// spec 6.3.x TemporalYearMonthToString (iso8601; the reference day is shown
/// for always/critical).
fn year_month_to_string_impl(
    agent: &mut Agent,
    this: &Value,
    options: Value,
) -> Result<Value, JsError> {
    let [y, m, d, ..] = match require_record(agent, this, RecordKind::YearMonth)? {
        TemporalRecord::YearMonth(ym) => ym,
        _ => unreachable!(),
    };
    let options = super::get_options_object(&options)?;
    let show = get_temporal_show_calendar_name_option(agent, &options)?;
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    // spec TemporalYearMonthToString: the reference day is shown whenever the
    // calendarName is always/critical, or the calendar is not iso8601 (it is
    // required to round-trip the month), independent of the calendarName
    // option (test262 toString/calendarname-never.js).
    let mut result = format!("{}-{:02}", iso::pad_iso_year(y), m);
    if show == "always" || show == "critical" || calendar != "iso8601" {
        result.push_str(&format!("-{d:02}"));
    }
    let flag = if show == "critical" { "!" } else { "" };
    if show == "auto" && calendar != "iso8601" || show == "always" || show == "critical" {
        result.push_str(&format!("[{flag}u-ca={calendar}]"));
    }
    Ok(Value::String(Handle::new(JsString::from_utf8(&result))))
}

/// spec 6.4.x TemporalMonthDayToString (iso8601; the reference year is shown
/// for always/critical).
fn month_day_to_string_impl(
    agent: &mut Agent,
    this: &Value,
    options: Value,
) -> Result<Value, JsError> {
    let [y, m, d, ..] = match require_record(agent, this, RecordKind::MonthDay)? {
        TemporalRecord::MonthDay(md) => md,
        _ => unreachable!(),
    };
    let options = super::get_options_object(&options)?;
    let show = get_temporal_show_calendar_name_option(agent, &options)?;
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    // spec TemporalMonthDayToString: the reference year is shown whenever the
    // calendarName is always/critical, or the calendar is not iso8601
    // (required to round-trip the month-day), independent of the calendarName
    // option.
    let mut result = format!("{m:02}-{d:02}");
    if show == "always" || show == "critical" || calendar != "iso8601" {
        result = format!("{}-{result}", iso::pad_iso_year(y));
    }
    let flag = if show == "critical" { "!" } else { "" };
    if show == "auto" && calendar != "iso8601" || show == "always" || show == "critical" {
        result.push_str(&format!("[{flag}u-ca={calendar}]"));
    }
    Ok(Value::String(Handle::new(JsString::from_utf8(&result))))
}

// ---------------------------------------------------------------------------
// PlainYearMonth / PlainMonthDay clusters (spec 6.3-6.4)
// ---------------------------------------------------------------------------

/// PrepareCalendarFields(iso8601, bag, «year, month, monthCode», [],
/// partial): read in ascending code point order, at least one required.
#[allow(clippy::type_complexity)]
fn read_year_month_fields(
    agent: &mut Agent,
    bag: &Value,
    partial: bool,
    calendar: &str,
) -> Result<(Option<i64>, Option<String>, Option<i64>), JsError> {
    let mut any = false;
    let mut month = None;
    let mut month_code = None;
    let mut year = None;
    for key in ["month", "monthCode", "year"] {
        let value = crate::context::get_property(agent, bag, &JsString::from_utf8(key), *bag)?;
        if matches!(value.kind(), ValueKind::Undefined) {
            continue;
        }
        any = true;
        match key {
            "month" => month = Some(super::to_positive_integer_with_truncation(agent, &value)?),
            "monthCode" => month_code = Some(read_month_code(agent, &value)?),
            _ => year = Some(super::to_integer_with_truncation(agent, &value)?),
        }
    }
    if partial && !any && !bag_has_era_fields(agent, bag, calendar)? {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "no supported properties found".into(),
        ));
    }
    Ok((month, month_code, year))
}

/// PrepareCalendarFields(iso8601, bag, «year, month, monthCode, day», [],
/// partial): read in ascending code point order, at least one required.
#[allow(clippy::type_complexity)]
fn read_month_day_fields(
    agent: &mut Agent,
    bag: &Value,
    partial: bool,
    calendar: &str,
) -> Result<(Option<i64>, Option<i64>, Option<String>, Option<i64>), JsError> {
    let mut any = false;
    let mut day = None;
    let mut month = None;
    let mut month_code = None;
    let mut year = None;
    for key in ["day", "month", "monthCode", "year"] {
        let value = crate::context::get_property(agent, bag, &JsString::from_utf8(key), *bag)?;
        if matches!(value.kind(), ValueKind::Undefined) {
            continue;
        }
        any = true;
        match key {
            "day" => day = Some(super::to_positive_integer_with_truncation(agent, &value)?),
            "month" => month = Some(super::to_positive_integer_with_truncation(agent, &value)?),
            "monthCode" => month_code = Some(read_month_code(agent, &value)?),
            _ => year = Some(super::to_integer_with_truncation(agent, &value)?),
        }
    }
    if partial && !any && !bag_has_era_fields(agent, bag, calendar)? {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "no supported properties found".into(),
        ));
    }
    Ok((day, month, month_code, year))
}

/// RejectDateRange for a YearMonth's first-of-month reference date (spec
/// 12.3.6 CalendarDateFromFields): the PlainDate epoch-day bounds.
fn check_reference_date(y: i64, m: i64) -> Result<(), JsError> {
    let days = iso::iso_date_to_epoch_days(y, m - 1, 1);
    if !(-100_000_001..=100_000_000).contains(&days) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "date out of range".into(),
        ));
    }
    Ok(())
}

/// ISOYearMonthWithinLimits (spec: a year-month is representable if it can
/// contain a day a PlainDate can represent — the extreme-dates and
/// extreme-unsupported-dates fixtures pin the edges).
fn iso_year_month_within_limits(year: i64, month: i64) -> bool {
    (-271_821..=275_760).contains(&year)
        && !(year == -271_821 && month < 4)
        && !(year == 275_760 && month > 9)
}

/// CalendarYearMonthFromFields (spec 12.3.7) for iso8601: year is required,
/// month/monthCode resolve to one month, the reference day is 1, and the
/// result is bounded by RejectYearMonthRange.
fn resolve_year_month(
    year: Option<i64>,
    month: Option<i64>,
    month_code: Option<String>,
    constrain: bool,
    calendar: Option<&str>,
) -> Result<(i64, i64), JsError> {
    let Some(year) = year else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "year is required".into(),
        ));
    };
    let had_month = month.is_some() || month_code.is_some();
    let month = match calendar {
        Some(cal) => calendar::resolve_calendar_month_with_overflow(
            cal,
            year,
            month,
            month_code.as_deref(),
            constrain,
        ),
        None => super::resolve_iso_month(month, month_code)?,
    };
    let Some(month) = month else {
        // A provided month/monthCode that does not resolve in the year is a
        // RangeError (an invalid calendar month); a missing month is a
        // TypeError (the reference-day-hebrew reject case pins the former).
        return Err(JsError::new(
            if had_month {
                ErrorKind::RangeError
            } else {
                ErrorKind::TypeError
            },
            if had_month {
                "invalid month".into()
            } else {
                "month or monthCode is required".into()
            },
        ));
    };
    if let Some(cal) = calendar {
        // The calendar bounds: the year-month must be within the calendar's
        // month count (reject) or constrained to it.
        if let Some(max) = calendar::calendar_months_in_year(cal, year) {
            let month = if constrain { month.min(max) } else { month };
            if month > max {
                return Err(JsError::new(
                    ErrorKind::RangeError,
                    "invalid year-month".into(),
                ));
            }
            // RejectYearMonthRange: the calendar year-month's ISO date must
            // be within the PlainDate range (the extreme-dates and
            // extreme-unsupported-dates fixtures pin the edges).
            if let Some((iy, im, _)) = calendar::calendar_date_to_iso(cal, year, month, 1)
                && !iso_year_month_within_limits(iy, im)
            {
                return Err(JsError::new(
                    ErrorKind::RangeError,
                    "invalid year-month".into(),
                ));
            }
            return Ok((year, month));
        }
        // The linear calendars: the ISO year-month of the calendar year-month
        // (buddhist/roc years differ from ISO; gregory/japanese are the ISO
        // arithmetic years). These are all 12-month calendars, so the
        // overflow option clamps the month before the validity check (test262
        // PlainYearMonth/with/options-undefined.js: month 13 constrains to
        // December).
        let month = if constrain { month.min(12) } else { month };
        let (iy, im) = match cal {
            "buddhist" => (year - 543, month),
            "roc" => (year + 1911, month),
            _ => (year, month),
        };
        if !iso::is_valid_iso_date(iy, im, 1) || !iso_year_month_within_limits(iy, im) {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "invalid year-month".into(),
            ));
        }
        return Ok((year, month));
    }
    let (year, month, _) = iso::regulate_iso_date(year, month, 1, constrain);
    if !iso::is_valid_iso_date(year, month, 1) || !iso_year_month_within_limits(year, month) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "invalid year-month".into(),
        ));
    }
    Ok((year, month))
}

/// CalendarMonthDayFromFields (spec 12.3.8) for iso8601: day is required,
/// month/monthCode resolve to one month, and the reference year (a provided
/// one or 1972) participates only in the overflow regulation.
fn resolve_month_day(
    year: Option<i64>,
    month: Option<i64>,
    month_code: Option<String>,
    day: Option<i64>,
    constrain: bool,
    calendar: Option<&str>,
) -> Result<(i64, i64), JsError> {
    let Some(day) = day else {
        return Err(JsError::new(ErrorKind::TypeError, "day is required".into()));
    };
    let ref_year = year.unwrap_or(1972);
    if let Some(cal) = calendar {
        let had_month = month.is_some() || month_code.is_some();
        // The calendar year of the ISO reference year (the day constraint
        // validates against that context's month lengths); an explicit year
        // is already a calendar year.
        let cal_ref_year = if year.is_some() {
            ref_year
        } else {
            calendar::calendar_iso_to_date(cal, ref_year, 6, 1)
                .map(|(cy, _, _)| cy)
                .unwrap_or(ref_year)
        };
        let month = if year.is_some() {
            // With an explicit year the code regulates against that year
            // (M05L in a common year constrains to Adar).
            calendar::resolve_calendar_month_with_overflow(
                cal,
                ref_year,
                month,
                month_code.as_deref(),
                constrain,
            )
        } else if month_code.is_some() {
            // No year: the code resolves without the leap validation (M05L
            // keeps its Adar I position, M03L its leap ordinal); the caller's
            // reference search validates it against the candidate years.
            calendar::resolve_calendar_month(cal, cal_ref_year, month, month_code.as_deref())
                .or_else(|| match (cal, month_code.as_deref()) {
                    ("hebrew", Some("M05L")) => Some(6),
                    ("chinese" | "dangi", Some(code)) if code.ends_with('L') => {
                        code[1..3].parse::<i64>().ok().map(|n| n + 1)
                    }
                    _ => None,
                })
        } else {
            calendar::resolve_calendar_month(cal, cal_ref_year, month, month_code.as_deref())
        };
        let Some(mut month) = month else {
            return Err(JsError::new(
                if had_month {
                    ErrorKind::RangeError
                } else {
                    ErrorKind::TypeError
                },
                if had_month {
                    "invalid month".into()
                } else {
                    "month or monthCode is required".into()
                },
            ));
        };
        // The month is bounded by the year's month count (a numeric month
        // past the year's end constrains to the last month).
        if let Some(max) = calendar::calendar_months_in_year(cal, cal_ref_year)
            && month > max
        {
            if !constrain {
                return Err(JsError::new(
                    ErrorKind::RangeError,
                    "invalid month-day".into(),
                ));
            }
            month = max;
        }
        // Regulate the day against the month length in the reference context
        // (reject validates, constrain clamps); the result keeps the
        // reference-year month-day for the caller's reference-date search.
        // With no explicit year the day is regulated against the month
        // code's maximum across any year (NonISOMonthDayToISOReferenceDate:
        // every umalqura month can be 30, hebrew M05L is always 30 — the
        // reference context's month length would wrongly clamp the
        // observational variation away); the reference search validates the
        // day against the actual reference year.
        // A bare chinese/dangi month-day clamps against the lunar maximum
        // (30): the reference search validates the day against the actual
        // reference year, so the 29/30 variation must not pre-clamp it.
        if matches!(cal, "chinese" | "dangi") && year.is_none() {
            if day > 30 {
                if constrain {
                    return Ok((month, 30));
                }
                return Err(JsError::new(
                    ErrorKind::RangeError,
                    "invalid month-day".into(),
                ));
            }
            return Ok((month, day));
        }
        let max = if year.is_none() {
            month_code
                .as_deref()
                .and_then(|code| calendar::calendar_max_days_in_month(cal, code))
        } else {
            calendar::calendar_days_in_month(cal, cal_ref_year, month)
        };
        if let Some(max) = max {
            if day > max {
                if constrain {
                    return Ok((month, max));
                }
                return Err(JsError::new(
                    ErrorKind::RangeError,
                    "invalid month-day".into(),
                ));
            }
            return Ok((month, day));
        }
        let (_, month, day) = iso::regulate_iso_date(ref_year, month, day, constrain);
        if !iso::is_valid_iso_date(ref_year, month, day) {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "invalid month-day".into(),
            ));
        }
        Ok((month, day))
    } else {
        let month = match super::resolve_iso_month(month, month_code)? {
            Some(m) => m,
            None => {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "month or monthCode is required".into(),
                ));
            }
        };
        // monthDayToISOReferenceDate: RegulateISODate with the provided
        // reference year (1972 by default), so reject validates the day
        // against that year and constrain clamps it; the result is stored
        // with the 1972 reference.
        let (_, month, day) = iso::regulate_iso_date(ref_year, month, day, constrain);
        if !iso::is_valid_iso_date(ref_year, month, day) {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "invalid month-day".into(),
            ));
        }
        Ok((month, day))
    }
}

/// spec 6.3.3 `with` (CalendarMergeFields over {monthCode, year}).
fn plain_year_month_with(
    agent: &mut Agent,
    this: &Value,
    item: Value,
    options: Value,
) -> Result<Value, JsError> {
    let ym = match require_record(agent, this, RecordKind::YearMonth)? {
        TemporalRecord::YearMonth(ym) => ym,
        _ => unreachable!(),
    };
    if !matches!(item.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "invalid argument".into(),
        ));
    }
    reject_temporal_like_object(agent, &item)?;
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    let (pm, pmc, py) = read_year_month_fields(agent, &item, true, &calendar)?;
    let (p_era, p_era_year) = read_partial_era_fields(agent, &item, &calendar)?;
    let options = super::get_options_object(&options)?;
    let constrain = super::get_temporal_overflow_option(agent, &options)? == Overflow::Constrain;
    // CalendarMergeFields: the partial month/monthCode dedup over the
    // existing year-month in calendar terms (the stored ISO year-month is
    // the reference date of the calendar month's first day).
    let (cy, cm, _) = calendar::calendar_iso_to_date(&calendar, ym[0], ym[1], ym[2])
        .unwrap_or((ym[0], ym[1], ym[2]));
    let year = merge_era_year(&calendar, py, cy, p_era.as_deref(), p_era_year)?;
    let month = pm;
    let month_code = pmc.or(if pm.is_some() {
        None
    } else {
        Some(calendar::calendar_month_code(&calendar, cy, cm))
    });
    let (y, m) = resolve_year_month(year, month, month_code, constrain, Some(&calendar))?;
    // The result keeps the reference day: the ISO year-month of the resolved
    // month's first day.
    let (y, m, d) = calendar::calendar_year_month_to_iso(&calendar, y, m).unwrap_or((y, m, 1));
    let value = create_temporal_object(
        agent,
        &Value::Undefined,
        PLAIN_YEAR_MONTH_PROTO,
        TemporalRecord::YearMonth([y, m, d]),
    )?;
    with_calendar(agent, value, Some(&calendar))
}

/// spec 6.3.5 `add` / 6.3.6 `subtract` (AddDurationToYearMonth: only years
/// and months; weeks, days, and the time fields throw).
fn plain_year_month_add_subtract(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    subtract: bool,
) -> Result<Value, JsError> {
    let ym = match require_record(agent, this, RecordKind::YearMonth)? {
        TemporalRecord::YearMonth(ym) => ym,
        _ => unreachable!(),
    };
    let duration_like = args.first().cloned().unwrap_or(Value::Undefined);
    let mut duration = super::to_temporal_duration(agent, &duration_like)?;
    if subtract {
        duration = super::negate_duration(&duration);
    }
    let options = super::get_options_object(args.get(1).unwrap_or(&Value::Undefined))?;
    let constrain = super::get_temporal_overflow_option(agent, &options)? == Overflow::Constrain;
    if duration[2] != 0.0 || duration[3] != 0.0 || duration[4..].iter().any(|v| *v != 0.0) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "only years and months can be added to Temporal.PlainYearMonth".into(),
        ));
    }
    // CalendarDateFromFields over the first-of-month reference date
    // (RejectDateRange: the YearMonth edges sit outside the PlainDate range).
    check_reference_date(ym[0], ym[1])?;
    // CalendarDateAdd with years+months from the stored reference date, then
    // CalendarYearMonthFromFields over the result (the ISO year-month of the
    // added month's first day, with the reference day).
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    let date = calendar::calendar_date_add(
        &calendar,
        (ym[0], ym[1], ym[2]),
        duration[0] as i64,
        duration[1] as i64,
        0,
        0,
        constrain,
    )
    .ok_or_else(|| JsError::new(ErrorKind::RangeError, "date out of range".into()))?;
    // RejectDateRange: the added reference date must stay within the PlainDate
    // limits (test262 PlainYearMonth/add/limits.js: +275760-09 + 1 month
    // overflows the maximum date).
    if !iso::iso_date_time_within_limits(date.0, date.1, date.2, 0, 0, 0, 0, 0, 0) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "date out of range".into(),
        ));
    }
    let (y, m, d) = match calendar::calendar_iso_to_date(&calendar, date.0, date.1, date.2) {
        Some((cy, cm, _)) => {
            calendar::calendar_year_month_to_iso(&calendar, cy, cm).unwrap_or((date.0, date.1, 1))
        }
        None => (date.0, date.1, 1),
    };
    create_temporal_object(
        agent,
        &Value::Undefined,
        PLAIN_YEAR_MONTH_PROTO,
        TemporalRecord::YearMonth([y, m, d]),
    )
    .and_then(|value| with_calendar(agent, value, Some(&calendar)))
}

/// spec 6.3.10 `until` / 6.3.11 `since` (DifferenceTemporalPlainYearMonth:
/// week and day are disallowed units, smallestUnit defaults to month).
fn plain_year_month_until_since(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    since: bool,
) -> Result<Value, JsError> {
    let ym = match require_record(agent, this, RecordKind::YearMonth)? {
        TemporalRecord::YearMonth(ym) => ym,
        _ => unreachable!(),
    };
    let other = to_plain_year_month(
        agent,
        &args.first().cloned().unwrap_or(Value::Undefined),
        &Value::Undefined,
    )?;
    if super::temporal_calendar_id(agent, this) != super::temporal_calendar_id(agent, &other) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "calendars must match".into(),
        ));
    }
    let o = match require_record(agent, &other, RecordKind::YearMonth)? {
        TemporalRecord::YearMonth(ym) => ym,
        _ => unreachable!(),
    };
    let options = super::get_options_object(args.get(1).unwrap_or(&Value::Undefined))?;
    // GetDifferenceSettings(operation, options, "date", ["week", "day"],
    // "month", "year").
    let largest_option = super::get_temporal_unit(agent, &options, "largestUnit", None)?;
    let rounding_increment = super::get_rounding_increment(agent, &options)?;
    let mut rounding_mode = super::get_rounding_mode(agent, &options, RoundingMode::Trunc)?;
    let smallest_option = super::get_temporal_unit(agent, &options, "smallestUnit", None)?;
    let disallowed = |u: Unit| matches!(u, Unit::Week | Unit::Day);
    if let UnitOption::Unit(u) = largest_option {
        super::validate_unit_group(u, UnitGroup::Date)?;
        if disallowed(u) {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "week and day are not allowed as units".into(),
            ));
        }
    }
    let smallest = match smallest_option {
        UnitOption::Unit(u) => {
            super::validate_unit_group(u, UnitGroup::Date)?;
            if disallowed(u) {
                return Err(JsError::new(
                    ErrorKind::RangeError,
                    "week and day are not allowed as units".into(),
                ));
            }
            u
        }
        UnitOption::Auto => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "smallestUnit cannot be auto".into(),
            ));
        }
        UnitOption::Unset => Unit::Month,
    };
    if since {
        rounding_mode = iso::negate_rounding_mode(rounding_mode);
    }
    let default_largest = iso::larger_of_two_units(Unit::Year, smallest);
    let largest = match largest_option {
        UnitOption::Unset | UnitOption::Auto => default_largest,
        UnitOption::Unit(u) => u,
    };
    if iso::larger_of_two_units(largest, smallest) != largest {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "largestUnit cannot be smaller than smallestUnit".into(),
        ));
    }
    if ym == o {
        let zero = [0.0f64; 10];
        return super::create_temporal_duration(agent, &zero, &Value::Undefined);
    }
    // CalendarDateFromFields on both first-of-month reference dates
    // (RejectDateRange; the YearMonth edges can be outside the PlainDate
    // range).
    check_reference_date(ym[0], ym[1])?;
    check_reference_date(o[0], o[1])?;
    // CalendarDateUntil at the stored reference dates (the day of the
    // non-ISO year-months is the first-of-month reference day), with the
    // week fields zeroed; rounding runs at midnight when needed.
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    let date_diff = calendar::calendar_date_until(
        &calendar,
        (ym[0], ym[1], ym[2]),
        (o[0], o[1], o[2]),
        largest,
    );
    let mut duration = super::InternalDuration {
        date: [date_diff.0 as f64, date_diff.1 as f64, 0.0, 0.0],
        time: 0,
    };
    if smallest != Unit::Month || rounding_increment != 1 {
        let origin = iso::get_utc_epoch_nanoseconds(ym[0], ym[1], 1, 0, 0, 0, 0, 0, 0);
        let dest = iso::get_utc_epoch_nanoseconds(o[0], o[1], 1, 0, 0, 0, 0, 0, 0);
        super::duration::round_relative_duration(
            &mut duration,
            origin,
            dest,
            (ym[0], ym[1], 1, 0, 0, 0, 0, 0, 0),
            None,
            largest,
            rounding_increment,
            smallest,
            rounding_mode,
        )?;
    }
    let mut fields =
        super::temporal_duration_from_internal(duration.date, duration.time, Unit::Day)?;
    if since {
        fields = super::negate_duration(&fields);
    }
    super::create_temporal_duration(agent, &fields, &Value::Undefined)
}

/// spec 6.3.4 `equals`.
fn plain_year_month_equals(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let ym = match require_record(agent, this, RecordKind::YearMonth)? {
        TemporalRecord::YearMonth(ym) => ym,
        _ => unreachable!(),
    };
    let other = to_plain_year_month(
        agent,
        &args.first().cloned().unwrap_or(Value::Undefined),
        &Value::Undefined,
    )?;
    let o = match require_record(agent, &other, RecordKind::YearMonth)? {
        TemporalRecord::YearMonth(ym) => ym,
        _ => unreachable!(),
    };
    Ok(Value::Boolean(
        o == ym
            && super::temporal_calendar_id(agent, this)
                == super::temporal_calendar_id(agent, &other),
    ))
}

/// spec 6.3.9 `toPlainDate` (CalendarDateFromFields over {monthCode, year}
/// plus the provided day, constrained).
fn plain_year_month_to_plain_date(
    agent: &mut Agent,
    this: &Value,
    item: Value,
) -> Result<Value, JsError> {
    let ym = match require_record(agent, this, RecordKind::YearMonth)? {
        TemporalRecord::YearMonth(ym) => ym,
        _ => unreachable!(),
    };
    if !matches!(item.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "argument should be an object".into(),
        ));
    }
    let value = crate::context::get_property(agent, &item, &JsString::from_utf8("day"), item)?;
    let day = match value.kind() {
        ValueKind::Undefined => {
            return Err(JsError::new(ErrorKind::TypeError, "day is required".into()));
        }
        _ => super::to_positive_integer_with_truncation(agent, &value)?,
    };
    // The stored ISO year-month is the reference date of the calendar month's
    // first day: convert it to the calendar fields first, then resolve with
    // the provided day (CalendarDateFromFields over {year, monthCode, day}).
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    let (cy, cm, _) = calendar::calendar_iso_to_date(&calendar, ym[0], ym[1], ym[2])
        .unwrap_or((ym[0], ym[1], ym[2]));
    let (y, m, d) = resolve_date_fields(
        Some(cy),
        None,
        Some(calendar::calendar_month_code(&calendar, cy, cm)),
        Some(day),
        true,
        Some(&calendar),
    )?;
    // The resolved calendar fields convert back to the ISO date (the
    // CalendarDateFromFields contract shared with the date from-fields path).
    let (y, m, d) = calendar::calendar_date_to_iso(&calendar, y, m, d).unwrap_or((y, m, d));
    create_plain_date(agent, (y, m, d), &Value::Undefined)
}

/// spec 6.4.3 `with` (CalendarMergeFields over {monthCode, day}).
fn plain_month_day_with(
    agent: &mut Agent,
    this: &Value,
    item: Value,
    options: Value,
) -> Result<Value, JsError> {
    let md = match require_record(agent, this, RecordKind::MonthDay)? {
        TemporalRecord::MonthDay(md) => md,
        _ => unreachable!(),
    };
    if !matches!(item.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "invalid argument".into(),
        ));
    }
    reject_temporal_like_object(agent, &item)?;
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    let (pd, pm, pmc, py) = read_month_day_fields(agent, &item, true, &calendar)?;
    // A bare month cannot resolve a non-ISO month-day (test262
    // prototype/with/fields-missing-properties.js).
    if calendar != "iso8601" && pm.is_some() && pmc.is_none() && py.is_none() {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "monthCode or year is required for a non-ISO calendar".into(),
        ));
    }
    let options = super::get_options_object(&options)?;
    let constrain = super::get_temporal_overflow_option(agent, &options)? == Overflow::Constrain;
    let calendar = super::temporal_calendar_id(agent, this).to_string_lossy();
    // CalendarMergeFields over the existing month-day in calendar terms (the
    // stored ISO reference date); the reference year from the partial
    // participates only in the overflow regulation.
    let (cy, cm, cd) = calendar::calendar_iso_to_date(&calendar, md[0], md[1], md[2])
        .unwrap_or((md[0], md[1], md[2]));
    let month = pm;
    let month_code = pmc.or(if pm.is_some() {
        None
    } else {
        Some(calendar::calendar_month_code(&calendar, cy, cm))
    });
    let day = pd.or(Some(cd));
    let (m, d) = resolve_month_day(
        py,
        month,
        month_code.clone(),
        day,
        constrain,
        Some(&calendar),
    )?;
    // The result stores the ISO reference date of the resolved month-day (a
    // chinese/dangi reject overflow with a month-day that never occurs in the
    // reference window throws; the other calendars fall back to 1972).
    let (y, m, d) = match calendar::calendar_month_day_reference(
        &calendar,
        m,
        d,
        month_code.as_deref(),
        constrain,
    ) {
        Some(date) => date,
        None if matches!(calendar.as_str(), "chinese" | "dangi") => {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "invalid month-day".into(),
            ));
        }
        None => (1972, m, d),
    };
    let value = create_temporal_object(
        agent,
        &Value::Undefined,
        PLAIN_MONTH_DAY_PROTO,
        TemporalRecord::MonthDay([y, m, d]),
    )?;
    with_calendar(agent, value, Some(&calendar))
}

/// spec 6.4.4 `equals`.
fn plain_month_day_equals(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let md = match require_record(agent, this, RecordKind::MonthDay)? {
        TemporalRecord::MonthDay(md) => md,
        _ => unreachable!(),
    };
    let other = to_plain_month_day(
        agent,
        &args.first().cloned().unwrap_or(Value::Undefined),
        &Value::Undefined,
    )?;
    let o = match require_record(agent, &other, RecordKind::MonthDay)? {
        TemporalRecord::MonthDay(md) => md,
        _ => unreachable!(),
    };
    Ok(Value::Boolean(
        o == md
            && super::temporal_calendar_id(agent, this)
                == super::temporal_calendar_id(agent, &other),
    ))
}

/// spec 6.4.6 `toPlainDate` (CalendarDateFromFields over {monthCode, day}
/// plus the provided year, constrained).
fn plain_month_day_to_plain_date(
    agent: &mut Agent,
    this: &Value,
    item: Value,
) -> Result<Value, JsError> {
    let md = match require_record(agent, this, RecordKind::MonthDay)? {
        TemporalRecord::MonthDay(md) => md,
        _ => unreachable!(),
    };
    if !matches!(item.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "argument should be an object".into(),
        ));
    }
    let calendar = super::temporal_calendar_id(agent, this);
    let value = crate::context::get_property(agent, &item, &JsString::from_utf8("year"), item)?;
    let year = match value.kind() {
        ValueKind::Undefined => None,
        _ => Some(super::to_integer_with_truncation(agent, &value)?),
    };
    // era/eraYear can supply the absent year (the corpus's toPlainDate
    // infinity-throws fixture pins eraYear's RangeError).
    let year = read_era_fields(agent, &item, Some(&calendar.to_string_lossy()), year)?;
    let Some(year) = year else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "year is required".into(),
        ));
    };
    let (y, m, d) = resolve_date_fields(
        Some(year),
        None,
        Some(format!("M{:02}", md[1])),
        Some(md[2]),
        true,
        Some(&calendar.to_string_lossy()),
    )?;
    let value = create_plain_date(agent, (y, m, d), &Value::Undefined)?;
    super::set_temporal_calendar(agent, &value, Some(&calendar.to_string_lossy()));
    Ok(value)
}

/// spec 6.3.1 Temporal.PlainYearMonth (the optional referenceISODay is
/// stored; RejectISODate then RejectYearMonthRange bound the result).
fn construct_year_month(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    let y = super::to_integer_with_truncation(agent, args.first().unwrap_or(&Value::Undefined))?;
    let m = super::to_integer_with_truncation(agent, args.get(1).unwrap_or(&Value::Undefined))?;
    let calendar = check_calendar(agent, args.get(2).unwrap_or(&Value::Undefined))?;
    let d = match args.get(3) {
        Some(v) if !matches!(v.kind(), ValueKind::Undefined) => {
            super::to_integer_with_truncation(agent, v)?
        }
        _ => 1,
    };
    if !iso::is_valid_iso_date(y, m, d)
        || !(-271_821..=275_760).contains(&y)
        || (y == -271_821 && m < 4)
        || (y == 275_760 && m > 9)
    {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "invalid year-month".into(),
        ));
    }
    let value = create_temporal_object(
        agent,
        new_target,
        PLAIN_YEAR_MONTH_PROTO,
        TemporalRecord::YearMonth([y, m, d]),
    )?;
    super::set_temporal_calendar(agent, &value, calendar.as_deref());
    Ok(value)
}

/// spec 6.4.1 Temporal.PlainMonthDay (the optional referenceISOYear defaults
/// to 1972; RejectISODate then RejectDateRange bound the result).
fn construct_month_day(
    agent: &mut Agent,
    args: &[Value],
    new_target: &Value,
) -> Result<Value, JsError> {
    let m = super::to_integer_with_truncation(agent, args.first().unwrap_or(&Value::Undefined))?;
    let d = super::to_integer_with_truncation(agent, args.get(1).unwrap_or(&Value::Undefined))?;
    let calendar = check_calendar(agent, args.get(2).unwrap_or(&Value::Undefined))?;
    let y = match args.get(3) {
        Some(v) if !matches!(v.kind(), ValueKind::Undefined) => {
            super::to_integer_with_truncation(agent, v)?
        }
        _ => 1972,
    };
    if !iso::is_valid_iso_date(y, m, d) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "invalid month-day".into(),
        ));
    }
    // CreateTemporalMonthDaySlots: RejectDateRange on the reference date.
    let days = iso::iso_date_to_epoch_days(y, m - 1, d);
    if !(-100_000_001..=100_000_000).contains(&days) {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "date out of range".into(),
        ));
    }
    let value = create_temporal_object(
        agent,
        new_target,
        PLAIN_MONTH_DAY_PROTO,
        TemporalRecord::MonthDay([y, m, d]),
    )?;
    super::set_temporal_calendar(agent, &value, calendar.as_deref());
    Ok(value)
}

/// spec 5.5.5 `equals`.
fn plain_date_time_equals(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let dt = match require_record(agent, this, RecordKind::PlainDateTime)? {
        TemporalRecord::PlainDateTime(dt) => dt,
        _ => unreachable!(),
    };
    let other = to_plain_date_time(
        agent,
        &args.first().cloned().unwrap_or(Value::Undefined),
        &Value::Undefined,
    )?;
    let o = match require_record(agent, &other, RecordKind::PlainDateTime)? {
        TemporalRecord::PlainDateTime(dt) => dt,
        _ => unreachable!(),
    };
    Ok(Value::Boolean(
        o == dt
            && super::temporal_calendar_id(agent, this)
                == super::temporal_calendar_id(agent, &other),
    ))
}

/// spec 6.5.6 `equals` (fixed-offset/UTC zones only).
fn zoned_equals(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let (ns, tz) = match require_record(agent, this, RecordKind::ZonedDateTime)? {
        TemporalRecord::ZonedDateTime(ns, tz) => (ns, tz.to_string_lossy()),
        _ => unreachable!(),
    };
    let other = to_zoned(
        agent,
        &args.first().cloned().unwrap_or(Value::Undefined),
        &Value::Undefined,
    )?;
    let (ons, otz) = match require_record(agent, &other, RecordKind::ZonedDateTime)? {
        TemporalRecord::ZonedDateTime(ns, tz) => (ns, tz.to_string_lossy()),
        _ => unreachable!(),
    };
    Ok(Value::Boolean(
        ns == ons
            && tz.eq_ignore_ascii_case(&otz)
            && super::temporal_calendar_id(agent, this)
                == super::temporal_calendar_id(agent, &other),
    ))
}

/// spec 6.3.2 ToTemporalYearMonth: records, strings (TemporalYearMonthString),
/// and property bags (the calendar, then month/monthCode/year, resolved with
/// the overflow option).
pub fn to_plain_year_month(
    agent: &mut Agent,
    item: &Value,
    options: &Value,
) -> Result<Value, JsError> {
    if let ValueKind::Object(obj) = item.kind()
        && let Some(record) = agent.temporal_data.get(&obj.id()).cloned()
    {
        let opts = super::get_options_object(options)?;
        super::get_temporal_overflow_option(agent, &opts)?;
        let calendar = super::temporal_calendar_id(agent, item);
        let value = match record {
            TemporalRecord::YearMonth(ym) => create_temporal_object(
                agent,
                &Value::Undefined,
                PLAIN_YEAR_MONTH_PROTO,
                TemporalRecord::YearMonth(ym),
            ),
            TemporalRecord::PlainDate(d) => create_temporal_object(
                agent,
                &Value::Undefined,
                PLAIN_YEAR_MONTH_PROTO,
                TemporalRecord::YearMonth([d[0], d[1], 1]),
            ),
            TemporalRecord::PlainDateTime(dt) => create_temporal_object(
                agent,
                &Value::Undefined,
                PLAIN_YEAR_MONTH_PROTO,
                TemporalRecord::YearMonth([dt[0], dt[1], 1]),
            ),
            _ => Err(JsError::new(
                ErrorKind::TypeError,
                "value is not convertible to a PlainYearMonth".into(),
            )),
        }?;
        return with_calendar(agent, value, Some(&calendar.to_string_lossy()));
    }
    if matches!(item.kind(), ValueKind::String(_)) {
        let text = crate::context::to_string(agent, item)?;
        let parsed = iso::parse_iso_date_time(text.as_slice(), iso::Format::DateTimePlain)
            .or_else(|_| iso::parse_iso_date_time(text.as_slice(), iso::Format::YearMonthString))
            .map_err(|_| JsError::new(ErrorKind::RangeError, "invalid year-month string".into()))?;
        if parsed.tz.z {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "Z designator not supported for PlainYearMonth".into(),
            ));
        }
        let calendar = match parsed.calendar.as_deref() {
            Some(c) => Some(super::canonicalize_calendar_id(c).ok_or_else(|| {
                JsError::new(ErrorKind::RangeError, "invalid calendar identifier".into())
            })?),
            None => None,
        };
        let opts = super::get_options_object(options)?;
        super::get_temporal_overflow_option(agent, &opts)?;
        // RejectYearMonthRange, then re-resolve with constrain (iso8601 is
        // the identity). The ISO calendar's reference day is the first of the
        // month (test262 from/argument-string.js: every string yields
        // "1976-11-01"); a non-ISO calendar keeps the parsed reference day
        // (the islamicc string fixture pins "2024-06-08[u-ca=islamicc]"
        // with day 8).
        let (y, m) = resolve_year_month(Some(parsed.year), Some(parsed.month), None, true, None)?;
        let day = if calendar.is_none() || calendar.as_deref() == Some("iso8601") {
            1
        } else {
            parsed.day
        };
        let value = create_temporal_object(
            agent,
            &Value::Undefined,
            PLAIN_YEAR_MONTH_PROTO,
            TemporalRecord::YearMonth([y, m, day]),
        )?;
        return with_calendar(agent, value, calendar.as_deref());
    }
    if let ValueKind::Object(_) = item.kind() {
        // Property bag: the calendar, then the fields in ascending code point
        // order (PrepareCalendarFields complete).
        let calendar = read_bag_calendar(agent, item)?;
        let calendar_id = calendar.as_deref().unwrap_or("iso8601");
        let (month, month_code, year) = read_year_month_fields(agent, item, false, calendar_id)?;
        let year = read_era_fields(agent, item, calendar.as_deref(), year)?;
        let opts = super::get_options_object(options)?;
        let constrain = super::get_temporal_overflow_option(agent, &opts)? == Overflow::Constrain;
        let (y, m) = resolve_year_month(year, month, month_code, constrain, calendar.as_deref())?;
        // CalendarYearMonthFromFields for the tabular Islamic calendars: the
        // ISO year-month of the month's first day with its reference day
        // (1445-12 → 2024-06 with reference day 8).
        let (y, m, d) = if let Some(calendar) = calendar.as_deref()
            && let Some((cy, cm, cd)) = calendar::calendar_year_month_to_iso(calendar, y, m)
        {
            (cy, cm, cd)
        } else {
            (y, m, 1)
        };
        let value = create_temporal_object(
            agent,
            &Value::Undefined,
            PLAIN_YEAR_MONTH_PROTO,
            TemporalRecord::YearMonth([y, m, d]),
        )?;
        return with_calendar(agent, value, calendar.as_deref());
    }
    Err(JsError::new(
        ErrorKind::TypeError,
        "value must be a string or object".into(),
    ))
}

/// spec 6.4.2 ToTemporalMonthDay: records, strings (TemporalMonthDayString),
/// and property bags (the calendar, then day/month/monthCode/year, resolved
/// with the overflow option).
/// Whether the calendar year can produce an ISO date within the engine's
/// limits (NonISOMonthDayToISOReferenceDate 5.a: a year with no representable
/// ISO date throws a RangeError before the month resolution — the
/// dont-calculate-month-info-for-out-of-range-year fixture pins ±999999).
fn calendar_year_in_iso_limits(calendar: &str, year: i64) -> bool {
    let (iy, im, id) = match calendar {
        // The linear calendars map directly to ISO (the japanese era offset
        // is irrelevant for the extreme-year range check).
        "iso8601" | "gregory" => (year, 1, 1),
        "buddhist" => (year + 543, 1, 1),
        "roc" => (year + 1911, 1, 1),
        "japanese" => (year + 1868, 1, 1),
        _ => {
            let Some((iy, im, id)) = calendar::calendar_date_to_iso(calendar, year, 1, 1) else {
                return false;
            };
            (iy, im, id)
        }
    };
    iso::iso_date_time_within_limits(iy, im, id, 0, 0, 0, 0, 0, 0)
}

/// The reference ISO date of the calendar month-day of an ISO date
/// (ToTemporalMonthDay with a date input): the month-day converts through the
/// calendar and the reference search (NonISOMonthDayToISOReferenceDate). A
/// chinese/dangi month-day that never occurs in the reference window throws
/// with reject overflow (chinese-dangi-leap-month-with-year-from-plaindate
/// pins the RangeError); the other calendars fall back to the 1972
/// month-day.
fn month_day_reference_date(
    calendar: &str,
    iso: (i64, i64, i64),
    constrain: bool,
) -> Result<[i64; 3], JsError> {
    let Some((cal_y, cm, cd)) = calendar::calendar_iso_to_date(calendar, iso.0, iso.1, iso.2)
    else {
        return Ok([1972, iso.1, iso.2]);
    };
    let code = if matches!(calendar, "hebrew" | "chinese" | "dangi") {
        Some(calendar::calendar_month_code(calendar, cal_y, cm))
    } else {
        None
    };
    match calendar::calendar_month_day_reference(calendar, cm, cd, code.as_deref(), constrain) {
        Some((y, m, d)) => Ok([y, m, d]),
        None if matches!(calendar, "chinese" | "dangi") => Err(JsError::new(
            ErrorKind::RangeError,
            "invalid month-day".into(),
        )),
        None => Ok([1972, iso.1, iso.2]),
    }
}

pub fn to_plain_month_day(
    agent: &mut Agent,
    item: &Value,
    options: &Value,
) -> Result<Value, JsError> {
    if let ValueKind::Object(obj) = item.kind()
        && let Some(record) = agent.temporal_data.get(&obj.id()).cloned()
    {
        let opts = super::get_options_object(options)?;
        let constrain = super::get_temporal_overflow_option(agent, &opts)? == Overflow::Constrain;
        let calendar = super::temporal_calendar_id(agent, item);
        let value = match record {
            TemporalRecord::MonthDay(md) => create_temporal_object(
                agent,
                &Value::Undefined,
                PLAIN_MONTH_DAY_PROTO,
                TemporalRecord::MonthDay(md),
            ),
            TemporalRecord::PlainDate(d) => create_temporal_object(
                agent,
                &Value::Undefined,
                PLAIN_MONTH_DAY_PROTO,
                TemporalRecord::MonthDay(month_day_reference_date(
                    &calendar.to_string_lossy(),
                    (d[0], d[1], d[2]),
                    constrain,
                )?),
            ),
            TemporalRecord::PlainDateTime(dt) => create_temporal_object(
                agent,
                &Value::Undefined,
                PLAIN_MONTH_DAY_PROTO,
                TemporalRecord::MonthDay(month_day_reference_date(
                    &calendar.to_string_lossy(),
                    (dt[0], dt[1], dt[2]),
                    constrain,
                )?),
            ),
            _ => Err(JsError::new(
                ErrorKind::TypeError,
                "value is not convertible to a PlainMonthDay".into(),
            )),
        }?;
        return with_calendar(agent, value, Some(&calendar.to_string_lossy()));
    }
    if matches!(item.kind(), ValueKind::String(_)) {
        let text = crate::context::to_string(agent, item)?;
        let parsed = iso::parse_iso_date_time(text.as_slice(), iso::Format::DateTimePlain)
            .or_else(|_| iso::parse_iso_date_time(text.as_slice(), iso::Format::MonthDayString))
            .map_err(|_| JsError::new(ErrorKind::RangeError, "invalid month-day string".into()))?;
        if parsed.tz.z {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "Z designator not supported for PlainMonthDay".into(),
            ));
        }
        let calendar = match parsed.calendar.as_deref() {
            Some(c) => Some(super::canonicalize_calendar_id(c).ok_or_else(|| {
                JsError::new(ErrorKind::RangeError, "invalid calendar identifier".into())
            })?),
            None => None,
        };
        let opts = super::get_options_object(options)?;
        super::get_temporal_overflow_option(agent, &opts)?;
        // NonISOMonthDayToISOReferenceDate 5.a: a full-date month-day string's
        // year must produce an ISO date within the limits (test262
        // plainMonthDayStringsInvalid pins "±999999-01-01[u-ca=gregory]"),
        // while the ISO calendar accepts ±999999 (plainMonthDayStringsValid)
        // and the short form's reference year (1972) is always within.
        if let Some(cal) = calendar.as_deref()
            && cal != "iso8601"
            && !calendar_year_in_iso_limits(cal, parsed.year)
        {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "calendar year out of ISO limits".into(),
            ));
        }
        // The calendar month-day of the parsed ISO date, stored as the ISO
        // reference date in the latest ISO year at or before 1972 (the
        // reference-date-noniso-calendar fixture pins 2023-01-01[u-ca=hebrew]
        // -> M04-08 with the 1972 reference year). This step always runs
        // with constrain (the spec's note in CalendarMonthDayFromFields).
        let md = month_day_reference_date(
            calendar.as_deref().unwrap_or("iso8601"),
            (parsed.year, parsed.month, parsed.day),
            true,
        )?;
        let value = create_temporal_object(
            agent,
            &Value::Undefined,
            PLAIN_MONTH_DAY_PROTO,
            TemporalRecord::MonthDay(md),
        )?;
        return with_calendar(agent, value, calendar.as_deref());
    }
    if let ValueKind::Object(_) = item.kind() {
        // Property bag: the calendar, then the fields in ascending code point
        // order (PrepareCalendarFields complete).
        let calendar = read_bag_calendar(agent, item)?;
        let calendar_id = calendar.as_deref().unwrap_or("iso8601");
        let (day, month, month_code, year) =
            read_month_day_fields(agent, item, false, calendar_id)?;
        let year = read_era_fields(agent, item, calendar.as_deref(), year)?;
        // spec (NonISOResolveFields): a numeric month requires a year (the
        // month cannot be resolved without it — the month/monthCode conflict
        // fixtures pin the TypeError before the RangeError); a bare monthCode
        // without a year is the PlainMonthDay reference search.
        if calendar.as_deref().unwrap_or("iso8601") != "iso8601"
            && month.is_some()
            && year.is_none()
        {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "month requires a year for a non-ISO calendar".into(),
            ));
        }
        // spec (NonISOMonthDayToISOReferenceDate 5.a): a year with no
        // representable ISO date throws before the month resolution
        // (dont-calculate-month-info-for-out-of-range-year pins ±999999).
        if let Some(cal) = calendar.as_deref()
            && let Some(y) = year
            && !calendar_year_in_iso_limits(cal, y)
        {
            return Err(JsError::new(
                ErrorKind::RangeError,
                "year out of range".into(),
            ));
        }
        let opts = super::get_options_object(options)?;
        let constrain = super::get_temporal_overflow_option(agent, &opts)? == Overflow::Constrain;
        let (m, d) = resolve_month_day(
            year,
            month,
            month_code.clone(),
            day,
            constrain,
            calendar.as_deref(),
        )?;
        // The reference search resolves the month code of the resolved
        // month-day (a leap month resolved from a numeric month keeps its
        // leap code — chinese-calendar-dates pins 2001-M04L-15 → 1963).
        let reference_code = month_code.clone().or_else(|| {
            let cal = calendar.as_deref()?;
            year.map(|y| calendar::calendar_month_code(cal, y, m))
        });
        // CalendarMonthDayFromFields: the month-day is stored as the ISO
        // reference date in the latest ISO year at or before 1972 where it
        // exists (the explicit year participates only in the overflow
        // regulation above; reference-year-1972 pins the 1972 reference even
        // for year 5781). A chinese/dangi reject overflow with a month-day
        // that never occurs in the window throws (the leap-month fixtures);
        // the other calendars fall back to 1972.
        let (y, m, d) = if let Some(calendar) = calendar.as_deref() {
            match calendar::calendar_month_day_reference(
                calendar,
                m,
                d,
                reference_code.as_deref(),
                constrain,
            ) {
                Some(date) => date,
                None if matches!(calendar, "chinese" | "dangi") => {
                    return Err(JsError::new(
                        ErrorKind::RangeError,
                        "invalid month-day".into(),
                    ));
                }
                None => (1972, m, d),
            }
        } else {
            (1972, m, d)
        };
        let value = create_temporal_object(
            agent,
            &Value::Undefined,
            PLAIN_MONTH_DAY_PROTO,
            TemporalRecord::MonthDay([y, m, d]),
        )?;
        return with_calendar(agent, value, calendar.as_deref());
    }
    Err(JsError::new(
        ErrorKind::TypeError,
        "value must be a string or object".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns(h: i64, min: i64, s: i64) -> i128 {
        (h as i128 * 3600 + min as i128 * 60 + s as i128) * 1_000_000_000
    }

    fn wall_epoch(y: i64, m: i64, d: i64, h: i64, min: i64) -> i128 {
        iso::get_utc_epoch_nanoseconds(y, m, d, h, min, 0, 0, 0, 0)
    }

    /// GetPossibleInstantsFor: 0 (a gap), 1, or 2 (an overlap) instants.
    #[test]
    fn possible_instants_for_wall_cases() {
        // Vancouver spring-forward gap 2000-04-02T02:30 (test262 add/dst.js).
        assert_eq!(
            possible_instants_for_wall("America/Vancouver", 2000, 4, 2, 2, 30, 0, 0, 0, 0)
                .unwrap()
                .len(),
            0
        );
        // The overlap on the same day's evening is a single instant.
        assert_eq!(
            possible_instants_for_wall("America/Vancouver", 2000, 4, 2, 12, 0, 0, 0, 0, 0)
                .unwrap()
                .len(),
            1
        );
        // Sao Paulo fall-back overlap 2019-02-16T23:45 (-02:00 -> -03:00).
        let overlap =
            possible_instants_for_wall("America/Sao_Paulo", 2019, 2, 16, 23, 45, 0, 0, 0, 0)
                .unwrap();
        assert_eq!(overlap.len(), 2);
        assert_eq!(overlap[0], wall_epoch(2019, 2, 17, 1, 45)); // -02:00
        assert_eq!(overlap[1], wall_epoch(2019, 2, 17, 2, 45)); // -03:00
        // The Apia dateline jump: 2011-12-30 is a skipped day.
        assert_eq!(
            possible_instants_for_wall("Pacific/Apia", 2011, 12, 30, 22, 0, 0, 0, 0, 0)
                .unwrap()
                .len(),
            0
        );
        // A normal instant.
        assert_eq!(
            possible_instants_for_wall("America/Vancouver", 2000, 4, 2, 12, 0, 0, 0, 0, 0).unwrap(),
            vec![wall_epoch(2000, 4, 2, 19, 0)]
        );
    }

    /// DisambiguatePossibleInstants through a gap (test262
    /// from/argument-string-dst-option-disambiguation.js: compatible is the
    /// legacy-Date mapping — the offset before the gap; earlier the offset
    /// after).
    #[test]
    fn gap_disambiguation_la() {
        let (y, mo, d) = (2020, 3, 8);
        assert_eq!(
            wall_to_epoch_ns(
                "America/Los_Angeles",
                y,
                mo,
                d,
                2,
                30,
                0,
                0,
                0,
                0,
                "compatible"
            )
            .unwrap(),
            wall_epoch(y, mo, d, 10, 30)
        );
        assert_eq!(
            wall_to_epoch_ns(
                "America/Los_Angeles",
                y,
                mo,
                d,
                2,
                30,
                0,
                0,
                0,
                0,
                "earlier"
            )
            .unwrap(),
            wall_epoch(y, mo, d, 9, 30)
        );
        assert_eq!(
            wall_to_epoch_ns("America/Los_Angeles", y, mo, d, 2, 30, 0, 0, 0, 0, "later").unwrap(),
            wall_epoch(y, mo, d, 10, 30)
        );
        assert!(
            wall_to_epoch_ns("America/Los_Angeles", y, mo, d, 2, 30, 0, 0, 0, 0, "reject").is_err()
        );
    }

    /// DisambiguatePossibleInstants through an overlap (test262
    /// from/argument-string-dst-option-disambiguation.js).
    #[test]
    fn overlap_disambiguation_sao_paulo() {
        let (y, mo, d) = (2019, 2, 16);
        assert_eq!(
            wall_to_epoch_ns(
                "America/Sao_Paulo",
                y,
                mo,
                d,
                23,
                45,
                0,
                0,
                0,
                0,
                "compatible"
            )
            .unwrap(),
            wall_epoch(2019, 2, 17, 1, 45)
        );
        assert_eq!(
            wall_to_epoch_ns("America/Sao_Paulo", y, mo, d, 23, 45, 0, 0, 0, 0, "earlier").unwrap(),
            wall_epoch(2019, 2, 17, 1, 45)
        );
        assert_eq!(
            wall_to_epoch_ns("America/Sao_Paulo", y, mo, d, 23, 45, 0, 0, 0, 0, "later").unwrap(),
            wall_epoch(2019, 2, 17, 2, 45)
        );
        assert!(
            wall_to_epoch_ns("America/Sao_Paulo", y, mo, d, 23, 45, 0, 0, 0, 0, "reject").is_err()
        );
    }

    /// The Apia dateline jump: compatible resolves the skipped wall time
    /// with the offset before the gap (test262 add/dst.js: 22:00 Dec 30 ->
    /// 08:00Z Dec 31).
    #[test]
    fn apia_skipped_day() {
        assert_eq!(
            wall_to_epoch_ns(
                "Pacific/Apia",
                2011,
                12,
                30,
                22,
                0,
                0,
                0,
                0,
                0,
                "compatible"
            )
            .unwrap(),
            wall_epoch(2011, 12, 31, 8, 0)
        );
        assert_eq!(
            wall_to_epoch_ns("Pacific/Apia", 2011, 12, 30, 22, 0, 0, 0, 0, 0, "earlier").unwrap(),
            wall_epoch(2011, 12, 30, 8, 0)
        );
    }

    /// InterpretISODateTimeOffset wall behaviour routes through the
    /// disambiguation, and the prefer offset falls back to it when the given
    /// offset is not a possible instant (test262 from/argument-string-dst-
    /// option-offset-disambiguation-combinations.js).
    #[test]
    fn interpret_offset_options() {
        // Wall behaviour (no offset in the string), compatible: the gap
        // resolves to the pre-gap offset.
        let dt = [2020, 3, 8, 2, 30, 0, 0, 0, 0];
        assert_eq!(
            interpret_iso_date_time_offset(
                dt,
                "America/Los_Angeles",
                None,
                false,
                "reject",
                "compatible",
                true
            )
            .unwrap(),
            wall_epoch(2020, 3, 8, 10, 30)
        );
        // Prefer with a wrong offset falls back to the disambiguation.
        assert_eq!(
            interpret_iso_date_time_offset(
                dt,
                "America/Los_Angeles",
                Some(-86_340_000_000_000), // -23:59
                false,
                "prefer",
                "compatible",
                true,
            )
            .unwrap(),
            wall_epoch(2020, 3, 8, 10, 30)
        );
        // Reject with a wrong offset throws.
        assert!(
            interpret_iso_date_time_offset(
                dt,
                "America/Los_Angeles",
                Some(-86_340_000_000_000),
                false,
                "reject",
                "compatible",
                true,
            )
            .is_err()
        );
        // Prefer with the matching offset (the overlap second occurrence).
        let dt = [2020, 11, 1, 1, 30, 0, 0, 0, 0];
        assert_eq!(
            interpret_iso_date_time_offset(
                dt,
                "America/Los_Angeles",
                Some(-8 * 3_600_000_000_000),
                false,
                "prefer",
                "compatible",
                true,
            )
            .unwrap(),
            wall_epoch(2020, 11, 1, 9, 30)
        );
    }

    /// The start-of-day instants behind the ZonedDateTime round day path
    /// (test262 round/dst-skipped-cross-midnight.js: 11:45 is exactly half of
    /// the 23.5-hour Toronto day; same-date-starts-twice.js: the Casey wall
    /// day is 27h while the startOfDay span is 24h).
    fn start_of_day(tz: &str, y: i64, m: i64, d: i64) -> i128 {
        let possible = possible_instants_for_wall(tz, y, m, d, 0, 0, 0, 0, 0, 0).unwrap();
        if !possible.is_empty() {
            return possible[0];
        }
        // A midnight gap: the day starts at the transition instant (the
        // first valid local time).
        let zone = unicode::tz::resolve_zone(tz).unwrap();
        let wall = iso::get_utc_epoch_nanoseconds(y, m, d, 0, 0, 0, 0, 0, 0);
        let day = 86_400_000_000_000i128;
        let (o1, ..) = unicode::tz::offset_info_at(zone, wall - day);
        let (o2, ..) = unicode::tz::offset_info_at(zone, wall + day);
        let e_before = wall - o1 as i128 * 1_000_000_000;
        let e_after = wall - o2 as i128 * 1_000_000_000;
        unicode::tz::next_transition(zone, e_before.min(e_after))
            .unwrap()
            .at_secs as i128
            * 1_000_000_000
    }

    #[test]
    fn round_day_windows() {
        // Toronto 1919-03-30: a 23.5-hour day — the transition at 23:30 EST
        // skips 00:00 on Mar 31, so the day starts at 00:30 EDT (30 minutes
        // before the disambiguated midnight).
        let s0 = start_of_day("America/Toronto", 1919, 3, 30);
        let s1 = start_of_day("America/Toronto", 1919, 3, 31);
        assert_eq!(s0, wall_epoch(1919, 3, 30, 5, 0)); // 00:00 EST
        assert_eq!(s1, wall_epoch(1919, 3, 31, 4, 30)); // 00:30 EDT
        assert_eq!(s1 - s0, ns(23, 30, 0));

        // Casey 2010-03-04: the startOfDay span is 24h, but the 3h backward
        // shift replays wall times past the start of Mar 5 (13:00Z) — the
        // round progress for such an instant is the wall time-of-day.
        let c0 = start_of_day("Antarctica/Casey", 2010, 3, 4);
        let c1 = start_of_day("Antarctica/Casey", 2010, 3, 5);
        assert_eq!(c0, wall_epoch(2010, 3, 3, 13, 0)); // 00:00+11:00
        assert_eq!(c1, wall_epoch(2010, 3, 4, 13, 0)); // the first 00:00
        assert_eq!(c1 - c0, ns(24, 0, 0));
    }
}
