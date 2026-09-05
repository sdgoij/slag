# Perf tasklist (merged from performance-plan.md + gap-close-plan.md, 2026-09-04)

> Merged, prioritized view of the remaining work in the active plan
> (`.notes/performance-plan.md`, mechanism-based, supersedes) and the closed
> historical plan (`.notes/gap-close-plan.md`). Status reflects everything
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
| 1.5 | Primitive-string METHOD reads (chain data/accessor/symbol keys) resolve on `%String.prototype%` without boxing | **landed 2026-09-04** | Probe: `s.charAt`/`charCodeAt`/`indexOf` each boxed 448B wrapper + 64B [[StringData]] per CALL on top-level eval, certified interp, AND the JIT (200k calls = 400k boxes; the compiled call path has no primitive-receiver member fast path). Fix: `Vm::get_string_primitive` — after the 1.4 length/index shortcuts, a string-primitive fallback resolves the key against the realm's cached `%String.prototype%` with the PRIMITIVE as the [[Get]] receiver (exact: the wrapper's own props are only length/index, and the engine threads Receiver=primitive through OrdinaryGet — data props, accessors (strict getters see the primitive; sloppy this-coercion boxes), proxy links, symbol keys all match). Boxes 400k -> 0 (charAt retains its inherent result-string box). Clean A/B on 200k rows: charCodeAt interp ~413-430 -> ~246-250ms (~1.7x), charAt ~348-371 -> ~207-216 (~1.7x), indexOf ~550-578 -> ~394-409 (~1.4x); jit similar; the residual per-call cost is the intrinsic CALL dispatch (4.1/L5), not the read. Semantic battery byte-identical vs the boxed path (sloppy/strict getters, patched methods, proxy-in-chain receiver, numeric OOB). Gates: clippy, workspace tests (new `string_primitive_method_reads_resolve_on_the_prototype_chain`), three sweeps at baseline (perf.md record). |
| 1.6 | Non-string primitives (Number/Boolean/BigInt/Symbol) METHOD reads resolve on their %X.prototype% without boxing | **landed 2026-09-04** | The 1.4/1.5 helper was string-only; probe (200k, `Gc::new` TLS counters) showed Number/Boolean/Symbol/BigInt method reads/calls boxed a 448B wrapper PER READ on both engines (number/boolean wrapper creation also inserts an agent boxed-value-table entry): `n.toFixed`/`toString` reads 200k x 448B, call rows + the inherent result-string boxes, `sym.description` + result, bigint call + 48B box. Fix: generalized `Vm::get_string_primitive` -> `Vm::get_primitive_member` and routed every primitive (not just String) through it in `get_member_name`/`get_member_computed`; new `Intrinsics::primitive_prototypes` cache (Number/Boolean/BigInt/Symbol, array indexed like `function_prototypes`). Exactness is the same argument as 1.5 (these wrappers are ordinary objects with NO own properties — the direct chain read with the primitive receiver reproduces every read; the `description` accessor and proxy links see the primitive receiver). Boxes 200k -> 0 on reads. Clean A/B on Number method-read rows (200k): interp ~106-119ms -> ~13.3-14.1ms (~8.4x), jit ~97-104ms -> ~7.9-9.7ms (~11-13x); the residual ~67ns/call interp is the %Number.prototype% own-scan chain read (the 4.1 probe's chain-read primitive — L2). 18-line semantic battery byte-identical (methods, patches, strict/sloppy getters, proxy receiver, `Symbol().description`, boxed `new Number(5)` reads). Gates: clippy, workspace tests (new `non_string_primitive_method_reads_resolve_on_the_prototype_chain`), three sweeps at baseline (perf.md record). |
| 1.7 | Store-cell capacity: `MEMBER_WRITE_CELLS` 256 -> 4096 (boxed table, heap-direct init) | **landed 2026-09-04** | The (a) probe: a >256-distinct-object cycling store loop hits a real cliff — 1M stores across 1024 objects ~180ns/store interp (~165 jit) vs ~55ns/~40ns for 1-256-object sets, because the 256-entry direct-mapped write cells thrash and every store falls to the full [[Set]]; READ rows do NOT cliff (59-61ns at both 64 and 1024 — the read map/proto-cell layer absorbs). Such loops are realistic (per-frame entity/record updates over thousands of objects). Fix: grow the boxed `MEMBER_WRITE_CELLS` 256 -> 4096 and make `Agent::new` build it heap-direct (the `from_fn` array temporary sat on the stack — ~128KB at 4096 — and overflowed the 1MB-stack embed doctest). 1024-object stores drop ~180 -> ~55ns interp (~165 -> ~40 jit); warm rows and the suite move within the cross-build layout band; the charCodeAt control flat. Working sets >4096 still cliff (that residual is the L2 shape-keyed store slice, 1.8). Gates: clippy, workspace tests (4652/0 incl. the embed doctest), three sweeps at baseline (perf.md record). |
| 1.8 | Shape-keyed store cells + the direct own-data fallback (L2 slices b/c): remove the >4096-object store cliff for same-shape and vector-only hot stores | **landed 2026-09-04** (this tree) | The (id, name) `MEMBER_WRITE_CELLS` table thrashes once a store loop's object working set exceeds 4096, even when every object shares one shape — measured cliff (200k-call rows, fresh release build of `ebe30cc`): 8192-object inline-field stores ~181ns/store interp (~162 jit) and 16384 ~182ns vs ~56ns/~40ns at 64-1024 objects. Two mechanisms. (1) **Shape-keyed cells**: a second direct-mapped table `member_write_map_cells` keyed by (map id, name) — a map id pins the descriptor layout for every instance of the shape, so a hit needs no per-object identity or generation. Probed as the fallback when the (id, name) cell misses; recorded ONLY for map-described inline keys (a vector-only property's slot is per-object — two objects can share a live map yet hold different vectors after it), so the pinned inline mirror is always real. (2) **Direct own-data fallback**: the residual probe showed the same cliff for a key the map does NOT pin (a 5th+ field of a many-field shape is vector-only — 8192 ~214ns, 16384 ~223ns interp) and that no per-step IC can serve it (nothing shape-pins a vector-only slot). Instead the miss chain now resolves the object's OWN vector slot (`property_slot`) and writes in place when the property is already an own writable data property — exact (an own writable data property shadows the chain, spec 7.3.3; accessor/non-writable/absent fall to the full [[Set]]) — turning every warm in-place store into an O(1) resolve+write regardless of object count. A hit on either fallback re-keys the (id, name) cell so the same instance's next store keeps the cheaper primary probe, and fronts the read-side value cell under the L1c no-bump discipline. Inline rows: 8192/16384 ~181-182ns -> ~58-67ns/interp (~162 -> ~44 jit), now AT the 64/1024 warm level; vector-only rows ~214-223ns -> ~68ns (jit ~184-196 -> ~54ns); single-object rows unchanged (no warm regression). Gates: clippy, workspace tests (new `stores_over_many_same_shape_objects_stay_exact` + `stores_over_many_vector_field_objects_stay_exact` — 9000 same-shape / six-field instances with distinct values, interleaved map transitions, non-transitioning defineProperty, and deletes to dictionary mode, all read back), three sweeps at baseline (perf.md record). The remaining full-[[Set]] stores are true defines (a genuinely new key), which no IC can make faster without the L1c storage migration; the chain-member-read slice (a) still waits on L1c's shape end-state. |

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
| 4.1 | Inline the `.apply`/`.call` member read on the compiled intrinsic path | probe done 2026-09-04 — target re-derived as diffuse; deferred to the L2 per-site IC and the L5 call-dispatch levers | Fresh A/B decomposition on the `apply leaf call` shape (200k, per call): apply-9 interp ~98-105ns / jit ~36-44ns; .call-9 interp ~100-107 / jit ~26-32; same-leaf direct 9-arg call interp ~58-62 / jit ~6.2 (the floor). Overhead vs the direct call: interp ~40-44ns, jit ~20-37ns — allocation-free (boxes 0) and the arg-array fill is NOT the term (interp .call ≈ .apply; jit apply-9 ≈ apply-1). The residual is spread across the per-iteration chain member read of the method, the intrinsic identity compare, and the CallApply dispatch; prototype-chain member reads cost ~4x own-data reads interp and ~10x jit (55 vs 16ns interp; ~28 vs ~2.8ns jit) across function/object/array receivers — the read IS a real primitive, but a narrow `.apply`-only inline has no clean target (the read is shared with every `o.m()`; an inline validation was measured slower in 2026-09-01). Defer the read-side fix to L2 (per-site shape/offset ICs) and the dispatch-side residual to L5. |
| 4.2 | Register agent-dependent builtin handlers in the O(1) per-function-id table so warm calls skip the module dispatch chains (the L5 intrinsic-call dispatch floor) | **landed 2026-09-04 — String + Number + Boolean + BigInt**; Object/Date/Keyed/etc. chains share the pattern | Probe (200k calls, certified rows, both engines): `s.charCodeAt` ~1.18µs/call interp and `a.push` ~1.7µs vs `Math.abs` ~150ns (a plain native closure, no agent chain) and a same-work JS leaf ~90ns. Mechanism: agent-dependent methods (ToString/@@-delegation need the agent, so they are placeholder-closure builtins dispatched by intrinsic identity) run the module's LINEAR `dispatch_call` chain on every warm call — each `intrinsics.get` arm allocates a JsString + hash-lookup, and only `array::handler_for`/`regexp::handler_for` register O(1) per-id handlers today. Fix: per-module `handler_for` maps (String ~39 non-HTML arms, Number 7, Boolean 3, BigInt 6 — each arm's `(agent, this, args)` handler, constructor arms via adapter closures) consulted by `Intrinsics::define`, registering each method by function id at install. charCodeAt interp ~1.18µs -> ~380ns (~3.1x); clean A/B per call on the primitive rows: `n.toFixed(1)` interp ~1170 -> ~680ns (~1.7x), `b.toString()` ~615-653 -> ~300-307 (~2.0-2.3x), `123n.toString()` ~926-963 -> ~346-361 (~2.6-2.8x); jit proportional. Residual is the primitive chain READ (~250ns) + native call (the L2 read lever). Gates: clippy, workspace tests (new `string_agent_builtins_dispatch_identically_via_registered_handlers` + `number_boolean_bigint_builtins_dispatch_via_registered_handlers`), three sweeps at baseline (perf.md record). Next: the same `handler_for` maps for the other agent-dependent modules if a corpus probe shows their methods hot. |
| 4.3 | Register the KEYED module (Map/Set/WeakMap/WeakSet + iterator nexts + statics/size/species) in the O(1) handler table — the (d)-landing follow-on (c) | **landed 2026-09-04** (this tree) | After the hash-index landing (d) the keyed rows' residual was the module's ~55-intrinsic `dispatch_call` chain (Set.has's arm ~40 `intrinsics.get` calls in — ~5µs/call; Map.get ~1.9µs at arm ~10). `keyed::handler_for` maps every `Intrinsics::define`'d keyed function (methods, groupBy, size/species getters, both iterator `next`s) to the named `(agent, this, args)` handler the chain already calls; the four constructors register their call-without-new TypeError (their `new` path keeps `dispatch_construct`). A/B vs parent `3cd5c9b` (the index landing, 200k-call rows): Map.get ~356 -> ~45.6ms (~7.8x, ~228ns/call), Map.set ~513 -> ~44.3ms (~11.6x), Set.has ~891 -> ~48.9ms (~18x — its late arm collapses to Map.get's cost), delete+set churn ~770 -> ~94ms (~8x); the JIT column matches. Gates: clippy, workspace tests (new `keyed_builtins_dispatch_via_registered_handlers`), three sweeps at baseline (perf.md record). Next: the (c)-probe's remaining chain-bound modules are Object (arms are INLINE closures — a named-handler refactor first) and DataView (~660ns) — extend per a corpus probe showing their methods hot. |
| 4.4 | Register the OBJECT and DataView modules in the O(1) handler table (candidate (c) completion) | **landed 2026-09-04** (this tree) | Object's `dispatch_call` arms were INLINE closures (the (c)-probe's reason registration was deferred): each closure is now extracted into a named `(agent, this, args)` handler (`prototype_has_own_property`, `object_create`, `object_define_property`, `object_entries`/`values`/`keys`, `object_get_own_property_descriptor(s)`, `object_has_own`, the integrity-level statics, ...), so the chain and the new `object::handler_for` share one implementation; DataView's get/set codecs register per element type and the buffer accessors directly. A/B vs the 4.3 parent (200k-call rows): Object.hasOwn ~1085 -> ~126ms (~8.6x, ~632ns/call), hasOwnProperty ~283 -> ~150ms (~1.9x — an early arm, work-bound), DataView.getUint8 ~630-720ns/call (the (c) probe) -> ~291ns (~2.2-2.5x); Object.keys' row is allocation-bound (64 fresh key strings per call), unchanged. Gates: clippy, workspace tests (new `object_and_dataview_builtins_dispatch_via_registered_handlers`), three sweeps at baseline (perf.md record). Candidate (c) is now CLOSED: every agent-dependent module whose methods a probe showed hot (String/Number/Boolean/BigInt/Keyed/Object/DataView) is registered. |

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
   `.length`/unit reads boxing a wrapper per access), 1.5 (string METHOD
   reads), and 1.6 (Number/Boolean/BigInt/Symbol METHOD reads) are LANDED
   on this tree (~23x/.length; ~8-13x on Number method-read rows; boxes
   200-600k -> 0 on every read). 4.1's fresh A/B is DONE (2026-09-04): the
   apply/.call residual (~40-44ns interp / ~20-37ns jit over a direct leaf
   call) is allocation-free, the fill is interp-free, and the cost spreads
   across the per-iteration chain method read + intrinsic compare +
   CallApply dispatch — no narrow .apply-only slice; the read side defers
   to L2 (per-site ICs), the dispatch side to L5. 4.2 (the intrinsic-CALL
   dispatch floor: warm agent-dependent builtin calls paid the module's
   linear identity chain) is LANDED for String + Number/Boolean/BigInt
   (charCodeAt ~3.1x, toFixed ~1.7x, bool/bigint toString ~2-2.8x; see the
   4.2 row). 1.7 (the write-side >256 capacity probe) is LANDED: the
   cliff was real (~3.3x at >256 objects) and the boxed `MEMBER_WRITE_CELLS`
   bump to 4096 removes it through ~4k-object sets at no measured warm-row
   cost. 1.8 (the >4096-object store ceiling) is LANDED (2026-09-04, this
   tree): a (map id, name)-keyed write table serves every instance of a
   shape at the map-pinned slot after the identity table thrashes, and a
   direct own-data resolve-and-write fallback serves vector-only (5th+)
   fields the map does not pin — the 8192/16384-object store rows drop
   ~181-223ns -> ~58-69ns/store interp (~162-196 -> ~44-54ns jit) and sit
   at the 64/1024 warm level, with the single-object rows unchanged. The
   remaining full-[[Set]] stores are true defines (a genuinely new key),
   which no IC can make faster without the L1c storage migration.
   Remaining candidates, in
   order: (a) the chain-member-read cost itself — probe DONE 2026-09-04:
   clean marginal warm chain reads (numeric values, bare-row-subtracted,
   both engines) are interp ~18ns / jit ~17ns vs own-data ~1-3ns, FLAT in
   link depth (1 vs 2 links identical) — the fixed member_chain_get
   validation dominates, not the walk; the JIT inline-probe experiment
   measured slower, and there is no shape-free slice, so the fix is L2's
   per-site shape/offset IC once the L1c representation lands; (b) the
   >4096-object store ceiling via L2 per-site store ICs once the shape
   representation exists; (c) extending 4.2's O(1) handler registration to
   the remaining agent-dependent modules — probe DONE 2026-09-04: the
   chain-bound residue is Object's (~40-arm chain: hasOwnProperty ~950ns,
   Object.hasOwn ~5µs — late arms pay ~35 `intrinsics.get`/call) and
   DataView's (~660ns) methods, but Object's dispatch arms are INLINE
   CLOSURES (registering them means refactoring to named fns — defer to L2
   or a dedicated mechanical pass); Map/Set/WeakMap are NOT chain-bound —
   they are O(n) per op (find_index/find_set_index linear-scans the
   entries Vec; Map.get ~2.9µs, Map.set ~3.7µs, Set.has ~5.5µs on a
   1024-entry map vs Math.abs ~155ns), a structural lever that registration
   cannot touch. So candidate (c) is superseded by (d): hash-index
   `map_data`/`set_data` (the entries Vec + a key index) — likely the
   largest remaining lever for Map/Set-heavy code, and O(n) is why no
   bench row exposes it. (d) is LANDED (2026-09-04, this tree): each
   Map/Set now carries a SameValue-consistent key-word index over its live
   entries (a `MapCollection`/`SetCollection` bundling the tombstoned
   entries List with the index), so get/has/set/delete probe O(1); a
   delete drops its row in O(1) and a word-absent probe is an
   authoritative miss (the exact scan runs only under a genuine 64-bit
   word collision, `collided`). A/B vs parent (fresh release builds,
   200k-call rows): the churn row (delete+set over a 1024-entry map, which
   tombstones and re-appends) drops ~34.5s -> ~0.9s (~38x); Map.get misses
   ~908 -> ~386ms; Map.get hits ~606 -> ~388ms and Set.has ~1245 -> ~1033ms
   with the row cost now FLAT in size (1024-entry == 16-entry rows), so
   the scans are gone. The per-call residual (~1.9µs Map.get / ~3-5µs
   Set.has) is the module's dispatch-chain arm, NOT a scan — so candidate
   (c) for the keyed module is the next slice: its dispatch arms are the
   named `(agent, this, args)` handlers 4.2's `handler_for` pattern wants
   (unlike Object's inline closures), and registration should collapse the
   Map.get/Set.has row floors toward the registered charCodeAt ~350ns
   floor. (c) is LANDED for the keyed module (4.3, 2026-09-04, this
   tree): Map.get/Map.set/Set.has rows drop to ~220-245ns/call
   (~7.8-18x vs the index-only rows), Set.has's late chain arm now equals
   Map.get, and the keyed row floor is the registered-call floor.
   WeakMap/WeakSet stay linear (their GC compaction renumbers
   slots, which a position index must clear at every sweep) — not in the
   measured rows. Remaining chain-bound modules from the (c) probe:
   Object (~40-arm; its `dispatch_call` arms are INLINE CLOSURES, so
   registration needs a named-handler refactor) and DataView (~660ns) —
   extend 4.2 only behind a corpus probe showing their methods hot; and
   the L2 per-site IC slices (a)/(b) stay behind the L1c shape
   representation. 4.4 (Object + DataView registration) is LANDED
   (2026-09-04, this tree): candidate (c) is CLOSED — every module whose
   methods a probe showed hot is now registered (String/Number/Boolean/
   BigInt/Keyed/Object/DataView); Object.hasOwn ~8.6x and DataView reads
   ~2.2-2.5x, and the only remaining linear chains are modules whose
   methods no probe has shown hot. The open structural items: the L2
   per-STEP store IC (for vector-only keys on shared maps and the JIT's
   shape-compare end-state) and the chain-member-read slice (a) both stay
   behind L1c's shape end-state; the JIT Float16Array/typed-array
   miscompile (0.1) is FIXED by the Linux work.
