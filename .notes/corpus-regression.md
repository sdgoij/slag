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

## 4. Mechanism evidence, and the leading hypothesis

- **Same helper counts.** `JIT_HELPER_STATS=1` on the pair is exactly the sum of
  the two alone runs (`CallSlow` stays 1.4M for `search_slice`); nothing explodes.
  So the compiled body does identical work — each op is just ~10x slower.
- **Not minor-GC pacing.** `--nursery-threshold 2000000000` does not help
  (275.9 -> 236.5 ms).
- **JIT-only.** `--jitless` on the same pair is 26 ms.

Leading hypothesis: the ported lane almost tripled `opt_lower` (541 lines) and
generates far more machine code per call site, so a call-heavy compiled body no
longer fits the instruction cache/btb after another call-heavy workload has
evicted it — the same ops, cache-thrashed. This is consistent with "same helper
counts, JIT-only, call-heavy rows and their predecessors only, allocation-free
numeric rows unaffected". It needs confirming (compiled code size per body with
and without the port), and the fix is either a slimmer call lowering or a revert
of the port.

## 5. Why it matters

`search_slice` alone is ~22 ms; in the whole-corpus process it is ~2200 ms — a
100x swing from a *shared-state* effect, not from its own code. This dominates
the gap-versus-README and is invisible to the isolated `scratch/corpus-row-ab.sh`
measurements the opt-tier notes have been using. Any whole-corpus or long-lived
comparison (the README tables, the Deno embed) sits on top of it.
