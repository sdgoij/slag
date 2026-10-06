# ECMAScript 2027 (18th edition): the 2026 → 2027 delta

**Status: audited 2026-10-06.** The edition bump is nomenclature. The vendored
`spec.html` refresh (`cced75e`) plus one engine-side rename are the entire
change; no 2027 clause adds a conformance requirement the engine does not
already meet. The pinned test262 — updated upstream to the current era — passes
100% (`sweep all` 48,632 pass / 0 fail / 1 stale skip of 48,633; `sweep intl402`
3,365 / 0), which is the empirical confirmation.

The context that made this confusing: the repo *targeted* "2026 (17th edition)"
in `README.md` / `PLAN.md`, but vendored the tc39 **living draft**, which
self-labels with the *next* edition. The file already read "ECMAScript 2027 …
eighteenth edition" before the refresh, so the "2026" claim in the docs was the
stale half. `cced75e` refreshed the file to a newer revision of the same 2027
document and corrected the docs.

## 1. How the delta was measured

The two vendored revisions are `HEAD~1:spec.html` (the older 2027 draft) and
`spec.html` (the refresh). The diff is done on **tag-stripped text**, because a
raw HTML line-diff of a 3 MB ecmarkup build is unreadable and the line churn is
mostly rewrapping:

```sh
git show HEAD~1:spec.html > scratch/spec_old.html
# norm.js: split tags to lines, strip tags, collapse whitespace, drop blanks
node scratch/norm.js scratch/spec_old.html scratch/spec_old.txt
node scratch/norm.js spec.html        scratch/spec_new.txt
diff -u scratch/spec_old.txt scratch/spec_new.txt     # -> spec_diff.txt
```

A clause-level map is derived by pairing each `emu-clause id=` with its first
`<h1>` and diffing the id → title tables (`ct_old.tsv` vs `ct_new.tsv`). The
`scratch/` artifacts (gitignored) are: `spec_diff.txt`, `ct_old.tsv`,
`ct_new.tsv`, `added.txt`, `removed.txt`.

The normalized text diff is 1,458 changed lines; the clause map shows 2,213 →
2,215 clauses, with 6 added, 4 removed, and 35 retitled.

## 2. Structural change

**Added (6):**

- `sec-iterator.prototype.join` — `Iterator.prototype.join ( _separator_ )`, a
  built-in function. Already implemented (`crates/runtime/src/builtins/iterator.rs`,
  `JOIN`) and green on its `builtins/Iterator/prototype/join` fixtures; the old
  vendored draft was simply behind the test262 corpus.
- `sec-epoch-nanoseconds` — `Epoch Nanoseconds and Range`, a `<dfn>` for the
  epoch-nanosecond count type.
- `sec-minutefromtime` / `sec-secondfromtime` / `sec-millisecondfromtime` — the
  renamed Date AOs (below).
- `sec-snaptointeger` — `SnapToInteger` (below).

**Removed (4):**

- `sec-daysinyear` — `DaysInYear` is inlined into `InLeapYear` (which now does
  `Let y be YearFromTime(tv)` and tests the leap rule directly).
- `sec-minfromtime` / `sec-secfromtime` / `sec-msfromtime` — renamed.

**Retitled (35):** every one is a Date parameter rename (`_date_` → `_day_`,
`_min_` → `_minute_`, `_sec_` → `_second_`, `_ms_` → `_millisecond_`) or an
AO return-type notation change (`an integral Number in the inclusive interval
from *+0*𝔽 to *59*𝔽` → `an integer in the inclusive interval from 0 to 59`).
Three are neither: `SetViewValue` returns `~unused~` instead of `*undefined*`,
`StringGetOwnProperty` takes "a String exotic object" instead of "an Object that
has a [[StringData]] internal slot", and `TemplateString(s)` takes
`_escapes_: ~raw~ or ~cooked~` instead of `_raw_: a Boolean`.

## 3. The classes of change — all non-behavioral

1. **Terminology.** "finite **Number**"; abstract operations now return
   mathematical *integers* rather than `an integral Number`, with callers
   wrapping in `𝔽()` (`HourFromTime`, `DateFromTime`, `WeekDay`, `DayFromYear`,
   `TimeFromYear`, `YearFromTime`, `MonthFromTime`, `DayWithinYear`,
   `TimeWithinDay`, `InLeapYear`, …).
2. **The internal-methods table** (`[[GetPrototypeOf]]` … `[[OwnPropertyKeys]]`,
   `[[Call]]`, `[[Construct]]`) lost its `Signature` column and gained typed
   `Definitions` with completion records ("either a normal completion containing
   … or a throw completion").
3. **`SnapToInteger`** extracted: `ToNumber`, throw `RangeError` on NaN/±∞,
   truncate-or-reject a finite non-integer, then clamp to an optional inclusive
   `[minimum, maximum]`. Its only callers are `Number::toString` and
   `BigInt::toString`'s radix coercion (`? SnapToInteger(_radix_, ~truncate~, 2, 36)`),
   which is behavior-identical to the `ToIntegerOrInfinity` + range check it
   replaces.
4. **Grammar notation.** Trailing-comma alternatives collapse to `,?`:
   `ObjectBindingPattern : { BindingPropertyList ,? }`, `ObjectAssignmentPattern`,
   `ObjectLiteral : { PropertyDefinitionList ,? }`, `( Expression ,? )`,
   `( ArgumentList ,? )`, `{ ImportsList ,? }`, `{ ExportsList ,? }`.
5. **Template literals.** `Static Semantics: TemplateString(s)` takes
   `_escapes_: ~raw~ or ~cooked~` instead of `_raw_: a Boolean`; the raw/cooked
   selection is unchanged (`if _escapes_ is ~raw~, return the TRV; return the TV`).
6. **Binding/destructuring algorithm renames** (`_bindingId_` → `_name_`,
   `_v_` → `_value_`, `_envRecord_`) and `_clampedLen_` → `_clampedLength_`.
7. **A `[normative-optional, legacy]` annotation** on `SetFunctionName`'s choice
   of `_prefixedName_` for a built-in's `[[InitialName]]`, and an
   `IdentifierCodePoints`-based `SetFunctionName` name computation.
8. **ECMA-402 hook prose** ("must implement this method as specified in
   ECMA-402 … Otherwise, the following specification is used") on the
   `toLocaleString` family, and non-finite steps clarified on the Number
   formatting methods.

## 4. The engine's one sync

This repo keeps abstract operations under the spec's names so a conformance bug
diffs cleanly, so the three Date renames are mirrored in
`crates/runtime/src/builtins/date.rs` (plus their `spec 21.4.1.x` citations):

| spec 2026 | spec 2027 | engine |
|---|---|---|
| `MinFromTime` | `MinuteFromTime` | `min_from_time` → `minute_from_time` |
| `SecFromTime` | `SecondFromTime` | `sec_from_time` → `second_from_time` |
| `msFromTime` | `MillisecondFromTime` | `ms_from_time` → `millisecond_from_time` |

`DaysInYear` needs no engine change (the engine never had it; the leap rule
lives inline in `days_in_month`). `GetUTCEpochNanoseconds`
in `builtins/temporal/iso.rs` only changed its *type annotation* (`a BigInt` →
`an epoch nanoseconds count`), which is spec prose, not code.

Verified: `cargo test -p runtime --lib date` 24 pass / 0 fail; `cargo fmt --all
-- --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean.

## 5. Deliberately not adopted

- **`SnapToInteger` is not extracted as a named helper.** The engine does not
  name the radix coercion in `Number`/`BigInt` `toString`, and the extraction is
  a spec-editorial unification, not a behavior. Add it only if the name-level
  fidelity is wanted.
- **The AO return-type notation** (mathematical integers vs `Number`) is not
  mirrored: the engine uses `f64` / `i64` at these boundaries, which is the same
  behavior the spec's `𝔽()` wrapping describes.
- **The internal-methods table format** is spec-prose and has no engine surface.

If a later edition *does* make a behavioral change, the method in §1 is the one
to re-run: normalize both revisions and diff the prose, then diff the clause
map for added/removed/retitled clauses.
