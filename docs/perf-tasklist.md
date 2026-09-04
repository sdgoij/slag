# Perf tasklist (merged from performance-plan.md + gap-close-plan.md, 2026-09-04)

> Merged, prioritized view of the remaining work in the active plan
> (`docs/performance-plan.md`, mechanism-based, supersedes) and the closed
> historical plan (`docs/gap-close-plan.md`). Status reflects everything
> landed through `b4a62e5` (the empty-map-cache GC fix): the gap-close
> milestones M1-M7/M10, the L1a/L1c register-path work, the typed-array
> no-alloc reads, the statement-update (`UpdateReg`) and strict-eq
> conditional (`JumpIfEqImm`/`JumpIfNeqImm`) slices, and the GC fixes.
> Only remaining work is listed. One experiment at a time; a lever opens
> with its probe; every landing gates on clippy clean, workspace tests
> green, and the three release sweeps.

## P0 — Correctness (JIT)

| # | Item | Status | Evidence / first action |
|---|---|---|---|
| 0.1 | JIT `Float16Array`/typed-array miscompile: compiled `makeArrayLike`-style loops read all-`NaN` from some iteration onward, then segfault (~200-fixture crash cluster, JIT-only, `--jitless` clean) | open; Linux-only; being debugged | Pre-existing at `d58caea`; not GC (reproduces with collections disabled). Lives in `crates/jit` lowering. Unblocks clean Linux JIT built-ins and the `-p jit` release test binary. |

## P1 — Structural property machinery (L1c → L2)

| # | Item | Status | Evidence / first action |
|---|---|---|---|
| 1.1 | L1c read/write end-state on maps/shapes: hot member paths serve via shape-compare + inline-field access instead of the generation/id/name value-cell probes; exotic receivers/accessors/index keys fall back to the exact machinery | partial (register-path folds, L1c-1 pinned mirrors, the map layer exist; the general path still probes cells) | perf.md read-path note: the residual ~7ns/read is the value-cell probe; "the shape-compare end state (per-site map validation, L2) is the structural follow-on". Read path first (interp shared helper, then JIT inline), then the write path, then transitions. |
| 1.2 | L2 per-site feedback: per-call-site IC entries (shape/offset) shared by the interpreter and the JIT, replacing the global direct-mapped tables | not started | Gated on 1.1 establishing the shape representation (the plan defers L2 until then). |

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

## Recommended order

1. 0.1 in parallel (the Linux debug agent owns it) — a correctness blocker.
2. 5.1 is LANDED (2026-09-04). The next small in-family candidate is the
   remaining branchy-body overhead (run splits around the branch) if the
   register-run arc continues.
3. Then the plan's own sequencing: 1.1 (L1c end-state) → 1.2 (L2) before
   2.1 (L3) unless a probe changes the calculus.
4. 3.1 (L4) and 4.1 (L5) after the L1c/L2/L3 probes re-derive their targets.
