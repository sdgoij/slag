# The opt tier's per-op overhead against the per-step register lane

**Status: open (opened 2026-10-10).** Probe-first. The opt tier is the default
(`SLAG_OPT` on), so a row where it is *slower* than the per-step tier is a
pessimization, not a missed optimization — the read-parity contract the arc runs
on. This note pins which rows lose, by how much, and to what, and proposes the
slices. No code change yet.

## 1. The rows that lose (isolated, min-of-3, opt vs `SLAG_OPT=0`)

Measured one row per process (`scratch/corpus-row-ab.sh`); the whole-corpus
`--corpus` sweep is contaminated (late rows in a family share a process) and
mismatches these — trust only the isolated runs.

| row | per-step | opt | opt/per |
|---|---|---|---|
| `opcost/baseline` (`k = i & MASK; s = (s + k) \| 0`) | 0.27ms | 1.59ms | **5.9x** |
| `opcost/object_alloc` (same shape) | 0.28ms | 1.58ms | **5.6x** |
| `calls/construct_churn` (`s += new Item(i).sum()`) | 94.9ms | 149.1ms | 1.57x |
| `objects/many_objects_read` (`os[i & 1023].w`) | 21.7ms | 39.4ms | 1.81x |
| `objects/compound_assign` | 45.8ms | 54.2ms | 1.18x |
| `globals/hoisted_local` | 2.58ms | 3.01ms | 1.17x |
| `opcost/object_keys` | 53.4ms | 58.5ms | 1.10x |

`construct_churn` is the largest *absolute* loss (+54ms); `opcost/baseline` the
largest *relative* (5.9x). They are different causes (§4, §5).

## 2. The cause, read from the code

The per-step tier compiled a certified fused loop with a **specialized register
lane**: `RunRegBody` keeps the accumulator and counter in registers, folds the
`& MASK` / `| 0` into i32 ops, and emits no per-op guard. `bench`'s loop in
`opcost/baseline` (per-step asm) is:

```
block27:
  vaddsd (%rip), %xmm6, %xmm6      ; i += 1.0        (counter in xmm6)
  ...
  cvt_float64_to_sint64_sat_seq %xmm1, ...   ; i -> i32
  leal (%rdi, %r8), %edi           ; acc += i        (accumulator in edi)
  vucomisd %xmm6, %xmm0            ; i < 100000
  jnbe label28 / j label29
```

The opt tier lifts the *generic* `Step` stream into `Op` values and lowers each
op faithfully, so it pays per-op overhead the register lane does not. `i & MASK`
in the opt CLIF is **~23 instructions**: two `fabs`+`fcmp` range checks, two
`fcvt_to_sint_sat`, the `band`, `sextend`/`fcvt_from_sint`/`bitcast` back, a
3-instruction `canon_double`, and a 5-instruction known-ness check + branch that
falls to `BinarySlow` out of range. The whole `baseline` loop is ~60 ops for
what the register lane does in ~10.

## 3. The contributing overheads (shares are `opcost/baseline`-approx)

1. **Frame-slot traffic, not registers (~15%).** The opt lowering emits
   `FrameLoad slot; <op>; FrameStore slot` per variable; the register lane keeps
   them in registers and stores only at the loop exit / deopt. Removing it needs
   slot promotion *with* materialization at deopt (`Op::Check`/`GuardOp`) and
   before any `Heap::Slots` effect — a real mem2reg-with-flush, not the existing
   callee-only `pass::mem2reg`.
2. **`canon_double` per arithmetic value (~25%).** Every `emit_bare_numeric`
   result is canonicalized against the tag region (3 ops). The register lane
   canonicalizes once, at the exit. Sinking it to the points where a `Value` is
   materialized (stores, call args, returns) is sound but needs every escape
   point enumerated — a missed one is a NaN reading as a tag (crash).
3. **Int-op range guards on provable operands (~15-20%). Done 2026-10-10.**
   `call_int_binary` always emitted the `|x| < 2^63` guard; an operand whose IR
   type is `Int` (a `| 0` / bit-op result, an int32) or an in-range constant
   cannot saturate the f64→i64 conversion. `narrow` now carries an `Int` lattice
   beside its `Number` one (a slot whose every store is `Int`), `trunc_i32`
   skips the `fabs`/`fcmp` for an `Int` operand, and the `known && known` tag
   conjunction constant-folds. See §6.2.
4. **Boolean materialization for a comparison terminator (~10%).** `Op::Lt`
   builds a canonical Boolean `Value`, and `Term::Branch` compares it against
   the NaN-boxed `false` bits; the register lane branches straight on the
   `fcmp`. An `Lt`-feeding-a-`Branch` fusion would remove it.
5. **The compiler's own `| 0` / `& -1` normalization ops.** Both tiers carry
   these; not an opt-vs-per-step difference.

## 4. `construct_churn` is a *different* item — the leaf-call lane

**Corrected 2026-10-10 by the newly instrumented opt tier.** The opt lowerer
now carries the same compile-time-gated helper counter as the per-step path
(`opt_lower::call_helper`, the single funnel for the opt tier's helper calls;
`JIT_HELPER_STATS`, off by default so a default build is byte-identical). The
census for `s += new Item(i).sum()` over 500k iterations (helper index =
`Helper as usize`):

| helper | opt | per-step |
|---|---|---|
| 11 `GetMemberName` | 7,000,003 | 3,500,007 |
| 18 `CallSlow` | **3,500,000** | 0 |
| 19 `LeafCallProbe` | **3,500,000** | 867 |
| 20 `LeafCallFill` | 0 | **3,499,133** |
| 53/54/57 `ArgsBase`/`ArgsPush`/`Construct` | 3,500,000 | 3,500,000 |

So it is **not** a prototype-read IC: both tiers resolve `.sum` identically
(the same `GetMemberName` misses). The gap is the **inline leaf-call lane**:
the opt tier's probe fires on every call and always misses to `CallSlow`,
while the per-step tier's probe is cached (867 verdicts → 3.5M `LeafCallFill`).
That is the "the inline leaf-call path is reached and never fires" finding in
`.notes/jit-quality-plan.md` §3 (G2), which the G port was meant to close but
which still bites a `this`-taking prototype method. The opt also makes **2x the
`GetMemberName` calls** (7M vs 3.5M) — the `CallSlow` path re-resolving, or a
duplicate read; to pin.

This reorders the slices below: the leaf-call lane (§6.1) is the largest
*absolute* win (one row, ~+50ms), ahead of the int-op work.

**Resolved (2026-10-10).** The cause is the **typer's read guard**. `lift`
emits `guard_read_value` — a `GuardType` asserting a read's value is a Number so
the arithmetic that consumes it lowers tag-free — and a guard-bearing body is
marked `deopts`. Every machine-code inline lane refuses a `deopts` callee (the
caller has no deopt path), so a *leaf* body carrying the read guard could never
be inlined and its call fell to `CallSlow`. The per-step tier's bodies are not
`deopts` (its flag is a diagnostic probe, normally false), so its leaf lane
fired.

The fix: do **not** emit the read guard for a *leaf* body (`lift` now passes
`typer_reads_enabled() && !body.leaf`). A leaf body is a call-site inline
candidate, so its inlinability is worth more than its own tag-free arithmetic;
a non-leaf body, which no caller inlines, keeps the guard. This is always
sound — the guard is a speculation, so removing it only leaves the consuming
arithmetic on its tag-checked path.

Measured: `construct_churn` **222.9 -> 94.7ms** (per-step 91.6, ~parity); the
census flips to `LeafCallFill` 3,499,133 / `CallSlow` 0 / `LeafCallProbe` 867,
and the `JIT_LEAF_TRACE` diagnostic shows only the transient `no-compiled-code`
rejection. The guard's benefit is preserved where it pays (`opcost/dyn_key_read`
-8.8%, `opcost/baseline` and `objects/destructure` unchanged).

## 5. `many_objects_read` is (mostly) inherent

`os[i & 1023].w` cycles 1024 objects past the 16-entry direct-mapped member
cell, so the guard misses by construction; the per-step tier is faster because
its fused-loop register lane does the *element* read inline. The residual is
§3's lane, plus a cell-size/collision question (`MEMBER_CELLS`).

## 6. Proposed slices (probe-first, ordered by value)

1. **The inline leaf-call lane for a `this`-taking callee** (§4). **DONE
   2026-10-10** — the read guard is no longer emitted for a leaf body, so a
   leaf callee stays inlinable. (`calls/direct_leaf`'s callee still keeps the
   guard and misses ~6%; revisit if the guard's leaf-body value is worth it.)
2. **`Int`-proven operands + `call_int_binary` guard elision** (§3.3). **DONE
   2026-10-10.** `narrow` gained an `Int` lattice (a slot whose every store is
   `Int`), the lift's `Type::Int` on a `&`/`| 0` result now reaches the lowering
   through the value table, `trunc_i32(…, known_int)` returns the constant guard
   for an `Int` operand, and the `known && known` tag conjunction folds. See the
   measured result below.
3. **`canon_double` sinking for `Int`-typed results** (§3.2, narrow case). An
   int32's f64 is never a NaN, so an `Int`-resulting op needs no canon at all —
   the cheapest cut of the canon cost, no escape analysis required.
4. **Frame-slot promotion with deopt materialization** (§3.1). The largest
   structural slice; needs the flush-point design (deopt ops + `Heap::Slots`
   effects). Do after 2-3 so the register-allocation win is measurable alone.
5. **`Lt`→`Branch` fusion** (§3.4). Small, removes the bool round trip.

The opt-tier helper instrument is landed (§4) and should be used to rank the
remaining call-shaped rows before 1 is attempted.

**Done (2026-10-10) — the int-op fast path + the `Int` lattice.** The first cut:
`call_int_binary` elides `is_double` for an operand the typer proves a Number
(mirroring `call_numeric_binary`), drops `canon_double` entirely (an int32's `f64`
is an integer, never a NaN), and `narrow`'s `is_numeric_inst` gained the bit ops,
so a slot stored `k = i & MASK` is proven numeric and its re-load lowers
tag-free. The second cut: `narrow` carries an `Int` lattice beside the `Number`
one, so `trunc_i32` skips its `|x| < 2^63` guard for an operand proven `Int` (and
the `known && known` tag conjunction constant-folds). Measured (isolated,
min-of-3, opt/per): `opcost/baseline` +510% -> +420% -> **+321.6%**,
`opcost/object_alloc` +460% -> +400% -> **+330%**, `opcost/element_read` +3% ->
-6.8% -> **-14.9%**, `opcost/dyn_key_read` -8.8% -> **~-16%**, `opcost/array_at`
-> **-11.6%**, `opcost/math_abs` -14.3%, `opcost/prim_prop` -17.4%,
`opcost/set_has` -13.0%, `objects/warm_store` -17.1%, `control/nested_loops`
-11.6%. No row regressed (the opt-slower rows — §1's `object_keys` ~+9%,
`many_objects_read` ~+73%, `compound_assign` ~+22%, `hoisted_local` ~+13% — are
unchanged). The residual on `baseline` is structural: `i` is stored `i + 1`, an
`Add` whose result `narrow` types `Number` (a sum of int32s can exceed `2^31`),
so the `i & MASK` left operand keeps its guard — only `k`, `s` and the constants
are provably `Int`. §6.3's arithmetic-op `canon` sinking and §6.4/§6.5 remain.

## 7. Measurement discipline

- One row per process (`scratch/corpus-row-ab.sh <row>`), min-of-3, interleaved
  `SLAG_OPT=0` vs default.
- A row is "fixed" only when `opt/per` is within noise of 1.0; a cluster fix is
  judged on the family sum, not one row.
- `--jit-bench`'s `arithmetic`, `bare loop`, `property read`, `typed-array *`
  are the companion micros.

## 8. The register lane: promote slots to SSA (plan, probe-first)

**Status: Cut 0 + Cut A tried 2026-10-10 and REVERTED (net ~zero); Cut B
untouched. The result below refutes §3.1** — slot traffic is not the `opcost`
gap. The original premise: the dominant residual on `opcost/baseline`
(opt ~4.6x the per-step, §1) is §3.1 — the per-step tier keeps the loop's `s` and
`i` in registers, while the opt lift round-trips every variable through its frame
slot (`FrameLoad slot; <op>; FrameStore slot` per iteration).

`pass::mem2reg` already implements the promotion this needs, including
loop-carried slots (a block-parameter phi on the back edge, `append_edge_arg`),
but the pipeline runs it only on a *callee's* lifted IR inside the trial-inline
resolver (`pass/mod.rs`: "`mem2reg` is not part of this pipeline"). The reason is
soundness: a spliced callee's slots are private, while a top-level body's slots
are observable — a frame-reading helper (`builder_bind`/`builder_store`,
`create_function_decl`, `create_arguments` all go through `Vm::frame_get`) and a
deopt's interpreter re-execution both read them.

### Cuts

**Cut 0 — widen the bit-op and numeric-comparison effects (prerequisite, small).**
`default_effects` gives `Op::BitAnd..UShr` and `Op::Lt..Ge`/`Op::Eq`
`Effects::call()`, so today every bit op and the loop test are slot-observers (the
dumped `opcost/baseline` IR shows `BitAnd ... [segw/segw]`). `narrow` already
proves a bit op numeric when its operands are; extend `rewrite` to widen those
ops — and a comparison whose operands are both numeric — to `pure()`. Sound:
`ToInt32`/`ToUint32` of a Number and a numeric comparison run no user code and
cannot throw. This alone also unblinds `dce`/`cse`/`licm` around bit ops.

**Cut A — run `mem2reg` on the top-level body when no slot-observer or deopt
remains.** After Cut 0 a pure register-loop body (`opcost/baseline`,
`object_alloc`) has no op reading/writing `Heap::Slots` except the slot accesses
themselves, and no `Op::Check`/`GuardType`/`GuardCallee` (a guard is `pure()` by
effects, so it must be excluded explicitly). There a promoted slot is
unobservable, so plain `mem2reg` is sound and closes the row. Gate: promote only
if the body has no op with `may_read(Heap::Slots) || may_write(Heap::Slots)`
outside `FrameLoad`/`FrameStore`, and no guard; **and** promote only a slot whose
every store is a non-heap value (see the GC trap below).

**Cut B — flush-aware promotion (the general case).** For a body with a
slot-observing op or a deopt, mem2reg must materialize: (a) store every live
promoted slot back to its frame slot before such an op, and start a fresh def
from a reload after it (a barrier splits the promoted region, like a merge);
(b) flush every live promoted slot before a guard's deopt branch, because the
resumed interpreter reads the frame. `emit_guard` already mirrors the operand
stack for deopt; it must also receive the live slot set.

### Result (2026-10-10): Cut 0 + Cut A reverted

Both cuts were implemented, gated (`SLAG_WIDEN=0`, `SLAG_MEM2REG=0`) and
measured in one binary (2x2, isolated, min-of-3; `scratch/ab-2x2.sh`):

| row | none | +widen | +m2r | both |
|---|---|---|---|---|
| `opcost/baseline` | 1.479 | 1.485 | 1.457 | 1.479 |
| `opcost/object_keys` | 74.0 | 74.4 | 68.9 | 69.2 |
| `control/nested_loops` | 28.9 | 29.6 | 30.4 | 28.8 |
| `opcost/element_read` | 0.878 | 0.840 | 0.856 | 0.831 |
| `objects/compound_assign` | 68.0 | 67.5 | 68.1 | 68.8 |
| `calls/construct_churn` | 122.2 | 121.7 | 123.6 | 126.5 |

**Cut 0 (effect widening) moves nothing** — the `+widen` column equals `none` on
every row. The bytecode compiler's own `| 0`/`& -1` normalizations and the
surrounding ops mean a widened bit op is not on a path the optimizer can
recompute or hoist usefully. **Cut A (mem2reg) wins `object_keys` ~7%** (a
member-heavy loop whose slot traffic is real) and `element_read` ~4%, but is a
wash or a touch worse on `nested_loops`/`construct_churn`. Net ~zero for a new
pipeline pass (which adds a dominator computation to every lifted body), so both
are reverted — this note is the only artifact; the code is back at `d93eafb`.

**§3.1 refuted.** The `opcost/baseline` gap is NOT the frame-slot round trip.
Promoting `s`/`i` to SSA block params (`JIT_DUMP_IR=1` confirms the loop loses
every `FrameLoad`/`FrameStore` and every op is `[-/-]`) leaves the time at 1.44ms
vs 1.49ms with the slots. The gap is the *lowering*: `call_int_binary`
round-trips every value through f64 (`bitcast` -> `fcvt_to_sint_sat` -> i32 op ->
`sextend`/`fcvt_from_sint` -> `bitcast`) with a tag check, a range check and a
branch to `BinarySlow` per bit op, where the per-step register lane folds the
whole `k = i & MASK; s = (s + k) | 0` into ~3 i32 instructions in an i32 register.
Matching that needs an **i32 value representation** in the opt IR (an `Int` value
is i32 bits, not an f64 bit pattern), not the slot promotion.

**Redirect:** the next lever is the int-op lowering's representation (§6.3/§6.4
re-read in this light) — and the slot promotion is at most a prerequisite for it,
never a win alone.

### Measurement

`scratch/corpus-row-ab.sh opcost/baseline opcost/object_alloc` (isolated,
min-of-3) is the target; Cut 0 is judged on the family sum and must not regress
`objects/own_read`/`globals/*`. A promoted body is confirmed by `JIT_DUMP_IR=1`
losing its `FrameLoad`/`FrameStore` pairs. Gates: clippy, workspace tests, both
sweeps (effect widening changes `cse`/`dce` behavior), the corpus differential,
GC-stress on `baseline`.

### Traps

- **A frame slot is a GC root — promoting a heap value unroots it.** The
  compiled frame is traced (conservatively, per the GC design), so a slot is the
  only reference to a young box it holds; promoting that slot to an untraced
  Cranelift register lets the collector free the box (the barrier-UAF/`EnvRecord`
  class). Cut A must therefore restrict promotion to slots whose every store is
  non-heap (`Number`/`Int`/`Bool`/`Undefined`/`Null` — `narrow`'s
  `slot_numeric`/`slot_int` give exactly this for the numeric rows); the general
  Cut B case must root the promoted value by another means before a collection.
- **A guard is `pure()` by effects but deopts.** Excluding it from the
  "no slot-observer" gate must be explicit.
- **`Op::Eq` is loose `==`.** Widening it is sound only when both operands are
  provably numeric.
- **A slot-observing call can *write* the slot.** After a barrier the promoted
  value is invalid — reload, don't reuse.
- **The entry block cannot carry a phi** (`verify`), and `mem2reg` already
  refuses a slot whose entry load needs the initial `undefined`; this is why the
  register body's `Const undefined` accumulator seed is not a candidate.
