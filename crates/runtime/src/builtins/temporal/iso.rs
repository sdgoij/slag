//! ISO-8601 calendar math and the RFC 9557 / ISO 8601 string grammar
//! (Temporal spec 12.3, 13.31-13.37), shared by all Temporal types.
//!
//! All date math is exact integer arithmetic on epoch days; epoch
//! nanoseconds are `i128` (the representable range ±8.64×10^21 fits).

use crux::BigInt;

pub const NS_PER_SECOND: i128 = 1_000_000_000;
pub const NS_PER_MINUTE: i128 = 60 * NS_PER_SECOND;
pub const NS_PER_HOUR: i128 = 60 * NS_PER_MINUTE;
pub const NS_PER_DAY: i128 = 24 * NS_PER_HOUR;
pub const MS_PER_DAY: i128 = 86_400_000;
/// The Instant/ZonedDateTime epoch-nanosecond bound (spec 8.4.1).
pub const NS_MAX_INSTANT: i128 = 100_000_000 * NS_PER_DAY;
pub const NS_MIN_INSTANT: i128 = -NS_MAX_INSTANT;
/// The maximum time duration in nanoseconds: 2^53 × 10^9 - 1 (spec 7.5.3).
pub const MAX_TIME_DURATION: i128 = (1 << 53) * NS_PER_SECOND - 1;

// Code units used by the string grammar (patterns need consts, not casts).
const CU_T: u16 = 0x54;
const CU_T_LOWER: u16 = 0x74;
const CU_Z: u16 = 0x5A;
const CU_Z_LOWER: u16 = 0x7A;
const CU_PLUS: u16 = 0x2B;
const CU_MINUS: u16 = 0x2D;
const CU_DOT: u16 = 0x2E;
const CU_COMMA: u16 = 0x2C;
const CU_LBRACKET: u16 = 0x5B;
const CU_RBRACKET: u16 = 0x5D;
const CU_EQ: u16 = 0x3D;
const CU_BANG: u16 = 0x21;
const CU_COLON: u16 = 0x3A;
const CU_SPACE: u16 = 0x20;
const CU_P: u16 = 0x50;
const CU_0: u16 = 0x30;
const CU_9: u16 = 0x39;

fn is_digit(c: u16) -> bool {
    (CU_0..=CU_9).contains(&c)
}

/// spec 12.3.17 ISODaysInMonth / 3.5.8 IsValidISODate.
pub fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

pub fn days_in_month(year: i64, month: i64) -> i64 {
    const MONTH_DAYS: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if month == 2 && is_leap_year(year) {
        29
    } else {
        MONTH_DAYS[(month - 1) as usize]
    }
}

pub fn days_in_year(year: i64) -> i64 {
    if is_leap_year(year) { 366 } else { 365 }
}

pub fn is_valid_iso_date(year: i64, month: i64, day: i64) -> bool {
    if !(1..=12).contains(&month) {
        return false;
    }
    (1..=days_in_month(year, month)).contains(&day)
}

/// spec 13.1 ISODateToEpochDays (0-based month, matching the spec callers).
pub fn iso_date_to_epoch_days(year: i64, month0: i64, day: i64) -> i64 {
    let resolved_year = year + month0.div_euclid(12);
    let resolved_month = month0.rem_euclid(12);
    let year_days = 365 * (resolved_year - 1970) + (resolved_year - 1969).div_euclid(4)
        - (resolved_year - 1901).div_euclid(100)
        + (resolved_year - 1601).div_euclid(400);
    let month_days: i64 = (0..resolved_month)
        .map(|m| days_in_month(resolved_year, m + 1))
        .sum();
    year_days + month_days + day - 1
}

/// Inverse of `iso_date_to_epoch_days` (spec 13.3 date equations).
pub fn epoch_days_to_iso_date(epoch_days: i64) -> (i64, i64, i64) {
    let mut year = 1970 + (epoch_days as f64 / 365.2425).floor() as i64;
    while iso_date_to_epoch_days(year, 0, 1) > epoch_days {
        year -= 1;
    }
    while iso_date_to_epoch_days(year + 1, 0, 1) <= epoch_days {
        year += 1;
    }
    let mut day_in_year = epoch_days - iso_date_to_epoch_days(year, 0, 1) + 1;
    let mut month = 1;
    while day_in_year > days_in_month(year, month) {
        day_in_year -= days_in_month(year, month);
        month += 1;
    }
    (year, month, day_in_year)
}

/// spec 14.6.1 GetUTCEpochNanoseconds: epoch ns of a UTC wall-clock time.
#[allow(clippy::too_many_arguments)]
pub fn get_utc_epoch_nanoseconds(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    millisecond: i64,
    microsecond: i64,
    nanosecond: i64,
) -> i128 {
    let days = iso_date_to_epoch_days(year, month - 1, day);
    let ms = days as i128 * MS_PER_DAY
        + hour as i128 * 3_600_000
        + minute as i128 * 60_000
        + second as i128 * 1_000
        + millisecond as i128;
    ms * 1_000_000 + microsecond as i128 * 1_000 + nanosecond as i128
}

/// spec 11.1.2 GetISOPartsFromEpoch: UTC date-time from epoch nanoseconds.
pub fn iso_parts_from_epoch(epoch_ns: i128) -> (i64, i64, i64, i64, i64, i64, i64, i64, i64) {
    let remainder_ns = epoch_ns.rem_euclid(1_000_000);
    let ms_f = ((epoch_ns - remainder_ns) / 1_000_000) as f64;
    let (year, month, day) = epoch_days_to_iso_date((ms_f / 86_400_000.0).floor() as i64);
    let ms_i = ms_f as i64;
    let hour = ms_i.div_euclid(3_600_000).rem_euclid(24);
    let minute = ms_i.div_euclid(60_000).rem_euclid(60);
    let second = ms_i.div_euclid(1_000).rem_euclid(60);
    let millisecond = ms_i.rem_euclid(1_000);
    let microsecond = (remainder_ns / 1_000).rem_euclid(1_000) as i64;
    let nanosecond = remainder_ns.rem_euclid(1_000) as i64;
    (
        year,
        month,
        day,
        hour,
        minute,
        second,
        millisecond,
        microsecond,
        nanosecond,
    )
}

/// spec 12.3.18-12.3.20: week/yearday helpers.
pub fn iso_day_of_week(year: i64, month: i64, day: i64) -> i64 {
    let days = iso_date_to_epoch_days(year, month - 1, day);
    let dow = (days + 4).rem_euclid(7);
    if dow == 0 { 7 } else { dow }
}

pub fn iso_day_of_year(year: i64, month: i64, day: i64) -> i64 {
    iso_date_to_epoch_days(year, month - 1, day) - iso_date_to_epoch_days(year, 0, 1) + 1
}

/// spec 12.3.10 PadISOYear.
pub fn pad_iso_year(year: i64) -> String {
    if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else if year > 0 {
        format!("+{year:06}")
    } else {
        format!("-{:06}", year.abs())
    }
}

/// spec 13.25 FormatFractionalSeconds.
pub fn format_fractional_seconds(sub_second_ns: i64, precision: FracPrecision) -> String {
    match precision {
        FracPrecision::Auto => {
            if sub_second_ns == 0 {
                return String::new();
            }
            let mut s = format!("{sub_second_ns:09}");
            while s.ends_with('0') {
                s.pop();
            }
            format!(".{s}")
        }
        FracPrecision::Digits(n) => {
            if n == 0 {
                return String::new();
            }
            let s = format!("{sub_second_ns:09}");
            format!(".{}", &s[..n as usize])
        }
        FracPrecision::Minute => String::new(),
    }
}

/// The seconds-precision value shared by `toString` paths (spec 13.16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FracPrecision {
    /// "auto": no insignificant trailing zeroes.
    Auto,
    /// 0-9 digits after the seconds.
    Digits(u8),
    /// Seconds omitted entirely (smallestUnit "minute").
    Minute,
}

/// spec 13.26 FormatTimeString.
pub fn format_time_string(
    hour: i64,
    minute: i64,
    second: i64,
    sub_second_ns: i64,
    precision: FracPrecision,
) -> String {
    let hh = format!("{hour:02}");
    let mm = format!("{minute:02}");
    if precision == FracPrecision::Minute {
        return format!("{hh}:{mm}");
    }
    let ss = format!("{second:02}");
    let frac = format_fractional_seconds(sub_second_ns, precision);
    format!("{hh}:{mm}:{ss}{frac}")
}

// ---------------------------------------------------------------------------
// RFC 9557 / ISO 8601 parsing (spec 13.31-13.35)
// ---------------------------------------------------------------------------

/// The parsed time zone representation of a date-time string.
#[derive(Debug, Clone, Default)]
pub struct ParsedTz {
    pub z: bool,
    /// Offset string as written (e.g. "+01:30"), empty when absent.
    pub offset_string: String,
    /// The time-zone annotation identifier (e.g. "Asia/Kolkata"), empty when absent.
    pub annotation: String,
}

/// An ISO Date-Time Parse Record (spec 13.34).
#[derive(Debug, Clone)]
pub struct ParsedDateTime {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    /// `None` when the time was omitted (start-of-day).
    pub time: Option<[i64; 6]>, // hour, minute, second, ms, us, ns
    pub tz: ParsedTz,
    pub calendar: Option<String>,
}

#[derive(Debug)]
pub enum ParseError {
    Invalid,
}

/// The formats a caller may require (spec 13.35 `allowedFormats`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    InstantString,
    DateTimeZoned,
    DateTimePlain,
    YearMonthString,
    MonthDayString,
    TimeString,
}

struct Cursor<'a> {
    text: &'a [u16],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(text: &'a [u16]) -> Self {
        Self { text, pos: 0 }
    }
    fn peek(&self) -> Option<u16> {
        self.text.get(self.pos).copied()
    }
    fn bump(&mut self) -> Option<u16> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }
    fn at_end(&self) -> bool {
        self.pos >= self.text.len()
    }
    fn is_digit(&self) -> bool {
        matches!(self.peek(), Some(c) if is_digit(c))
    }
    fn eat(&mut self, c: u16) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, c: u16) -> Result<(), ParseError> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(ParseError::Invalid)
        }
    }
    fn digits(&mut self, count: usize) -> Result<i64, ParseError> {
        let mut value = 0i64;
        for _ in 0..count {
            let c = self.bump().ok_or(ParseError::Invalid)?;
            if !is_digit(c) {
                return Err(ParseError::Invalid);
            }
            value = value * 10 + (c - CU_0) as i64;
        }
        Ok(value)
    }
    fn number(&mut self) -> Result<i64, ParseError> {
        if !self.is_digit() {
            return Err(ParseError::Invalid);
        }
        let mut value = 0i64;
        while let Some(c) = self.peek() {
            if !is_digit(c) {
                break;
            }
            self.pos += 1;
            // A duration field beyond i64 is invalid either way; reject the
            // overflow instead of wrapping (test262 argument-string-is-infinity).
            value = value
                .checked_mul(10)
                .and_then(|v| v.checked_add((c - CU_0) as i64))
                .ok_or(ParseError::Invalid)?;
        }
        Ok(value)
    }
}

/// The decimal fraction of a time/offset part: 1-9 digits (more is an error,
/// spec grammar TimeFraction).
fn parse_fraction(cur: &mut Cursor) -> Result<(i64, usize), ParseError> {
    if !cur.eat(CU_DOT) && !cur.eat(CU_COMMA) {
        return Err(ParseError::Invalid);
    }
    let mut digits = 0usize;
    let mut value = 0i64;
    while let Some(c) = cur.peek() {
        if !is_digit(c) {
            break;
        }
        cur.pos += 1;
        digits += 1;
        value = value * 10 + (c - CU_0) as i64;
    }
    if digits == 0 || digits > 9 {
        return Err(ParseError::Invalid);
    }
    Ok((value, digits))
}

/// spec 14.6.11 ParseDateTimeUTCOffset.
pub fn parse_date_time_utc_offset(offset: &str) -> Result<i128, ParseError> {
    let units: Vec<u16> = offset.encode_utf16().collect();
    let mut cur = Cursor::new(&units);
    let sign = match cur.bump() {
        Some(CU_PLUS) => 1i128,
        Some(CU_MINUS) => -1i128,
        _ => return Err(ParseError::Invalid),
    };
    let hour = cur.digits(2)?;
    if hour > 23 {
        return Err(ParseError::Invalid);
    }
    if cur.at_end() {
        return Ok(sign * hour as i128 * NS_PER_HOUR);
    }
    let separated = cur.eat(CU_COLON);
    let minute = cur.digits(2)?;
    if minute > 59 {
        return Err(ParseError::Invalid);
    }
    let mut second = 0i64;
    let mut ns = 0i128;
    if !cur.at_end() {
        if separated {
            cur.expect(CU_COLON)?;
        }
        second = cur.digits(2)?;
        if second > 59 {
            return Err(ParseError::Invalid);
        }
        if !cur.at_end() {
            let (frac, digits) = parse_fraction(&mut cur)?;
            ns = frac as i128 * 10i128.pow((9 - digits) as u32);
        }
    }
    if !cur.at_end() {
        return Err(ParseError::Invalid);
    }
    Ok(sign * (((hour * 60 + minute) * 60 + second) as i128 * NS_PER_SECOND + ns))
}

/// spec 13.33/11.1.16 ParseTimeZoneIdentifier: `Ok(None)` is a named zone
/// (the input is the name), `Ok(Some(ns))` is an offset zone, `Err` is
/// syntactically invalid. The offset form is minute precision only
/// (UTCOffset[~SubMinutePrecision]: `±HH`, `±HH:MM`, `±HHMM`); the named form
/// is the IANA name character set (`TZLeadingChar`/`TZChar`, `/`-separated).
pub fn parse_time_zone_identifier(text: &str) -> Result<Option<i128>, ParseError> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut cur = Cursor::new(&units);
    match cur.peek() {
        Some(CU_PLUS) | Some(CU_MINUS) => {
            let sign = if cur.bump() == Some(CU_MINUS) {
                -1i128
            } else {
                1i128
            };
            let hour = cur.digits(2)?;
            if hour > 23 {
                return Err(ParseError::Invalid);
            }
            let mut minute = 0i64;
            if !cur.at_end() {
                let _separated = cur.eat(CU_COLON);
                minute = cur.digits(2)?;
                if minute > 59 {
                    return Err(ParseError::Invalid);
                }
            }
            if !cur.at_end() {
                return Err(ParseError::Invalid);
            }
            Ok(Some(sign * (hour * 60 + minute) as i128 * NS_PER_MINUTE))
        }
        _ => {
            if text.is_empty() {
                return Err(ParseError::Invalid);
            }
            // TimeZoneIANAName starts with a TZLeadingChar (alpha, '.', or
            // '_'); a leading digit is not a name (test262
            // timezone-string-datetime basic-format strings).
            let mut chars = text.chars();
            let first = chars.next().unwrap();
            if !(first.is_ascii_alphabetic() || matches!(first, '.' | '_')) {
                return Err(ParseError::Invalid);
            }
            let valid = chars
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '/'));
            if !valid {
                return Err(ParseError::Invalid);
            }
            Ok(None)
        }
    }
}

/// Parse the trailing `[annotation]` runs into the calendar and time-zone
/// fields (spec 13.35 annotation loop + the multiple-annotation rules).
fn parse_annotations(
    cur: &mut Cursor,
    tz: &mut ParsedTz,
    calendar: &mut Option<String>,
) -> Result<(), ParseError> {
    let mut calendar_count: u32 = 0;
    let mut calendar_critical = false;
    while cur.eat(CU_LBRACKET) {
        let critical = cur.eat(CU_BANG);
        // Collect until `=` or `]`; the content is either an annotation key
        // (with `=`) or a time-zone identifier (without).
        let mut content = String::new();
        while let Some(c) = cur.peek() {
            if c == CU_RBRACKET || c == CU_EQ {
                break;
            }
            content.push(char::from_u32(c as u32).unwrap());
            cur.pos += 1;
        }
        if content.is_empty() {
            return Err(ParseError::Invalid);
        }
        if cur.eat(CU_EQ) {
            // Annotation keys are lowercase-only: [a-z0-9_-]+ (spec grammar
            // AnnotationKey); anything else is a parse error.
            if !content
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
            {
                return Err(ParseError::Invalid);
            }
            let mut value = String::new();
            while let Some(c) = cur.peek() {
                if c == CU_RBRACKET {
                    break;
                }
                value.push(char::from_u32(c as u32).unwrap());
                cur.pos += 1;
            }
            if value.is_empty() {
                return Err(ParseError::Invalid);
            }
            if content == "u-ca" {
                calendar_count += 1;
                calendar_critical |= critical;
                if calendar_count > 1 && calendar_critical {
                    // More than one calendar annotation is only an error if
                    // any carries the critical flag (test262
                    // argument-string-multiple-calendar.js); the first
                    // annotation wins otherwise.
                    return Err(ParseError::Invalid);
                }
                if calendar.is_none() {
                    *calendar = Some(value);
                }
            } else if critical {
                return Err(ParseError::Invalid);
            }
        } else {
            // A time-zone annotation; at most one. The identifier must be a
            // valid TimeZoneIdentifier (minute-precision offset or IANA name)
            // per the TimeZoneAnnotation grammar — sub-minute offsets and
            // arbitrary text are parse errors.
            if !tz.annotation.is_empty() {
                return Err(ParseError::Invalid);
            }
            if parse_time_zone_identifier(&content).is_err() {
                return Err(ParseError::Invalid);
            }
            tz.annotation = content;
        }
        cur.expect(CU_RBRACKET)?;
    }
    Ok(())
}

/// spec 13.35 ParseISODateTime for the allowed formats.
pub fn parse_iso_date_time(text: &[u16], format: Format) -> Result<ParsedDateTime, ParseError> {
    let mut cur = Cursor::new(text);
    let mut year: i64;
    let month;
    let day;

    // Time-only form for TemporalTimeString: an optional T/t prefix, then
    // HH:MM[:SS[.f]] / HHMM[SS[.f]], with trailing offset/annotations. The Z
    // designator is rejected in finish_parse (spec 13.34).
    if format == Format::TimeString {
        if matches!(cur.peek(), Some(CU_T) | Some(CU_T_LOWER)) {
            cur.bump();
        }
        let t = parse_time(&mut cur)?;
        return finish_parse(&mut cur, format, 1972, 1, 1, Some(t), false);
    }

    // Month-day forms `--MM-DD` / `--MMDD` / `MM-DD` / `MMDD` for
    // TemporalMonthDayString (spec 13.33; the RFC 9557 short form, checked
    // before the year forms because a leading digit would otherwise parse as a
    // year).
    if format == Format::MonthDayString {
        cur.eat(CU_MINUS);
        cur.eat(CU_MINUS);
        month = cur.digits(2)?;
        cur.eat(CU_MINUS);
        day = cur.digits(2)?;
        if !is_valid_month_day(month, day) {
            return Err(ParseError::Invalid);
        }
        return finish_parse(&mut cur, format, 1972, month, day, None, false);
    }

    // DateYear: 4 digits, or sign + 6 digits ("-000000" is an early error).
    match cur.peek() {
        Some(CU_PLUS) | Some(CU_MINUS) => {
            let negative = cur.bump() == Some(CU_MINUS);
            year = cur.digits(6)?;
            if negative {
                if year == 0 {
                    return Err(ParseError::Invalid);
                }
                year = -year;
            }
        }
        Some(c) if is_digit(c) => {
            year = cur.digits(4)?;
        }
        _ => return Err(ParseError::Invalid),
    }

    // Date: `MM-DD` / `MMDD` with matching separators (spec DateSpec).
    let separated = cur.eat(CU_MINUS);
    if separated {
        month = cur.digits(2)?;
        if format == Format::YearMonthString && cur.peek() != Some(CU_MINUS) {
            return finish_parse(&mut cur, format, year, month, 1, None, true);
        }
        cur.expect(CU_MINUS)?;
        day = cur.digits(2)?;
    } else if cur.is_digit() {
        month = cur.digits(2)?;
        if cur.is_digit() {
            day = cur.digits(2)?;
        } else if format != Format::YearMonthString {
            // `YYYYMM` (no day) is only a valid year-month form.
            return Err(ParseError::Invalid);
        } else {
            return finish_parse(&mut cur, format, year, month, 1, None, true);
        }
    } else {
        return Err(ParseError::Invalid);
    }
    if !is_valid_iso_date(year, month, day) {
        return Err(ParseError::Invalid);
    }

    // Time part: `THH[:MM[:SS[.f]]]` / `THHMM[SS[.f]]`, `t`, or space.
    let time = if matches!(cur.peek(), Some(CU_T) | Some(CU_T_LOWER) | Some(CU_SPACE)) {
        cur.bump();
        Some(parse_time(&mut cur)?)
    } else {
        None
    };

    finish_parse(&mut cur, format, year, month, day, time, false)
}

fn finish_parse(
    cur: &mut Cursor,
    format: Format,
    year: i64,
    month: i64,
    day: i64,
    time: Option<[i64; 6]>,
    day_omitted: bool,
) -> Result<ParsedDateTime, ParseError> {
    // The RFC 9557 year-month form requires a 01-12 month (the polyfill's
    // yearmonth regex monthpart); the DateTimePlain fallback already checks
    // the full date.
    if format == Format::YearMonthString && !(1..=12).contains(&month) {
        return Err(ParseError::Invalid);
    }
    let mut tz = ParsedTz::default();
    let mut calendar = None;
    // Offset: `Z`/`z`, or `±HH[:MM[:SS[.f]]]` / `±HHMM[SS[.f]]`.
    match cur.peek() {
        Some(CU_Z) | Some(CU_Z_LOWER) => {
            cur.bump();
            tz.z = true;
        }
        Some(CU_PLUS) | Some(CU_MINUS) => {
            let start = cur.pos;
            cur.bump();
            let hour = cur.digits(2)?;
            if hour > 23 {
                return Err(ParseError::Invalid);
            }
            let separated = cur.eat(CU_COLON);
            if !cur.at_end() && cur.peek() != Some(CU_LBRACKET) {
                let minute = cur.digits(2)?;
                if minute > 59 {
                    return Err(ParseError::Invalid);
                }
                if !cur.at_end() && cur.peek() != Some(CU_LBRACKET) {
                    if separated {
                        cur.expect(CU_COLON)?;
                    }
                    let second = cur.digits(2)?;
                    if second > 59 {
                        return Err(ParseError::Invalid);
                    }
                    if !cur.at_end() && cur.peek() != Some(CU_LBRACKET) {
                        // The fraction only needs syntax validation here; the
                        // raw offset text is stored below.
                        parse_fraction(cur)?;
                    }
                }
            }
            // Keep the offset exactly as written: ParseTimeZoneIdentifier must
            // reject sub-minute offsets (e.g. "-07:00:00"), and the raw text
            // also drives the ZonedDateTime matchBehaviour rule.
            tz.offset_string = cur.text[start..cur.pos]
                .iter()
                .map(|&c| char::from_u32(c as u32).unwrap())
                .collect();
        }
        _ => {}
    }
    parse_annotations(cur, &mut tz, &mut calendar)?;
    if !cur.at_end() {
        return Err(ParseError::Invalid);
    }
    // RFC 9557 year-month: a non-default calendar annotation requires the
    // explicit day (test262 from/argument-string-invalid.js pins
    // "1976-11[u-ca=hebrew]" invalid while "1976-11[u-ca=iso8601]" and
    // "2019-12" stay valid). The month-day short form never carries a year,
    // so a non-default calendar is invalid there too (test262
    // from/argument-string-date-with-utc-offset.js pins "09-15[u-ca=chinese]"
    // while a full date "2022-09-15[u-ca=chinese]" parses via DateTimePlain).
    if (format == Format::YearMonthString && day_omitted || format == Format::MonthDayString)
        && calendar
            .as_deref()
            .is_some_and(|c| !c.eq_ignore_ascii_case("iso8601"))
    {
        return Err(ParseError::Invalid);
    }
    // An offset/designator only accompanies a time part, and an instant
    // string requires both the time and the offset.
    if time.is_none() && (tz.z || !tz.offset_string.is_empty()) {
        return Err(ParseError::Invalid);
    }
    if format == Format::InstantString && (time.is_none() || (!tz.z && tz.offset_string.is_empty()))
    {
        return Err(ParseError::Invalid);
    }
    // AnnotatedDateTime[+Zoned] requires a time zone annotation, and
    // DateTime[~Z] forbids the Z designator (spec 13.31 grammar; the
    // relativeTo parse of a bare "…Z" string must fail both ways).
    if format == Format::DateTimeZoned && tz.annotation.is_empty() {
        return Err(ParseError::Invalid);
    }
    if format == Format::DateTimePlain && tz.z {
        return Err(ParseError::Invalid);
    }
    // TemporalTimeString never accepts the Z designator (spec 13.34).
    if format == Format::TimeString && tz.z {
        return Err(ParseError::Invalid);
    }
    Ok(ParsedDateTime {
        year,
        month,
        day,
        time,
        tz,
        calendar,
    })
}

/// spec 13.31.1 IsValidMonthDay.
fn is_valid_month_day(month: i64, day: i64) -> bool {
    if !(1..=12).contains(&month) {
        return false;
    }
    match (month, day) {
        (2, 30) => false,
        (2 | 4 | 6 | 9 | 11, 31) => false,
        _ => (1..=31).contains(&day),
    }
}

/// `HH[:MM[:SS[.f]]]` / `HHMM[SS[.f]]` with matching separators.
fn parse_time(cur: &mut Cursor) -> Result<[i64; 6], ParseError> {
    let hour = cur.digits(2)?;
    let mut minute = 0i64;
    let mut second = 0i64;
    let mut ms = 0i64;
    let mut us = 0i64;
    let mut ns = 0i64;
    let separated = cur.eat(CU_COLON);
    let mut second_present = false;
    if separated {
        minute = cur.digits(2)?;
        if cur.eat(CU_COLON) {
            second = cur.digits(2)?;
            second_present = true;
        }
    } else if cur.is_digit() {
        minute = cur.digits(2)?;
        if cur.is_digit() {
            second = cur.digits(2)?;
            second_present = true;
        }
    }
    if minute > 59 {
        return Err(ParseError::Invalid);
    }
    // Leap second: `60` clamps to 59 regardless of the minute (spec 13.35
    // ParseISODateTime: "If secondMV = 60, set secondMV to 59"; the
    // PlainTime/from leap-second fixtures use e.g. 12:30:60).
    if second == 60 {
        second = 59;
    } else if second > 59 {
        return Err(ParseError::Invalid);
    }
    // The Time grammar only allows a fractional part after seconds; a
    // fraction after minutes or hours (e.g. "05:07.123") is a parse error
    // (test262 relativeto-no-fractional-minutes-hours.js).
    if matches!(cur.peek(), Some(CU_DOT) | Some(CU_COMMA)) {
        if !second_present {
            return Err(ParseError::Invalid);
        }
        let (frac, digits) = parse_fraction(cur)?;
        let total = frac as i128 * 10i128.pow((9 - digits) as u32);
        ms = (total / 1_000_000) as i64;
        us = ((total / 1_000) % 1_000) as i64;
        ns = (total % 1_000) as i64;
    }
    // Hour 24 is only valid at exactly midnight.
    if hour == 24 && (minute != 0 || second != 0 || ms != 0 || us != 0 || ns != 0) {
        return Err(ParseError::Invalid);
    }
    if hour > 24 {
        return Err(ParseError::Invalid);
    }
    Ok([hour, minute, second, ms, us, ns])
}

/// Normalize an offset in nanoseconds to the canonical ±HH:MM:SS.fff form
/// (spec 11.1.6 FormatUTCOffsetNanoseconds).
pub fn format_offset_nanoseconds(offset_ns: i128) -> String {
    if offset_ns == 0 {
        return "+00:00".to_string();
    }
    let sign = if offset_ns < 0 { "-" } else { "+" };
    let abs = offset_ns.abs();
    let hour = abs / NS_PER_HOUR;
    let minute = (abs / NS_PER_MINUTE) % 60;
    let second = (abs / NS_PER_SECOND) % 60;
    let sub = abs % NS_PER_SECOND;
    let mut out = format!("{sign}{hour:02}:{minute:02}");
    if second != 0 || sub != 0 {
        out.push_str(&format!(":{second:02}"));
        if sub != 0 {
            out.push_str(&format_fractional_seconds(sub as i64, FracPrecision::Auto));
        }
    }
    out
}

/// spec 11.1.5 FormatOffsetTimeZoneIdentifier (minutes, separated).
pub fn format_offset_time_zone_identifier(offset_minutes: i64) -> String {
    if offset_minutes == 0 {
        return "+00:00".to_string();
    }
    let sign = if offset_minutes < 0 { "-" } else { "+" };
    let abs = offset_minutes.abs();
    format!("{sign}{:02}:{:02}", abs / 60, abs % 60)
}

/// spec 11.1.7 FormatDateTimeUTCOffsetRounded (to the minute).
pub fn format_date_time_utc_offset_rounded(offset_ns: i128) -> String {
    let rounded =
        round_number_to_increment(offset_ns, 60 * NS_PER_SECOND, RoundingMode::HalfExpand);
    format_offset_time_zone_identifier((rounded / (60 * NS_PER_SECOND)) as i64)
}

// ---------------------------------------------------------------------------
// Rounding (spec 13.27-13.30)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundingMode {
    Ceil,
    Floor,
    Expand,
    Trunc,
    HalfCeil,
    HalfFloor,
    HalfExpand,
    HalfTrunc,
    HalfEven,
}

impl RoundingMode {
    #[allow(clippy::should_implement_trait)]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ceil" => Self::Ceil,
            "floor" => Self::Floor,
            "expand" => Self::Expand,
            "trunc" => Self::Trunc,
            "halfCeil" => Self::HalfCeil,
            "halfFloor" => Self::HalfFloor,
            "halfExpand" => Self::HalfExpand,
            "halfTrunc" => Self::HalfTrunc,
            "halfEven" => Self::HalfEven,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnsignedMode {
    Zero,
    Infinity,
    HalfZero,
    HalfInfinity,
    HalfEven,
}

/// spec 13.27 GetUnsignedRoundingMode.
fn unsigned_mode(mode: RoundingMode, is_negative: bool) -> UnsignedMode {
    match mode {
        RoundingMode::Ceil => {
            if is_negative {
                UnsignedMode::Zero
            } else {
                UnsignedMode::Infinity
            }
        }
        RoundingMode::Floor => {
            if is_negative {
                UnsignedMode::Infinity
            } else {
                UnsignedMode::Zero
            }
        }
        RoundingMode::Expand => UnsignedMode::Infinity,
        RoundingMode::Trunc => UnsignedMode::Zero,
        RoundingMode::HalfCeil => {
            if is_negative {
                UnsignedMode::HalfZero
            } else {
                UnsignedMode::HalfInfinity
            }
        }
        RoundingMode::HalfFloor => {
            if is_negative {
                UnsignedMode::HalfInfinity
            } else {
                UnsignedMode::HalfZero
            }
        }
        RoundingMode::HalfExpand => UnsignedMode::HalfInfinity,
        RoundingMode::HalfTrunc => UnsignedMode::HalfZero,
        RoundingMode::HalfEven => UnsignedMode::HalfEven,
    }
}

/// spec 13.28 ApplyUnsignedRoundingMode on integer distances; returns whether
/// the upper bound r2 wins. `q` is the floor quotient (for the half-even
/// cardinality check).
fn apply_unsigned(d1: u128, d2: u128, q: u128, mode: UnsignedMode) -> bool {
    match mode {
        UnsignedMode::Zero => false,
        UnsignedMode::Infinity => true,
        UnsignedMode::HalfZero | UnsignedMode::HalfInfinity | UnsignedMode::HalfEven => {
            if d1 < d2 {
                false
            } else if d2 < d1 {
                true
            } else {
                match mode {
                    UnsignedMode::HalfInfinity => true,
                    UnsignedMode::HalfEven => q & 1 == 1,
                    _ => false,
                }
            }
        }
    }
}

/// spec 13.29 RoundNumberToIncrement (sign-aware).
pub fn round_number_to_increment(x: i128, increment: i128, mode: RoundingMode) -> i128 {
    if increment <= 0 || x % increment == 0 {
        return x;
    }
    let is_negative = x < 0;
    let ax = x.unsigned_abs();
    let ai = increment.unsigned_abs();
    let q = ax / ai;
    let r1 = q * ai;
    let r2 = (q + 1) * ai;
    let d1 = ax - r1;
    let d2 = r2 - ax;
    let mode = unsigned_mode(mode, is_negative);
    let rounded_abs = if apply_unsigned(d1, d2, q, mode) {
        r2
    } else {
        r1
    };
    if is_negative {
        -(rounded_abs as i128)
    } else {
        rounded_abs as i128
    }
}

/// spec 13.30 RoundNumberToIncrementAsIfPositive (used for exact times:
/// "rounding down" always means toward the beginning of time). The rounding
/// mode is applied with the positive sign convention to the signed quotient,
/// so ceil/expand round toward zero for negative inputs.
pub fn round_number_to_increment_as_if_positive(
    x: i128,
    increment: i128,
    mode: RoundingMode,
) -> i128 {
    if increment <= 0 || x % increment == 0 {
        return x;
    }
    let r1 = x.div_euclid(increment) * increment;
    let r2 = r1 + increment;
    let d1 = (x - r1) as u128;
    let d2 = (r2 - x) as u128;
    let mode = unsigned_mode(mode, false);
    let q = x.div_euclid(increment);
    if apply_unsigned(d1, d2, q.unsigned_abs(), mode) {
        r2
    } else {
        r1
    }
}

/// spec 13.8 NegateRoundingMode (used by `since`).
pub fn negate_rounding_mode(mode: RoundingMode) -> RoundingMode {
    match mode {
        RoundingMode::Ceil => RoundingMode::Floor,
        RoundingMode::Floor => RoundingMode::Ceil,
        RoundingMode::HalfCeil => RoundingMode::HalfFloor,
        RoundingMode::HalfFloor => RoundingMode::HalfCeil,
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Duration strings (spec 13.37)
// ---------------------------------------------------------------------------

/// Parse a duration string into [y, mo, w, d, h, m, s, ms, us, ns] (sign
/// applied). Fractional time units cascade into the sub-units with floor
/// division at each level (the spec's rational-arithmetic steps).
pub fn parse_duration_string(text: &[u16]) -> Result<[i64; 10], ParseError> {
    let mut cur = Cursor::new(text);
    let sign = if cur.eat(CU_MINUS) {
        -1i64
    } else {
        cur.eat(CU_PLUS);
        1i64
    };
    if !cur.eat(CU_P) && !cur.eat(0x70) {
        // 'p' is the lowercase designator; the duration grammar accepts both
        // cases (test262 Duration/from/argument-string.js).
        return Err(ParseError::Invalid);
    }
    let mut fields = [0i64; 10];
    let mut any = false;

    // Date part: Y M W D in ascending order, no fractions.
    let mut last_date = -1i64;
    loop {
        match cur.peek() {
            Some(CU_T) | Some(0x74) => break, // 'T' or 't'
            Some(c) if is_digit(c) => {
                any = true;
                let value = cur.number()?;
                let unit = cur.bump().ok_or(ParseError::Invalid)?;
                let idx = match unit {
                    0x59 | 0x79 => 0, // Y y
                    0x4D | 0x6D => 1, // M m
                    0x57 | 0x77 => 2, // W w
                    0x44 | 0x64 => 3, // D d
                    _ => return Err(ParseError::Invalid),
                };
                if matches!(cur.peek(), Some(CU_DOT) | Some(CU_COMMA)) {
                    return Err(ParseError::Invalid);
                }
                if idx as i64 <= last_date {
                    return Err(ParseError::Invalid);
                }
                last_date = idx as i64;
                fields[idx] = value;
            }
            _ => break,
        }
    }

    // Time part: T H M S in ascending order; fractions on the last unit only.
    if cur.eat(CU_T) || cur.eat(0x74) {
        let mut last_time = -1i64;
        let mut fraction_seen = false;
        loop {
            match cur.peek() {
                Some(c) if is_digit(c) => {
                    if fraction_seen {
                        return Err(ParseError::Invalid);
                    }
                    any = true;
                    let value = cur.number()?;
                    let mut frac = 0i64;
                    let mut scale = 0usize;
                    if matches!(cur.peek(), Some(CU_DOT) | Some(CU_COMMA)) {
                        (frac, scale) = parse_fraction(&mut cur)?;
                        fraction_seen = true;
                    }
                    let unit = cur.bump().ok_or(ParseError::Invalid)?;
                    let idx = match unit {
                        0x48 | 0x68 => 4, // H h
                        0x4D | 0x6D => 5, // M m
                        0x53 | 0x73 => 6, // S s
                        _ => return Err(ParseError::Invalid),
                    };
                    if idx as i64 <= last_time {
                        return Err(ParseError::Invalid);
                    }
                    last_time = idx as i64;
                    fields[idx] = value;
                    if scale > 0 {
                        // The fraction cascades into the sub-units: exact
                        // rational nanoseconds, floor at each unit.
                        let unit_ns = match idx {
                            4 => NS_PER_HOUR,
                            5 => NS_PER_MINUTE,
                            _ => NS_PER_SECOND,
                        };
                        let total_ns = frac as i128 * unit_ns;
                        let den = 10i128.pow(scale as u32);
                        let mut rest = total_ns / den;
                        if idx == 4 {
                            fields[5] += (rest / NS_PER_MINUTE) as i64;
                            rest %= NS_PER_MINUTE;
                        }
                        if idx <= 5 {
                            fields[6] += (rest / NS_PER_SECOND) as i64;
                            rest %= NS_PER_SECOND;
                        }
                        fields[7] += (rest / 1_000_000) as i64;
                        rest %= 1_000_000;
                        fields[8] += (rest / 1_000) as i64;
                        fields[9] += (rest % 1_000) as i64;
                    }
                }
                _ => break,
            }
        }
    }

    if !any || !cur.at_end() {
        return Err(ParseError::Invalid);
    }
    for f in fields.iter_mut() {
        *f *= sign;
    }
    Ok(fields)
}

// ---------------------------------------------------------------------------
// Unit tables (spec 13.5)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Year,
    Month,
    Week,
    Day,
    Hour,
    Minute,
    Second,
    Millisecond,
    Microsecond,
    Nanosecond,
}

impl Unit {
    #[allow(clippy::should_implement_trait)]
    pub fn from_string(s: &str) -> Option<Self> {
        Some(match s {
            "year" | "years" => Self::Year,
            "month" | "months" => Self::Month,
            "week" | "weeks" => Self::Week,
            "day" | "days" => Self::Day,
            "hour" | "hours" => Self::Hour,
            "minute" | "minutes" => Self::Minute,
            "second" | "seconds" => Self::Second,
            "millisecond" | "milliseconds" => Self::Millisecond,
            "microsecond" | "microseconds" => Self::Microsecond,
            "nanosecond" | "nanoseconds" => Self::Nanosecond,
            _ => return None,
        })
    }
    pub fn category(self) -> Category {
        match self {
            Self::Year | Self::Month | Self::Week | Self::Day => Category::Date,
            _ => Category::Time,
        }
    }
    /// Length in nanoseconds (calendar-dependent units have none).
    pub fn length_ns(self) -> Option<i128> {
        Some(match self {
            Self::Day => NS_PER_DAY,
            Self::Hour => NS_PER_HOUR,
            Self::Minute => NS_PER_MINUTE,
            Self::Second => NS_PER_SECOND,
            Self::Millisecond => 1_000_000,
            Self::Microsecond => 1_000,
            Self::Nanosecond => 1,
            _ => return None,
        })
    }
    pub fn max_rounding_increment(self) -> Option<i64> {
        Some(match self {
            Self::Hour => 24,
            Self::Minute => 60,
            Self::Second => 60,
            Self::Millisecond => 1000,
            Self::Microsecond => 1000,
            Self::Nanosecond => 1000,
            _ => return None,
        })
    }
    pub fn ordinal(self) -> usize {
        match self {
            Self::Year => 0,
            Self::Month => 1,
            Self::Week => 2,
            Self::Day => 3,
            Self::Hour => 4,
            Self::Minute => 5,
            Self::Second => 6,
            Self::Millisecond => 7,
            Self::Microsecond => 8,
            Self::Nanosecond => 9,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Date,
    Time,
}

/// spec 13.20 LargerOfTwoTemporalUnits.
pub fn larger_of_two_units(a: Unit, b: Unit) -> Unit {
    if a.ordinal() <= b.ordinal() { a } else { b }
}

/// spec 13.21 IsCalendarUnit.
pub fn is_calendar_unit(unit: Unit) -> bool {
    matches!(unit, Unit::Year | Unit::Month | Unit::Week)
}

// ---------------------------------------------------------------------------
// ISO calendar date arithmetic (spec 3.5.6-3.5.9, 12.3.7, 12.3.9)
// ---------------------------------------------------------------------------

/// spec 9.5.4 BalanceISOYearMonth.
pub fn balance_iso_year_month(year: i64, month: i64) -> (i64, i64) {
    let y = year + (month - 1).div_euclid(12);
    let m = (month - 1).rem_euclid(12) + 1;
    (y, m)
}

/// spec 3.5.7 RegulateISODate (constrain clamps; reject keeps raw values and
/// the caller validates).
pub fn regulate_iso_date(year: i64, month: i64, day: i64, constrain: bool) -> (i64, i64, i64) {
    if constrain {
        let month = month.clamp(1, 12);
        let day = day.clamp(1, days_in_month(year, month));
        (year, month, day)
    } else {
        (year, month, day)
    }
}

/// spec 3.5.9 AddDaysToISODate.
pub fn add_days_to_iso_date(year: i64, month: i64, day: i64, days: i64) -> (i64, i64, i64) {
    let epoch = iso_date_to_epoch_days(year, month - 1, day) + days;
    epoch_days_to_iso_date(epoch)
}

/// spec 12.3.7 CalendarDateAdd for the iso8601 calendar.
#[allow(clippy::too_many_arguments)]
pub fn calendar_date_add(
    year: i64,
    month: i64,
    day: i64,
    years: i64,
    months: i64,
    weeks: i64,
    days: i64,
    constrain: bool,
) -> Option<(i64, i64, i64)> {
    let (y, m) = balance_iso_year_month(year + years, month + months);
    let (y, m, d) = regulate_iso_date(y, m, day, constrain);
    if !is_valid_iso_date(y, m, d) {
        return None;
    }
    Some(add_days_to_iso_date(y, m, d, days + 7 * weeks))
}

/// spec 3.5.5 CompareSurpasses on integer fields: year, then month, then day.
fn compare_surpasses(sign: i64, y: i64, m: i64, d: i64, target: (i64, i64, i64)) -> bool {
    if y != target.0 {
        return surpasses_field(sign, y, target.0);
    }
    if m != target.1 {
        return surpasses_field(sign, m, target.1);
    }
    d != target.2 && surpasses_field(sign, d, target.2)
}

/// spec 3.5.6 ISODateSurpasses for the ISO calendar.
#[allow(clippy::too_many_arguments)]
fn iso_date_surpasses(
    sign: i64,
    base: (i64, i64, i64),
    target: (i64, i64, i64),
    years: i64,
    months: i64,
    weeks: i64,
    days: i64,
) -> bool {
    // Step 4: compare (y0, the base month/day) against the target.
    let y0 = base.0 + years;
    if compare_surpasses(sign, y0, base.1, base.2, target) {
        return true;
    }
    // Steps 5-8: the months-added check only applies when months != 0; the
    // weeks/days logic below must still run (the week/day loops of
    // CalendarDateUntil pass months = 0).
    let (ym, mm) = balance_iso_year_month(y0, base.1 + months);
    if months != 0 && compare_surpasses(sign, ym, mm, base.2, target) {
        return true;
    }
    if weeks == 0 && days == 0 {
        return false;
    }
    let regulated = regulate_iso_date(ym, mm, base.2, true);
    let (y, m, d) = add_days_to_iso_date(regulated.0, regulated.1, regulated.2, 7 * weeks + days);
    compare_surpasses(sign, y, m, d, target)
}

fn surpasses_field(sign: i64, value: i64, target: i64) -> bool {
    sign * (value - target) > 0
}

/// spec 3.5.13 CompareISODate.
pub fn compare_iso_date(a: (i64, i64, i64), b: (i64, i64, i64)) -> i64 {
    if a.0 != b.0 {
        return (a.0 - b.0).signum();
    }
    if a.1 != b.1 {
        return (a.1 - b.1).signum();
    }
    (a.2 - b.2).signum()
}

/// spec 12.3.9 CalendarDateUntil for the iso8601 calendar.
pub fn calendar_date_until(
    one: (i64, i64, i64),
    two: (i64, i64, i64),
    largest_unit: Unit,
) -> (i64, i64, i64, i64) {
    let sign = compare_iso_date(one, two);
    if sign == 0 {
        return (0, 0, 0, 0);
    }
    let sign = -sign;
    let mut years = 0i64;
    if largest_unit == Unit::Year {
        let mut candidate = sign;
        while !iso_date_surpasses(sign, one, two, candidate, 0, 0, 0) {
            years = candidate;
            candidate += sign;
        }
    }
    let mut months = 0i64;
    if matches!(largest_unit, Unit::Year | Unit::Month) {
        let mut candidate = sign;
        while !iso_date_surpasses(sign, one, two, years, candidate, 0, 0) {
            months = candidate;
            candidate += sign;
        }
    }
    let mut weeks = 0i64;
    if largest_unit == Unit::Week {
        let mut candidate = sign;
        while !iso_date_surpasses(sign, one, two, years, months, candidate, 0) {
            weeks = candidate;
            candidate += sign;
        }
    }
    let mut days = 0i64;
    if years == 0 && months == 0 && weeks == 0 {
        // Pure day difference (largest_unit == Day): closed form. The
        // day-by-day loop is O(days) — ~100M iterations for the
        // edge-of-range dates (the argument-string-limits fixtures),
        // which took ~12s and crossed the sweep's 15s deadline.
        days = iso_date_to_epoch_days(two.0, two.1 - 1, two.2)
            - iso_date_to_epoch_days(one.0, one.1 - 1, one.2);
    } else {
        let mut candidate = sign;
        while !iso_date_surpasses(sign, one, two, years, months, weeks, candidate) {
            days = candidate;
            candidate += sign;
        }
    }
    (years, months, weeks, days)
}

/// spec 5.5.4 ISODateTimeWithinLimits.
#[allow(clippy::too_many_arguments)]
pub fn iso_date_time_within_limits(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    ms: i64,
    us: i64,
    ns: i64,
) -> bool {
    let days = iso_date_to_epoch_days(year, month - 1, day);
    if days.abs() > 100_000_001 {
        return false;
    }
    let epoch = get_utc_epoch_nanoseconds(year, month, day, hour, minute, second, ms, us, ns);
    epoch > NS_MIN_INSTANT - NS_PER_DAY && epoch < NS_MAX_INSTANT + NS_PER_DAY
}

pub fn epoch_ns_to_bigint(epoch_ns: i128) -> BigInt {
    BigInt::parse_str(&epoch_ns.to_string(), 10).unwrap_or_else(BigInt::zero)
}

pub fn bigint_to_epoch_ns(b: &BigInt) -> Option<i128> {
    b.0.to_str_radix(10).parse::<i128>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_days_roundtrip() {
        for (y, m, d) in [
            (1970, 1, 1),
            (2000, 3, 1),
            (1969, 12, 31),
            (-271821, 4, 20),
            (275760, 9, 13),
        ] {
            let days = iso_date_to_epoch_days(y, m - 1, d);
            assert_eq!(epoch_days_to_iso_date(days), (y, m, d), "{y}-{m}-{d}");
        }
    }

    #[test]
    fn epoch_ns_known_values() {
        assert_eq!(get_utc_epoch_nanoseconds(1970, 1, 1, 0, 0, 0, 0, 0, 0), 0);
        assert_eq!(
            get_utc_epoch_nanoseconds(2016, 12, 31, 23, 59, 59, 0, 0, 0),
            1_483_228_799_000_000_000
        );
    }

    #[test]
    fn iso_parts_known_values() {
        assert_eq!(iso_parts_from_epoch(0), (1970, 1, 1, 0, 0, 0, 0, 0, 0));
        assert_eq!(
            iso_parts_from_epoch(1_483_228_799_123_456_789),
            (2016, 12, 31, 23, 59, 59, 123, 456, 789)
        );
    }

    #[test]
    fn rounding_as_if_positive() {
        // The negative-instant fixture (1938-04-24T22:13:20Z rounded to hours).
        let x = -1_000_000_000_000_000_000i128;
        let inc = 3_600_000_000_000i128;
        let down = -1_000_000_800_000_000_000i128;
        let up = -999_997_200_000_000_000i128;
        for mode in [
            RoundingMode::HalfCeil,
            RoundingMode::HalfFloor,
            RoundingMode::HalfExpand,
            RoundingMode::HalfTrunc,
            RoundingMode::HalfEven,
            RoundingMode::Floor,
            RoundingMode::Trunc,
        ] {
            assert_eq!(
                round_number_to_increment_as_if_positive(x, inc, mode),
                down,
                "{mode:?}"
            );
        }
        for mode in [RoundingMode::Ceil, RoundingMode::Expand] {
            assert_eq!(
                round_number_to_increment_as_if_positive(x, inc, mode),
                up,
                "{mode:?}"
            );
        }
    }

    #[test]
    fn rounding_signed() {
        assert_eq!(round_number_to_increment(5, 2, RoundingMode::HalfEven), 4);
        assert_eq!(round_number_to_increment(7, 2, RoundingMode::HalfEven), 8);
        assert_eq!(
            round_number_to_increment(-7, 2, RoundingMode::HalfExpand),
            -8
        );
        assert_eq!(round_number_to_increment(-7, 2, RoundingMode::Trunc), -6);
        assert_eq!(round_number_to_increment(-7, 2, RoundingMode::Ceil), -6);
        assert_eq!(round_number_to_increment(-7, 2, RoundingMode::Floor), -8);
    }

    #[test]
    fn parse_instant_strings() {
        let s = |text: &str| text.encode_utf16().collect::<Vec<_>>();
        let p = |text: &str| parse_iso_date_time(&s(text), Format::InstantString);
        assert!(p("1976-11-18T15:23z").is_ok());
        assert!(p("1976-11-18T15:23:30.123456789-02:00").is_ok());
        assert!(p("19761118T152330.1+00:00").is_ok());
        assert!(p("-009999-11-18T15:23:30.12Z").is_ok());
        assert!(p("1970-01-01T00:00Z[UTC][u-ca=iso8601]").is_ok());
        assert!(p("1970-01-01T00:00Z[Asia/Kolkata]").is_ok());
        assert!(p("2020-01-01T00:00Zjunk").is_err());
        assert!(p("2020-01-01T00:00").is_err()); // no offset for Instant
        assert!(p("2020-01-01T00:00[UTC]").is_err());
        assert!(p("2020-01-01T01:60:00Z").is_err());
        assert!(p("2020-01-01T00:00:00.1234567891Z").is_err());
        assert!(p("-000000-03-30T00:45Z").is_err());
        assert!(p("2020-01-01T00:00Z[UTC][UTC]").is_err());
        assert!(p("2020-01-01T00:00Z[!foo=bar]").is_err());
        assert!(p("2020-01-01T00:00Z[foo=bar]").is_ok());
        assert!(p("2020-01-01T00:00Z[U-CA=iso8601]").is_err());
        // Duplicate calendar annotations: allowed unless any is critical.
        assert!(p("1970-01-01T00:00Z[u-ca=iso8601][u-ca=discord]").is_ok());
        assert!(p("1970-01-01T00:00Z[!u-ca=hebrew]").is_ok());
        assert!(p("1970-01-01T00:00Z[u-ca=iso8601][!u-ca=iso8601]").is_err());
        assert!(p("1970-01-01T00:00Z[!u-ca=iso8601][u-ca=iso8601]").is_err());
        assert!(p("1970-01-01T00:02:00.000000000+00:02[+01:30]").is_ok());
        assert!(p("1970-01-01T00:19:32.37+00:19:32.37").is_ok());
        assert!(p("2016-12-31T23:59:60Z").is_ok());
        assert!(p("2020-01-01T24:00:00Z").is_ok());
        assert!(p("2020-01-01T24:00:00.000000001Z").is_err());
        assert!(p("2020-01-01T25:00:00Z").is_err());
        assert!(p("2020-01-01T00:00-24:00").is_err());
        assert!(p("+999999-01-01T00:00Z").is_ok()); // parse ok; range check later
        assert!(p("02020-01-01T00:00Z").is_err());
    }

    #[test]
    fn parse_duration_strings() {
        let s = |text: &str| text.encode_utf16().collect::<Vec<_>>();
        assert_eq!(
            parse_duration_string(&s("P5Y5M5W5DT5H5M5.500409040S")).unwrap(),
            [5, 5, 5, 5, 5, 5, 5, 500, 409, 40]
        );
        assert_eq!(
            parse_duration_string(&s("-PT46H66M71.50040904S")).unwrap(),
            [0, 0, 0, 0, -46, -66, -71, -500, -409, -40]
        );
        assert_eq!(
            parse_duration_string(&s("PT1.03125H")).unwrap(),
            [0, 0, 0, 0, 1, 1, 52, 500, 0, 0]
        );
        assert_eq!(
            parse_duration_string(&s("-PT3,025M")).unwrap(),
            [0, 0, 0, 0, 0, -3, -1, -500, 0, 0]
        );
        assert_eq!(
            parse_duration_string(&s("PT3.125M")).unwrap(),
            [0, 0, 0, 0, 0, 3, 7, 500, 0, 0]
        );
        assert_eq!(
            parse_duration_string(&s("P3Y4W")).unwrap(),
            [3, 0, 4, 0, 0, 0, 0, 0, 0, 0]
        );
        assert!(parse_duration_string(&s("P1Y1M1W1DT1H1M1.123456789123S")).is_err());
        assert!(parse_duration_string(&s("P0.5Y")).is_err());
        assert!(parse_duration_string(&s("P")).is_err());
        assert!(parse_duration_string(&s("PT.1H")).is_err());
        assert!(parse_duration_string(&s("P2H")).is_err());
        assert!(parse_duration_string(&s("P2.5M")).is_err());
        assert!(parse_duration_string(&s("PT1Y1M1W1DT0.5H5S")).is_err());
        assert!(parse_duration_string(&s("")).is_err());
    }
}
