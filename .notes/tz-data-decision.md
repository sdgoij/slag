# Time-zone data: scoping the decision (intl-plan risk item 3)

This scopes the "chrono-tz vendored vs corpus-derived tables" decision the
Cut 9 sweep exposed. It is the largest remaining Cut 9 bucket: **154 of the
281 new-surface intl402 failures** are time-zone dependent (the other big
bucket is non-iso calendar data, ~90).

## 1. What the corpus actually demands

### Operation split (measured from the failing fixtures)

| Operation | Fixtures | Runtime need |
|---|---|---|
| offset at an instant (construct/from/equals/toString/until/since) | 71 | `offset_at(zone, epoch_ns)` |
| DST transitions (hoursInDay, startOfDay, round, dst-*, add/subtract, disambiguation, same-date-starts-twice) | 38 | next/previous transition instants |
| `getTimeZoneTransition` (next/prev) | 9 | next/previous transition instants |
| canonicalization (links + case: Asia/Calcutta→Asia/Kolkata, Europe/Kiev→Europe/Kyiv, ASIA/calcutta) | 11 | alias table + case folding |
| sub-minute / historical offsets (Africa/Monrovia -00:44:30, LMT pre-1883, Pacific/Apia 2011 dateline) | 12 | full transition history (offsets in seconds) |
| Duration `relativeTo` with named zones (DST day lengths, twenty-five-hour-day) | 13 | offset + transitions |

### Zone surface (measured)

- **311 distinct identifiers** in the intl402 Temporal fixtures.
- **~160 real zones** need offset/transition data. The rest are links,
  renames, case variants, and Etc/GMT forms:
  - 81 explicit IANA links (`backward`): Africa/Asmera, America/Virgin,
    Asia/Calcutta, Europe/Belfast, US/Eastern, ...
  - ~60 more "primary-looking" names are actually renames/backward links:
    Asia/Dacca→Dhaka, Asia/Katmandu→Kathmandu, Asia/Rangoon→Yangon,
    Asia/Saigon→Ho_Chi_Minh, Asia/Macao→Macau, Asia/Ashkhabad→Ashgabat,
    Asia/Thimbu→Thimphu, Asia/Ujung_Pandang→Makassar, Atlantic/Faeroe→Faroe,
    Pacific/Ponape→Pohnpei, Europe/Kiev→Kyiv, America/Jujuy/Mendoza/Rosario→
    Argentina/*, Australia/Yancowinna→Broken_Hill, Asia/Tel_Aviv→Jerusalem,
    Antarctica/McMurdo→Auckland, Pacific/Enderbury→Kanton, America/St_Barthelemy→
    Guadeloupe, Asia/Harbin/Chongqing/Kashgar→Shanghai/Urumqi, ...
  - 9 Etc/GMT forms (+4 invalid rejection-only forms like `Etc/GMT+24`,
    `Etc/GMT-0N` — the fixtures test rejection, no data needed).
  - case-variant tests (`ASIA/calcutta`, `africa/cairo`) — the lookup must
    case-fold.
- The heavy-DST set is small: America/Vancouver (42 fixtures),
  America/New_York (13), Pacific/Apia (12), Europe/Vienna (12),
  Africa/Monrovia (11, sub-minute), America/Los_Angeles (9),
  America/Toronto (7), Asia/Kolkata+Calcutta (10), Europe/London (4),
  America/Sao_Paulo (3), America/Noronha (3), Australia/Lord_Howe (2),
  America/St_Johns (2), plus ~20 others used at one or two instants.
- **Era pin**: the corpus canonicalizes Europe/Kiev→Europe/Kyiv (tzdata
  2022b+) and America/Godthab→America/Nuuk (2023a+) — a tzdata 2023a-era
  snapshot, the same era the test262 submodule pins.

## 2. The two options

### A. External crate (chrono-tz or jiff)

Both are cached locally (`chrono-tz-0.10.4.crate`, `jiff-0.2.35.crate` +
`jiff-tzdb`), so an offline `cargo add` works. Both give offset-at-instant,
the abbreviation, and the DST flag.

**Decisive gap (verified against the crate sources): neither exposes
next/previous transition instants.** `chrono_tz::TimeZone` has only
`offset_from_utc_datetime`/`abbreviation`; `jiff::tz::TimeZone` has
`to_offset`/`to_offset_info` (offset + DST + abbreviation) but no
transition API. The corpus's `getTimeZoneTransition` (9), `hoursInDay` (6)
and the DST arithmetic (~30) need the transition table. Workarounds:

- reach into jiff's internal TZif tables (not public API — fragile);
- write a TZif parser over the embedded bytes (`jiff::tz::TimeZone::tzif`
  parses one, but still hides transitions).

Either workaround reimplements the table + binary search the derived
approach generates anyway — but with a dependency and an implicit data
version you don't control.

### B. Corpus-derived tables (the established Slag pattern)

The `crates/unicode/build.rs` precedent: the fixtures ARE the data spec;
a build script derives the tables at build time from the pinned corpus
(erroring if the submodule is missing).

- **Data source**: a vendored minimal IANA subset — the ~160 zone files +
  the `backward` link file from tzdata 2023a — pinned alongside the test262
  submodule (the same era the corpus encodes).
- **Generator**: `tools/gen_tz_tables.py` (following the
  `crates/unicode/build.rs` corpus-derivation pattern) parses the tzdata
  text format
  (Zone/Rule/Link lines, a stable ~30-year-old format) and writes
  `crates/unicode/src/tz_data.rs`.
- **Runtime table**: per-zone sorted transition array
  `(at_ns, offset_secs, is_dst, abbreviation)`. Binary search serves all
  four operations natively — offset at an instant, next transition,
  previous transition, and the alias/canonicalization table.
- **Sub-minute and pre-1970 offsets come free**: the tzdata text format
  carries the full history (LMT from ~1880, Monrovia's -00:44:30 until
  1972, the Apia 2011-12-30 dateline jump).
- **Size**: ~160 zones × ~50 transitions ≈ 8-10k rows ≈ 100-150 KB source,
  static tables at runtime.

## 3. The runtime API the tables must serve

```rust
// crates/unicode/src/tz_data.rs (generated)
/// The offset (seconds), DST flag, and abbreviation in effect at epoch_ns.
pub fn offset_seconds_at(zone: &str, epoch_ns: i128) -> Option<(i64, bool, &'static str)>;
/// GetIANATimeZoneNextTransition / PreviousTransition.
pub fn next_transition(zone: &str, epoch_ns: i128) -> Option<TzTransition>;
pub fn previous_transition(zone: &str, epoch_ns: i128) -> Option<TzTransition>;
/// GetAvailableNamedTimeZoneIdentifier: case-folded alias → primary.
pub fn canonical_identifier(name: &str) -> Option<&'static str>;
/// AvailablePrimaryTimeZoneIdentifiers (supportedValuesOf("timeZone")).
pub fn primary_identifiers() -> &'static [&'static str];
```

Consumers:

- `crates/runtime/src/builtins/temporal/` — `offset_time_zone_offset_ns`
  (currently UTC + fixed offsets only), the ZonedDateTime machinery
  (local time, transitions, disambiguation), `getTimeZoneTransition`.
- `crates/runtime/src/builtins/intl/date_time_format.rs` —
  `named_zone_offset` (currently falls back to UTC) and `format_time_zone`,
  so the ZDT named-zone `toLocaleString` fixtures
  (`options-timeZoneName-affects-instance-time-zone`, `time-zone-canonicalized`)
  and the canonical `resolvedOptions().timeZone` work.
- `Intl.supportedValuesOf("timeZone")` — the generated primary list
  reconciles with the existing 446-entry `SUPPORTED_TIME_ZONES`
  (the `Temporal/ZonedDateTime/supported-values-of.js` fixture iterates it
  and constructs every entry).

## 4. The spike (mirrors the Cut 2 spike)

Before committing to the generator, hand-write the tables for the four
hardest zones and measure:

1. **America/Vancouver** — DST rules + pre-1883 LMT (`offset-before-1883`).
2. **Africa/Monrovia** — sub-minute offset until 1972 (`sub-minute-offset`).
3. **Pacific/Apia** — the 2011-12-30 dateline jump (`dst-skipped-cross-midnight`).
4. **Australia/Lord_Howe** — the 30-minute DST offset (`dst-less-than-hour`).

Wire the four operations into `temporal/instant.rs` + `date_time_format.rs`,
then sweep the DST/transition cluster (~45 fixtures). Success = the cluster
turns green and the table format feels right; that validates the era pin
and the data shape before the generator does the remaining ~156 zones.

## 4a. Spike results (done)

The spike went further than the four hand-written zones: instead of writing
tables by hand, it extracted the **real IANA data** from the jiff-tzdb
flattened blob already in the cargo cache (`tools/tzdata/` now vendors the
202KB `concatenated-zoneinfo.dat` + the `tzname.rs` name index, tzdata
2026c). Findings:

- **The extraction matches the corpus's pinned values exactly**: America/New_York's
  1883-11-18T17:00Z standard-time introduction, Europe/London's
  1847-12-01T00:01:15Z LMT->GMT switch, Vancouver's -08:12:28 pre-1883 LMT,
  the LA 1965 CA rule (offsetNanoseconds at exactly 09:00:00Z), Apia's
  2011-12-30T10:00Z -10->+14 dateline jump, Monrovia's -00:44:30 MMT.
  Pinned as unit tests in `crates/unicode/src/tz.rs`.
- **The POSIX footer is required**: jiff's flattened data trims the explicit
  transitions (NY ends 2007, London 1996) and relies on the footer rules
  string. The generator evaluates the POSIX TZ rules (the Mm.w.d/Jn/n
  forms, local-time interpretation, southern-hemisphere wrapping) and
  extends the tables through 2050 — the corpus's post-1996 instants need
  it (London 2020-03-29T01:00Z, NY 2019-11-03T06:00Z).
- **Blob dedup means same-offset zones share data but stay distinct
  primaries** (Africa/Abidjan != Africa/Accra): the generator emits one
  zone entry per primary name, sharing the transition pool; links (the
  `backward` set) resolve to their primary (Asia/Calcutta -> Asia/Kolkata,
  Europe/Monaco -> Europe/Paris, America/Godthab -> America/Nuuk). The
  engine's `SUPPORTED_TIME_ZONES` had two links (Europe/Monaco,
  America/Godthab) that were removed; the DateTimeFormat timeZone option
  now resolves + canonicalizes through the generated tables.
- **Operations wired**: case-folded lookup, offset-at-instant,
  next/previous offset-changing transitions (abbreviation-only transitions
  are skipped — the corpus pins Paris's 1891 LMT->PMT switch is not a
  transition), per-midnight start-of-day (hoursInDay/startOfDay with a
  midnight gap/overlap), the offset/offsetNanoseconds getters, toString,
  the string/property-bag ZonedDateTime.from wall-clock conversion
  (compatible-disambiguation approximation), and the DateTimeFormat
  named-zone offsets.
- **Measured**: tz cluster 0 -> **47/116 pass**; full intl402 1355 ->
  **1402 pass**; **0 regressions** on the previously-passing union; clippy
  `-D warnings` clean; workspace tests green.
- **The remaining 69 cluster failures are Temporal-algorithm work, not
  data work**: the DST-aware add/subtract/round/with machinery, the
  property-bag disambiguation options (prefer/reject/ignore gaps and
  overlaps), Duration `relativeTo` with named zones, and a couple of
  getTimeZoneTransition/hoursInDay edges. The data decision is settled.
- **Era note**: jiff-tzdb is 2026c vs the corpus's 2023a-era pin; the
  fixture-tested instants (<= 2024) are stable across the gap, but the
  final generator should vendor the exact pinned era (tzdata 2023a).

## 5. Decision matrix

| Criterion | chrono-tz / jiff | Derived tables |
|---|---|---|
| transition instants (getTimeZoneTransition, hoursInDay, DST arithmetic) | ✗ not public API | ✓ native (the table IS the transitions) |
| sub-minute + pre-1970 offsets | ✓ | ✓ |
| era pin vs the corpus | implicit (crate's tzdata snapshot) | explicit (vendored 2023a subset) |
| self-contained Rust ethos | ✗ 2-3 deps, ~1.5-2 MB data | ✓ |
| offline build | ✓ (cached) | ✓ |
| supportedValuesOf primary list | partial (chrono-tz iter) | ✓ generated |
| fits the plan architecture | fallback only | primary |

## 6. Recommendation

**Derived tables + a vendored minimal tzdata 2023a subset**, generator
following the `crates/unicode/build.rs` corpus-derivation pattern, tables in
`crates/unicode/src/tz_data.rs`. Start with the 4-zone spike to validate
the era pin and the table format against the corpus's exact offsets before
generating the rest. The external-crate route is now effectively a data
SOURCE at best (its TZif bytes still need a parser of your own for
transitions) — strictly worse than derived on every axis that matters here.

## 7. Risks

1. **Era exactness** — the corpus pins a 2023a-era snapshot; a vendored
   subset must match Monrovia's sub-minute end date, the Apia jump, the
   Kyiv/Nuuk renames, etc. The corpus is the net; the spike measures this
   before the generator commits to a full extraction.
2. **The Etc/GMT±N forms** — the valid fixed offsets (`Etc/GMT-14`..+12)
   and the rejection-only forms; the engine's offset parsing already
   handles the valid ones, the generator must not emit data for the
   invalid forms.
3. **Case folding** — the corpus tests `ASIA/calcutta`/`africa/cairo`;
   the lookup must fold case before the alias table (the engine already
   uppercases in `lookup_named_time_zone`).
4. **The generator's zic-format parser** — tzdata text format is stable
   but has corners (rule continuations, `S` suffix, TZif-vs-text
   discrepancies); the dogfooded `.js` sweep is the regression net.
