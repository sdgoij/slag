# Performance: the 3–5x program

**Status: open, started 2026-10-06.** Base measurement: tag `v0.1.0`
(`e380d9b2`). Goal: **3–5x wall-clock** on the 77-workload corpus and the
micro-suite, both columns (`jit` and `--jitless`), measured against that tag.
This is the successor to `.notes/jit-quality-plan.md`, whose JIT-side work list
is exhausted (its remaining items are architectural, not inline paths); this
program is **interpreter-first**, because the interpreter is the deopt target,
the `--jitless` column, and the floor under every compiled row — a win there
moves all three at once.

## 1. Why the baseline is `v0.1.0`

`v0.1.0` (`e380d9b2`) is the full-conformance release. `HEAD` is two commits
past it (`cced75e` — ECMAScript 2027 docs + `0.2.0`; `7e09cf7d` — Date AO
renames + this program's sibling audit note), both behavior-identical, so a
`HEAD` measurement *is* the `v0.1.0` measurement until the first perf slice
lands. Every slice's before/after is taken against this tag, so a delta is never
confounded by unrelated tree movement.

## 2. The baseline (measured)

Micro-suite (`--jit-bench`, medians of 5 — `.notes`/README carry the full table):

| body | interp | jit | ratio |
|---|---|---|---|
| arithmetic | 7.90 ms | 0.671 | 0.08 |
| wide leaf call | 23.02 | 2.018 | 0.09 |
| bare loop | 7.39 | 0.772 | 0.10 |
| property read | 8.23 | 0.808 | 0.10 |
| global read | 10.47 | 1.512 | 0.14 |
| typed-array read | 96.48 | 14.522 | 0.15 |
| builtin call | 4.60 | 0.748 | 0.16 |
| string concat | 1.22 | 0.205 | 0.17 |
| function calls | 5.97 | 1.006 | 0.17 |
| typed-array length | 11.90 | 2.220 | 0.19 |
| buildString shape | 79.47 | 15.672 | 0.20 |
| non-leaf call | 28.11 | 6.394 | 0.23 |
| typed-array write | 28.11 | 6.518 | 0.23 |
| buildString full | 68.66 | 18.873 | 0.27 |
| apply leaf call | 20.59 | 7.318 | 0.36 |
| compound assign | 3.47 | 1.758 | **0.51** |

Corpus (77 workloads, gap = slag/node; `node tools/corpus/bench.js`):

| family | n | jitGap | jlGap |
|---|---|---|---|
| arrays | 6 | 18.74 | 4.23 |
| builtins | 5 | 10.60 | 5.07 |
| calls | 6 | 13.02 | 5.63 |
| control | 5 | 20.17 | 5.66 |
| globals | 4 | 2.16 | 0.72 |
| language | 3 | 14.39 | **10.26** |
| objects | 7 | 87.57 | 3.22 |
| opcost | 36 | 24.20 | 4.81 |
| strings | 5 | 28.69 | **21.75** |
| **all** | **77** | **26.28** | **5.86** |

The micro ratios are the interpreter-bound signal: the rows where the JIT buys
the least (`compound assign` 0.51, `apply leaf call` 0.36, `buildString full`
0.27) are interpreter-shaped work. The corpus's worst *interpreter* families are
`strings` (21.75) and `language` (10.26); its worst *JIT* families are `objects`
(87.57 — dominated by `destructure`, which V8 scalar-evolves, so its ratio is
not machinery) and `strings` (28.69).

## 3. Method

1. **One row or family at a time.** Pin the target row, measure it isolated (its
   own process / its own `--corpus` directory), then in the full suite.
2. **Before/after from binaries.** Build the tag binary once and keep it; each
   slice is `tag-binary` vs `slice-binary` on the same rows, interleaved, min of
   N. Never quote a delta computed against a different run's control row.
3. **Correctness gate every slice:** `cargo test --locked --workspace`, and a
   test262 `all` + `intl402` sweep at baseline (15s deadlines), plus fmt/clippy.
4. **Keep a slice only if** it clears the agreed bar (1.3x is acceptable for a
   row; 3–5x is the aggregate goal) *and* nothing else regresses. A change that
   trades one family for another is not a win.
5. **Record the nulls.** A measured dead end is written down (as
   `jit-quality-plan.md` does) so a later pass does not re-open it.

## 4. Levers, in order

- **L1 — interpreter hot paths (first).** The per-op machinery the interpreter
   runs: string concat/building, the call funnel (`apply`, non-leaf), compound
   assignment, property/element read-write. Moves `jit`, `--jitless` and the
   JIT's residual together.
- **L2 — allocation and frames.** The `language/*` amplified rows and the
   for-in head-let row are allocation-bound (fresh environment + closure per
   iteration; ~950 ms/rep against 1,058 ms jitless — no inline path can pay).
  This is the **memory** priority (§3 of the project priorities) wearing a perf
  hat; the win is bigger than it looks because those rows dominate absolute time.
- **L3 — JIT coverage.** Resume `.notes/jit-quality-plan.md` where it stopped —
  the L1c shape end-state for chain reads, and the general call's pooled private
  Vm (its Stage 9) — only if L1/L2 leave a compiled-row gap worth it.
- **L4 — codegen.** Cranelift is the backend; no front-half (feedback → SSA →
  speculation → deopt) exists. That front half is the eventual TurboFan-scale
  rewrite, out of scope for this program; it is the thing to *start* when L1/L2
  plateau (the tell: per-row `emit_*` cost stops buying, family means stop
  moving).

## 5. Stages

- **Stage 0 — baseline (this note).** Pin `v0.1.0`, keep the tag binary, capture
  the micro + corpus tables above as the reference. **In progress.**
- **Stage 1 — L1, interpreter hot paths.** First target chosen by
  measurement: the compiled member **write** (see §6). **First slice landed
  2026-10-06 — the vector-free field update** (option (a)): a compiled store to
  a written, writable map-pinned in-object field on a `props_deferred`
  receiver now writes `in_fields[slot]` inline instead of calling
  `SetMemberSlot`. The deferred store story is CLOSED (see §6). **Next target:
  the call funnel's `LeafCallFill`** (top call-family helper; see §6). Remaining
  hard candidates: the `strings` read+concat triple (`GetMemberName` method
  reads — the L1c shape end-state; `ConcatStrings`/`BinarySlow`) and
  `LoadContext` (a machine-addressable env chain).

## 6. Status log

- **2026-10-06 — Stage 0 done: baseline pinned, census re-run, one false lead
  cleared.** The release CLI is built and kept as the `v0.1.0` reference. The
  helper census (`JIT_HELPER_STATS=1 … --corpus`, `Helper as usize` order) over
  the 77 rows now reads: `GetMemberName` **102.1M**, `SetMemberSlot` 70.0M,
  `LoadContext` 38.8M, `CallSlow` 38.5M, `LeafCallFill` 23.0M, `ObjectFast`
  15.6M, `BinarySlow` 14.2M, `CertifiedCall` 11.3M, `ArgsSpread` 9.1M,
  `ApplyArgsFill` 7.0M. So the JIT's top undischarged work is still the member
  read/write pair, which `jit-quality-plan.md` §5 already closed by measurement
  as needing the **L1c shape end-state**, not an inline path.
- **The first candidate was a measurement artifact, and the method lesson is
  the point.** `strings/split_join` read 76 ms jit / 563 ms jitless in the
  family run — a 7.4x interpreter "outlier" — and 684 ms jitless in the corpus
  run. Run *alone* (the corpus README's rule for allocation-heavy rows) it is
  jit **91–269 ms** (bimodal) against jitless **111–134 ms**: the gap was the
  documented allocation-context effect (where the first big mark-sweep lands
  depends on what ran before in the process), not a deficit. Recorded so it is
  not re-opened. **Consequence for target selection:** the corpus's
  allocation-heavy rows are too context- and GC-sensitive to slice against;
  select from the low-variance micro-suite (medians of 5, both columns) and the
  census (stable counts), and confirm any corpus row alone before pricing it.
- **Next — Stage 1 target.** Unpriced; to be chosen from the micro-suite's
  worst rows (`compound assign` 0.51, `apply leaf call` 0.36) or the census's
  top entries (`GetMemberName` 102M / `SetMemberSlot` 70M / `LoadContext` 39M /
  `CallSlow` 39M).
- **2026-10-06 — Stage 1 target found by profiling, and the obvious fix is
  ruled out.** Per-row census + shape attribution of the `compound assign`
  micro body (100k iters, all jit, ns/iter): `s += o.x` (read) **2.5**,
  `o.x = i` (write) **11.7**, `o.x += 1; s += o.x` (the row) **17.4**. The
  census for the row is `SetMemberSlot` 4,800,000 (exactly 1/iter) and
  `GetMemberName` **1** — the read is a warmed cell, the write pays a helper
  every store. So the target is the compiled member **write**: ~4.7x the read's
  cost, `SetMemberSlot` is the census's #2 helper (70.0M), and it underlies the
  0.51 row. The cost is inside `set_member_slot`'s body — `write_data_property`
  does a key -> position lookup every call (linear scan below 16 props, else a
  hashmap probe) and a second `map_set` descriptor scan to mirror the field,
  where the interpreter's warm-store path is O(1) off a cached slot.
  **The naive fix — route `set_member_slot` through `Vm::warm_store_put`,
  whose slot + pinned-field machinery already exists — is rejected**, and the
  `slag-property-writes` skill is why: the interpreter and the compiled store
  have deliberately different generation disciplines (interpreter writes bump;
  the compiled fast store must not), and the skill says outright not to unify
  them. The write cells are also interpreter-only (`MemberWriteCell` is never
  read by the JIT; only `member_value_cells` is the `#[repr(C)]` ABI). `set_member_slot`
  is therefore left unchanged.
- **Next — Stage 1, corrected options.** (a) A JIT-side store cell: a new
  `#[repr(C)]` (id, name) -> slot table plus a compiled probe, mirroring the
  read cell — architectural (a new ABI + record/refresh discipline), but it is
  the only way to give the compiled write the slot the interpreter has.
  (b) Micro-optimize the shared `write_data_property` (the RefCell borrows, the
  `map_set` mirror when the key is unmapped, the barrier) — cheap, additive to
  both engines, but sweep-gated (every interpreter `put_value` shares it).
  Price (b) first (it is smaller and touches no ABI); scope (a) only if (b)
  cannot reach the bar.
- **2026-10-06 — Stage 1, first slice landed: the vector-free field UPDATE
  (option (a)), and the warm-store route is recorded as a null.** The
  rejected naive fix (route `set_member_slot` through `Vm::warm_store_put`) was
  re-examined against the code and the reason it regressed is now exact: for a
  `props_deferred` receiver the write cell's hit path calls
  `write_data_property_slot`, whose vector `get_mut(slot)` finds nothing (the
  vector is empty), and the record on the success path calls
  `JsObject::property_slot`, which **materializes** the object — so the route
  turned a cheap `deferred_field_write` into a vector materialization per
  store. The compile-side fix is the right one: the shared (map id, name) shape
  cell already pins the descriptor ordinal, which for a vector-free object IS
  the `in_fields` field, so the field write needs no new ABI table at all.
  **Landed:** `MemberMapCell` gained a `writable` bit (recorded from the map
  descriptor at both record sites — pinned by the map id, since maps are
  immutable); `emit_deferred_hole_fill` became `emit_deferred_field_store` and
  now serves BOTH the presize-hole fill (unchanged: bump + chain gate) and the
  written-field update (new: an own writable data property shadows the chain
  per spec 7.3.3, so no chain gate — write the field, refresh the value cell at
  the UNCHANGED generation with the L1c no-bump discipline). The update gate is
  `deferred && slot < INLINE_FIELDS && !hole && writable && (receiver young ||
  value not heap) && receiver != the global object`; the last two keep the
  barrier and the name-keyed global-value cell exact, and every failure falls
  to the narrow `set_member_slot` helper. `emit_deferred_inline_store` routes
  the **value-cell hit** (a warmed read, where the helper was still charged)
  through the same path, wired into the register `StoreMemberName*` ops
  (`emit_validated_member_store`) and the step `AssignMemberName` (both the
  compound and plain branches).
  **Measured** (tag-binary vs slice-binary, same profile, min-of-5):
  `--jit-bench` `compound assign` **1.94 → 1.33 ms (1.47x)**, ratio 0.57 →
  0.37, with `SetMemberSlot` on that row going **4,400,000 → 0** calls (the
  census); `property read` (`rd.js`) unchanged; corpus
  `objects/compound_assign` run alone **69.7 → 49.6 ms (~1.4x)**. The
  whole-corpus single-process run is the documented context/GC-noise regime
  (median ratio 1.000, mean 1.070, large spurious moves both ways); every
  apparent >1.1x regression was re-run alone and is equal or faster
  (`opcost/array_reverse` 1.86x claimed → 22.3 vs 22.5 ms flat;
  `opcost/array_fill`, `objects/warm_store`, `objects/spread_assign` flat).
  **Gates:** fmt clean, `clippy --locked --workspace --all-targets -D warnings`
  clean, `cargo test --locked --workspace` **5,635 passed / 0 failed** (an
  earlier run ICE'd in the untouched `wasmtest` bin — an incremental-cache
  artifact, green with `CARGO_INCREMENTAL=0`), a new
  `installed_jit_vector_free_field_update_matches_the_interpreter` test
  (constructor fill + a prototype setter that must not intercept the own-field
  update, compared against the interpreter), test262 `all` **48,632 / 0 / 1 /
  `intl402` **3,365 / 0 / 0 / 0 / 0** at baseline.
- **The deferred store story is CLOSED; the proposed ordinal-`>=INLINE_FIELDS`
  follow-up is moot.** `define_fresh` materializes an object at the
  `INLINE_FIELDS`-th define, so a `props_deferred` receiver can only ever hold
  descriptors at ordinals `< INLINE_FIELDS` — the `slot_inline` guard in the
  update gate is defensive, never a real decline, and there is no
  vector-storage variant of this slice. A MATERIALIZED receiver still pays
  `write_data_property`'s vector scan on every store, and that one is not
  inline-able: the property vector is a `SmallProps` (an inline array that
  spills to a `Vec` behind a `RefCell`), so its address is not stable, and an
  `in_fields`-only write would leave the descriptor value stale for
  enumeration/`Object.values`/`JSON.stringify` (the map read path serves the
  field, but the own-property lookup scans the vector). That is the
  TurboFan-front-half / vector-redesign territory, not a slice.
- **2026-10-06 — Stage 1, next target chosen by census: the call funnel's
  `LeafCallFill`.** Per-row helper attribution (single-row `--corpus` dirs,
  `Helper as usize`):
  - `calls`: `LoadContext` **28.9M** (`closure_capture` 14.0M, `recursive_fib`
    14.9M), `LeafCallFill` **23.2M** (`method_call` 14.0M, `closure_capture`
    6.9M, `construct_churn` 3.5M), `CallSlow` 14.9M, `GetMemberName` 10.5M,
    `ApplyArgsFill` 7.0M (`apply_call` alone is 7.0M `ApplyArgsFill` + 7.0M
    `GetMemberName`), `LeafCallEnv` 5.7M, `Construct`/`ArgsPush`/`ArgsBase`
    3.5M each.
  - `strings`: `GetMemberName` **9.96M** + `ConcatStrings` 4.5M + `BinarySlow`
    4.2M (`char_ops` 2.1M `GetMemberName`, `coercion_concat` 4.2M `BinarySlow` +
    2.1M `ConcatStrings`, `concat_loop` 2.4M `ConcatStrings`,
    `search_slice`/`split_join` `GetMemberName` + `CallSlow`).
  - `language`: `BinarySlow` 9.88M, `CertifiedCall` 9.8M, `LoadContext` 8.5M.
  **Chosen: `LeafCallFill`** — the top call-funnel helper, purely mechanical (no
  speculation and no new architecture), and it moves `method_call`,
  `closure_capture` and `construct_churn` at once. It is already lean
  (`record_matches` + `fill_leaf_frame`, crates/runtime/src/jit.rs), so the
  candidate is to emit the frame fill inline in the compiler — the same "give
  the compiled path the work the helper does" move as this slice — **pending a
  check of how much of `method_call`'s count takes the `aliased` branch**
  (`frame_size == arity && argc >= frame_size` skips the fill loop entirely,
  which would cap the win at the helper-call and cache-entry cost).
  `GetMemberName`'s method reads and `LoadContext` stay the hard ones (the L1c
  shape end-state and a machine-addressable env chain respectively).
