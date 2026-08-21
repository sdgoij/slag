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
    // The molad arithmetic floors the carries (negative years otherwise
    // drift — the withCalendar/extreme-dates fixture pins the year −268058
    // month starts).
    let hours_elapsed = 5 + 12 * months + 793 * months.div_euclid(1080) + parts_elapsed / 1080;
    let mut day = 1 + 29 * months + hours_elapsed.div_euclid(24);
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

/// The tabular Islamic date → fixed date. The leap-day count runs euclidean
/// (the negative years of the roundtrip anchors need floor semantics).
fn islamic_to_fixed(year: i64, month: i64, day: i64, epoch: i64) -> i64 {
    epoch - 1
        + (year - 1) * 354
        + (3 + 11 * year).div_euclid(30)
        + 29 * (month - 1)
        + month / 2
        + day
}

/// The fixed date → tabular Islamic date (the ISO → Islamic direction the
/// DateTimeFormat calendar-field conversion uses). The divisions run
/// euclidean: the negative years (the roundtrip-from-property-bag anchor ISO
/// 1-01-01 = islamic-civil -640 M05-18) need floor semantics.
fn islamic_from_fixed(rd: i64, epoch: i64) -> (i64, i64, i64) {
    let year = (30 * (rd - epoch) + 10646).div_euclid(10631);
    let prior = rd - islamic_to_fixed(year, 1, 1, epoch);
    let month = (11 * prior + 330).div_euclid(325);
    let day = rd - islamic_to_fixed(year, month, 1, epoch) + 1;
    (year, month, day)
}

// --- The solar calendars (buddhist, coptic, ethiopic, ethioaa, indian,
// --- persian) ---
//
// Fixed-date calendars: a year start (an epoch RD) plus month lengths. The
// corpus pins the epochs through the roundtrip-from-property-bag anchors and
// the leap rules through the inLeapYear/daysInYear fixtures; the persian
// nowruz table is the Iranian calendar authority data the corpus pins for
// AP 1206-1498, with the 33-year arithmetic cycle outside.

/// The Coptic epoch: 1 Thout 1 = ISO 284-08-29 (RD).
const COPTIC_EPOCH: i64 = 103_605;

/// The Ethiopic anchor: year -7 M01-01 = ISO 0-08-27 (RD; the corpus's
/// roundtrip anchor ISO 1-01-01 = ethiopic -7 M05-08 pins the epoch).
const ETHIOPIC_BASE: i64 = -126;

/// The Persian epoch: 1 Farvardin 1 AP = ISO 622-03-21 (RD).
const PERSIAN_EPOCH: i64 = 226_895;

/// The Buddhist calendar is the proleptic Gregorian year + 543 (the Thai
/// solar calendar; the months and days are the ISO ones).
fn buddhist_to_fixed(year: i64, month: i64, day: i64) -> i64 {
    iso::iso_date_to_epoch_days(year - 543, month - 1, day) + RD_OFFSET
}

fn buddhist_from_fixed(rd: i64) -> (i64, i64, i64) {
    let (y, m, d) = iso::epoch_days_to_iso_date(rd - RD_OFFSET);
    (y + 543, m, d)
}

/// The Coptic calendar: 12 months of 30 days plus a 5/6-day epagomenal
/// month; the leap years are 3 mod 4 (the inLeapYear fixture pins 1687,
/// 1691, ...).
fn coptic_leap_year(year: i64) -> bool {
    year.rem_euclid(4) == 3
}

/// 1 Thout of year y (RD): 365 days per year plus the leap days of the
/// earlier years.
fn coptic_year_start(year: i64) -> i64 {
    COPTIC_EPOCH + 365 * (year - 1) + year.div_euclid(4)
}

fn coptic_to_fixed(year: i64, month: i64, day: i64) -> i64 {
    coptic_year_start(year) + 30 * (month - 1) + day - 1
}

fn coptic_from_fixed(rd: i64) -> (i64, i64, i64) {
    let mut year = (4 * (rd - COPTIC_EPOCH) + 1463).div_euclid(1461);
    while coptic_year_start(year) > rd {
        year -= 1;
    }
    while coptic_year_start(year + 1) <= rd {
        year += 1;
    }
    let prior = rd - coptic_year_start(year);
    let month = (prior / 30).min(12) + 1;
    let day = prior - 30 * (month - 1) + 1;
    (year, month, day)
}

/// The Ethiopic calendar: the same 13-month structure as Coptic with the
/// year field offset -7 from ISO (year -7 M01-01 = ISO 0-08-27); the year
/// field is the Amete Alem numbering the corpus pins (ISO 2000 → year
/// 1992, ISO 1 → year -7), and the era getters derive the am/aa era from it.
fn ethiopic_year_start(year: i64) -> i64 {
    ETHIOPIC_BASE + 365 * (year + 7) + year.div_euclid(4) + 2
}

fn ethiopic_to_fixed(year: i64, month: i64, day: i64) -> i64 {
    ethiopic_year_start(year) + 30 * (month - 1) + day - 1
}

fn ethiopic_from_fixed(rd: i64) -> (i64, i64, i64) {
    let mut year = (4 * (rd - ETHIOPIC_BASE) - 1).div_euclid(1461) - 7;
    while ethiopic_year_start(year) > rd {
        year -= 1;
    }
    while ethiopic_year_start(year + 1) <= rd {
        year += 1;
    }
    let prior = rd - ethiopic_year_start(year);
    let month = (prior / 30).min(12) + 1;
    let day = prior - 30 * (month - 1) + 1;
    (year, month, day)
}

/// The Ethiopic Amete Alem calendar: the ethiopic year field plus 5500 (the
/// era-monthcode aa/am split: the year field < 1 is the aa era, ≥ 1 the am
/// era, with aa eraYear = year + 5500).
fn ethioaa_to_fixed(year: i64, month: i64, day: i64) -> i64 {
    ethiopic_year_start(year - 5500) + 30 * (month - 1) + day - 1
}

fn ethioaa_from_fixed(rd: i64) -> (i64, i64, i64) {
    let (y, m, d) = ethiopic_from_fixed(rd);
    (y + 5500, m, d)
}

/// The Indian national (Saka) calendar: Chaitra 1 = ISO (year+78)-03-21 in
/// a leap year, 03-22 otherwise; month 1 has 30/31 days (31 in leap years),
/// months 2-6 have 31, months 7-12 have 30 (the daysInMonth fixture pins the
/// leap day at 31 Chaitra).
fn saka_leap_year(year: i64) -> bool {
    iso::is_leap_year(year + 78)
}

fn saka_year_start(year: i64) -> i64 {
    let (y, m, d) = if saka_leap_year(year) {
        (year + 78, 3, 21)
    } else {
        (year + 78, 3, 22)
    };
    iso::iso_date_to_epoch_days(y, m - 1, d) + RD_OFFSET
}

fn saka_month_length(year: i64, month: i64) -> i64 {
    match month {
        1 => i64::from(saka_leap_year(year)) + 30,
        2..=6 => 31,
        _ => 30,
    }
}

fn saka_to_fixed(year: i64, month: i64, day: i64) -> i64 {
    saka_year_start(year) + (1..month).map(|m| saka_month_length(year, m)).sum::<i64>() + day - 1
}

fn saka_from_fixed(rd: i64) -> (i64, i64, i64) {
    let (iy, im, id) = iso::epoch_days_to_iso_date(rd - RD_OFFSET);
    let mut year = if (im, id) < (3, 21) { iy - 79 } else { iy - 78 };
    while saka_year_start(year) > rd {
        year -= 1;
    }
    while saka_year_start(year + 1) <= rd {
        year += 1;
    }
    let prior = rd - saka_year_start(year);
    let mut month = 1;
    let mut rem = prior;
    while month < 12 && rem >= saka_month_length(year, month) {
        rem -= saka_month_length(year, month);
        month += 1;
    }
    (year, month, rem + 1)
}

// The Persian (AP) calendar: the year starts at the nowruz the authority
// data pins (the nowruz day of March for AP 1206-1498; the 33-year cycle
// with 8 leap years outside). Months 1-6 have 31 days, 7-11 30, and 12 has
// 29 (30 in leap years).

const PERSIAN_TABLE_START: i64 = 1206;
const PERSIAN_TABLE_END: i64 = 1498;

/// The nowruz day of March for AP 1206-1498 (the persian-new-year-dates
/// fixture: the Iranian calendar authority data; nowruz(Y) = ISO
/// (Y+621)-03-DAY).
const PERSIAN_NOWRUZ_DAY: [u8; 293] = [
    22, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21,
    21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 20, 21, 21, 21, 20, 21, 21, 21, 20, 21, 21,
    21, 20, 21, 21, 21, 20, 21, 21, 21, 20, 21, 21, 21, 20, 21, 21, 21, 20, 21, 21, 21, 20, 20, 21,
    21, 21, 21, 22, 22, 21, 21, 22, 22, 21, 21, 22, 22, 21, 21, 22, 22, 21, 21, 22, 22, 21, 21, 22,
    22, 21, 21, 22, 22, 21, 21, 21, 22, 21, 21, 21, 22, 21, 21, 21, 22, 21, 21, 21, 22, 21, 21, 21,
    22, 21, 21, 21, 22, 21, 21, 21, 22, 21, 21, 21, 22, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21,
    21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21,
    21, 20, 21, 21, 21, 20, 21, 21, 21, 20, 21, 21, 21, 20, 21, 21, 21, 20, 21, 21, 21, 20, 21, 21,
    21, 20, 21, 21, 21, 20, 21, 21, 21, 20, 20, 21, 21, 20, 20, 21, 21, 20, 20, 21, 21, 20, 20, 21,
    21, 20, 20, 21, 21, 20, 20, 21, 21, 20, 20, 21, 21, 20, 20, 21, 21, 20, 20, 20, 21, 20, 20, 20,
    21, 20, 20, 20, 21, 20, 20, 20, 21, 20, 20, 20, 21, 20, 20, 20, 21, 20, 20, 20, 21, 20, 20, 20,
    21, 20, 20, 20, 20, 20, 20, 20, 20, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21,
    21, 21, 21, 21, 21,
];

/// The 33-year arithmetic cycle (8 leap years per cycle: the years 1, 5, 9,
/// 13, 17, 22, 26, 30 of each cycle are leap).
fn persian_leap_arithmetic(year: i64) -> bool {
    (25 * year + 11).rem_euclid(33) < 8
}

fn persian_year_start_arithmetic(year: i64) -> i64 {
    if year < 1 {
        let mut rd = PERSIAN_EPOCH;
        for k in (year..1).rev() {
            rd -= 365 + i64::from(persian_leap_arithmetic(k));
        }
        rd
    } else {
        let cycles = (year - 1) / 33;
        let mut rd = PERSIAN_EPOCH + cycles * 12_053;
        for k in (cycles * 33 + 1)..year {
            rd += 365 + i64::from(persian_leap_arithmetic(k));
        }
        rd
    }
}

/// 1 Farvardin of the Persian year (RD): the authority table inside
/// 1206-1498, the arithmetic cycle outside.
fn persian_year_start(year: i64) -> i64 {
    if (PERSIAN_TABLE_START..=PERSIAN_TABLE_END).contains(&year) {
        let day = PERSIAN_NOWRUZ_DAY[(year - PERSIAN_TABLE_START) as usize] as i64;
        iso::iso_date_to_epoch_days(year + 621, 2, day) + RD_OFFSET
    } else {
        persian_year_start_arithmetic(year)
    }
}

fn persian_month_length(year: i64, month: i64) -> i64 {
    match month {
        1..=6 => 31,
        7..=11 => 30,
        _ => i64::from(persian_leap_year(year)) + 29,
    }
}

/// A Persian year is leap when the next year's nowruz is one day later than
/// the year's own (the authority table and the arithmetic cycle agree here).
fn persian_leap_year(year: i64) -> bool {
    persian_year_start(year + 1) - persian_year_start(year) == 366
}

fn persian_to_fixed(year: i64, month: i64, day: i64) -> i64 {
    persian_year_start(year)
        + (1..month)
            .map(|m| persian_month_length(year, m))
            .sum::<i64>()
        + day
        - 1
}

fn persian_from_fixed(rd: i64) -> (i64, i64, i64) {
    let (iy, _, _) = iso::epoch_days_to_iso_date(rd - RD_OFFSET);
    let mut year = iy - 621;
    while persian_year_start(year) > rd {
        year -= 1;
    }
    while persian_year_start(year + 1) <= rd {
        year += 1;
    }
    let prior = rd - persian_year_start(year);
    let mut month = 1;
    let mut rem = prior;
    while month < 12 && rem >= persian_month_length(year, month) {
        rem -= persian_month_length(year, month);
        month += 1;
    }
    (year, month, rem + 1)
}

// --- The islamic-umalqura calendar (the Umm al-Qura calendar: the real
// --- astronomical month layouts for AH 1300-1500, from the ICU umalqura
// --- table (each year a 12-bit mask: bit set = 30-day month, cleared = 29,
// --- month 1 the high bit); outside the table the calendar falls back to
// --- islamic-civil (the extreme-dates fixture pins the fallback roundtrips).
// --- The anchor AH 1390 M01-01 = ISO 1970-03-09 (RD 719230); the corpus pin
// --- ISO 2000-01-01 = AH 1420 M09-24 (roundtrip-from-property-bag) fixes it.

const UMALQURA_TABLE_START: i64 = 1300;
const UMALQURA_TABLE_END: i64 = 1500;
const UMALQURA_ANCHOR_YEAR: i64 = 1390;
/// ISO 1970-03-09 (RD): the corpus pin ISO 2000-01-01 = AH 1420 M09-24
/// (roundtrip-from-property-bag) fixes the anchor.
const UMALQURA_ANCHOR_RD: i64 = 719_230;

/// The ICU umalqura month masks for AH 1300-1500 (year 1300 first): each
/// mask bit 11..0 is the 30-day flag of months 1..12. The year totals match
/// the daysInYear/inLeapYear corpus pins (1390-1469 has exactly 30 leap
/// years) and the month positions match the month-boundary/basic fixtures.
const UMALQURA_MONTHLENGTH: [u16; 201] = [
    0x0AAA, 0x0D54, 0x0EC9, 0x06D4, 0x06EA, 0x036C, 0x0AAD, 0x0555, 0x06A9, 0x0792, 0x0BA9, 0x05D4,
    0x0ADA, 0x055C, 0x0D2D, 0x0695, 0x074A, 0x0B54, 0x0B6A, 0x05AD, 0x04AE, 0x0A4F, 0x0517, 0x068B,
    0x06A5, 0x0AD5, 0x02D6, 0x095B, 0x049D, 0x0A4D, 0x0D26, 0x0D95, 0x05AC, 0x09B6, 0x02BA, 0x0A5B,
    0x052B, 0x0A95, 0x06CA, 0x0AE9, 0x02F4, 0x0976, 0x02B6, 0x0956, 0x0ACA, 0x0BA4, 0x0BD2, 0x05D9,
    0x02DC, 0x096D, 0x054D, 0x0AA5, 0x0B52, 0x0BA5, 0x05B4, 0x09B6, 0x0557, 0x0297, 0x054B, 0x06A3,
    0x0752, 0x0B65, 0x056A, 0x0AAB, 0x052B, 0x0C95, 0x0D4A, 0x0DA5, 0x05CA, 0x0AD6, 0x0957, 0x04AB,
    0x094B, 0x0AA5, 0x0B52, 0x0B6A, 0x0575, 0x0276, 0x08B7, 0x045B, 0x0555, 0x05A9, 0x05B4, 0x09DA,
    0x04DD, 0x026E, 0x0936, 0x0AAA, 0x0D54, 0x0DB2, 0x05D5, 0x02DA, 0x095B, 0x04AB, 0x0A55, 0x0B49,
    0x0B64, 0x0B71, 0x05B4, 0x0AB5, 0x0A55, 0x0D25, 0x0E92, 0x0EC9, 0x06D4, 0x0AE9, 0x096B, 0x04AB,
    0x0A93, 0x0D49, 0x0DA4, 0x0DB2, 0x0AB9, 0x04BA, 0x0A5B, 0x052B, 0x0A95, 0x0B2A, 0x0B55, 0x055C,
    0x04BD, 0x023D, 0x091D, 0x0A95, 0x0B4A, 0x0B5A, 0x056D, 0x02B6, 0x093B, 0x049B, 0x0655, 0x06A9,
    0x0754, 0x0B6A, 0x056C, 0x0AAD, 0x0555, 0x0B29, 0x0B92, 0x0BA9, 0x05D4, 0x0ADA, 0x055A, 0x0AAB,
    0x0595, 0x0749, 0x0764, 0x0BAA, 0x05B5, 0x02B6, 0x0A56, 0x0E4D, 0x0B25, 0x0B52, 0x0B6A, 0x05AD,
    0x02AE, 0x092F, 0x0497, 0x064B, 0x06A5, 0x06AC, 0x0AD6, 0x055D, 0x049D, 0x0A4D, 0x0D16, 0x0D95,
    0x05AA, 0x05B5, 0x02DA, 0x095B, 0x04AD, 0x0595, 0x06CA, 0x06E4, 0x0AEA, 0x04F5, 0x02B6, 0x0956,
    0x0AAA, 0x0B54, 0x0BD2, 0x05D9, 0x02EA, 0x096D, 0x04AD, 0x0A95, 0x0B4A, 0x0BA5, 0x05B2, 0x09B5,
    0x04D6, 0x0A97, 0x0547, 0x0693, 0x0749, 0x0B55, 0x056A, 0x0A6B, 0x052B,
];

/// The month layouts (29/30 per month) of the umalqura table years.
fn umalqura_month_lengths(year: i64) -> Option<[u8; 12]> {
    if !(UMALQURA_TABLE_START..=UMALQURA_TABLE_END).contains(&year) {
        return None;
    }
    let mask = UMALQURA_MONTHLENGTH[(year - UMALQURA_TABLE_START) as usize];
    Some(std::array::from_fn(|i| {
        if (mask >> (11 - i)) & 1 == 1 { 30 } else { 29 }
    }))
}

/// The umalqura year length (355 in a leap year, 354 otherwise): the table
/// years from the masks, the islamic-civil classification outside (the
/// fallback calendar of the out-of-table years).
fn umalqura_year_length(year: i64) -> Option<i64> {
    if let Some(lens) = umalqura_month_lengths(year) {
        Some(lens.iter().map(|&l| l as i64).sum())
    } else {
        Some(if islamic_leap_year(year) { 355 } else { 354 })
    }
}

/// The umalqura month length (29/30): the table years from the masks, the
/// islamic-civil month outside (the NonISODateSurpasses day balance and the
/// era-boundary adds exercise out-of-table years).
fn umalqura_days_in_month(year: i64, month: i64) -> Option<i64> {
    if let Some(lens) = umalqura_month_lengths(year) {
        if (1..=12).contains(&month) {
            Some(lens[(month - 1) as usize] as i64)
        } else {
            None
        }
    } else if (1..=12).contains(&month) {
        Some(days_in_islamic_month(year, month))
    } else {
        None
    }
}

/// 1 Muharram of the umalqura year (RD): the anchor AH 1390 M01-01 = ISO
/// 1970-01-01 plus the cumulative year lengths.
fn umalqura_year_start(year: i64) -> Option<i64> {
    if year == UMALQURA_ANCHOR_YEAR {
        return Some(UMALQURA_ANCHOR_RD);
    }
    if year < UMALQURA_ANCHOR_YEAR {
        let mut rd = UMALQURA_ANCHOR_RD;
        for k in (year..UMALQURA_ANCHOR_YEAR).rev() {
            rd -= umalqura_year_length(k)?;
        }
        Some(rd)
    } else {
        let mut rd = UMALQURA_ANCHOR_RD;
        for k in UMALQURA_ANCHOR_YEAR..year {
            rd += umalqura_year_length(k)?;
        }
        Some(rd)
    }
}

/// The umalqura date → fixed date: the table inside [1300, 1500], the
/// islamic-civil arithmetic outside (the extreme-dates fixture pins the
/// fallback: the umalqura min/max roundtrips equal the civil ones).
fn umalqura_to_fixed(year: i64, month: i64, day: i64) -> Option<i64> {
    if (UMALQURA_TABLE_START..=UMALQURA_TABLE_END).contains(&year) {
        let start = umalqura_year_start(year)?;
        let lens = umalqura_month_lengths(year)?;
        if (1..=12).contains(&month) && day >= 1 && day <= lens[(month - 1) as usize] as i64 {
            Some(
                start
                    + lens[..(month - 1) as usize]
                        .iter()
                        .map(|&l| l as i64)
                        .sum::<i64>()
                    + day
                    - 1,
            )
        } else {
            None
        }
    } else {
        Some(islamic_to_fixed(year, month, day, ISLAMIC_CIVIL_EPOCH))
    }
}

fn umalqura_from_fixed(rd: i64) -> Option<(i64, i64, i64)> {
    // The ISO span of the table years; outside it the civil fallback. The
    // span is walked from the anchor through the accumulated year lengths.
    let first = umalqura_year_start(UMALQURA_TABLE_START)?;
    let last = umalqura_year_start(UMALQURA_TABLE_END + 1)?;
    if !(first..last).contains(&rd) {
        return Some(islamic_from_fixed(rd, ISLAMIC_CIVIL_EPOCH));
    }
    // Binary search the year whose span contains rd.
    let mut lo = UMALQURA_TABLE_START;
    let mut hi = UMALQURA_TABLE_END;
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        if umalqura_year_start(mid)? <= rd {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let year = lo;
    let start = umalqura_year_start(year)?;
    let lens = umalqura_month_lengths(year)?;
    let prior = rd - start;
    let mut month = 1;
    let mut rem = prior;
    while month < 12 && rem >= lens[(month - 1) as usize] as i64 {
        rem -= lens[(month - 1) as usize] as i64;
        month += 1;
    }
    Some((year, month, rem + 1))
}

// --- Chinese/Dangi (the lunisolar east-asian-traditional calendars) ---
//
// The tabular data (1898-2102) is the ICU4X china_data/korea_data/qing_data
// set that V8 (via temporal_rs → ICU4X) generates the corpus against; the
// pinned 1969-2048 fixture core verifies against it with zero problems. Years
// outside the table use the ICU4X `simple` calculation (mean solar terms and
// mean new moons), matching the ICU4X out-of-table behavior the fixtures
// exercise (e.g. the leap-month-with-year constructions at 1898, 1651, ...).

const LUNAR_TABLE_START: i64 = 1898;
const LUNAR_TABLE_END: i64 = 2102;

/// The simple-calculation offsets (UTC+8 Beijing for chinese, UTC+9 for
/// korean/dangi; the pre-1900 reference meridian UTC+1397/180 for both).
const LUNAR_UTC_PLUS_8_MS: i64 = 8 * 3_600_000;
const LUNAR_UTC_PLUS_9_MS: i64 = 9 * 3_600_000;
const LUNAR_BEIJING_MS: i64 = 1397 * 3_600_000 / 180;
const LUNAR_MS_PER_DAY: i64 = 86_400_000;
const LUNAR_MEAN_YEAR_MS: i64 = 31_556_952_000; // 146097/400 days
const LUNAR_MEAN_SOLAR_TERM_MS: i64 = LUNAR_MEAN_YEAR_MS / 12;
// 295305888531/10000000000 days (the mean synodic month on 2000-01-01); the
// product overflows i64, so the division runs in i128.
const LUNAR_MEAN_SYNODIC_MS: i64 =
    (295_305_888_531_i128 * (LUNAR_MS_PER_DAY as i128) / 10_000_000_000) as i64;

/// The largest moment `base + n * period` on or before `rata_die`, as an RD
/// plus the milliseconds into the day (the ICU4X simple.rs port).
fn lunar_periodic_on_or_before(
    rata_die: i64,
    base_rd: i64,
    base_ms: i64,
    period_ms: i64,
) -> (i64, i64) {
    let diff = (rata_die - base_rd) * LUNAR_MS_PER_DAY - base_ms;
    let n = (diff + LUNAR_MS_PER_DAY - 1).div_euclid(period_ms);
    let millis = base_rd * LUNAR_MS_PER_DAY + base_ms + n * period_ms;
    (
        millis.div_euclid(LUNAR_MS_PER_DAY),
        millis.rem_euclid(LUNAR_MS_PER_DAY),
    )
}

/// `moment + duration` (the ICU4X LocalMoment Add).
fn lunar_add_ms(rd: i64, ms: i64, duration_ms: i64) -> (i64, i64) {
    let t = ms + duration_ms;
    (
        rd + t.div_euclid(LUNAR_MS_PER_DAY),
        t.rem_euclid(LUNAR_MS_PER_DAY),
    )
}

/// The ICU4X `simple` approximation of a lunisolar year: the mean solar terms
/// and mean new moons walked to the sui, yielding (new-year epoch day, the
/// 13 month lengths, the leap ordinal).
fn lunar_simple_year(offset_ms: i64, related_iso: i64) -> (i64, [i64; 13], Option<i64>) {
    // The anchor moments: 1999-12-22T07:44 (winter solstice) and
    // 2000-01-06T18:14 (new moon), both shifted to the calendar meridian.
    let solstice = (
        iso::iso_date_to_epoch_days(1999, 11, 22) + RD_OFFSET,
        7 * 3_600_000 + 44 * 60_000 + offset_ms,
    );
    let new_moon0 = (
        iso::iso_date_to_epoch_days(2000, 0, 6) + RD_OFFSET,
        18 * 3_600_000 + 14 * 60_000 + offset_ms,
    );
    let day_before = iso::iso_date_to_epoch_days(related_iso, 0, 1) - 1 + RD_OFFSET;
    let (mut major_rd, mut major_ms) =
        lunar_periodic_on_or_before(day_before, solstice.0, solstice.1, LUNAR_MEAN_YEAR_MS);
    let (mut nm_rd, nm_ms) =
        lunar_periodic_on_or_before(major_rd, new_moon0.0, new_moon0.1, LUNAR_MEAN_SYNODIC_MS);
    let (mut next_rd, mut next_ms) = lunar_add_ms(nm_rd, nm_ms, LUNAR_MEAN_SYNODIC_MS);
    // Walk to the sui's first month (the month of the 0th solar term).
    let mut solar_term = -2i64;
    let mut had_leap = false;
    while solar_term < 0 || (next_rd <= major_rd && !had_leap) {
        if next_rd <= major_rd && !had_leap {
            had_leap = true;
        } else {
            solar_term += 1;
            (major_rd, major_ms) = lunar_add_ms(major_rd, major_ms, LUNAR_MEAN_SOLAR_TERM_MS);
        }
        (nm_rd, _) = (next_rd, next_ms);
        (next_rd, next_ms) = lunar_add_ms(next_rd, next_ms, LUNAR_MEAN_SYNODIC_MS);
    }
    let new_year_rd = nm_rd;
    // The 12 solar terms, producing up to 13 months (a month without a major
    // term is the leap month).
    let mut lens = [0i64; 13];
    let mut leap_month = None;
    while solar_term < 12 || (next_rd <= major_rd && !had_leap) {
        let idx = (solar_term + i64::from(leap_month.is_some())) as usize;
        if idx < 13 {
            lens[idx] = if next_rd - nm_rd == 30 { 30 } else { 29 };
        }
        if next_rd <= major_rd && !had_leap {
            had_leap = true;
            leap_month = Some(solar_term + 1);
        } else {
            solar_term += 1;
            (major_rd, major_ms) = lunar_add_ms(major_rd, major_ms, LUNAR_MEAN_SOLAR_TERM_MS);
        }
        (nm_rd, _) = (next_rd, next_ms);
        (next_rd, next_ms) = lunar_add_ms(next_rd, next_ms, LUNAR_MEAN_SYNODIC_MS);
    }
    (new_year_rd - RD_OFFSET, lens, leap_month)
}

/// The (new-year epoch day, month lengths, leap ordinal) of a lunisolar year:
/// the table 1898-2102, the ICU4X simple calculation outside it (UTC+8/+9
/// above, the Beijing reference meridian below, matching the ICU4X Rules).
fn lunar_year_data(calendar: &str, year: i64) -> (i64, [i64; 13], Option<i64>) {
    if (LUNAR_TABLE_START..=LUNAR_TABLE_END).contains(&year) {
        let table = if calendar == "chinese" {
            &super::lunar_tables::CHINESE_TABLE
        } else {
            &super::lunar_tables::DANGI_TABLE
        };
        let (start, layout, leap) = table[(year - LUNAR_TABLE_START) as usize];
        let mut lens = [0i64; 13];
        for (i, len) in layout.iter().enumerate() {
            lens[i] = *len;
        }
        return (start, lens, if leap == 0 { None } else { Some(leap) });
    }
    if year < LUNAR_TABLE_START {
        lunar_simple_year(LUNAR_BEIJING_MS, year)
    } else if calendar == "chinese" {
        lunar_simple_year(LUNAR_UTC_PLUS_8_MS, year)
    } else {
        lunar_simple_year(LUNAR_UTC_PLUS_9_MS, year)
    }
}

/// The months in a lunisolar year (13 with a leap month).
fn lunar_months_in_year(leap: Option<i64>) -> i64 {
    if leap.is_some() { 13 } else { 12 }
}

/// The days in the month at ordinal position `month` (1-based).
fn lunar_days_in_month(calendar: &str, year: i64, month: i64) -> Option<i64> {
    let (_, lens, leap) = lunar_year_data(calendar, year);
    let n = lunar_months_in_year(leap);
    if (1..=n).contains(&month) {
        Some(lens[(month - 1) as usize])
    } else {
        None
    }
}

/// Parse a month code into (number, is_leap): M01-M12 and M01L-M12L.
fn lunar_parse_code(code: &str) -> Option<(i64, bool)> {
    if code.len() == 3 && code.starts_with('M') {
        let n = code[1..].parse::<i64>().ok()?;
        if (1..=12).contains(&n) {
            return Some((n, false));
        }
    } else if code.len() == 4 && code.starts_with('M') && code.ends_with('L') {
        let n = code[1..3].parse::<i64>().ok()?;
        if (1..=12).contains(&n) {
            return Some((n, true));
        }
    }
    None
}

/// The ordinal position of a month code in a year with the given leap ordinal
/// (`None` when the leap month does not exist in the year).
fn lunar_code_ordinal(number: i64, is_leap: bool, leap: Option<i64>) -> Option<i64> {
    if is_leap {
        if leap == Some(number + 1) {
            Some(number + 1)
        } else {
            None
        }
    } else {
        Some(number + i64::from(leap.is_some_and(|l| l <= number)))
    }
}

/// The month code of the month at ordinal position `month` (the leap month at
/// position l is M{l-1}L; the later months shift their numbers).
pub fn lunar_month_code(calendar: &str, year: i64, month: i64) -> String {
    let (_, _, leap) = lunar_year_data(calendar, year);
    match leap {
        Some(l) if month == l => format!("M{:02}L", l - 1),
        Some(l) if month > l => format!("M{:02}", month - 1),
        _ => format!("M{month:02}"),
    }
}

/// The calendar month code of a calendar month (the hebrew/chinese/dangi
/// codes depend on the year's leap structure; the rest are the month number).
pub fn calendar_month_code(calendar: &str, year: i64, month: i64) -> String {
    match calendar {
        "hebrew" => hebrew_month_code(year, month),
        "chinese" | "dangi" => lunar_month_code(calendar, year, month),
        _ => format!("M{month:02}"),
    }
}

/// Lunisolar date → fixed date (the new-year RD plus the month lengths).
fn lunar_to_fixed(calendar: &str, year: i64, month: i64, day: i64) -> Option<i64> {
    let (start, lens, leap) = lunar_year_data(calendar, year);
    let n = lunar_months_in_year(leap);
    if !(1..=n).contains(&month) || day < 1 || day > lens[(month - 1) as usize] {
        return None;
    }
    let mut rd = start + RD_OFFSET + day - 1;
    for len in &lens[..(month - 1) as usize] {
        rd += *len;
    }
    Some(rd)
}

/// Fixed date → lunisolar date (the year estimate then the month/day walk;
/// the chinese year differs from the ISO year by at most one at the January
/// boundary).
fn lunar_from_fixed(calendar: &str, rd: i64) -> (i64, i64, i64) {
    let iso_year = iso::epoch_days_to_iso_date(rd - RD_OFFSET).0;
    let mut year = iso_year;
    let mut guard = 0;
    while lunar_to_fixed(calendar, year + 1, 1, 1).is_some_and(|ny| ny <= rd) {
        year += 1;
        guard += 1;
        if guard > 10 {
            break;
        }
    }
    while lunar_to_fixed(calendar, year, 1, 1).is_some_and(|ny| ny > rd) {
        year -= 1;
        guard += 1;
        if guard > 10 {
            break;
        }
    }
    let mut month = 1;
    while let Some(next) = lunar_to_fixed(calendar, year, month + 1, 1) {
        if next > rd {
            break;
        }
        month += 1;
    }
    let day = rd - lunar_to_fixed(calendar, year, month, 1).unwrap_or(rd) + 1;
    (year, month, day)
}

/// The reference ISO date of a chinese/dangi month-day: the latest date in
/// [1900-01-01, 1972-12-31] with the month code and day, else the earliest in
/// [1973-01-01, 2035-12-31]; a leap month-day that never occurs in the window
/// folds to the regular month under constrain (reject returns `None`). The
/// corpus pins the resulting years (the spec's Table 6).
pub fn lunar_month_day_reference(
    calendar: &str,
    month: i64,
    day: i64,
    month_code: Option<&str>,
    constrain: bool,
) -> Option<(i64, i64, i64)> {
    let (number, is_leap) = match month_code {
        Some(code) => lunar_parse_code(code)?,
        None => (month, false),
    };
    if let Some(date) = lunar_reference_search(calendar, number, is_leap, day) {
        return Some(date);
    }
    if is_leap && constrain {
        return lunar_reference_search(calendar, number, false, day);
    }
    None
}

/// The two-window search: the latest date in ISO [1900-01-01, 1972-12-31]
/// with the month code and day, then the earliest in [1973-01-01,
/// 2035-12-31]. The chinese years 1899..=2102 cover the window bounds (a
/// year's late months spill into the next ISO January).
fn lunar_reference_search(
    calendar: &str,
    number: i64,
    is_leap: bool,
    day: i64,
) -> Option<(i64, i64, i64)> {
    for (descending, lo, hi) in [(true, 1900, 1972), (false, 1973, 2035)] {
        let range: Vec<i64> = if descending {
            (1899..=1973).rev().collect()
        } else {
            (1973..=2102).collect()
        };
        for y in range {
            let (start, lens, leap) = lunar_year_data(calendar, y);
            let Some(pos) = lunar_code_ordinal(number, is_leap, leap) else {
                continue;
            };
            if day < 1 || day > lens[(pos - 1) as usize] {
                continue;
            }
            let rd = start + RD_OFFSET + lens[..(pos - 1) as usize].iter().sum::<i64>() + day - 1;
            let (iy, im, id) = iso::epoch_days_to_iso_date(rd - RD_OFFSET);
            if (lo..=hi).contains(&iy) && (iy, im, id) >= (lo, 1, 1) && (iy, im, id) <= (hi, 12, 31)
            {
                return Some((iy, im, id));
            }
        }
    }
    None
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
        "islamic-umalqura" => umalqura_to_fixed(year, month, day)?,
        "hebrew" => hebrew_to_fixed(year, month, day),
        "chinese" | "dangi" => lunar_to_fixed(calendar, year, month, day)?,
        "buddhist" => buddhist_to_fixed(year, month, day),
        "roc" => iso::iso_date_to_epoch_days(year + 1911, month - 1, day) + RD_OFFSET,
        "coptic" => coptic_to_fixed(year, month, day),
        "ethiopic" => ethiopic_to_fixed(year, month, day),
        "ethioaa" => ethioaa_to_fixed(year, month, day),
        "indian" => saka_to_fixed(year, month, day),
        "persian" => persian_to_fixed(year, month, day),
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
        "islamic-umalqura" => umalqura_from_fixed(rd),
        "hebrew" => Some(hebrew_from_fixed(rd)),
        "chinese" | "dangi" => Some(lunar_from_fixed(calendar, rd)),
        "buddhist" => Some(buddhist_from_fixed(rd)),
        "roc" => Some({
            let (y, m, d) = iso::epoch_days_to_iso_date(rd - RD_OFFSET);
            (y - 1911, m, d)
        }),
        "coptic" => Some(coptic_from_fixed(rd)),
        "ethiopic" => Some(ethiopic_from_fixed(rd)),
        "ethioaa" => Some(ethioaa_from_fixed(rd)),
        "indian" => Some(saka_from_fixed(rd)),
        "persian" => Some(persian_from_fixed(rd)),
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
/// invalid for the year or the month and monthCode conflict (the
/// calendarresolvefields-error-ordering fixtures pin the month/monthCode
/// RangeError).
pub fn resolve_calendar_month(
    calendar: &str,
    year: i64,
    month: Option<i64>,
    month_code: Option<&str>,
) -> Option<i64> {
    // A provided monthCode resolves the month; a provided month must agree
    // with it (the conflict is a RangeError for the caller).
    let code_month = month_code.and_then(|code| resolve_calendar_month_code(calendar, year, code));
    if let (Some(m), Some(cm)) = (month, code_month)
        && m != cm
    {
        return None;
    }
    match month {
        Some(m) => {
            // No upper bound here: the callers regulate against the year's
            // month count (constrain clamps, reject errors).
            if m >= 1 { Some(m) } else { None }
        }
        None => code_month,
    }
}

/// The calendar month number of a monthCode alone (the month codes depend on
/// the year's leap status for hebrew; `None` when the code is malformed or
/// does not exist in the year).
fn resolve_calendar_month_code(calendar: &str, year: i64, code: &str) -> Option<i64> {
    if calendar == "hebrew" {
        return match code {
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
        };
    }
    if matches!(calendar, "chinese" | "dangi") {
        let (number, is_leap) = lunar_parse_code(code)?;
        return lunar_code_ordinal(number, is_leap, lunar_year_data(calendar, year).2);
    }
    // The pass-through calendars (iso8601, gregory, coptic, ...): M01-M12
    // (or M01-M13 for the 13-month calendars), bounded by the calendar's own
    // month count.
    if code.len() == 3 && code.starts_with('M') {
        let n = code[1..].parse::<i64>().ok()?;
        let max = calendar_months_in_year(calendar, year).unwrap_or(12);
        if (1..=max).contains(&n) {
            return Some(n);
        }
    }
    None
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
    if resolved.is_none() && constrain && month.is_none() {
        if calendar == "hebrew" && month_code == Some("M05L") && !hebrew_leap_year(year) {
            return Some(6); // Adar
        }
        // A chinese/dangi leap month that does not exist in the year folds to
        // the regular month of the same number (its ordinal shifts past the
        // year's leap month when the leap precedes it).
        if matches!(calendar, "chinese" | "dangi")
            && let Some(code) = month_code
            && let Some((number, true)) = lunar_parse_code(code)
        {
            return lunar_code_ordinal(number, false, lunar_year_data(calendar, year).2);
        }
    }
    resolved
}

/// The months in a calendar year (for the year-month overflow regulation).
pub fn calendar_months_in_year(calendar: &str, year: i64) -> Option<i64> {
    match calendar {
        "hebrew" => Some(hebrew_months_in_year(year)),
        "islamic-civil" | "islamic-tbla" | "islamic-umalqura" | "indian" | "persian" => Some(12),
        "coptic" | "ethiopic" | "ethioaa" => Some(13),
        "chinese" | "dangi" => Some(lunar_months_in_year(lunar_year_data(calendar, year).2)),
        _ => None,
    }
}

/// The days in a calendar month (for the overflow regulation). The
/// pass-through calendars' entries cover the month codes the corpus
/// constrains against (constrain-to-leap-day): the chinese/dangi lunar months
/// are the real 29/30 lengths, the coptic/ethiopic months are 30 with a
/// 5/6-day epagomenal month 13, persian months 1-6 are 31 (7-11 30, 12
/// 29/30), and indian is the same shape.
pub fn calendar_days_in_month(calendar: &str, year: i64, month: i64) -> Option<i64> {
    match calendar {
        "hebrew" => Some(hebrew_month_length(year, month)),
        "islamic-civil" | "islamic-tbla" => Some(days_in_islamic_month(year, month)),
        "chinese" | "dangi" => lunar_days_in_month(calendar, year, month),
        "coptic" | "ethiopic" | "ethioaa" => Some(if month == 13 {
            i64::from(coptic_leap_year(year)) + 5
        } else {
            30
        }),
        "islamic-umalqura" => umalqura_days_in_month(year, month),
        "buddhist" => Some(iso::days_in_month(year - 543, month)),
        "roc" => Some(iso::days_in_month(year + 1911, month)),
        "persian" => Some(persian_month_length(year, month)),
        "indian" => Some(saka_month_length(year, month)),
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
        "islamic-umalqura" => umalqura_to_fixed(year, month, 1)?,
        "hebrew" => hebrew_to_fixed(year, month, 1),
        "chinese" | "dangi" => lunar_to_fixed(calendar, year, month, 1)?,
        "buddhist" => buddhist_to_fixed(year, month, 1),
        "roc" => iso::iso_date_to_epoch_days(year + 1911, month - 1, 1) + RD_OFFSET,
        "coptic" => coptic_to_fixed(year, month, 1),
        "ethiopic" => ethiopic_to_fixed(year, month, 1),
        "ethioaa" => ethioaa_to_fixed(year, month, 1),
        "indian" => saka_to_fixed(year, month, 1),
        "persian" => persian_to_fixed(year, month, 1),
        _ => return None,
    };
    let (y, m, d) = iso::epoch_days_to_iso_date(rd - RD_OFFSET);
    Some((y, m, d))
}

/// The reference ISO date for a calendar month-day: the date in the latest
/// ISO year at or before 1972 where the month-day exists (CalendarMonthDay-
/// FromFields; the canonicalize-calendar fixtures pin 1972-02-11 for islamic
/// M12-25 and the hebrew fixtures their Cheshvan/Kislev variants). The month
/// code participates: M05L only exists in leap years. The chinese/dangi
/// search covers the full spec window (1900-1972, else 1973-2035) with the
/// leap month-day folding to the regular month under constrain; `None` under
/// reject for a month-day that never occurs in the window.
pub fn calendar_month_day_reference(
    calendar: &str,
    month: i64,
    day: i64,
    month_code: Option<&str>,
    constrain: bool,
) -> Option<(i64, i64, i64)> {
    if matches!(calendar, "chinese" | "dangi") {
        return lunar_month_day_reference(calendar, month, day, month_code, constrain);
    }
    // NonISOMonthDayToISOReferenceDate: the latest ISO date in
    // [1900-01-01, 1972-12-31] with the month code and day (else the earliest
    // in [1973-01-01, 2035-12-31]). The scan runs over the calendar years
    // bounded by the years containing ISO 1972-01-01 and ISO 1900-01-01; the
    // ISO year checks filter the window edges.
    let year0 = calendar_iso_to_date(calendar, 1972, 1, 1)?.0;
    let low = calendar_iso_to_date(calendar, 1900, 1, 1)?.0;
    let candidate = |year: i64| -> Option<(i64, i64, i64)> {
        let month = match month_code {
            Some(code) => resolve_calendar_month(calendar, year, None, Some(code))?,
            None => month,
        };
        if day > calendar_days_in_month(calendar, year, month)? {
            return None;
        }
        calendar_date_to_iso(calendar, year, month, day)
    };
    for year in (low - 1..=year0 + 2).rev() {
        let Some((y, m, d)) = candidate(year) else {
            continue;
        };
        if y < 1900 {
            break;
        }
        if y <= 1972 {
            return Some((y, m, d));
        }
    }
    // No date in [1900, 1972]: the earliest in [1973, 2035].
    let high = calendar_iso_to_date(calendar, 2035, 12, 31)?.0;
    for year in year0 + 3..=high {
        let Some((y, m, d)) = candidate(year) else {
            continue;
        };
        if y >= 1973 {
            return Some((y, m, d));
        }
        if y > 2035 {
            return None;
        }
    }
    None
}

/// The maximum days in the month described by the month code across any year
/// (NonISOMonthDayToISOReferenceDate: with no year the day is regulated
/// against this maximum — every umalqura month can be 30, persian M01-M06 can
/// be 31, coptic M13 at most 6 — before the reference search validates the
/// day against the actual reference year).
pub fn calendar_max_days_in_month(calendar: &str, month_code: &str) -> Option<i64> {
    let code = month_code.strip_suffix('L').unwrap_or(month_code);
    let n: i64 = code[1..].parse().ok()?;
    match calendar {
        // Tishrei/Heshvan/Kislev/Shevat/Nisan/Sivan/Av 30, Tevet/Adar
        // II/Iyar/Tammuz/Elul 29, Adar I (M05L) 30.
        "hebrew" => Some(match n {
            1 | 2 | 3 | 5 | 7 | 9 | 11 => 30,
            4 | 6 | 8 | 10 | 12 => 29,
            _ => return None,
        }),
        // Odd months 30, even 29, M12 30 in a leap year.
        "islamic-civil" | "islamic-tbla" => Some(if n == 12 || n % 2 == 1 { 30 } else { 29 }),
        // The observational calendar: any month can be 30.
        "islamic-umalqura" => Some(30),
        "coptic" | "ethiopic" | "ethioaa" => Some(match n {
            1..=12 => 30,
            13 => 6,
            _ => return None,
        }),
        "persian" | "indian" => Some(if n <= 6 { 31 } else { 30 }),
        "chinese" | "dangi" => Some(30),
        // The iso8601 and linear calendars: the ISO month maxima (M02 29).
        _ => Some(match n {
            2 => 29,
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            _ => return None,
        }),
    }
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
        "chinese" | "dangi" => {
            let rd = iso::iso_date_to_epoch_days(date.0, date.1 - 1, date.2) + RD_OFFSET;
            let (ly, lm, ld) = lunar_from_fixed(calendar, rd);
            let code = lunar_month_code(calendar, ly, lm);
            let mut year = ly + years;
            // The month code must still exist after the year add (a leap month
            // in a year without it rejects; constrain folds it to the regular
            // month — leap-months-chinese.js pins the fold and the reject).
            let mut month =
                resolve_calendar_month_with_overflow(calendar, year, None, Some(&code), constrain)?;
            month += months;
            while month < 1 {
                year -= 1;
                month += lunar_months_in_year(lunar_year_data(calendar, year).2);
            }
            while month > lunar_months_in_year(lunar_year_data(calendar, year).2) {
                month -= lunar_months_in_year(lunar_year_data(calendar, year).2);
                year += 1;
            }
            let max = lunar_days_in_month(calendar, year, month)?;
            let day = if ld > max {
                if constrain {
                    max
                } else {
                    return None;
                }
            } else {
                ld
            };
            let rd = lunar_to_fixed(calendar, year, month, day)? + days + 7 * weeks;
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
        // The solar calendars (coptic/ethiopic/ethioaa with 13 months,
        // persian/indian/umalqura with 12): convert to the calendar fields,
        // balance the years/months against the year's own month count (the
        // 13-month calendars cannot use the 12-month ISO balance), and
        // constrain the day against the month length.
        "coptic" | "ethiopic" | "ethioaa" | "persian" | "indian" | "islamic-umalqura" => {
            solar_date_add(calendar, date, years, months, weeks, days, constrain)
        }
        _ => iso::calendar_date_add(
            date.0, date.1, date.2, years, months, weeks, days, constrain,
        ),
    }
}

/// CalendarDateAdd for the 12/13-month solar calendars: the calendar fields
/// of the date, the years/months balanced against the year's own month count,
/// the day constrained against the month length, and the weeks/days applied
/// as fixed-date arithmetic.
#[allow(clippy::too_many_arguments)]
fn solar_date_add(
    calendar: &str,
    date: (i64, i64, i64),
    years: i64,
    months: i64,
    weeks: i64,
    days: i64,
    constrain: bool,
) -> Option<(i64, i64, i64)> {
    let (year, month, cd) = calendar_year_month_add(calendar, date, years, months)?;
    let max = calendar_days_in_month(calendar, year, month)?;
    let day = if cd > max {
        if constrain {
            max
        } else {
            return None;
        }
    } else {
        cd
    };
    let (y, m, d) = calendar_date_to_iso(calendar, year, month, day)?;
    let rd = iso::iso_date_to_epoch_days(y, m - 1, d) + RD_OFFSET + days + 7 * weeks;
    Some(iso::epoch_days_to_iso_date(rd - RD_OFFSET))
}

/// The (calendar year, calendar month, source day) of the date plus the
/// years/months: the calendar's own month logic (the hebrew M05L/M06
/// resolution, the chinese leap-month codes, the 12-month balance of the
/// tabular Islamic and solar calendars) with the source day kept
/// unconstrained — the CalendarDateUntil years/months phase, which constrains
/// the day only after the years and months are determined. A leap-month code
/// missing from the target year keeps the source ordinal in the pure-years
/// phase (the leap-months fixtures pin M04L + 1y as 12 months to M04 but
/// 1y 1mo to M05) and folds in the months phase (the constrained add).
fn calendar_year_month_add(
    calendar: &str,
    date: (i64, i64, i64),
    years: i64,
    months: i64,
) -> Option<(i64, i64, i64)> {
    let rd = iso::iso_date_to_epoch_days(date.0, date.1 - 1, date.2) + RD_OFFSET;
    match calendar {
        "hebrew" => {
            let (hy, hm, hd) = hebrew_from_fixed(rd);
            let code = hebrew_month_code(hy, hm);
            let mut year = hy + years;
            let resolved = resolve_calendar_month(calendar, year, None, Some(&code));
            let month = match resolved {
                Some(m) => m,
                // The leap code missing from the target year (M05L in a
                // common year): the pure-years phase keeps the source
                // ordinal, the months phase folds to the regular month.
                None if months == 0 => hm,
                None => {
                    resolve_calendar_month_with_overflow("hebrew", year, None, Some(&code), true)?
                }
            };
            let mut month = month + months;
            while month < 1 {
                year -= 1;
                month += hebrew_months_in_year(year);
            }
            while month > hebrew_months_in_year(year) {
                month -= hebrew_months_in_year(year);
                year += 1;
            }
            Some((year, month, hd))
        }
        "chinese" | "dangi" => {
            let (ly, lm, ld) = lunar_from_fixed(calendar, rd);
            let code = lunar_month_code(calendar, ly, lm);
            let mut year = ly + years;
            let resolved = resolve_calendar_month(calendar, year, None, Some(&code));
            let month = match resolved {
                Some(m) => m,
                // The leap code missing from the target year (M04L in a
                // common year): the pure-years phase keeps the source ordinal,
                // the months phase folds to the regular month.
                None if months == 0 => lm,
                None => {
                    resolve_calendar_month_with_overflow(calendar, year, None, Some(&code), true)?
                }
            };
            let mut month = month + months;
            while month < 1 {
                year -= 1;
                month += lunar_months_in_year(lunar_year_data(calendar, year).2);
            }
            while month > lunar_months_in_year(lunar_year_data(calendar, year).2) {
                month -= lunar_months_in_year(lunar_year_data(calendar, year).2);
                year += 1;
            }
            Some((year, month, ld))
        }
        "islamic-civil" | "islamic-tbla" => {
            let (iy, im, id) = islamic_from_fixed(rd, islamic_epoch(calendar)?);
            let (year, month) = iso::balance_iso_year_month(iy + years, im + months);
            Some((year, month, id))
        }
        _ => {
            let (cy, cm, cd) = calendar_iso_to_date(calendar, date.0, date.1, date.2)?;
            let mut year = cy + years;
            let mut month = cm + months;
            while month < 1 {
                year -= 1;
                month += calendar_months_in_year(calendar, year)?;
            }
            while month > calendar_months_in_year(calendar, year)? {
                month -= calendar_months_in_year(calendar, year)?;
                year += 1;
            }
            Some((year, month, cd))
        }
    }
}

/// CalendarDateUntil (spec 12.3.9 / the era-monthcode NonISODateUntil): the
/// largest whole units between the two calendar dates. The iso8601 and linear
/// calendars use the ISO arithmetic; the others walk the candidate units with
/// NonISODateSurpasses — the years candidate compares the target year with the
/// source MONTH CODE (a leap month compares by its code position, so chinese
/// M04L sits between M04 and M05), the months candidate folds a missing leap
/// code, balances the months at day 1 and compares with the source day, and
/// the weeks/days candidates balance the constrained day against the target.
pub fn calendar_date_until(
    calendar: &str,
    one: (i64, i64, i64),
    two: (i64, i64, i64),
    largest_unit: iso::Unit,
) -> (i64, i64, i64, i64) {
    if matches!(
        calendar,
        "iso8601" | "gregory" | "buddhist" | "japanese" | "roc"
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
/// direction (the era-monthcode NonISODateSurpasses). The years/months
/// candidates compare the calendar fields with the source day kept
/// unconstrained (the intercalary-month fixtures pin "day is constrained after
/// determining number of years and months added"); the weeks/days candidates
/// balance the constrained day.
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
    let Some((py, pm, pd)) = calendar_iso_to_date(calendar, one.0, one.1, one.2) else {
        return true;
    };
    let Some((ty, tm, td)) = calendar_iso_to_date(calendar, two.0, two.1, two.2) else {
        return true;
    };
    let code = calendar_month_code(calendar, py, pm);
    let y0 = py + years;
    // The years phase: the shifted year with the source month CODE and day
    // against the target (the leap month compares by its code position).
    if calendar_surpasses_code(sign, y0, &code, pd, ty, tm, td, calendar) {
        return true;
    }
    // The months phase: the constrained month code's ordinal plus the months,
    // balanced at day 1, compared with the source day.
    let m0 =
        resolve_calendar_month_with_overflow(calendar, y0, None, Some(&code), true).unwrap_or(pm);
    let Some((my, mm, _)) = balance_non_iso_date(calendar, y0, m0 + months, 1) else {
        return true;
    };
    if calendar_surpasses_ordinal(sign, my, mm, pd, ty, tm, td) {
        return true;
    }
    if weeks == 0 && days == 0 {
        return false;
    }
    // The weeks/days phases: the constrained day at the end of the
    // months-added month, plus the weeks/days, balanced against the target.
    let Some((ey, em, ed)) = balance_non_iso_date(calendar, my, mm + 1, 0) else {
        return true;
    };
    let base = pd.min(ed);
    let Some((by, bm, bd)) = balance_non_iso_date(calendar, ey, em, base + 7 * weeks + days) else {
        return true;
    };
    calendar_surpasses_ordinal(sign, by, bm, bd, ty, tm, td)
}

/// Whether the (year, monthCode, day) candidate surpasses the target calendar
/// fields (the spec's CompareSurpasses for the years phase: the month codes
/// compare lexicographically, so a leap month sits between its neighbors).
#[allow(clippy::too_many_arguments)]
fn calendar_surpasses_code(
    sign: i64,
    year: i64,
    code: &str,
    day: i64,
    ty: i64,
    tm: i64,
    td: i64,
    calendar: &str,
) -> bool {
    if year != ty {
        return sign * (year - ty) > 0;
    }
    let tcode = calendar_month_code(calendar, ty, tm);
    match code.cmp(&tcode) {
        std::cmp::Ordering::Less => sign < 0,
        std::cmp::Ordering::Greater => sign > 0,
        std::cmp::Ordering::Equal => sign * (day - td) > 0,
    }
}

/// Whether the (year, ordinal month, day) candidate surpasses the target
/// calendar fields (the spec's CompareSurpasses for the months/days phases).
fn calendar_surpasses_ordinal(
    sign: i64,
    year: i64,
    month: i64,
    day: i64,
    ty: i64,
    tm: i64,
    td: i64,
) -> bool {
    if year != ty {
        return sign * (year - ty) > 0;
    }
    if month != tm {
        return sign * (month - tm) > 0;
    }
    sign * (day - td) > 0
}

/// BalanceNonISODate (the era-monthcode spec): the potentially out-of-range
/// month and day overflow into the next-highest unit against the calendar's
/// own month count and month lengths.
fn balance_non_iso_date(
    calendar: &str,
    year: i64,
    month: i64,
    day: i64,
) -> Option<(i64, i64, i64)> {
    let mut y = year;
    let mut m = month;
    let mut months_in_year = calendar_months_in_year(calendar, y)?;
    while m <= 0 {
        y -= 1;
        months_in_year = calendar_months_in_year(calendar, y)?;
        m += months_in_year;
    }
    while m > months_in_year {
        m -= months_in_year;
        y += 1;
        months_in_year = calendar_months_in_year(calendar, y)?;
    }
    let mut d = day;
    let mut days_in_month = calendar_days_in_month(calendar, y, m)?;
    while d <= 0 {
        m -= 1;
        if m == 0 {
            y -= 1;
            months_in_year = calendar_months_in_year(calendar, y)?;
            m = months_in_year;
        }
        days_in_month = calendar_days_in_month(calendar, y, m)?;
        d += days_in_month;
    }
    while d > days_in_month {
        d -= days_in_month;
        m += 1;
        if m > months_in_year {
            y += 1;
            months_in_year = calendar_months_in_year(calendar, y)?;
            m = 1;
        }
        days_in_month = calendar_days_in_month(calendar, y, m)?;
    }
    Some((y, m, d))
}

/// The days in a calendar year (for the daysInYear getter).
pub fn calendar_days_in_year(calendar: &str, year: i64) -> Option<i64> {
    match calendar {
        "hebrew" => Some(hebrew_year_length(year)),
        "islamic-civil" | "islamic-tbla" => Some(if islamic_leap_year(year) { 355 } else { 354 }),
        "islamic-umalqura" => umalqura_year_length(year),
        "chinese" | "dangi" => {
            let (_, lens, leap) = lunar_year_data(calendar, year);
            Some(lens[..lunar_months_in_year(leap) as usize].iter().sum())
        }
        "coptic" | "ethiopic" => Some(365 + i64::from(coptic_leap_year(year))),
        "ethioaa" => Some(365 + i64::from(coptic_leap_year(year - 5500))),
        "buddhist" => Some(if iso::is_leap_year(year - 543) {
            366
        } else {
            365
        }),
        "roc" => Some(if iso::is_leap_year(year + 1911) {
            366
        } else {
            365
        }),
        "persian" => Some(365 + i64::from(persian_leap_year(year))),
        "indian" => Some(365 + i64::from(saka_leap_year(year))),
        _ => None,
    }
}

/// Whether the calendar year is a leap year (for the inLeapYear getter).
pub fn calendar_in_leap_year(calendar: &str, year: i64) -> Option<bool> {
    match calendar {
        "hebrew" => Some(hebrew_months_in_year(year) == 13),
        "islamic-civil" | "islamic-tbla" => Some(islamic_leap_year(year)),
        "islamic-umalqura" => umalqura_year_length(year).map(|l| l == 355),
        "chinese" | "dangi" => Some(lunar_year_data(calendar, year).2.is_some()),
        "coptic" | "ethiopic" => Some(coptic_leap_year(year)),
        "ethioaa" => Some(coptic_leap_year(year - 5500)),
        "buddhist" => Some(iso::is_leap_year(year - 543)),
        "roc" => Some(iso::is_leap_year(year + 1911)),
        "persian" => Some(persian_leap_year(year)),
        "indian" => Some(saka_leap_year(year)),
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
        "islamic-umalqura" => {
            let rd = umalqura_to_fixed(year, month, day)?;
            Some(rd - umalqura_to_fixed(year, 1, 1)? + 1)
        }
        "chinese" | "dangi" => {
            let rd = lunar_to_fixed(calendar, year, month, day)?;
            Some(rd - lunar_to_fixed(calendar, year, 1, 1)? + 1)
        }
        "buddhist" => Some(
            iso::iso_date_to_epoch_days(year - 543, month - 1, day)
                - iso::iso_date_to_epoch_days(year - 543, 0, 1)
                + 1,
        ),
        "coptic" | "ethiopic" => {
            Some(coptic_to_fixed(year, month, day) - coptic_year_start(year) + 1)
        }
        "ethioaa" => Some(ethioaa_to_fixed(year, month, day) - ethioaa_to_fixed(year, 1, 1) + 1),
        "persian" => Some(persian_to_fixed(year, month, day) - persian_year_start(year) + 1),
        "indian" => Some(saka_to_fixed(year, month, day) - saka_year_start(year) + 1),
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
            calendar_month_day_reference("islamic-civil", 12, 25, None, true),
            Some(iso(1972, 2, 11))
        );
        // The hebrew reference-year-1972 fixtures: M01-1 lands in ISO 1972
        // (Tishri 5733), M02-30 in 1971 (Cheshvan 5732 is a 30-day month and
        // Cheshvan 5733 a 29-day one), and M05L only exists in leap years
        // (5730; 5732 is common).
        assert_eq!(
            calendar_month_day_reference("hebrew", 1, 1, None, true),
            Some(iso(1972, 9, 9))
        );
        assert_eq!(
            calendar_month_day_reference("hebrew", 2, 30, None, true),
            Some(iso(1971, 11, 18))
        );
        assert_eq!(
            calendar_month_day_reference("hebrew", 2, 29, None, true),
            Some(iso(1972, 11, 6))
        );
        assert_eq!(
            calendar_month_day_reference("hebrew", 6, 1, Some("M05L"), true),
            Some(iso(1970, 2, 7))
        );
        assert_eq!(
            calendar_month_day_reference("hebrew", 6, 1, None, true),
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

    #[test]
    fn chinese_year_lengths() {
        // The corpus's pinned year lengths (daysInYear/basic-chinese.js): the
        // ICU4X data reproduces every entry.
        let table = [
            (1969, 354),
            (1970, 355),
            (1971, 384),
            (1972, 354),
            (1973, 354),
            (1974, 384),
            (1987, 384),
            (2001, 384),
            (2004, 384),
            (2012, 384),
            (2017, 384),
            (2020, 384),
            (2022, 355),
            (2025, 384),
            (2028, 384),
            (2033, 384),
            (2044, 384),
            (2047, 384),
        ];
        for (y, want) in table {
            assert_eq!(calendar_days_in_year("chinese", y), Some(want), "year {y}");
            assert_eq!(
                calendar_in_leap_year("chinese", y),
                Some(want > 370),
                "leap {y}"
            );
        }
        // The leap months: the pinned positions (monthCode fixtures pin the
        // ordinal position of the leap month).
        for (y, ord) in [
            (1971, 6),
            (1974, 5),
            (1976, 9),
            (1979, 7),
            (1982, 5),
            (1987, 7),
            (1990, 6),
            (1993, 4),
            (1995, 9),
            (1998, 6),
            (2001, 5),
            (2004, 3),
            (2006, 8),
            (2009, 6),
            (2012, 5),
            (2017, 7),
            (2020, 5),
            (2023, 3),
            (2025, 7),
            (2028, 6),
            (2031, 4),
            (2036, 7),
            (2039, 6),
            (2042, 3),
            (2044, 8),
            (2047, 6),
        ] {
            assert_eq!(
                lunar_year_data("chinese", y).2,
                Some(ord),
                "leap ordinal {y}"
            );
        }
        // The 2022 month starts (reference-day-chinese.js): M01-1 = 2022-02-01.
        assert_eq!(
            calendar_date_to_iso("chinese", 2022, 1, 1),
            Some(iso(2022, 2, 1))
        );
        assert_eq!(
            calendar_date_to_iso("chinese", 2022, 5, 1),
            Some(iso(2022, 5, 30))
        );
        // The with/calendar-dates out-of-range anchors: chinese 1899-M12-1 is
        // ISO 1900-01-01 (the CNY 1899 anchor is 1899-02-10).
        assert_eq!(
            calendar_date_to_iso("chinese", 1899, 12, 1),
            Some(iso(1900, 1, 1))
        );
        assert_eq!(
            calendar_date_to_iso("chinese", 2025, 1, 1),
            Some(iso(2025, 1, 29))
        );
        // Round trips across the table edges.
        for (y, m, d) in [(1899, 12, 1), (1900, 2, 19), (1971, 6, 1), (2099, 13, 30)] {
            let iso = calendar_date_to_iso("chinese", y, m, d).unwrap();
            assert_eq!(
                calendar_iso_to_date("chinese", iso.0, iso.1, iso.2),
                Some((y, m, d))
            );
        }
    }

    #[test]
    fn chinese_month_day_reference_years() {
        // The spec's Table 6 reference years (chinese-30-day-leap-months,
        // chinese-month-codes, chinese-calendar-dates): the day-30 years and
        // the leap-month years.
        for (code, day, want) in [
            ("M03L", 30, 1955),
            ("M04L", 30, 1944),
            ("M05L", 30, 1952),
            ("M06L", 30, 1941),
            ("M07L", 30, 1938),
            ("M04L", 15, 1963),
            ("M06", 29, 1972),
            ("M11L", 1, 2033),
            ("M11L", 29, 2034),
            ("M09L", 29, 2014),
            ("M10L", 29, 1984),
        ] {
            let m = code[1..3].parse::<i64>().unwrap();
            let d = calendar_month_day_reference("chinese", m, day, Some(code), true)
                .unwrap_or((0, 0, 0));
            assert_eq!(d.0, want, "{code}-{day}");
        }
        // The regular month day-30 years (chinese-month-codes.js).
        for (code, want) in [
            ("M01", 1970),
            ("M02", 1972),
            ("M03", 1966),
            ("M04", 1970),
            ("M05", 1972),
            ("M06", 1971),
            ("M07", 1972),
            ("M08", 1971),
            ("M09", 1972),
            ("M10", 1972),
            ("M11", 1970),
            ("M12", 1972),
        ] {
            let m = code[1..].parse::<i64>().unwrap();
            let d = calendar_month_day_reference("chinese", m, 30, Some(code), true)
                .unwrap_or((0, 0, 0));
            assert_eq!(d.0, want, "{code}-30");
        }
        // A leap month-day that never occurs folds to the regular month under
        // constrain (M01L-29 → M01-29 → 1972; M11L-30 → M11-30 → 1970) and
        // returns None under reject.
        assert_eq!(
            calendar_month_day_reference("chinese", 1, 29, Some("M01L"), true).map(|d| d.0),
            Some(1972)
        );
        assert_eq!(
            calendar_month_day_reference("chinese", 12, 30, Some("M11L"), true).map(|d| d.0),
            Some(1970)
        );
        assert_eq!(
            calendar_month_day_reference("chinese", 1, 29, Some("M01L"), false),
            None
        );
    }

    #[test]
    fn chinese_month_add_semantics() {
        // leap-months-chinese.js: the month arithmetic steps the ordinals
        // through the year's own sequence (2020 M03 + 2mo → M04L) and the
        // year add keeps the leap month code when it exists (2012 M04L + 8y
        // → 2020 M04L), folds under constrain, rejects otherwise.
        let c = calendar_date_to_iso("chinese", 2020, 3, 1).unwrap();
        assert_eq!(
            calendar_date_add("chinese", c, 0, 2, 0, 0, true),
            calendar_date_to_iso("chinese", 2020, 5, 1) // M04L at ordinal 5
        );
        assert_eq!(
            calendar_date_add("chinese", c, 0, 1, 0, 0, true),
            calendar_date_to_iso("chinese", 2020, 4, 1) // M04, not M04L
        );
        let start = calendar_date_to_iso("chinese", 2012, 5, 1).unwrap(); // M04L
        assert_eq!(
            calendar_date_add("chinese", start, 8, 0, 0, 0, true),
            calendar_date_to_iso("chinese", 2020, 5, 1)
        );
        // A leap month in a year without it folds under constrain and rejects
        // under reject (leap-months-chinese.js pins the fold).
        let leap1966 = calendar_date_to_iso("chinese", 1966, 4, 1).unwrap(); // M03L
        assert_eq!(
            calendar_date_add("chinese", leap1966, 1, 0, 0, 0, true),
            calendar_date_to_iso("chinese", 1967, 3, 1)
        );
        assert!(calendar_date_add("chinese", leap1966, 1, 0, 0, 0, false).is_none());
        // The leap-year-until fixture: 2016-07-31 → 2017-07-31 chinese is
        // 1y 0m 0w 10d (the dates are ISO dates; the calendar machinery
        // converts).
        assert_eq!(
            calendar_date_until(
                "chinese",
                iso(2016, 7, 31),
                iso(2017, 7, 31),
                iso::Unit::Year
            ),
            (1, 0, 0, 10)
        );
    }

    #[test]
    fn solar_calendar_roundtrip_anchors() {
        // The roundtrip-from-property-bag anchors: ISO 2000-01-01 and
        // ISO 1-01-01 in each solar calendar, plus the leap classifications
        // the inLeapYear fixtures pin.
        let anchors = [
            ("buddhist", (2000, 1, 1), (2543, 1, 1)),
            ("buddhist", (1, 1, 1), (544, 1, 1)),
            ("coptic", (2000, 1, 1), (1716, 4, 22)),
            ("coptic", (1, 1, 1), (-283, 5, 8)),
            ("ethioaa", (2000, 1, 1), (7492, 4, 22)),
            ("ethioaa", (1, 1, 1), (5493, 5, 8)),
            ("ethiopic", (2000, 1, 1), (1992, 4, 22)),
            ("ethiopic", (1, 1, 1), (-7, 5, 8)),
            ("indian", (2000, 1, 1), (1921, 10, 11)),
            ("indian", (1, 1, 1), (-78, 10, 11)),
            ("persian", (2000, 1, 1), (1378, 10, 11)),
            ("persian", (1, 1, 1), (-621, 10, 11)),
            ("islamic-civil", (2000, 1, 1), (1420, 9, 24)),
            ("islamic-civil", (1, 1, 1), (-640, 5, 18)),
            ("islamic-umalqura", (2000, 1, 1), (1420, 9, 24)),
            ("islamic-umalqura", (1, 1, 1), (-640, 5, 18)),
        ];
        for (calendar, iso_date, cal_date) in anchors {
            assert_eq!(
                calendar_iso_to_date(calendar, iso_date.0, iso_date.1, iso_date.2),
                Some(cal_date),
                "{calendar} ISO {iso_date:?}"
            );
            assert_eq!(
                calendar_date_to_iso(calendar, cal_date.0, cal_date.1, cal_date.2),
                Some(iso_date),
                "{calendar} roundtrip {cal_date:?}"
            );
        }
    }

    #[test]
    fn solar_calendar_leap_years() {
        // The inLeapYear fixtures: coptic 1687, 1691, ... (3 mod 4); indian
        // 1894, 1898, ... ((y+78) Gregorian leap); ethioaa/ethiopic 7463,
        // 7467, ... (3 mod 4 in the Amete Alem numbering).
        assert!(calendar_in_leap_year("coptic", 1687) == Some(true));
        assert!(calendar_in_leap_year("coptic", 1686) == Some(false));
        assert!(calendar_in_leap_year("indian", 1894) == Some(true));
        assert!(calendar_in_leap_year("indian", 1893) == Some(false));
        assert!(calendar_in_leap_year("ethioaa", 7463) == Some(true));
        assert!(calendar_in_leap_year("ethioaa", 7462) == Some(false));
        assert!(calendar_in_leap_year("ethiopic", 7463) == Some(true));
        assert!(calendar_in_leap_year("persian", 1395) == Some(true));
        assert!(calendar_in_leap_year("persian", 1396) == Some(false));
        // The coptic 13th month: 6 days in leap years, 5 otherwise.
        assert_eq!(calendar_days_in_month("coptic", 1739, 13), Some(6));
        assert_eq!(calendar_days_in_month("coptic", 1740, 13), Some(5));
        // The indian leap day is 31 Chaitra (M01); common years have 30.
        assert_eq!(calendar_days_in_month("indian", 1894, 1), Some(31));
        assert_eq!(calendar_days_in_month("indian", 1895, 1), Some(30));
        // The umalqura pinned month layouts (daysInMonth fixture).
        assert_eq!(
            calendar_days_in_month("islamic-umalqura", 1390, 1),
            Some(29)
        );
        assert_eq!(
            calendar_days_in_month("islamic-umalqura", 1390, 2),
            Some(30)
        );
        assert_eq!(
            calendar_days_in_month("islamic-umalqura", 1391, 1),
            Some(29)
        );
        assert!(calendar_in_leap_year("islamic-umalqura", 1390) == Some(true));
        assert!(calendar_in_leap_year("islamic-umalqura", 1391) == Some(false));
    }

    #[test]
    fn persian_nowruz_dates() {
        // The persian-new-year-dates fixture: 1 Farvardin of 1206-1498.
        assert_eq!(
            calendar_date_to_iso("persian", 1206, 1, 1),
            Some(iso(1827, 3, 22))
        );
        assert_eq!(
            calendar_date_to_iso("persian", 1301, 1, 1),
            Some(iso(1922, 3, 22))
        );
        assert_eq!(
            calendar_date_to_iso("persian", 1375, 1, 1),
            Some(iso(1996, 3, 20))
        );
        assert_eq!(
            calendar_date_to_iso("persian", 1399, 1, 1),
            Some(iso(2020, 3, 20))
        );
        assert_eq!(
            calendar_date_to_iso("persian", 1498, 1, 1),
            Some(iso(2119, 3, 21))
        );
    }

    #[test]
    fn coptic_until_intercalary() {
        // commonLast (1738 M13-05) → leapLast (1739 M13-06): 13 months + 1
        // day; leapLast → common2Last (1740 M13-05): 12 months + 29 days.
        let common_last = calendar_date_to_iso("coptic", 1738, 13, 5).unwrap();
        let leap_last = calendar_date_to_iso("coptic", 1739, 13, 6).unwrap();
        assert_eq!(
            calendar_date_until("coptic", common_last, leap_last, iso::Unit::Month),
            (0, 13, 0, 1)
        );
        assert_eq!(
            calendar_date_until("coptic", common_last, leap_last, iso::Unit::Year),
            (1, 0, 0, 1)
        );
        assert_eq!(
            calendar_date_until("coptic", common_last, leap_last, iso::Unit::Day),
            (0, 0, 0, 366)
        );
        let common2_last = calendar_date_to_iso("coptic", 1740, 13, 5).unwrap();
        assert_eq!(
            calendar_date_until("coptic", leap_last, common2_last, iso::Unit::Month),
            (0, 12, 0, 29)
        );
    }
}
