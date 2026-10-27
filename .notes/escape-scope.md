# Scoping the next escape-analysis slice: a non-escaping object's mutable field

**Status: landed 2026-10-27.** Measured: `objects/warm_store` 34.7 -> 9.3 ms
(-73%), `objects/compound_assign` 43.9 -> 28.1 (-36%), `objects/destructure`
3.6 -> 2.6 (-28%), corpus -1.85%; `.notes/perf.md` has the entry. The probe
below is the pre-implementation record, kept because it names the shape and the
falsifier. The pipeline also re-runs `narrow`/`mem2reg` after `scalar`, without
which the elided object leaves a guard-poisoned `Unknown` accumulator as a
generic tagged `Add`; that re-type is where most of the win is.

## What already exists

`opt/pass/scalar.rs` replaces a `NewObject`'s named reads with the SSA value the
literal stored, and a `NewArray`'s constant in-range element reads with the
literal's elements. It is on by default and `SLAG_SCALAR=0` is the same-binary
seam. Measured over the 77-row corpus, 5 interleaved rounds, per-row minima:

| row | scalar off | on | |
| --- | ---: | ---: | ---: |
| corpus total | 2118.4 ms | 1995.2 ms | **-5.81%** |
| `objects/destructure` | 120.5 | 3.6 | **-97.0%** |
| `opcost/object_alloc` | 0.0 | 0.0 | flat |
| `opcost/array_alloc` | 0.1 | 0.1 | flat |
| `calls/construct_churn` | 54.7 | 55.1 | +0.65% |
| `opcost/array_slice` | 20.3 | 20.4 | +0.37% |
| `objects/warm_store` | 34.3 | 34.4 | +0.06% |

So the pass's entire corpus value is `destructure`; it does not reach the other
allocation rows. That is the frontier this note scopes.

## The probe: `objects/warm_store`

`var o = { a: 0, b: 0, c: 0, d: 0 }; for (i < 4e6) { o.d = i; s += o.d; }`

`JIT_DUMP_IR=1` on the row, the relevant instructions (block `b0` is the
preheader, `b1` the loop body):

```
b0:
  v0:num  = Const() 0
  v4:obj  = NewObject(v0, v0, v0, v0) 4
  FrameStore(v4) slot0          # o = <literal>
  ...
b1(v24):
  v8:?    = FrameLoad() slot0    # o   (store receiver)
  v10:?   = FrameLoad() slot0    # o   (read receiver)
  MemberStore(v8, v24) atom646   # o.d = i
  v11:?   = MemberCellLoad(v10) atom646
  v12:?   = MemberGuard(v10, v11) atom646
  v14:?   = Add(v13, v12)        # s += o.d
```

`v4` reaches only a `FrameStore` into a private slot, two `FrameLoad`s of that
slot, and `MemberStore`/`MemberCellLoad`/`MemberGuard` pairs on `atom646`. It is
never a call argument, a return operand, or stored into another object, so the
escape lattice already proves it non-escaping. The pass still leaves it on the
heap for two reasons:

1. **The slot forwarding is block-local.** The `FrameStore slot0` that defines
   the object is in `b0`; every `FrameLoad slot0` that could alias it is in
   `b1`. The pass forwards a load only within the block that stored the slot.
2. **The field is mutable.** The pass replaces a field read with the literal's
   *constant* value; `atom646` (`d`) is written by `MemberStore(v8, v24)` before
   the read, so its live value is `v24`, not the literal's `v0`. The pass has no
   store-to-load forward for a mutable member, so it declines the object.

Node scalar-evolves the whole thing to `s += i` (2.4 ms against Slag's 34.4),
which is the 15x gap on this row.

## The slice

Extend `scalar.rs` in three exact, guard-free parts:

- **Dominating cross-block slot forwarding.** A `FrameLoad slot` aliases the
  object of a `FrameStore slot = obj` when that store dominates the load, the
  slot is not written on any path between them (the existing store-reach rule,
  extended from block-local to the dominator tree), and `obj` is a virtual
  allocation by the same criterion the pass already uses. A slot stored more
  than once (a loop-carried redefinition) bails.
- **Store-to-load forwarding for a virtual member.** A `MemberCellLoad(obj,
  atom)` whose nearest preceding definition of `obj[atom]` is a
  `MemberStore(obj, atom, v)` with no intervening `Effects::call()` and no
  other store/delete on that atom is `v`; the paired `MemberGuard` is removed
  with it (the object is elided, so there is no shape to check). The effect
  sets already carry the "can clobber" distinction, so the check is a walk of
  the block plus the dominator path.
- **Dead-object removal.** When every member access on a virtual allocation is
  replaced, the `NewObject`, its `FrameStore`, and the aliasing `FrameLoad`s are
  dead. `DCE` keeps `NewObject` (its effects are not pure), so the pass removes
  it explicitly, as it already does for the constant-field case.

### Soundness

All three are exact properties of the lifted IR (dominance plus the effect
sets), never a runtime guess, which is the invariant `optimizing-tier-impl.md`
section 2 requires for the tier: no `Recover_*`, no speculative materialization.
A store-to-load forward is the ordinary rule for a non-escaping object with no
intervening clobber; the escape lattice already establishes that no call can
retain the object, so no getter or proxy can observe the field between the
store and the load.

### What it reaches, and what it does not

- `objects/warm_store` (34.4 ms, 15x) is the direct target: the object has one
  mutable field read in the same block as its store.
- Any loop that mutates a local literal's fields (`for (...) { o.a = f(i); s +=
  o.a }`) shares the shape.
- `calls/construct_churn` (55 ms, 5x) is **not** reached: the instance is a
  `ConstructArgs` op whose constructor and method run in separate bodies, so
  `this` is never a `NewObject` in the calling body. It needs the constructor and
  the method inlined first (I5), then this slice applies to the inlined `this`.
- `opcost/array_slice` (20 ms, 20x) is **not** reached: the result array is
  consumed by `.length`, which is a builtin call, so eliding it needs
  `Array.prototype.slice` inlined into the body first (stage B). The scalar pass
  already handles the shape once the builtin is in the body (the isolated
  `var a = [i, i+1, i+2]; s += a[0]+a[1]+a[2]` row is 24x).
- `builtins/object_keys`-style rows are **not** reached: the keys array is
  returned, so it escapes the body (I6d, heap-valued fields, does not change
  that).

### Probe and gate

The instrument is already in place: the `SLAG_SCALAR=0` seam (feature cost) and
`JIT_DUMP_IR` (the op shape). The prediction is that `objects/warm_store`'s IR
loses its `NewObject`, both `FrameLoad`s, the `MemberStore`, the
`MemberCellLoad` and the `MemberGuard`, and its jit ms falls from ~34 toward the
`opcost/baseline` floor. The falsifier: if `warm_store` does not fall below about
10 ms, or any row regresses on the jit-loss gate, the slice is a NULL and is
recorded rather than kept. `--gc-stress` and `--nursery-stress` must be clean,
because an elided object's surviving heap-valued fields are the roots the
analysis must not have dropped (that is I6d; this slice's fields are scalars).

### Traps

- A cross-block slot forward must prove the slot is not re-stored on any path to
  the load; the loop in `warm_store` does not re-store `o`, but a body that
  reassigns `o` each iteration must bail.
- A store-to-load forward must treat a `MemberGuard`'s fallback (it has
  `Effects::call()`) as a clobber of everything, so a forward may not cross one.
- The removed `MemberGuard` carries the read's deopt/slow-path behaviour; the
  object being elided is what makes that safe, so the forward must be gated on
  the object being virtual, not just on the store dominating.
- An elision that grows the body is not a concern here (it shrinks), but
  measure the step count as the pass lands, per I6's standing trap.
