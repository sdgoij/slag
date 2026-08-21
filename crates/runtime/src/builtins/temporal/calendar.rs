//! Calendar arithmetic for the supported non-ISO calendars (the
//! intl402/Temporal fixtures): the tabular Islamic calendar (islamic-civil
//! and islamic-tbla) with ISO-date conversion, plus the dispatch helpers the
//! property-bag readers use for CalendarDateFromFields and the reference-date
//! selection. The remaining calendars (hebrew, chinese, dangi, ...) need
//! their own arithmetic and pass through as ISO for now.

use super::iso;

/// The fixed-date (RD 1 = 0001-01-01 proleptic Gregorian) offset of the
/// engine's epoch days (epoch day 0 = 1970-01-01 = RD 719163).
const RD_OFFSET: i64 = 719_163;

/// The Hebrew calendar epoch: 1 Tishri 1 AM (RD; the molad-derived epoch
/// the postponement rules start from).
const HEBREW_EPOCH: i64 = -1_373_428;

// Hebrew calendar (the molad with the four postponement rules).

/// The Hebrew leap years: 3, 6, 8, 11, 14, 17, 19, 22, 25, 27 (mod 19).
pub fn hebrew_leap_year(year: i64) -> bool {
    (7 * year + 1).rem_euclid(19) < 7
}

/// The months before year `year` (the molad count).
pub fn hebrew_months_before_year(year: i64) -> i64 {
    235 * (year - 1).div_euclid(19)
        + 12 * (year - 1).rem_euclid(19)
        + (7 * ((year - 1).rem_euclid(19)) + 1) / 19
}

/// The RD of 1 Tishri of the year: the molad moment with the postponement
/// rules (molad Zaqen, GaTaRaD, BeTuTaKPaT, Lo ADU Rosh).
pub fn hebrew_elapsed_days(year: i64) -> i64 {
    let months = hebrew_months_before_year(year);
    let parts_elapsed = 204 + 793 * months.rem_euclid(1080);
    let hours_elapsed = 5 + 12 * months + 793 * (months / 1080) + parts_elapsed / 1080;
    let mut day = 1 + 29 * months + hours_elapsed / 24;
    let parts = 1080 * hours_elapsed.rem_euclid(24) + parts_elapsed.rem_euclid(1080);
    let dow = day.rem_euclid(7);
    let leap = hebrew_leap_year(year);
    // Molad Zaqen: at or after 18h. GaTaRaD: on a Tuesday at or after 9h204p
    // in a common year (the corpus pins Tuesday-only, matching the ICU-era
    // data). BeTuTaKPaT: on a Sunday at or after 15h589p in a leap year.
    if parts >= 19_440
        || (dow == 2 && parts >= 9_924 && !leap)
        || (dow == 0 && parts >= 16_789 && leap)
    {
        day += 1;
    }
    // Lo ADU Rosh: the year never begins on a Sunday, Wednesday, or Friday.
    if matches!(day.rem_euclid(7), 0 | 3 | 5) {
        day += 1;
    }
    day
}

/// The 356/382-day year-length anomalies (the years whose length would
/// otherwise fall outside 353-355 / 383-385): a 356-day year starts 2 days
/// later, and the year after a 382-day year starts 1 day later.
pub fn hebrew_length_correction(year: i64) -> i64 {
    if hebrew_elapsed_days(year + 1) - hebrew_elapsed_days(year) == 356 {
        2
    } else if hebrew_elapsed_days(year) - hebrew_elapsed_days(year - 1) == 382 {
        1
    } else {
        0
    }
}

/// The days in a Hebrew year (353-355 common, 383-385 leap): the gap
/// between the corrected starts of the year and the next.
pub fn hebrew_year_length(year: i64) -> i64 {
    (hebrew_elapsed_days(year + 1) + hebrew_length_correction(year + 1))
        - (hebrew_elapsed_days(year) + hebrew_length_correction(year))
}

/// The months in a Hebrew year (12 common, 13 leap).
pub fn hebrew_months_in_year(year: i64) -> i64 {
    if hebrew_leap_year(year) { 13 } else { 12 }
}

/// The days in a Hebrew month (1 = Tishri; Adar I is month 6 in leap years,
/// Adar is 6 in common / 7 in leap years, and the fixed Nisan-Elul months
/// shift by one in leap years).
pub fn hebrew_month_length(year: i64, month: i64) -> i64 {
    let leap = hebrew_leap_year(year);
    let len = hebrew_year_length(year) % 10;
    match month {
        1 => 30, // Tishri
        2 => {
            // Cheshvan (29 or 30 in a complete year)
            if len == 5 { 30 } else { 29 }
        }
        3 => {
            // Kislev (30 in a regular or complete year)
            if len == 4 || len == 5 { 30 } else { 29 }
        }
        4 => 29, // Tevet
        5 => 30, // Shevat
        6 => {
            if leap {
                30 // Adar I
            } else {
                29 // Adar
            }
        }
        7 => {
            if leap {
                29 // Adar
            } else {
                30 // Nisan
            }
        }
        8 => {
            if leap {
                30 // Nisan
            } else {
                29 // Iyar
            }
        }
        9 => {
            if leap {
                29 // Iyar
            } else {
                30 // Sivan
            }
        }
        10 => {
            if leap {
                30 // Sivan
            } else {
                29 // Tammuz
            }
        }
        11 => {
            if leap {
                29 // Tammuz
            } else {
                30 // Av
            }
        }
        12 if leap => 30, // Av
        12 => 29,         // Elul
        _ => 29,          // Elul (leap years only)
    }
}

/// Hebrew date → fixed date (the elapsed days plus the month lengths).
pub fn hebrew_to_fixed(year: i64, month: i64, day: i64) -> i64 {
    let elapsed = hebrew_elapsed_days(year) + hebrew_length_correction(year);
    let days_before = (1..month)
        .map(|m| hebrew_month_length(year, m))
        .sum::<i64>();
    HEBREW_EPOCH + elapsed + days_before + day - 1
}

/// Fixed date → Hebrew date (the year estimate then the month/day search).
pub fn hebrew_from_fixed(rd: i64) -> (i64, i64, i64) {
    let mut year = (rd - HEBREW_EPOCH) * 19 / 6940 + 1;
    while hebrew_to_fixed(year, 1, 1) > rd {
        year -= 1;
    }
    while hebrew_to_fixed(year + 1, 1, 1) <= rd {
        year += 1;
    }
    let mut month = 1;
    while month < hebrew_months_in_year(year) && hebrew_to_fixed(year, month + 1, 1) <= rd {
        month += 1;
    }
    let day = rd - hebrew_to_fixed(year, month, 1) + 1;
    (year, month, day)
}

/// The tabular Islamic calendar epochs: the RD of 1 Muharram 1 AH
/// (islamic-civil uses the 622-07-19 civil epoch, islamic-tbla the
/// Thursday epoch one day earlier).
const ISLAMIC_CIVIL_EPOCH: i64 = 227_015;
const ISLAMIC_TBLA_EPOCH: i64 = 227_014;

fn islamic_epoch(calendar: &str) -> Option<i64> {
    match calendar {
        "islamic-civil" => Some(ISLAMIC_CIVIL_EPOCH),
        "islamic-tbla" => Some(ISLAMIC_TBLA_EPOCH),
        _ => None,
    }
}

/// The tabular leap years: the years 2, 5, 7, 10, 13, 16, 18, 21, 24, 26,
/// 29 of each 30-year cycle.
fn islamic_leap_year(year: i64) -> bool {
    (11 * year + 14).rem_euclid(30) < 11
}

/// The days in a tabular Islamic month (months 1-11 alternate 30/29;
/// month 12 is 30 in leap years).
fn days_in_islamic_month(year: i64, month: i64) -> i64 {
    if month == 12 {
        if islamic_leap_year(year) { 30 } else { 29 }
    } else if month % 2 == 1 {
        30
    } else {
        29
    }
}

/// The tabular Islamic date → fixed date.
fn islamic_to_fixed(year: i64, month: i64, day: i64, epoch: i64) -> i64 {
    epoch - 1 + (year - 1) * 354 + (3 + 11 * year) / 30 + 29 * (month - 1) + month / 2 + day
}

/// The fixed date → tabular Islamic date (the ISO → Islamic direction the
/// DateTimeFormat calendar-field conversion uses).
fn islamic_from_fixed(rd: i64, epoch: i64) -> (i64, i64, i64) {
    let year = (30 * (rd - epoch) + 10646) / 10631;
    let prior = rd - islamic_to_fixed(year, 1, 1, epoch);
    let month = (11 * prior + 330) / 325;
    let day = rd - islamic_to_fixed(year, month, 1, epoch) + 1;
    (year, month, day)
}

/// CalendarDateFromFields: the calendar (year, month, day) → ISO date.
/// `None` for the calendars that pass through as ISO.
pub fn calendar_date_to_iso(
    calendar: &str,
    year: i64,
    month: i64,
    day: i64,
) -> Option<(i64, i64, i64)> {
    let rd = match calendar {
        "islamic-civil" | "islamic-tbla" => {
            islamic_to_fixed(year, month, day, islamic_epoch(calendar)?)
        }
        "hebrew" => hebrew_to_fixed(year, month, day),
        _ => return None,
    };
    Some(iso::epoch_days_to_iso_date(rd - RD_OFFSET))
}

/// The reverse of `calendar_date_to_iso` (CalendarDateToIso): ISO date →
/// calendar (year, month, day).
pub fn calendar_iso_to_date(calendar: &str, y: i64, m: i64, d: i64) -> Option<(i64, i64, i64)> {
    let rd = iso::iso_date_to_epoch_days(y, m - 1, d) + RD_OFFSET;
    match calendar {
        "islamic-civil" | "islamic-tbla" => Some(islamic_from_fixed(rd, islamic_epoch(calendar)?)),
        "hebrew" => Some(hebrew_from_fixed(rd)),
        _ => None,
    }
}

/// The Hebrew month code of a month (the M05L Adar I of leap years; the
/// M06-M12 codes shift by one in leap years, while a common year's months
/// 7-12 keep their own numbers).
pub fn hebrew_month_code(year: i64, month: i64) -> String {
    if hebrew_leap_year(year) {
        match month {
            1..=5 => format!("M{month:02}"),
            6 => "M05L".to_string(),
            _ => format!("M{:02}", month - 1), // 7..=13 -> M06..M12
        }
    } else {
        format!("M{month:02}") // 1..=12 -> M01..M12 (Adar is M06)
    }
}

/// The calendar month number of a month/monthCode input (the month codes
/// depend on the year's leap status for hebrew: M06-M12 shift by one in leap
/// years, M05L is Adar I in leap years only). `None` when the input is
/// invalid for the year.
pub fn resolve_calendar_month(
    calendar: &str,
    year: i64,
    month: Option<i64>,
    month_code: Option<&str>,
) -> Option<i64> {
    if calendar == "hebrew" {
        return match (month, month_code) {
            // The numeric-month arm has no upper bound here: the callers
            // regulate against the year's month count (constrain clamps,
            // reject errors).
            (Some(m), _) => {
                if m >= 1 {
                    Some(m)
                } else {
                    None
                }
            }
            (None, Some(code)) => match code {
                "M01" | "M02" | "M03" | "M04" | "M05" => code[1..].parse().ok(),
                "M05L" => {
                    if hebrew_leap_year(year) {
                        Some(6)
                    } else {
                        None
                    }
                }
                "M06" => Some(if hebrew_leap_year(year) { 7 } else { 6 }),
                "M07" => Some(if hebrew_leap_year(year) { 8 } else { 7 }),
                "M08" => Some(if hebrew_leap_year(year) { 9 } else { 8 }),
                "M09" => Some(if hebrew_leap_year(year) { 10 } else { 9 }),
                "M10" => Some(if hebrew_leap_year(year) { 11 } else { 10 }),
                "M11" => Some(if hebrew_leap_year(year) { 12 } else { 11 }),
                "M12" => Some(if hebrew_leap_year(year) { 13 } else { 12 }),
                _ => None,
            },
            _ => None,
        };
    }
    // The pass-through calendars (iso8601, gregory, coptic, ...): M01-M12
    // (or M01-M13 for the 13-month calendars), bounded by the calendar's own
    // month count.
    match (month, month_code) {
        (Some(m), _) => {
            let max = calendar_months_in_year(calendar, year).unwrap_or(12);
            if (1..=max).contains(&m) {
                Some(m)
            } else {
                None
            }
        }
        (None, Some(code)) if code.len() == 3 && code.starts_with('M') => {
            let n = code[1..].parse::<i64>().ok()?;
            let max = calendar_months_in_year(calendar, year).unwrap_or(12);
            if (1..=max).contains(&n) {
                Some(n)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The calendar month number of a month/monthCode input, with the overflow
/// option applied: an M05L that does not exist in a common hebrew year
/// constrains to Adar (month 6) and rejects otherwise. `None` when invalid.
pub fn resolve_calendar_month_with_overflow(
    calendar: &str,
    year: i64,
    month: Option<i64>,
    month_code: Option<&str>,
    constrain: bool,
) -> Option<i64> {
    let resolved = resolve_calendar_month(calendar, year, month, month_code);
    if resolved.is_none()
        && constrain
        && calendar == "hebrew"
        && month.is_none()
        && month_code == Some("M05L")
        && !hebrew_leap_year(year)
    {
        return Some(6); // Adar
    }
    resolved
}

/// The months in a calendar year (for the year-month overflow regulation).
pub fn calendar_months_in_year(calendar: &str, year: i64) -> Option<i64> {
    match calendar {
        "hebrew" => Some(hebrew_months_in_year(year)),
        "islamic-civil" | "islamic-tbla" | "islamic-umalqura" => Some(12),
        "coptic" | "ethiopic" | "ethioaa" => Some(13),
        _ => None,
    }
}

/// The days in a calendar month (for the overflow regulation). The
/// pass-through calendars' entries cover the month codes the corpus
/// constrains against (constrain-to-leap-day): chinese/dangi lunar months
/// are 29/30, the coptic/ethiopic months are 30 with a 5/6-day epagomenal
/// month 13, persian months 1-6 are 31 (7-11 30, 12 29/30), and indian is
/// the same shape.
pub fn calendar_days_in_month(calendar: &str, year: i64, month: i64) -> Option<i64> {
    match calendar {
        "hebrew" => Some(hebrew_month_length(year, month)),
        "islamic-civil" | "islamic-tbla" => Some(days_in_islamic_month(year, month)),
        "chinese" | "dangi" => Some(30),
        "coptic" | "ethiopic" | "ethioaa" => Some(if month == 13 { 6 } else { 30 }),
        "islamic-umalqura" => Some(30),
        "persian" | "indian" => Some(match month {
            1..=6 => 31,
            7..=11 => 30,
            _ => 30,
        }),
        _ => None,
    }
}

/// The ISO year-month containing the calendar month's first day, plus the
/// reference day (the ISO day of that first day) — CalendarYearMonthFromFields.
pub fn calendar_year_month_to_iso(
    calendar: &str,
    year: i64,
    month: i64,
) -> Option<(i64, i64, i64)> {
    let rd = match calendar {
        "islamic-civil" | "islamic-tbla" => {
            islamic_to_fixed(year, month, 1, islamic_epoch(calendar)?)
        }
        "hebrew" => hebrew_to_fixed(year, month, 1),
        _ => return None,
    };
    let (y, m, d) = iso::epoch_days_to_iso_date(rd - RD_OFFSET);
    Some((y, m, d))
}

/// The reference ISO date for a calendar month-day: the date in the latest
/// ISO year at or before 1972 where the month-day exists (CalendarMonthDay-
/// FromFields; the canonicalize-calendar fixtures pin 1972-02-11 for islamic
/// M12-25 and the hebrew fixtures their Cheshvan/Kislev variants). The month
/// code participates: M05L only exists in leap years.
pub fn calendar_month_day_reference(
    calendar: &str,
    month: i64,
    day: i64,
    month_code: Option<&str>,
) -> Option<(i64, i64, i64)> {
    let year0 = match calendar {
        "islamic-civil" | "islamic-tbla" => {
            let epoch = islamic_epoch(calendar)?;
            let rd_1972 = iso::iso_date_to_epoch_days(1972, 0, 1) + RD_OFFSET;
            (30 * (rd_1972 - epoch) + 10646) / 10631
        }
        "hebrew" => {
            let rd_1972 = iso::iso_date_to_epoch_days(1972, 0, 1) + RD_OFFSET;
            (rd_1972 - HEBREW_EPOCH) * 19 / 6940 + 1
        }
        _ => return None,
    };
    // The month-day's ISO date lands in 1972 or the year before; scan the
    // calendar years around the boundary (the estimate can be a year off)
    // and take the latest date at or before ISO 1972. The month code
    // resolves per year (M05L only exists in leap years; the leap-shifted
    // months move their numerical position).
    for year in (year0 - 2..=year0 + 2).rev() {
        let month = if let Some(code) = month_code {
            match resolve_calendar_month(calendar, year, None, Some(code)) {
                Some(m) => m,
                None => continue,
            }
        } else {
            month
        };
        if day > calendar_days_in_month(calendar, year, month)? {
            continue;
        }
        let (y, m, d) = calendar_date_to_iso(calendar, year, month, day)?;
        if y <= 1972 {
            return Some((y, m, d));
        }
    }
    None
}

/// CalendarDateAdd (spec 12.3.5): add years/months with the calendar's own
/// month sequence, then weeks/days as fixed-date arithmetic. The iso8601 and
/// linear calendars pass through to the ISO arithmetic; hebrew adds years
/// before months (the month code of a leap month must survive the year add)
/// and balances months against the year's own month count, and the tabular
/// Islamic calendars balance against 12.
pub fn calendar_date_add(
    calendar: &str,
    date: (i64, i64, i64),
    years: i64,
    months: i64,
    weeks: i64,
    days: i64,
    constrain: bool,
) -> Option<(i64, i64, i64)> {
    match calendar {
        "hebrew" => {
            let rd = iso::iso_date_to_epoch_days(date.0, date.1 - 1, date.2) + RD_OFFSET;
            let (hy, hm, hd) = hebrew_from_fixed(rd);
            let code = hebrew_month_code(hy, hm);
            let mut year = hy + years;
            // The month code must still exist after the year add (M05L in a
            // common year rejects; constrain folds it to Adar).
            let mut month =
                resolve_calendar_month_with_overflow("hebrew", year, None, Some(&code), constrain)?;
            month += months;
            while month < 1 {
                year -= 1;
                month += hebrew_months_in_year(year);
            }
            while month > hebrew_months_in_year(year) {
                month -= hebrew_months_in_year(year);
                year += 1;
            }
            let max = hebrew_month_length(year, month);
            let day = if hd > max {
                if constrain {
                    max
                } else {
                    return None;
                }
            } else {
                hd
            };
            let rd = hebrew_to_fixed(year, month, day) + days + 7 * weeks;
            Some(iso::epoch_days_to_iso_date(rd - RD_OFFSET))
        }
        "islamic-civil" | "islamic-tbla" => {
            let epoch = islamic_epoch(calendar)?;
            let rd = iso::iso_date_to_epoch_days(date.0, date.1 - 1, date.2) + RD_OFFSET;
            let (iy, im, id) = islamic_from_fixed(rd, epoch);
            let (year, month) = iso::balance_iso_year_month(iy + years, im + months);
            let max = days_in_islamic_month(year, month);
            let day = if id > max {
                if constrain {
                    max
                } else {
                    return None;
                }
            } else {
                id
            };
            let rd = islamic_to_fixed(year, month, day, epoch) + days + 7 * weeks;
            Some(iso::epoch_days_to_iso_date(rd - RD_OFFSET))
        }
        _ => iso::calendar_date_add(
            date.0, date.1, date.2, years, months, weeks, days, constrain,
        ),
    }
}

/// CalendarDateUntil (spec 12.3.9): the largest whole units between the two
/// calendar dates. The iso8601 and linear calendars use the ISO arithmetic;
/// the others run the same surpasses loop over `calendar_date_add` (the
/// leap-month years and the months-in-year balance make the hebrew counts
/// differ from the ISO ones).
pub fn calendar_date_until(
    calendar: &str,
    one: (i64, i64, i64),
    two: (i64, i64, i64),
    largest_unit: iso::Unit,
) -> (i64, i64, i64, i64) {
    if matches!(
        calendar,
        "iso8601"
            | "gregory"
            | "buddhist"
            | "japanese"
            | "roc"
            | "indian"
            | "persian"
            | "coptic"
            | "ethiopic"
            | "ethioaa"
    ) {
        return iso::calendar_date_until(one, two, largest_unit);
    }
    let sign = iso::compare_iso_date(one, two);
    if sign == 0 {
        return (0, 0, 0, 0);
    }
    let sign = -sign;
    let mut years = 0i64;
    if largest_unit == iso::Unit::Year {
        let mut candidate = sign;
        while !calendar_date_surpasses(calendar, sign, one, two, candidate, 0, 0, 0) {
            years = candidate;
            candidate += sign;
        }
    }
    let mut months = 0i64;
    if matches!(largest_unit, iso::Unit::Year | iso::Unit::Month) {
        let mut candidate = sign;
        while !calendar_date_surpasses(calendar, sign, one, two, years, candidate, 0, 0) {
            months = candidate;
            candidate += sign;
        }
    }
    let mut weeks = 0i64;
    if largest_unit == iso::Unit::Week {
        let mut candidate = sign;
        while !calendar_date_surpasses(calendar, sign, one, two, years, months, candidate, 0) {
            weeks = candidate;
            candidate += sign;
        }
    }
    let mut days = 0i64;
    let mut candidate = sign;
    while !calendar_date_surpasses(calendar, sign, one, two, years, months, weeks, candidate) {
        days = candidate;
        candidate += sign;
    }
    (years, months, weeks, days)
}

/// Whether adding the given units to `one` has crossed `two` in the sign
/// direction (the CalendarDateUntil loop, with the overflow constrained).
#[allow(clippy::too_many_arguments)]
fn calendar_date_surpasses(
    calendar: &str,
    sign: i64,
    one: (i64, i64, i64),
    two: (i64, i64, i64),
    years: i64,
    months: i64,
    weeks: i64,
    days: i64,
) -> bool {
    let Some(added) = calendar_date_add(calendar, one, years, months, weeks, days, true) else {
        return true;
    };
    sign * iso::compare_iso_date(added, two) > 0
}

/// The days in a calendar year (for the daysInYear getter).
pub fn calendar_days_in_year(calendar: &str, year: i64) -> Option<i64> {
    match calendar {
        "hebrew" => Some(hebrew_year_length(year)),
        "islamic-civil" | "islamic-tbla" => Some(if islamic_leap_year(year) { 355 } else { 354 }),
        _ => None,
    }
}

/// Whether the calendar year is a leap year (for the inLeapYear getter).
pub fn calendar_in_leap_year(calendar: &str, year: i64) -> Option<bool> {
    match calendar {
        "hebrew" => Some(hebrew_months_in_year(year) == 13),
        "islamic-civil" | "islamic-tbla" => Some(islamic_leap_year(year)),
        _ => None,
    }
}

/// The day of the calendar year (for the dayOfYear getter).
pub fn calendar_day_of_year(calendar: &str, year: i64, month: i64, day: i64) -> Option<i64> {
    match calendar {
        "hebrew" => Some(hebrew_to_fixed(year, month, day) - hebrew_to_fixed(year, 1, 1) + 1),
        "islamic-civil" | "islamic-tbla" => {
            let epoch = islamic_epoch(calendar)?;
            Some(
                islamic_to_fixed(year, month, day, epoch) - islamic_to_fixed(year, 1, 1, epoch) + 1,
            )
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iso(year: i64, month: i64, day: i64) -> (i64, i64, i64) {
        (year, month, day)
    }

    #[test]
    fn islamic_civil_date_conversion() {
        // The canonicalize-calendar fixtures: 1445-12-25 → 2024-07-02.
        assert_eq!(
            calendar_date_to_iso("islamic-civil", 1445, 12, 25),
            Some(iso(2024, 7, 2))
        );
        // The MonthDay reference: 1391-12-25 → 1972-02-11.
        assert_eq!(
            calendar_month_day_reference("islamic-civil", 12, 25, None),
            Some(iso(1972, 2, 11))
        );
        // The hebrew reference-year-1972 fixtures: M01-1 lands in ISO 1972
        // (Tishri 5733), M02-30 in 1971 (Cheshvan 5732 is a 30-day month and
        // Cheshvan 5733 a 29-day one), and M05L only exists in leap years
        // (5730; 5732 is common).
        assert_eq!(
            calendar_month_day_reference("hebrew", 1, 1, None),
            Some(iso(1972, 9, 9))
        );
        assert_eq!(
            calendar_month_day_reference("hebrew", 2, 30, None),
            Some(iso(1971, 11, 18))
        );
        assert_eq!(
            calendar_month_day_reference("hebrew", 2, 29, None),
            Some(iso(1972, 11, 6))
        );
        assert_eq!(
            calendar_month_day_reference("hebrew", 6, 1, Some("M05L")),
            Some(iso(1970, 2, 7))
        );
        assert_eq!(
            calendar_month_day_reference("hebrew", 6, 1, None),
            Some(iso(1972, 2, 16))
        );
        // The YearMonth reference day: 1445-12-01 → 2024-06-08.
        assert_eq!(
            calendar_year_month_to_iso("islamic-civil", 1445, 12),
            Some(iso(2024, 6, 8))
        );
    }

    #[test]
    fn islamic_roundtrip() {
        for (year, month, day) in [(1, 1, 1), (1445, 12, 25), (1391, 12, 25), (1000, 7, 15)] {
            let epoch = ISLAMIC_CIVIL_EPOCH;
            let rd = islamic_to_fixed(year, month, day, epoch);
            assert_eq!(islamic_from_fixed(rd, epoch), (year, month, day));
        }
    }

    #[test]
    fn islamic_leap_years() {
        // The first cycle's leap years: 2, 5, 7, 10, 13, 16, 18, 21, 24, 26, 29.
        let leaps: Vec<i64> = (1..=30).filter(|&y| islamic_leap_year(y)).collect();
        assert_eq!(leaps, vec![2, 5, 7, 10, 13, 16, 18, 21, 24, 26, 29]);
        // A leap year has 355 days: 1445-01-01 + 354 = 1445-12-30 exists.
        assert_eq!(days_in_islamic_month(1445, 12), 30);
        assert_eq!(days_in_islamic_month(1444, 12), 29);
    }

    #[test]
    fn hebrew_date_conversion() {
        // The reference-day-hebrew fixture: Tevet 5782 (month 4) begins
        // 2021-12-05 (1 Tishri 5782 = 2021-09-07).
        assert_eq!(
            calendar_date_to_iso("hebrew", 5782, 4, 1),
            Some(iso(2021, 12, 5))
        );
        // The same date as a year-month: 5782 M04 → ISO 2021-12 with the
        // reference day 5.
        assert_eq!(
            calendar_year_month_to_iso("hebrew", 5782, 4),
            Some(iso(2021, 12, 5))
        );
        // 5782 is a leap year (13 months); 5783 is not.
        assert!(hebrew_leap_year(5782));
        assert!(!hebrew_leap_year(5783));
        assert_eq!(hebrew_months_in_year(5782), 13);
        assert_eq!(hebrew_months_in_year(5783), 12);
        // The round trip.
        let rd = hebrew_to_fixed(5782, 4, 5);
        assert_eq!(hebrew_from_fixed(rd), (5782, 4, 5));
    }

    #[test]
    fn hebrew_month_codes() {
        // M06 is Adar in a common year (month 6) and Adar II in a leap year
        // (month 7); M05L is Adar I in leap years only.
        assert_eq!(
            resolve_calendar_month("hebrew", 5783, None, Some("M06")),
            Some(6)
        );
        assert_eq!(
            resolve_calendar_month("hebrew", 5782, None, Some("M06")),
            Some(7)
        );
        assert_eq!(
            resolve_calendar_month("hebrew", 5782, None, Some("M05L")),
            Some(6)
        );
        assert_eq!(
            resolve_calendar_month("hebrew", 5783, None, Some("M05L")),
            None
        );
        assert_eq!(
            resolve_calendar_month("hebrew", 5782, None, Some("M12")),
            Some(13)
        );
        assert_eq!(
            resolve_calendar_month("hebrew", 5783, None, Some("M12")),
            Some(12)
        );
        // The numeric month is returned as-is (the callers bound it against
        // the year's month count: constrain clamps, reject errors).
        assert_eq!(
            resolve_calendar_month("hebrew", 5783, Some(13), None),
            Some(13)
        );
    }

    #[test]
    fn hebrew_year_lengths() {
        // The corpus's keviah table (daysInYear/basic-hebrew.js): the
        // postponement rules (Tuesday-only GaTaRaD, Lo ADU Rosh) plus the
        // 356/382-day corrections must reproduce every entry.
        let table = [
            (5730, 383),
            (5731, 354),
            (5732, 355),
            (5733, 383),
            (5734, 355),
            (5735, 354),
            (5736, 385),
            (5737, 353),
            (5738, 384),
            (5739, 355),
            (5740, 355),
            (5741, 383),
            (5742, 354),
            (5743, 355),
            (5744, 385),
            (5745, 354),
            (5746, 383),
            (5747, 355),
            (5748, 354),
            (5749, 383),
            (5750, 355),
            (5751, 354),
            (5752, 385),
            (5753, 353),
            (5754, 355),
            (5755, 384),
            (5756, 355),
            (5757, 383),
            (5758, 354),
            (5759, 355),
            (5760, 385),
            (5761, 353),
            (5762, 354),
            (5763, 385),
            (5764, 355),
            (5765, 383),
            (5766, 354),
            (5767, 355),
            (5768, 383),
            (5769, 354),
            (5770, 355),
            (5771, 385),
            (5772, 354),
            (5773, 353),
            (5774, 385),
            (5775, 354),
            (5776, 385),
            (5777, 353),
            (5778, 354),
            (5779, 385),
            (5780, 355),
            (5781, 353),
            (5782, 384),
            (5783, 355),
            (5784, 383),
            (5785, 355),
            (5786, 354),
            (5787, 385),
            (5788, 355),
            (5789, 354),
            (5790, 383),
            (5791, 355),
            (5792, 354),
            (5793, 383),
            (5794, 355),
            (5795, 385),
            (5796, 354),
            (5797, 353),
            (5798, 385),
            (5799, 354),
            (5800, 355),
            (5801, 383),
            (5802, 354),
            (5803, 385),
            (5804, 353),
            (5805, 355),
            (5806, 384),
            (5807, 355),
            (5808, 353),
            (5809, 384),
        ];
        for (y, want) in table {
            assert_eq!(hebrew_year_length(y), want, "year {y}");
        }
        // The reference-day-hebrew anchor: Cheshvan 5732 (the 1972 reference
        // year) has 30 days, so M02-31 constrains to 30 (constrain-to-leap-day).
        assert_eq!(hebrew_month_length(5732, 2), 30);
        assert_eq!(hebrew_month_length(5781, 2), 29);
    }

    #[test]
    fn hebrew_calendar_date_add_until() {
        // leap-year-until: 1967-02-28 hebrew -> 1968-03-01 hebrew is
        // 0y 12m 13d (the months balance against the leap year's 13).
        let c = calendar_date_to_iso("hebrew", 5727, 6, 19).unwrap();
        let d = calendar_date_to_iso("hebrew", 5728, 6, 2).unwrap();
        assert_eq!(
            calendar_date_until("hebrew", c, d, iso::Unit::Year),
            (0, 12, 0, 13)
        );
        // The reverse direction (d.since(c)) is asymmetric: adding -1 year
        // to Adar 5728 lands in Adar II 5727, still after c, so the diff is
        // -1y 0m -13d (leap-year-since expects 1y 0m 13d after negation).
        assert_eq!(
            calendar_date_until("hebrew", d, c, iso::Unit::Year),
            (-1, 0, 0, -13)
        );
        // A leap month (Adar I) survives the year add only in a leap year:
        // 5000-06-01 + 1 year rejects in the common year 5001 (the
        // leap-month-hebrew-numerical-months fixture).
        let start = calendar_date_to_iso("hebrew", 5000, 6, 1).unwrap();
        assert!(calendar_date_add("hebrew", start, 1, 0, 0, 0, false).is_none());
        assert!(calendar_date_add("hebrew", start, 1, 1, 0, 0, false).is_none());
        // With constrain the code folds to Adar.
        assert_eq!(
            calendar_date_add("hebrew", start, 1, 0, 0, 0, true),
            calendar_date_to_iso("hebrew", 5001, 6, 1)
        );
        // The month code steps through the year's own sequence: Adar I 5727
        // + 12 months lands in Shevat 5728 (the until loop above).
        assert_eq!(
            calendar_date_add("hebrew", c, 0, 12, 0, 0, true),
            calendar_date_to_iso("hebrew", 5728, 5, 19)
        );
    }

    #[test]
    fn islamic_calendar_date_add() {
        // basic-islamic-civil: 1445-01-01 + 12 months = 1446-01-01; a
        // large month count balances across years.
        let muharram = calendar_date_to_iso("islamic-civil", 1445, 1, 1).unwrap();
        assert_eq!(
            calendar_date_add("islamic-civil", muharram, 0, 12, 0, 0, true),
            calendar_date_to_iso("islamic-civil", 1446, 1, 1)
        );
        let start = calendar_date_to_iso("islamic-civil", 1400, 1, 1).unwrap();
        assert_eq!(
            calendar_date_add("islamic-civil", start, 0, 100, 0, 0, true),
            calendar_date_to_iso("islamic-civil", 1408, 5, 1)
        );
        // Day regulation against the tabular month length.
        let safar = calendar_date_to_iso("islamic-civil", 1444, 2, 29).unwrap();
        assert_eq!(
            calendar_date_add("islamic-civil", safar, 1, 0, 0, 0, true),
            calendar_date_to_iso("islamic-civil", 1445, 2, 29)
        );
    }

    #[test]
    fn tbla_differs_by_epoch() {
        // The same tabular date one epoch apart: tbla is one day earlier.
        let (y, m, d) = calendar_date_to_iso("islamic-tbla", 1445, 12, 25).unwrap();
        let (cy, cm, cd) = calendar_date_to_iso("islamic-civil", 1445, 12, 25).unwrap();
        let rd_tbla = iso::iso_date_to_epoch_days(y, m - 1, d) + RD_OFFSET;
        let rd_civil = iso::iso_date_to_epoch_days(cy, cm - 1, cd) + RD_OFFSET;
        assert_eq!(rd_civil - rd_tbla, 1);
    }
}
