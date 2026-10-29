---
name: slag-performance
description: "Use when measuring, improving, or reviewing Slag's performance: benchmarking, profiling, corpus rows, the V8 gap, the JIT tiers, a claimed speedup or regression, a performance note or commit. Covers the same-binary A/B protocol and noise floors, the diagnostic switches (SLAG_OPT, SLAG_TYPER, SLAG_GUARD, SLAG_INLINE, JIT_DUMP_*, SLAG_JIT_REPORT, JIT_HELPER_STATS), the build cache, the required gates, the measured tier map and per-operation costs, the NULL register, and the index of the `.notes` performance archive. Implementation traps live in the slag-jit skill; sweeps in slag-conformance."
---

# Slag performance

This is the single entry point for performance work: how to measure, what the
current gap is, what has already been ruled out, and where the evidence lives.
It is deliberately not a history. The dated journal is `.notes/perf.md`; the
design notes are indexed in section 8.

Sibling skills: `slag-jit` (JIT implementation traps, the helper mirror, the
opt IR, the leaf lane), `slag-conformance` (sweeps and fixture triage),
`slag-binary-freshness` (is this binary the source?), `slag-gc-rooting` (GC
values held in Rust locals).

## 1. The rule that matters most

**Counts rank structure. Only time ranks cost.**

A profile or histogram can tell you which functions are entered, how often,
and in what shape. It cannot tell you whether removing that work changes wall
time. `sum(blocks)` cannot separate "many cheap calls" from "one deep loop",
and even a correct blocks-per-call figure does not price the removal.

Every claim of the form "X costs N ns" needs one of:

- a **same-binary switch** (`SLAG_OPT=0`, `SLAG_INLINE=0`, an env-gated
  feature) measured with the corpus protocol below, or
- an **in-place phase timer** accumulated over many iterations and printed
  once (the shape that correctly decomposed the generator `next()` at 155 ns).

Both reverted changes of 2026-10-26 (the atom memo and the `!=`/`!==` lift)
were count-driven and measured the opposite of the prediction. The note
`.notes/opt-per-op-overhead.md` reached the same conclusion earlier: the opt
tier loses on specific rows, and only isolated per-row runs show it.

## 2. Measurement protocol

### 2.1 Build first

`./tools/build.sh` builds the release binaries into `target/release` through a
content-addressed cache. Each cache entry's `meta.txt` records `head`, `state`
and `tree`. A release build is 3 to 4 minutes, so batch candidates.

Never measure a binary you cannot tie to the source you are reading. See
`slag-binary-freshness`. Delete the cache directories of aborted experiments.

### 2.2 Two questions, two instruments

- **"Is X faster?"** Run A/B with the same workload and compare per-row
  minima. Prefer one binary with an env seam over two builds, because layout
  differs between builds and layout is performance here.
- **"Where does the time go?"** Ablate one component at a time with an
  existing switch, or add a gated phase timer. Do not answer this from a
  stored table or a count.

### 2.3 Same-binary seams

| switch | what it disables / reports |
| --- | --- |
| `SLAG_OPT=0` | the optimizing tier; the body runs the per-step (baseline) tier |
| `SLAG_TYPER=0` | the typer's read guard (`Op::GuardType` on a read) |
| `SLAG_GUARD=0` | the guard-pruning pass (`opt/pass/guard.rs`) |
| `SLAG_INLINE=0` | the trial-inline splice (on by default) |
| `SLAG_INLINE_CREATED=0` | only the static route (a callee the body itself creates) |
| `SLAG_RETYPE_INLINED=0` | the `normalize`/`narrow`/`mem2reg` re-run after `inline` |
| `SLAG_SCALAR=0` | the scalar-replacement pass (`opt/pass/scalar.rs`) |
| `SLAG_SCALAR_LOOP=0` | only the loop-carried field promotion (`promote_loop_carried_field`) |
| `SLAG_INLINE_FEEDBACK=1` | additionally splices feedback-recorded callees |
| `SLAG_INLINE_BUDGET=N` | inline code-growth budget |
| `--jitless` | no compiled code; the interpreter only |
| `JIT_DUMP_IR=1` | the optimizing tier's SSA, one `--- IR ---` per lifted body |
| `JIT_DUMP_CLIF=1` | Cranelift CLIF plus disassembly, and the bail reasons |
| `JIT_DUMP_STEPS=1` | the blocking step names on a lift refusal |
| `SLAG_JIT_REPORT=1` | refused bodies by reason and tier, plus compiled/deopt totals |
| `JIT_HELPER_STATS=1` | per-helper call histogram with names (instrumented build) |
| `JIT_LEAF_TRACE=1` | the first leaf-probe rejection reasons |
| `SLAG_JIT_DEOPT_PROBE=<step>` | forces a deopt at that step in every eligible body |
| `SLAG_INLINE_TRACE=1` | per compiled body: the splice-site count, each recorded site's state/hits/first id, and each resolver lookup's outcome |

Instrumentation changes the code under test (the helper counter costs four
instructions per call site), so never measure wall time with it enabled.
Redirect stdout and stderr to **separate files** when capturing a dump:
`JIT_DUMP_*` (stderr) and the `bench` lines (stdout) splice mid-line if they
share one file, which has produced wrong readings.

### 2.4 Corpus protocol (`tools/corpus`)

The runner prints `bench <mode> <row> <ms> <result>` and checks parity across
slag-jit, slag-jitless, node-jit and node-jitless.

- **One process per row** for per-row ratios. A whole-directory run shares an
  allocation context; the corpus README records `json_stringify` at 127 ms
  alone and 179 ms in a directory. The bare `baseline` row is stable.
- **Alternate A/B and B/A** across rounds. Always running A first correlates
  with machine warm-up and has produced a fake 13-20% "win" on identical
  binaries.
- **7 interleaved rounds**, per-row **minima**, compare within a round.
- **Noise floor**: about 0.3% on the corpus total, up to about 2.8% on a row,
  and about 4% on a 40 ms row. Anything under those is layout until a
  same-binary switch proves otherwise.
- **Keep two controls** in every run: `opcost/baseline` (a bare integer loop,
  flat at 0.083 ms across every seam so far) and one row the change cannot touch.
  `objects/warm_store` used to be the second and no longer is: two changes moved
  it 34.7 -> 9.3 -> 3.1 ms, so pick the second control per change and prove it
  with the IR diff below. If a control moves, the pair is not a measurement.
- **A before/after build pair cannot attribute a per-row move**: layout alone
  moved `calls/direct_leaf` +6.7% and `template-literal/evaluation-order` +5.9%
  between two builds whose IR for those rows is byte-identical. Diff the whole
  corpus's `JIT_DUMP_IR` between the two binaries (normalising the interned
  string addresses, `0x[0-9a-f]+`) and treat only a row whose IR differs as
  attributable.
- **V8 is not a fair per-operation reference on the `opcost` rows**: its JIT
  folds the affine loops and scalar-evolves the constants, so its time is near
  zero. Read Slag's absolute ns per operation; use the ratio only for ranking.

A one-row A/B, per-process:

```sh
mkdir -p scratch/one
cp tools/corpus/workloads/<family>/<row>.js scratch/one/
for r in 1 2 3 4 5 6 7; do
  ./target/release/slag.exe --corpus scratch/one
  SLAG_OPT=0 ./target/release/slag.exe --corpus scratch/one
done
```

### 2.5 The micro-suite

`./target/release/slag.exe --jit-bench` runs the same bodies through the
optimizing tier and the interpreter in one binary, with low variance. Use it to
price a mechanism. Read the raw **jit** milliseconds, not the ratio: the
interpreter column is load-sensitive and has moved 20% on an unchanged build.
Use the corpus to price the product.

## 3. Gates (definition of done)

In this order, every time:

1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace`
4. `./tools/build.sh`
5. The four sweeps **separately, never concurrently**:
   `./target/release/sweep.exe <area> --timeout 15 --recheck-timeout 15`
   for `language` 23,726, `built-ins` 23,821, `annexB` 1,086 and `intl402`
   3,365: 51,998 fixtures at 100%, 0 fail / 0 crash / 0 hang.
6. `node tools/corpus/bench.js`: `mismatches=0`.
7. A `.notes/perf.md` entry with the measurement, the method and the result,
   including NULLs.

Never raise the 15 s sweep timeout. A hang past it is a result.

**A performance change that breaks a single fixture is a revert, not a
trade-off.** Report the failing fixture and revert; do not commit around it.

## 4. Current state (measured 2026-10-26)

### 4.1 Headline

77 corpus rows, all four modes agree: **JIT gap about 10x, interpreter gap
about 3.8x** (2026-10-28; was 10.12 / 3.69 at `7685ef95`). Do not quote the
second decimal of a single run: six runs of the two binaries around the last
change read 9.00 / 9.73 / 10.00 / 10.04 / 10.27 / 10.53, and the family means
swing with them (`calls` 8.67x to 10.88x) because V8's side of each ratio is a
single unbounded run. A row-level change is the only resolvable unit, and only
when it is backed by a same-binary seam or an IR diff. The interpreter number is
the important one: Slag's interpreter is within about 3.8x of V8's, while its
compiled code is 10x off. V8's tier-on-tier gain is therefore about 2.6x larger
than Slag's. The deficit is in what the optimizing tier does with a body, not in
the cost of a single runtime operation.

Per family (README, 2026-10-28): globals 2.33x, objects 4.18x, language 6.95x,
builtins 8.17x, calls 8.67x, arrays 10.06x, strings 10.64x, control 11.79x,
opcost 12.36x. The `objects` family fell 9.29x -> 4.18x on four rows: the
scalar-replacement work (`warm_store` 34.7 -> 3.1 ms, `compound_assign` 30.4 ->
3.1, `destructure` 3.6 -> 1.0) and the loop-carried-field typing.

### 4.2 Per-operation cost (ns per operation, one process per row, minima)

| operation | Slag | V8 | note |
| --- | ---: | ---: | --- |
| `baseline` (bare integer loop) | 0.85 | 0.23 | the loop floor |
| `element_write` | 4.3 | 0.40 | |
| `obj_prop` (property read) | 6.6 | 0.23 | |
| `element_read` | 7.0 | 0.44 | |
| `js_call` | 8.6 | 0.27 | |
| `method_call` | 12.8 | 0.25 | |
| `prim_prop` | 17.9 | 0.57 | property read on a primitive |
| `map_get` | 34.9 | 3.37 | |
| `set_has` | 35.9 | 3.06 | |

Loop shapes, same method: `s = s + 1` 0.62 (V8 0.33), `s = (s + 1) | 0` 0.51
(0.24), `s = s + i` 0.81 (0.62), `k = i & 1023; s = (s + k) | 0` 0.84 (0.24).
An empty `for (;;) {}` costs 2.75 ns and is a tier-selection outlier, not the
loop floor.

Reading: each heap access costs about 5 to 6 ns in the tier the row actually
runs. V8's near-zero numbers are hoisting, not a cheaper guard.

### 4.3 The tier map (`SLAG_OPT=0` vs default)

Same binary, 5 interleaved corpus rounds, per-row minima: total
3480.1 -> 3257.9 ms, so the optimizing tier is a net **-6.4%**. But roughly a
third of the rows are 5% or more faster on the **baseline** tier:

| row | optimizing | baseline | |
| --- | ---: | ---: | ---: |
| `strings/coercion_concat` | 105.24 | 87.27 | +21% |
| `objects/proto_read` | 71.08 | 62.01 | +15% |
| `opcost/array_for_each` | 40.25 | 35.48 | +13% |
| `control/generator_loop` | 47.80 | 42.64 | +12% |
| `opcost/json_stringify` | 137.06 | 124.99 | +10% |

Its total depends on two rows: `objects/destructure` 4.64 vs 171.17 (-97%) and
`arrays/typed_array` 18.83 vs 29.88 (-37%). The tier is chosen statically (the
optimizing tier whenever the lift accepts), so the losing rows pay the wrong
choice. `.notes/opt-per-op-overhead.md` (2026-10-10) was the first isolated-row
version of this map; today's run reproduces and extends it.

### 4.4 Why the optimizing tier loses on scalar loops

A fused loop on the baseline tier keeps the counter and the accumulator in
f64 registers. The optimizing tier keeps a loop-carried binding in a frame
slot and lowers `Add` on untyped `Value`s generically. Measured on
`objects/own_read`, same binary, disassembly:

| per iteration | baseline tier | optimizing tier |
| --- | ---: | ---: |
| instructions | 7 | **32** |
| accumulator | f64 register | frame load + frame store |
| operand typing | proven Number | two tag checks, NaN canonicalisation, `select` |
| loop test | `vucomisd` + branch | tagged Bool materialised, then compared to `false` |
| fallback | none | `call_indirect` to the generic binary helper |

That is 0.8 ns/iteration versus 2.7. The engine already emits a LICM hoist
(`Step::HoistMemberGuard`/`HoistGlobalGuard`, with a `numbers` flag on the
variant that proves Numbers), but the optimizing tier refuses it and ignores
the proof. The proof is the priced next slice; see section 7.

### 4.5 Landed wins (do not re-derive)

| change | measured |
| --- | --- |
| Leaf-call lane and certified-callee lane | `calls/method_call` 132.7 -> 58.7 ms; non-leaf call about 2.7x |
| Closure record 408 -> 200 bytes (per-site source, `ClassParts`, shared capture chains) | 300k-closure probe 106.65 -> 75.8 ms; for-in head-let row about -11% across the stages |
| Generator `next()` driver | 155 ns per `next()` (about 55 resume, 70 driver, 49 result object) |
| Dense array store (`buildString` shape) | 600-700 ms -> 42-52 ms JIT |
| `Object.assign`/spread own-data copy: read the descriptor value, and enumerate a vector-free ordinary source from its map (`own_string_data_keys`) instead of `materialize_properties` + three `Vec`s | `objects/spread_assign` 117.3 -> 90.1 ms (-23%), `Object.assign` 101 -> 78 ms; corpus mean-jitGap 10.51 -> 10.26 |
| `Object.keys` ordinary enumeration: `own_string_props` (no per-key lookup, sound because `keys` never calls `Get`) + an `ordinary_own_property_keys` one-vector fast path for the no-index/no-symbol shape; `Object.values`/`entries` read the descriptor value instead of a second `Get`; `Object.keys` boxes during enumeration (no second pass) | `opcost/object_keys` 43.8 -> 31.0 ms (-29%), `builtins/object_keys` 58.7 -> 42.3 ms (-28%), `opcost/json_stringify` ~-5% |
| Scalar replacement extends to a non-escaping object's mutable field (cross-block slot alias + store-to-load), and the pipeline re-runs `narrow`/`mem2reg` after `scalar` so a guard-poisoned accumulator types to a raw f64 add | `objects/warm_store` 34.7 -> 9.3 ms (-73%), `objects/compound_assign` 43.9 -> 28.1 (-36%), `objects/destructure` 3.6 -> 2.6 (-28%); corpus -1.85%, mean-jitGap 10.26 -> 10.12 |
| `promote_loop_carried_field`: a field read before its store in the loop header becomes a header phi (literal on the entry edge, stored value on the back-edge) and the object is elided | `objects/compound_assign` 30.4 -> 12.1 ms (-60%), total separation over 7 interleaved rounds and over the `SLAG_SCALAR_LOOP=0` seam (-61%); the `objects` family 6.26 -> 5.3x, headline unchanged inside its spread |
| The carried field is typed `Number` by induction (numeric entry value, numeric-preserving arithmetic on the back-edge), and `narrow` seeds its lattice from a block parameter's declared type | `objects/compound_assign` 12.1 -> 3.0 ms (-75%), `objects/warm_store` 9.6 -> 3.1 (-68%), `objects/destructure` 2.8 -> 1.0 (-63%); corpus flat (+0.07%: 3 rows of 77), `objects` family 5.27 -> 4.18x, headline 10.12 -> 10.00 |
| The splice's static route: a call whose callee operand is a `NewClosure` (directly, or through a frame slot stored once in the entry block) splices with no comparison and no feedback, plus the `normalize`/`narrow`/`mem2reg` re-run after `inline` | `opcost/js_call` 0.88 -> 0.53 ms (-40%), `opcost/array_for_each_js` -9%; corpus flat (-0.16%: 2 rows of 77), IR diff confirms exactly those 2 rows |
| Global read cells (declarative record, nested bodies) | top-level `const` 52.5 -> about 2.4 ms per 1M; nested helper 130.2 -> 2.5 ms |
| Map key-index mixer (Goats) | `mapget` -29%, `maphas` -37% |
| Optimizing-tier GC safe point | a lifted allocating loop had no safepoint and a 46 ms pause; fixed |
| wasm stack bound (GC root scan) | the scan no longer covers the linear-memory heap; the 560-780 ms pauses and the `live` climb are gone |

PGO is measured at **-19.7%** on the corpus (-11.6% on held-out rows, A/A
control +0.33%) and is **not** the default: it moves layout, so it cannot be
mixed into another A/B, and the profile is a build input. `tools/pgo.sh` is
the reproduction.

### 4.6 The inlining seam

`SLAG_INLINE=0` is the same-binary A/B for the trial-inline splice. It showed
the splice is **not** the missing factor: the largest effect is
`calls/closure_capture` at -8.0% when inlining is **off** (40.93 -> 37.65 ms),
everything else is within about 2.5%. `SLAG_INLINE_FEEDBACK=1` helps nothing
in the corpus.

## 5. The NULL register (measured; do not redo)

| attempt | result |
| --- | --- |
| Atom memo direct-mapped table | corpus -0.96% (re-run -1.12%), served rows flat, unrelated rows swung; reverted |
| Lift `!=`/`!==` into the optimizing tier | corpus +0.85%; the two rows that moved tier +4.5% and +3.4%; reverted |
| Model the LICM hoist as "always miss" | `own_read` +571%, the `globals` rows about +60%; refused by design |
| Lift the LICM hoist faithfully (`SLAG_OPT_HOIST`) | `own_read` +235%, `globals` +35 to +39%, `proto_read` +5%; reverted (32 vs 7 instructions) |
| PGO as the default build | rejected by decision; keep it a build mode |
| Deferred `length`/`name`/`caller`/`arguments` | 300k closures 97-100 -> 108-113 ms; with a call 138-141 -> 177-178 ms |
| `HashMap<u64, Box<EcmaFunction>>` | 52 -> 128 ns per insert; the allocation costs more than the copy saved |
| `Vec` slab indexed by function id | about 77 MB at 300k ids |
| Function record boxed into the GC arena (Stage D, twice) | for-in head-let +70%, then +80.6%; the map roots and promotes every record; reverted |
| Hoist `MemberCellLoad` out of the loop (T4) | `read_loop` 0.905 -> 1.701x; register pressure, not tag checks; refused by LICM |
| Refine the FrameStore guard exception (T5b) | negative; the stored value is read once after the loop |
| Inline prototype-chain probe | 5-10% slower than the helper; reverted |
| Inline `LeafCallFill` | null; reverted |
| Re-type after inlining (`scalarized || inlined`) | +0.71% when measured (2026-10-27); SUPERSEDED — inlining fired on almost no body then, so it measured noise. Landed 2026-10-28 with the static splice, where it is -17% on the row it reaches |
| Rank a function by `sum(blocks)` | method error, not a change; divide by the entry block first |

## 6. How to choose the next target

1. Start from a **measured** gap: a family in the corpus table, a row in the
   tier map, or a phase decomposition of one row. Not a count table.
2. Confirm the row **alone**, one process, with its control rows.
3. State the prediction and the falsifier before writing code. Example: "the
   guarded loop drops from 32 to about 10 instructions and from 2.7 to
   0.9 ns; falsified if `own_read` and the `globals` rows do not reach the
   baseline tier".
4. Price the mechanism on the micro-suite first, then the product on the
   corpus.
5. Land only with the gates and a journal entry. A NULL is a valid outcome and
   must be recorded with its numbers, because it is what stops the next person
   from repeating it.

Corollary: "refused by the optimizing tier" does not mean slow. Three bodies
that left the refusal census were 3 to 5% **slower** compiled. Tier choice
needs its own per-row measurement.

## 7. Open leads (2026-10-28)

- **The inline splice cannot fire on a loop-bodied caller, and that is the
  headline's biggest single ceiling.** `JitEngine::consult` compiles a loop body
  on its FIRST consult (see the 2026-10-28 PROBE), so `record_call_site` never
  runs and `sites_from_feedback` is empty for exactly those bodies. The static
  route (a callee the body itself creates) now lands; the *member-call* shape
  (`obj.m(k)` with `obj` a literal the body built) is still unspliced, because
  forwarding its callee needs the receiver's escape analysis relaxed and that
  needs a proof about the callee's body. That is the next slice.
- **The spliced region's block split.** A splice at a call inside a loop leaves
  the callee in its own block pair and canonicalizes the raw-f64 result per
  iteration (`Canon`), which is most of the remaining 5.3 ns on `js_call`.
- **Type a function's parameters at the call site.** The row that is left keeps a
  tag check and a `Canon` per iteration only because the loop bound is a
  parameter typed `Unknown`; the lattice now honours a declared numeric type, so
  the missing half is proving one at entry. This is the widest remaining lever:
  every loop with a numeric bound pays it, and no row can currently be read as
  "the body is slow" without that term.
- **Type a loop-carried field whose arithmetic reaches outside the loop.** The
  promotion types the field when its arithmetic operand is a constant or a
  literally-typed value; an operand that is itself a loop param of undeclared
  type leaves it `Unknown`. Falls out of the first lead.
- **Consume the `numbers=true` hoist proof.** The step already establishes the
  invariant operand's type at loop entry; the optimizing tier ignores it. Type
  the hoisted-slot `FrameLoad`, let the slot fixpoint type the accumulator.
  Falsifier in section 6.3. This is the prerequisite for re-testing the hoist.
- **Tier selection.** A third of the corpus rows are faster on the baseline
  tier, but there is no predictor. Measure the losing rows individually
  (`opt-per-op-overhead.md` has the method) before proposing a rule.
- **The loop floor and the empty-loop outlier.** An empty `for` costs 2.75 ns
  against the 0.85 ns floor of a loop with work, which suggests a tier or
  refusal artifact worth understanding.
- **`generator_loop` and `array_for_each`** lose to the baseline tier by 12 to
  13%; determine whether they share the scalar-loop cause or have their own.
- **The escape-analysis frontier.** `opt/pass/scalar.rs` already replaces a
  non-escaping literal's constant-field reads (worth -5.8% on the corpus, all via
  `destructure`). The next slice, scoped in `.notes/escape-scope.md` with its
  probe, extends it to a non-escaping object's *mutable* field: cross-block
  slot forwarding plus store-to-load forwarding, target `objects/warm_store`
  (34 ms, 15x). `construct_churn` and `array_slice` need inline first, not this.

## 8. The archive

The journal is append-only and is the evidence: `.notes/perf.md`. Start at the
tail for the current session; the heading list is the fastest index.

| file | status | what it answers |
| --- | --- | --- |
| `perf.md` | live journal | every landed change, probe, A/B and NULL, dated |
| `perf-plan.md` | plan, mostly closed | the post-security-audit recovery; P1 items closed as NULLs |
| `perf-program.md` | historical | the older 3-5x program; the measurement-artifact lesson |
| `perf-findings.md` | historical | early consolidated findings |
| `bottleneck-method.md` | partly implemented | counters, the refusal census, the sweep, PGO, the sampler fallback |
| `frame-cost-profile.md` | historical, still cited | the Goats frame profile; section 4b is the global/captured-const cost |
| `opt-per-op-overhead.md` | open | the isolated-row tier map (2026-10-10), the method for row-by-row tier measurement |
| `tier-typer.md` | landed, with negatives | the typer and read guard T1-T5b; T4 and T5b are negative results |
| `tier-guarded-read.md` | negative | the measurement that decided the guard placement |
| `opt-int-representation.md` | landed | `Int` as i32 in the optimizing tier |
| `opt-arc-regressions.md` | historical | the int-representation arc's regressions; `nested_loops` |
| `corpus-regression.md` | fixed | the optimizing tier ran loops with no GC safe point |
| `optimizing-tier-plan.md` | plan of record | why a TurboFan-shaped tier, what the engines teach, the stage order |
| `optimizing-tier-impl.md` | engineering log | where the tier's code lives and how each increment landed |
| `jit-quality-plan.md` | superseded direction, live census | the helper-share census and traps G1-G21 |
| `jit-report.md` | historical | the per-row JIT report that sized the old work |
| `jit-compile-threshold.md` | landed | the compile threshold and the four consult sites |
| `non-leaf-jit.md` | historical | the non-leaf coverage plan and its census |
| `structural-gap.md` | open conclusion | the gap is the object model, not codegen |
| `escape-scope.md` | design + probe, not started | the next scalar-replacement slice: a non-escaping object's mutable field (`warm_store`) |
| `closure-cost-plan.md` | landed, Stage D rejected | 408 -> 200 bytes and the measured Stage D regressions |
| `array-store-plan.md` | landed | the dense element store that fixed `buildString` |
| `dense-store-redesign.md` | Stage 1 landed | elements kinds and the prototype-elements protector |
| `v8-shaped-arrays.md` | scoped, not started | Smi integers and elements kinds as a project |
| `global-read-cells.md` | half landed | the declarative global cell; the nested-body half is open |
| `nursery-gc-plan.md` | live spec | the generational collector, A0-A7 and what did not pay |
| `gc-plan.md` | superseded by the nursery plan | the original mark-sweep, GC-0 to GC-5 |
| `host-object-gc.md` | slices 1-4 landed | traced host state, finalizers, weak handles |
| `call-frame-plan.md` | design record | the frame, ABI and call-lane design |
| `build-cache.md` | tooling | the content-addressed release cache |
| `security-audit.md` | live | the hardening findings and their measured cost |

Smaller JIT notes (`opt-lift-unreachable-block.md`, `pass-pipeline-fixpoint.md`,
`opt-tier-shape-read.md`, `leaf-lane-deopt.md`, `tier-resume-fidelity.md`,
`typer-guard-miscompile.md`) are point records for their named mechanisms and
are referenced from the code.

## 9. One-paragraph summary

Slag's interpreter is within 3.8x of V8's, but its compiled code is 10.5x off,
and the optimizing tier is not uniformly better than the baseline tier. A heap
access costs about 5 to 6 ns in both tiers; V8 removes the access instead. The
optimizing tier's scalar loop costs 32 instructions against the baseline tier's
7 because it keeps a loop-carried binding in a frame slot and does not consume
the type proofs the front end already emits. The next change is to consume
those proofs, measured per row, with a stated falsifier. Everything else in
this skill exists to stop the next change from being a guess.
