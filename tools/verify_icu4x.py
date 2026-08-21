"""Verify the ICU4X east-asian-traditional data against the fixture pins.

The corpus (V8 -> temporal_rs -> ICU4X) is generated against ICU4X's
PackedEastAsianTraditionalYearData tables (china/korea 1912-2102, qing
1900-1911) plus the `simple` calculation outside those ranges. This script:

1. parses the ICU4X data files (already vendored into scratch/),
2. implements the `simple` calculation for out-of-table years,
3. builds (year -> epoch_start, month_layout, leap_ordinal) per calendar,
4. verifies every pinned fixture constraint against the built table
   (year lengths, leap years, leap positions, month patterns, the 2022
   month starts, and the with/calendar-dates out-of-range years), and
5. prints a summary of mismatches.

RD 1 = 0001-01-01; engine epoch day 0 = 1970-01-01 = RD 719163.
"""

import os
import re
import sys
from datetime import date

sys.path.insert(0, os.path.dirname(__file__))
from extract_lunar import (
    extract_year_lengths,
    extract_leap_years,
    extract_leap_positions,
    extract_month_patterns,
    extract_2022_anchors,
)

RD_OFFSET = 719_163  # engine epoch day 0 (1970-01-01) as an RD

# --- ISO/RD helpers -----------------------------------------------------------

def rd_to_iso(rd):
    # RD 1 = 0001-01-01, proleptic Gregorian, any i64 year.
    # Calendrical Calculations gregorian-from-fixed.
    d0 = rd - 1
    n400 = d0 // 146097
    d1 = d0 % 146097
    n100 = d1 // 36524
    d2 = d1 % 36524
    n4 = d2 // 1461
    d3 = d2 % 1461
    n1 = d3 // 365
    day = d3 % 365 + 1
    year = 400 * n400 + 100 * n100 + 4 * n4 + n1
    if n100 != 4 and n1 != 4:
        year += 1
    # Month from day-of-year.
    leap = year % 4 == 0 and (year % 100 != 0 or year % 400 == 0)
    mdays = [31, 29 if leap else 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    doy = day
    for m in range(12):
        if doy <= mdays[m]:
            return (year, m + 1, doy)
        doy -= mdays[m]
    raise AssertionError("unreachable")


def iso_to_rd(y, m, d):
    # Calendrical Calculations fixed-from-gregorian, any i64 year.
    leap = y % 4 == 0 and (y % 100 != 0 or y % 400 == 0)
    adj = 0 if m <= 2 else (-1 if leap else -2)
    return (365 * (y - 1) + (y - 1) // 4 - (y - 1) // 100 + (y - 1) // 400
            + (367 * m - 362) // 12 + adj + d)


def epoch_of_rd(rd):
    return rd - RD_OFFSET


# --- ICU4X data parsing --------------------------------------------------------

def parse_year_data(path):
    """Parse a PackedEastAsianTraditionalYearData::new(...) table file.

    Returns {related_iso: (new_year_rd, [13 bools], leap_ordinal_or_None)}.
    """
    src = open(path, encoding="utf-8").read()
    out = {}
    pat = re.compile(
        r"PackedEastAsianTraditionalYearData::new\(\s*(\d+),\s*\[([^\]]*)\],\s*"
        r"(Some\((\d+)\)|None),\s*gregorian\((\d+),\s*(\d+),\s*(\d+)\)\s*\)"
    )
    for m in pat.finditer(src):
        year = int(m.group(1))
        lens = [x.strip() == "l" for x in m.group(2).split(",")]
        leap = int(m.group(4)) if m.group(3).startswith("Some") else None
        ny = iso_to_rd(int(m.group(5)), int(m.group(6)), int(m.group(7)))
        out[year] = (ny, lens, leap)
    return out


CHINA = parse_year_data(os.path.join(os.path.dirname(__file__), "china_data.rs"))
KOREA = parse_year_data(os.path.join(os.path.dirname(__file__), "korea_data.rs"))
QING = parse_year_data(os.path.join(os.path.dirname(__file__), "qing_data.rs"))

# --- The simple calculation (ICU4X simple.rs port) ----------------------------

MS_PER_DAY = 86_400_000
MEAN_GREGORIAN_YEAR_MS = 31_556_952_000  # 146097/400 days = 365.2425 days
MEAN_GREGORIAN_SOLAR_TERM_MS = MEAN_GREGORIAN_YEAR_MS // 12
MEAN_SYNODIC_MS = int(295305888531 * MS_PER_DAY / 10_000_000_000)  # 29.5305888531 d

UTC_PLUS_8 = 8 * 3_600_000
UTC_PLUS_9 = 9 * 3_600_000
BEIJING_UTC_OFFSET = int(1397 / 180 * 3_600_000)  # UTC + 1397/180 hours


def solstice_base(offset_ms):
    # 1999-12-22T07:44 local
    rd = iso_to_rd(1999, 12, 22)
    return (rd, 7 * 3_600_000 + 44 * 60_000 + offset_ms)


def new_moon_base(offset_ms):
    # 2000-01-06T18:14 local
    rd = iso_to_rd(2000, 1, 6)
    return (rd, 18 * 3_600_000 + 14 * 60_000 + offset_ms)


def periodic_on_or_before(rata_die, base_rd, base_ms, period_ms):
    """Largest moment base + n*period <= rata_die (as (rd, ms_in_day))."""
    diff = (rata_die - base_rd) * MS_PER_DAY - base_ms
    n = (diff + MS_PER_DAY - 1) // period_ms
    millis = base_rd * MS_PER_DAY + base_ms + n * period_ms
    rd = millis // MS_PER_DAY - (1 if millis < 0 else 0)
    ms = millis % MS_PER_DAY + (MS_PER_DAY if millis < 0 else 0)
    return (rd, ms)


def simple_year(related_iso, offset_ms):
    """ICU4X EastAsianTraditionalYear::simple — returns (new_year_rd, [13 lens], leap)."""
    day_before_year = iso_to_rd(related_iso, 1, 1) - 1
    s_rd, s_ms = solstice_base(offset_ms)
    nm_rd, nm_ms = new_moon_base(offset_ms)
    major = periodic_on_or_before(day_before_year, s_rd, s_ms, MEAN_GREGORIAN_YEAR_MS)
    new_moon = periodic_on_or_before(major[0], nm_rd, nm_ms, MEAN_SYNODIC_MS)
    next_nm = (new_moon[0] + (new_moon[1] + MEAN_SYNODIC_MS) // MS_PER_DAY,
               (new_moon[1] + MEAN_SYNODIC_MS) % MS_PER_DAY)
    solar_term = -2
    had_leap = False
    while solar_term < 0 or (next_nm[0] <= major[0] and not had_leap):
        if next_nm[0] <= major[0] and not had_leap:
            had_leap = True
        else:
            solar_term += 1
            major = (major[0] + (major[1] + MEAN_GREGORIAN_SOLAR_TERM_MS) // MS_PER_DAY,
                     (major[1] + MEAN_GREGORIAN_SOLAR_TERM_MS) % MS_PER_DAY)
        new_moon, next_nm = next_nm, (
            next_nm[0] + (next_nm[1] + MEAN_SYNODIC_MS) // MS_PER_DAY,
            (next_nm[1] + MEAN_SYNODIC_MS) % MS_PER_DAY)
    assert solar_term == 0, related_iso
    new_year = new_moon[0]
    month_lengths = [False] * 13
    leap_month = None
    while solar_term < 12 or (next_nm[0] <= major[0] and not had_leap):
        idx = solar_term + (1 if leap_month is not None else 0)
        if idx < 13:
            month_lengths[idx] = (next_nm[0] - new_moon[0]) == 30
        if next_nm[0] <= major[0] and not had_leap:
            had_leap = True
            leap_month = solar_term + 1
        else:
            solar_term += 1
            major = (major[0] + (major[1] + MEAN_GREGORIAN_SOLAR_TERM_MS) // MS_PER_DAY,
                     (major[1] + MEAN_GREGORIAN_SOLAR_TERM_MS) % MS_PER_DAY)
        new_moon, next_nm = next_nm, (
            next_nm[0] + (next_nm[1] + MEAN_SYNODIC_MS) // MS_PER_DAY,
            (next_nm[1] + MEAN_SYNODIC_MS) % MS_PER_DAY)
    assert solar_term == 12, related_iso
    return (new_year, month_lengths, leap_month)


def year_data(cal, related_iso):
    """The year data for a calendar + related ISO year (the ICU4X hierarchy)."""
    tables = {"chinese": (CHINA, 1912), "dangi": (KOREA, 1912)}
    table, start = tables[cal]
    if related_iso in table:
        return table[related_iso]
    if related_iso > start:
        off = UTC_PLUS_8 if cal == "chinese" else UTC_PLUS_9
        return simple_year(related_iso, off)
    if related_iso in QING:
        return QING[related_iso]
    return simple_year(related_iso, BEIJING_UTC_OFFSET)


# --- Table building ------------------------------------------------------------

def build_table(cal, years):
    table = {}
    for y in years:
        ny, lens, leap = year_data(cal, y)
        n = 12 + (1 if leap else 0)
        layout = [29 + lens[i] for i in range(n)]
        table[y] = {"start": epoch_of_rd(ny), "layout": layout, "leap": leap}
    return table


def chinese_to_iso(table, y, position, day):
    entry = table[y]
    rd = entry["start"] + RD_OFFSET
    for i in range(1, position):
        rd += entry["layout"][i - 1]
    return rd_to_iso(rd + day - 1)


def iso_from_fields(table, iso):
    rd = iso_to_rd(*iso)
    for y in sorted(table):
        entry = table[y]
        start = entry["start"] + RD_OFFSET
        if start <= rd < start + sum(entry["layout"]):
            acc = start
            for pos, ln in enumerate(entry["layout"], 1):
                if rd < acc + ln:
                    return (y, pos, rd - acc + 1)
                acc += ln
    return None


# --- Verification --------------------------------------------------------------

def main():
    problems = []
    for cal in ("chinese", "dangi"):
        lengths = extract_year_lengths(cal)
        leaps = set(extract_leap_years(cal))
        positions = extract_leap_positions(cal)
        patterns = extract_month_patterns(cal)
        years = sorted(set(lengths) | set(leaps) | set(positions) | set(patterns))
        years = [y for y in years if 1969 <= y <= 2048]
        table = build_table(cal, years)
        n_problems = 0

        # 1. Year lengths.
        for y, want in lengths.items():
            got = sum(table[y]["layout"])
            if got != want:
                problems.append(f"{cal} {y}: ICU4X year length {got} != pinned {want}")
                n_problems += 1

        # 2. Leap years (months in year == 13).
        for y in years:
            is_leap = len(table[y]["layout"]) == 13
            if (y in leaps) != is_leap:
                problems.append(f"{cal} {y}: leap status pinned={y in leaps} icu4x={is_leap}")
                n_problems += 1

        # 3. Leap positions: the fixture's `month` field is the ordinal
        #    position of the leap month (1971: month 6, M05L); compare
        #    directly against the ICU4X leap ordinal.
        for y, (mo, code) in positions.items():
            got = table[y]["leap"]
            want_ordinal = mo
            if got != want_ordinal:
                problems.append(
                    f"{cal} {y}: leap ordinal ICU4X={got} pinned={want_ordinal} ({code})")
                n_problems += 1

        # 4. Pinned month patterns (daysInMonth pins months 1..len-1; the
        #    leap-dates fixture pins all months).
        for y, pat in patterns.items():
            layout = table[y]["layout"]
            for i in range(min(len(pat) - 1, len(layout))):
                if layout[i] != pat[i]:
                    problems.append(f"{cal} {y}: month {i+1} ICU4X={layout[i]} pinned={pat[i]}")
                    n_problems += 1

        # 5. The 2022 reference days (chinese).
        if cal == "chinese":
            anchors = extract_2022_anchors()
            for code, (month, day) in sorted(anchors.items()):
                iso = chinese_to_iso(table, 2022, month, 1)
                if iso[2] != day:
                    problems.append(f"chinese 2022 {code}: ISO day {iso[2]} != pinned {day}")
                    n_problems += 1

        print(f"== {cal}: {len(table)} years, {n_problems} problems ==")
        if n_problems:
            for p in problems:
                if p.startswith(cal):
                    print("  ", p)

    # 6. The out-of-range years the fixtures construct (with/calendar-dates
    #    and the leap-month-with-year surfaces): the table must exist and
    #    resolve without error for the pinned dates.
    print("\n== out-of-range checks ==")
    for cal, cases in (
        ("chinese", [(1899, 12, 1), (2099, 11, 21), (1999, 11, 25)]),
        ("dangi", [(1899, 12, 1), (2049, 11, 21), (1999, 11, 25)]),
    ):
        table = build_table(cal, list(range(1898, 2101)))
        for (y, m, d) in cases:
            entry = table.get(y)
            ok = entry is not None and m <= len(entry["layout"])
            print(f"  {cal} {y}-M{m:02d}: table year={'yes' if entry else 'NO'} "
                  f"month={'yes' if ok else 'NO'}")
            if ok:
                iso = chinese_to_iso(table, y, m, d)
                print(f"    -> ISO {iso[0]}-{iso[1]:02d}-{iso[2]:02d}")
                back = iso_from_fields(table, iso)
                print(f"    <- fields {back}")
        # Continuity: each year starts the day after the previous ends.
        bad = 0
        for y in range(1899, 2100):
            a = table[y]
            b = table[y + 1]
            if a["start"] + sum(a["layout"]) != b["start"]:
                bad += 1
                print(f"  {cal} gap between {y} and {y+1}")
        print(f"  {cal} year-start continuity breaks: {bad}")

    print("\n== with/calendar-dates ISO anchors ==")
    # chinese 1899: M01-1 (CNY) and M12-1; 2099: M11-21; 2025: M01-1
    t = build_table("chinese", list(range(1898, 2101)))
    for (y, m) in [(1899, 1), (1899, 12), (2099, 11), (2025, 1)]:
        print(f"  chinese {y}-M{m:02d}-1 = {chinese_to_iso(t, y, m, 1)}")
    t2 = build_table("dangi", list(range(1898, 2101)))
    for (y, m) in [(1899, 1), (1899, 12), (2049, 11), (2025, 1)]:
        print(f"  dangi {y}-M{m:02d}-1 = {chinese_to_iso(t2, y, m, 1)}")


if __name__ == "__main__":
    main()
