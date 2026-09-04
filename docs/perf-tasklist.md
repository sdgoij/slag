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
| 1.4 | Primitive-string property reads box a String-exotic wrapper per access | **landed 2026-09-04** | Certified-body probe (200k `s.length` reads, `Gc::new` TLS counters): top-level eval, certified interp, AND the JIT all boxed 448B wrapper + 64B [[StringData]] per read. Fix in the shared `Vm::get_member_name`/`get_member_computed` helpers (mirroring the typed-array `length`/element shortcuts, so the step path, register ops, and JIT ABI all inherit): string-`.length` returns the code-unit count; in-range canonical numeric index returns the single code unit (StringGetOwnProperty — own, shadows the chain); OOB/non-index falls through (patched `%String.prototype%` numeric keys still found). Counts 400k -> 0; clean A/B on the 200k row: interp ~106-119ms -> ~4.2-4.8ms (~23x), jit ~108-308ms -> ~2.4-2.8ms (~40x). Gates: clippy, workspace tests (new `string_primitive_member_reads_serve_length_and_units_without_boxing`), three sweeps at baseline (perf.md record). |

## P2 — JIT coverage (L3)

| # | Item | Status | Evidence / first action |
|---|---|---|---|
| 2.1 | Compile the general path (Sparkplug analog): emit every step in machine code for bodies the scope gate excludes, routing env/handler steps through the shared machinery | re-scoped by the 2026-09-04 probes | The scope-gate probe falsified the plan's premise for try/catch (those certify AND reach the JIT — per-iter try interp ~125ms / jit ~72ms) and 2.3 (this-capturing arrows) landed the dominant residual scope=None shape (~33x). The remaining scope=None hot shapes are narrow (with/eval/async-generator/super-constructors); their dispatch cost is not measured as a lever. Do NOT start the Sparkplug analog without a corpus probe showing uncertified hot bodies whose cost is dispatch (not the env path the certification fixes remove). |
| 2.2 | Certification over-rejection: nested NON-ARROW functions' own `this`/`arguments` bailed the enclosing body's scope certification | **landed 2026-09-04** | The closure walker (`closure_*_allows`) now threads an `own` flag: entering a nested non-arrow function sets it (its `this`/`arguments` are its OWN, bound at its own call); arrows propagate the caller's flag (an arrow in the analyzed body still observes its lexical `this` and bails). `super`/`class`/private/tagged/import stays rejected under `own`. Probe (perf.md, 2026-09-04): the construct-churn loop function-wrapped with a nested `function C(x){ this.x = x; }` ran ~117-129ms vs ~19-21ms with C a global; after the fix the nested form matches the control (~6x), because the body re-certifies and its `var`s leave the env path. Gates: clippy, workspace tests (new `nested_function_own_this_and_arguments_keep_the_body_certified`), three sweeps at baseline. |
| 2.3 | Certify `this`-capturing arrows: an arrow created in a certified non-arrow body that references `this` captures the body's this value (a synthetic context entry sourced from the this slot at creation); the arrow body reads it as a depth-0 context slot | **landed 2026-09-04** | The closure walker records a reserved marker (\u{1}captured-this) when an arrow references `this`; a NON-ARROW body allocates a marker context slot + forced this slot and `compile_body` emits an entry store copying this into it; an ARROW body certifies only when its outer chain carries the marker (its direct `this` compiles to a `LoadContextSlot` resolved through the chain; deeper this-arrows flow the same way). Env-path arrows (rest params etc.) inside a capturing body resolve lexical this through the capture context (`DeclarativeEnv::has_captured_this`/`captured_this_value` make the marker env a this-environment — the Object/keys/proxy-keys regression fix). Measurement (perf.md, 2026-09-04): the callback-in-method probe dropped ~1.4s -> ~42ms (~33x). Gates: clippy, workspace tests (new `this_capturing_arrows_certify`), three sweeps at baseline. |

## P3 — Allocation (L4 / M8 arena)

| # | Item | Status | Evidence / first action |
|---|---|---|---|
| 3.1 | Bump arena for the hot shapes (ropes, fresh ordinary objects), swept by the existing collector | **closed by probe 2026-09-04 — no arena work indicated** | Counting probe (perf.md, 2026-09-04): `construct churn` = exactly 1 x 448B arena box per iteration (the `new C(i)` instance itself; no context/env/key extras); `buildString full` = 390 boxes TOTAL for the whole row (the ~1.1M dense element writes allocate zero). The bump arena the plan proposed ALREADY exists (A5.1: bump + size-classed free-list; GC-5 measured the free-list half net-neutral and registration ~11ns/alloc). No second hot shape to give a dedicated arena; the rows' residual cost is the certified-construct path and branchy step dispatch. The probe's side finding (primitive-string property reads boxing a wrapper per access) is tracked as 1.4. |

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
   value-cell misses. 2.2 (certification over-rejection) and 2.3
   (this-capturing arrows, ~33x) are LANDED, and the scope-gate probe
   (2.1) closed the Sparkplug-analog premise for try/catch.
4. 3.1 (L4 arena) is CLOSED by its counting probe (2026-09-04): the arena
   already exists and both target rows measured 1 box/iter (construct) and
   ~390 boxes total (buildString full) — no arena to build. 1.4 (string
   `.length`/unit reads boxing a wrapper per access) is LANDED on this
   tree (the certified-body probe showed every read path boxes; the shared
   member helpers now serve length/units off the raw string — interp ~23x,
   jit ~40x on the probe row). Next candidates, in order: (a) 4.1 (L5
   `.apply`/`.call` member-read residual) only after a fresh A/B re-derives
   its target; (b) the write-side >256 follow-on probe (per-site store
   ICs) only if a realistic >256-object store loop shows up; (c) a
dispatch-side sweep for the remaining primitive-receiver reads (method
calls like `s.charAt(i)`/`s[i]` in the compiled call path may still box
the receiver — probe before fixing). 0.1 (the JIT
   Float16Array/typed-array miscompile) stays owned by the Linux debug
   agent in parallel.
