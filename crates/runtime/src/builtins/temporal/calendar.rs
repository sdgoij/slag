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

/// CalendarDateFromFields for the tabular Islamic calendars: the calendar
/// (year, month, day) → ISO date. `None` for the calendars that pass
/// through as ISO.
pub fn calendar_date_to_iso(
    calendar: &str,
    year: i64,
    month: i64,
    day: i64,
) -> Option<(i64, i64, i64)> {
    let epoch = islamic_epoch(calendar)?;
    let rd = islamic_to_fixed(year, month, day, epoch);
    Some(iso::epoch_days_to_iso_date(rd - RD_OFFSET))
}

/// The reverse of `calendar_date_to_iso` (CalendarDateToIso for the tabular
/// Islamic calendars): ISO date → calendar (year, month, day).
pub fn calendar_iso_to_date(calendar: &str, y: i64, m: i64, d: i64) -> Option<(i64, i64, i64)> {
    let epoch = islamic_epoch(calendar)?;
    let rd = iso::iso_date_to_epoch_days(y, m - 1, d) + RD_OFFSET;
    Some(islamic_from_fixed(rd, epoch))
}

/// The ISO year-month containing the calendar month's first day, plus the
/// reference day (the ISO day of that first day) — CalendarYearMonthFromFields
/// for the tabular Islamic calendars.
pub fn calendar_year_month_to_iso(
    calendar: &str,
    year: i64,
    month: i64,
) -> Option<(i64, i64, i64)> {
    let epoch = islamic_epoch(calendar)?;
    let rd = islamic_to_fixed(year, month, 1, epoch);
    let (y, m, d) = iso::epoch_days_to_iso_date(rd - RD_OFFSET);
    Some((y, m, d))
}

/// The reference ISO date for a calendar month-day: the date in the latest
/// ISO year at or before 1972 where the month-day exists (CalendarMonthDay-
/// FromFields for the tabular Islamic calendars — test262
/// equals/canonicalize-calendar.js pins 1972-02-11 for islamic M12-25).
pub fn calendar_month_day_reference(
    calendar: &str,
    month: i64,
    day: i64,
) -> Option<(i64, i64, i64)> {
    let epoch = islamic_epoch(calendar)?;
    let rd_1972 = iso::iso_date_to_epoch_days(1972, 0, 1) + RD_OFFSET;
    let year0 = (30 * (rd_1972 - epoch) + 10646) / 10631;
    // The month-day exists in most years; try the years around 1972 and
    // take the latest date at or before ISO 1972.
    for year in [year0 + 1, year0, year0 - 1] {
        if day <= days_in_islamic_month(year, month) {
            let rd = islamic_to_fixed(year, month, day, epoch);
            let (y, m, d) = iso::epoch_days_to_iso_date(rd - RD_OFFSET);
            if y <= 1972 {
                return Some((y, m, d));
            }
        }
    }
    None
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
            calendar_month_day_reference("islamic-civil", 12, 25),
            Some(iso(1972, 2, 11))
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
    fn tbla_differs_by_epoch() {
        // The same tabular date one epoch apart: tbla is one day earlier.
        let (y, m, d) = calendar_date_to_iso("islamic-tbla", 1445, 12, 25).unwrap();
        let (cy, cm, cd) = calendar_date_to_iso("islamic-civil", 1445, 12, 25).unwrap();
        let rd_tbla = iso::iso_date_to_epoch_days(y, m - 1, d) + RD_OFFSET;
        let rd_civil = iso::iso_date_to_epoch_days(cy, cm - 1, cd) + RD_OFFSET;
        assert_eq!(rd_civil - rd_tbla, 1);
    }
}
