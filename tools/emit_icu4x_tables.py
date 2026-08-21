"""Emit the chinese/dangi engine tables (1898-2102) from the ICU4X data.

Data sources (all verified in verify_icu4x.py against the fixture pins):
- 1898-1899: the ICU4X `simple` calculation (BEIJING offset)
- 1900-1911: the ICU4X qing_data table
- 1912-2102: the ICU4X china_data / korea_data tables

The engine table format matches the existing lunar_tables.rs:
    YEAR => (epoch_day_start, &[month_lengths...], leap_ordinal_or_0)
where epoch_day_start is the CNY date in engine epoch days (0 = 1970-01-01)
and the month lengths are in ordinal order (the leap month included at its
position; leap_ordinal is its 1-based position, 0 for common years).

Years outside 1898-2102 are handled by the engine's port of the `simple`
calculation (matching ICU4X's own out-of-table behavior).
"""

import os
import sys

sys.path.insert(0, os.path.dirname(__file__))
from verify_icu4x import year_data, rd_to_iso, iso_to_rd, RD_OFFSET  # noqa: E402


def build_table(cal, years):
    table = {}
    for y in years:
        ny, lens, leap = year_data(cal, y)
        n = 12 + (1 if leap else 0)
        layout = [29 + lens[i] for i in range(n)]
        assert sum(layout) in (353, 354, 355, 383, 384, 385), (cal, y, sum(layout))
        table[y] = (ny - RD_OFFSET, layout, leap or 0)
    return table


def emit(cal, table):
    lines = [f"    // {cal}: {min(table)}-{max(table)} (ICU4X data, pinned core verified)"]
    for y in sorted(table):
        start, layout, leap = table[y]
        lens = ",".join(map(str, layout))
        lines.append(f"    ({start}, &[{lens}], {leap}), // {y}")
    return "\n".join(lines)


def main():
    years = list(range(1898, 2103))
    out = []
    for cal in ("chinese", "dangi"):
        table = build_table(cal, years)
        out.append(f"// {cal}: {len(table)} years ({min(table)}-{max(table)}), "
                   "ICU4X east-asian-traditional data")
        out.append(f"pub const {cal.upper()}_TABLE: [(i64, &[i64], i64); {len(table)}] = [")
        out.append(emit(cal, table))
        out.append("];")
    path = os.path.join(os.path.dirname(__file__), "lunar_tables_icu4x.rs")
    with open(path, "w", encoding="utf-8") as f:
        f.write("\n".join(out) + "\n")
    print(f"wrote {path} ({len(out)} lines)")


if __name__ == "__main__":
    main()
