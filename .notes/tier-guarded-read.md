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

**G2a landed (2026-10-07).** `crates/jit/src/cells.rs` holds the shared
primitives — `object_data_ptr`, `is_plain_object`, `member_value_cell_addr`,
`member_value_cell_valid`, `member_value_cell_value` — and both per-step probes
(the read probe in `emit_member_cell_probe`, the store path in
`emit_validated_member_store`) now call them. Scope note: the tier needs only
these *primitives*, not the whole 370-line method — the length/typed-array/map
machinery is per-step-only, so the refactor is small and behavior-neutral (clippy
clean; `cargo test -p jit` 306 / 0; test262 `language` 23,726 / 0 and
`built-ins` 23,820 / 0 / 1 at baseline — the probe is on the hottest read path,
so a drift would have shown).

## Measurement

`scratch/tier-ab/wl/read_loop.js` (`s += o.x`, 5M iters) is the row. The plain
helper made the tier 1.586x slower; the inline read (G2b) is 0.991x. The hoist
target — the per-iteration read becoming a single load before the loop — is a
register-pressure regression (G2b) and is not landed. Also watch
`obj_prop`/`prim_prop` and the corpus `mean-jitGap` (noisy on this machine — use
the interleaved micro-bench).

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

## G2b: the inline read landed; the hoist is the regression (2026-10-07)

**Landed — the inline member read.** `Op::MemberCellLoad` + `Op::MemberGuard`
(the guard compares `cell.value` to the speculative load, so an in-place write
fails it) replace the tier's `get_member_name` helper call with the same inline
cell probe the per-step path has. On `read_loop` (`s += o.x`, 5M iters, min-of-5
interleaved, `SLAG_OPT` 0 vs 1):

| tier's read | tier/per-step |
|---|---|
| `get_member_name` helper (before) | **1.586x slower** |
| inline cell probe (this change) | **0.991x** |

The helper was a standing tier regression the read row had hidden — the per-step
path already inlines its read (`emit_member_cell_probe`), so the tier paid a
helper call per iteration where the interpreter paid none. The pair removes it: a
~1.6x improvement for the tier on that row, and no regression elsewhere (`licm`
0.727x, `arith` 0.731x, `for` 0.896x, `lcg` 0.922x; all tier-faster).

**Not landed — the hoist.** Hoisting the cell load out of the loop (LICM) made
`read_loop` **~1.8x slower**, reproducibly (per-step 35.6 ms vs tier 63.8 ms, and
34.5 vs 62.4 on a repeat). `JIT_DUMP_CLIF` shows the emitted code is the same
*size* (578 vs 584 CLIF lines; 295 vs 301 disasm lines) but **not the same
register allocation** — the hoisted build saves more callee-saved registers and
differs in ~112 real instructions. The cause is **register pressure**: hoisting
makes the cell value live across the whole loop, and the tier's loop is
register-tight (every `+`/`<` expands to a tag-check + `select` canonicalize
sequence), so one extra loop-live value degrades the allocation enough to cost
~1.8x. `Op::MemberCellLoad` is therefore **absent from LICM's hoistable set** — the
plan's "N-reads-to-1" lever does not hold up for this code, though the read was
well worth *inlining*. Fixing the register pressure is a separate project over the
tier's arithmetic sequences.

## A real LICM bug, found en route (fixed)

Chasing the hoist exposed that `pass/licm.rs`'s `natural_loop` **never added the
back-edge source block** — it added only the source's predecessors. So for any
loop whose back edge's source is a separate block (`entry -> pre -> header ->
body -> header`, an ordinary `for`/`while`), `body` was not in the loop, the
header's outside predecessors were `{preheader, body}` (no unique preheader), and
**nothing hoisted at all**. Only self-loops (a back edge to the header itself)
were ever optimized. The fix adds the source and walks up through the
header-dominated predecessors; pinned by `hoists_from_a_non_self_loop`.

Measured (min-of-5 interleaved, `SLAG_OPT` 0 vs 1): `licm_loop` **0.879x ->
0.788x** (1.14x -> 1.27x), `for` 0.894x, `arith` 0.744x. Gates: clippy clean;
`cargo test -p jit` 307 / 0; test262 `language` 23,726 / 0, `built-ins`
23,820 / 0 / 1, `annexB` 1,086 / 0 — all at baseline (one `built-ins` run showed
3 hangs under build load and 0 on a quiet re-run: the documented wobble).

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
