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
