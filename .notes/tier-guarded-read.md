# The guarded, hoisted member read

**Status: plan (2026-10-07).** The `Op::Check` guard + resume is landed
(`.notes/tier-resume-fidelity.md`); this is the transform that consumes it. It is
a multi-slice feature and has one load-bearing open decision (below), so the
design is pinned before code.

## Goal

Make a monomorphic member read **hoistable**, so a loop that reads `o.x` every
iteration pays O(1) instead of one `Helper::GetMemberName` call per iteration.
The `read` row (`s += o.x`) is currently only parity; the whole-body win is the
hoist (N reads → 1), not a cheaper single read.

## Why the read can't be hoisted today

`Op::MemberLoad` carries `Effects::call()` (a getter, a proxy trap, or a throwing
receiver can run user code), so neither CSE nor LICM may move it, and the lowered
read is an opaque helper call. The tier's structural advantage (no per-step
dispatch) is fully consumed by that call.

## The key simplification: the premise is the member cell, not feedback

A guard's *correctness* premise needs no type feedback. The read-side cache is
`MemberValueCell { id, name, generation, value }` (`ir.rs:2716`), indexed
`member_cell_index(object_id, name) = (object_id ^ name) & (MEMBER_CELLS-1)`
(`ir.rs:5013`). The validation is `cell.id == o.id && cell.name == atom &&
cell.generation == o.generation`. A valid cell **proves** `o.x` is a *data
property* (only data-property reads warm the cell) whose cached `value` is
current: the in-place store paths refresh the cell, and every structural change
(define, delete, accessor conversion, map transition) bumps
`JsObject::generation`, which fails the check. So a guarded cell read handles the
getter/proxy/throw cases by **deopting** rather than by knowing them statically.

Feedback's role is therefore only the *decision*: a guarded read deopts on a cell
miss, so guarding a polymorphic site thrashes. The `MemberReadSite` /
`ICState` megamorphic valve (`feedback.rs`) is the intended throttle — but it is
`SLAG_FEEDBACK`-gated (off by default, `feedback.rs:177`), which is the open
decision.

## The mechanism

- **`Heap::Members`** — a new region. `MemberLoadCell` reads it; every
  `MemberStore`/`ElementStore` and every `Call` writes it (a call can mutate any
  object). This is what stops LICM hoisting a read across a loop-local store the
  transform cannot otherwise see.
- **`Op::MemberLoadCell { atom, step }`**, args = `[object]` — a *guarded* cell
  read. Lowering: check the object tag, compute the cell index, validate
  `id`/`name`/`generation`; on a miss (or a non-object receiver) mirror the stack
  and `return DISPATCH_DEOPT` (the landed resume); on a hit return the cell's
  `value`. Effects `read(Heap::Members)`; it **exits, never traps**, so it is
  hoistable (running it in a zero-trip loop's preheader deopts unnecessarily but
  is not observable).
- **LICM** hoists a `MemberLoadCell` when the receiver is loop-invariant and the
  loop has no `Heap::Members` write and no call.
- The lowerer reuses the per-step `emit_member_cell_probe` shape; this is the one
  place the "do not fork the per-step lowerer" rule bites — factor the cell probe
  so both lowerers emit the identical sequence, rather than duplicating it.

## Sub-slices

| id | content | gate |
|----|---------|------|
| G1 | `Heap::Members` + the write-side effects (Member/ElementStore, Call) | behavior-neutral; workspace green |
| G2 | `Op::MemberLoadCell` + lowering (guard → deopt) + a diagnostic trigger, so **measure** whether the hoist pays on `read_loop` | a *negative result is valid*: if the hoist does not pay, stop and record it |
| G3 | LICM: hoist a guarded member read (invariant receiver, Members-clean loop) | `read_loop` / `obj_prop` measured A/B |
| G4 | the decision gate (feedback valve, or a cheaper heuristic) so a polymorphic site is not guarded | no deopt thrash on the corpus |
| G5 | retirement: bind the body to the cell generation, retire on invalidation | premise-invalidation test |

## Scoping findings (2026-10-07, before G2)

**G1 landed.** `Heap::Members` is a region now; `MemberStore`/`ElementStore` and
`Effects::call` all clobber a `Members` read (`call` was widened to `0b1_1111`, or
a call would not invalidate a hoisted read), with a clobber test. Behavior-
neutral; the jit suite is green.

Building the measurement exposed two prerequisites that make G2 larger than "one
op":

1. **Single-sourcing the cell probe.** The guard's lowering must emit the *same*
   cell probe the per-step path emits (plan §6's "do not fork"); that probe is
   `Compiler::emit_member_cell_probe`, ~370 lines coupled to the per-step
   compiler's state. It must be factored into a shared emitter before the tier
   can call it — a behavior-neutral refactor that is itself a slice.
2. **The hoist's soundness.** The N→1 win needs the *cell value load* hoisted
   while the *validity check* stays in the loop. Two designs:
   - **Deopt (a hoisted `Check`).** A guard hoisted to the preheader deopts
     *before the loop runs*, so its resume step must be the **loop entry**, and
     the IR carries no block step indices (the lift knows them, the IR does
     not). Needs `Block::start_step` plumbing before it is sound.
   - **Deopt-free (preferred).** Hoist a *speculative cell value load* whose
     address is computed behind a hoisted object-tag check, and keep the full
     validity check (`id`/`name`/`generation`) **in the loop** with a
     `Helper::GetMemberName` fallback. No deopt, no resume-step plumbing, and a
     getter site simply always takes the helper — no thrash, so the megamorphic
     valve stops being load-bearing for the throttle. The "guard" is a cheap
     in-loop compare, not a `Check`; this resolves open decision #2 in favour of
     the non-`Check` shape for reads.

**Revised first sub-slices:** G2a — the shared cell-probe emitter (refactor,
behavior-neutral); G2b — `Op::MemberCellLoad`, a speculative load that hoists
behind the in-loop validity guard (the deopt-free design); then G3 (LICM hoists
the load). The `Check` guard mechanism stays the tool for premises that genuinely
need a per-activation exit (a polymorphic shape), not for this read.

## Measurement

`scratch/tier-ab/wl/read_loop.js` (`s += o.x`, 5M iters) is the row: currently
parity (`SLAG_OPT` 0 vs 1). The target is the hoist — the per-iteration read
becomes a single load before the loop. Also watch `obj_prop`/`prim_prop` and the
corpus `mean-jitGap` (noisy on this machine — use the interleaved micro-bench).

## Traps

- **An in-place value write refreshes the cell without a generation bump.** So
  the loop's own `o.x = v` must block the hoist — which is exactly why
  `MemberStore` writes `Heap::Members`. Do not "optimize" that away.
- **`get_member_name`'s ABI is a four-file mirror reached through an untyped fn
  pointer** (arity slips are silent) — this matters only for G4/G5 if the
  compiled tier must record the site; the correctness guard needs no ABI change.
- **A hoisted guard can deopt where the interpreter would have continued.** That
  is a resume, not an error, but it makes the megamorphic valve load-bearing for
  G4: without it, a polymorphic site deopts every activation.
- **The resume step for a hoisted read.** The read's original step index is the
  resume point; the op must carry it (`imm`), because the lowering's deopt block
  sets `vm.ip` to it and the interpreter re-executes from there.

## Open decisions

1. **The decision gate (load-bearing).** Guard every member read and rely on the
   valve, or gate on feedback (which then must be default-on, with its
   recording cost)? The first is simpler but risks deopt thrash before the valve
   engages; the second needs `SLAG_FEEDBACK` on by default. Recommend G2 as the
   *measurement* first — if the hoist's win dwarfs the thrash, guard-always wins.
2. **One fused guarded op vs `Check` + a separate cell load.** One fused op is
   simpler (no preheader split, LICM hoists it as a unit); a split matches the
   `Check` the guard mechanism was built around. Recommend the fused op for the
   read, keeping `Check` for future non-read premises.
3. **Where the cell probe lives** so the two lowerers stay single-sourced.
