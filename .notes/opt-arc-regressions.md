# The int-representation arc's regressions

**Status: open (found 2026-10-10, during the post-arc re-measurement).**
The arc `014b3d41` → `418986d0` → `d612ce0c` (opt-tier `Int` as i32; keep it
only for arith/bit uses; the bounded-counter proof) is a **net wash** on the
corpus. This note records the measurement, the bisect that localizes the two
real losses, and the one big correction: the earlier `typed_array` report was a
measurement artifact.

## 1. Re-measurement vs V8 (the archive stays flat)

`node tools/corpus/bench.js`, three runs, HEAD `d612ce0c`:

| | README (`v0.1.0`) | run 1 | run 2 | run 3 |
|---|---|---|---|---|
| `mean-jitGap` | 23.09 | 23.31 | 23.38 | 23.51 |
| `mean-jlGap` | 5.43 | 5.48 | 5.30 | 5.48 |

`mismatches 0` in every run. Overall flat — the arc did not move the headline.

## 2. Whole-corpus A/B (pre-arc vs HEAD)

`scratch/corpus-binary-ab.sh scratch/slag-head.exe target/release/slag.exe 3`:

```
n=77  sum A=4134.6  B=4134.2  (-0.0%)  wins(<=-2%)=26 losses(>=+2%)=11
```

A wash. **But the per-row *absolutes* in the full-corpus run are not
trustworthy:** `arrays/typed_array` measures 31.3 (README), 41.8 (pre-arc
corpus), 56.1 (HEAD corpus), but 41.7 (pre-arc isolated) and 38.2 (HEAD
isolated). Cross-workload pollution moves a small hot loop 40%+. The same
happens to `objects/destructure` (166 isolated vs ~340 in-corpus) and
`strings/coercion_concat` (83 isolated vs 186 in-corpus). **Per-row corpus
deltas are not a gate; the isolated min-of-N A/B is.**

## 3. The bisect (isolated min-of-5)

Built a binary at each code-changing commit and A/B'd the rows:

| row | P `ef058490` | S1 `014b3d41` | S2 `418986d0` | S3 `d612ce0c` |
|---|---|---|---|---|
| `opcost/baseline` | 1.34 | 0.286 | 0.283 | **0.096** |
| `opcost/object_alloc` | 1.345 | 0.251 | 0.251 | 0.251 |
| `control/nested_loops` | 26.95 | **29.82** | 29.80 | 29.78 |
| `opcost/element_read` | 0.680 | **0.877** | 0.734 | 0.735 |
| `arrays/typed_array` | 41.7 | 39.1 | 38.0 | 38.2 |
| `objects/spread_assign` | 143.7 | 142.9 | 144.8 | 142.3 |
| `objects/warm_store` | 51.0 | 51.0 | 51.5 | 51.6 |
| `objects/destructure` | 168.1 | 167.1 | 165.6 | 168.6 |

`P` was reproduced by three independent builds (the prior session's
`scratch/slag-head.exe`, plus two built here), so `P` vs the stages is a
source difference, not build layout.

**Localization:**

- **S1 (`014b3d41`, `Int` as i32) is the whole lever** — the 14x `baseline` win
  (1.34 → 0.286) and the 5x `object_alloc` win are both here.
- **S1's losses:** `control/nested_loops` +11% (26.95 → 29.82, never recovered)
  and `opcost/element_read` +29% (0.680 → 0.877).
- **S2 (`418986d0`, `restrict_int_to_arith_use`)** recovers `element_read` to
  +8% (0.877 → 0.734) and does nothing else.
- **S3 (`d612ce0c`, the bounded-counter proof)** takes `baseline` 0.286 → 0.096
  and does nothing else.
- **`arrays/typed_array` improves** (41.7 → 38.2). The earlier "+34%" was the
  full-corpus pollution of §2 — **retracted**; the arc does not regress it.
- `spread_assign`, `warm_store`, `destructure` are flat isolated → their
  corpus deltas were also pollution.

## 4. The one real, localized regression: `nested_loops`

The `nested_loops` IR is **identical** between `P` and `S1` except the effects
annotation on the two `Lt` guards (`[segw/segw]` → pure) — no typing change
(the loop is all `num`/`?`; it has no bit ops, so neither `restrict` nor the
counter proof touch it). Yet S1 is +11%. So the loss is in the *lowering* of
that effects change (or the CLIF S1 emits for it), not the pass IR.

**Next probe:** dump CLIF at `P` vs `S1` for `nested_loops`, normalize the
helper-address constants, and diff the loop body. The `Lt` effects annotation
is the only pass-IR delta; find what S1's lowering does with it.

## 5. Gates

`cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace`;
test262 `language` 23726/0/0/0 and `built-ins` 23820/0/1/0 at
`--timeout 15 --recheck-timeout 15`; corpus differential (`node
tools/corpus/bench.js`, `mismatches 0`).
