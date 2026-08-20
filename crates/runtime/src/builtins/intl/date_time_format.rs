//! `Intl.DateTimeFormat` (ECMA-402 §11): the constructor (calendar/
//! numberingSystem/hourCycle/hour12 resolution, date/time components,
//! dateStyle/timeStyle, timeZone), `format`/`formatToParts` through
//! FormatDateTimePattern (the gregorian calendar, UTC and offset time
//! zones), and `resolvedOptions`. The pattern and name data is the
//! corpus-pinned en-US/en surface (the dateStyle/timeStyle tables, the
//! component formats, and the month/weekday/dayPeriod/era names); other
//! locales fall back to the en-US data. Instances store their record in the
//! agent's `intl_date_time_format_data` map.

use crux::error::{ErrorKind, JsError};
use crux::function::{Function, NativeFn};
use crux::handle::Handle;
use crux::object::JsObject;
use crux::property::{PropertyDescriptor, PropertyKey};
use crux::string::JsString;
use crux::value::{Value, ValueKind};

use crate::agent::Agent;
use crate::builtins::intl::number_format::{self, get_option};
use crate::builtins::temporal::TemporalRecord;
use crate::context::{as_object, get_property, to_number, to_string};
use crate::realm::Realm;

pub const DATE_TIME_FORMAT: &str = "%Intl.DateTimeFormat%";
pub const DATE_TIME_FORMAT_PROTO: &str = "%Intl.DateTimeFormat.prototype%";
pub const DTF_RESOLVED_OPTIONS: &str = "%Intl.DateTimeFormat.prototype.resolvedOptions%";
pub const DTF_FORMAT_GETTER: &str = "%Intl.DateTimeFormat.prototype.format%";
pub const DTF_FORMAT_TO_PARTS: &str = "%Intl.DateTimeFormat.prototype.formatToParts%";
pub const DTF_FORMAT_RANGE: &str = "%Intl.DateTimeFormat.prototype.formatRange%";
pub const DTF_FORMAT_RANGE_TO_PARTS: &str = "%Intl.DateTimeFormat.prototype.formatRangeToParts%";
pub const DTF_SUPPORTED_LOCALES_OF: &str = "%Intl.DateTimeFormat.supportedLocalesOf%";

fn range_error(message: &str) -> JsError {
    JsError::new(ErrorKind::RangeError, message.into())
}

fn type_error(message: &str) -> JsError {
    JsError::new(ErrorKind::TypeError, message.into())
}

/// The en-US (and fallback) month names: [long, short, narrow].
const MONTH_NAMES: [[&str; 12]; 3] = [
    [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ],
    [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ],
    ["J", "F", "M", "A", "M", "J", "J", "A", "S", "O", "N", "D"],
];

/// The en-US (and fallback) weekday names: [long, short, narrow].
const WEEKDAY_NAMES: [[&str; 7]; 3] = [
    [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ],
    ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"],
    ["S", "M", "T", "W", "T", "F", "S"],
];

/// The en day-period names per style: the five periods in hour order
/// (night, morning, noon, afternoon, evening) — the corpus pins
/// `in the morning`/`noon`/`in the afternoon`/`in the evening`/`at night`
/// for long and short, and `n` for the narrow noon.
const DAY_PERIOD_LONG: [&str; 5] = [
    "at night",
    "in the morning",
    "noon",
    "in the afternoon",
    "in the evening",
];
const DAY_PERIOD_NARROW: [&str; 5] = [
    "at night",
    "in the morning",
    "n",
    "in the afternoon",
    "in the evening",
];

/// The gregorian era names: [long, short, narrow] × [BC, AD].
const ERA_NAMES: [&[&str; 2]; 3] = [
    &["Before Christ", "Anno Domini"],
    &["BC", "AD"],
    &["B", "A"],
];

/// The day period of an hour (0-23): morning, noon, afternoon, evening,
/// night (the en-US CLDR ranges the corpus pins: 0-11 morning, 12 noon,
/// 13-17 afternoon, 18-20 evening, 21-23 night).
fn day_period_index(hour: u32) -> usize {
    match hour {
        12 => 2,
        13..=17 => 3,
        18..=20 => 4,
        21..=23 => 0,
        // 0..=11: morning.
        _ => 1,
    }
}

/// The dateStyle/timeStyle pattern table for en-US (CLDR `dateTimeFormats`).
/// Each entry is (style, hour-12 pattern, hour-24 pattern). The 24-hour
/// pattern is `None` when the 12-hour pattern is used for both.
const DATE_STYLE_PATTERNS: [(&str, &str); 4] = [
    ("full", "EEEE, MMMM d, y"),
    ("long", "MMMM d, y"),
    ("medium", "MMM d, y"),
    ("short", "M/d/yy"),
];
const TIME_STYLE_PATTERNS: [(&str, &str, &str); 4] = [
    ("full", "h:mm:ss a zzzz", "HH:mm:ss zzzz"),
    ("long", "h:mm:ss a z", "HH:mm:ss z"),
    ("medium", "h:mm:ss a", "HH:mm:ss"),
    ("short", "h:mm a", "HH:mm"),
];

/// The component formats for the en-US date/time available formats: the
/// (field-bits, pattern) table used by the format matcher (V8's en-US
/// `availableFormats`). Field bits: 1=weekday, 2=era, 4=year, 8=month,
/// 16=day, 32=hour, 64=minute, 128=second, 256=dayPeriod, 512=timeZoneName,
/// 1024=fractionalSecond. The 24-hour patterns use two-digit hours (HH);
/// the 12-hour `a`/`b` dayPeriod fields use the U+202F narrow no-break
/// space separator the en-US data pins.
const AVAILABLE_FORMATS: &[(u16, &str, &str)] = &[
    // Date-only.
    (4 | 8 | 16, "M/d/y", "M/d/y"),
    (1 | 4 | 8 | 16, "E, M/d/y", "E, M/d/y"),
    (1 | 4 | 8 | 16, "EEE, M/d/y", "EEE, M/d/y"),
    (1 | 4 | 8 | 16, "EEEE, M/d/y", "EEEE, M/d/y"),
    (4 | 8 | 16, "MMM d, y", "MMM d, y"),
    (4 | 8 | 16, "MMMM d, y", "MMMM d, y"),
    (1 | 4 | 8 | 16, "EEE, MMM d, y", "EEE, MMM d, y"),
    (1 | 4 | 8 | 16, "EEEE, MMMM d, y", "EEEE, MMMM d, y"),
    (4 | 8 | 16, "MM/dd/yy", "MM/dd/yy"),
    (4 | 8, "MMMM y", "MMMM y"),
    (4 | 8, "MMM y", "MMM y"),
    (4 | 8, "M/y", "M/y"),
    (4 | 8, "M/yy", "M/yy"),
    (8 | 16, "M/d", "M/d"),
    (1 | 8 | 16, "E, M/d", "E, M/d"),
    (1 | 8 | 16, "EEE, MMM d", "EEE, MMM d"),
    (1 | 8 | 16, "EEEE, MMMM d", "EEEE, MMMM d"),
    (8 | 16, "MMM d", "MMM d"),
    (8 | 16, "MMMM d", "MMMM d"),
    (2 | 4, "y G", "y G"),
    (2 | 4, "y GGGGG", "y GGGGG"),
    (2 | 4 | 8 | 16, "M/d/y G", "M/d/y G"),
    (2 | 4 | 8 | 16, "M/d/y GGGG", "M/d/y GGGG"),
    (2 | 4 | 8 | 16, "M/d/y GGGGG", "M/d/y GGGGG"),
    (4 | 8 | 16 | 512, "M/d/y, z", "M/d/y, z"),
    (4 | 8 | 16 | 512, "M/d/y, zzzz", "M/d/y, zzzz"),
    (4, "y", "y"),
    (4, "yy", "yy"),
    (16, "d", "d"),
    (16, "dd", "dd"),
    (8, "M", "M"),
    (8, "MM", "MM"),
    (8, "MMM", "MMM"),
    (8, "MMMM", "MMMM"),
    (8, "MMMMM", "MMMMM"),
    (1, "EEEE", "EEEE"),
    (1, "EEE", "EEE"),
    (1, "EEEEE", "EEEEE"),
    // Time-only (12h / 24h).
    (32 | 256, "h\u{202F}a", "HH"),
    (32 | 256, "hh\u{202F}a", "HH"),
    (32 | 256, "h b", "HH"),
    (256, "b", "b"),
    (32 | 64, "h:mm a", "HH:mm"),
    (32 | 64, "hh:mm a", "HH:mm"),
    (32 | 64 | 128, "h:mm:ss a", "HH:mm:ss"),
    (32 | 64 | 128, "hh:mm:ss a", "HH:mm:ss"),
    (32 | 64 | 256, "h:mm b", "HH:mm"),
    (32 | 256 | 512, "h\u{202F}a z", "HH z"),
    (32 | 256 | 512, "h\u{202F}a zzzz", "HH zzzz"),
    (32 | 64 | 128 | 512, "h:mm:ss a z", "HH:mm:ss z"),
    (32 | 64 | 128 | 512, "h:mm:ss a zzzz", "HH:mm:ss zzzz"),
    (64 | 128, "mm:ss", "mm:ss"),
    (64, "m", "m"),
    (64, "mm", "mm"),
    (128, "s", "s"),
    (32 | 64 | 128 | 1024, "h:mm:ss.SSS a", "HH:mm:ss.SSS"),
    (64 | 128 | 1024, "mm:ss.SSS", "mm:ss.SSS"),
    (
        32 | 64 | 128 | 512 | 1024,
        "h:mm:ss.SSS a z",
        "HH:mm:ss.SSS z",
    ),
    // Date + time.
    (4 | 8 | 16 | 32 | 64, "M/d/y, h:mm a", "M/d/y, HH:mm"),
    (
        4 | 8 | 16 | 32 | 64 | 128,
        "M/d/y, h:mm:ss a",
        "M/d/y, HH:mm:ss",
    ),
    (
        4 | 8 | 16 | 32 | 64 | 128 | 256,
        "M/d/y, h:mm:ss a",
        "M/d/y, HH:mm:ss",
    ),
    (
        4 | 8 | 16 | 32 | 64 | 128 | 512,
        "M/d/y, h:mm:ss a z",
        "M/d/y, HH:mm:ss z",
    ),
    (
        2 | 4 | 8 | 16 | 32 | 64 | 128,
        "M/d/y G, h:mm:ss a",
        "M/d/y G, HH:mm:ss",
    ),
    (
        1 | 4 | 8 | 16 | 32 | 64 | 128 | 256,
        "EEEE, MMMM d, y 'at' h:mm:ss a",
        "EEEE, MMMM d, y 'at' HH:mm:ss",
    ),
    (
        1 | 2 | 4 | 8 | 16 | 32 | 64 | 128 | 256 | 512,
        "EEEE, MMMM d, y 'at' h:mm:ss a zzzz",
        "EEEE, MMMM d, y 'at' HH:mm:ss zzzz",
    ),
    (
        1 | 4 | 8 | 16 | 32 | 64 | 128 | 256 | 512,
        "EEEE, M/d/y, h:mm:ss a z",
        "EEEE, M/d/y, HH:mm:ss z",
    ),
];

/// The pattern-field character sets per component (used by resolvedOptions
/// to decide which components appear).
const HOUR_CHARS: &str = "hHKk";

/// A formatted part.
struct DtfPart {
    part_type: String,
    value: String,
    source: Option<String>,
}

/// The [[InitializedDateTimeFormat]] record: the resolved options plus the
/// matched format (pattern + component field formats).
#[derive(Debug, Clone)]
pub struct DateTimeFormatRecord {
    pub locale: String,
    pub calendar: String,
    pub numbering_system: String,
    pub time_zone: String,
    pub hour_cycle: String,
    pub hour12: Option<bool>,
    pub date_style: Option<String>,
    pub time_style: Option<String>,
    pub pattern: String,
    /// The 12-hour pattern variant (the hourCycle flips between them).
    pub pattern12: Option<String>,
    /// The format fields: component name → field format (e.g. "year" →
    /// "2-digit").
    pub fields: Vec<(String, String)>,
    /// The component bits the caller explicitly requested (the
    /// CreateDateTimeFormat `hasExplicitFormatComponents` set — excludes the
    /// non-gregory era addition and the defaults).
    pub explicit_bits: u16,
    pub fractional_second_digits: Option<u32>,
    /// The cached [[BoundFormat]] function value.
    pub bound_format: Option<Value>,
}

impl DateTimeFormatRecord {
    /// The active pattern for the record's hour cycle.
    fn active_pattern(&self) -> &str {
        let uses_12h = self.hour_cycle == "h11" || self.hour_cycle == "h12";
        if uses_12h && let Some(pattern12) = &self.pattern12 {
            return pattern12;
        }
        &self.pattern
    }
}

/// The calendar/numbering-system/hour-cycle supported lists.
fn supported_calendars() -> &'static [&'static str] {
    crate::builtins::intl::number_data::SUPPORTED_CALENDARS
}

fn supported_numbering_systems() -> &'static [&'static str] {
    crate::builtins::intl::number_data::SUPPORTED_NUMBERING_SYSTEMS
}

/// The `hc` keyword values.
const HOUR_CYCLES: [&str; 4] = ["h11", "h12", "h23", "h24"];

/// The CLDR default hour cycle is 24-hour for most of Europe (the corpus
/// pins de-AT's 24-hour format in the Temporal toLocaleString basic
/// fixtures; en/ja default to the 12-hour clock).
fn locale_uses_24h(locale: &str) -> bool {
    let base = locale.split(['-', '_']).next().unwrap_or(locale);
    matches!(
        base,
        "de" | "fr"
            | "it"
            | "es"
            | "pt"
            | "nl"
            | "fi"
            | "sv"
            | "no"
            | "nb"
            | "nn"
            | "da"
            | "is"
            | "lt"
            | "lv"
            | "et"
            | "pl"
            | "cs"
            | "sk"
            | "sl"
            | "hr"
            | "sr"
            | "bs"
            | "mk"
            | "bg"
            | "el"
            | "hu"
            | "ro"
            | "uk"
            | "be"
            | "ru"
            | "ca"
            | "sq"
            | "th"
            | "vi"
            | "id"
    )
}

/// ResolveLocale for DateTimeFormat: the `ca`, `nu`, and `hc` extension
/// keys, with the option overrides. Returns
/// (resolved_locale, calendar, numbering_system, hour_cycle).
fn resolve_locale_dtf(
    _agent: &mut Agent,
    requested: &[String],
    calendar: Option<&str>,
    numbering_system: Option<&str>,
    hour_cycle: Option<&str>,
) -> Result<(String, String, String, String), JsError> {
    let available = crate::builtins::intl::number_data::NUMBER_FORMAT_LOCALES;
    let mut found: Option<String> = None;
    let mut extension: Option<String> = None;
    for locale in requested {
        let base = number_format::strip_unicode_extension(locale);
        if let Some(matched) = number_format::best_fit(available, &base) {
            found = Some(matched);
            extension = if base == *locale {
                None
            } else {
                Some(locale.clone())
            };
            break;
        }
    }
    let mut found_locale = found.unwrap_or_else(|| number_format::default_locale().to_string());
    let mut ca = "gregory".to_string();
    let mut nu = "latn".to_string();
    let mut hc: Option<String> = None;
    let mut supported: Vec<(String, String)> = Vec::new();
    if let Some(ext) = extension {
        if let Some(value) = number_format::unicode_extension_keyword_value(&ext, "ca")
            && !value.is_empty()
        {
            let value = crate::builtins::intl::bcp47::canonicalize_uvalue("ca", &value);
            if supported_calendars().contains(&value.as_str()) {
                ca = value.clone();
                supported.push(("ca".to_string(), value));
            }
        }
        if let Some(value) = number_format::unicode_extension_keyword_value(&ext, "nu")
            && !value.is_empty()
        {
            let value = crate::builtins::intl::bcp47::canonicalize_uvalue("nu", &value);
            if supported_numbering_systems().contains(&value.as_str()) {
                nu = value.clone();
                supported.push(("nu".to_string(), value));
            }
        }
        if let Some(value) = number_format::unicode_extension_keyword_value(&ext, "hc")
            && !value.is_empty()
            && HOUR_CYCLES.contains(&value.as_str())
        {
            hc = Some(value.clone());
            supported.push(("hc".to_string(), value));
        }
    }
    // The option overrides: a supported option value replaces the
    // extension value and drops the corresponding keyword from the locale.
    if let Some(value) = calendar {
        let value = crate::builtins::intl::bcp47::canonicalize_uvalue("ca", value);
        if supported_calendars().contains(&value.as_str()) && value != ca {
            ca = value;
            supported.retain(|(key, _)| key != "ca");
        }
    }
    if let Some(value) = numbering_system {
        let value = crate::builtins::intl::bcp47::canonicalize_uvalue("nu", value);
        if supported_numbering_systems().contains(&value.as_str()) && value != nu {
            nu = value;
            supported.retain(|(key, _)| key != "nu");
        }
    }
    if let Some(value) = hour_cycle
        && HOUR_CYCLES.contains(&value)
        && Some(value) != hc.as_deref()
    {
        hc = Some(value.to_string());
        supported.retain(|(key, _)| key != "hc");
    }
    if !supported.is_empty() {
        let mut keywords: Vec<String> = supported
            .into_iter()
            .map(|(key, value)| format!("{key}-{value}"))
            .collect();
        keywords.sort();
        // Insert the whole keyword list as the `u` extension (the shared
        // insert_unicode_extension helper takes a single keyword).
        let base = number_format::strip_unicode_extension(&found_locale);
        let tagged = format!("{base}-u-{}", keywords.join("-"));
        found_locale = crate::builtins::intl::bcp47::canonicalize(&tagged).unwrap_or(tagged);
    }
    Ok((
        found_locale,
        ca,
        nu,
        // The empty string means the locale had no `hc` keyword (the
        // caller falls back to the locale's default hour cycle).
        hc.unwrap_or_default(),
    ))
}

/// Remove an `hc-XXXX` keyword from the locale's unicode extension (the
/// `hour12`/`hourCycle` option overrides drop it when the values differ).
fn strip_hc_keyword(locale: &str) -> String {
    let Some((base, ext)) = locale.split_once("-u-") else {
        return locale.to_string();
    };
    let mut out: Vec<&str> = Vec::new();
    let mut skipping = false;
    for part in ext.split('-') {
        if part == "hc" {
            skipping = true;
            continue;
        }
        if skipping {
            skipping = false;
            continue;
        }
        out.push(part);
    }
    if out.is_empty() {
        return base.to_string();
    }
    format!("{base}-u-{}", out.join("-"))
}

/// GetOption with type `boolean`: ToBoolean (no ToString on the value —
/// the constructor-options order fixture pins that only the getter runs).
fn get_boolean_option(
    agent: &mut Agent,
    options: &Value,
    name: &str,
) -> Result<Option<bool>, JsError> {
    let value = get_property(agent, options, &JsString::from_utf8(name), options.clone())?;
    if value.is_undefined() {
        return Ok(None);
    }
    Ok(Some(crux::convert::to_boolean(&value)))
}

/// The `type` Unicode locale nonterminal (the calendar/`ca` values).
fn is_type_identifier(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    value.split('-').all(|subtag| {
        (3..=8).contains(&subtag.len()) && subtag.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

/// The `type` Unicode locale nonterminal (the calendar/`ca` values).
/// FormatOffsetTimeZoneIdentifier: `±HH:MM`.
fn format_offset_time_zone_identifier(offset_minutes: i64) -> String {
    let sign = if offset_minutes < 0 { '-' } else { '+' };
    let absolute = offset_minutes.abs();
    format!("{sign}{:02}:{:02}", absolute / 60, absolute % 60)
}

/// Parse a `±HH`, `±HHMM`, or `±HH:MM` offset time-zone string into
/// minutes (the resolved form is always `±HH:MM`).
fn parse_offset_time_zone(value: &str) -> Option<i64> {
    let sign = match value.as_bytes().first() {
        Some(b'+') => 1i64,
        Some(b'-') => -1i64,
        _ => return None,
    };
    let rest = &value[1..];
    let (hours_text, minutes_text) = match rest.len() {
        2 => (rest, "00"),
        4 => (&rest[..2], &rest[2..]),
        5 if rest.as_bytes()[2] == b':' => (&rest[..2], &rest[3..]),
        _ => return None,
    };
    let hours: i64 = hours_text.parse().ok()?;
    let minutes: i64 = minutes_text.parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(sign * (hours * 60 + minutes))
}

/// The fixed offset (minutes) of the UTC/Etc/GMT±N named zones; None for
/// the other named zones (whose local time falls back to UTC).
/// The offset (minutes) of a named zone at the formatted instant (the UTC
/// family and the fixed Etc/GMT±N zones are constant; the IANA zones use the
/// generated tables). 0 when unsupported (the legacy fallback).
fn named_zone_offset(time_zone: &str, epoch_ms: f64) -> i64 {
    if time_zone == "UTC" || time_zone == "Etc/UTC" || time_zone == "Etc/GMT" {
        return 0;
    }
    if let Some(rest) = time_zone.strip_prefix("Etc/GMT") {
        // IANA Etc/GMT-N is UTC+N (the sign is inverted).
        if let Ok(hours) = rest.parse::<i64>() {
            return -hours * 60;
        }
    }
    let Some(zone) = unicode::tz::resolve_zone(time_zone) else {
        return 0;
    };
    let epoch_ns = (epoch_ms * 1_000_000.0) as i128;
    let (offset_secs, ..) = unicode::tz::offset_info_at(zone, epoch_ns);
    offset_secs as i64 / 60
}

/// The (gregorian) local-time fields of an epoch milliseconds value in the
/// record's time zone.
struct LocalTime {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    millisecond: u32,
    weekday: u32,
}

/// Convert an epoch-milliseconds value to the gregorian calendar fields
/// in the time zone (UTC, an offset, or the fixed Etc/GMT±N zones).
fn to_local_time(epoch_ms: f64, time_zone: &str) -> LocalTime {
    let offset_minutes = if let Some(offset) = parse_offset_time_zone(time_zone) {
        offset
    } else {
        named_zone_offset(time_zone, epoch_ms)
    };
    let local_ms = epoch_ms + offset_minutes as f64 * 60_000.0;
    let days = (local_ms / 86_400_000.0).floor() as i64;
    let ms_of_day = (local_ms - days as f64 * 86_400_000.0).floor() as i64;
    let (year, month, day) = civil_from_days(days);
    let hour = (ms_of_day / 3_600_000) % 24;
    let minute = (ms_of_day / 60_000) % 60;
    let second = (ms_of_day / 1_000) % 60;
    let millisecond = ms_of_day % 1_000;
    // The weekday: 1970-01-01 was a Thursday. The JS weekday is 0=Sunday.
    let weekday = ((days + 4) % 7 + 7) % 7;
    LocalTime {
        year,
        month: month as u32,
        day: day as u32,
        hour: hour as u32,
        minute: minute as u32,
        second: second as u32,
        millisecond: millisecond as u32,
        weekday: weekday as u32,
    }
}

/// The proleptic gregorian calendar: days since 1970-01-01 → (y, m, d)
/// (Howard Hinnant's civil_from_days).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The component field-bits table (Table 16), in read order.
const COMPONENT_ORDER: &[(&str, u16)] = &[
    ("weekday", 1),
    ("era", 2),
    ("year", 4),
    ("month", 8),
    ("day", 16),
    ("dayPeriod", 256),
    ("hour", 32),
    ("minute", 64),
    ("second", 128),
    ("timeZoneName", 512),
];

/// The component option values per property.
fn component_values(property: &str) -> &'static [&'static str] {
    match property {
        "weekday" => &["narrow", "short", "long"],
        "era" => &["narrow", "short", "long"],
        "year" => &["2-digit", "numeric"],
        "month" => &["2-digit", "numeric", "narrow", "short", "long"],
        "day" => &["2-digit", "numeric"],
        "dayPeriod" => &["narrow", "short", "long"],
        "hour" => &["2-digit", "numeric"],
        "minute" => &["2-digit", "numeric"],
        "second" => &["2-digit", "numeric"],
        "timeZoneName" => &[
            "short",
            "long",
            "shortOffset",
            "longOffset",
            "shortGeneric",
            "longGeneric",
        ],
        _ => &[],
    }
}

/// Intl.DateTimeFormat (ECMA-402 §11.1.2 CreateDateTimeFormat).
fn initialize(
    agent: &mut Agent,
    locales: &Value,
    options: &Value,
    required: &str,
    defaults: &str,
) -> Result<DateTimeFormatRecord, JsError> {
    let requested = crate::builtins::intl::canonicalize_locale_list(agent, locales)?;
    let options = number_format::coerce_options_to_object(agent, options)?;
    // ResolveOptions: localeMatcher, then the ca/nu/hc options, then the
    // hour12 option (modifyResolutionOptions reads it and nulls hc).
    get_option(
        agent,
        &options,
        "localeMatcher",
        &["lookup", "best fit"],
        Some("best fit"),
    )?;
    let calendar = get_option(agent, &options, "calendar", &[], None)?;
    if let Some(value) = &calendar
        && !is_type_identifier(value)
    {
        return Err(range_error(
            "Value cannot be matched by the type Unicode locale nonterminal",
        ));
    }
    let numbering_system = get_option(agent, &options, "numberingSystem", &[], None)?;
    if let Some(value) = &numbering_system
        && !is_type_identifier(value)
    {
        return Err(range_error(
            "Value cannot be matched by the type Unicode locale nonterminal",
        ));
    }
    // hour12 is a boolean option (no ToString); the constructor-options
    // order fixture pins that only the getter runs.
    let hour12 = get_boolean_option(agent, &options, "hour12")?;
    let hour_cycle = get_option(agent, &options, "hourCycle", &[], None)?;
    if let Some(value) = &hour_cycle
        && !HOUR_CYCLES.contains(&value.as_str())
    {
        return Err(range_error("Invalid hourCycle"));
    }
    let (locale, calendar, numbering_system, hc_value) = resolve_locale_dtf(
        agent,
        &requested,
        calendar.as_deref(),
        numbering_system.as_deref(),
        hour_cycle.as_deref(),
    )?;
    // The hc value from the unicode extension (the empty string means the
    // locale has no `hc` keyword).
    let ext_hc = (!hc_value.is_empty()).then_some(hc_value.clone());
    // The locale's hour cycles: en-US (and the fallback) use h12/h23; ja
    // uses h11 for the 12-hour clock (the corpus pins `hour12` == h11 for
    // ja and h23 for `hour12: false` everywhere); the CLDR 24-hour locales
    // (most of Europe plus th/vi/id) default to h23.
    let is_ja = locale.starts_with("ja");
    let hc12 = if is_ja { "h11" } else { "h12" };
    let hc24 = "h23";
    let default_hc = if locale_uses_24h(&locale) { hc24 } else { hc12 };
    let hc = if let Some(true) = hour12 {
        hc12.to_string()
    } else if let Some(false) = hour12 {
        hc24.to_string()
    } else if hour_cycle.is_some() {
        // The option overrides the extension (resolve_locale_dtf already
        // dropped the `hc` keyword from the locale when they differ).
        hc_value
    } else if let Some(value) = &ext_hc {
        value.clone()
    } else {
        default_hc.to_string()
    };
    // The hour12 option also drops a differing `hc` unicode-extension
    // keyword from the resolved locale (resolved-locale-with-hc-unicode).
    let locale = if hour12.is_some()
        && let Some(ext) = &ext_hc
        && ext != &hc
    {
        strip_hc_keyword(&locale)
    } else {
        locale
    };
    let time_zone_value = get_property(
        agent,
        &options,
        &JsString::from_utf8("timeZone"),
        options.clone(),
    )?;
    let time_zone = if time_zone_value.is_undefined() {
        "UTC".to_string()
    } else {
        let value = to_string(agent, &time_zone_value)?.to_string_lossy();
        if let Some(offset) = parse_offset_time_zone(&value) {
            format_offset_time_zone_identifier(offset)
        } else if value.eq_ignore_ascii_case("utc")
            || value.eq_ignore_ascii_case("etc/utc")
            || value.eq_ignore_ascii_case("etc/gmt")
        {
            "UTC".to_string()
        } else if value.starts_with("Etc/GMT+") || value.starts_with("Etc/GMT-") {
            // The fixed-offset Etc/GMT±N zones.
            value
        } else if let Some(zone) = unicode::tz::resolve_zone(&value) {
            // The IANA zones (case-insensitive; links resolve to their
            // primary identifier).
            unicode::tz::primary_identifier(zone).to_string()
        } else {
            return Err(range_error("Invalid time zone"));
        }
    };
    // The format options (Table 16) + fractionalSecondDigits, read in the
    // pinned order (fractionalSecondDigits between second and timeZoneName).
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut requested_bits: u16 = 0;
    let mut explicit_bits: u16 = 0;
    let mut fractional_second_digits: Option<u32> = None;
    for (property, bit) in COMPONENT_ORDER {
        let value = get_option(agent, &options, property, component_values(property), None)?;
        if let Some(value) = value {
            fields.push((property.to_string(), value));
            requested_bits |= bit;
            explicit_bits |= bit;
        }
        if *property == "second"
            && let Some(value) =
                get_number_option(agent, &options, "fractionalSecondDigits", 1.0, 3.0)?
        {
            fractional_second_digits = Some(value as u32);
            requested_bits |= 1024;
            explicit_bits |= 1024;
        }
    }
    // The non-gregorian calendars format with an era field (V8 adds the
    // era to the japanese/islamic/hebrew etc. patterns; the corpus only
    // pins that some calendars use a different pattern than gregory). The
    // iso8601 calendar formats like gregory (no era).
    if calendar != "gregory" && calendar != "iso8601" && !fields.iter().any(|(n, _)| n == "era") {
        fields.push(("era".to_string(), "short".to_string()));
        requested_bits |= 2;
    }
    let format_matcher = get_option(
        agent,
        &options,
        "formatMatcher",
        &["basic", "best fit"],
        Some("best fit"),
    )?;
    let date_style = get_option(
        agent,
        &options,
        "dateStyle",
        &["full", "long", "medium", "short"],
        None,
    )?;
    let time_style = get_option(
        agent,
        &options,
        "timeStyle",
        &["full", "long", "medium", "short"],
        None,
    )?;
    if date_style.is_some() || time_style.is_some() {
        // CreateDateTimeFormat: format components conflict with the styles,
        // and a style the required set cannot express is a TypeError (the
        // PlainDate/PlainTime toLocaleString required date/time rejections).
        if explicit_bits != 0 {
            return Err(type_error(
                "Cannot specify both dateStyle/timeStyle and format components",
            ));
        }
        if required == "date" && time_style.is_some() {
            return Err(type_error(
                "timeStyle is not allowed for a date-only toLocaleString",
            ));
        }
        if required == "time" && date_style.is_some() {
            return Err(type_error(
                "dateStyle is not allowed for a time-only toLocaleString",
            ));
        }
    }
    // The pattern selection.
    let (pattern, pattern12, _field_bits) = if date_style.is_some() || time_style.is_some() {
        style_pattern(
            date_style.as_deref(),
            time_style.as_deref(),
            &mut fields,
            &hc,
        )
    } else {
        // The defaults: a DateTimeFormat() with no options formats
        // y/M/d (the constructor passes defaults "date"; the toLocale*
        // variants pass their required/defaults).
        let mut bits = requested_bits;
        let mut effective = fields.clone();
        let mut need_defaults = true;
        if required == "date" || required == "any" {
            for (name, _) in [("weekday", 1), ("year", 4), ("month", 8), ("day", 16)] {
                if effective.iter().any(|(n, _)| n == name) {
                    need_defaults = false;
                }
            }
        }
        if required == "time" || required == "any" {
            for (name, _) in [
                ("dayPeriod", 256),
                ("hour", 32),
                ("minute", 64),
                ("second", 128),
            ] {
                if effective.iter().any(|(n, _)| n == name) {
                    need_defaults = false;
                }
            }
            if fractional_second_digits.is_some() {
                need_defaults = false;
            }
        }
        if need_defaults && (defaults == "date" || defaults == "all") {
            for (name, bit) in [("year", 4), ("month", 8), ("day", 16)] {
                if !effective.iter().any(|(n, _)| n == name) {
                    bits |= bit;
                    effective.push((name.to_string(), "numeric".to_string()));
                }
            }
        }
        if need_defaults && (defaults == "time" || defaults == "all") {
            for (name, bit) in [("hour", 32), ("minute", 64), ("second", 128)] {
                if !effective.iter().any(|(n, _)| n == name) {
                    bits |= bit;
                    effective.push((name.to_string(), "numeric".to_string()));
                }
            }
        }
        fields = effective;
        match_format(&fields, bits, &hc, fractional_second_digits)
    };
    let _ = format_matcher;
    Ok(DateTimeFormatRecord {
        locale,
        calendar,
        numbering_system,
        time_zone,
        hour_cycle: hc,
        hour12,
        date_style,
        time_style,
        pattern,
        pattern12,
        fields,
        explicit_bits,
        fractional_second_digits,
        bound_format: None,
    })
}

/// GetNumberOption for fractionalSecondDigits.
fn get_number_option(
    agent: &mut Agent,
    options: &Value,
    name: &str,
    minimum: f64,
    maximum: f64,
) -> Result<Option<f64>, JsError> {
    let value = get_property(agent, options, &JsString::from_utf8(name), options.clone())?;
    if value.is_undefined() {
        return Ok(None);
    }
    let number = to_number(agent, &value)?;
    if !number.is_finite() || number < minimum || number > maximum {
        return Err(range_error(&format!(
            "Value {number} out of range for option {name}"
        )));
    }
    Ok(Some(number))
}

/// DateTimeStyleFormat: the dateStyle/timeStyle pattern table (en-US).
/// Returns (pattern, pattern12, field_bits).
fn style_pattern(
    date_style: Option<&str>,
    time_style: Option<&str>,
    fields: &mut Vec<(String, String)>,
    hc: &str,
) -> (String, Option<String>, u16) {
    let uses_12h = hc == "h11" || hc == "h12";
    let date_part = date_style.and_then(|style| {
        DATE_STYLE_PATTERNS
            .iter()
            .find(|(s, _)| *s == style)
            .map(|(_, p)| *p)
    });
    let time_part = time_style.and_then(|style| {
        TIME_STYLE_PATTERNS
            .iter()
            .find(|(s, _, _)| *s == style)
            .map(|(_, p12, p24)| if uses_12h { *p12 } else { *p24 })
    });
    let (pattern, pattern12, bits): (String, Option<String>, u16) = match (date_style, time_style) {
        (Some(_), None) => {
            let mut bits = 4 | 8 | 16;
            if date_style == Some("full") {
                bits |= 1;
            }
            (date_part.unwrap_or("M/d/y").to_string(), None, bits)
        }
        (None, Some(_)) => {
            let mut bits = 32 | 64 | 128;
            if time_style == Some("short") {
                bits &= !128;
            }
            if time_style == Some("full") || time_style == Some("long") {
                bits |= 512;
            }
            let pattern = time_part.unwrap_or("h:mm a").to_string();
            let pattern12 = if uses_12h {
                Some(pattern.clone())
            } else {
                None
            };
            (pattern, pattern12, bits)
        }
        _ => {
            // date + time: the connector (en-US: ", " — the fixture also
            // allows " at ").
            let date = date_part.unwrap_or("M/d/y");
            let time = time_part.unwrap_or("h:mm a");
            let mut bits = 4 | 8 | 16 | 32 | 64 | 128;
            if date_style == Some("full") {
                bits |= 1;
            }
            if time_style == Some("full") || time_style == Some("long") {
                bits |= 512;
            }
            let pattern = format!("{date}, {time}");
            let pattern12 = if uses_12h {
                let time12 = TIME_STYLE_PATTERNS
                    .iter()
                    .find(|(s, _, _)| *s == time_style.unwrap_or("short"))
                    .map(|(_, p12, _)| *p12)
                    .unwrap_or("h:mm a");
                Some(format!("{date}, {time12}"))
            } else {
                None
            };
            (pattern, pattern12, bits)
        }
    };
    // The resolved fields for resolvedOptions.
    fields.clear();
    for (name, bit) in COMPONENT_ORDER {
        if bits & bit != 0 {
            fields.push((name.to_string(), "numeric".to_string()));
        }
    }
    (pattern, pattern12, bits)
}

/// The format matcher: pick the available format whose field set best
/// matches the requested bits (the BasicFormatMatcher penalties, applied
/// to the curated table). Returns (pattern, pattern12, field_bits).
fn match_format(
    fields: &[(String, String)],
    requested_bits: u16,
    hc: &str,
    fractional: Option<u32>,
) -> (String, Option<String>, u16) {
    let uses_12h = hc == "h11" || hc == "h12";
    // The requested field styles (e.g. month "short" → the "MMM"
    // pattern, year "2-digit" → "yy").
    let style_of = |name: &str| -> Option<&str> {
        fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, value)| value.as_str())
    };
    let month_style = style_of("month");
    let year_style = style_of("year");
    let weekday_style = style_of("weekday");
    let hour_style = style_of("hour");
    let day_style = style_of("day");
    let minute_style = style_of("minute");
    let day_period_requested = requested_bits & 256 != 0;
    let fractional_requested = requested_bits & 1024 != 0;
    let mut best: Option<(i64, &(u16, &str, &str))> = None;
    for entry in AVAILABLE_FORMATS {
        let (bits, p12, _p24) = *entry;
        let mut score = 0i64;
        for (_name, bit) in COMPONENT_ORDER {
            let requested = requested_bits & bit != 0;
            let present = bits & bit != 0;
            if requested && !present {
                score -= 120;
            } else if !requested && present {
                score -= 20;
            }
        }
        let has_fractional = bits & 1024 != 0;
        if fractional_requested && !has_fractional {
            score -= 120;
        } else if !fractional_requested && has_fractional {
            score -= 20;
        }
        // The field-style match: a requested narrow/short/long/2-digit
        // style must be reflected in the pattern field width.
        if let Some(style) = month_style {
            let width = pattern_width(p12, "ML");
            let expected = match style {
                "long" => 4,
                "short" => 3,
                "narrow" => 5,
                "2-digit" => 2,
                _ => 1,
            };
            if width != expected {
                score -= 6;
            }
        }
        if let Some(style) = year_style {
            let width = pattern_width(p12, "y");
            if (style == "2-digit") != (width == 2) {
                score -= 6;
            }
        }
        if let Some(style) = weekday_style {
            let width = pattern_width(p12, "Eec");
            let expected = match style {
                "long" => 4,
                "short" => 3,
                _ => 1,
            };
            if width != expected {
                score -= 6;
            }
        }
        if let Some(style) = hour_style {
            let width = pattern_width(p12, "hHKk");
            if (style == "2-digit") != (width >= 2) {
                score -= 6;
            }
        }
        if let Some(style) = day_style {
            let width = pattern_width(p12, "d");
            if (style == "2-digit") != (width >= 2) {
                score -= 6;
            }
        }
        if let Some(style) = minute_style {
            let width = pattern_width(p12, "m");
            if (style == "2-digit") != (width >= 2) {
                score -= 6;
            }
        }
        // A requested dayPeriod needs a flexible (b/B) field; the `a`
        // am/pm field only appears when the hour alone is requested.
        if day_period_requested && pattern_width(p12, "bB") == 0 {
            score -= 6;
        }
        if best.map(|(s, _)| score > s).unwrap_or(true) {
            best = Some((score, entry));
        }
    }
    let (_, (bits, p12, p24)) = best.unwrap_or((0, &AVAILABLE_FORMATS[0]));
    // The fractionalSecondDigits adjust the fractional field width in the
    // chosen pattern (the `S` run becomes the requested digit count).
    let mut p12 = p12.to_string();
    let mut p24 = p24.to_string();
    if let Some(digits) = fractional {
        let rewrite = |pattern: &mut String| {
            if pattern.contains('S') {
                let mut out = String::new();
                for c in pattern.chars() {
                    if c == 'S' {
                        out.push_str(&"S".repeat(digits as usize));
                    } else {
                        out.push(c);
                    }
                }
                *pattern = out;
            }
        };
        rewrite(&mut p12);
        rewrite(&mut p24);
    }
    let pattern = if uses_12h { p12 } else { p24 };
    let pattern12 = if uses_12h {
        Some(pattern.clone())
    } else {
        None
    };
    (pattern, pattern12, *bits)
}

/// The width of the first field run of any character in `chars`.
fn pattern_width(pattern: &str, chars: &str) -> u32 {
    let mut width = 0;
    for c in pattern.chars() {
        if chars.contains(c) {
            width += 1;
        } else if width > 0 {
            break;
        }
    }
    width
}

/// TimeClip for the format entry points: an |x| > 8.64×10^15 (or NaN)
/// value throws a RangeError (the corpus pins `date-is-nan-throws`,
/// `date-is-infinity-throws`, and the time-boundary fixtures), and the
/// value is truncated toward zero (ToInteger, the time-clip-to-integer
/// fixtures: format(-0.9) formats 0).
fn clip_epoch_ms(epoch_ms: f64) -> Result<f64, JsError> {
    if epoch_ms.is_nan() || epoch_ms.abs() > 8.64e15 {
        return Err(range_error("Invalid time value"));
    }
    let clipped = epoch_ms.trunc();
    Ok(if clipped == 0.0 { 0.0 } else { clipped })
}

/// FormatDateTimePattern (ECMA-402 §11.5.5): the parts of `epoch_ms` under
/// the record's pattern.
fn format_date_time_pattern(record: &DateTimeFormatRecord, epoch_ms: f64) -> Vec<DtfPart> {
    let pattern = record.active_pattern();
    format_with_pattern(record, epoch_ms, pattern)
}

/// The pattern-formatting core: the parts of `epoch_ms` under an explicit
/// pattern (the record's own, or a range-pattern fragment).
fn format_with_pattern(
    record: &DateTimeFormatRecord,
    epoch_ms: f64,
    pattern: &str,
) -> Vec<DtfPart> {
    let local = to_local_time(epoch_ms, &record.time_zone);
    let ns = &record.numbering_system;
    let mut parts = Vec::new();
    let mut literal = String::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' {
            // A quoted literal: the quotes are escapes, not output. A
            // doubled quote is a literal quote.
            if chars.peek() == Some(&'\'') {
                chars.next();
                literal.push('\'');
                continue;
            }
            let mut closed = false;
            for next in chars.by_ref() {
                if next == '\'' {
                    closed = true;
                    break;
                }
                literal.push(next);
            }
            let _ = closed;
            continue;
        }
        let field = match c {
            'y' => Some((
                "year",
                format_field_year(&local, field_length(&mut chars, c), ns),
            )),
            'M' | 'L' => Some((
                "month",
                format_field_month(&local, field_length(&mut chars, c), ns),
            )),
            'd' => Some((
                "day",
                format_number(
                    local.day as i64,
                    if field_length(&mut chars, c) >= 2 {
                        2
                    } else {
                        1
                    },
                    ns,
                ),
            )),
            'E' | 'e' | 'c' => Some((
                "weekday",
                format_field_weekday(&local, field_length(&mut chars, c)),
            )),
            'h' | 'K' => Some((
                "hour",
                format_hour(
                    &local,
                    &record.hour_cycle,
                    c == 'K',
                    field_length(&mut chars, c),
                    ns,
                ),
            )),
            'H' | 'k' => {
                let width = field_length(&mut chars, c);
                // `k` is 1-24; the h24 hour cycle also renders midnight as
                // 24 (spec: set value to 24 when hour is 0 and the cycle is
                // h24).
                let hour = if (c == 'k' || record.hour_cycle == "h24") && local.hour == 0 {
                    24
                } else {
                    local.hour
                };
                Some((
                    "hour",
                    format_number(hour as i64, if width >= 2 { 2 } else { 1 }, ns),
                ))
            }
            'm' => {
                let width = field_length(&mut chars, c);
                Some((
                    "minute",
                    format_number(local.minute as i64, if width >= 2 { 2 } else { 1 }, ns),
                ))
            }
            's' => {
                let width = field_length(&mut chars, c);
                Some((
                    "second",
                    format_number(local.second as i64, if width >= 2 { 2 } else { 1 }, ns),
                ))
            }
            'a' => {
                let _ = field_length(&mut chars, c);
                Some(("dayPeriod", format_am_pm(&local)))
            }
            'b' | 'B' => {
                let _ = field_length(&mut chars, c);
                Some(("dayPeriod", format_day_period(&local, record)))
            }
            'G' => {
                let width = field_length(&mut chars, c);
                Some(("era", format_era(&local, width)))
            }
            'z' | 'v' | 'V' => {
                let width = field_length(&mut chars, c);
                Some(("timeZoneName", format_time_zone(record, width)))
            }
            'S' => {
                let _ = field_length(&mut chars, c);
                Some(("fractionalSecond", format_fractional(&local, record)))
            }
            _ => None,
        };
        if let Some((part_type, value)) = field {
            if !literal.is_empty() {
                parts.push(DtfPart {
                    part_type: "literal".to_string(),
                    value: literal_separator(std::mem::take(&mut literal), ns),
                    source: None,
                });
            }
            parts.push(DtfPart {
                part_type: part_type.to_string(),
                value,
                source: None,
            });
        } else {
            literal.push(c);
        }
    }
    if !literal.is_empty() {
        parts.push(DtfPart {
            part_type: "literal".to_string(),
            value: literal_separator(literal, ns),
            source: None,
        });
    }
    parts
}

/// The plain Temporal kinds handled by HandleDateTimeValue: the value's ISO
/// fields are interpreted as UTC wall-clock (the time zone is ignored) and
/// the pattern is filtered to the fields the kind supports.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PlainKind {
    Date,
    Time,
    DateTime,
    YearMonth,
    MonthDay,
}

impl PlainKind {
    /// Whether the kind keeps a component field (the field-filter of
    /// HandleDateTimeTemporalDate/Time/DateTime/YearMonth/MonthDay).
    fn keeps(self, field: &str) -> bool {
        match self {
            PlainKind::Date => matches!(field, "weekday" | "era" | "year" | "month" | "day"),
            PlainKind::Time => matches!(field, "dayPeriod" | "hour" | "minute" | "second"),
            PlainKind::DateTime => field != "timeZoneName",
            PlainKind::YearMonth => matches!(field, "era" | "year" | "month"),
            PlainKind::MonthDay => matches!(field, "weekday" | "era" | "month" | "day"),
        }
    }

    /// The fractional-second component is a time-only field.
    fn keeps_fractional(self) -> bool {
        matches!(self, PlainKind::Time | PlainKind::DateTime)
    }
}

/// The result of HandleDateTimeValue: a tagged value with its epoch
/// milliseconds (the plain kinds carry the UTC wall-clock epoch).
pub(crate) enum FormatValue {
    /// A Temporal.Instant: epoch milliseconds from [[EpochNanoseconds]].
    Instant { epoch_ms: f64 },
    /// A Temporal.Plain* value: the UTC wall-clock fields (isPlain true).
    Plain { kind: PlainKind, epoch_ms: f64 },
}

/// The UTC epoch milliseconds of an ISO date-time field set (the plain
/// path: no time-zone offset, no TimeClip — the day count is exact i64
/// math, so the PlainDate limits far exceed the legacy Date range).
#[allow(clippy::too_many_arguments)]
pub(crate) fn plain_epoch_ms(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    millisecond: i64,
    microsecond: i64,
    nanosecond: i64,
) -> f64 {
    let days = crate::builtins::temporal::iso::iso_date_to_epoch_days(year, month - 1, day);
    let ms_of_day = hour as f64 * 3_600_000.0
        + minute as f64 * 60_000.0
        + second as f64 * 1_000.0
        + millisecond as f64
        + microsecond as f64 / 1_000.0
        + nanosecond as f64 / 1_000_000.0;
    days as f64 * 86_400_000.0 + ms_of_day
}

/// HandleDateTimeValue (ECMA-402 §11.5.15): detect a Temporal argument.
/// Returns `None` for non-Temporal values (the Number/to_number path). A
/// Temporal.ZonedDateTime throws a TypeError (not supported by format).
pub(crate) fn handle_datetime_value(
    agent: &mut Agent,
    value: &Value,
    record_calendar: &str,
) -> Result<Option<FormatValue>, JsError> {
    let ValueKind::Object(obj) = value.kind() else {
        return Ok(None);
    };
    let Some(record) = agent.temporal_data.get(&obj.id()).cloned() else {
        return Ok(None);
    };
    let calendar = crate::builtins::temporal::temporal_calendar_id(agent, value).to_string_lossy();
    // A plain value whose calendar is neither iso8601 nor the formatter's
    // calendar cannot be formatted (a RangeError, like formatRange).
    let check_calendar = || -> Result<(), JsError> {
        if calendar != "iso8601" && calendar != record_calendar {
            return Err(range_error(
                "calendar does not match the DateTimeFormat calendar",
            ));
        }
        Ok(())
    };
    match record {
        TemporalRecord::Instant(ns) => Ok(Some(FormatValue::Instant {
            epoch_ms: ns as f64 / 1_000_000.0,
        })),
        TemporalRecord::ZonedDateTime(..) => Err(type_error(
            "format() does not support Temporal.ZonedDateTime",
        )),
        TemporalRecord::PlainDate([y, m, d]) => {
            check_calendar()?;
            Ok(Some(FormatValue::Plain {
                kind: PlainKind::Date,
                epoch_ms: plain_epoch_ms(y, m, d, 12, 0, 0, 0, 0, 0),
            }))
        }
        TemporalRecord::PlainTime([h, min, s, ms, us, ns]) => Ok(Some(FormatValue::Plain {
            kind: PlainKind::Time,
            epoch_ms: plain_epoch_ms(1970, 1, 1, h, min, s, ms, us, ns),
        })),
        TemporalRecord::PlainDateTime([y, m, d, h, min, s, ms, us, ns]) => {
            check_calendar()?;
            Ok(Some(FormatValue::Plain {
                kind: PlainKind::DateTime,
                epoch_ms: plain_epoch_ms(y, m, d, h, min, s, ms, us, ns),
            }))
        }
        TemporalRecord::YearMonth([y, m, d]) => {
            check_calendar()?;
            Ok(Some(FormatValue::Plain {
                kind: PlainKind::YearMonth,
                epoch_ms: plain_epoch_ms(y, m, d, 12, 0, 0, 0, 0, 0),
            }))
        }
        TemporalRecord::MonthDay([y, m, d]) => {
            check_calendar()?;
            Ok(Some(FormatValue::Plain {
                kind: PlainKind::MonthDay,
                epoch_ms: plain_epoch_ms(y, m, d, 12, 0, 0, 0, 0, 0),
            }))
        }
        TemporalRecord::Duration(_) => Ok(None),
    }
}

/// The dateStyle component fields with their real styles (DateTimeStyleFormat
/// for the plain kinds, which regenerate the pattern via the matcher).
fn date_style_fields(style: &str) -> Vec<(String, String)> {
    let fields: &[(&str, &str)] = match style {
        "full" => &[
            ("weekday", "long"),
            ("month", "long"),
            ("day", "numeric"),
            ("year", "numeric"),
        ],
        "long" => &[("month", "long"), ("day", "numeric"), ("year", "numeric")],
        "medium" => &[("month", "short"), ("day", "numeric"), ("year", "numeric")],
        _ => &[
            ("month", "numeric"),
            ("day", "numeric"),
            ("year", "2-digit"),
        ],
    };
    fields
        .iter()
        .map(|(name, style)| (name.to_string(), style.to_string()))
        .collect()
}

/// Remove a trailing time-zone name field (and the literal before it) from a
/// pattern — the IsPlain formatting omits timeZoneName parts.
fn strip_trailing_tz(pattern: &str) -> String {
    let mut end = pattern.len();
    let bytes = pattern.as_bytes();
    while end > 0 && matches!(bytes[end - 1], b'z' | b'v' | b'V') {
        end -= 1;
    }
    if end == pattern.len() {
        return pattern.to_string();
    }
    while end > 0 && !bytes[end - 1].is_ascii_alphanumeric() {
        end -= 1;
    }
    pattern[..end].to_string()
}

/// The record used to format a plain Temporal value: the time zone becomes
/// UTC (the wall-clock fields are used directly) and the pattern is
/// regenerated from the kind's supported fields. A kind with no overlap
/// with the format throws a TypeError (e.g. a timeStyle-only formatter with
/// a PlainDate).
pub(crate) fn plain_record(
    record: &DateTimeFormatRecord,
    kind: PlainKind,
) -> Result<DateTimeFormatRecord, JsError> {
    let mut plain = record.clone();
    let has_date = kind.keeps("month");
    let has_time = kind.keeps("hour");
    let date_style = if has_date {
        record.date_style.as_deref()
    } else {
        None
    };
    let time_style = if has_time {
        record.time_style.as_deref()
    } else {
        None
    };
    let (pattern, pattern12) = if date_style.is_some() || time_style.is_some() {
        match kind {
            // YearMonth/MonthDay drop fields the date-style pattern cannot
            // express by string surgery, so the pattern is regenerated from
            // the filtered style fields.
            PlainKind::YearMonth | PlainKind::MonthDay => {
                let mut fields = date_style_fields(date_style.unwrap_or("short"));
                fields.retain(|(name, _)| kind.keeps(name));
                if fields.is_empty() {
                    return Err(type_error(
                        "format() does not support this Temporal value type",
                    ));
                }
                let mut bits = 0u16;
                for (name, _) in &fields {
                    if let Some((_, bit)) = COMPONENT_ORDER.iter().find(|(n, _)| n == name) {
                        bits |= bit;
                    }
                }
                let (pattern, pattern12, _) = match_format(&fields, bits, &record.hour_cycle, None);
                (pattern, pattern12)
            }
            _ => {
                let (pattern, pattern12, _) =
                    style_pattern(date_style, time_style, &mut Vec::new(), &record.hour_cycle);
                (
                    strip_trailing_tz(&pattern),
                    pattern12.map(|p| strip_trailing_tz(&p)),
                )
            }
        }
    } else {
        // The component/default path: filter the resolved fields.
        let filtered: Vec<(String, String)> = record
            .fields
            .iter()
            .filter(|(name, _)| kind.keeps(name))
            .cloned()
            .collect();
        let fractional = record
            .fractional_second_digits
            .filter(|_| kind.keeps_fractional());
        if filtered.is_empty() && fractional.is_none() {
            return Err(type_error(
                "format() does not support this Temporal value type",
            ));
        }
        let mut bits = 0u16;
        for (name, _) in &filtered {
            if let Some((_, bit)) = COMPONENT_ORDER.iter().find(|(n, _)| n == name) {
                bits |= bit;
            }
        }
        if fractional.is_some() {
            bits |= 1024;
        }
        let (pattern, pattern12, _) = match_format(&filtered, bits, &record.hour_cycle, fractional);
        plain.fields = filtered;
        plain.fractional_second_digits = fractional;
        (pattern, pattern12)
    };
    plain.pattern = pattern;
    plain.pattern12 = pattern12;
    plain.time_zone = "UTC".to_string();
    Ok(plain)
}

/// The component bit of a field name (0 when not a component).
fn component_bit(name: &str) -> u16 {
    COMPONENT_ORDER
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, bit)| *bit)
        .unwrap_or(0)
}

/// Whether a record has no explicit format components (the CreateDateTimeFormat
/// `needDefaults` computation for required="any", defaults="all": the date list
/// weekday/year/month/day, the time list dayPeriod/hour/minute/second plus
/// fractionalSecondDigits; era and timeZoneName never suppress the defaults).
fn need_component_defaults(record: &DateTimeFormatRecord) -> bool {
    if record.fractional_second_digits.is_some() {
        return false;
    }
    ![
        "weekday",
        "year",
        "month",
        "day",
        "dayPeriod",
        "hour",
        "minute",
        "second",
    ]
    .iter()
    .any(|name| record.explicit_bits & component_bit(name) != 0)
}

/// Re-resolve a record as if created with required="any", defaults="all" —
/// the Temporal-value branch of HandleDateTimeValue: the user's explicit
/// components keep their styles, the defaults fill in the missing date and
/// time fields, and the styles are kept unchanged (an Instant formats with
/// the full date+time default even when the formatter was created with the
/// date-only constructor default).
pub(crate) fn temporal_all_record(record: &DateTimeFormatRecord) -> DateTimeFormatRecord {
    if record.date_style.is_some() || record.time_style.is_some() {
        return record.clone();
    }
    let mut fields: Vec<(String, String)> = record
        .fields
        .iter()
        .filter(|(name, _)| component_bit(name) & record.explicit_bits != 0)
        .cloned()
        .collect();
    // The non-gregory calendar's era field is kept (a calendar addition,
    // not a user component).
    if record.fields.iter().any(|(name, _)| name == "era")
        && !fields.iter().any(|(name, _)| name == "era")
    {
        fields.push(("era".to_string(), "short".to_string()));
    }
    let mut bits = record.explicit_bits;
    if record.fields.iter().any(|(name, _)| name == "era") {
        bits |= 2;
    }
    if need_component_defaults(record) {
        for (name, bit) in [("year", 4), ("month", 8), ("day", 16)] {
            if !fields.iter().any(|(n, _)| n == name) {
                bits |= bit;
                fields.push((name.to_string(), "numeric".to_string()));
            }
        }
        for (name, bit) in [("hour", 32), ("minute", 64), ("second", 128)] {
            if !fields.iter().any(|(n, _)| n == name) {
                bits |= bit;
                fields.push((name.to_string(), "numeric".to_string()));
            }
        }
    }
    let (pattern, pattern12, _) = match_format(
        &fields,
        bits,
        &record.hour_cycle,
        record.fractional_second_digits,
    );
    let mut all = record.clone();
    all.fields = fields;
    all.pattern = pattern;
    all.pattern12 = pattern12;
    all
}

/// The literal separator in a literal run: the arabic numbering systems
/// use U+066B (the `٫` of `٢:٣٥:٠٦٫٧٨٩`).
fn literal_separator(literal: String, ns: &str) -> String {
    if ns == "arab" || ns == "arabext" {
        literal.replace('.', "٫")
    } else {
        literal
    }
}

/// The count of consecutive occurrences of `c` starting at the peeked
/// position (the pattern field width), consuming them from the iterator.
fn field_length(chars: &mut std::iter::Peekable<std::str::Chars>, c: char) -> u32 {
    let mut count = 1;
    while chars.peek() == Some(&c) {
        count += 1;
        chars.next();
    }
    count
}

/// The year with 2-digit (the last two digits) or full formatting.
fn format_field_year(local: &LocalTime, width: u32, ns: &str) -> String {
    let year = if local.year <= 0 {
        1 - local.year
    } else {
        local.year
    };
    if width == 2 {
        let text = year.to_string();
        let digits: String = text
            .chars()
            .rev()
            .take(2)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format_number(digits.parse().unwrap_or(0), 2, ns)
    } else {
        format_number(year, 1, ns)
    }
}

fn format_field_month(local: &LocalTime, width: u32, ns: &str) -> String {
    let index = (local.month - 1) as usize;
    match width {
        1 => format_number(local.month as i64, 1, ns),
        2 => format_number(local.month as i64, 2, ns),
        3 => MONTH_NAMES[1][index].to_string(),
        4 => MONTH_NAMES[0][index].to_string(),
        _ => MONTH_NAMES[2][index].to_string(),
    }
}

fn format_field_weekday(local: &LocalTime, width: u32) -> String {
    let index = local.weekday as usize;
    match width {
        3 => WEEKDAY_NAMES[1][index].to_string(),
        4 => WEEKDAY_NAMES[0][index].to_string(),
        _ => WEEKDAY_NAMES[2][index].to_string(),
    }
}

/// The 12-hour hour with the cycle (h11: 0-11, h12: 12,1-11).
fn format_hour(
    local: &LocalTime,
    hour_cycle: &str,
    _zero_based: bool,
    width: u32,
    ns: &str,
) -> String {
    let mut hour = local.hour % 12;
    if hour == 0 && hour_cycle == "h12" {
        hour = 12;
    }
    format_number(hour as i64, if width >= 2 { 2 } else { 1 }, ns)
}

/// The AM/PM form of the `a` pattern field.
fn format_am_pm(local: &LocalTime) -> String {
    if local.hour < 12 {
        "AM".to_string()
    } else {
        "PM".to_string()
    }
}

fn format_day_period(local: &LocalTime, record: &DateTimeFormatRecord) -> String {
    let index = day_period_index(local.hour);
    let narrow = record
        .fields
        .iter()
        .any(|(name, value)| name == "dayPeriod" && value == "narrow");
    if narrow {
        DAY_PERIOD_NARROW[index].to_string()
    } else {
        DAY_PERIOD_LONG[index].to_string()
    }
}

/// The era name of the gregorian era: short (`G`), long (`GGGG`), or
/// narrow (`GGGGG`).
fn format_era(local: &LocalTime, width: u32) -> String {
    let era_index = if local.year <= 0 { 0 } else { 1 };
    let table = if width >= 5 {
        ERA_NAMES[2]
    } else if width >= 4 {
        ERA_NAMES[0]
    } else {
        ERA_NAMES[1]
    };
    table[era_index].to_string()
}

/// The time-zone name: UTC (or the long form for `zzzz`), the offset zones
/// as `GMT±H:MM` (padded for the long forms), the fixed Etc/GMT±N zones, or
/// the identifier itself.
fn format_time_zone(record: &DateTimeFormatRecord, width: u32) -> String {
    let zone = record.time_zone.as_str();
    if zone == "UTC" || zone == "Etc/UTC" || zone == "Etc/GMT" {
        if width >= 4 {
            return "Coordinated Universal Time".to_string();
        }
        return "UTC".to_string();
    }
    // The offset zones (stored as `±HH:MM`) and the fixed Etc/GMT±N zones
    // (constant offsets — the GMT±H display).
    let fixed_minutes = parse_offset_time_zone(zone).or_else(|| {
        zone.strip_prefix("Etc/GMT")
            .and_then(|rest| rest.parse::<i64>().ok())
            .map(|hours| -hours * 60)
    });
    if let Some(minutes) = fixed_minutes {
        if minutes == 0 {
            // A zero offset formats as the plain GMT name (the corpus
            // `offset-time-zones.js` pins no `+`/`-` for +00:00).
            return if width >= 4 {
                "Greenwich Mean Time".to_string()
            } else {
                "GMT".to_string()
            };
        }
        let sign = if minutes < 0 { '-' } else { '+' };
        let absolute = minutes.abs();
        let hours = absolute / 60;
        let minutes_part = absolute % 60;
        return if width >= 4 {
            format!("GMT{sign}{hours:02}:{minutes_part:02}")
        } else if minutes_part == 0 {
            format!("GMT{sign}{hours}")
        } else {
            format!("GMT{sign}{hours}:{minutes_part:02}")
        };
    }
    zone.to_string()
}

fn format_fractional(local: &LocalTime, record: &DateTimeFormatRecord) -> String {
    let digits = record.fractional_second_digits.unwrap_or(3);
    let value = local.millisecond as f64 * 10f64.powi(digits as i32 - 3);
    let text = format!("{:0width$.0}", value.floor(), width = digits as usize);
    number_format::transliterate(&record.numbering_system, &text)
}

/// The digits of a number field, zero-padded and transliterated to the
/// numbering system.
fn format_number(value: i64, min_digits: u32, ns: &str) -> String {
    let text = if min_digits > 1 {
        format!("{value:0>width$}", width = min_digits as usize)
    } else {
        value.to_string()
    };
    number_format::transliterate(ns, &text)
}

/// Install `Intl.DateTimeFormat` onto `%Intl%`.
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
        Some(JsString::from_utf8("DateTimeFormat")),
        0,
        placeholder("Intl.DateTimeFormat"),
        Some(placeholder("Intl.DateTimeFormat")),
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
    let methods: &[(&str, &str, u64)] = &[
        ("resolvedOptions", DTF_RESOLVED_OPTIONS, 0),
        ("formatToParts", DTF_FORMAT_TO_PARTS, 1),
        ("formatRange", DTF_FORMAT_RANGE, 2),
        ("formatRangeToParts", DTF_FORMAT_RANGE_TO_PARTS, 2),
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
        .define(DTF_FORMAT_GETTER, Value::Function(format_getter.clone()));
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
    // %Intl.DateTimeFormat.prototype%[@@toStringTag] = "Intl.DateTimeFormat".
    proto.define_property_key(
        &PropertyKey::Symbol(crux::symbol::well_known("toStringTag").as_ref().clone()),
        &PropertyDescriptor {
            value: Some(Value::String(Handle::new(JsString::from_utf8(
                "Intl.DateTimeFormat",
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
    let supported = Function::create_builtin(
        Some(JsString::from_utf8("supportedLocalesOf")),
        1,
        placeholder("supportedLocalesOf"),
        None,
        function_proto.clone(),
    )?;
    realm
        .intrinsics
        .define(DTF_SUPPORTED_LOCALES_OF, Value::Function(supported.clone()));
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
    realm.intrinsics.define(DATE_TIME_FORMAT_PROTO, proto_value);
    realm
        .intrinsics
        .define(DATE_TIME_FORMAT, Value::Function(ctor.clone()));
    if let Some(obj) = as_object(intl_value) {
        obj.define_property(
            &JsString::from_utf8("DateTimeFormat"),
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

/// The record of `this` (RequireInternalSlot).
fn date_time_format_record(agent: &Agent, this: &Value) -> Result<DateTimeFormatRecord, JsError> {
    let Some(obj) = as_object(this) else {
        return Err(type_error("Not a DateTimeFormat instance"));
    };
    agent
        .intl_date_time_format_data
        .get(&obj.id())
        .cloned()
        .ok_or_else(|| type_error("Not a DateTimeFormat instance"))
}

/// The `[[FallbackSymbol]]` used by the legacy constructor mode (shared
/// with NumberFormat — it is `%Intl%.[[FallbackSymbol]]`).
fn fallback_symbol() -> crux::symbol::Symbol {
    number_format::fallback_symbol()
}

/// The dateTimeFormat record after the legacy-constructor unwrap.
fn unwrap_date_time_format(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    if let Some(obj) = as_object(this) {
        if agent.intl_date_time_format_data.contains_key(&obj.id()) {
            return Ok(this.clone());
        }
        if let Ok(inner) = crate::context::get_property_key(
            agent,
            &Value::Object(obj.clone()),
            &PropertyKey::Symbol(fallback_symbol()),
            Value::Object(obj.clone()),
        ) && as_object(&inner).is_some()
        {
            return Ok(inner);
        }
    }
    Err(type_error("Not a DateTimeFormat instance"))
}

/// GetPrototypeFromConstructor: the newTarget's `prototype`, falling back to
/// %Intl.DateTimeFormat.prototype% of the newTarget's realm.
fn proto_from_ctor(agent: &mut Agent, new_target: &Value) -> Result<Handle<JsObject>, JsError> {
    let proto = get_property(
        agent,
        new_target,
        &JsString::from_utf8("prototype"),
        new_target.clone(),
    )?;
    if let Some(obj) = as_object(&proto) {
        return Ok(obj);
    }
    crate::context::get_function_realm(agent, new_target)?
        .intrinsics
        .get(DATE_TIME_FORMAT_PROTO)
        .and_then(|value| as_object(&value))
        .ok_or_else(|| type_error("%Intl.DateTimeFormat.prototype% missing"))
}

fn create_instance(
    agent: &mut Agent,
    proto: Handle<JsObject>,
    record: DateTimeFormatRecord,
) -> Result<Value, JsError> {
    let instance = JsObject::ordinary_object_create(Some(proto));
    agent
        .intl_date_time_format_data
        .insert(instance.id(), record);
    Ok(Value::Object(instance))
}

/// Intl.DateTimeFormat.prototype.format (ECMA-402 §11.3.3): the cached
/// bound function (a dispatched placeholder registered in
/// `intl_dtf_format_functions`).
fn format_getter(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let dtf = unwrap_date_time_format(agent, this)?;
    let mut record = date_time_format_record(agent, &dtf)?;
    if let Some(bound) = &record.bound_format {
        return Ok(bound.clone());
    }
    let Some(obj) = as_object(&dtf) else {
        return Err(type_error("Not a DateTimeFormat instance"));
    };
    let dtf_id = obj.id();
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
    agent.intl_dtf_format_functions.insert(func.id(), dtf_id);
    let bound = Value::Function(func);
    record.bound_format = Some(bound.clone());
    agent.intl_date_time_format_data.insert(dtf_id, record);
    Ok(bound)
}

/// The bound format function body: format the argument.
fn format_bound(agent: &mut Agent, dtf_id: u64, args: &[Value]) -> Result<Value, JsError> {
    let record = agent
        .intl_date_time_format_data
        .get(&dtf_id)
        .cloned()
        .ok_or_else(|| type_error("Not a DateTimeFormat instance"))?;
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    let (record, epoch_ms) = resolve_format_value(agent, &record, &value)?;
    let parts = format_date_time_pattern(&record, epoch_ms);
    let mut result = String::new();
    for part in &parts {
        result.push_str(&part.value);
    }
    Ok(Value::String(Handle::new(JsString::from_utf8(&result))))
}

/// Resolve a DateTimeFormat value argument: undefined → the current
/// date-time; a Temporal value → the instant/plain path; otherwise →
/// ToNumber + TimeClip (the legacy behavior). Returns the record to format
/// with (the plain record for plain Temporal kinds) and the epoch ms.
fn resolve_format_value(
    agent: &mut Agent,
    record: &DateTimeFormatRecord,
    value: &Value,
) -> Result<(DateTimeFormatRecord, f64), JsError> {
    // A missing/undefined argument formats the current date-time (the
    // DateTime Format Functions spec: Date.now() when undefined).
    if value.is_undefined() {
        return Ok((record.clone(), crate::builtins::date::now_ms()));
    }
    if let Some(format_value) = handle_datetime_value(agent, value, &record.calendar)? {
        // A Temporal value formats with the (any/all) re-resolution: the
        // user's explicit components plus the date+time defaults (an Instant
        // keeps the full record; the plain kinds filter it further).
        let record = temporal_all_record(record);
        return match format_value {
            FormatValue::Instant { epoch_ms } => Ok((record, clip_epoch_ms(epoch_ms)?)),
            FormatValue::Plain { kind, epoch_ms, .. } => {
                Ok((plain_record(&record, kind)?, epoch_ms))
            }
        };
    }
    Ok((record.clone(), clip_epoch_ms(to_number(agent, value)?)?))
}

/// The type tag of a formatRange argument (HandleDateTimeValue): "number"
/// for a legacy Date or plain Number, the Temporal type name otherwise.
fn range_value_tag(agent: &mut Agent, value: &Value) -> Result<String, JsError> {
    let ValueKind::Object(obj) = value.kind() else {
        return Ok("number".to_string());
    };
    let Some(record) = agent.temporal_data.get(&obj.id()).cloned() else {
        return Ok("number".to_string());
    };
    Ok(match record {
        TemporalRecord::Instant(_) => "instant".to_string(),
        TemporalRecord::ZonedDateTime(..) => {
            return Err(type_error(
                "formatRange does not support Temporal.ZonedDateTime",
            ));
        }
        TemporalRecord::PlainDate(_) => "date".to_string(),
        TemporalRecord::PlainTime(_) => "time".to_string(),
        TemporalRecord::PlainDateTime(_) => "datetime".to_string(),
        TemporalRecord::YearMonth(_) => "yearmonth".to_string(),
        TemporalRecord::MonthDay(_) => "monthday".to_string(),
        TemporalRecord::Duration(_) => "number".to_string(),
    })
}

/// Resolve a formatRange argument pair: both values must be the same type
/// (a legacy Date/Number, an Instant, or one plain kind — the corpus pins
/// the distinct-type TypeError) and each plain value's calendar must match
/// the formatter's (a RangeError otherwise).
fn resolve_range_values(
    agent: &mut Agent,
    record: &DateTimeFormatRecord,
    start_arg: &Value,
    end_arg: &Value,
) -> Result<(DateTimeFormatRecord, f64, f64), JsError> {
    let start_tag = range_value_tag(agent, start_arg)?;
    let end_tag = range_value_tag(agent, end_arg)?;
    if start_tag != end_tag {
        return Err(type_error("formatRange arguments must be of the same type"));
    }
    let (start_record, start) = resolve_format_value(agent, record, start_arg)?;
    let (_end_record, end) = resolve_format_value(agent, record, end_arg)?;
    Ok((start_record, start, end))
}

/// Intl.DateTimeFormat.prototype.formatToParts (ECMA-402 §11.3.4).
fn format_to_parts_method(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let dtf = unwrap_date_time_format(agent, this)?;
    let record = date_time_format_record(agent, &dtf)?;
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    let (record, epoch_ms) = resolve_format_value(agent, &record, &value)?;
    let parts = format_date_time_pattern(&record, epoch_ms);
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
                    &part.part_type,
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

/// Intl.DateTimeFormat.prototype.resolvedOptions (ECMA-402 §11.3.2).
/// The resolved component keys in Table 15 order (fractionalSecondDigits
/// between second and timeZoneName).
const RESOLVED_COMPONENT_ORDER: &[&str] = &[
    "weekday",
    "era",
    "year",
    "month",
    "day",
    "dayPeriod",
    "hour",
    "minute",
    "second",
    "fractionalSecondDigits",
    "timeZoneName",
];

/// The resolved component value derived from the pattern field width
/// (e.g. a month `MMM` field resolves to "short", `HH` hours to
/// "2-digit").
fn resolved_field_value(pattern: &str, name: &str) -> Option<&'static str> {
    let width = match name {
        "weekday" => pattern_width(pattern, "Eec"),
        "era" => pattern_width(pattern, "G"),
        "year" => pattern_width(pattern, "y"),
        "month" => pattern_width(pattern, "ML"),
        "day" => pattern_width(pattern, "d"),
        "hour" => pattern_width(pattern, "hHKk"),
        "minute" => pattern_width(pattern, "m"),
        "second" => pattern_width(pattern, "s"),
        _ => return None,
    };
    Some(match name {
        "weekday" => match width {
            3 => "short",
            4 => "long",
            _ => "narrow",
        },
        "era" => match width {
            4 => "long",
            5.. => "narrow",
            _ => "short",
        },
        "year" => {
            if width == 2 {
                "2-digit"
            } else {
                "numeric"
            }
        }
        "month" => match width {
            2 => "2-digit",
            3 => "short",
            4 => "long",
            5.. => "narrow",
            _ => "numeric",
        },
        "day" | "hour" | "minute" | "second" => {
            if width >= 2 {
                "2-digit"
            } else {
                "numeric"
            }
        }
        _ => return None,
    })
}

/// Intl.DateTimeFormat.prototype.resolvedOptions (ECMA-402 §11.3.2).
fn resolved_options_method(agent: &mut Agent, this: &Value) -> Result<Value, JsError> {
    let dtf = unwrap_date_time_format(agent, this)?;
    let record = date_time_format_record(agent, &dtf)?;
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
    let str = |s: &str| Value::String(Handle::new(JsString::from_utf8(s)));
    define("locale", Some(str(&record.locale)))?;
    define("calendar", Some(str(&record.calendar)))?;
    define("numberingSystem", Some(str(&record.numbering_system)))?;
    define("timeZone", Some(str(&record.time_zone)))?;
    let pattern = record.active_pattern();
    let has_hour = pattern.chars().any(|c| HOUR_CHARS.contains(c));
    let has_styles = record.date_style.is_some() || record.time_style.is_some();
    // The hourCycle/hour12 pair: present only when the format includes an
    // hour field (the default date format exposes neither).
    if has_hour {
        define("hourCycle", Some(str(&record.hour_cycle)))?;
        define(
            "hour12",
            Some(Value::Boolean(
                record.hour_cycle == "h11" || record.hour_cycle == "h12",
            )),
        )?;
    }
    // The component fields (Table 15 order): suppressed when dateStyle or
    // timeStyle are present, with the values derived from the pattern.
    // fractionalSecondDigits sits between second and timeZoneName.
    if !has_styles {
        for name in RESOLVED_COMPONENT_ORDER {
            if *name == "fractionalSecondDigits" {
                if let Some(digits) = record.fractional_second_digits {
                    define("fractionalSecondDigits", Some(Value::Number(digits as f64)))?;
                }
                continue;
            }
            if !record.fields.iter().any(|(n, _)| n == name) {
                continue;
            }
            let value = match *name {
                "dayPeriod" => Some(str(record
                    .fields
                    .iter()
                    .find(|(n, _)| n == "dayPeriod")
                    .map(|(_, v)| v.as_str())
                    .unwrap_or("short"))),
                "timeZoneName" => record
                    .fields
                    .iter()
                    .find(|(n, _)| n == "timeZoneName")
                    .map(|(_, v)| str(v.as_str())),
                _ => resolved_field_value(pattern, name).map(str),
            };
            define(name, value)?;
        }
    }
    if let Some(date_style) = &record.date_style {
        define("dateStyle", Some(str(date_style)))?;
    }
    if let Some(time_style) = &record.time_style {
        define("timeStyle", Some(str(time_style)))?;
    }
    Ok(Value::Object(options))
}

/// Intl.DateTimeFormat.supportedLocalesOf (ECMA-402 §11.2.2).
fn supported_locales_of(
    agent: &mut Agent,
    locales: Value,
    options: Value,
) -> Result<Value, JsError> {
    let requested = crate::builtins::intl::canonicalize_locale_list(agent, &locales)?;
    let options = number_format::coerce_options_to_object(agent, &options)?;
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
        let base = number_format::strip_unicode_extension(locale);
        if number_format::best_fit(available, &base).is_some() {
            subset.push(Value::String(Handle::new(JsString::from_utf8(locale))));
        }
    }
    crate::builtins::array::array_from_values(agent, &subset)
}

/// The en-US range separator: U+2009 THIN SPACE, U+2013 EN DASH, U+2009
/// (the corpus computes it from `formatRangeToParts` and only requires
/// `formatRange`/`formatRangeToParts` consistency).
const RANGE_SEPARATOR: &str = "\u{2009}\u{2013}\u{2009}";

/// A range-pattern part: a sub-pattern formatted against the start or end
/// date, or a shared (separator/connector) literal.
struct RangePart {
    pattern: String,
    source: &'static str,
}

/// The CLDR en-US `rangePatterns` for a date pattern: the collapsed forms
/// for the same-day/month/year cases.
struct RangeTemplates {
    day: Vec<RangePart>,
    month: Vec<RangePart>,
    year: Vec<RangePart>,
}

fn full_range_parts(pattern: &str) -> Vec<RangePart> {
    vec![
        RangePart {
            pattern: pattern.to_string(),
            source: "startRange",
        },
        RangePart {
            pattern: RANGE_SEPARATOR.to_string(),
            source: "shared",
        },
        RangePart {
            pattern: pattern.to_string(),
            source: "endRange",
        },
    ]
}

/// The collapse templates for the named-month date patterns. For
/// "MMM d, y"/"MMMM d, y" the same-month range shares the month name once
/// ("Jan 3 – 5, 2019") and the same-year range shares the year ("Jan 3 –
/// Mar 4, 2019"); the weekday-bearing full pattern repeats both sides (the
/// weekday changes across the range).
fn range_templates_for(pattern: &str) -> RangeTemplates {
    let joined = RangeTemplates {
        day: full_range_parts(pattern),
        month: full_range_parts(pattern),
        year: full_range_parts(pattern),
    };
    let month_day = |month_field: &str| RangeTemplates {
        day: vec![
            RangePart {
                pattern: format!("{month_field} "),
                source: "shared",
            },
            RangePart {
                pattern: "d".to_string(),
                source: "startRange",
            },
            RangePart {
                pattern: RANGE_SEPARATOR.to_string(),
                source: "shared",
            },
            RangePart {
                pattern: "d".to_string(),
                source: "endRange",
            },
            RangePart {
                pattern: ", y".to_string(),
                source: "shared",
            },
        ],
        month: vec![
            RangePart {
                pattern: format!("{month_field} d"),
                source: "startRange",
            },
            RangePart {
                pattern: RANGE_SEPARATOR.to_string(),
                source: "shared",
            },
            RangePart {
                pattern: format!("{month_field} d"),
                source: "endRange",
            },
            RangePart {
                pattern: ", y".to_string(),
                source: "shared",
            },
        ],
        year: full_range_parts(pattern),
    };
    let weekday_full = |date_prefix: &str| RangeTemplates {
        day: vec![
            RangePart {
                pattern: date_prefix.to_string(),
                source: "startRange",
            },
            RangePart {
                pattern: RANGE_SEPARATOR.to_string(),
                source: "shared",
            },
            RangePart {
                pattern: date_prefix.to_string(),
                source: "endRange",
            },
            RangePart {
                pattern: ", y".to_string(),
                source: "shared",
            },
        ],
        month: vec![
            RangePart {
                pattern: date_prefix.to_string(),
                source: "startRange",
            },
            RangePart {
                pattern: RANGE_SEPARATOR.to_string(),
                source: "shared",
            },
            RangePart {
                pattern: date_prefix.to_string(),
                source: "endRange",
            },
            RangePart {
                pattern: ", y".to_string(),
                source: "shared",
            },
        ],
        year: full_range_parts(pattern),
    };
    match pattern {
        "MMM d, y" => month_day("MMM"),
        "MMMM d, y" => month_day("MMMM"),
        "EEEE, MMMM d, y" => weekday_full("EEEE, MMMM d"),
        "EEE, MMM d, y" | "E, MMM d, y" => weekday_full("EEE, MMM d"),
        _ => joined,
    }
}

/// PartitionDateTimeRangePattern (ECMA-402 §11.1.8): the shared/collapsed
/// range parts for the en-US patterns (the corpus pins the day/month/year
/// collapse of `MMM d, y` and the full join of `M/d/y`).
fn partition_date_time_range(record: &DateTimeFormatRecord, start: f64, end: f64) -> Vec<DtfPart> {
    let start_parts = format_date_time_pattern(record, start);
    let end_parts = format_date_time_pattern(record, end);
    let start_string: String = start_parts.iter().map(|part| part.value.as_str()).collect();
    let end_string: String = end_parts.iter().map(|part| part.value.as_str()).collect();
    if start_string == end_string {
        return start_parts
            .into_iter()
            .map(|mut part| {
                part.source = Some("shared".to_string());
                part
            })
            .collect();
    }
    let start_local = to_local_time(start, &record.time_zone);
    let end_local = to_local_time(end, &record.time_zone);
    let templates = range_templates_for(record.active_pattern());
    // The spec's field-order selection: the collapse picks the coarsest
    // field that differs (year, then month, then day).
    let selected = if start_local.year != end_local.year {
        &templates.year
    } else if start_local.month != end_local.month {
        &templates.month
    } else if start_local.day != end_local.day {
        &templates.day
    } else {
        // Same local date but different formatted strings (a time range):
        // the full join.
        &templates.year
    };
    let mut parts = Vec::new();
    for range_part in selected {
        let epoch_ms = match range_part.source {
            "endRange" => end,
            _ => start,
        };
        let mut fragment_parts = format_with_pattern(record, epoch_ms, &range_part.pattern);
        for part in &mut fragment_parts {
            part.source = Some(range_part.source.to_string());
        }
        parts.append(&mut fragment_parts);
    }
    parts
}

/// Intl.DateTimeFormat.prototype.formatRange (ECMA-402 §11.3.5).
fn format_range_method(agent: &mut Agent, this: &Value, args: &[Value]) -> Result<Value, JsError> {
    let dtf = unwrap_date_time_format(agent, this)?;
    let record = date_time_format_record(agent, &dtf)?;
    let start_arg = args.first().cloned().unwrap_or(Value::Undefined);
    let end_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    if start_arg.is_undefined() || end_arg.is_undefined() {
        return Err(type_error("formatRange requires two date arguments"));
    }
    let (record, start, end) = resolve_range_values(agent, &record, &start_arg, &end_arg)?;
    let parts = partition_date_time_range(&record, start, end);
    let mut result = String::new();
    for part in &parts {
        result.push_str(&part.value);
    }
    Ok(Value::String(Handle::new(JsString::from_utf8(&result))))
}

/// Intl.DateTimeFormat.prototype.formatRangeToParts (ECMA-402 §11.3.6):
/// the partitioned range parts (each with its `source`).
fn format_range_to_parts_method(
    agent: &mut Agent,
    this: &Value,
    args: &[Value],
) -> Result<Value, JsError> {
    let dtf = unwrap_date_time_format(agent, this)?;
    let record = date_time_format_record(agent, &dtf)?;
    let start_arg = args.first().cloned().unwrap_or(Value::Undefined);
    let end_arg = args.get(1).cloned().unwrap_or(Value::Undefined);
    if start_arg.is_undefined() || end_arg.is_undefined() {
        return Err(type_error("formatRangeToParts requires two date arguments"));
    }
    let (record, start, end) = resolve_range_values(agent, &record, &start_arg, &end_arg)?;
    let parts = partition_date_time_range(&record, start, end);
    let object_proto = agent
        .current_realm()?
        .intrinsics
        .get("%Object.prototype%")
        .and_then(|value| as_object(&value));
    let part_object = |part: &DtfPart| -> Result<Value, JsError> {
        let obj = JsObject::ordinary_object_create(object_proto.clone());
        obj.define_property(
            &JsString::from_utf8("type"),
            &PropertyDescriptor {
                value: Some(Value::String(Handle::new(JsString::from_utf8(
                    &part.part_type,
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
        if let Some(source) = &part.source {
            obj.define_property(
                &JsString::from_utf8("source"),
                &PropertyDescriptor {
                    value: Some(Value::String(Handle::new(JsString::from_utf8(source)))),
                    writable: Some(true),
                    get: None,
                    set: None,
                    enumerable: Some(true),
                    configurable: Some(true),
                },
            )?;
        }
        Ok(Value::Object(obj))
    };
    let mut array = Vec::new();
    for part in &parts {
        array.push(part_object(part)?);
    }
    crate::builtins::array::array_from_values(agent, &array)
}

/// Date.prototype.toLocaleString/toLocaleDateString/toLocaleTimeString
/// (ECMA-262 §21.4.4.21-23): construct a DateTimeFormat with the given
/// (required, defaults) and format the Date value. A NaN date value
/// formats as "Invalid Date" (the DateToString convention).
pub fn to_locale_string(
    agent: &mut Agent,
    locales: &Value,
    options: &Value,
    epoch_ms: f64,
    required: &str,
    defaults: &str,
) -> Result<String, JsError> {
    if epoch_ms.is_nan() {
        return Ok("Invalid Date".to_string());
    }
    let record = initialize(agent, locales, options, required, defaults)?;
    let parts = format_date_time_pattern(&record, epoch_ms);
    let mut result = String::new();
    for part in &parts {
        result.push_str(&part.value);
    }
    Ok(result)
}

/// Temporal.ZonedDateTime.prototype.toLocaleString (spec 6.5.6): a
/// timeZone option is rejected (even when it agrees with the instance's
/// zone), the formatter uses the instance's own time zone, the value's
/// calendar must match the locale's (a RangeError when it is neither
/// iso8601 nor the locale calendar), and the no-options default includes
/// the time zone name (the corpus's `timeZoneName: "short"` default).
pub fn zoned_to_locale_string(
    agent: &mut Agent,
    locales: &Value,
    options: &Value,
    epoch_ns: i128,
    zone: &str,
    calendar: &str,
) -> Result<String, JsError> {
    let options_obj = number_format::coerce_options_to_object(agent, options)?;
    let time_zone = get_property(
        agent,
        &options_obj,
        &JsString::from_utf8("timeZone"),
        options_obj.clone(),
    )?;
    if !time_zone.is_undefined() {
        return Err(type_error(
            "timeZone option is not allowed for ZonedDateTime.toLocaleString",
        ));
    }
    let mut record = initialize(agent, locales, &options_obj, "any", "all")?;
    // The calendar must match the locale calendar when it is not iso8601
    // (the `calendar-mismatch` corpus fixture).
    if calendar != "iso8601" && calendar != record.calendar {
        return Err(range_error("calendar does not match the locale calendar"));
    }
    record.time_zone = zone.to_string();
    // The default format includes the zone name (a component formatter with
    // timeZoneName "short"); dateStyle/timeStyle records keep their style,
    // and any explicit component suppresses the default (the lone-options
    // and era fixtures pin `{year: "numeric"}`/`{era: "narrow"}` format
    // without the zone name).
    if record.date_style.is_none()
        && record.time_style.is_none()
        && !record.fields.iter().any(|(name, _)| name == "timeZoneName")
        && record.explicit_bits == 0
    {
        record
            .fields
            .push(("timeZoneName".to_string(), "short".to_string()));
        let mut bits = 0u16;
        for (name, _) in &record.fields {
            if let Some((_, bit)) = COMPONENT_ORDER.iter().find(|(n, _)| n == name) {
                bits |= bit;
            }
        }
        let (pattern, pattern12, _) = match_format(
            &record.fields,
            bits,
            &record.hour_cycle,
            record.fractional_second_digits,
        );
        record.pattern = pattern;
        record.pattern12 = pattern12;
    }
    let epoch_ms = clip_epoch_ms(epoch_ns as f64 / 1_000_000.0)?;
    let parts = format_date_time_pattern(&record, epoch_ms);
    let mut result = String::new();
    for part in &parts {
        result.push_str(&part.value);
    }
    Ok(result)
}

/// The Temporal.Plain* toLocaleString methods: build a DateTimeFormat with
/// the given (required, defaults), verify the value's calendar matches the
/// formatter's (an iso8601 value is always accepted), and format the plain
/// wall-clock fields.
pub(crate) fn plain_to_locale_string(
    agent: &mut Agent,
    locales: &Value,
    options: &Value,
    value: &Value,
    kind: PlainKind,
    required: &str,
    defaults: &str,
) -> Result<String, JsError> {
    let record = initialize(agent, locales, options, required, defaults)?;
    let calendar = crate::builtins::temporal::temporal_calendar_id(agent, value).to_string_lossy();
    if calendar != "iso8601" && calendar != record.calendar {
        return Err(range_error("calendar does not match the locale calendar"));
    }
    let format_value = handle_datetime_value(agent, value, &record.calendar)?
        .ok_or_else(|| type_error("Not a Temporal plain value"))?;
    let FormatValue::Plain {
        kind: _, epoch_ms, ..
    } = format_value
    else {
        unreachable!("plain_to_locale_string with a non-plain kind")
    };
    let record = plain_record(&record, kind)?;
    let parts = format_date_time_pattern(&record, epoch_ms);
    let mut result = String::new();
    for part in &parts {
        result.push_str(&part.value);
    }
    Ok(result)
}

/// dispatch_call: the DateTimeFormat constructor (as a function — the
/// legacy chain), the prototype members, and supportedLocalesOf.
pub fn dispatch_call(
    agent: &mut Agent,
    callee: &Value,
    this: &Value,
    args: &[Value],
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    let intrinsics = &realm.intrinsics;
    if intrinsics.get(DATE_TIME_FORMAT).as_ref() == Some(callee) {
        return Some(construct_inner(agent, callee, this, true, args));
    }
    if intrinsics.get(DTF_SUPPORTED_LOCALES_OF).as_ref() == Some(callee) {
        return Some(supported_locales_of(
            agent,
            args.first().cloned().unwrap_or(Value::Undefined),
            args.get(1).cloned().unwrap_or(Value::Undefined),
        ));
    }
    if intrinsics.get(DTF_RESOLVED_OPTIONS).as_ref() == Some(callee) {
        return Some(resolved_options_method(agent, this));
    }
    if intrinsics.get(DTF_FORMAT_GETTER).as_ref() == Some(callee) {
        return Some(format_getter(agent, this));
    }
    if intrinsics.get(DTF_FORMAT_TO_PARTS).as_ref() == Some(callee) {
        return Some(format_to_parts_method(agent, this, args));
    }
    if intrinsics.get(DTF_FORMAT_RANGE).as_ref() == Some(callee) {
        return Some(format_range_method(agent, this, args));
    }
    if intrinsics.get(DTF_FORMAT_RANGE_TO_PARTS).as_ref() == Some(callee) {
        return Some(format_range_to_parts_method(agent, this, args));
    }
    // The per-instance bound format functions.
    if let ValueKind::Function(function) = callee.kind()
        && let Some(dtf_id) = agent.intl_dtf_format_functions.get(&function.id()).copied()
    {
        return Some(format_bound(agent, dtf_id, args));
    }
    None
}

/// dispatch_construct: `new Intl.DateTimeFormat(...)`.
pub fn dispatch_construct(
    agent: &mut Agent,
    callee: &Value,
    args: &[Value],
    new_target: &Value,
) -> Option<Result<Value, JsError>> {
    let realm = agent.current_realm().ok()?;
    if realm.intrinsics.get(DATE_TIME_FORMAT).as_ref() == Some(callee) {
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

/// The shared constructor path: `new` (new_target present) and the legacy
/// function-call chain (new_target was undefined).
fn construct_inner(
    agent: &mut Agent,
    new_target: &Value,
    this: &Value,
    new_target_was_undefined: bool,
    args: &[Value],
) -> Result<Value, JsError> {
    let proto = proto_from_ctor(agent, new_target)?;
    let locales = args.first().cloned().unwrap_or(Value::Undefined);
    let options = args.get(1).cloned().unwrap_or(Value::Undefined);
    let record = initialize(agent, &locales, &options, "any", "date")?;
    if new_target_was_undefined
        && let Some(this_obj) = as_object(this)
        && ordinary_has_instance(agent, this)
    {
        let inner = create_instance(agent, proto, record)?;
        this_obj.define_property_key(
            &PropertyKey::Symbol(fallback_symbol()),
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
    create_instance(agent, proto, record)
}

/// OrdinaryHasInstance for the legacy-constructor chain.
fn ordinary_has_instance(agent: &mut Agent, value: &Value) -> bool {
    let Some(proto) = agent
        .current_realm()
        .ok()
        .and_then(|realm| realm.intrinsics.get(DATE_TIME_FORMAT_PROTO))
        .and_then(|value| as_object(&value))
    else {
        return false;
    };
    let mut current = as_object(value);
    while let Some(obj) = current {
        if obj.id() == proto.id() {
            return true;
        }
        match obj.get_prototype_of() {
            Ok(Some(next)) => current = Some(next),
            _ => return false,
        }
    }
    false
}
