# The corpus regression: a cross-workload JIT slowdown

**Status: open (found 2026-10-10). Bisected to `efa7bb91` ("perf(jit): reproduce the per-step leaf-call lane in the optimizing tier (G)", 2026-10-09).**

## 1. The re-measurement

`node tools/corpus/bench.js` on the same machine and node the README's
Performance section used (AMD Ryzen 9 7950X, Windows 11, node v24.12.0) gives a
JIT gap well above the README's table, while the interpreter gap matches:

| Family | README jitGap | now jitGap | README jlGap | now jlGap |
|---|---|---|---|---|
| arrays | 18.23x | 19.15x | 4.10x | 4.17x |
| builtins | 10.28x | 9.26x | 5.10x | 5.68x |
| calls | 11.42x | **28.92x** | 5.46x | 5.53x |
| control | 17.89x | **72.49x** | 5.15x | 5.46x |
| globals | 2.14x | 2.44x | 0.75x | 0.78x |
| language | 11.42x | **88.31x** | 9.05x | 10.68x |
| objects | 66.63x | 56.02x | 2.90x | 3.18x |
| opcost | 23.20x | 24.91x | 4.77x | 5.02x |
| strings | 23.03x | **183.10x** | 17.53x | 17.02x |
| **All** | **23.09x** | **41.25x** | **5.43x** | **5.68x** |

The interpreter is flat; the JIT gap regressed, concentrated in
`calls`/`control`/`language`/`strings`.

## 2. It is not a uniform slowdown — it is cross-workload

The whole-corpus process inflates a subset of rows 10-25x, and **the JIT column
far more than `--jitless`**. Isolated (its own directory), the same row is fine:

| row | isolated jit | isolated jl | whole-corpus jit | whole-corpus jl |
|---|---|---|---|---|
| `strings/search_slice.js` | 22 | 30 | 2213 | 128 |
| `calls/construct_churn.js` | 112 | 211 | 2450 | 254 |
| `control/generator_loop.js` | 103 | 159 | 1044 | 194 |
| `language/.../evaluation-order.js` | 385 | 798 | 9273 | 965 |
| `opcost/baseline.js` | 1.4 | 3.6 | 1.4 | 1.5 |

Per-family, `control/generator_loop` measures 96 ms — matching the README's
93.9 ms — while the whole-corpus process gives 1044 ms. So the README's run did
not have the effect; it is a regression.

## 2b. Decisive: the whole corpus regressed since the README commit

Building `b3fd0d9e` (the README's own refresh commit) and running the *identical*
`--corpus tools/corpus/workloads` command on the same machine:

| row | b3fd0d9e (README) | HEAD |
|---|---|---|
| `opcost/baseline.js` | 0.26 | 1.36 |
| `strings/search_slice.js` | 60.4 | 1739 |
| `calls/construct_churn.js` | 128.9 | 1498 |
| `control/generator_loop.js` | 103.6 | 647 |
| `language/.../evaluation-order.js` | 460.7 | 5315 |
| `strings/coercion_concat.js` | 194.7 | 106 |
| whole-process sum (77 rows) | **4813** | **13185** |

`b3fd0d9e` matches the README (generator_loop 103.6 ≈ 93.9; `baseline` 0.26 ≈ the
0.3 control row), so the README's run is reproducible and HEAD is 2.7x worse
overall. Two components, both from the opt-lift arc (the F*/I5*/I6* commits
between `b3fd0d9e` and HEAD):

1. **Lift coverage moved bodies into the opt tier, which is slower on some
   shapes.** `opcost/baseline` was per-step at `b3fd0d9e` (0.26 ms) and is opt
   at HEAD (1.36 ms) — the opt tier's known ~5x loss on the `i & MASK`/`| 0`
   loop (`.notes/opt-per-op-overhead.md` §1), now applied because the body lifts.
2. **The opt tier has a severe cross-workload pathology the per-step tier does
   not.** `search_slice` is *faster* in the opt tier isolated (22 ms vs the
   per-step ~60 ms) but 29x slower in a long-lived process (1739 ms) — §3-4.

## 3. A clean reproducer, and the bisect

Two files in one directory, run in one process (`coercion_concat` first, sorting
as `a_pre.js`; `search_slice` as `z_ss.js`):

```
target/release/slag.exe --corpus <dir with a_pre.js=coercion_concat, z_ss.js=search_slice>
```

`z_ss` is ~22 ms alone and **~220-290 ms** in the pair (reproducible). The same
pair under `--jitless` is fine (26 ms), so the compiled path is the culprit. Not
process lifetime (ten copies of `search_slice` in one process stay flat at
~23 ms) — it is *specific preceding workloads*. Degraders so far:
`coercion_concat`, `split_join`, `spread_assign`; non-degraders: `map_churn`,
`object_alloc`, `typed_array`, `json_roundtrip`, `generator_loop`, and any
allocation-free numeric row (`nested_loops` is unaffected by `coercion_concat`).

`git bisect` over `b3fd0d9e..HEAD` (61 commits, the README's refresh to HEAD)
names **`efa7bb91`** — the first commit that ports the per-step leaf-call lane
into the opt tier's `Op::Call` via `emit_leaf_call` — as the first bad commit.
Its own message verifies correctness (`--gc-stress`/`--nursery-stress`/
`--gc-verify` exact on the corpus) but not this cross-workload timing.

## 4. Mechanism evidence, and the corrected trigger

- **Same helper counts.** `JIT_HELPER_STATS=1` on the pair is exactly the sum of
  the two alone runs (`CallSlow` stays 1.4M for `search_slice`); nothing explodes.
  So the compiled body does identical work — each op is just ~10x slower.
- **Not minor-GC pacing.** `--nursery-threshold 2000000000` does not help
  (275.9 -> 236.5 ms).
- **JIT-only.** `--jitless` on the same pair is 26 ms.

The trigger is the **string-literal constant widening** in `efa7bb91`'s
`constant()` (`Imm::U64(value.bits())`, `Type::String`): a body containing a
string literal used to bail (`Unsupported::Step("Push")`) and stay on the
per-step tier; now it lifts. A seam on that one arm (`SLAG_STRCONST=0`) restores
the per-step path and drops the isolated-pair reproducer from 209 ms to 23 ms.

The `Op::Call` leaf lane is **ruled out**: a seam that lowers `Op::Call` back to
`call_slow` (`SLAG_OPTCALL=0`) leaves the pair degraded (210 vs 202 ms), and
`search_slice`'s calls lower as `Op::Intrinsic`, not `Op::Call`.

But disabling the string-const lift is **not** a fix: on the whole corpus it
makes `coercion_concat` explode to 19027 ms (reproducible) and the process sum
worse (32260 vs 13185). So the lift is net-positive overall; it merely *exposes*
the pathology. The mechanism proper is still open — the only facts are "same ops,
JIT-only, ~10x slower after another workload ran", which needs native profiling
(RSS, GC-pause distribution, code-cache stats) rather than more guessing.

## 4b. The compiled-loop GC safe point is the cost

Seams on the safepoint (`runtime::jit::gc_safepoint` → `Agent::maybe_collect`),
one build, the isolated pair:

| seam | `search_slice` |
|---|---|
| (none) | 194.7 ms |
| `SLAG_GC_POLL=0` (skip the safepoint) | **18.9 ms** |
| `SLAG_GC_SKIP=minor` | 204.2 ms |
| `SLAG_GC_SKIP=major` | **19.2 ms** |

So the compiled loop's safe point runs a **major** collection, and that is the
whole cost. `SLAG_GC_LOG=1` on the corpus pair shows ~171 majors at a *tiny* live
set (`live=7260 last=3164`): after each sweep `last_collected_live` drops to the
swept-down live (~3164), the loop's allocations double `live` back to ~7260, and
the check `live > 2*last_collected_live` re-fires — a major every ~585
iterations, ~1 ms each.

`--gc-trace` (the corpus path does not apply it — use the file path) on an
equivalent single-file pair shows the JIT and the interpreter diverge on
*pacing*, not on collection cost:

| | collections | max pause | young at the big one |
|---|---|---|---|
| jitless | hundreds (majors, `swept=4100`) | **306 µs** | — |
| jit | 14 | **46,643 µs** | **1,504,007 boxes** |

The compiler's fast path lets the young cohort balloon to 1.5M boxes and then
pays a 46 ms collection, where the interpreter's back-edge pacing keeps every
collection ~150-300 µs. `stack_words` is modest in both (~6000), so it is NOT
the conservative scan — it is the collection cadence.

**Root cause:** the compiled loop's safe point does not pace collections like the
interpreter's back-edge check, so an allocating compiled loop either majors
repeatedly (`live > 2*last` sawtoothing at a tiny live set) or accumulates a huge
cohort; either way the pause dominates. The fix is a pacing fix in the compiled
safe point (and the major trigger), not a codegen or scan change.

## 5. Why it matters

`search_slice` alone is ~22 ms; in the whole-corpus process it is ~2200 ms — a
100x swing from a *shared-state* effect, not from its own code. This dominates
the gap-versus-README and is invisible to the isolated `scratch/corpus-row-ab.sh`
measurements the opt-tier notes have been using. Any whole-corpus or long-lived
comparison (the README tables, the Deno embed) sits on top of it.

The `search_slice` figure above is the pre-fix number; §6 records the fix.

## 6. Fixed (2026-10-10): the optimizing tier had no GC safe point

The mechanism in §4b — a compiled loop that allocates with no collection — is
that `opt_lower.rs` had **no** safe point: `emit_gc_probe` / `GcSafepoint` /
`gc_ticks` existed only in the per-step compiler (`compiler.rs`), so every loop
that LIFTED into the optimizing tier ran with no probe at all. The `SLAG_GC_LOG`
seam confirmed it: the reproducer's `coercion_concat` loop made **0**
`gc_safepoint` calls over 300k iterations (1.5M allocations), while the per-step
path polled ~292 times.

Fix: `opt_lower::emit_gc_probe`, emitted at each loop header (the target of a
back edge, `target <= source`), mirroring the per-step probe — decrement the
ctx's `gc_ticks`, and on underflow reset it and call `gc_safepoint` through
`call_helper` (whose pending check is the termination channel). The loop block's
parameters flow through the poll's continuation block.

Measured: the isolated reproducer's `search_slice` **194.7 -> 22.6 ms**; the
whole-corpus process sum **13185 -> 4385 ms** (the README commit `b3fd0d9e` was
4813, so the fix also clears the residual); `node tools/corpus/bench.js`
mean-jitGap **41.25 -> 24.98** (README 23.09), mean-jlGap 5.55 (README 5.43),
with `calls` 28.9 -> 11.9, `control` 72.5 -> 18.3, `language` 88.3 -> 12.7,
`strings` 183.1 -> 24.7. Values unchanged (0 corpus mismatches; `baseline` /
`dyn_key_read` exact under `--gc-stress`). Gates: clippy clean, 38 suites green,
test262 `language` 23726/0/0/0 and `built-ins` 23820/0/1/0. A regression test,
`installed_jit_opt_loop_safe_point_runs_minors`, pins it (a lifted allocating
loop runs minors and its cohort stays bounded).

Component 1 (§2b) is untouched: the opt tier is still slower than the per-step
register lane on shapes like `opcost/baseline` (1.36 ms vs 0.26 ms), which now
lifts into it — that is the read-parity work in `.notes/opt-per-op-overhead.md`,
not this regression.

## 7. Component 1: the opt tier vs the per-step register lane (measured)

After the GC-safe-point fix (§6), the optimizing tier is still slower than the
per-step register lane on int/bit-heavy register bodies, and *faster* on element
bodies. The `SLAG_NOREG=1` seam (`opt/lift/mod.rs`) refuses `RunRegBody` lifting
so the body stays on the register lane; the "refuse" column is the same binary
with it set:

| row | opt | refuse (= per-step) | opt better? |
|---|---|---|---|
| `opcost/baseline` | 1.354 | 0.264 | no (5.1x worse) |
| `opcost/object_alloc` | 1.372 | 0.272 | no (5.0x) |
| `objects/compound_assign` | 55.6 | 43.9 | no (1.27x) |
| `opcost/element_write` | 0.699 | 1.134 | yes (1.62x) |
| `opcost/array_alloc` | 0.603 | 0.693 | yes (1.15x) |
| `opcost/array_at` | 2.671 | 2.873 | yes (1.08x) |
| `objects/many_objects_read` | 37.4 | 37.1 | ~equal |

**Refusing the lift is a net wash** — it recovers `baseline`/`object_alloc`/
`compound_assign` to the register lane's speed but gives back
`element_write`/`array_alloc`/`array_at`. So the fix is not the lift decision;
it is the opt tier's lowering.

The opt CLIF for `opcost/baseline`'s loop shows why. `i & MASK` lowers to
`bitcast.f64` -> `fcvt_to_sint_sat` -> `fabs`+`fcmp` (the range guard) ->
`ireduce` -> `band` -> `sextend` -> `fcvt_from_sint` -> `bitcast`, plus a `brif`
to a `BinarySlow` fallback — and that per-op shape repeats ~6 times per
iteration, with frame-slot round-trips between. The per-step register lane folds
the same loop into ~3 i32 instructions with the accumulator in an i32 register.
The reason is representation: the opt IR carries `Int`-typed values as f64 bit
patterns, so every int op converts f64<->i32 and re-guards.

**Direction:** keep an `Int`-typed value as i32 through the lowering (a
per-type representation) so a chain of int ops stays in i32 — no `fcvt`, no
`sextend`, no guard-visible f64 round trip. That is the `Int`-lattice work's
natural continuation (`.notes/opt-per-op-overhead.md` §6); the lift-decision
refusal is ruled out by the table above.
