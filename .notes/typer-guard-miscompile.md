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

## Update (2026-10-11): the guard's own code is correct; the added CFG perturbs
codegen elsewhere

Dumped the culprit body's CLIF for both `SLAG_TYPER` values from the pinned Deno
(`JIT_DUMP_IR=1 JIT_DUMP_CLIF=1`, `deno run -A scratch/h_only.js`). Exactly one
body reaches the opt tier — the same body the guard changes. It is small: six IR
blocks, `let` slots with `TdzCheck`, two `ElementLoad` reads, a `MemberCellLoad`,
a `Call`, and five `ElementStore`s. Its CLIF is 242 blocks (the shape-cell
fast paths expand), and a **normalized diff** (block/value ids and constants
rewritten) shows the guard adds **only** its own blocks — nothing else moves:

```
# per guard                                   # guard off
v# = band v#, <TAG_MASK>                        (the store, straight-line)
v# = icmp ne v#, <TAG_PREFIX>                   store.i64 v#, v#+8
brif v#, cont, deopt
d: store.i64 v#, v#            # mirror the value into work+0
   store v#+8, ctx+suspend_sp
   brif vm != 0, s, r
s: store <step>, vm+ip; jump r
r: return -4
cont: store.i64 v#, frame+slot  # the guarded read's store, unchanged
```

Reading the emitted machine code for the same region (`JIT_DUMP_CLIF` prints the
disasm), the guard is correct: `movq %rax, %rcx; andq <mask>, %rcx; cmpq <prefix>,
%rcx; jnz <cont>` with `%rax` the guarded value, and `cont` does `movq %rax,
slot(frame)`. The value the guard checks is the value `cont` stores.

Three toggles pin the trigger (each an env `emit_guard`/`run_jit_body` seam):

- **No deopt fires.** An unconditional print in the `DISPATCH_DEOPT` branch
  (`run_jit_body`) is silent for the header run, so the guard's *check passes* and
  `cont` runs. `vm.ip`/`suspend_sp` are never written; the resume path is not it.
- **Forcing the condition true fixes it.** `ok = iconst 1` (so Cranelift folds the
  `brif` and drops the `deopt` block) makes every call correct. A constant-true
  branch is a semantic no-op given the check already passes — yet it is the only
  thing that turns the wrong answer right.
- **The deopt block's *contents* and the seal timing do not matter.** Replacing the
  deopt block with a bare `return -4` (no mirror, no `vm.ip`) still miscompiles,
  and moving `seal_block(cont)` to `seal_all_blocks` (lazy seal) still
  miscompiles.

So the guard's blocks and its machine code are each correct, no deopt is taken,
and yet the *presence* of the conditional branch is what corrupts the body. The
only variable is Cranelift's layout/register allocation with the extra (live but
never-entered) `deopt`/`cont` blocks. This is **not** a CLIF-level mistake in
`emit_guard` that reading the dump reveals — it is a codegen-level consequence.

Also confirmed to matter (each tried on the pinned build): `SLAG_TYPER=0` fixes
it; `SLAG_GUARD=0`, `SLAG_MEM2REG=0`, `SLAG_WIDEN=0`, `SLAG_INLINE=1` do **not**.
The guard survives `pass/guard.rs`'s pruning because its result feeds a
`FrameStore` (and, for the third guard, a block edge argument) — both of which
that pass keeps unconditionally.

### Next attempts, in order

1. **A self-contained lift→lower unit test.** This is the blocker: the CLI mimics
   do not reproduce, so every experiment costs a ~15-minute Deno rebuild. A test
   that lifts a body of this shape (a guarded read whose result is stored, a
   block-param merge, a `let` slot) and runs the compiled code against the
   interpreter would iterate in seconds. Find the minimal shape that diverges.
2. **Thread the guarded value through `cont` as a block parameter.** The current
   shape relies on the value being live from `cont`'s predecessor's body into the
   sealed single-predecessor `cont`. If the divergence is a liveness/spill bug,
   making the value an explicit `cont` argument (the standard Cranelift phi) is a
   plausible dodge — and a small, local change to `emit_guard` plus its three
   callers.
3. **Tighten `pass/guard.rs`.** A `FrameStore` currently keeps its guard
   unconditionally, but the guard only pays if the *slot* is later read into a
   type-sensitive op. Slot-aware pruning would drop all three guards in this body
   (a real ~1% win the pass already targets) and sidestep the trigger — but it
   does not fix a *paying* guard that hits the same codegen path.
