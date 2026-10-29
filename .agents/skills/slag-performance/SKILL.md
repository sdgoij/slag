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
| `SLAG_TYPER_LEAF=1` | the typer's read guard reaching a leaf body (off: it costs leaf inlining) |
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
| `SLAG_SAMPLE=<µs>` | a flat self-time profile: a watchdog samples the engine thread's `Rip` and prints image offsets to stderr; `tools/profile.py` symbolizes them. Use it to *choose* a target, never during a timed A/B |
| `SLAG_CLOSURE_PROFILE=1` | per-phase mean of closure instantiation (`closure <phase> <raw> <net>`): `register_function`'s phases plus the instantiation prelude and the interpreter's creation step. Cycle-counter based, inert when unset |
| `SLAG_CLOSURE_SKIP=props` | skips `set_function_properties_batched` at closure creation, so `ablate.sh` prices that component in the same binary |

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
- **A phase timer's absolute scale is not evidence on its own.** Two traps cost
  a session on 2026-10-29. (1) `std::env::var_os` costs **~140 ns** on Windows,
  so an env-gated instrument that reads its switch *per call* is itself the
  largest term it measures; cache the gate in a `OnceLock`. (2)
  `Instant::now()` costs ~30 ns here, so it cannot split a 300 ns function, and
  a bare `_rdtsc` is not ordered against surrounding loads, so its intervals
  come out compressed and non-uniformly so. Use `_mm_lfence(); _rdtsc()` and
  **calibrate with an ablation**: add a switch that skips the component
  (`SLAG_CLOSURE_SKIP=props`), price it with `ablate.sh`, and rescale the phase
  table by that ratio. In the closure case the timer under-read by **2.4x**.

### 2.6 Self-time attribution (`SLAG_SAMPLE`)

The corpus table says *which row* is expensive; the sampler says *which
function* is running. Run it on the whole corpus for the map, or on one row to
prove a slice:

```sh
SLAG_SAMPLE=300 ./target/release/slag.exe --corpus tools/corpus/workloads \
    2> scratch/prof.txt          # stdout is the bench table, keep them apart
python tools/profile.py scratch/prof.txt --group   # by crate::module
python tools/profile.py scratch/prof.txt --top 30  # by function
```

Reading it:

- **Samples outside the executable's image are compiled bodies.** A row that
  is mostly `[jit/native code]` has no runtime slice to take (the splice
  already removed it).
- **Flat profile means uniform tax, not a hidden hot spot.** If the top
  function is under ~2%, no single slice can move the corpus and the work is a
  portfolio.
- **Only time ranks cost.** The share is a share of *self* time; it does not
  say what a fix would save. Price the fix with a same-binary seam before
  believing it.

Instrumenting a running engine means trusting the attribution: a public
symbol has no size, so an address resolved directly against the PDB lands on
whichever public symbol precedes it. That artifact showed `Vm::setup_frame`
(a 4.4 KB function) as 7.3% of the corpus. `profile.py` resolves through the
PE unwind directory first and names a range by the public at its start; if you
touch it, re-validate on `opcost/baseline` (90% outside the image) and
`opcost/regexp_test` (topped by the regexp matcher).

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

**Read the total-time gap, not the mean-of-ratios headline.** Same binary, one
corpus pass, 77 rows (2026-10-29): slag jit and node jit totals are
**2085.4 ms against 727.0 ms, so the gap is 2.87x**, and each engine's
optimizer buys the same factor (slag jit/jitless **2.55x**, node **2.42x**).
The `mean-jitGap` line (9.7x) averages per-row ratios and is dominated by rows
V8 folds to nothing; it ranks nothing and overstates the gap threefold.

The reading that follows from the tiers being equal: **the deficit is the
runtime (about 3x on every primitive), not the optimizer.** Inlining, escape
analysis and better IR cannot close it, because slag's compiled code is
already as much better than its interpreter as V8's is.

Corpus self time, 63k samples (`SLAG_SAMPLE`, 2026-10-29): 37.8% compiled code,
9.0% `runtime::jit` helpers, 8.2% `runtime::builtins`, 8.1% `runtime::ir`,
6.1% `crux::object`, 5.8% `crux::heap`, 4.8% `crux::string`, 3.7%
`runtime::function`, 2.2% sorting. It is **flat**: no function exceeds 2.2%.

Per family (README, 2026-10-28): globals 2.20x, objects 3.94x, language 5.72x,
builtins 7.18x, calls 8.51x, strings 9.60x, arrays 9.83x, opcost 11.80x, control
12.19x. The `objects` family fell 9.29x -> 3.94x on four rows: the
scalar-replacement work (`warm_store` 34.7 -> 3.0 ms, `compound_assign` 30.4 ->
2.9, `destructure` 3.6 -> 1.0), the loop-carried-field typing, and the earlier
`Object.assign`/`Object.keys` slices. `opcost` moved on the two rows the call
splice reaches (`method_call` 1.19 -> 0.67 ms, `js_call` 0.88 -> 0.53).

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
| The member-call splice: a member read of a literal object's field that holds a closure splices behind `CalleeEq(operand, literal_value)`, the literal being the body's own `NewClosure` SSA value (so a per-call closure still matches, and a field written since falls back) | `opcost/method_call` 1.19 -> 0.68 ms (-43%), `opcost/js_call` 0.778 -> 0.588, corpus -0.74%; IR diff confirms exactly 3 rows |
| The canonical empty map a function kind's `[[Prototype]]` implies is cached per kind in the agent roots, and `Function::new_with_prototype_and_map` builds the object on it | closure probe 94.87 -> 91.52 ms min (-3.5%), for-in head-let 755.3 -> 746.0 ms min (-1.2%, every paired run), corpus mean-jitGap 9.46 -> 9.36 |
| The optimizing tier lifts `Term::Throw` (was its only refused terminator); `throw_control` is the lowering, so iterator close and `using` disposal still run | `opt-blockers: Throw` gone; for-in head-let jit 377.7 -> 368.8 ms (-2.4%), corpus flat (-0.35%, inside the noise floor) |
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
| `-C target-cpu=native` | two builds, 5 interleaved corpus rounds, per-row minima: 2251.1 -> 2328.9 ms (**+3.46%**, i.e. worse), with row deltas from -21% to +44% on rows the flag cannot reach. Unresolvable against the layout confound on this machine; keep the generic target |
| Ranking a slice by a before/after **build** pair | two binaries built from *identical source* with only the codegen target differing moved the corpus total 3.46% and single rows up to 44%. A slice landing under ~4% must be proven with a same-binary seam |
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
| i32 element key (keep the index's int32 proof, skip the `ToUint64Sat` gate) | sound and implemented, but inside the noise on every row it touches (`element_read`, `element_write`, `dyn_key_read`, `array_at`); the ladder's dependent-load chain dominates, not the gate |
| Typer read guard on a leaf body (`SLAG_TYPER_LEAF`) | `calls/construct_churn` 58 -> 110 ms: a guard marks the body `deopts` and the leaf-inline lane refuses it |
| `EcmaFunction` 200 -> 152 bytes (drop `name`, pass it to `compile_body`) | a-var-closure and for-in head-let both flat (7 interleaved pairs, minima 94.4 -> 93.4 ms and 741.4 -> 748.3 ms). Record size is NOT what the registry insert pays at 200 bytes: the map machinery does. Kept only as a simplification |
| `std::env::var_os` on a per-closure diagnostic path | the switch read itself was **142 ns** of a 300 ns closure; cache it in a `OnceLock` |
| Tier selection as the fix for the low-`jl_gap` rows | `jl_gap` is slag-jitless/node-jitless, not jit vs jitless. The JIT is 2.4-13.7x FASTER than the interpreter on every `calls` row, and `SLAG_OPT=0` moves `closure_capture` by 6% and `method_call` by nothing. Corpus-wide the optimizing tier is 1966.7 ms against the baseline's 2236.6 ms, so the opt tier is 13.7% better overall; an oracle could still find ~14 ms per-row but no static rule does |
| Whole-body register segmentation (reuse `lower_leaf_ops_segmented` + `apply_register_runs` when `leaf_ops` is `None`) | compiles, then the probe dies with `TypeError: Uncaught exception`: a run absorbed steps whose value-stack effect the surrounding step-path code still depends on. The accept rules are written for a loop body, where the runs are bracketed by loop machinery. Reverted; see the 2026-10-29 NULL in `.notes/perf.md` |
| Branch-capable register leaves (`LeafOp::Jump`/`JumpIfFalse`/`JumpIfTrue` over op indices + an index-loop executor) | built twice. First attempt never fired: a guard clause is bracketed by `ResetCompletion`/`NormalizeCompletion` and the branch target IS one of them. Second attempt also relaxed `lower_step`'s "a `return` must be the last step" rule (sound: `ReturnAcc` ends the run) and resolved targets to the first emitting op; still refused, and tracing every other return point shows the **branch guard** is what fires — the register binary leaves its left operand as a re-readable shadow entry, so a branch is reached with a live compile-time stack. The flat op array has no phi, so the guard is right. Fixing it changes the register op contracts. Designs and step lists are in the two 2026-10-29 NULLs |
| `loop_counter` typing the counter cycle it proved int32 | works, but the use rule it needs rejects a previously-correct fusion: `opcost/element_write` 0.375 -> 0.45 ms. `Type::Int` on a block parameter is not an i32 register (params are `i64` in CLIF), so a type cannot move a counter into an i32 register at all |
| In-place splicing (a single-block callee copied into the caller's block) | `opcost/js_call` 0.577 vs 0.510 with the block-splitting splice. The continuation block is load bearing: it is what lets `narrow`/`mem2reg` derive a parameter's type from one incoming value, and without it the accumulator and the `|0` stay `Number` words with a `Canon` per iteration |
| Rank a function by `sum(blocks)` | method error, not a change; divide by the entry block first |

## 6. How to choose the next target

-1. **Start from `SLAG_SAMPLE` self time (section 2.6).** It names the functions
   the engine actually runs and their share of the whole corpus, which is what
   the row tables cannot say. A flat map (top function under ~2%) means the
   work is a portfolio of 30-60 ms slices and no single slice can be big.

0. **Rank by absolute recoverable time, not by `mean-jitGap`.** The headline gives
   a row where V8 does 0.02 ms the same weight as one where it does 80 ms, so it
   puts 0.6 ms rows at 55x ahead of 452 ms rows at 8.6x, and three sessions of
   work went into the 0.6 ms rows. Run `python tools/corpus/where.py bench.txt
   tier.tsv` (see its header for producing the inputs) and take the top of the
   *recoverable* column. Two of the columns decide what kind of work it is:
   - `tier` (`SLAG_OPT=0` vs default, in ms): a row the optimizing tier is
     slower on is a tier-selection bug. Twenty corpus rows are, for 14.3 ms.
   - `jl` (the interpreter gap): both engines interpret the same work, so V8's
     optimizer cannot flatter it. `jl` near `gap` means shared runtime machinery
     (the fixable kind); `jl` far below it means V8 removes work we do
     (inlining, escape analysis) and the row's ceiling may not be reachable.
1. Start from a **measured** time, not a count table: the corpus table, the tier
   map, or a component cost from step 2.
2. **Put a time on each component before touching the engine.** Write the row's
   shape as ablation variants (one component removed per file), run
   `tools/corpus/ablate.sh <dir>`, and read the `jl` deltas. That is how the
   2026-10-28 work order was found: closure creation 460 ns against V8's 65
   (7x), member read/call 194 against 12 (17x), calls and frames 1.5x, i.e. fine.
   Guessing from IR shapes and count censuses produced a day of NULLs instead.
3. Confirm the row **alone**, one process, with its control rows.
4. State the prediction and the falsifier before writing code. Example: "the
   guarded loop drops from 32 to about 10 instructions and from 2.7 to
   0.9 ns; falsified if `own_read` and the `globals` rows do not reach the
   baseline tier".
5. Price the mechanism on the micro-suite first, then the product on the
   corpus.
6. Land only with the gates and a journal entry. A NULL is a valid outcome and
   must be recorded with its numbers, because it is what stops the next person
   from repeating it.

Corollary: "refused by the optimizing tier" does not mean slow. Three bodies
that left the refusal census were 3 to 5% **slower** compiled. Tier choice
needs its own per-row measurement.

## 7. Open leads (2026-10-29)

- **The property define path is the largest coherent runtime block.**
  `objects/spread_assign` is 40% property machinery (`Map::find` 8.1%,
  `set_with_receiver_key` 8.0%, `define_property_or_throw` 7.2%,
  `Map::get_or_create_child` 5.1%, `get_own_property_key` 3.8%). A define of a
  new key on a fresh object costs **45 ns** against 3.5 ns for a warm store
  (`scratch/storecost`: `{}`+`o.a=1` 91.8 ns/body against `{}` 45.4, six
  defines 333.6). The work is a linear `Map::find` over the current shape's
  descriptors plus an FxHash transition lookup keyed on
  `(PropertyKey, MapAttrs)`; both are per define and neither is warmed by a
  fresh receiver. `crux::object` + `crux::map` is 7.8% of the corpus.
- **`runtime::function` (closure creation) is 10.8% of the #1 row and 12.9% of
  the #2 row**, and 3.7% corpus-wide. The mechanism is known and priced
  (280-300 ns against node's 9); every structural attempt so far is a measured
  regression (see the NULL register). This is the largest named term on the
  two rows that are 30% of the recoverable time.
- **Strings are 4.8% and entirely in three functions** — `string::lookup`
  1.51, `JsString::from_utf8` 0.91, `string::intern` 0.70 (plus `as_slice`
  0.70, `concat` 0.63). `lookup(id)` clones a `JsString` per call, and its
  callers are property-key boxing (`Object.keys`/`values`/`entries`, JSON, the
  proxy traps). The atom memo itself is already a measured NULL; the open
  question is why these callers need a `JsString` at all on their hot path.
- **Allocation + GC is 5.8%**: `collect_minor_inner` 1.99, `Heap::alloc` 0.95,
  the `scan_regions` closure 1.19, `collect_with_stack_compacting` 0.64,
  `with_heap_cell` 0.44. `with_heap_cell` is a per-allocation cell access; the
  nursery plan (`.notes/nursery-gc-plan.md`) is the owner.

- **The env-carrying leaf lane** (2026-10-29). Per call, minima: a named global leaf is 7.6 ns, a computed callee 13.5, but a leaf that reads a captured binding is **26.6** (31.4 with two captures), against node's 1.2-1.4. Every callee in `calls/closure_capture` captures, so that lane is what the row pays. The interpreter shows the same step (85 -> 127 ns), so it is the lane and not an interpreter artifact. Node's number is inlining (the optimizer project); the env lane is runtime work and the cheaper half. Note the leaf call lane itself is already 5x faster than node's compiled call (7.6 vs 41 ns), so "calls are slow" is wrong as a general statement.
- **A call site inside a loop body records no feedback at all**, confirmed 2026-10-29: `SLAG_FEEDBACK=1` reports 0 sites / 0 hits for a named leaf, an 8-way computed callee and a monomorphic computed callee, and `SLAG_INLINE_TRACE=1` prints `0 site(s) of 21 step(s), feedback=false`. So the feedback splice cannot reach any loop body, named or computed — that is the ceiling to keep in mind, not a per-row diagnosis.

- **The corpus's #1 row is 62% calls, and a branching leaf body is a step-path leaf** (2026-10-29). `add(a,b){return a+b}` costs 92 ns/call interpreted; add `if (a===b) throw ...` and it costs 301 ns, because a branching body has no register run and falls to the step-path leaf. The other half of the same shape is fixed: the optimizing tier now lifts `throw` (it was the only refused terminator), so such a body gets machine code. What remains is item 2, giving a branching leaf a register run. Decomposition and the per-call table are in `.notes/perf.md` (2026-10-29); closure creation, the two sessions before this one, is only 12% of that row.
- **Closure creation is 280-300 ns against node-jitless's 9 ns, and the
  registry insert and the fresh `JsObject` are the two biggest phases**
  (2026-10-29, `SLAG_CLOSURE_PROFILE` + `SLAG_CLOSURE_SKIP=props`). The record
  import is NOT size-bound at 200 bytes: see the NULL register. The remaining
  structural option is the V8 shape — the record reachable from its function
  object, with the table a non-owning id -> function index — since every
  `IdMap<EcmaFunction>` form that roots the record is measured worse (Stage D,
  twice).
- **The inline splice cannot fire on a loop-bodied caller, and that is the
  headline's biggest single ceiling.** `JitEngine::consult` compiles a loop body
  on its FIRST consult (see the 2026-10-28 PROBE), so `record_call_site` never
  runs and `sites_from_feedback` is empty for exactly those bodies. Both static
  routes now land (a callee the body creates, and a member read of a literal
  field holding one), so the remaining gap is the *feedback* class: a computed
  or megamorphic callee (`calls/closure_capture`, `fns[i & 63](i)`), which no
  static proof reaches.
- **Type a splice's result.** The continuation parameter takes the call's
  result type (`Unknown`), so a spliced region's arithmetic does not promote the
  accumulator unless the guard's fallback path is absent. Typing it from the
  callee's return value is the same missing piece as typing parameters.
- **The spliced region's block split.** A splice at a call inside a loop leaves
  the callee in its own block pair and canonicalizes the raw-f64 result per
  iteration (`Canon`), which is most of the remaining 5.3 ns on `js_call`.
- **`Type::Int` is an annotation, not a register.** A block parameter is `i64` in
  CLIF whatever its IR type; only a producing instruction (`IntAdd`, an int
  result bit op, `Const` with `Imm::Int`) yields an i32 CLIF value. So a lowering
  decision must read `builder.func.dfg.value_type(v)`, and no pass can move a
  value into an i32 register by setting types.
- **The splice rows' remaining cost is the continuation block's `Canon` and the
  guard's `CalleeEq`.** In-place splicing loses (see the NULL register); the
  route left is a versioned loop (hoist the callee check to the preheader and
  keep one copy of the body), which is a loop transformation, not a slice.
- **The element rows need a hoisted array descriptor, not a cheaper gate.** The
  `opcost` element/property rows decompose to 5.5 ns of 6.5 in
  `emit_dense_element_read`'s ladder (four dependent L1 loads: `array_dense` ->
  `elem_ptr` -> element). The key gate is a wash to remove (2026-10-28). Matching
  V8 needs the descriptor hoisted out of the loop and an element-kind proof, both
  of which are subsystems.
- **The register-lane rows cannot be typed at all.** A deopt inside a register run
  resumes at the run's start, so the typer's read guard cannot be used in
  `LeafOp::GetMemberNameLocal`/`GetMemberComputedLocal`; that is why `obj_prop`,
  `prim_prop` and `element_write` sit at 28-30x with no value typing.
- **A function's parameters at the call site.** The row that is left keeps a
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

Slag's total corpus time is 2.87x node's, and both engines' optimizer tiers buy
the same factor (2.55x for slag, 2.42x for node), so the deficit is the
runtime, not codegen: about 3x on every primitive, spread evenly enough that
no function exceeds 2.2% of self time (37.8% is compiled code, 9% the JIT
helpers, 8% builtins, 8% the VM, 6% objects, 6% heap, 5% strings). A heap
access costs about 5 to 6 ns in both tiers; V8 removes the access instead.
The work is therefore a portfolio of 30-60 ms slices, each chosen from the
sampler's share and proven with a same-binary seam, not a search for one big
win.
