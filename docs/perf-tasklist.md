# Perf tasklist (merged from performance-plan.md + gap-close-plan.md, 2026-09-04)

> Merged, prioritized view of the remaining work in the active plan
> (`docs/performance-plan.md`, mechanism-based, supersedes) and the closed
> historical plan (`docs/gap-close-plan.md`). Status reflects everything
> landed through `0d70d3e` (the L1c record-discipline landing), the
> write-cell capacity slice (1.3), and the certification-coverage slice
> (2.2): the gap-close milestones M1-M7/M10, the L1a/L1c register-path
> work, the typed-array no-alloc reads, the `UpdateReg`/`JumpIfEqImm`/
> `BinStoreReg` slices, and the GC fixes. Only remaining work is listed.
> One experiment at a time; a lever opens with its probe; every landing
> gates on clippy clean, workspace tests green, and the three release
> sweeps.

## P0 — Correctness (JIT)

| # | Item | Status | Evidence / first action |
|---|---|---|---|
| 0.1 | JIT `Float16Array`/typed-array miscompile: compiled `makeArrayLike`-style loops read all-`NaN` from some iteration onward, then segfault (~200-fixture crash cluster, JIT-only, `--jitless` clean) | open; Linux-only; being debugged | Pre-existing at `d58caea`; not GC (reproduces with collections disabled). Lives in `crates/jit` lowering. Unblocks clean Linux JIT built-ins and the `-p jit` release test binary. |

## P1 — Structural property machinery (L1c → L2)

| # | Item | Status | Evidence / first action |
|---|---|---|---|
| 1.1 | L1c read/write end-state on maps/shapes: hot member paths serve via shape-compare + inline-field access instead of the generation/id/name value-cell probes; exotic receivers/accessors/index keys fall back to the exact machinery | partial; stones 1-3 (record discipline) and 1.3 (write-cell capacity) LANDED; the READ-end-state premise was probed and FALSIFIED on the interpreter (perf.md, 2026-09-04) | **Stones 1-3 (LANDED, 2026-09-04)**: the warm member write stopped bumping the generation (`write_data_property_slot`) after converting the three generation-stamped VALUE caches to the L1c oracle pattern (`construct_this_object` reads `prototype` via the shared member value cell; `member_chain_cells` cache the resolution and re-read live; the for-of verdict oracles AIP's `next`). **1.3 (LANDED, this tree)**: the L1a store cells moved to a SEPARATE 256-entry table (`MEMBER_WRITE_CELLS`) — see the 1.3 row. **Read probe (FALSIFIED the per-site read premise)**: warm register member reads are ~3.5ns and read 64 distinct same-map objects at ~4.2ns (+0.7ns — the map-cell layer absorbs value-cell misses at near-warm cost); a 16->256 `MEMBER_CELLS` experiment did NOT speed the cycling-object rows and bloated the Agent's inline tables ~25% slower on every warm row. So the interpreter read path is near its floor; per-site read ICs are NOT the next slice. **Next**: the write end-state — see 1.3's follow-on (per-site store cells only if >256-object working sets show up in a probe). |
| 1.2 | L2 per-site feedback: per-call-site IC entries (shape/offset) shared by the interpreter and the JIT, replacing the global direct-mapped tables | re-scoped by the 2026-09-04 probes | The interpreter read path does not need per-site ICs (1.1's probe: reads ~3.5-4.2ns across 64-object working sets). The remaining per-site argument is the WRITE side beyond the 256-entry capacity (1.3) and the JIT's compiled shape-compare end-state. Defer full L2 until a >256-object store-loop probe shows the capacity ceiling, or the JIT work needs the shape/offset representation. |
| 1.3 | L1a store cells on a separate, larger table (`MEMBER_WRITE_CELLS`) | **landed 2026-09-04** (this tree) | The 16-entry write cells alias across any >16-object store loop; a store-cell miss falls back to the full [[Set]] (~140ns). Interleaved A/B (parent `0d70d3e` + probe rows vs this tree): a 64-distinct-object cycling-store row (32M stores) drops ~4.4-4.5s -> ~0.42s (~10x, ~140ns -> ~13ns/store); the warm rows moved within the cross-build layout band (`arithmetic` — no property cells — moved ~±25%, recorded as noise). Read cells stay at 16 (1.1's probe); the store probe/record now index by `MEMBER_WRITE_CELLS` while the value-cell front keeps the read table's mask. Gates: clippy, workspace tests (new `warm_stores_across_many_distinct_objects_keep_separate_cells`), three sweeps at baseline. Follow-on: per-site store ICs if a probe finds >256-object hot store loops (the JIT's compiled stores are separate). |

## P2 — JIT coverage (L3)

| # | Item | Status | Evidence / first action |
|---|---|---|---|
| 2.1 | Compile the general path (Sparkplug analog): emit every step in machine code for bodies the scope gate excludes (try/catch, `with`, captured closures, complex-object methods), routing env/handler steps through the shared machinery | not started | Those bodies run interpreted forever today. Probe first: (a) fraction of a realistic hot corpus blocked by the scope gate; (b) dispatch share of an uncertified hot loop. Widest JIT-coverage lever. |
| 2.2 | Certification over-rejection: nested NON-ARROW functions' own `this`/`arguments` bailed the enclosing body's scope certification | **landed 2026-09-04** (this tree) | The closure walker (`closure_*_allows`) now threads an `own` flag: entering a nested non-arrow function sets it (its `this`/`arguments` are its OWN, bound at its own call); arrows propagate the caller's flag (an arrow in the analyzed body still observes its lexical `this` and bails). `super`/`class`/private/tagged/import stays rejected under `own`. Probe (perf.md, 2026-09-04): the construct-churn loop function-wrapped with a nested `function C(x){ this.x = x; }` ran ~117-129ms vs ~19-21ms with C a global; after the fix the nested form matches the control (~6x), because the body re-certifies and its `var`s leave the env path. Gates: clippy, workspace tests (new `nested_function_own_this_and_arguments_keep_the_body_certified`), three sweeps at baseline. **Next**: this narrows the 2.1 scope gate on the nested-constructor/helper shapes; the full general-path (try/catch bodies) probe is still 2.1. |

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
   is at its 4-dispatch floor. 1.3 (write-cell capacity) is LANDED on this
   tree.
3. The read-end-state premise (per-site member reads) was probed and
   FALSIFIED (1.1): interpreter member reads are ~3.5-4.2ns across
   64-object working sets — the map-cell layer already absorbs the
   value-cell misses. 2.2 (certification over-rejection) is LANDED.
   The next candidates: (a) the write-side follow-on probe (does a
   realistic >256-object hot store loop exist, justifying per-site store
   ICs / full L2); (b) 2.1's scope-gate probe on the remaining uncertified
   hot shapes (try/catch bodies) to size the general-path JIT work.
4. 3.1 (L4) and 4.1 (L5) after the L1c/L2/L3 probes re-derive their targets.
