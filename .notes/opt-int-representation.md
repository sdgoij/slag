# The optimizing tier's int representation: `Int` as i32

**Status: proposed 2026-10-10 (plan, probe-first).** Follows
`.notes/corpus-regression.md` §7 and `.notes/opt-per-op-overhead.md` §6.

## 1. Why

The optimizing tier carries `Int`-typed values as f64 bit patterns (the 64-bit
`Value` encoding), so every int op converts f64<->i32 and re-guards. The per-step
register lane carries the accumulator as an **i32 register** and folds a whole
`k = i & MASK; s = (s + k) | 0` loop into ~3 i32 instructions. Measured
(`.notes/corpus-regression.md` §7): `opcost/baseline` opt 1.354 ms vs per-step
0.264, `object_alloc` 1.372 vs 0.272, `compound_assign` 55.6 vs 43.9 — while the
opt tier *wins* on element bodies (`element_write` 0.699 vs 1.134). Refusing the
lift is a wash; the fix is the representation.

The opt CLIF for `baseline`'s loop body: `i & MASK` is `bitcast.f64` ->
`fcvt_to_sint_sat` -> `fabs`+`fcmp` (the range guard) -> `ireduce` -> `band` ->
`sextend` -> `fcvt_from_sint` -> `bitcast`, plus a `brif` to a `BinarySlow`
fallback, ~6 times per iteration, with `store`/`load` frame round-trips between.

## 2. The design

Carry an `Int`-typed value as an **i32** ClifValue; every other value stays I64
(the Value/NaN-box bits). A chain of int ops is then pure i32 (`band`/`bor`/
`iadd`) with no `fcvt`, no `sextend`, no per-op range guard.

Boundaries convert Int <-> I64:

- `Op::FrameStore` of Int: `sextend.i64` -> `fcvt_from_sint.f64` -> `bitcast.i64`.
- `Op::FrameLoad` into Int: `bitcast.f64` -> `fcvt_to_sint_sat.i64` -> `ireduce.i32`.
- `call_helper` args / results and `Term::Return`: the same to-I64.
- Block parameters: I32 for an Int phi, I64 otherwise (`Type::join` decides).

## 3. The prerequisite — this is one arc, not a cut of its own

`baseline`'s accumulator and counter live in **frame slots** (the CLIF's
`store v69, v0+24` / `load v0+24`), so every int op already round-trips through
the frame *and* through f64. Keeping Int as i32 pays only once those slots are
SSA (`mem2reg`), which needs the body **sealed** — no slot-observer, no guard —
which needs the bit-op and comparison effects widened from `Effects::call()` to
`pure()`. Those two (the reverted "Cut 0"/"Cut A") measured only ~3.5% alone,
because the values become SSA block parameters that are still I64/f64; with the
i32 representation they are the win. So the arc is:

**effect widening + `mem2reg` + `Int`-as-i32, landed and measured together.**

Order in the pipeline: `narrow` (widening) -> `mem2reg` (promotion, sealed gate)
-> the i32 representation is a *lowering* concern (`opt_lower`), applied after
the IR is final.

## 4. Cuts

- **Cut P (probe, first).** Count, per corpus int row, how many `Int` values
  cross a boundary (a frame store/load, a helper arg/result) per iteration after
  `mem2reg`. If the count stays high, the boundary conversions cap the win and
  the probe says so *before* any lowering change. `JIT_DUMP_CLIF=1` already shows
  the `fcvt`/`sextend` sequence; a small counter in `lower_inst` gives the census.
- **Cut A (bounded).** The register-body path only: `emit_reg_body`'s int ops,
  and its entry/exit, stay i32 across the `RunRegBody` region. Bounded to the
  shapes that lose (`baseline`, `object_alloc`, `compound_assign`); the element
  rows are untouched. This is the smallest change that can reach parity.
- **Cut B (general).** Every int-producing `Op` reads and writes i32 for `Int`
  operands, with the §2 boundary conversions. Verify the rows the opt tier
  already wins (`element_write`, `array_alloc`, `array_at`) do not regress.

## 4b. Probe result (Cut P, 2026-10-10)

Re-applying the widening + mem2reg and dumping `baseline`'s post-pipeline IR
(`JIT_DUMP_IR=1`):

```
b1(v27:?, v31:?):
  v6:int  = BitAnd(v31, v5)
  v10:int = BitOr(v27, v9)
  v12:int = BitOr(v6, v9)
  v13:num = Add(v10, v12)
  v15:int = BitOr(v13, v9)
  v17:int = BitAnd(v15, v16)
  v24:num = Add(v31, v23)
  v26:bool = Lt(v24, v25)
  branch v26, b1(v17, v24), b2(v17, v24)
```

The loop is **pure SSA**: 0 frame-slot crossings and 0 helper crossings for an
`Int` value, so the representation's boundary conversions (§2) will not cap the
win — Cut A/B can proceed. The timing is unchanged (1.39 ms), confirming the
residual is the per-op f64 round trip, not the boundary. Two preconditions
surfaced, both now part of the arc:

1. **`mem2reg`'s phis are `Type::Unknown`.** `v27`/`v31` dump as `?`: `narrow`
   runs *before* `promote` in the pipeline, and nothing re-types the new block
   parameters. So the i32 representation would not see the loop-carried
   accumulator as `Int` without a re-type after `mem2reg` (a second `narrow`, or
   running `promote` before `narrow` once the IR is in SSA).
2. **The counter `v31` joins to `Number`, not `Int`.** Its stores are the entry
   `Const 0` (`Int`) and `Add(i, 1)` (`Number`), so `i & MASK`'s left operand
   keeps its range guard. That is a separate range proof on the loop counter;
   the i32 representation removes the round trip on `s` and the `| 0`/`& -1`
   results but not the guard on `i`.

The probe patch (the reverted widening + mem2reg) is not shipped; this note is
the artifact.

## 5. Measurement and gates

Target: the register-lane rows reach parity with the per-step lane (`opt/per`
within noise — `baseline` <= ~0.28 ms). Companion rows must not regress:
`element_write`, `array_alloc`, `array_at`, the `opcost/*` family mean, and the
full `node tools/corpus/bench.js` table. Gates as usual: clippy, 38 suites, both
test262 sweeps, the corpus differential, `--gc-stress` on the int rows.

## 6. Traps

- **The frame slot and the helper ABI are I64.** An Int value crossing them
  converts; a plan that ignores that cost measures worse. Cut P exists for this.
- **Block parameters and phis.** `lower` appends an I64 param for every IR param
  (`append_block_param(block, types::I64)`); an Int phi needs I32, but a phi
  mixing Int and non-Int cannot — keep I64 where `Type::join` is not Int. The
  `mem2reg` phis (`append_edge_arg`) must carry the representation.
- **`Type::Int` is a value type, not a bit pattern.** An Int value at the ABI is
  still f64 bits; the conversion is `sextend`/`fcvt_from_sint`/`bitcast`. `-0`
  and NaN do not arise (an i32 has neither), so the conversion is exact.
- **Do not change the ABI.** Compiled entries, helper signatures and the
  completion register stay I64; only the *within-body* representation changes.
  A helper argument list is untyped at the call site (`emit_raw_call`), so an
  arity/representation slip is silent — check every `call_helper`/`emit_raw_call`
  for an Int argument.
- **The register lane is the reference, not a rival.** Where its i32 path is
  simpler than the opt lowering can express, port its shape rather than invent
  one.

## 7. Result (Cut A/B, 2026-10-10)

The `Int`-as-i32 representation landed whole (there is no useful"bounded Cut A":
the representation is a property of a value, so it is global or absent):

- **Lowering** (`opt_lower`): a `Type::Int` value rides in an i32 register;
  block parameters, `Op::Const`, and `Op::FrameLoad` follow the type; every
  other use widens back to its `Value` word (`int_to_bits`/`bits_to_int`). The
  bit-op fast path (`call_int_binary`) keeps a signed result in i32, converts
  only a `Number` operand, and skips the guard/slow-block entirely when both
  operands are already i32.
- **New IR ops** `IntAdd`/`IntSub`/`IntMul` (wrapping int32, `pure`) for the
  register body's proven-int arithmetic, and `narrow::fuse_wrapping_arith`
  rewrites a general `ToInt32(a + b)`/`(a - b)` (two int32 operands) to them.
  **`*` is excluded**: an int32 product can exceed `2^53`, so the spec's f64
  multiply rounds before `ToInt32` (the register plan's separate `Mul` bound is
  what keeps its `IntMul` safe).
- **`mem2reg` phis** are retyped to the join of their incoming values, and
  `pass::normalize_param_types` re-types every block parameter the same way (a
  lifted merge parameter is often `Unknown` while its edges are `Int`);
  `opt_lower::resolve` widens an `Int` edge argument when the parameter is not
  an int32.
- **The guard mirror** (`emit_guard`) widens an `Int` live value back to its
  `Value` word before mirroring the operand stack into the working region — a
  raw i32 store would corrupt the slot the interpreter resumes from (found via
  a parallel-test failure; regression test
  `opt_tier_int_value_survives_a_guard_deopt`).

Measured two ways. **Isolated, min-of-5, HEAD build vs this build** (`scratch/row-after-ab.sh`, the only reliable instrument — the corpus `mean-jitGap` moves ±0.7 run-to-run on the *same* binary and its big rows ±60 ms, which is what made the first corpus diff of this change look like a wash):

| row | HEAD | this | delta |
| --- | --- | --- | --- |
| `opcost/baseline` | 1.344 | 0.292 | **-78%** |
| `opcost/object_alloc` | 1.366 | 0.252 | **-82%** |
| `opcost/array_alloc` | 0.608 | 0.251 | **-59%** |
| `opcost/array_at` | 2.646 | 2.618 | -1% |
| `opcost/element_write` | 0.694 | 0.714 | +3% |
| `opcost/element_read` | 0.686 | 0.884 | **+29%** |
| `objects/destructure` | 169.4 | 168.7 | -0.4% |
| `control/generator_loop` | 76.0 | 77.3 | +2% |
| `arrays/typed_array` | 42.4 | 39.6 | -7% |
| `calls/construct_churn` | 94.1 | 95.8 | +2% |
| `opcost/array_to_sorted` | 148.6 | 147.6 | -1% |

The rows a single corpus run flagged as regressions (`destructure`, `generator_loop`, `construct_churn`, `array_to_sorted`) are **flat** under the isolated harness — do not trust a one-shot corpus diff for this.

The opt-vs-`SLAG_OPT=0` view (min-of-3) of the same rows:

| row | before | after |
| --- | --- | --- |
| `opcost/baseline` | 1.35 vs 0.25 (+427%) | 0.30 vs 0.27 (+12%) |
| `opcost/object_alloc` | 1.35 vs 0.28 (+390%) | 0.27 vs 0.28 (-9%) |
| `opcost/element_write` | 0.69 vs 1.13 (-39%) | 0.77 vs 1.16 (-34%) |
| `opcost/array_alloc` | 0.59 vs 0.68 (-12%) | 0.26 vs 0.69 (-63%) |
| `opcost/element_read` | 0.70 vs 0.78 (-10%) | 0.92 vs 0.81 (+13%) |

The headline regression is closed (baseline 1.34 -> 0.29 ms, 4.6x; object_alloc
5.4x; array_alloc 2.4x). `element_read` was the one real regression (+29%):
its `k = i & MASK` result is a proven `Int` stored to a **frame slot** `mem2reg`
does not promote (the body's `new Array` gives the whole function a Slots
observer), and each iteration then paid an i32<->word conversion on the store
and the load.

**Fixed by consumer-driven typing** (`narrow::restrict_int_to_arith_use`): the
representation is worth carrying only inside an int chain, so a value whose uses
are all boundaries (a frame store, a helper arg, an element key) stays a
`Number` — the store/load then cost nothing. A value is kept `Int` when it has an
arithmetic/bit use, or is the store of a slot read into one (the register body's
accumulator), and `IntAdd`/`IntSub`/`IntMul` results are always kept (their
lowering *is* an i32 op). This is safe by construction: a fusion operand is by
definition an arith use, so no `ToInt32(a + b)` fusion is ever lost, and no
`IntAdd` operand can be downgraded (which would have been a refusal). Result:
`element_read` 0.884 -> 0.74 ms (now 6% *faster* than the per-step lane, was 29%
slower) and `element_write` 0.77 -> 0.60 ms.

`baseline` is ~+12% off the per-step lane: its counter `i` is a `Number`, so
`i & MASK` keeps the f64 range guard — a separate range proof on the loop
counter, not the representation.

Gates: clippy clean; `cargo test --workspace` green (parallel; the serial-only
`opt_tier_leaf_guard` failure predates this change); test262 `language`
23726/0/0/0 and `built-ins` 23820/0/1/0 at `--timeout 15`; corpus differential
0 mismatches; `--gc-stress` values exact on the int rows.

Measurement note: the corpus `mean-jitGap` is not trustworthy at this grain —
it is dominated by rows with a near-zero `node` denominator and by two
~400 ms outliers, and it moves ±0.7 on a *fixed* binary. Use the isolated
min-of-N row A/B (`corpus-row-ab.sh` for opt-vs-per-step, `row-after-ab.sh` for
binary-vs-binary).
