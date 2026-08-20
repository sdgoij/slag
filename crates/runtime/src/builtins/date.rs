//! The `%Date%` intrinsic (spec 21.4): the constructor, `Date.parse`/`UTC`/
//! `now`, and `%Date.prototype%` (UTC and local-time getters/setters, the
//! string forms, `toJSON`, `valueOf`, `@@toPrimitive`). Time values are ms
//! since the epoch, stored per instance in the agent's `date_data` table.
//! Local time currently uses a fixed UTC offset of 0 (the host-timezone
//! plumbing is documented follow-up work); every other spec algorithm is
//! exact.

use crux::convert::to_integer_or_infinity;
use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, ValueKind, is_callable};

use crate::agent::Agent;
use crate::builtins::temporal::instant::create_instant;
use crate::context::as_object;
use crate::realm::Realm;

const DATE: &str = "%Date%";
const DATE_PROTO: &str = "%Date.prototype%";
const DATE_TO_PRIMITIVE: &str = "%Date.prototype.@@toPrimitive%";

const MS_PER_DAY: f64 = 86_400_000.0;
const MS_PER_HOUR: f64 = 3_600_000.0;
const MS_PER_MINUTE: f64 = 60_000.0;
const MS_PER_SECOND: f64 = 1_000.0;

const DAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn placeholder(name: &'static str) -> NativeFn {
    Box::new(move |_, _| {
        Err(JsError::new(
            ErrorKind::TypeError,
            format!("{name} must be called through the agent"),
        ))
    })
}

/// Date.prototype[@@toPrimitive] (spec 21.4.3.5): OrdinaryToPrimitive with
/// the tryFirst order chosen by the hint — "default" prefers the string
/// form, so binary `+` yields the date text while unary `+` (number hint)
/// yields the time value. The hint is compared as a String value; any other
/// value (undefined included) throws a TypeError.
fn date_to_primitive(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    if !matches!(this.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Date.prototype[Symbol.toPrimitive] called on a non-object".into(),
        ));
    }
    let hint = args.first().cloned().unwrap_or(Value::Undefined);
    let is_text = |s: &JsString, text: &str| {
        s.as_slice() == text.encode_utf16().collect::<Vec<u16>>().as_slice()
    };
    let (first, second) = match hint.kind() {
        ValueKind::String(text) if is_text(&text, "string") => ("toString", "valueOf"),
        ValueKind::String(text) if is_text(&text, "default") => ("toString", "valueOf"),
        ValueKind::String(text) if is_text(&text, "number") => ("valueOf", "toString"),
        _ => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Invalid hint value for Date.prototype[Symbol.toPrimitive]".into(),
            ));
        }
    };
    for name in [first, second] {
        let method = crate::context::get_property_key(
            agent,
            this,
            &PropertyKey::from_utf8(name),
            this.clone(),
        )?;
        if is_callable(&method) {
            let result = crate::function::call(agent, &method, this.clone(), &[])?;
            if !matches!(result.kind(), ValueKind::Object(_) | ValueKind::Function(_)) {
                return Ok(result);
            }
        }
    }
    Err(JsError::new(
        ErrorKind::TypeError,
        "Cannot convert object to primitive value".into(),
    ))
}

/// The [[DateValue]] time value of `this` (spec 21.4.3.1 RequireInternalSlot).
fn this_date_value(agent: &Agent, this: &Value) -> Result<f64, JsError> {
    match this.kind() {
        ValueKind::Object(obj) => match agent.date_data.get(&obj.id()) {
            Some(t) => Ok(*t),
            None => Err(JsError::new(
                ErrorKind::TypeError,
                "Date.prototype method called on an incompatible receiver".into(),
            )),
        },
        _ => Err(JsError::new(
            ErrorKind::TypeError,
            "Date.prototype method called on an incompatible receiver".into(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Time math (spec 21.4.1).
// ---------------------------------------------------------------------------

/// spec 21.4.1.4 DayFromYear.
fn day_from_year(year: i64) -> f64 {
    let y = year as f64;
    365.0 * (y - 1970.0) + ((y - 1969.0) / 4.0).floor() - ((y - 1901.0) / 100.0).floor()
        + ((y - 1601.0) / 400.0).floor()
}

/// spec 21.4.1.3 YearFromTime.
fn year_from_time(time: f64) -> i64 {
    let days = (time / MS_PER_DAY).floor() as i64;
    let mut year = 1970 + ((days as f64) / 365.2425).floor() as i64;
    while day_from_year(year) > days as f64 {
        year -= 1;
    }
    while day_from_year(year + 1) <= days as f64 {
        year += 1;
    }
    year
}

/// Days in each month of `year` (leap-aware).
fn days_in_month(year: i64, month: i64) -> i64 {
    const MONTH_DAYS: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if month == 1 && year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
        return 29;
    }
    MONTH_DAYS[month as usize]
}

/// spec 21.4.1.11 MonthFromTime.
fn month_from_time(time: f64) -> i64 {
    let year = year_from_time(time);
    let mut day = (time / MS_PER_DAY).floor() - day_from_year(year);
    let mut month = 0;
    while month < 12 {
        let days = days_in_month(year, month) as f64;
        if day < days {
            return month;
        }
        day -= days;
        month += 1;
    }
    11
}

/// spec 21.4.1.12 DateFromTime.
fn date_from_time(time: f64) -> f64 {
    let year = year_from_time(time);
    let mut day = (time / MS_PER_DAY).floor() - day_from_year(year);
    let mut month = 0;
    while month < 12 {
        let days = days_in_month(year, month) as f64;
        if day < days {
            return day + 1.0;
        }
        day -= days;
        month += 1;
    }
    day + 1.0
}

/// spec 21.4.1.13 WeekDay.
fn week_day(time: f64) -> i64 {
    (((time / MS_PER_DAY).floor() as i64) + 4).rem_euclid(7)
}

/// spec 21.4.1.14 HourFromTime.
fn hour_from_time(time: f64) -> i64 {
    ((time / MS_PER_HOUR).floor() as i64).rem_euclid(24)
}

/// spec 21.4.1.15 MinFromTime.
fn min_from_time(time: f64) -> i64 {
    ((time / MS_PER_MINUTE).floor() as i64).rem_euclid(60)
}

/// spec 21.4.1.16 SecFromTime.
fn sec_from_time(time: f64) -> i64 {
    ((time / MS_PER_SECOND).floor() as i64).rem_euclid(60)
}

/// spec 21.4.1.17 msFromTime.
fn ms_from_time(time: f64) -> i64 {
    time.rem_euclid(1000.0) as i64
}

/// spec 21.4.1.18 TimeWithinDay.
fn time_within_day(time: f64) -> f64 {
    time.rem_euclid(MS_PER_DAY)
}

/// spec 21.4.1.12 MakeDay.
fn make_day(year: f64, month: f64, date: f64) -> f64 {
    if year.is_nan() || month.is_nan() || date.is_nan() {
        return f64::NAN;
    }
    let year = year.trunc();
    let month = month.trunc();
    let date = date.trunc();
    let ym = year + (month / 12.0).floor();
    if !ym.is_finite() {
        return f64::NAN;
    }
    let mn = month.rem_euclid(12.0);
    let mut day_within = 0.0f64;
    for m in 0..(mn as i64) {
        day_within += days_in_month(ym as i64, m) as f64;
    }
    day_from_year(ym as i64) + day_within + date - 1.0
}

/// spec 21.4.1.19 MakeTime.
fn make_time(hour: f64, min: f64, sec: f64, ms: f64) -> f64 {
    if !hour.is_finite() || !min.is_finite() || !sec.is_finite() || !ms.is_finite() {
        return f64::NAN;
    }
    hour.trunc() * MS_PER_HOUR
        + min.trunc() * MS_PER_MINUTE
        + sec.trunc() * MS_PER_SECOND
        + ms.trunc()
}

/// spec 21.4.1.20 MakeDate.
fn make_date(day: f64, time: f64) -> f64 {
    if day.is_nan() || time.is_nan() {
        f64::NAN
    } else {
        day * MS_PER_DAY + time
    }
}

/// spec 21.4.1.21 TimeClip.
fn time_clip(time: f64) -> f64 {
    if time.is_nan() || time.abs() > 8.64e15 {
        f64::NAN
    } else {
        // TimeClip adds +0 to convert -0 to +0 (spec 21.4.1.15 step 3).
        time.trunc() + 0.0
    }
}

/// spec 21.4.1.8 LocalTime — the host timezone offset is fixed at UTC for
/// now; the real offset plumbing is documented follow-up work.
fn local_time(time: f64) -> f64 {
    time
}

/// spec 21.4.1.7 UTC — inverse of LocalTime.
fn utc_time(time: f64) -> f64 {
    time
}

/// The current time in ms since the epoch.
pub(crate) fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

/// The year field of the Date Time String Format: 4 digits for 0-9999,
/// ±6 digits otherwise.
fn format_year(year: i64) -> String {
    if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else if year < 0 {
        // Negative years pad to at least four digits (no forced six).
        format!("-{:04}", year.abs())
    } else {
        format!("+{year:06}")
    }
}

// ---------------------------------------------------------------------------
// Constructor and statics.
// ---------------------------------------------------------------------------

fn instance_proto(agent: &mut Agent, new_target: &Value) -> Result<Handle<JsObject>, JsError> {
    let proto = crate::context::get_property(
        agent,
        new_target,
        &JsString::from_utf8("prototype"),
        new_target.clone(),
    )?;
    if let Some(obj) = as_object(&proto) {
        return Ok(obj);
    }
    // GetPrototypeFromConstructor (spec 10.1.8): a non-object prototype
    // falls back to the newTarget's realm's %Date.prototype%.
    crate::context::get_function_realm(agent, new_target)?
        .intrinsics
        .get(DATE_PROTO)
        .and_then(|value| as_object(&value))
        .ok_or_else(|| JsError::new(ErrorKind::TypeError, "%Date.prototype% missing".into()))
}

/// The time value from the constructor's arguments (spec 21.4.2.1).
fn date_value_from_args(agent: &mut Agent, args: &[Value]) -> Result<f64, JsError> {
    if args.is_empty() {
        return Ok(now_ms());
    }
    if args.len() == 1 {
        let value = &args[0];
        // spec 21.4.3.1: an object with a [[DateValue]] slot clones it
        // directly (no ToPrimitive); other objects are ToPrimitive'd.
        if let ValueKind::Object(obj) = value.kind()
            && let Some(t) = agent.date_data.get(&obj.id())
        {
            return Ok(*t);
        }
        let prim =
            crate::context::to_primitive(agent, value, crux::convert::ToPrimitiveHint::Default)?;
        if let ValueKind::String(text) = prim.kind() {
            return Ok(date_parse(&text));
        }
        return Ok(time_clip(crate::context::to_number(agent, &prim)?));
    }
    multi_arg_time(agent, args)
}

/// `new Date(...)` (spec 21.4.2.1).
fn date_construct(agent: &mut Agent, args: &[Value], new_target: &Value) -> Result<Value, JsError> {
    let proto = instance_proto(agent, new_target)?;
    let object = JsObject::ordinary_object_create(Some(proto));
    let time = date_value_from_args(agent, args)?;
    agent.date_data.insert(object.id(), time);
    Ok(Value::Object(object))
}

/// `Date()` call form (spec 21.4.2.2): the current time as a string.
fn date_call() -> Result<Value, JsError> {
    Ok(Value::String(Handle::new(JsString::from_utf8(
        &format_time(now_ms()),
    ))))
}

/// The shared multi-argument time computation (spec 21.4.2.4 Date.UTC and
/// the constructor's 2+ argument form), with the 0-99 year adjustment.
/// Arguments are ToNumber'd in order through the agent (each may throw).
fn multi_arg_time(agent: &mut Agent, args: &[Value]) -> Result<f64, JsError> {
    let year = match args.first() {
        Some(v) => crate::context::to_number(agent, v)?,
        None => f64::NAN,
    };
    let month = match args.get(1) {
        Some(v) => crate::context::to_number(agent, v)?,
        None => 0.0,
    };
    let date = match args.get(2) {
        Some(v) => crate::context::to_number(agent, v)?,
        None => 1.0,
    };
    let hours = match args.get(3) {
        Some(v) => crate::context::to_number(agent, v)?,
        None => 0.0,
    };
    let minutes = match args.get(4) {
        Some(v) => crate::context::to_number(agent, v)?,
        None => 0.0,
    };
    let seconds = match args.get(5) {
        Some(v) => crate::context::to_number(agent, v)?,
        None => 0.0,
    };
    let ms = match args.get(6) {
        Some(v) => crate::context::to_number(agent, v)?,
        None => 0.0,
    };
    // spec 21.4.2.4 step 8: the 1900 offset applies to ToInteger(y) in
    // [0, 99]; -0.9 truncates to -0 and counts as 0.
    let year = if year.is_nan() {
        year
    } else {
        let year_int = to_integer_or_infinity(year);
        if (0.0..=99.0).contains(&year_int) {
            year_int + 1900.0
        } else {
            year
        }
    };
    Ok(time_clip(make_date(
        make_day(year, month, date),
        make_time(hours, minutes, seconds, ms),
    )))
}

// ---------------------------------------------------------------------------
// Date.parse (spec 21.4.1.16 + the Date Time String Format).
// ---------------------------------------------------------------------------

fn parse_digits(text: &[u16], start: usize, count: usize) -> Option<(i64, usize)> {
    let mut value = 0i64;
    for i in 0..count {
        let unit = *text.get(start + i)?;
        if !(0x30..=0x39).contains(&unit) {
            return None;
        }
        value = value * 10 + (unit - 0x30) as i64;
    }
    Some((value, start + count))
}

/// The Date Time String Format (spec 21.4.1.15): `YYYY-MM-DDTHH:mm:ss.sssZ`.
fn parse_iso(text: &[u16]) -> Option<f64> {
    let (negative_year, mut year, mut pos) = match text.first()? {
        u if *u == b'+' as u16 => (false, parse_digits(text, 1, 6)?.0, 7),
        u if *u == b'-' as u16 => (true, parse_digits(text, 1, 6)?.0, 7),
        _ => (false, parse_digits(text, 0, 4)?.0, 4),
    };
    if negative_year {
        year = -year;
        // "-000000" is invalid: a negative extended year is never zero
        // (spec 21.4.1.16 step 4).
        if year == 0 {
            return None;
        }
    }
    if !(-271821..=275760).contains(&year) {
        return None;
    }
    let (mut month, mut date) = (1i64, 1i64);
    if text.get(pos) == Some(&(b'-' as u16)) {
        pos += 1;
        (month, pos) = parse_digits(text, pos, 2)?;
        if !(1..=12).contains(&month) {
            return None;
        }
        if text.get(pos) == Some(&(b'-' as u16)) {
            pos += 1;
            (date, pos) = parse_digits(text, pos, 2)?;
            if !(1..=31).contains(&date) {
                return None;
            }
        }
    }
    let (mut hours, mut minutes, mut seconds, mut ms) = (0i64, 0i64, 0i64, 0i64);
    let mut has_time = false;
    if text.get(pos) == Some(&(b'T' as u16)) {
        has_time = true;
        pos += 1;
        (hours, pos) = parse_digits(text, pos, 2)?;
        if !(0..=24).contains(&hours) {
            return None;
        }
        if text.get(pos) == Some(&(b':' as u16)) {
            pos += 1;
            (minutes, pos) = parse_digits(text, pos, 2)?;
            if !(0..=59).contains(&minutes) {
                return None;
            }
            if text.get(pos) == Some(&(b':' as u16)) {
                pos += 1;
                (seconds, pos) = parse_digits(text, pos, 2)?;
                if !(0..=59).contains(&seconds) {
                    return None;
                }
                if text.get(pos) == Some(&(b'.' as u16)) {
                    pos += 1;
                    let mut fraction = 0i64;
                    let mut count = 0;
                    while count < 3 {
                        let unit = *text.get(pos)?;
                        if !(0x30..=0x39).contains(&unit) {
                            break;
                        }
                        fraction = fraction * 10 + (unit - 0x30) as i64;
                        pos += 1;
                        count += 1;
                    }
                    while count < 3 {
                        fraction *= 10;
                        count += 1;
                    }
                    ms = fraction;
                }
            }
        }
    }
    let mut offset_minutes = 0i64;
    let mut has_offset = false;
    match text.get(pos) {
        Some(u) if *u == b'Z' as u16 => {
            has_offset = true;
            pos += 1;
        }
        Some(u) if *u == b'+' as u16 || *u == b'-' as u16 => {
            has_offset = true;
            let sign = if *u == b'-' as u16 { -1 } else { 1 };
            pos += 1;
            let (oh, next) = parse_digits(text, pos, 2)?;
            pos = next;
            let mut om = 0;
            if text.get(pos) == Some(&(b':' as u16)) {
                pos += 1;
                (om, pos) = parse_digits(text, pos, 2)?;
            }
            if oh > 23 || om > 59 {
                return None;
            }
            offset_minutes = sign * (oh * 60 + om);
        }
        _ => {}
    }
    if pos != text.len() {
        return None;
    }
    let day = make_day(year as f64, (month - 1) as f64, date as f64);
    let time = make_time(hours as f64, minutes as f64, seconds as f64, ms as f64);
    let utc = make_date(day, time) - offset_minutes as f64 * MS_PER_MINUTE;
    // Date-only forms are UTC; date-time forms without an offset are local.
    if has_time && !has_offset {
        Some(utc_time(utc))
    } else {
        Some(utc)
    }
}

/// Common non-ISO textual forms: "Mon DD YYYY", "DD Mon YYYY", with an
/// optional "HH:MM[:SS]" time and AM/PM.
fn parse_fallback(text: &[u16]) -> Option<f64> {
    let parts: Vec<&[u16]> = text
        .split(|u| *u == b' ' as u16 || *u == 0x2C)
        .filter(|part| !part.is_empty())
        .collect();
    let month_name = |part: &[u16]| -> Option<i64> {
        let name: String = part.iter().map(|u| char::from(*u as u8)).collect();
        MONTH_NAMES
            .iter()
            .position(|m| m.eq_ignore_ascii_case(&name))
            .map(|i| i as i64)
    };
    // "Mon DD YYYY" or "DD Mon YYYY": find the year (4 digits).
    let mut year_index = None;
    for (i, part) in parts.iter().enumerate() {
        if part.len() == 4
            && part.iter().all(|u| (0x30..=0x39).contains(u))
            && let Some(y) = parse_digits(part, 0, 4)
            && y.0 >= 1000
        {
            year_index = Some(i);
            break;
        }
    }
    let yi = year_index?;
    let mut year = parse_digits(parts[yi], 0, 4)?.0;
    let (month, date) = if yi >= 2 {
        if let Some(m) = month_name(parts[yi - 2]) {
            // Mon DD YYYY
            (m, parse_digits(parts[yi - 1], 0, parts[yi - 1].len())?.0)
        } else {
            // DD Mon YYYY
            (
                month_name(parts[yi - 1])?,
                parse_digits(parts[yi - 2], 0, parts[yi - 2].len())?.0,
            )
        }
    } else {
        return None;
    };
    if year < 100 {
        year += 1900;
    }
    let (mut hours, mut minutes, mut seconds) = (0i64, 0i64, 0i64);
    if let Some(time_part) = parts.get(yi + 1) {
        let mut pm = false;
        let mut has_ampm = false;
        let cleaned: Vec<u16> = time_part
            .iter()
            .copied()
            .filter(|u| (0x30..=0x39).contains(u) || *u == b':' as u16)
            .collect();
        // AM/PM suffix check on the raw part.
        let tail: String = time_part
            .iter()
            .filter(|&&u| !(0x30..=0x39).contains(&u) && u != b':' as u16)
            .map(|&u| char::from(u as u8))
            .collect();
        if tail.eq_ignore_ascii_case("PM") || tail.eq_ignore_ascii_case("AM") {
            pm = tail.eq_ignore_ascii_case("PM");
            has_ampm = true;
        }
        let colon_parts: Vec<&[u16]> = cleaned.split(|u| *u == b':' as u16).collect();
        if let Some(first) = colon_parts.first()
            && let Some((h, _)) = parse_digits(first, 0, first.len())
        {
            hours = h;
            if let Some(min_part) = colon_parts.get(1) {
                (minutes, _) = parse_digits(min_part, 0, min_part.len())?;
                if let Some(sec_part) = colon_parts.get(2) {
                    (seconds, _) = parse_digits(sec_part, 0, sec_part.len())?;
                }
            }
            if pm && hours != 12 {
                hours += 12;
            }
            if has_ampm && !pm && hours == 12 {
                hours = 0;
            }
        }
    }
    let day = make_day(year as f64, month as f64, date as f64);
    let time = make_time(hours as f64, minutes as f64, seconds as f64, 0.0);
    Some(utc_time(make_date(day, time)))
}

/// spec 21.4.2.3 Date.parse: parse the already-coerced string value.
fn date_parse(text: &JsString) -> f64 {
    let units = text.as_slice();
    let mut start = 0;
    let mut end = units.len();
    while start < end && (units[start] as u8).is_ascii_whitespace() {
        start += 1;
    }
    while end > start && (units[end - 1] as u8).is_ascii_whitespace() {
        end -= 1;
    }
    let body = &units[start..end];
    // Date.parse returns TimeClip of the parsed value (spec 21.4.2.3 step 5).
    time_clip(
        parse_iso(body)
            .or_else(|| parse_fallback(body))
            .unwrap_or(f64::NAN),
    )
}

// ---------------------------------------------------------------------------
// Formatting.
// ---------------------------------------------------------------------------

/// "Thu Jan 01 1970 00:00:00 GMT+0000" — local-time toString (spec 21.4.4.43).
fn format_time(t: f64) -> String {
    if t.is_nan() {
        return "Invalid Date".into();
    }
    let local = local_time(t);
    format!(
        "{} {} {:02} {} {:02}:{:02}:{:02} GMT+0000",
        DAY_NAMES[week_day(local) as usize],
        MONTH_NAMES[month_from_time(local) as usize],
        date_from_time(local) as i64,
        format_year(year_from_time(local)),
        hour_from_time(local),
        min_from_time(local),
        sec_from_time(local)
    )
}

/// "Thu, 01 Jan 1970 00:00:00 GMT" — UTC (spec 21.4.4.45 toUTCString).
fn format_utc_string(t: f64) -> String {
    if t.is_nan() {
        return "Invalid Date".into();
    }
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        DAY_NAMES[week_day(t) as usize],
        date_from_time(t) as i64,
        MONTH_NAMES[month_from_time(t) as usize],
        format_year(year_from_time(t)),
        hour_from_time(t),
        min_from_time(t),
        sec_from_time(t)
    )
}

/// "Thu Jan 01 1970" — the date portion of toString.
fn format_date_string(t: f64) -> String {
    if t.is_nan() {
        return "Invalid Date".into();
    }
    let local = local_time(t);
    format!(
        "{} {} {:02} {}",
        DAY_NAMES[week_day(local) as usize],
        MONTH_NAMES[month_from_time(local) as usize],
        date_from_time(local) as i64,
        format_year(year_from_time(local))
    )
}

/// "00:00:00 GMT+0000" — the time portion of toString.
fn format_time_string(t: f64) -> String {
    if t.is_nan() {
        return "Invalid Date".into();
    }
    let local = local_time(t);
    format!(
        "{:02}:{:02}:{:02} GMT+0000",
        hour_from_time(local),
        min_from_time(local),
        sec_from_time(local)
    )
}

/// spec 21.4.4.36 Date.prototype.toISOString.
fn to_iso_string(t: f64) -> Result<String, JsError> {
    if t.is_nan() {
        return Err(JsError::new(
            ErrorKind::RangeError,
            "Invalid time value".into(),
        ));
    }
    Ok(format!(
        "{}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        format_year(year_from_time(t)),
        month_from_time(t) + 1,
        date_from_time(t) as i64,
        hour_from_time(t),
        min_from_time(t),
        sec_from_time(t),
        ms_from_time(t)
    ))
}

// ---------------------------------------------------------------------------
// Prototype handlers.
// ---------------------------------------------------------------------------

fn get_component(
    agent: &Agent,
    this: &Value,
    local: bool,
    getter: fn(f64) -> f64,
) -> Result<Value, JsError> {
    let t = this_date_value(agent, this)?;
    if t.is_nan() {
        return Ok(Value::Number(f64::NAN));
    }
    let t = if local { local_time(t) } else { t };
    Ok(Value::Number(getter(t)))
}

/// The shared setter machinery (spec 21.4.4.21-21.4.4.32): overwrite the
/// present components (absent arguments keep the current component, per the
/// spec's "If x is not present, let x be ComponentFromTime(t)"), and store
/// the new time value back on the receiver.
fn set_components(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    local: bool,
    present: &[bool; 7],
    nan_is_zero: bool,
) -> Result<Value, JsError> {
    let ValueKind::Object(obj) = this.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Date.prototype method called on an incompatible receiver".into(),
        ));
    };
    let t = match agent.date_data.get(&obj.id()) {
        Some(t) => *t,
        None => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Date.prototype method called on an incompatible receiver".into(),
            ));
        }
    };
    // The provided arguments are ToNumber'd in order before the stored value
    // decides anything (spec 21.4.4.x steps 2+); setFullYear converts an
    // invalid date to +0 instead of failing. The namesake component (the
    // first set bit) is always ToNumber'd — an absent argument is undefined
    // and yields NaN — while later components keep the base when absent.
    let first = present.iter().position(|p| *p).unwrap_or(0);
    let mut coerced = [0.0f64; 7];
    for i in 0..7 {
        if present[i] && (i == first || args.get(i - first).is_some()) {
            coerced[i] = crate::context::to_number(
                agent,
                &args.get(i - first).cloned().unwrap_or(Value::Undefined),
            )?;
        }
    }
    let t = if nan_is_zero && t.is_nan() { 0.0 } else { t };
    if t.is_nan() {
        return Ok(Value::Number(f64::NAN));
    }
    let base = if local { local_time(t) } else { t };
    let mut values = [
        year_from_time(base) as f64,
        month_from_time(base) as f64,
        date_from_time(base),
        hour_from_time(base) as f64,
        min_from_time(base) as f64,
        sec_from_time(base) as f64,
        ms_from_time(base) as f64,
    ];
    for i in 0..7 {
        if present[i] && (i == first || args.get(i - first).is_some()) {
            values[i] = coerced[i];
        }
    }
    let day = make_day(values[0], values[1], values[2]);
    let time = make_time(values[3], values[4], values[5], values[6]);
    let composed = if local {
        utc_time(make_date(day, time))
    } else {
        make_date(day, time)
    };
    let clipped = time_clip(composed);
    agent.date_data.insert(obj.id(), clipped);
    Ok(Value::Number(clipped))
}

/// spec 21.4.4.3 / 21.4.4.34 setDate/setUTCDate: the date component is the
/// (unconditionally ToNumber'd) first argument; the time of day is kept.
fn set_date(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
    local: bool,
) -> Result<Value, JsError> {
    let ValueKind::Object(obj) = this.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Date.prototype method called on an incompatible receiver".into(),
        ));
    };
    let t = match agent.date_data.get(&obj.id()) {
        Some(t) => *t,
        None => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Date.prototype method called on an incompatible receiver".into(),
            ));
        }
    };
    // The argument is ToNumber'd before the NaN check (spec 21.4.4.3 step
    // 4 comes before step 5), but the slot is left untouched on NaN.
    let date = match args.first() {
        Some(v) => crate::context::to_number(agent, v)?,
        None => f64::NAN,
    };
    if t.is_nan() {
        return Ok(Value::Number(f64::NAN));
    }
    let base = if local { local_time(t) } else { t };
    let day = make_day(
        year_from_time(base) as f64,
        month_from_time(base) as f64,
        date,
    );
    let composed = if local {
        utc_time(make_date(day, time_within_day(base)))
    } else {
        make_date(day, time_within_day(base))
    };
    let clipped = time_clip(composed);
    agent.date_data.insert(obj.id(), clipped);
    Ok(Value::Number(clipped))
}

// ---------------------------------------------------------------------------
// Install.
// ---------------------------------------------------------------------------

/// Install the Date intrinsics and the global `Date` binding (spec 21.4.2)
/// during SetDefaultGlobalBindings.
pub fn install(realm: &Handle<Realm>) -> Result<(), JsError> {
    let object_proto = realm
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let date_proto = JsObject::ordinary_object_create(object_proto);
    let date_proto_value = Value::Object(date_proto.clone());

    let date_ctor = Function::create_builtin(
        Some(JsString::from_utf8("Date")),
        7,
        placeholder("Date"),
        Some(Box::new(placeholder("Date"))),
        None,
    )?;
    let date_ctor_value = Value::Function(date_ctor.clone());

    realm.intrinsics.define(DATE, date_ctor_value.clone());
    realm
        .intrinsics
        .define(DATE_PROTO, date_proto_value.clone());

    date_ctor.define_property(
        &JsString::from_utf8("prototype"),
        &PropertyDescriptor {
            value: Some(date_proto_value.clone()),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(false),
        },
    )?;
    date_proto.define_property(
        &JsString::from_utf8("constructor"),
        &PropertyDescriptor {
            value: Some(date_ctor_value.clone()),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    // Date.parse / Date.UTC / Date.now.
    for (name, key, length) in [
        ("parse", "%Date.parse%", 1),
        ("UTC", "%Date.UTC%", 7),
        ("now", "%Date.now%", 0),
    ] {
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            placeholder(name),
            None,
            None,
        )?;
        realm.intrinsics.define(key, Value::Function(func.clone()));
        date_ctor.define_property(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: Some(Value::Function(func.clone())),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
    }

    // Prototype methods: every member dispatches by intrinsic identity.
    let methods: [(&str, &str, u64); 46] = [
        ("getDate", "%Date.prototype.getDate%", 0),
        ("getDay", "%Date.prototype.getDay%", 0),
        ("getFullYear", "%Date.prototype.getFullYear%", 0),
        ("getHours", "%Date.prototype.getHours%", 0),
        ("getMilliseconds", "%Date.prototype.getMilliseconds%", 0),
        ("getMinutes", "%Date.prototype.getMinutes%", 0),
        ("getMonth", "%Date.prototype.getMonth%", 0),
        ("getSeconds", "%Date.prototype.getSeconds%", 0),
        ("getTime", "%Date.prototype.getTime%", 0),
        ("getTimezoneOffset", "%Date.prototype.getTimezoneOffset%", 0),
        ("getUTCDate", "%Date.prototype.getUTCDate%", 0),
        ("getUTCDay", "%Date.prototype.getUTCDay%", 0),
        ("getUTCFullYear", "%Date.prototype.getUTCFullYear%", 0),
        ("getUTCHours", "%Date.prototype.getUTCHours%", 0),
        (
            "getUTCMilliseconds",
            "%Date.prototype.getUTCMilliseconds%",
            0,
        ),
        ("getUTCMinutes", "%Date.prototype.getUTCMinutes%", 0),
        ("getUTCMonth", "%Date.prototype.getUTCMonth%", 0),
        ("getUTCSeconds", "%Date.prototype.getUTCSeconds%", 0),
        ("getYear", "%Date.prototype.getYear%", 0),
        ("setDate", "%Date.prototype.setDate%", 1),
        ("setFullYear", "%Date.prototype.setFullYear%", 3),
        ("setHours", "%Date.prototype.setHours%", 4),
        ("setMilliseconds", "%Date.prototype.setMilliseconds%", 1),
        ("setMinutes", "%Date.prototype.setMinutes%", 3),
        ("setMonth", "%Date.prototype.setMonth%", 2),
        ("setSeconds", "%Date.prototype.setSeconds%", 2),
        ("setTime", "%Date.prototype.setTime%", 1),
        ("setUTCDate", "%Date.prototype.setUTCDate%", 1),
        ("setUTCFullYear", "%Date.prototype.setUTCFullYear%", 3),
        ("setUTCHours", "%Date.prototype.setUTCHours%", 4),
        (
            "setUTCMilliseconds",
            "%Date.prototype.setUTCMilliseconds%",
            1,
        ),
        ("setUTCMinutes", "%Date.prototype.setUTCMinutes%", 3),
        ("setUTCMonth", "%Date.prototype.setUTCMonth%", 2),
        ("setUTCSeconds", "%Date.prototype.setUTCSeconds%", 2),
        ("setYear", "%Date.prototype.setYear%", 1),
        ("toDateString", "%Date.prototype.toDateString%", 0),
        ("toISOString", "%Date.prototype.toISOString%", 0),
        ("toJSON", "%Date.prototype.toJSON%", 1),
        (
            "toLocaleDateString",
            "%Date.prototype.toLocaleDateString%",
            0,
        ),
        ("toLocaleString", "%Date.prototype.toLocaleString%", 0),
        (
            "toLocaleTimeString",
            "%Date.prototype.toLocaleTimeString%",
            0,
        ),
        ("toString", "%Date.prototype.toString%", 0),
        ("toTemporalInstant", "%Date.prototype.toTemporalInstant%", 0),
        ("toTimeString", "%Date.prototype.toTimeString%", 0),
        ("toUTCString", "%Date.prototype.toUTCString%", 0),
        ("valueOf", "%Date.prototype.valueOf%", 0),
    ];
    for (name, key, length) in methods {
        if key.is_empty() {
            continue;
        }
        let func = Function::create_builtin(
            Some(JsString::from_utf8(name)),
            length,
            placeholder(name),
            None,
            None,
        )?;
        realm.intrinsics.define(key, Value::Function(func.clone()));
        date_proto.define_property(
            &JsString::from_utf8(name),
            &PropertyDescriptor {
                value: Some(Value::Function(func.clone())),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
    }
    // Annex B.2.6: Date.prototype.toGMTString is the *same function object*
    // as toUTCString (value.js checks the identity; the dispatch above routes
    // calls through the toUTCString intrinsic).
    if let Some(utc) = realm.intrinsics.get("%Date.prototype.toUTCString%") {
        date_proto.define_property(
            &JsString::from_utf8("toGMTString"),
            &PropertyDescriptor {
                value: Some(utc),
                writable: Some(true),
                get: None,
                set: None,
                enumerable: Some(false),
                configurable: Some(true),
            },
        )?;
    }

    // The Date tag comes from the [[DateValue]] slot in
    // Object.prototype.toString; there is no @@toStringTag on the prototype.

    // Date.prototype[@@toPrimitive] (writable: false per spec 21.4.3.5).
    let to_primitive = Function::create_builtin(
        Some(JsString::from_utf8("[Symbol.toPrimitive]")),
        1,
        placeholder("Date.prototype[Symbol.toPrimitive]"),
        None,
        None,
    )?;
    realm
        .intrinsics
        .define(DATE_TO_PRIMITIVE, Value::Function(to_primitive.clone()));
    date_proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toPrimitive").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::Function(to_primitive)),
            writable: Some(false),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;

    realm.global_object.define_property_or_throw(
        &JsString::from_utf8("Date"),
        &PropertyDescriptor {
            value: Some(date_ctor_value),
            writable: Some(true),
            get: None,
            set: None,
            enumerable: Some(false),
            configurable: Some(true),
        },
    )?;
    Ok(())
}

/// The prototype method bodies, dispatched by intrinsic identity from
/// `runtime::function::call`.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(DATE).as_ref() == Some(callee) {
        return Some(date_call());
    }
    if intrinsics.get("%Date.parse%").as_ref() == Some(callee) {
        let value = args.first().cloned().unwrap_or(Value::Undefined);
        // spec 21.4.2.3 step 1: ToString is abrupt-completing; the parse
        // failure itself yields NaN.
        return Some(
            crate::context::to_string(agent, &value).map(|text| Value::Number(date_parse(&text))),
        );
    }
    if intrinsics.get("%Date.UTC%").as_ref() == Some(callee) {
        return Some(multi_arg_time(agent, args).map(Value::Number));
    }
    if intrinsics.get("%Date.now%").as_ref() == Some(callee) {
        return Some(date_now());
    }
    // Component getters: (intrinsic key, getter, is-local).
    type GetterSpec = (&'static str, fn(f64) -> f64, bool);
    let getters: [GetterSpec; 19] = [
        ("%Date.prototype.getDate%", date_from_time, true),
        ("%Date.prototype.getDay%", week_day_f, true),
        ("%Date.prototype.getFullYear%", year_f, true),
        ("%Date.prototype.getHours%", hour_f, true),
        ("%Date.prototype.getMilliseconds%", ms_f, true),
        ("%Date.prototype.getMinutes%", min_f, true),
        ("%Date.prototype.getMonth%", month_f, true),
        ("%Date.prototype.getSeconds%", sec_f, true),
        ("%Date.prototype.getYear%", get_year_f, true),
        ("%Date.prototype.getUTCDate%", date_from_time, false),
        ("%Date.prototype.getUTCDay%", week_day_f, false),
        ("%Date.prototype.getUTCFullYear%", year_f, false),
        ("%Date.prototype.getUTCHours%", hour_f, false),
        ("%Date.prototype.getUTCMilliseconds%", ms_f, false),
        ("%Date.prototype.getUTCMinutes%", min_f, false),
        ("%Date.prototype.getUTCMonth%", month_f, false),
        ("%Date.prototype.getUTCSeconds%", sec_f, false),
        ("%Date.prototype.getTime%", year_f, false), // replaced below
        ("%Date.prototype.valueOf%", year_f, false), // replaced below
    ];
    for (key, getter, local) in getters {
        if intrinsics.get(key).as_ref() == Some(callee) {
            if matches!(key, "%Date.prototype.getTime%" | "%Date.prototype.valueOf%") {
                return Some(this_date_value(agent, this).map(Value::Number));
            }
            return Some(get_component(agent, this, local, getter));
        }
    }
    if intrinsics
        .get("%Date.prototype.getTimezoneOffset%")
        .as_ref()
        == Some(callee)
    {
        return match this_date_value(agent, this) {
            Ok(t) if t.is_nan() => Some(Ok(Value::Number(f64::NAN))),
            Ok(_) => Some(Ok(Value::Number(0.0))),
            Err(e) => Some(Err(e)),
        };
    }
    // Component setters: (intrinsic key, is-local, present mask).
    let setters: [(&str, bool, &[bool; 7]); 13] = [
        (
            "%Date.prototype.setFullYear%",
            true,
            &[true, true, true, false, false, false, false],
        ),
        (
            "%Date.prototype.setHours%",
            true,
            &[false, false, false, true, true, true, true],
        ),
        (
            "%Date.prototype.setMilliseconds%",
            true,
            &[false, false, false, false, false, false, true],
        ),
        (
            "%Date.prototype.setMinutes%",
            true,
            &[false, false, false, false, true, true, true],
        ),
        (
            "%Date.prototype.setMonth%",
            true,
            &[false, true, true, false, false, false, false],
        ),
        (
            "%Date.prototype.setSeconds%",
            true,
            &[false, false, false, false, false, true, true],
        ),
        (
            "%Date.prototype.setUTCFullYear%",
            false,
            &[true, true, true, false, false, false, false],
        ),
        (
            "%Date.prototype.setUTCHours%",
            false,
            &[false, false, false, true, true, true, true],
        ),
        (
            "%Date.prototype.setUTCMilliseconds%",
            false,
            &[false, false, false, false, false, false, true],
        ),
        (
            "%Date.prototype.setUTCMinutes%",
            false,
            &[false, false, false, false, true, true, true],
        ),
        (
            "%Date.prototype.setUTCMonth%",
            false,
            &[false, true, true, false, false, false, false],
        ),
        (
            "%Date.prototype.setUTCSeconds%",
            false,
            &[false, false, false, false, false, true, true],
        ),
        (
            "%Date.prototype.setYear%",
            true,
            &[true, false, false, false, false, false, false],
        ),
    ];
    for (key, local, present) in setters {
        if intrinsics.get(key).as_ref() == Some(callee) {
            if key == "%Date.prototype.setYear%" {
                return Some(set_year(agent, this, args));
            }
            let nan_is_zero =
                key == "%Date.prototype.setFullYear%" || key == "%Date.prototype.setUTCFullYear%";
            return Some(set_components(
                agent,
                this,
                args,
                local,
                present,
                nan_is_zero,
            ));
        }
    }
    if intrinsics.get("%Date.prototype.setDate%").as_ref() == Some(callee) {
        return Some(set_date(agent, this, args, true));
    }
    if intrinsics.get("%Date.prototype.setUTCDate%").as_ref() == Some(callee) {
        return Some(set_date(agent, this, args, false));
    }
    if intrinsics.get("%Date.prototype.setTime%").as_ref() == Some(callee) {
        return Some((|| {
            // thisTimeValue first: a non-Date receiver throws before the
            // argument is coerced (spec 21.4.4.44 step 1).
            let ValueKind::Object(obj) = this.kind() else {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "Date.prototype method called on an incompatible receiver".into(),
                ));
            };
            if !agent.date_data.contains_key(&obj.id()) {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "Date.prototype method called on an incompatible receiver".into(),
                ));
            }
            let value = match args.first() {
                Some(v) => crate::context::to_number(agent, v)?,
                None => f64::NAN,
            };
            let clipped = time_clip(value);
            agent.date_data.insert(obj.id(), clipped);
            Ok(Value::Number(clipped))
        })());
    }
    // String forms.
    if intrinsics.get("%Date.prototype.toISOString%").as_ref() == Some(callee) {
        return Some(
            this_date_value(agent, this)
                .and_then(to_iso_string)
                .map(|s| Value::String(Handle::new(JsString::from_utf8(&s)))),
        );
    }
    if intrinsics.get("%Date.prototype.toLocaleString%").as_ref() == Some(callee) {
        return Some(match this_date_value(agent, this) {
            Ok(t) => crate::builtins::intl::date_time_format::to_locale_string(
                agent,
                &args.first().cloned().unwrap_or(Value::Undefined),
                &args.get(1).cloned().unwrap_or(Value::Undefined),
                t,
                "any",
                "all",
            )
            .map(|s| Value::String(Handle::new(JsString::from_utf8(&s)))),
            Err(e) => Err(e),
        });
    }
    if intrinsics
        .get("%Date.prototype.toLocaleDateString%")
        .as_ref()
        == Some(callee)
    {
        return Some(match this_date_value(agent, this) {
            Ok(t) => crate::builtins::intl::date_time_format::to_locale_string(
                agent,
                &args.first().cloned().unwrap_or(Value::Undefined),
                &args.get(1).cloned().unwrap_or(Value::Undefined),
                t,
                "date",
                "date",
            )
            .map(|s| Value::String(Handle::new(JsString::from_utf8(&s)))),
            Err(e) => Err(e),
        });
    }
    if intrinsics
        .get("%Date.prototype.toLocaleTimeString%")
        .as_ref()
        == Some(callee)
    {
        return Some(match this_date_value(agent, this) {
            Ok(t) => crate::builtins::intl::date_time_format::to_locale_string(
                agent,
                &args.first().cloned().unwrap_or(Value::Undefined),
                &args.get(1).cloned().unwrap_or(Value::Undefined),
                t,
                "time",
                "time",
            )
            .map(|s| Value::String(Handle::new(JsString::from_utf8(&s)))),
            Err(e) => Err(e),
        });
    }
    if intrinsics.get("%Date.prototype.toString%").as_ref() == Some(callee) {
        return Some(match this_date_value(agent, this) {
            Ok(t) => Ok(Value::String(Handle::new(JsString::from_utf8(
                &format_time(t),
            )))),
            Err(e) => Err(e),
        });
    }
    if intrinsics.get("%Date.prototype.toDateString%").as_ref() == Some(callee) {
        return Some(match this_date_value(agent, this) {
            Ok(t) => Ok(Value::String(Handle::new(JsString::from_utf8(
                &format_date_string(t),
            )))),
            Err(e) => Err(e),
        });
    }
    if intrinsics.get("%Date.prototype.toTimeString%").as_ref() == Some(callee) {
        return Some(match this_date_value(agent, this) {
            Ok(t) => Ok(Value::String(Handle::new(JsString::from_utf8(
                &format_time_string(t),
            )))),
            Err(e) => Err(e),
        });
    }
    if intrinsics.get("%Date.prototype.toUTCString%").as_ref() == Some(callee) {
        return Some(match this_date_value(agent, this) {
            Ok(t) => Ok(Value::String(Handle::new(JsString::from_utf8(
                &format_utc_string(t),
            )))),
            Err(e) => Err(e),
        });
    }
    if intrinsics
        .get("%Date.prototype.toTemporalInstant%")
        .as_ref()
        == Some(callee)
    {
        // spec 21.4.3.28 (proposal-temporal): RequireInternalSlot
        // ([[DateValue]]), then ns = NumberToBigInt(t) × 10⁶ — NaN is not an
        // integral Number, so an invalid date throws RangeError — then
        // CreateTemporalInstant.
        return Some((|| {
            let t = this_date_value(agent, this)?;
            if t.is_nan() {
                return Err(JsError::new(ErrorKind::RangeError, "invalid date".into()));
            }
            let ns = (t as i128) * 1_000_000;
            create_instant(agent, ns, &Value::Undefined)
        })());
    }
    if intrinsics.get(DATE_TO_PRIMITIVE).as_ref() == Some(callee) {
        return Some(date_to_primitive(agent, this, args));
    }
    if intrinsics.get("%Date.prototype.toJSON%").as_ref() == Some(callee) {
        return Some((|| {
            // toJSON is generic (spec 21.4.4.36): ToObject the receiver,
            // ToPrimitive(Number) for the finite check, then Invoke
            // "toISOString".
            let object = crate::context::to_object(agent, this)?;
            let tv = crate::context::to_primitive(
                agent,
                &object,
                crux::convert::ToPrimitiveHint::Number,
            )?;
            if let ValueKind::Number(n) = tv.kind()
                && !n.is_finite()
            {
                return Ok(Value::Null);
            }
            let to_iso = crate::context::get_property_key(
                agent,
                &object,
                &PropertyKey::from_utf8("toISOString"),
                object.clone(),
            )?;
            if !is_callable(&to_iso) {
                return Err(JsError::new(
                    ErrorKind::TypeError,
                    "toISOString is not a function".into(),
                ));
            }
            crate::function::call(agent, &to_iso, object, &[])
        })());
    }
    None
}

fn week_day_f(t: f64) -> f64 {
    week_day(t) as f64
}

fn year_f(t: f64) -> f64 {
    year_from_time(t) as f64
}

fn month_f(t: f64) -> f64 {
    month_from_time(t) as f64
}

fn hour_f(t: f64) -> f64 {
    hour_from_time(t) as f64
}

fn min_f(t: f64) -> f64 {
    min_from_time(t) as f64
}

fn sec_f(t: f64) -> f64 {
    sec_from_time(t) as f64
}

fn ms_f(t: f64) -> f64 {
    ms_from_time(t) as f64
}

fn get_year_f(t: f64) -> f64 {
    (year_from_time(t) - 1900) as f64
}

/// The legacy setYear (Annex B.4.5).
fn set_year(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let ValueKind::Object(obj) = this.kind() else {
        return Err(JsError::new(
            ErrorKind::TypeError,
            "Date.prototype method called on an incompatible receiver".into(),
        ));
    };
    let t = match agent.date_data.get(&obj.id()) {
        Some(t) => *t,
        None => {
            return Err(JsError::new(
                ErrorKind::TypeError,
                "Date.prototype method called on an incompatible receiver".into(),
            ));
        }
    };
    // spec B.2.4.2: the stored time value is read *before* the argument
    // coerces (a valueOf that mutates the date must not affect the read), a
    // NaN stored value becomes +0, and the year is ToInteger'd before the
    // 0-99 → 1900+ offset (setYear(50.999999) is 1950, setYear(-0.9999999)
    // is 1900).
    let y = match args.first() {
        Some(v) => crate::context::to_number(agent, v)?,
        None => f64::NAN,
    };
    if y.is_nan() {
        agent.date_data.insert(obj.id(), f64::NAN);
        return Ok(Value::Number(f64::NAN));
    }
    let y = to_integer_or_infinity(y);
    let y = if (0.0..=99.0).contains(&y) {
        y + 1900.0
    } else {
        y
    };
    let t = if t.is_nan() { 0.0 } else { t };
    let local = local_time(t);
    let day = make_day(y, month_from_time(local) as f64, date_from_time(local));
    let composed = utc_time(make_date(day, time_within_day(local)));
    let clipped = time_clip(composed);
    agent.date_data.insert(obj.id(), clipped);
    Ok(Value::Number(clipped))
}

pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    if realm.intrinsics.get(DATE).as_ref() == Some(callee) {
        return Some(date_construct(agent, args, new_target));
    }
    None
}

fn date_now() -> Result<Value, JsError> {
    Ok(Value::Number(now_ms()))
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
        match run(source).unwrap().kind() {
            ValueKind::Number(n) => n,
            other => panic!("expected a number, got {other:?}"),
        }
    }

    fn text(source: &str) -> String {
        match run(source).unwrap().kind() {
            ValueKind::String(s) => s.to_string_lossy(),
            other => panic!("expected a string, got {other:?}"),
        }
    }

    #[test]
    fn epoch_and_components() {
        assert_eq!(number("new Date(0).getTime()"), 0.0);
        assert_eq!(number("new Date(0).valueOf()"), 0.0);
        assert_eq!(number("new Date(0).getUTCFullYear()"), 1970.0);
        assert_eq!(number("new Date(0).getUTCMonth()"), 0.0);
        assert_eq!(number("new Date(0).getUTCDate()"), 1.0);
        assert_eq!(number("new Date(0).getUTCDay()"), 4.0); // Thursday
        assert_eq!(number("new Date(0).getUTCHours()"), 0.0);
        assert_eq!(number("new Date(0).getUTCMinutes()"), 0.0);
        assert_eq!(number("new Date(0).getUTCSeconds()"), 0.0);
        assert_eq!(number("new Date(0).getUTCMilliseconds()"), 0.0);
        assert_eq!(number("new Date(0).getTimezoneOffset()"), 0.0);
    }

    #[test]
    fn constructor_forms() {
        assert_eq!(number("new Date(2024, 0, 15).getUTCFullYear()"), 2024.0);
        assert_eq!(number("new Date(2024, 0, 15).getUTCMonth()"), 0.0);
        assert_eq!(number("new Date(2024, 0, 15).getUTCDate()"), 15.0);
        // 0-99 years are 1900 + year.
        assert_eq!(number("new Date(99, 0).getUTCFullYear()"), 1999.0);
        assert!(number("new Date('garbage').getTime()").is_nan());
        assert_eq!(run("typeof Date(0)").unwrap().to_string(), "string");
        assert_eq!(number("Date.UTC(2024, 0, 15)"), 1705276800000.0);
        assert_eq!(number("Date.UTC(1970, 0, 1)"), 0.0);
        assert!(matches!(
            run("Date.now()"),
            Ok(v) if matches!(v.kind(), ValueKind::Number(n) if n > 0.0)
        ));
    }

    #[test]
    fn parse_iso_format() {
        assert_eq!(number("Date.parse('1970-01-01T00:00:00.000Z')"), 0.0);
        assert_eq!(number("Date.parse('1970-01-01')"), 0.0);
        assert_eq!(number("Date.parse('1970-01-01T00:00:00Z')"), 0.0);
        assert_eq!(
            number("Date.parse('2024-01-15T12:30:00.500Z')"),
            1705321800500.0
        );
        assert_eq!(
            number("Date.parse('2024-01-15T12:30:00+02:00')"),
            1705314600000.0
        );
        assert_eq!(
            number("Date.parse('2024-01-15T12:30:00-05:30')"),
            1705341600000.0
        );
        assert!(number("Date.parse('2024-13-01')").is_nan());
        assert!(number("Date.parse('not a date')").is_nan());
        assert_eq!(
            number("Date.parse('Jan 15 2024')"),
            number("Date.parse('2024-01-15')")
        );
        assert_eq!(
            number("Date.parse('15 Jan 2024')"),
            number("Date.parse('2024-01-15')")
        );
        assert_eq!(
            number("Date.parse('Jan 15 2024 12:30:00')"),
            number("Date.parse('2024-01-15T12:30:00')")
        );
    }

    #[test]
    fn setters() {
        assert_eq!(number("new Date(0).setUTCFullYear(2000)"), 946684800000.0);
        assert_eq!(number("new Date(0).setUTCHours(5)"), 18000000.0);
        assert_eq!(number("new Date(0).setUTCMonth(5)"), 13046400000.0); // June 1 1970
        assert_eq!(number("new Date(0).setTime(12345)"), 12345.0);
        assert_eq!(number("new Date(0).setDate(15)"), 1209600000.0);
        assert_eq!(number("new Date(0).setYear(2020)"), 1577836800000.0);
    }

    #[test]
    fn string_forms() {
        assert_eq!(
            text("new Date(0).toISOString()"),
            "1970-01-01T00:00:00.000Z"
        );
        assert_eq!(
            text("new Date(1705276800000).toISOString()"),
            "2024-01-15T00:00:00.000Z"
        );
        assert_eq!(
            text("new Date(0).toUTCString()"),
            "Thu, 01 Jan 1970 00:00:00 GMT"
        );
        assert_eq!(
            text("new Date(0).toString()"),
            "Thu Jan 01 1970 00:00:00 GMT+0000"
        );
        assert_eq!(text("new Date(0).toDateString()"), "Thu Jan 01 1970");
        assert_eq!(text("new Date(0).toTimeString()"), "00:00:00 GMT+0000");
        assert!(matches!(
            run("new Date(NaN).toISOString()"),
            Err(e) if e.kind == ErrorKind::RangeError
        ));
        assert_eq!(text("new Date(NaN).toString()"), "Invalid Date");
    }

    #[test]
    fn leap_years_and_edges() {
        // 2000 is a leap year (divisible by 400); 1900 is not, so Feb 29 rolls
        // into March.
        assert_eq!(number("new Date(2000, 1, 29).getUTCMonth()"), 1.0);
        assert_eq!(number("new Date(1900, 1, 29).getUTCMonth()"), 2.0);
        assert_eq!(number("new Date(2024, 1, 29).getUTCDate()"), 29.0);
        assert_eq!(
            number("new Date(2024, 1, 29).getTime()"),
            number("new Date(2024, 2, 1).getTime() - 86400000")
        );
        // Max/min time values.
        assert!(number("new Date(8.64e15).getTime()").is_finite());
        assert!(number("new Date(8.64e15 + 1).getTime()").is_nan());
        assert!(number("new Date(-8.64e15).getTime()").is_finite());
    }

    #[test]
    fn to_json() {
        assert_eq!(text("new Date(0).toJSON()"), "1970-01-01T00:00:00.000Z");
        assert!(matches!(
            run("new Date(NaN).toJSON() === null"),
            Ok(v) if matches!(v.kind(), ValueKind::Boolean(true))
        ));
    }

    #[test]
    fn invalid_dates() {
        assert!(number("new Date(NaN).getTime()").is_nan());
        assert!(number("Date.parse('')").is_nan());
    }

    #[test]
    fn month_rollover_and_utc_edges() {
        // Feb 29 in a non-leap year rolls to March 1 of the same year.
        assert_eq!(number("new Date(2021, 1, 29).getMonth()"), 2.0);
        assert_eq!(number("new Date(2021, 1, 29).getDate()"), 1.0);
        assert_eq!(number("new Date(2021, 1, 29).getFullYear()"), 2021.0);
        // Months are 0-indexed; month 12 rolls to January of the next year.
        assert_eq!(number("new Date(2020, 0, 1).getMonth()"), 0.0);
        assert_eq!(number("new Date(2020, 12, 1).getMonth()"), 0.0);
        assert_eq!(number("new Date(2020, 12, 1).getFullYear()"), 2021.0);
        assert_eq!(number("Date.UTC(2020, 0, 1)"), 1577836800000.0);
        assert_eq!(
            number(
                "(function () { var d = new Date(0); d.setUTCFullYear(2020); \
                 return d.getUTCFullYear(); })()"
            ),
            2020.0
        );
    }
}
