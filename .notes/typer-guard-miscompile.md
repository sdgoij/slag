# The typer read guard miscompiles Deno's header path (no deopt)

**Status: guard disabled by default (2026-10-10).** The optimizing tier's
typer read guard (`SLAG_TYPER`, on by default since `eaaacae4`) miscompiles
Deno's `Headers`/`Response` construction: a `Headers` built from a record keeps
no entries once its body is compiled, so `response.headers.get("etag")` answers
`null`.

## Symptom

The Deno embedding CI job fails on every platform:

```
FAIL Request/Response/Headers/AbortController: headers = null want 1
```

at `new Response("abc", { headers: { etag: "1" } }).headers.get("etag")`.

## Reproduction

Rebuilt the pinned Deno (`tools/build-deno.py`, rev `abd22074e4`) against the
tree; it reproduces deterministically. Minimised (fresh process):

- `new Headers({ etag: "1" })` — the **first** call is right; **every later**
  call loses the entry (`get` → `null`). Unrolled pairs are fine; a loop fails
  from the 2nd iteration. Instrumenting the JS masks it (a body-shape-sensitive
  miscompile), so the probes below are engine-side or env toggles only.

## Isolation (what it is *not*)

- `SLAG_OPT=0` → correct (no optimizing tier). `SLAG_TYPER=0` → correct (tier
  on, guard off). So it is the typer read guard, `lift`'s `guard_read_value` →
  `Op::GuardType`.
- **It does not deopt.** `SLAG_JIT_DEOPT_TRACE=1` prints nothing for the header
  run (it does print for `SLAG_JIT_DEOPT_PROBE=1`), so the guard's check is not
  failing — the compiled body computes the wrong answer. This is **not** the
  deopt-resume path.
- **Not the numeric-slot retype.** A `SLAG_SLOTTYPE=0` seam that skips
  `narrow`'s `FrameLoad`→`Number` retype does not change the failure.
- **Not the guard's effect modelling.** Emitting the guard `Effects::call()`
  instead of `pure()` does not fix it.
- **Not `9fae69f4` and not the int-rep arc.** It reproduces at `d612ce0c`,
  `b94f48c1`, and (minimised) since at least the guard's own introduction
  (`bb5c0998`, Oct 7) — it predates the shape cells and the i32 representation.

## The IR evidence

`JIT_DUMP_IR=1` (the single optimizing-tier body compiled in the minimal run)
shows the guard adds one `GuardType` **and** (via `narrow`) retypes a slot load:

```
# guard on                                   # guard off
v3:?  = ElementLoad(v1, v2) [segw/segw]      v3:? = ElementLoad(v1, v2) [segw/segw]
v4:num = GuardType(v3, v3) 3 [-/-]           FrameStore(v3) slot1 [-/s]
FrameStore(v4) slot1 [-/s]
...
v20:num = FrameLoad() slot2 [s/-]            v17:? = FrameLoad() slot2 [s/-]
v21:?  = MemberCellLoad(v20) atom5413        v18:? = MemberCellLoad(v17) atom5413
v22:?  = MemberGuard(v20, v21) atom5413      v19:? = MemberGuard(v17, v18) atom5413
v23:num = GuardType(v22, v22) 26 [-/-]
```

The IR itself verifies; the difference is the `GuardType` presence. Since the
guard never fires and the retype/effects are ruled out, the miscompile is in the
guard's **Cranelift lowering** — `emit_guard`'s `deopt`/`cont` block wiring (the
success path splits every guarded read across extra blocks) — or its interaction
with the surrounding value/phi handling, for a body of this shape.

## Fix

`typer_reads_enabled()` is back to **off by default** (`SLAG_TYPER=1` opts in) —
it reverts only `eaaacae4`'s default flip, one line to restore once the lowering
bug is fixed. A guard that silently miscalculates a real program cannot stay on
by default. The guard's code path is still exercised by the
`opt_tier_guarded_read_*` / `opt_tier_int_value_*` tests (they force `OPT_TYPER`).

## Next step

Dump the culprit body's CLIF (`JIT_DUMP_CLIF=1`) guard-on vs guard-off and
compare `emit_guard`'s blocks — the success path returns the operand unchanged,
so any divergence there (a mis-sealed `cont`, a dropped value across the split,
a phi whose edge args lose a value) is the bug. A minimal lift→lower unit test
with a `GuardType` before a block-branching consumer should reproduce it without
Deno.
