# Slag RegExp engine — performance report & implementation plan

Status: optimization rounds 1–5 implemented and measured (2026-08-31),
plus the runtime-dispatch + sweep-runner round (§5D) that cleared the
residual `\S`/`\p{...}` fixture hangs (RegExp cluster 1879/1879 at
`--batch 4`); the only plan item left is R5.3 (step budget, last
resort). The engine lives in `crates/regexp`; the runtime integration
point is `crates/runtime/src/builtins/regexp.rs`.

## 1. The engine today

`crates/regexp` is a **pure backtracking matcher over the compiled AST**
(`Node`, produced by `parse.rs`), with no optimization layer of any kind
before this round of work:

- `engine::match_node` is a recursive continuation-passing matcher: every
  node hands a `cont: &mut dyn FnMut(&mut Caps, usize) -> MatchResult` to its
  children, so every AST transition is an indirect call through a closure
  capture.
- Choices are tried in spec order (leftmost alternative first, greedy
  quantifiers consume maximally, lookarounds commit to first success).
- Captures use an undo-log (`Caps.trail`): every capture write pushes
  `(index, old_value)`; backtracking rolls back to a mark.
- A single fast path existed for **greedy repeats of a capture-free single
  atom** (`Char`/`Any`/`Class`), consuming iteratively to avoid one stack
  frame per consumed character.
- The runtime (`regexp_builtin_exec`) drove its own search loop with
  `exec_at`, calling the engine once per input position — each call
  allocating a fresh capture buffer.

The hot-path costs measured before this round (release build):

- per search position: one `exec_at` call = one `Caps` allocation
  (`vec![None; groups+1]`) + closure dispatch;
- per consumed character in a repeat: one heap-allocated `Vec` endpoint
  list (`atom_match_ends`);
- capture repeats (`(a)+`) fell off the fast path entirely and recursed one
  stack frame per character — an O(n) recursion depth that **overflowed the
  stack on multi-megabyte inputs**;
- no literal/prefilter guidance for the search loop: `/abc/` scanned every
  position with a full matcher attempt.

## 2. Optimization round 1 (implemented)

Four changes, all in `crates/regexp` except the runtime wiring.

### 2.1 Leading-char search prefilter

`engine.rs` gains a compile-time analysis (`first_char_analysis` /
`first_char_analysis_seq`) computing, for the whole program, whether it can
match empty and the set of code points its first consumed character can be:

- peels `Sequence` (skipping zero-width terms: `Empty`, `Start`, `End`,
  word boundaries, lookarounds), `Capture`, and `Repeat` with `min ≥ 1`;
- unions alternatives of `Alternate`;
- `Repeat` with `min == 0` matches empty → no prefilter;
- refuses (`None`) anything unanalyzable: `i`-folded literals, predicates,
  negated/string classes, backrefs, and any pattern that can match empty
  (an empty match can start anywhere, so no position is skippable).

The result is mapped to a **leading-UTF-16-unit range set**
(`search_prefilter(program, unicode)`): BMP code points are identity,
non-BMP code points become their high surrogate range, and a legacy-mode
range beyond the BMP refuses the prefilter (the mapping would be
incomplete). The set is computed **once at compile time** and stored on the
`Regex` struct (`Regex::prefilter`, built in `parse.rs::compile_pattern`),
so `search` reads it without per-call work — the first version computed it
on every `search_at` call and cost ~90 ns/call on an ~47 ns baseline.

The new `Regex::search_at` (`lib.rs`) is a leftmost search that skips
positions whose first unit is not in the set (false positives are fine —
the matcher re-verifies; false negatives would be bugs) and **reuses one
`Caps` buffer across position attempts**, resetting with `rollback(0)`
after each failure (the trail entries chain back to the untouched state).
`Regex::exec` is reimplemented on top of `search_at`; `exec_at` is
unchanged.

### 2.2 Allocation-free repeat fast path

`atom_match_ends` now fills an `AtomEnds` (`[usize; 8]` inline + heap spill
for `/v` classes with more matching `\q` strings) instead of returning a
`Vec`, and the fast path's per-position `consumed: Vec<(usize, Vec<usize>)>`
becomes a flat `consumed: Vec<usize>` (atom start positions) plus a flat
`pending: Vec<(usize, usize)>` of untried alternative endpoints. No heap
allocation per consumed character in the common case (Char/Any/plain
class); the alternative-endpoint order among the "rest" is irrelevant to
the result, so LIFO popping is fine.

### 2.3 Capture-wrapped single-atom repeats

`single_atom` peels captures and one-element sequences down to a single
`Char`/`Any`/`Class`, so `(a)+`, `(?:a)+`, and `((a))+` take the iterative
fast path. This required two fixes:

- **The peel is mandatory**: a group's disjunction always wraps its body in
  `Node::Sequence` (`parse_disjunction`), so `(a)+` parses as
  `Repeat { Capture { Sequence([Char]) } }` — the first version missed it
  and silently kept the recursive path.
- Capture semantics: the repeat's owned captures are cleared once at entry
  (`clear_owned`), and `set_repeat_captures` derives the **last iteration's
  span** from the consumed stack before each continuation attempt (spec
  RepeatMatcher), writing it to the trail so outer backtracking restores the
  pre-repeat value. Zero iterations leave the captures cleared. No
  per-iteration trail churn.

This also fixed a latent **stack overflow**: `(a)+` on a 1M-unit input
previously recursed ~2M frames and crashed; it now consumes iteratively.

### 2.4 Runtime wiring

`regexp_builtin_exec` (`crates/runtime/src/builtins/regexp.rs`):

- **sticky** keeps exact-position semantics: one `exec_at` attempt, reset
  `lastIndex` to 0 on failure;
- **non-sticky** (global or plain) uses `search_at`, which returns the
  match start index so `lastIndex`/`.index` keep the same values; on
  exhaustion it resets `lastIndex` to 0 only when `global` (matching the
  old loop's `global || sticky` exit).

### 2.5 Benchmark harness

`crates/regexp/tests/perf.rs` — an ignored test (criterion would need a
network fetch):

```
cargo test -p regexp --release --test perf -- --ignored --nocapture
```

Covers search-loop cost, repeat fast paths, captures, backrefs, unicode
predicates, pathological backtracking, and compile cost, with warmup and
assertions so a behavior change fails the bench instead of skewing it.

## 3. Measured results

Release build, before/after the round-1 changes. The bench harness calls
`search_at` (what the runtime now uses); the pre-session numbers used the
old `exec_at`-loop.

| case | before | after | speedup |
|---|---|---|---|
| `/abc/` no match, 100k input | 3.19 ms | **68 µs** | 47× |
| `/abc/` match at end, 100k | 3.19 ms | **68 µs** | 47× |
| alternation no match, 100k | 12.3 ms | **119 µs** | 103× |
| `/(a+)/` on 10k | 311 µs | **98 µs** | 3.2× |
| `/a+/` on 200k | 8.2 ms | **2.4 ms** | 3.4× |
| `/a*$/` on 100k | 4.0 ms | **1.1 ms** | 3.6× |
| `/[a-zA-Z_0-9]+/` on 50k | 2.5 ms | **0.68 ms** | 3.7× |
| `/\p{L}+/u` on 20k | 1.44 ms | **0.57 ms** | 2.5× |
| `/abcdefghij/i` match at end, 50k | 1.81 ms | **0.59 ms** | 3× |
| `/(a+)b\1/` on 10k | 2.09 s | **1.02 s** | 2.1× |
| empty-match `/a*/` every position, 200k | 8.9 ms | 10.1 ms | 0.88× |

Interpretation:

- The prefilter turns *search* from O(n) full matcher attempts into O(n)
  unit-range tests + attempts only at candidate positions — the reason the
  literal and alternation cases drop ~50–100×.
- The repeat cases improve 3–4× from removing the per-character
  allocation and, for `(a+)`, the per-character recursion.
- `/(a+)b\1/` stays quadratic (each search position re-consumes the `a`-run
  against a uniform input); only the constant improved. See R2/R5.
- The empty-match case is ~13% slower than the old loop: the engine's
  `search_at` per-call machinery (loop setup, `Ctx`) over a baseline that is
  already just one allocation + one match attempt. This is the price of the
  unified search path and is dwarfed by the runtime's per-exec `RegExpState`
  clone (see R1). Patterns the prefilter *can* skip are orders of magnitude
  better.

## 4. Validation

- `cargo test -p regexp`: 25 tests (4 new: capture-repeat backtracking /
  overflow, prefilter skip-and-hit incl. `a?b` unions and `^abc` anchoring,
  unicode non-BMP prefilter).
- `cargo test -p runtime`: 642 tests; `cargo test --workspace` green.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- test262 RegExp sweep: see 5.5 (run and green in round 2).

## 5. Optimization round 2 (implemented)

Four more items from the plan below, plus the R0 sweep that validated them
(and caught two bugs — see 5.5).

### 5.1 R1 — stop cloning the compiled `Regex` per exec

`RegExpState.compiled` is now `Rc<regexp::Regex>` (`regexp.rs`): the
per-exec `regexp_data` clone is two refcount bumps instead of a full `Node`
AST copy. The invariant holds — `compiled` is never mutated in place
(`RegExp.prototype.compile` re-runs `regexp_initialize`, which inserts a
fresh state). All deref through the `Rc` unchanged.

### 5.2 R3 — predicate-class prefilter

`Predicate` is now `Hash`; `engine::predicate_ranges` caches each unique
predicate's explicit code-point ranges in a process-wide
`OnceLock<Mutex<HashMap<Predicate, Arc<Vec<(u32,u32)>>>>>` (enumerating
~1M code points once per predicate instead of once per compile; `\d`/`\w`/
`\s`/`\p{…}` all flow through it). `first_char_analysis`'s `Class` arm
consults the cache for non-folded, non-negated, string-free predicate
classes and refuses a full-space predicate (`\p{Any}`). Search over
non-digit text with `\d{6}` is now ~47× (68 µs vs 3.2 ms).

### 5.3 R4 — linear-sequence capture repeats

`single_atom` now also admits a **non-empty linear sequence** of string-free
single atoms, so `(ab)+`/`(?:ab)+` take the iterative fast path
(`atom_match_ends` walks the sequence deterministically). `(ab)+` on a 1M
unit input no longer recurses per iteration (stack overflow fixed);
measured 2.3 ms for 200k units. Branching inner atoms (`(a|b)+`) still need
R2.

### 5.4 R5.1 — repeat coalescing

`parse.rs::coalesce_repeats` merges adjacent greedy capture-free repeats of
structurally-identical atoms (`a*a*` → `a*`, `a{2}a{3}` → `a{5}`) — the
split between two identical greedy atoms is never observable, so the merge
preserves results while removing redundant backtracking states. The
pathological bench `a*a*a*a*b` collapsed to linear: 5.8 µs/iter (was
~2.6 ms). Lazy quantifiers and capture-owning atoms are untouched.

### 5.5 R0 — the test262 RegExp sweep

`built-ins/RegExp` (1879 fixtures) and `language/literals/regexp` (238):
**1506 + 238 pass, 0 fail, 0 crash**. The 373 `built-ins` "hangs" are all
`property-escapes/generated/*` fixtures whose `buildString` (constructing
~2M-unit strings via `String.fromCodePoint`) takes ~6.5 s — verified
independent of the regexp engine by running a build-only copy (no regexps)
at the same speed. The sweep caught two real regressions, both fixed with
tests:

1. **Prefilter skip advanced by one unit** instead of code-point-aware:
   under `/u`, position 1 of a surrogate pair is never visited by the spec's
   AdvanceStringIndex loop, so `\udf06` falsely matched `'\ud834\udf06'`
   at index 1. The skip now advances via `advance_string_index`
   (`search_prefilter_never_visits_surrogate_halves`).
2. **Capture-repeat spans in lookbehind were reversed**: the fast path
   recorded `(start, cur)` unnormalized; a right-to-left repeat has
   `start > cur`. `set_repeat_captures` now records `(min, max)`
   (`lookbehind_capture_repeat_spans`).

## 5A. Optimization round 3 — R2, the explicit backtracking stack

`engine.rs` is rewritten from CPS recursion (closures per node) to a single
loop over two explicit stacks, deleting `match_node`/`match_sequence`/
`repeat_loop` (622 lines):

- **`Task`** is the forward path (what to match next): node, sequence
  element, capture close, repeat loops, the assertion-complete marker, and
  the resume tasks (repeat-done, lazy-iterate, assertion-failed,
  class-string advance, fast-repeat shrink). **`Choice`** is a backtrack
  point. The loop dispatches tasks; an atomic failure pops choices.
- **Continuation snapshots**: each choice stores the task-stack content at
  its push (shared via `Rc`), because the forward path consumes continuation
  frames below a choice before it is popped — a bare stack length cannot
  reconstruct them (this was the first design's bug).
- **Zero recursion**: repeats, alternations, and lookarounds all iterate on
  the stacks, so recursion depth tracks pattern structure, never input
  length — no pattern can overflow the stack (`(a|b)+` on 1M units now
  passes; it would have overflowed before).
- **Simple-atom repeat fast path** (`greedy_simple`): a greedy repeat of a
  literal/dot/class/linear-sequence atom consumes iteratively into a
  `consumed: Vec<usize>` and arms **one** shrink choice that re-arms itself
  (`Task::FastShrink`), sharing the continuation snapshot. This restores
  the round-1 memory profile (O(1) choices per repeat) and beats the old
  numbers on repeats.
- **Class string alternatives**: a class whose `\q` members match at a
  position commits to the first and arms the rest as backtracking
  alternatives (`[\q{a|ab}]+` on "ab" still matches "ab").

### R2 perf (release, per iteration)

| case | round 1 | R2 |
|---|---|---|
| `/a+/` on 200k | 2.4 ms | **1.35 ms** |
| `/(a+)/` on 10k | 98 µs | **37 µs** |
| `/(ab)+/` on 200k | 2.3 ms | **1.2 ms** |
| `/a*$/` on 100k | 1.1 ms | **0.77 ms** |
| `/[a-zA-Z_0-9]+/` on 50k | 0.68 ms | **0.49 ms** |
| `/\p{L}+/u` on 20k | 0.57 ms | **0.55 ms** |
| `/(a+)b\1/` on 10k | 1.02 s | 2.0 s |
| empty-match `/a*/` per position, 200k | 10 ms | 16.5 ms |
| `/abcdefghij/i` search, 50k | 0.59 ms | 1.48 ms |

Repeats are ~1.8× faster than round 1 (no per-node indirect calls, no
per-iteration allocation). The remaining regressions are the per-position
search-attempt overhead on prefilter-less patterns (empty-match 1.65×,
ignore-case 2.5× — absolute cost ~30 ns/position) and the quadratic
backref path (2×, from the shrink choice re-arm per backtrack step). All
are dwarfed by the runtime's per-exec overhead in real code.

### R2 validation

32 regexp tests (new: `branching_atom_repeat_does_not_overflow`), 642
runtime tests, workspace clippy clean, and the R0 sweep re-run on the new
engine: `built-ins/RegExp` 1515 pass / 0 fail / 0 crash,
`language/literals/regexp` 238/238. The property fixtures run at the same
speed (their cost is `buildString`, engine-independent).

## 5B. Optimization round 4 — R5.2, failure memoization

`Matcher` gains a per-(atom, position) failure memo that kills the classic
exponential nested-repeat blowups (`(a+)+b`, `(?:a*)*b`). The probe measured
11–14 s at n=25 before; both are now microseconds.

### The two memo entries

- **Min-unsatisfiable** (recorded in `greedy_simple` when the atom cannot
  reach `min` consumptions from a position): the simple atom's consumption is
  deterministic, so "can't reach the minimum here" is a pure position
  property — sound unconditionally, backrefs or not.
- **Exhausted** (recorded when the `FastShrink` chain bottoms out — every
  shrink level dead): "the repeat at (atom, pos) can never succeed" is only
  sound when the continuation below the repeat is *acceptance-equivalent* at
  every re-entry, so recording is gated on:
  - **no backrefs anywhere in the pattern** — the continuation never reads
    captures, so acceptance is capture-independent; and
  - **every enclosing repeat has `min <= 1`** (`*`, `+`, `{0,m}`, `{1,m}`) —
    such a repeat can stop at any iteration count (its stop choice always
    passes the minimum), so the continuation does not depend on its own
    iteration count, which the (atom, position) key does not carry.

The second gate is the soundness fix found while implementing: a naive
ungated memo misses `(a+){3,}b` on `"aaab"` (the match stops the `{3,}`
repeat after group 3 — a continuation that a `{2,}`-or-tighter ancestor
makes count-dependent). `memo_safe_atoms` walks the AST to find the eligible
atoms; the memo persists across `match_at` calls in the search loop (the
continuation is pattern-determined, so entries stay valid across position
attempts).

### R5.2 perf (release, per iteration)

| case | before (R2) | R5.2 |
|---|---|---|
| `(a+)+b` no match, n=25 | ~12 s (probe) | **70 µs** |
| `(a+)+b` no match, n=50 | exponential | **240 µs** |
| `(a+)+b` no match, n=100 | exponential | **884 µs** |
| `(?:a*)*b` no match, n=100 | exponential | **780 µs** |
| `(a+){3,}b` no match, n=12 (memo gated off) | 1.6 ms | 1.6 ms |

Exponential becomes polynomial: the residual is O(n²) for an a-run of length
n, from the first exploration of each position re-consuming the remaining
run greedily (70 µs → 240 µs → 884 µs at n=25/50/100 matches the quadratic
shape). `{2,}`-nested catastrophes stay exponential (R5.3 territory). The
quadratic backref bench (`/(a+)b\1/` on 10k, ~2 s) is unchanged — that path
is capture-dependent and correctly never memoized.

### R5.2 validation

36 regexp tests (new: memoized-nested-repeat timing, soundness gates —
`(a+){3,}b` still matches `"aaab"` and a backref pattern still matches,
the enclosing-stop choice is still found, the memo survives search-loop
position attempts), 3323 runtime tests, workspace clippy clean, R0 sweep
re-run green. Follow-up: `backref_free` and the `memo_safe` atom set are
computed once at compile time (stored on `Regex`, like `prefilter`); the
per-`Matcher::new` AST walk had regressed the worst-case per-attempt
search (empty-match `/a*/` over 200k: 16.5 ms → 29 ms), now back to
17.7 ms.

## 5C. Optimization round 5 — case-insensitive leading-char prefilter

The search prefilter previously refused `i`-folded literals and classes
(`first_char_analysis` returned "unconstrained"), so `/abcdefghij/i` scanned
every position and the R2 per-attempt overhead made it 2.5× slower than
round 1 (1.48 ms on 50k).

`engine.rs` gains a lazy, process-wide **reverse fold index**
(`FOLD_CLASSES`, one per unicode mode): `canonical code point → the
code-point ranges folding to it`, built once by enumerating the code point
space (same pattern as the predicate cache). The leading-char set of a
folded literal is now the **fold equivalence class** of its
(pre-canonicalized) pattern char — the *preimage* of the canonical form,
which the forward-closure helpers would get wrong: `/k/iu` matches U+212A
KELVIN SIGN, so the prefilter must include it. Folded classes union their
members' classes (bounded at 4096 members to keep compile fast; bigger
classes refuse the prefilter as before).

Measured (release, per iteration):

| case | before (R2/R5.2) | round 5 |
|---|---|---|
| `/abcdefghij/i` search, 50k | 1.57 ms | **50 µs** (31×) |

Validation: 38 regexp tests (new: fold-class prefilter soundness — Kelvin
sign under `/iu`, `ß`/`ẞ`, legacy `é`/`É`, non-BMP Deseret pair;
skip-to-match for a folded literal and a folded class), workspace clippy
clean, R0 sweep re-run green.

## 5D. Runtime dispatch + sweep runner round (2026-08-31)

Profiling the `\S+` / `\p{...}` matching (the residual sweep-hang
fixtures after the R0-R5.2 engine work) found the ENGINE was already
fast — `exec_at` on `\S+` against a 1-char input measures ~125 ns —
and the per-`replace` cost was in the runtime wrappers and the sweep
runner, not the matcher:

1. **The builtin dispatch chain**: every RegExp method/accessor call
   (`exec`, `test`, the `flags`/`global`/... getters, `@@replace`) was
   dispatched by scanning `regexp::dispatch_call`'s ~20
   `Intrinsics::get` lookups, each allocating a `JsString` + SipHash
   lookup. Measured ~5 µs per call (a plain-object accessor is ~0.3 µs).
   Fixed by registering the hot RegExp members in a
   `regexp::handler_for` table (consulted by `Intrinsics::define` like
   `array::handler_for`), so a warm call dispatches O(1) by function id.
   Measured: `flags` read 49 → 5.5 µs (the remaining 5.5 µs is the
   getter's own spec-required composition of 8 flag reads), a flag
   getter 5.3 → 0.46 µs, `replace` 69.5 → 22.4 µs, and the
   `character-class-escape-non-whitespace` fixture (65,536 `\S+`
   replaces) 11.3 → 5.3 s. `built-ins/RegExp` is now **1879/1879 pass,
   0 fail, 0 crash, 0 hang** at `--batch 4 --recheck-timeout 15`.
2. **The sweep runner's verdict wobble** (`test262-sweep`): the hang
   count was inflated and load-dependent because (a) the recheck
   deadline (5 s) was inconsistent with the batch budget (15 s) — a
   fixture that fits its own 15 s batch was classified a hang whenever
   its 32-fixture batch was killed by composition; and (b) full-core
   parallelism ran every batch under contention, skewing the marginal
   timings. The defaults now use a **15 s recheck (matching the batch
   budget)** with full-core jobs, plus `--fast` (full-core jobs, 5 s
   recheck — a quick smoke whose verdicts wobble on 5-15 s fixtures)
   and `--accurate` (half-core jobs, 15 s recheck — steadier timings).
   Measured on the full 48,622-fixture sweep: old defaults 334-374
   hangs → default (full cores, recheck 15) ~240 → `--accurate` ~136,
   with 0 fail / 0 crash throughout.

## 6. Remaining opportunities — implementation plan

Completed: **R0–R5.2**, plus the round-5 i-fold prefilter (§5C) — see
§5/§5A/§5B/§5C. Remaining: R5.3 only.

### R5.3. Step budget (last resort)

V8-style interrupt/limit on backtracking steps — a spec deviation, so only
a last resort if real-world hangs remain after R2/R5.2. `{2,}`-nested
catastrophic patterns (the R5.2 gate deliberately leaves them exponential)
are the remaining hang risk.

## 7. Suggested `.rules` additions

Validated this session; proposed for the repo root rules (or
`crates/regexp/.rules`):

> In `crates/regexp`, a group body is always wrapped in `Node::Sequence` by
> `parse_disjunction`, even for a single atom — `(a)+` parses as
> `Repeat { Capture { Sequence([Char]) } }`. Any AST-peeling logic (the
> repeat fast path, leading-char analysis) must peel one-element sequences
> and captures, or it silently misses common patterns (a missed fast path
> is a perf bug; a missed prefilter member is a soundness bug).
