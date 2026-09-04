# Perf tasklist (merged from performance-plan.md + gap-close-plan.md, 2026-09-04)

> Merged, prioritized view of the remaining work in the active plan
> (`docs/performance-plan.md`, mechanism-based, supersedes) and the closed
> historical plan (`docs/gap-close-plan.md`). Status reflects everything
> landed through `83b7bea` (the `BinStoreReg` local-compound fuse): the gap-
> close milestones M1-M7/M10, the L1a/L1c register-path work, the typed-
> array no-alloc reads, the `UpdateReg`/`JumpIfEqImm`/`BinStoreReg` slices,
> and the GC fixes. Only remaining work is listed. One experiment at a
> time; a lever opens with its probe; every landing gates on clippy clean,
> workspace tests green, and the three release sweeps.

## P0 — Correctness (JIT)

| # | Item | Status | Evidence / first action |
|---|---|---|---|
| 0.1 | JIT `Float16Array`/typed-array miscompile: compiled `makeArrayLike`-style loops read all-`NaN` from some iteration onward, then segfault (~200-fixture crash cluster, JIT-only, `--jitless` clean) | open; Linux-only; being debugged | Pre-existing at `d58caea`; not GC (reproduces with collections disabled). Lives in `crates/jit` lowering. Unblocks clean Linux JIT built-ins and the `-p jit` release test binary. |

## P1 — Structural property machinery (L1c → L2)

| # | Item | Status | Evidence / first action |
|---|---|---|---|
| 1.1 | L1c read/write end-state on maps/shapes: hot member paths serve via shape-compare + inline-field access instead of the generation/id/name value-cell probes; exotic receivers/accessors/index keys fall back to the exact machinery | partial; the record-discipline program (stones 1-3) LANDED 2026-09-04; the remaining read end-state (per-site map validation) is the next slice | **Stones 1-3 (LANDED, one commit, 2026-09-04)**: the interpreter's warm member write stopped bumping the generation (`write_data_property_slot`, the JIT's existing no-bump discipline), after converting the three generation-stamped VALUE caches to the L1c oracle pattern (cache the RESOLUTION, never the value): **stone 1** `construct_this_object` reads `prototype` via the shared (JsObject id, "prototype") member value cell when warm (Cut 26 `construct_prototypes` deleted; a value write to `prototype` can no longer leave a stale construct proto); **stone 2** `member_chain_cells` cache links + the found vector slot and re-read the value live through the found link's member value cell (all-scalar, trace arm gone) so a warm proto-link write is observed; **stone 3** the for-of fast verdict oracles AIP's own `next` through its member value cell, then the bump drops. Every other value write still bumps (`set_key`, defines, deletes, accessor conversion); the construct boilerplate map re-validates the CURRENT proto id, so a warm proto swap cannot reuse a stale shape. Measurement (perf.md, 2026-09-04): ~0.5-1ns/write by within-binary isolation; the row A/B is inconclusive (cross-build layout moves the no-property-write `arithmetic` control ~24% — recorded as measured). Gates: clippy, workspace tests (new `construct_observes_warm_prototype_value_writes`, `chain_reads_observe_warm_value_writes_to_the_found_link`), three sweeps at baseline (the one `spread-sngl-iter.js` failure seen mid-program is a non-reproducible batch flake of the documented heap-state class — full language sweep clean on the landing tree). **Next**: the read end-state — per-site map-validated member reads on the register executor (the map/shape layer exists; the read residual is the in-suite direct-mapped probe), which is the 1.2 entry slice below. |
| 1.2 | L2 per-site feedback: per-call-site IC entries (shape/offset) shared by the interpreter and the JIT, replacing the global direct-mapped tables | not started (entry slice is 1.1's first slice — see the 1.1 row) | The map/shape representation 1.1 was to establish now exists, so the per-site IC machinery (a per-body mutable IC table for the register member ops, interpreter first, then the JIT) is the concrete first slice of the 1.1 read end-state rather than a separate deferred item. |

## P2 — JIT coverage (L3)

| # | Item | Status | Evidence / first action |
|---|---|---|---|
| 2.1 | Compile the general path (Sparkplug analog): emit every step in machine code for bodies the scope gate excludes (try/catch, `with`, captured closures, complex-object methods), routing env/handler steps through the shared machinery | not started | Those bodies run interpreted forever today. Probe first: (a) fraction of a realistic hot corpus blocked by the scope gate; (b) dispatch share of an uncertified hot loop. Widest JIT-coverage lever. |

## P3 — Allocation (L4 / M8 arena)

| # | Item | Status | Evidence / first action |
|---|---|---|---|
| 3.1 | Bump arena for the hot shapes (ropes, fresh ordinary objects), swept by the existing collector | not started | The 2026-09-04 L4 probe falsified the arena for the `buildString shape` row (that row is branchy step-dispatch, not allocation). Rescope per the plan's mandate: count boxes/iteration on the construct-churn and `buildString full` rows first, then build the smallest arena that covers measured need. |

## P4 — Call/apply residual (L5 / M10 slice 2)

| # | Item | Status | Evidence / first action |
|---|---|---|---|
| 4.1 | Inline the `.apply`/`.call` member read on the compiled intrinsic path | not started | gap-close M10 measured jit ~7.0ms after slice 1; residual ~35ns/call is the member read + per-iteration fill. The plan defers design until 2.1 changes what the JIT can reach. |

## P5 — Small interpreter micro-slices (probe first)

| # | Item | Status | Evidence / first action |
|---|---|---|---|
| 5.1 | Drop the per-`if` `ResetCompletion` in certified loop bodies (one fewer dispatch/iteration on branchy bodies) | **landed 2026-09-04** | `buildString shape` ~99-104 -> ~94-97ms interp (~5%); completion battery + three sweeps at baseline (perf.md record). |
| 5.2 | Closed-plan residuals (assessed not-worth in gap-close §5, listed for completeness): M7 slice 2 (second register accumulator), M1-C-deep (machine dense append), M2 65+ args, M3 slice 2, general LICM of `o.a`/`g` reads | closed | No bench row exercises most; revisit only if a probe shows otherwise. |
| 5.3 | Fuse the statement-position local compound into one op when its RHS is in the accumulator (`BinStoreReg` — the `n += i*2`/`s += o.x` tails) | **landed 2026-09-04** (`83b7bea`) | arithmetic interp ~13.2-13.5 -> ~11.3-11.5ms (~15%), compound assign ~12%; JIT flat; three sweeps at baseline (perf.md record). |
| 5.4 | Direct-operand local compounds (`n += 1`, `s += t`) fused into one fat op | **closed by measurement 2026-09-04 (REVERTED)** | Interleaved A/B vs `83b7bea`: arithmetic +~1.2ms, bare loop +~0.4ms regression — the per-op match dispatch is cheaper than a fat arm with operand branches + a cold tail. The direct-right shapes stay three ops (perf.md record). |

## Recommended order

1. 0.1 in parallel (the Linux debug agent owns it) — a correctness blocker.
2. 5.1-5.4 are LANDED/CLOSED (2026-09-04): the register-run local-compound
   arc ended at the `BinStoreReg` fuse (5.3); the direct-right fat-op
   generalization measured as a regression (5.4) and the branchy micro-arc
   is at its 4-dispatch floor. The next slice is 1.1's first slice: a
   per-site map-validated member read on the register executor (the shape
   layer is in place; the read residual is in-suite direct-mapped thrash).
3. Then the plan's own sequencing: the remaining 1.1 work (the read
   end-state) is the per-site map-validated member read — the 1.2 entry
   slice, ahead of 2.1 (L3) unless a probe changes the calculus.
4. 3.1 (L4) and 4.1 (L5) after the L1c/L2/L3 probes re-derive their targets.
