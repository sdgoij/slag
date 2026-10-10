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
3. **Int-op range guards on provable operands (~15-20%).** `call_int_binary`
   always emits the `|x| < 2^63` guard; for an operand whose IR type is `Int`
   (a `| 0` / bit-op result, an int32) or an in-range constant the guard is
   unnecessary. Needs (a) the typer to prove an `Int` slot (today `narrow`
   proves only Number, so a slot stored `k = i & MASK` reads back `Unknown`) and
   (b) `call_int_binary` to take the operand proven-ness, as
   `call_numeric_binary` already does.
4. **Boolean materialization for a comparison terminator (~10%).** `Op::Lt`
   builds a canonical Boolean `Value`, and `Term::Branch` compares it against
   the NaN-boxed `false` bits; the register lane branches straight on the
   `fcmp`. An `Lt`-feeding-a-`Branch` fusion would remove it.
5. **The compiler's own `| 0` / `& -1` normalization ops.** Both tiers carry
   these; not an opt-vs-per-step difference.

## 4. `construct_churn` is a *different* item — the prototype-read IC

`s += new Item(i).sum()` reads `.sum` off a **fresh** object every iteration.
The opt tier's member read is a per-object cell keyed `(object.id, atom)`
(`Op::MemberCellLoad`/`MemberGuard`); a fresh object's id is never in the cell,
so the guard **misses every iteration** and calls `get_member_name`. The method
lives on `Item.prototype`, so the read is a prototype-chain lookup — the same
gap `.notes/perf-findings.md` records (`proto_read` 20.5ns/read vs SM 2.4, "a
chain-caching IC that keys the lookup on the holder's shape"). Fixing it is a
shape-keyed prototype IC, not a loop-lane change.

## 5. `many_objects_read` is (mostly) inherent

`os[i & 1023].w` cycles 1024 objects past the 16-entry direct-mapped member
cell, so the guard misses by construction; the per-step tier is faster because
its fused-loop register lane does the *element* read inline. The residual is
§3's lane, plus a cell-size/collision question (`MEMBER_CELLS`).

## 6. Proposed slices (probe-first, ordered by value)

1. **`Int`-proven operands + `call_int_binary` guard elision** (§3.3). Bounded,
   sound (an int32 cannot saturate the f64→i64 conversion), and it helps the 36
   `opcost/*` rows — the largest family. Extend `narrow`'s lattice with an `Int`
   flag (a slot whose every store is `Type::Int`) and skip `trunc_i32`'s range
   check for an `Int`/in-range-constant operand. Expected ~1.2-1.4x on
   `baseline`.
2. **`canon_double` sinking for `Int`-typed results** (§3.2, narrow case). An
   int32's f64 is never a NaN, so an `Int`-resulting op needs no canon at all —
   the cheapest cut of the canon cost, no escape analysis required.
3. **Frame-slot promotion with deopt materialization** (§3.1). The largest
   structural slice; needs the flush-point design (deopt ops + `Heap::Slots`
   effects). Do after 1-2 so the register-allocation win is measurable alone.
4. **`Lt`→`Branch` fusion** (§3.4). Small, removes the bool round trip.
5. **The prototype-read IC** (§4). Separate arc; the largest absolute row.

Two slices (1, 2) are contained enough to land independently; 3 is the arc-scale
one. Anything here must be judged on the isolated rows above, never the
whole-corpus sweep.

## 7. Measurement discipline

- One row per process (`scratch/corpus-row-ab.sh <row>`), min-of-3, interleaved
  `SLAG_OPT=0` vs default.
- A row is "fixed" only when `opt/per` is within noise of 1.0; a cluster fix is
  judged on the family sum, not one row.
- `--jit-bench`'s `arithmetic`, `bare loop`, `property read`, `typed-array *`
  are the companion micros.
