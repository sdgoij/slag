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

## 5. Why it matters

`search_slice` alone is ~22 ms; in the whole-corpus process it is ~2200 ms — a
100x swing from a *shared-state* effect, not from its own code. This dominates
the gap-versus-README and is invisible to the isolated `scratch/corpus-row-ab.sh`
measurements the opt-tier notes have been using. Any whole-corpus or long-lived
comparison (the README tables, the Deno embed) sits on top of it.
