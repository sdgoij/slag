# Intl (ECMA-402): implementation plan

This is the engineering spec for implementing the ECMA-402 Intl API. The
reference is the vendored `ecma402.html` (the rendered spec from
https://tc39.es/ecma402/ — the current ECMAScript 2027 draft era, which
matches the proposal-stage features the corpus tests; the live URL is
the fallback reference). The vendored `spec.html` is ECMA-262 only. The
regression net is the `intl402` fixture area of the pinned test262
submodule — the corpus encodes the exact locale data this implementation
must match.

Status: Cuts 1-8 are committed and green (NumberFormat, Locale, PluralRules,
RelativeTimeFormat, ListFormat, DisplayNames, DateTimeFormat, Collator,
Segmenter, DurationFormat). Cut 9 (the intl402/Temporal integration) is in
flight: the Temporal×DateTimeFormat integration (format/toLocaleString on
Temporal values, the [[Calendar]] slots, the un-skipped intl402 gate) is
committed; the **time-zone data decision is made** (corpus-derived tables,
spike validated — see `docs/tz-data-decision.md`); the remaining work is the
DST-aware Temporal algorithms (add/subtract/round/with, property-bag
disambiguation, Duration relativeTo) and the non-iso calendar data.

## 1. The fixture surface (measured)

`test262/test/intl402/` holds **3,335 fixtures**. Temporal (the Intl×Temporal
integration) is **2,029** of them; the other **1,306** break down as:

| Area | Fixtures | Core work |
|---|---|---|
| NumberFormat | 249 | + v3 (99) + unified (68) + formatToParts |
| DateTimeFormat | 244 | + formatRange (37); the biggest data surface |
| Locale | 168 | + Locale-info (60); BCP 47, data-free core |
| DurationFormat | 110 | builds on Temporal.Duration + NumberFormat |
| ListFormat | 81 | list patterns per locale |
| RelativeTimeFormat | 80 | units strings per locale |
| Segmenter | 79 | grapheme/word/sentence segmentation |
| Intl (namespace) | 66 | getCanonicalLocales, supportedValuesOf, toStringTag |
| Collator | 65 | the hard data problem (collation) |
| DisplayNames | 57 | display-name data per locale |
| PluralRules | 53 | plural categories per locale |
| String / Date / Number / BigInt / Array / TypedArray / FallbackSymbol | 52+2 | toLocale* / localeCompare integration |

**Harness surface** (what `run_fixture` must additionally allow): the
intl402 includes are `testIntl.js` (153 uses), `testIntlNumberFormat.js`
(4), and already-allowed `propertyHelper.js` (261), `compareArray.js`
(71), `isConstructor.js` (27), `dateConstants.js`, `deepEqual.js`,
`testTypedArray.js`. Both `testIntl.js`/`testIntlNumberFormat.js` ship in
the submodule's harness dir. The feature gates the harness must track as
cuts land: `Intl.Locale` (172), `Intl.DurationFormat` (109),
`Intl.NumberFormat-v3` (99), `Intl.ListFormat` (81),
`Intl.RelativeTimeFormat` (79), `Intl.Segmenter` (79), `Temporal` (71 in
non-Temporal dirs), `Intl.NumberFormat-unified` (68), `Intl.Locale-info`
(60), `Intl.DisplayNames` (47), `Intl.DateTimeFormat-formatRange` (37).

**Locale surface** (data scope bound): the fixtures concentrate on a
small real-locale set — en (≈287), en-US (127), de (43), lt (32), ja-JP
(29), de-DE (25), zh-TW (25), ar (20), fr (20), ko-KR (19), ja (18), tr
(17), az (16), zh (15), en-GB (14), sv (11), th (10), ab (10), no-bok
(8), kn (7) — roughly **40 real locales**, plus the arbitrary-tag tests
(`foo`, `abc-abcdefghi`) that exercise parsing only. The data tables need
to cover the fixture locales, not the world.

## 2. The data-strategy decision (the crux)

The corpus asserts **exact, locale-data-dependent strings** — e.g.
`format-significant-digits.js` checks `new Intl.NumberFormat("ar",
{numberingSystem:"arab"}).format(0)` against Arabic-Indic digits, and the
same table for `de`/`th`/`ja` with `latn`/`thai`/`hanidec`. The expected
values encode a specific CLDR era plus ICU-specific behavior in places.
This drives the strategy:

- **ICU4C via bindings** (what V8 does) — a C++ build dependency and
  ~30MB of data, plus pinning the exact ICU version the corpus was
  generated against. Foreign to this codebase's self-contained Rust ethos.
- **ICU4X** — pure Rust, but a large workspace (locale, decimal,
  plurals, datetime, collator, list, segmenter, displaynames + the
  datagen pipeline), and its output is not guaranteed to byte-match the
  corpus's ICU-era expectations.
- **Corpus-derived data** — the established Slag pattern
  (`gen_regexp_unicode_tables.py` generates `\p{...}` tables *from the
  fixtures* because the fixtures ARE the data spec). Intl is the same
  shape at larger scale: the **algorithms** are spec'd exactly (rounding,
  pattern application, plural selection, list joining) and get real
  implementations; the **data** (separators, digits, plural rules, date
  patterns, collation) is parametric and gets derived tables for the
  ~40 fixture locales.

**Recommendation: real algorithms + corpus/CLDR-derived data tables.**
A generator (following the `gen_regexp_unicode_tables` precedent — the
`.py` is the tool, the `.js` is the dogfooded benchmark) extracts the
data from (a) the fixtures' expected outputs and (b) a vendored minimal
CLDR subset pinned to the era the fixtures encode. The decision is
validated by a **spike at the start of Cut 2** (below): hand-write the
NumberFormat data for 6 locales and measure how far derived data goes
before the CLDR subset is needed. **Fallback**: if collation (Cut 6)
proves too deep to derive, use ICU4X's `icu_collator` as the one
allowed external data dependency.

## 3. Architecture

- `crates/runtime/src/builtins/intl/` — one module per component
  (`locale.rs`, `number_format.rs`, `plural_rules.rs`,
  `date_time_format.rs`, `collator.rs`, `list_format.rs`,
  `relative_time_format.rs`, `segmenter.rs`, `display_names.rs`,
  `duration_format.rs`, `supported_values.rs`), each with the standard
  `install(realm)` pattern used by `builtins/temporal/`; the `%Intl%`
  intrinsic registers the namespace object and constructors.
- `crates/unicode/` grows the Intl data modules (numbering systems,
  plural rules, date patterns, collation, display names) plus the
  generated tables; the generator lives in `tools/`.
- `Agent`/`Realm` gain the internal slots the components need
  (the existing `temporal_data` map is the pattern for
  `[[InitializedNumberFormat]]`-style records).
- `crates/test262` gains `Area::Intl402` so the corpus becomes the
  regression net (see Cut 0).

## 4. Cut order (each cut ends clippy-clean + workspace-green + a
sweep diff at zero regressions on the previously-passing union)

**Cut 0 — measurement infrastructure (no engine code).** Add
`intl402` as a sweep area; allow `testIntl.js`/`testIntlNumberFormat.js`
in the include set; add the feature-gate table to `run_fixture` /
`tools/skip_tally.js` (all Intl features initially unimplemented →
everything skips with an explicit reason). Baseline the area: the
fail+crash union is the comparison from here on. The 2,029 Temporal
fixtures stay skipped until Cut 9.

**Cut 1 — `%Intl%` namespace + BCP 47 locale machinery (data-free).**
`Intl.getCanonicalLocales`, the internal `CanonicalizeLocaleList` /
`CanonicalizeUnicodeLocaleId` / `IsStructurallyValidLanguageTag` (all
syntactic + case folding — no registry data), `Intl[@@toStringTag]`, the
FallbackSymbol wiring, and `Intl.Locale` (constructor + prototype:
maximize/minimize, `toString`, getters, `Intl.Locale-info` deferral).
Fixtures: ~66 Intl + ~168 Locale + 2 FallbackSymbol. This cut is pure
ECMA-402 + BCP 47 — the fastest way to prove the module plumbing and the
sweep loop end-to-end.

**Cut 2 — NumberFormat** (+ Number/BigInt/Array/TypedArray
`toLocaleString`). The full algorithm: `InitializeNumberFormat` (locale
resolution, options coercion, pattern generation), `ToRawFixed` /
`ToRawPrecision` / rounding, grouping, then `formatToParts`, then the
unified (notation/compact/unit) and v3 (roundingMode etc.) sub-features.
Data: numbering systems (latn, arab, thai, hanidec + the tested set),
decimal/group separators per locale, ISO 4217 minor units, currency
symbols for the tested currencies. **The data-strategy spike lives
here**: 6 locales by hand → generator → CLDR subset. Fixtures: 249 + 21
integration.

**Cut 3 — PluralRules + RelativeTimeFormat.** Plural categories
(cardinal + ordinal) per locale (the `one/few/lt`-style fixture strings
are real data), unit strings for the tested units. Fixtures: 53 + 80.

**Cut 4 — ListFormat + DisplayNames.** List patterns (the
`conjunction/disjunction/unit` templates per locale) and display-name
data (language/region/script/currency names for the tested locales).
Fixtures: 81 + 57.

**Cut 5 — DateTimeFormat** (+ String/Date `toLocale*`, the largest data
surface). Calendar machinery (gregory first; the tested alternate
calendars as needed), month/weekday/day-period/era names, hour cycles,
date/time patterns, and **time zones** — the IANA tz data question was
deferred to Cut 9 and **decided in favor of corpus-derived tables**
(`docs/tz-data-decision.md`): a generator over a vendored IANA subset
(chrono-tz/jiff lack the transition API the corpus's getTimeZoneTransition
and hoursInDay fixtures require). `formatToParts` + `formatRange`.
Fixtures: 244 + 37 formatRange + 31 String/Date.

**Cut 6 — Collator.** The hard data problem: real collation
(DUCET-level) for the tested locales, including `de-u-co-phonebk` (9
fixtures). Decide: derived DUCET subset vs the `icu_collator` fallback.
Fixtures: 65 + the String `localeCompare` portion of the 19.

**Cut 7 — Segmenter.** Grapheme/word/sentence segmentation — the
`unicode-segmentation` crate covers grapheme+word but not sentence;
sentence boundaries need generated tables + the algorithm (the corpus
asserts exact boundaries). Fixtures: 79.

**Cut 8 — DurationFormat.** Builds on Temporal.Duration internals
(already implemented) + NumberFormat composition, units data.
Fixtures: 110.

**Cut 9 — the intl402/Temporal area (2,029).** DateTimeFormat.format on
Temporal types (Instant, ZonedDateTime, PlainDate/Time/DateTime),
`toLocaleString` on the Temporal prototypes, Temporal.Instant/PlainDate
etc. integration. Un-skip via the feature table. Depends on Cut 5 + the
existing Temporal implementation.

**In flight.** Committed: the `[[Calendar]]` slots on Temporal instances,
`HandleDateTimeValue` + the (any/all) re-resolution for format/formatToParts/
formatRange on Temporal values, the `toLocaleString` family on all Temporal
prototypes, and the un-skipped intl402 gate (sweep: 1355 pass, 0
regressions on the previously-passing union). The **time-zone data spike**
landed the derived tables (data extraction + POSIX-rule extension through
2050 + the four operations), wiring the offset getters, hoursInDay,
getTimeZoneTransition, ZonedDateTime.from string/plain conversion, toString,
and the DateTimeFormat named-zone offsets (sweep: 1402 pass, 0
regressions). The **DST-aware wall→instant machinery** landed: the shared
resolver (possible instants + gap/overlap disambiguation, the
prefer/reject/ignore/use offset semantics, and the start-of-day paths),
`add`/`subtract` via AddZonedDateTime (days stay calendar days on the wall
clock), `round` to-day (startOfDay windows), `with`/`withPlainTime`/`from`
(sweep: 1431 pass, 0 regressions). **Remaining:** Duration `relativeTo`
with named zones (compare/round/total), `until`/`since`
(DifferenceZonedDateTimeWithRounding), the non-iso calendar data
(chinese/dangi/hebrew/japanese/islamic conversions, era fields, leap
months), sub-minute offset matching, and a few edge singles.

## 5. Risk register (investigate before implementing)

1. **CLDR-version exactness** — the corpus pins a CLDR era; derived data
   must match its expected strings. The corpus IS the net; the generator
   extracts from fixtures + a vendored CLDR subset pinned to that era.
   The Cut 2 spike measures the risk.
2. **Collation depth** — `de-u-co-phonebk`, `sv` (å sorts after z), `tr`,
   `no-bok` require real collation rules. Highest-risk data surface;
   `icu_collator` is the escape hatch.
3. **Time zones** — DST-correct offsets for specific zones. **DECIDED in
   Cut 9: corpus-derived tables** (the derived-data precedent), validated
   by the spike: the generator parses a vendored IANA TZif blob
   (`tools/tzdata/`, from jiff-tzdb) + evaluates the POSIX footer rules
   through 2050; chrono-tz/jiff were ruled out because neither exposes the
   transition instants the `getTimeZoneTransition`/`hoursInDay`/DST
   arithmetic fixtures require. The spike's 47-fixture gain on the tz
   cluster + the pinned unit tests confirm the era/data shape; the final
   generator should vendor the exact tzdata 2023a-era pin.
4. **ICU quirks the corpus encodes** — some fixtures assert
   ICU-specific behavior (number rounding edges, pattern details); the
   derived-tables approach bakes them in by construction.
5. **`formatToParts`/`formatRange` exactness** — part boundaries are
   asserted as exact arrays; the parts model must match.
6. **`Intl.Locale-info` and `supportedValuesOf`** — data-driven lists
   (calendars, collations, currencies, numbering systems, time zones)
   whose contents the fixtures assert; the lists must be complete for
   the tested entries.
7. **Harness surface** — `testIntl.js` exercises the engine through
   helper assertions; its expectations (e.g. resolvedOptions shapes)
   are part of the contract.
8. **Perf** — Intl is not in the `--bench` gates; correctness-first.
   `NumberFormat.format` in a loop is a future bench candidate, not a
   gate.

## 6. Validation per cut

- `cargo clippy --workspace --all-targets -- -D warnings` clean;
  `cargo test --workspace` green.
- The intl402 sweep becomes the regression net: rebuild `sweep.exe`,
  diff the fail+crash union against the previous cut (Cut 0 records the
  all-fail baseline; every cut must not regress the already-passing
  union).
- Unit tests asserting corpus-shaped outputs (e.g. the
  `format-significant-digits` table for one locale) live in
  `crates/runtime/src/builtins/intl/`.
- The data generator is dogfooded through the engine (the `.js` variant)
  like `gen_regexp_unicode_tables`.

## 7. Non-goals

- No full CLDR/ICU adoption unless the Cut 2 spike or Cut 6 collation
  forces it (bounded: `icu_collator` only).
- No Temporal work beyond the intl402 integration points (Temporal
  itself is implemented and its skips are closed).
- No Intl performance gates in `docs/perf.md`.
