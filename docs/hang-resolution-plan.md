# Hang-resolution plan: clearing the remaining test262 sweep hangs

Scope: close the test262 sweep's remaining hang clusters. The RegExp
property-escape cluster — the array-store plan's motivation — is cleared
(280 hangs) by the dense-elements commit `609ce62` plus the uncommitted
regexp per-char predicate fix. This plan covers what remains, in order,
with a named output and an acceptance gate at every step so progress is
auditable and nothing is changed on a guess.

References: `docs/array-store-plan.md` (landed), `docs/perf.md`
("Interpreter per-op floor"), the `slag-conformance` skill (sweep
methodology: union-of-failures comparisons, never raw counts).

## Measured state (2026-08-31)

Last full sweep (pre-regexp-fix binary): 48,622 fixtures, **287 hangs**.
Measured since, on the current tree:

| cluster | before | after |
|---|---|---|
| RegExp/property-escapes/generated | 280 hangs | **613/613 pass** (regexp predicate fix, uncommitted) |
| TypedArray/prototype/copyWithin | 3 hang | 3 hang (in scope) |
| Temporal (PlainDate/PlainDateTime since/until) | 4 hang | 4 hang (scope TBD, below) |

The dense-elements work is committed (`609ce62`); the regexp fix is a
working-tree change and must be committed first.

## Item 1 — Commit the regexp predicate fix

`class_matches` (crates/regexp/src/engine.rs) now matches `\p{…}` /
`\P{…}` by binary search over the process-cached per-predicate ranges
(`predicate_ranges`) instead of a per-character property-name lookup —
the fixtures run anchored `\p{…}+` over ~2.1M-char strings, so the name
lookup per character dominated.

- Steps: commit the engine.rs change as its own commit.
- Acceptance: the property-escapes cluster (613 fixtures) passes 613/613
  on the committed tree; regexp crate tests 38/38; clippy `-D warnings`
  clean.

## Item 2 — Isolate the copyWithin hangs (measure, don't guess)

Known facts: all three fixtures are the "detached" coercion variants —
`copyWithin(target, start, end)` where a `valueOf` detaches the buffer
mid-argument-coercion. Hand-rolled repros of the detached-copyWithin
path for all 11 typed-array constructors throw the correct TypeError, so
the hang is in a layer the repros did not cover (harness mechanics,
`makeCtorArg`, `Array.prototype.fill`, or typed-array construction from
the array-like).

- Step 2.1 — reproduce exactly: run the 3 fixtures through the sweep
  worker with the real harness prelude (testTypedArray.js,
  detachArrayBuffer.js). Output: hang confirmed on the current binary,
  or a pass (which would reclassify the earlier result as load).
- Step 2.2 — bisect the fixture body with checkpoints: constructor loop,
  array build + `fill`, typed-array construction, `copyWithin` with a
  detaching `valueOf`, asserts. Output: the single operation that does
  not terminate, and the constructor that triggers it.
- Step 2.3 — classify: infinite loop vs. quadratic blowup vs. harness
  prelude. Known suspect (unconfirmed): `new BigInt64Array([7])` throws
  "Cannot convert a Number to a BigInt", though spec 25.2.4.3 requires
  `ToBigInt(7)` to succeed — the fixture's `makeCtorArg` may rely on it.
- Acceptance: a written statement naming the operation, the layer
  (engine vs. harness), and the failure class. No code changes before
  this gate.

## Item 3 — Fix the copyWithin root cause

- Steps: one targeted change at the named layer; a regression unit test
  in the runtime crate that runs the exact failing shape.
- Acceptance: the 3 fixtures pass in isolation; the copyWithin cluster
  (41 fixtures) passes 41/41; runtime crate tests green; clippy clean.

## Item 4 — Full validation

- Step 4.1: full `built-ins` sweep vs. the `609ce62` baseline — compare
  the fail+crash+hang UNIONS, run under similar load (the conformance
  skill's methodology).
- Step 4.2: `cargo test --workspace` + clippy `-D warnings` clean.
- Acceptance: zero new fixture failures vs. baseline, plus the 280+3
  previously-hanging fixtures no longer in the hang union.

## Out of scope / deferred

- **Temporal hangs (4):** Temporal is a partially-implemented area; the
  sweep's skip taxonomy lists it out of scope generally. Triage decision
  after Item 3 — if they are the stale `relativeTo` cases (per the
  existing skip note), mark them; if they are real engine loops, this
  plan is amended with a scoped Item 5.
- **Interpreter core-loop floor** (`docs/perf.md`, ~0.7µs bare loop): a
  perf milestone, not a hang; the plan's dense-elements work already
  removed the buildString share of it.
- **Dense-elements Phases B–D** (re-densify, sealed/frozen kinds,
  monotonic elements-kind): the array-store plan's own deferred items,
  not hang blockers.

## Risks

- The hang may live in the harness prelude (`makeCtorArg` /
  `testTypedArray.js`), not the engine — Item 2.2 must name the layer
  before any change.
- A quadratic blowup disguised as a hang needs a size-bounded repro
  before the fix can be validated (Item 2.3).
- Sweep classification wobbles with load; every pass/fail claim is a
  fixture-union comparison, never a raw count.

## Recommendation

Land Item 1 (the regexp commit) immediately — it is the 280-fixture
win, already measured. Then Item 2's bisect on the real fixtures before
touching any copyWithin code. Time-box the triage: if Step 2.3 names a
deep rabbit hole, report the evidence and amend the plan rather than
expanding scope silently.

## Resolution (measured 2026-09-01)

Full sweep on the committed tree + the Temporal fix (`all`, release, JIT by
default, 15s deadline, `--jobs 8 --batch 32`): **48,622 total, 48,464
pass, 0 fail, 0 crash, 0 hang, 158 skip** — a fully clean sweep; the only
non-runnable fixtures are the out-of-scope skips.

- **All RegExp, copyWithin, decodeURI, and TypedArray hangs are resolved**
  by the dense-elements/typed-array/regexp work, and the deferred Temporal
  cluster by the closed-form date-difference fix below. The 287-hang union
  (measured 2026-08-31) is now zero.
- **The Temporal cluster fix:** `iso::calendar_date_until` computed a pure
  day difference (largest unit Day) by iterating once per day — ~100M
  iterations for edge-of-range dates (~12s, crossing the sweep's 15s
  deadline; the four `argument-string-limits` fixtures). The all-zero
  years/months/weeks case now uses the closed-form epoch-day difference
  (`iso_date_to_epoch_days`), making the fixtures ~9ms. The remaining
  unit loops (Year/Month/Week) keep their bounded day iteration.
- **The one failure on the committed tree is fixed** (dense `array_set_length`
  skipped the non-configurable-length validation; see the regression test
  in `runtime/src/builtins/object.rs`).

Item 4's acceptance gate is fully met: zero failures and zero hangs vs the
baseline's 0-fail / 458-hang (README) numbers.
