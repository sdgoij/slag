#!/usr/bin/env python3
"""Generate crates/unicode/src/tz_data.rs from the vendored IANA tzdata
(jiff-tzdb's flattened TZif blob + tzname index, pinned at 2026c).

The data source: tools/tzdata/concatenated-zoneinfo.dat (the flattened,
de-duplicated TZif data) + tools/tzdata/tzname.rs (name -> byte range).
Each TZif blob is a standard TZif v2+ file whose second (8-byte) block
carries the full pre-1970 transition history the Temporal corpus pins.

The corpus (test262 intl402/Temporal) asserts exact offsets and transition
instants (e.g. America/New_York's 1883-11-18T17:00Z standard-time
introduction, Europe/London's 1847-12-01T00:01:15Z LMT->GMT switch,
Africa/Monrovia's sub-minute offset), so this generated table IS the data
spec for the engine.

Usage: python3 tools/gen_tz_tables.py
Writes: crates/unicode/src/tz_data.rs
"""
import calendar
import os
import re
import struct
import sys
from datetime import date, datetime, timezone

ROOT = os.path.join(os.path.dirname(__file__), "..")
DATA = os.path.join(ROOT, "tools", "tzdata", "concatenated-zoneinfo.dat")
TZNAME = os.path.join(ROOT, "tools", "tzdata", "tzname.rs")
OUT = os.path.join(ROOT, "crates", "unicode", "src", "tz_data.rs")
SUPPORTED = os.path.join(ROOT, "crates", "runtime", "src", "builtins", "intl", "number_data.rs")

# The IANA `backward` links (link -> primary) exercised by the corpus: these
# names must resolve to their primary identifier. The full backward set would
# come from the vendored tzdata source; the fixture surface covers the tested
# links plus the renames.
BACKWARD_LINKS = {
    "Africa/Asmera": "Africa/Asmara",
    "Africa/Timbuktu": "Africa/Bamako",
    "America/Atka": "America/Adak",
    "America/Godthab": "America/Nuuk",
    "America/Montreal": "America/Toronto",
    "America/Nipigon": "America/Toronto",
    "America/Pangnirtung": "America/Iqaluit",
    "America/Porto_Acre": "America/Rio_Branco",
    "America/Rainy_River": "America/Winnipeg",
    "America/Santa_Isabel": "America/Tijuana",
    "America/Shiprock": "America/Denver",
    "America/Thunder_Bay": "America/Toronto",
    "America/Virgin": "America/Port_of_Spain",
    "America/Yellowknife": "America/Edmonton",
    "Antarctica/South_Pole": "Pacific/Auckland",
    "Arctic/Longyearbyen": "Europe/Berlin",
    "Asia/Calcutta": "Asia/Kolkata",
    "Asia/Chungking": "Asia/Chongqing",
    "Asia/Ulan_Bator": "Asia/Ulaanbaatar",
    "Atlantic/Jan_Mayen": "Arctic/Longyearbyen",
    "Australia/ACT": "Australia/Sydney",
    "Australia/Canberra": "Australia/Sydney",
    "Australia/LHI": "Australia/Lord_Howe",
    "Australia/NSW": "Australia/Sydney",
    "Australia/North": "Australia/Darwin",
    "Australia/Queensland": "Australia/Brisbane",
    "Australia/South": "Australia/Adelaide",
    "Australia/Tasmania": "Australia/Hobart",
    "Australia/Victoria": "Australia/Melbourne",
    "Australia/West": "Australia/Perth",
    "Brazil/DeNoronha": "America/Noronha",
    "Brazil/East": "America/Sao_Paulo",
    "Brazil/West": "America/Manaus",
    "Canada/Atlantic": "America/Halifax",
    "Canada/Central": "America/Winnipeg",
    "Canada/Eastern": "America/Toronto",
    "Canada/Mountain": "America/Edmonton",
    "Canada/Newfoundland": "America/St_Johns",
    "Canada/Pacific": "America/Vancouver",
    "Canada/Saskatchewan": "America/Regina",
    "Canada/Yukon": "America/Whitehorse",
    "Chile/Continental": "America/Santiago",
    "Chile/EasterIsland": "Pacific/Easter",
    "Eire": "Europe/Dublin",
    "Etc/Greenwich": "UTC",
    "Etc/UCT": "UTC",
    "Etc/Universal": "UTC",
    "Etc/Zulu": "UTC",
    "Europe/Belfast": "Europe/London",
    "Europe/Monaco": "Europe/Paris",
    "Europe/Tiraspol": "Europe/Chisinau",
    "Mexico/BajaNorte": "America/Tijuana",
    "Mexico/BajaSur": "America/Mazatlan",
    "Mexico/General": "America/Mexico_City",
    "Pacific/Samoa": "Pacific/Pago_Pago",
    "Pacific/Truk": "Pacific/Chuuk",
    "Pacific/Yap": "Pacific/Chuuk",
    "US/Alaska": "America/Anchorage",
    "US/Aleutian": "America/Adak",
    "US/Arizona": "America/Phoenix",
    "US/Central": "America/Chicago",
    "US/East-Indiana": "America/Indiana/Indianapolis",
    "US/Eastern": "America/New_York",
    "US/Hawaii": "Pacific/Honolulu",
    "US/Indiana-Starke": "America/Indiana/Knox",
    "US/Michigan": "America/Detroit",
    "US/Mountain": "America/Denver",
    "US/Pacific": "America/Los_Angeles",
    "US/Samoa": "Pacific/Pago_Pago",
}


def parse_tzif(blob):
    """Parse a TZif v2+ blob. Returns (transitions, ttinfo, abbrs, posix)
    where transitions is [(at_secs, type_idx)] from the second block."""
    assert blob[:4] == b"TZif", blob[:8]
    version = blob[4:5]
    assert version in (b"\0", b"2", b"3", b"4"), version

    def read_header(off):
        (isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt) = struct.unpack_from(">6I", blob, off + 20)
        return off + 44, (isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt)

    off, counts = read_header(0)
    if version == b"\0":
        # v1: only the 4-byte block; treat it as the authoritative one.
        isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt = counts
        times = struct.unpack_from(">%di" % timecnt, blob, off)
        off += timecnt * 4
        indices = blob[off : off + timecnt]
        off += timecnt
        ttinfo = []
        for _ in range(typecnt):
            utoff, isdst, abbrind = struct.unpack_from(">ibB", blob, off)
            off += 6
            ttinfo.append((utoff, isdst, abbrind))
        abbrs = blob[off : off + charcnt].decode("latin-1")
        off += charcnt
        # skip leaps, stdwants, utleap
        return times, indices, ttinfo, abbrs, ""
    # v2+: skip the first block, use the second (8-byte) block.
    isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt = counts
    off += timecnt * 4 + timecnt + typecnt * 6 + charcnt + leapcnt * 8 + isstdcnt + isutcnt
    off, counts = read_header(off)
    isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt = counts
    times = struct.unpack_from(">%dq" % timecnt, blob, off)
    off += timecnt * 8
    indices = blob[off : off + timecnt]
    off += timecnt
    ttinfo = []
    for _ in range(typecnt):
        utoff, isdst, abbrind = struct.unpack_from(">ibB", blob, off)
        off += 6
        ttinfo.append((utoff, isdst, abbrind))
    abbrs = blob[off : off + charcnt].decode("latin-1")
    off += charcnt
    # The footer: "\n" + POSIX TZ rules + "\n".
    posix = ""
    if off < len(blob) and blob[off : off + 1] == b"\n":
        rest = blob[off + 1 :]
        end = rest.find(b"\n")
        posix = rest[:end].decode("latin-1") if end >= 0 else ""
    return times, indices, ttinfo, abbrs, posix


def zone_records(blob):
    """The (at_secs, offset_secs, dst, abbr) transition records of a blob,
    plus the initial (pre-first-transition) offset."""
    times, indices, ttinfo, abbrs, posix = parse_tzif(blob)
    records = []
    for at, type_idx in zip(times, indices):
        utoff, isdst, abbrind = ttinfo[type_idx]
        end = abbrs.find("\0", abbrind)
        abbr = abbrs[abbrind:end] if end >= 0 else abbrs[abbrind:]
        records.append((at, utoff, isdst, abbr))
    initial = None
    if ttinfo:
        utoff, isdst, abbrind = ttinfo[0]
        end = abbrs.find("\0", abbrind)
        abbr = abbrs[abbrind:end] if end >= 0 else abbrs[abbrind:]
        initial = (utoff, isdst, abbr)
    return records, initial, posix


def rs_str(text):
    """A Rust string literal (double-quoted, escapes the quotes/backslashes)."""
    return '"' + text.replace("\\", "\\\\").replace('"', '\\"') + '"'


def parse_posix_offset(text):
    """A POSIX offset `±hh[:mm[:ss]]`: the value is the amount ADDED to local
    time to get UTC, so the seconds-east offset is the NEGATION (POSIX
    `EST5` = UTC-5 = -18000s; `<+03>-3` = UTC+3 = +10800s)."""
    sign = 1
    t = text
    if t.startswith(("+", "-")):
        sign = -1 if t[0] == "-" else 1
        t = t[1:]
    parts = t.split(":")
    hh = int(parts[0])
    mm = int(parts[1]) if len(parts) > 1 else 0
    ss = int(parts[2]) if len(parts) > 2 else 0
    return -(sign * (hh * 3600 + mm * 60 + ss))


def parse_posix_name(text, i):
    """A POSIX TZ name: `<...>` (any chars) or a run of alpha/numeric/sign
    characters (3+ unless bracketed). Returns (name, next_index)."""
    if text.startswith("<", i):
        end = text.find(">", i)
        return text[i + 1 : end], end + 1
    j = i
    while j < len(text) and text[j].isalpha():
        j += 1
    return text[i:j], j


def parse_posix(tz):
    """Parse a POSIX TZ string. Returns
    (std_name, std_offset, dst_name_or_None, dst_offset_or_None,
     start_rule_or_None, end_rule_or_None)
    where a rule is ((kind, args...), time_secs)."""
    t = tz.strip()
    if not t:
        return None, None, None, None, None, None
    if t.startswith(":"):
        t = t[1:]
    i = 0
    std_name, i = parse_posix_name(t, i)
    if i >= len(t):
        return std_name, 0, None, None, None, None
    # The std offset: an optional sign, then digits (and :mm[:ss]).
    j = i
    if j < len(t) and t[j] in "+-":
        j += 1
    while j < len(t) and (t[j].isdigit() or t[j] == ":"):
        j += 1
    std_off = parse_posix_offset(t[i:j])
    i = j
    dst_name = dst_off = None
    start_rule = end_rule = None
    if i < len(t) and t[i] != ",":
        dst_name, nxt = parse_posix_name(t, i)
        i = nxt
        if i < len(t) and t[i] != ",":
            j = i
            if j < len(t) and t[j] in "+-":
                j += 1
            while j < len(t) and (t[j].isdigit() or t[j] == ":"):
                j += 1
            if j > i:
                dst_off = parse_posix_offset(t[i:j])
                i = j
            else:
                dst_off = std_off + 3600
        else:
            dst_off = std_off + 3600
    if i < len(t) and t[i] == ",":
        rules = t[i + 1 :].split(",")
        if len(rules) >= 2:
            start_rule = parse_rule(rules[0])
            end_rule = parse_rule(rules[1])
    return std_name, std_off, dst_name, dst_off, start_rule, end_rule


def parse_rule(spec):
    """A POSIX transition rule `Jnn[n]/time | nnn/time | Mm.w.d/time`.
    Returns ((kind, args...), time_secs)."""
    if "/" in spec:
        rule, time = spec.split("/", 1)
        hhmmss = time.split(":")
        time_secs = int(hhmmss[0]) * 3600
        if len(hhmmss) > 1:
            time_secs += int(hhmmss[1]) * 60
        if len(hhmmss) > 2:
            time_secs += int(hhmmss[2])
    else:
        rule = spec
        time_secs = 7200  # 02:00:00
    if rule.startswith("J"):
        return ("J", int(rule[1:])), time_secs
    if rule.startswith("M"):
        m, w, d = rule[1:].split(".")
        return ("M", int(m), int(w), int(d)), time_secs
    return ("n", int(rule)), time_secs


def epoch_secs(y, mo, d, h=0, mi=0, s=0):
    return int(datetime(y, mo, d, h, mi, s, tzinfo=timezone.utc).timestamp())


def rule_instant(rule, time_secs, year, offset):
    """The UTC epoch seconds of a rule moment in `year`, with the rule time
    interpreted in the local offset `offset` (the offset in effect just
    before the transition)."""
    kind = rule[0]
    if kind == "M":
        _, m, w, d = rule
        first = date(year, m, 1)
        # POSIX wday 0=Sunday; Python's weekday() 0=Monday: shift by (wday-1)%7.
        target = (d - 1) % 7
        first_occurrence = 1 + ((target - first.weekday()) % 7)
        dom = first_occurrence + (w - 1) * 7 if w <= 4 else first_occurrence + 4 * 7
        if w == 5 and dom > calendar.monthrange(year, m)[1]:
            dom -= 7
        return epoch_secs(year, m, dom) + time_secs - offset
    _, n = rule
    if kind == "J":
        doy = n + (1 if calendar.isleap(year) and n >= 60 else 0)
    else:
        doy = n + 1
    return epoch_secs(year, 1, 1) + (doy - 1) * 86400 + time_secs - offset


def extend_records(records, posix):
    """Extend the explicit transitions with the POSIX footer's annual rules
    through 2050 (the corpus's instants are <= 2024; the horizon is the
    extension boundary). Returns (records, final_offset) where final_offset is
    the offset in effect after the last transition."""
    std_name, std_off, dst_name, dst_off, start_rule, end_rule = parse_posix(posix)
    if std_off is None or not records:
        # A fixed zone (no transitions) or an empty footer: the initial
        # offset covers the whole span.
        return records, (std_off if std_off is not None else 0)
    final_offset = std_off
    if not start_rule or not end_rule or dst_off is None:
        # A no-rules footer: the std offset applies after the last transition.
        return records, final_offset
    last_year = datetime.fromtimestamp(records[-1][0], tz=timezone.utc).year
    last_at = records[-1][0]
    dst_abbr = dst_name or records[-1][3]
    std_abbr = std_name or records[-1][3]
    out = list(records)
    for year in range(last_year, 2051):
        start = rule_instant(start_rule[0], start_rule[1], year, std_off)
        end = rule_instant(end_rule[0], end_rule[1], year, dst_off)
        if end < start:
            # southern-hemisphere zones: the end rule is in the following year.
            end = rule_instant(end_rule[0], end_rule[1], year + 1, dst_off)
        if start > last_at:
            out.append((start, dst_off, True, dst_abbr))
        if end > last_at:
            out.append((end, std_off, False, std_abbr))
    out.sort()
    return out, final_offset


def main():
    data = open(DATA, "rb").read()
    src = open(TZNAME, encoding="utf-8").read()
    entries = re.findall(r'\(r"([^"]+)", (\d+)\.\.(\d+)\)', src)

    # The canonical-primary set: the engine's existing supportedValuesOf list.
    sup_src = open(SUPPORTED, encoding="utf-8").read()
    m = re.search(r"SUPPORTED_TIME_ZONES: &\[&str\] = &\[(.*?)\];", sup_src, re.S)
    supported = set(re.findall(r'"([^"]+)"', m.group(1)))

    # Group names by blob range.
    by_range = {}
    for name, lo, hi in entries:
        by_range.setdefault((int(lo), int(hi)), []).append(name)

    # Build the zone table: one entry PER PRIMARY NAME (the same-offset zones
    # share a deduplicated TZif blob but are distinct primaries and must
    # round-trip through `supportedValuesOf`); the transition pool is shared
    # across them. Links resolve to their primary's entry.
    zones = []  # (primary, records, initial, final_offset)
    name_to_zone = {}
    zone_of_blob = {}
    for (lo, hi), names in sorted(by_range.items(), key=lambda kv: kv[0][0]):
        records, initial, posix = zone_records(data[lo:hi])
        records, final_offset = extend_records(records, posix)
        primaries = [n for n in names if n in supported]
        if not primaries:
            primaries = [names[0]]
        for primary in primaries:
            name_to_zone[primary] = len(zones)
            zones.append((primary, records, initial, final_offset))
            # The links in this blob whose map target is this primary (a
            # link is a non-supported name; supported names round-trip).
            for n in names:
                if n not in supported and BACKWARD_LINKS.get(n) == primary:
                    name_to_zone[n] = name_to_zone[primary]
        zone_of_blob[(lo, hi)] = name_to_zone[primaries[0]]
    # The remaining links (not in the map) resolve to their blob's primary.
    for (lo, hi), names in by_range.items():
        for n in names:
            if n not in name_to_zone:
                name_to_zone[n] = zone_of_blob[(lo, hi)]

    name_index = []
    for name, idx in sorted(name_to_zone.items()):
        name_index.append((name.lower(), idx))

    # Emit the Rust module.
    lines = []
    lines.append("//! Generated time-zone data (IANA tzdata 2026c, vendored via")
    lines.append("//! `tools/tzdata/`). Do not edit by hand; regenerate with")
    lines.append("//! `tools/gen_tz_tables.py`.")
    lines.append("")
    lines.append("/// A time-zone transition: the instant the offset takes effect.")
    lines.append("#[derive(Clone, Copy, Debug)]")
    lines.append("pub struct TzTransition {")
    lines.append("    pub at_secs: i64,")
    lines.append("    pub offset_secs: i32,")
    lines.append("    pub dst: bool,")
    lines.append("    pub abbr: &'static str,")
    lines.append("}")
    lines.append("")
    lines.append("/// A time-zone zone: the canonical primary identifier plus its")
    lines.append("/// full transition history (the initial offset applies before the")
    lines.append("/// first transition; the final offset after the last).")
    lines.append("pub struct TzZone {")
    lines.append("    pub primary: &'static str,")
    lines.append("    pub initial_offset: i32,")
    lines.append("    pub initial_dst: bool,")
    lines.append("    pub initial_abbr: &'static str,")
    lines.append("    pub final_offset: i32,")
    lines.append("    pub transitions: &'static [TzTransition],")
    lines.append("}")
    lines.append("")

    # The transition pool (deduplicated across zones).
    pool = []
    pool_index = {}
    for primary, records, initial, final_offset in zones:
        key = tuple(records)
        if key not in pool_index:
            pool_index[key] = len(pool)
            pool.append(records)
    lines.append("/// The deduplicated transition pools (offsets in seconds).")
    lines.append("static POOLS: &[&[TzTransition]] = &[")
    for records in pool:
        lines.append("    &[")
        for at, utoff, isdst, abbr in records:
            lines.append(
                f"        TzTransition {{ at_secs: {at}, offset_secs: {utoff}, dst: {str(bool(isdst)).lower()}, abbr: {rs_str(abbr)} }},"
            )
        lines.append("    ],")
    lines.append("];")
    lines.append("")

    lines.append("/// The zones (indexed by `NAME_INDEX`).")
    lines.append("pub static ZONES: &[TzZone] = &[")
    for primary, records, initial, final_offset in zones:
        utoff, isdst, abbr = initial if initial else (0, False, "UTC")
        lines.append("    TzZone {")
        lines.append(f"        primary: {rs_str(primary)},")
        lines.append(f"        initial_offset: {utoff},")
        lines.append(f"        initial_dst: {str(bool(isdst)).lower()},")
        lines.append(f"        initial_abbr: {rs_str(abbr)},")
        lines.append(f"        final_offset: {final_offset},")
        lines.append(f"        transitions: POOLS[{pool_index[tuple(records)]}],")
        lines.append("    },")
    lines.append("];")
    lines.append("")
    lines.append("/// The case-folded identifier -> zone index (sorted, for lookup).")
    lines.append("pub static NAME_INDEX: &[(&str, u16)] = &[")
    for name, idx in sorted(name_index):
        lines.append(f"    ({rs_str(name)}, {idx}),")
    lines.append("];")

    open(OUT, "w", encoding="utf-8").write("\n".join(lines) + "\n")
    print(f"wrote {OUT}: {len(zones)} zones, {len(pool)} pools, {len(name_index)} names")
    # Sanity: print the spike zones.
    for probe in ["America/Vancouver", "Africa/Monrovia", "Pacific/Apia",
                  "Australia/Lord_Howe", "Asia/Singapore", "America/St_Johns",
                  "Antarctica/Casey", "America/New_York", "Europe/London"]:
        recs = zone_records(data[by_range[probe][0][0]:by_range[probe][0][1]]) if False else None
        for (lo, hi), names in by_range.items():
            if probe in names:
                r, init, posix = zone_records(data[lo:hi])
                print(f"  {probe}: {len(r)} transitions, initial={init[0]}s {init[2]}, first={r[0] if r else '-'}, last={r[-1] if r else '-'}")


if __name__ == "__main__":
    main()
