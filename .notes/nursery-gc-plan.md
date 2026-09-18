# Nursery GC: implementation plan

Engineering spec for a generational collector on top of the existing
non-moving mark-sweep heap (`.notes/gc-plan.md` GC-1..GC-7,
`.notes/engine-redesign.md` A5.1/A5.1b). The regression net is the project
standard: `cargo clippy --workspace --all-targets -- -D warnings` clean,
`cargo test --workspace` green, and the full release test262 sweep at zero
regressions on the previously-passing union, at every cut. From A2 on, the
`--gc-stress` sweep is a hard gate; from A3 on, the new `--gc-verify` mode
is too.

Status: **A0-A7 landed** (§5: arena-walk enumeration / registry
removal; per-box generation bit + the young cohort list; the write barrier +
remembered set + its exactness verifier; the minor collection with in-place
promotion and its `--gc-verify` net; weak semantics under a minor, covered by
tests; the per-level triggers, nursery sizing, `--nursery-stress` and
`--gc-trace` telemetry; slot-range remembered sets via a dirty low-water mark;
JIT participation — the compiled safe point drives both levels, the inline dense
append's primitive appends stay inline against a promoted array, and the
leaf-call-cache flush is gated on a sweep that freed a box; then the locked
numbers and the doc pointers). The one open item is closed too: the A5.1-era
canary was a body-cache key collision, not the collector — see under A5.1.

## 1. Why now

`engine-redesign.md` deferred the generational half (A5.2-A5.4) as low ROI
when a construct-churn collection measured ~2.5ms. That measurement was the
micro-benchmark's small, stable live set. The 2026-09-15/16 hot-path work
raised the engine's allocation rate substantially, and a long-running
workload now hits the collector's cost model:

- Every collection is a **single synchronous stop-the-world mark-sweep**.
  Per phase: `live.sort` O(n log n), the conservative stack scan O(stack
  words), the mark O(reachable), the dead-set build O(n), the sweep O(n)
  with a `drop_in_place` + free-list push per box, and a fresh `keep` Vec.
  All O(live).
- The trigger (`maybe_collect`: `live > max(last_collected_live, 1024) * 2`)
  means the heap grows to ~2x the retained count between collections, so
  **the garbage batch is itself O(live)**. Pause therefore grows with
  retention — and retention grows because the conservative stack scan pins
  floating garbage. That is the feedback loop this plan breaks.
- `Heap::register` costs ~3ns/alloc (`.notes/perf.md` 2026-09-10, corrected
  2026-09-11) and exists only to serve the collector.

A minor collection whose cost is O(young) rather than O(live) breaks the
loop: pause stops tracking the retained set, and the floating garbage the
scan pins is reclaimed one cohort later instead of riding a full cycle.

## 2. Goal and non-goals

**Goal.** A minor collection that traces only the young cohort, at cost
O(young reachable + roots + stack words), with in-place promotion; a full
collection that runs rarely. No change to the rooting model (precise agent
roots + conservative native-stack scan + opaque-region scans).

**Non-goals (explicit).** Compaction/copying. Precise rooting (GC-7
option 3 — deleting the conservative scan). Concurrent or incremental
marking. Age-2+ promotion, generational tuning beyond one knob. Workers.

## 3. The decisive constraint: the collector cannot move objects

A copying nursery (the original A-D1) is off the table. Three independent
reasons, each sufficient:

1. **Registers.** A live `Gc<T>` can sit in a callee-saved register that the
   conservative stack scan cannot see. A non-moving collector's blind spot
   costs retention; a moving collector's is a use-after-free it cannot even
   detect, because there is no way to rewrite the register.
2. **`Trace` is read-only and passes no field location.**
   `Trace::trace(&self, visit: &mut dyn FnMut(GcAny))` hands the visitor the
   *child*, never the *field*. A relocation has to write the new address back
   into the field it came from. Adding that to ~50 `Trace` impls is blocked
   anyway by interior mutability: `RefCell`/`GcCell` tracing uses
   `try_borrow` and aborts the sweep on a mid-collection borrow, so there is
   no `&mut` to write through.
3. **The collector runs with `&self`.** `Agent::collect_garbage_with(&self)`
   is `&self` specifically so the `--gc-stress` collector can run from inside
   code that already holds `&mut Agent`. A moving collector needs mutable
   access to every table it relocates through (the IC value cells, the Vm
   stacks, the job queues) and many of those are plain `Cell`s/Vecs behind
   `&self`.

**Consequence.** Generations are a **per-box bit**, not an address range, and
promotion is **in place**. There is no semi-space, no forwarding pointer, no
field rewrite, and the existing deref path is untouched.

## 4. Design

### 4.1 Box header (A0)

`GcBox` today is `{ mark: Cell<bool>, size: u32, data: T }` — 8 bytes of
header with 3 bytes of padding. Replace with:

```text
{ flags: Cell<u32>, size: u32, vtable: &'static VTable, data: T }
```

- `flags`: bit0 `live`, bit1 `mark`, bit2 `young`, bits 3.. `age` (reserved).
- `vtable` points at a per-`T` static `{ trace: fn(*const (), &mut dyn
  FnMut(GcAny)), drop: fn(*mut ()) }`, so the erased walk can trace and drop
  a box without a fat pointer.
- Header 8 -> 16 bytes. **Measured size classes** (A0): the hot three are
  byte-neutral — `JsObject` 480 payload -> 496 both ways, `JsString` 48 -> 64,
  `Function` 112 -> 128 — while six secondary types grow one 16-byte class:
  `Map` 88 (+16), `Symbol` 56 (+16), `ArraySlots` 72 (+16),
  `ArgumentsSlots` 24 (+16), `PrivateElement` 40 (+16), `ProxySlots` 56 (+16).
  Payloads `p == 8 (mod 16)` cross a boundary; the hot allocation sites do
  not. If a later cut needs the class back, shrink the payload (do not
  shrink the header — the walk needs `size` and a per-type drop).
- `GcAny` becomes a **thin** pointer (the box base); the vtable comes from the
  header. `Gc::as_any` becomes a cast; `trace` reads `(*ptr).vtable.trace`.
- `GCBOX_DATA_OFFSET` moves 8 -> 16; the JIT reads it via `offset_of!`, so
  compiled code follows automatically.

### 4.2 Generations

- **Old** = allocated before the last minor collection, or promoted.
- **Young** = allocated since the last minor collection.
- Promotion is age 1: a young box that survives a minor collection has its
  `young` bit cleared **in place**. Old is forever in v1 (no age counter).
- Because promotion is in place, a promoted box keeps its nursery address —
  so `is_young` must read the header bit, not compare an address range.

### 4.3 Young enumeration (A1)

A thread-local `young: Vec<usize>` of box addresses:

- Pushed by `Gc::new`/`new_in_place` when the box is young.
- **Cleared after every minor collection** (age-1 promotion means nothing
  young survives one), so its size is bounded by the allocation between two
  minor collections.
- 8-byte entries, no vtable word, no `live_min`/`live_max` branches, no sort.

This is deliberately *not* the fat `live` registry A0 removes: half the entry
size, no dedup, and emptied every minor cycle.

**Recorded alternative (rejected for v1).** Enumerate young with the A0 arena
walk plus a per-chunk `young_count`. Rejected because free-list reuse scatters
young boxes into old-heavy chunks (a swept young slot is recycled for a young
allocation), so the walk degrades toward O(live) exactly as the chunk fills
with promoted boxes. The walk stays for the *old* sweep, where "every box" is
the right bound.

### 4.4 The remembered set and the write barrier (A2 — the crux)

**Invariant: every old→young edge is in the remembered set.** A missed edge
is a use-after-free at the next minor collection, not a leak. This is the
highest-risk piece of the milestone and the reason A2 lands before anything
depends on it.

Barrier, on a store of `value` into `target`:

```text
if target_is_old && value_is_young { remember(target) }
```

- `value_is_young`: a tag test first (doubles and primitives exit before any
  memory access; reuse `Value::encoded_box_address`), then the header bit.
- `target_is_old`: the header bit of the target box. Most store paths already
  hold the box (the `Gc<JsObject>` receiver) or can read `self_handle`.
- **Common case is one load and a not-taken branch**: in a constructor `this`
  is young, so the target check exits immediately. Only a store into an old
  object reaches the value check.
- Remembered set: `Vec<usize>` plus a small dedup set; cleared after each
  minor collection (all young are promoted, so every pre-existing edge has
  become old->old; new edges re-record themselves).

**Store sites to instrument (the audit).** The completeness of this list is
the correctness of the milestone.

Heap-object fields, via crux:

- `JsObject`: `properties` (SmallProps inline and heap), `in_fields`,
  `prototype`, `map` (descriptor keys / transition `Map` handles),
  `ObjectKind::Array(ArraySlots)` elements, `ObjectKind::String`,
  `ObjectKind::Arguments` (`parameter_map`, `env`), `ObjectKind::Proxy`
  (`target`/`handler`), `ObjectKind::IntegerIndexed` (`buffer_object`).
- `Function`: `object`, `name`, bound target/`this`/args, `self_handle`.
- `JsString::ConsString`/`Rope`: `left`/`right`. (The flatten cache is a plain
  `OnceCell<Arc<[u16]>>` — not a GC edge, no barrier.)
- `Symbol`: `description`.
- `Map`: `prototype`, `transitions`.
- Env records: `DeclarativeEnv`/`FunctionEnv` slots, `PrivateEnvironment`.
- `Realm`: intrinsics table, global env/object (realm is old; young stores
  here are rare but must be covered).
- Slots structs: `ArraySlots`, `TypedArraySlots`, `ArgumentsSlots`,
  `ModuleNamespaceSlots`, `ProxySlots`, `PrivateElement`.

Interpreter paths that reach them: `JsObject::set_key`,
`write_data_property`, `write_data_property_slot` (the L1a cell),
`fresh_data_define`/`define_fresh`, `map_set`, `define_property_key`,
`validate_and_apply`, `array_element_write` (dense and generic),
`array_pop`/`array_set_length`, `set_prototype_of`, `private_set`,
`Function::set_key`/`define_property_key`, env `set_mutable_binding`/
`initialize_binding`/`set_value`, module-namespace writes.

JIT paths:

- Helpers (`set_member_slot`, `set_member_name`, `set_member_computed`) defer
  to the same crux writers, so the barrier lands once in Rust.
- **Inline machine-code stores that can create an edge:**
  `emit_dense_array_append_inline` (the element store) and
  `emit_deferred_hole_fill` (the object-literal property store). v1 should
  **bail the inline path to the helper when the target is old** rather than
  emit barrier code — reuse the existing guard-chain structure, measure the
  cost, and only emit the check if the bail shows up. (`emit_typed_array_
  store_inline` writes raw bytes, not a `Value` — no barrier.)
- Frame-slot and value-stack stores need no barrier: the `Vm` stacks are
  precise roots, traced every collection.

**Not needed:** agent IC value cells, job queues, and other agent tables are
**roots**, not heap objects — they are traced every collection, so a young
value in them is always found. (This is also why an old box held in a table
does not need remembering: the table root traces... nothing in it — see the
next paragraph.)

**The mark must stop at old boxes.** Tracing a root that is an old box must
not recurse: the old box's young children are exactly what the remembered set
covers. Without that rule the young mark would walk the whole old graph and
the minor collection would be O(live) again.

### 4.5 Minor collection (A3)

- Roots: the precise agent roots (visited, young children marked), the
  conservative stack scan (young words only), `scan_regions` (opaque job
  closures), and the remembered set's old boxes (their young children
  marked).
- Worklist rule: a visited child that is **young and unmarked** is marked and
  pushed; a child that is old is **not** pushed. Old boxes therefore never
  contribute to the worklist except from the remembered set.
- **Ephemeron/weak rule: an old box counts as marked.** The fixpoint's
  `key_marked` test becomes `marked_or_old(key)`; otherwise an old WeakMap key
  would read as unmarked and its young value would be swept.
- Sweep: walk `young`. Unmarked -> `drop_in_place` + free-list the slot.
  Marked -> clear the young bit (promote in place). Clear the young list.
- **Do not call `drop_unmarked_empty_maps` in a minor collection.** It prunes
  the canonical empty-map cache by reading mark bits, and a minor mark leaves
  every old box unmarked, so it would prune live old maps. Any future
  mark-bit consumer needs the same treatment — add an `is_marked_or_old`
  accessor and audit the callers.

### 4.6 Major collection

The existing full mark-sweep, enumerated by the A0 arena walk, over young and
old alike. During its trace it **rebuilds the remembered set** (record each
old box with a young child) instead of clearing it. It promotes nothing —
young stay young for the next minor cycle, or, simpler and preferred, a major
also promotes every surviving young box (so after a major the young list is
empty and the state is uniform). Decide by measurement; prefer promoting.

### 4.7 Triggers

- **Minor** per the existing safe-point budget (`allocation_budget_exceeded`
  at `Step::Jump` back-edges and `FastLoopHead`, `ir.rs`; `gc_safepoint` via
  `emit_gc_probe` in the JIT), gated on young bytes/count rather than the
  live count.
- **Major** on old-gen growth past `2x` the post-major old live count.
- `reset_allocation_budget`/`note_collection`/`BUDGET_BACKOFF` need per-level
  semantics: a minor that reclaims nothing must not disable the major
  trigger, and vice versa.

## 5. Cuts

Each cut ends clippy-clean, workspace-green, and at zero sweep regressions.

### A0 — arena-walk enumeration, registry removal (the standalone win)

The "quick workaround": remove `Heap::register` and `live: Vec` entirely.

- Header gains `vtable`; `flags: Cell<u32>` (`live` + `mark`); `GcAny` thin.
- Sweep enumerates by walking chunks (sorted by base address, so the walk is
  naturally an ascending box list — the per-collection sort disappears).
  Every slot keeps a valid `size` (swept slots included), so the walk steps
  exactly; slots beyond `FREE_CLASSES` are never reused and still step.
- `scan_regions`' `by_addr` map and the conservative scan's sorted list come
  from the walk, not a rebuilt registry.
- `live_count` becomes a maintained counter; `live_min`/`live_max` derive from
  chunk bases.
- No behavior change: still one full mark-sweep per collection.
- Gate: `cargo test --workspace`; test262; `--gc-stress`; leak harness; the
  bench rows and the target workload no worse (target: the ~3ns/alloc win).

**Landed.** `heap.rs` + `map.rs`. `GcAny` is thin and tracing goes through the
header vtable; the sweep, the stack scan's sorted address list, and
`scan_regions`' address map all come from the walk (no per-collection sort, no
`keep` Vec rebuild); `live_count` is a maintained counter and `live_min`/
`live_max` derive from the chunk range.

Landmine the header change exposed: **`Map::add_transition` /
`get_or_create_child` hardcoded the header offset** (`let header_offset: usize
= 8;` with a comment describing the old `mark`+padding+`size` layout) to build
a back-pointer handle to the parent map. With a 16-byte header that yields
`box + 8` — an interior address whose header reads as `vtable = null` — and the
mark phase crashed on it the first time a transition tree was traced. Fixed by
a new `Gc::from_payload(&T)` (`heap.rs`) that computes the box from *this
 type's* `offset_of!(GcBox<T>, data)`, so the offset can never drift with the
header again. Regression test: `map::tests::
transition_back_pointer_targets_the_parent_box`; and
`heap::tests::arena_walk_multi_chunk_varied_sizes` covers the walk across
chunks.

Validation: clippy `--workspace --all-targets -D warnings` clean;
`cargo test --workspace` green (236 crux / 768 runtime / 3324 test262); release
sweep `all` **48,464 pass, 0 fail, 158 skip, 0 crash, 0 hang** of 48,622;
`--gc-stress` clean on `language` (23,711 pass, 0 fail, 0 crash) and
`built-ins` (23,365 pass, 0 fail, 0 crash; the hangs are the documented
stress-cost clusters); leak harness bounded (`cycle` +3.9 MB, `chain`
+4.4 MB over 200k evals).

Perf: **not yet established.** The interpreter `--bench` on this machine is
noise-dominated right now — a non-allocating row (arithmetic) swung 57%
between runs and one `function calls` sample read 58.6ms against 29.7ms — so
the single-run baseline is not a usable control. Within the noise the
allocation-bound rows moved the right way (`construct churn` ~21.3 -> ~20.4,
`property access` ~10.1 -> ~9.0) and the rows that read slower
(`closure capture`, `per-iteration`) do not allocate in their timed loop.
The ~3ns/alloc win and the size-class cost need a quiet-machine A/B (and the
pinned `scratch/sconcat/pin.js` protocol) before A1 builds on it.

### A1 — generation bit + young list (no behavior change)

- Every fresh box is born young and pushed to `young`; the collector does not
  read the list yet.
- A full collection promotes all surviving young (clears the bits and the
  list).
- Gate: no behavioral change; verify the `young` list is drained every
  collection (a debug assertion).

**Landed** (`heap.rs`). `FLAG_YOUNG` (bit 2) is set at allocation
(`Gc::new`/`new_in_place` write `FLAG_LIVE | FLAG_YOUNG`); the young cohort is
a `Heap::young: Vec<usize>` (allocation-ordered box addresses), pushed by
`note_alloc` — the same heap borrow `Gc::new` already takes, so there is no
extra TLS access. Promotion is in place, folded into the sweep's existing
per-slot walk: a survivor gets `set_marked(false)` + `set_young(false)`, and
the list is cleared after the sweep. The abort path (a traced `RefCell`
borrowed mid-mark) resets marks, promotes every live box, and drains too, so
the invariant holds on every path. `Heap::young_count()` is public (tests and
future telemetry); `debug_assert_young_drained` walks the arena in debug
builds and asserts no live box is young and the list is empty — the A3 bug
net, compiled out in release.

Provenance note for A3: because the young bit is authoritative and promotion
happens in place, `is_young` must be read from the header — an address range
cannot stand in for it once a box has been promoted at a nursery address.

Validation: clippy `--workspace --all-targets -D warnings` clean; release
build warning-free; `cargo test --workspace` green (237 crux / 768 runtime /
3324 test262); release sweep `all` **48,464 pass, 0 fail, 158 skip, 0 crash,
0 hang** — byte-identical to the post-A0 sweep, i.e. behavior-neutral as
intended; `--gc-stress language` 23,711 pass, 0 fail, 0 crash; leak harness
bounded (`chain` +4.3 MB). New tests:
`heap::tests::boxes_are_born_young_and_promoted_in_place` (birth, in-place
promotion, cohort drain, next-cohort restart).

Perf: still not established (the machine remains noise-dominated). A1's only
new per-allocation work is an 8-byte cohort push, against A0's removal of a
16-byte registry push plus two range branches — the A0/A1 pair needs the
quiet-machine A/B recorded there before A5 tunes anything.

### A2 — write barrier + remembered set (no behavior change)

- `crux::heap::write_barrier(target, value)` plus the site instrumentation
  from §4.4, interpreter and JIT.
- The set is populated and ignored by the collector.
- **Exactness gate (the key intermediate).** A debug verifier run after every
  full collection: walk every old box, and assert the remembered set contains
  exactly the old boxes with a young child (no misses, no stale entries).
  Run it under `--gc-stress` across the full sweep before A3 starts.
- Gate: verifier clean; test262 unchanged.

**Landed** (`heap.rs`, `map.rs`, `object.rs`, `runtime/{env,realm,ir,jit,module}.rs`,
`jit/compiler.rs`).

*Infrastructure.* `FLAG_REMEMBERED` (bit 3) is set by the barrier and serves
as the remembered set's dedup index, so a store needs no hash lookup;
`REMEMBERED` is a thread-local `Vec<usize>` (not a `Heap` field, so the
barrier can never re-enter the heap borrow the collector holds).
`write_barrier`/`write_barrier_handle` take the *container* by reference and
derive its box from this type's own `offset_of!` — a debug-only arena-ownership
check catches a misused call site (a stack temporary).

The collector clears the set at the end of every collection (`drain_remembered`,
both the normal and the abort path) — a full collection promotes the whole
heap, so no old->young edge survives one. `verify_barrier` runs *before* the
sweep, while the young bits still describe the pre-collection heap: it traces
every live old box and asserts that each one holding a young reference is
remembered. Only misses are fatal; a remembered box with no young child is
imprecision (one extra trace), which a store that overwrote a young value with
a primitive cannot avoid without a delete barrier, so it is counted, not
asserted. The traversal saves and restores `ABORT_SWEEP`, since tracing a
mutably-borrowed `RefCell` would otherwise abort the real collection. Failures
name the offender and the child type (`VTable` gained a `name` fn pointer — a
fn pointer because `type_name` is not const-stable) — that diagnostic is what
found the sites below.

The verifier defaults on in debug builds and is forced on by `--gc-stress`
(`Agent::set_gc_stress`), so the release stress sweep audits it too.

*Sites instrumented.* crux: `JsObject::map_set`, `write_data_property`,
`write_data_property_slot`, `set_key`'s in-place branch, `define_fresh`,
`validate_and_apply` (both the append and the update, via `barrier_property`),
`deferred_field_write`, `adopt_vector_free_fields`, `array_element_write` (the
own-element and append branches), `array_element_write_dense`,
`spill_dense_array`, `set_prototype_of`, `map_add_property_cell`,
`private_element_add`; `Map::add_transition`/`get_or_create_child` (the
transition table) and `add_descriptor` (symbol keys). runtime: `EnvRecord`'s
`initialize_binding`/`set_mutable_binding`/`push_initialized_binding` (the
choke point all five env forms delegate through), `Intrinsics::define` (which
needed a realm back-reference — the table lives inside the `Realm` box, so
there is no `&Realm` to derive; a lazily installed builtin was 2956 of the
fixture misses), and the interpreter's global-store fast paths
(`store_global_value`, `update_global`).

*Validation so far.* clippy `--workspace --all-targets -D warnings` clean;
release build warning-free; `cargo test --workspace` green with the verifier on
(239 crux / 768 runtime / 3324 test262 — the fixtures would fail loudly on any
miss); a 55-fixture `--gc-stress --sample` release sweep clean. New crux
tests: `barrier_records_an_old_to_young_edge`, and
`a_missing_barrier_is_detected` (`#[should_panic]` — the verifier's own
regression test).

**JIT inline stores — landed.** Only two of the three suspected sites were
actually inline: `emit_dense_array_append_inline` (the element store) and
`emit_deferred_hole_fill` (the property-value store).
`emit_validated_member_store`'s fast path already calls the `SetMemberSlot`
helper, which runs the barrier. Each site now carries a young-target guard in
its existing inline gate: `box_ptr_is_young` reads the container's `flags` at
box offset 0 and tests `GC_FLAG_YOUNG` (exposed for the JIT, with
`FLAG_REMEMBERED`; the header is a stable ABI like `GCBOX_DATA_OFFSET`). A
young container keeps the inline path (it cannot hold an old->young edge); an
old one bails to the helper, which barriers. Two traps found by the JIT suite:
**Cranelift is not short-circuit**, so the flags load must sit in a block where
the pointer is already known valid — the dense-append check therefore lives in
the `len_check` block (past the non-null gate on `slots_base`), not in the
`probe` block beside the null test (that emitted an eager load from null and
segfaulted the JIT tests); the deferred-fill check is safe where it is, since
that block already dereferences the same box for `props_deferred`/`map`.

**The guard is proven by a biting test**, not just by reasoning:
`cli::tests::compiled_stores_into_a_promoted_container_record_the_barrier`
installs the JIT and runs the steady-capacity shape (`a.length = 0` mid-loop,
so the buffer keeps its capacity and no append ever takes the helper path —
the shape the earlier attempt missed, where a growing buffer's helper calls
recorded the box and hid the gap behind the verifier's per-box granularity).
With the guard disabled it panics
`write-barrier miss ... offenders: [("crux::object::ArraySlots", "crux::string::JsString")]`;
with it, clean.

**Initially open — found by the release `--gc-stress` sweep with the verifier
on**
(`sweep language --gc-stress`: 21,738 pass, 0 fail, **1,976 crash** — the
crashes are the verifier's panics aborting whole batches). The non-stress
suite could not reach these; every-allocation collection makes old->young
edges common enough to expose them:

| container | child | misses | likely site |
|---|---|---|---|
| `ArraySlots` | `JsObject` | 49 | bulk element writes (`elements_mut()` in the Array builtins, `Array.from`/`map`/`filter`/`fill`, `dense_index_define`) — the dense append is guarded, these are not |
| `Realm` | `SourceTextModule` | 19 | `Realm::loaded_modules` (a module recorded into an old realm) |
| `SourceTextModule` | `EnvRecord` | 16 | the module record's `environment`/`namespace` |
| `EnvRecord` | `JsObject` | 9 | the remaining env write paths (`set_slot`, `add_disposable_resource`, direct `bindings` writes) |

**Resolved — the miss list above is closed.** The one unguarded dense
element store was `dense_index_define` (the shared store behind
`create_data_property_index`, so *every* index-native bulk writer: array
literals, spread, `Array.from`, `map`/`filter`/`fill`/`slice`), which now
barriers. The verifier's per-box granularity was hiding several more sites
in the same boxes, so the whole box was audited rather than just the named
edge:

- `SourceTextModule`: `environment`, `namespace`, `deferred_namespace`,
  `module_source`, `import_meta`, `evaluation_error`, `top_level_capability`
  (all three `PromiseCapability` values), and the `cycle_root` /
  `async_parents` handles.
- `Realm`: the `loaded_modules` insert, plus every cached-value field in the
  embedded `Intrinsics` (`object_prototype`, `array_prototype`,
  `function_prototypes`, `apply_builtin`, `call_builtin`, `string_prototype`,
  `primitive_prototypes`) — they all live in the `Realm` box, so one guarded
  edge would have masked the rest. A private `Intrinsics::cache_barrier`
  funnels them through the owner back-reference (as `define` already did).
- `EnvRecord`: a record-level `set_slot` (the barrier must run on the enum,
  not the `DeclarativeEnv` payload — a target derived from the payload names
  an interior address, not the box header), `bind_this_value`, and
  `add_disposable_resource` (both the value and the method). Every
  `context_env(&env).set_slot(..)` / `declarative.set_slot(..)` call site in
  `ir.rs` and `jit.rs` now goes through the record-level method.

**Gate met.** `sweep all` (no stress) = 48,464 pass / 0 fail / 158 skip / 0
crash / 0 hang — unchanged. `sweep all --gc-stress` (verifier on) = 47,767
pass / 0 fail / 158 skip / **0 crash** / 697 hang; zero crashes means zero
verifier misses. The 697 hangs are the known `--gc-stress` cost artifact
(per-allocation O(live) collection), not barrier gaps: the same fixtures
pass with 0 hang on the non-stress sweep. `cargo test --workspace` green
(239 crux / 200 jit / 768 runtime / 3324 test262) with the verifier on. Perf:
`buildString shape` ~78 -> ~80.5ms, within this machine's noise band (the guard
bails only for an old buffer, and the bench's array is re-created young each
eval). The plan's v1 choice was bail-then-measure, so if a later measurement
shows the bail costing on a real workload, emit the barrier call on the old
branch instead of bailing.

**Caveat found in A3.** That `--gc-stress` gate is real but *narrow*: under
stress every fresh box is promoted by the collection its own allocation
triggers, so a store into an old container rarely has a young value to record.
The barrier audit in normal mode is `test262-sweep --gc-verify` (see A3).

### A3 — minor collection

- §4.5, with in-place promotion and the young sweep.
- **`--gc-verify` mode**: after each minor collection, run a full precise mark
  and assert that no box reachable from the precise roots was swept — the
  GC-7 poison-sweep idea as a built-in detector. This is the net that makes a
  barrier gap fail loudly instead of corrupting.
- Gate: `--gc-verify` clean on the full sweep; the GC-3/GC-4 unit tests still
  green; leak harness bounded; a measured pause drop on the target workload.

**Landed.** `Heap::collect_minor_with_stack` (+ the scan-free
`Heap::collect_minor` for tests) is §4.5 exactly:

- **Mark.** Seeded from the precise roots, the conservative stack scan
  *restricted to young boxes* (the young list is sorted once per minor, so the
  scan binary-searches O(young) instead of O(live)), and the remembered set.
  `seed_minor` pushes a young box as-is and traces an old one one level deep
  without marking it; `drain_young_work` marks a young box once and pushes its
  young children. **Tracing stops at an old box** — its young children are what
  the remembered set holds — so the old graph is never walked. The ephemeron
  fixpoint uses `marked || !is_young` on both ends (an old box counts as
  marked; see A4); the young dead set drives the existing weak-compaction hook
  (which therefore handles A4's FinalizationRegistry case unchanged).
- **Sweep.** Walks `young`: a marked box clears its mark and its young bit
  (promotion in place); an unmarked one drops its payload, free-lists its slot
  and decrements `live_boxes`. `for_each_live`/the arena walk is never used, so
  the cost is O(young + remembered + the roots' fan-out). Never calls
  `drop_unmarked_empty_maps` (§4.5). The abort path (`ABORT_SWEEP`) clears the
  young marks and returns without sweeping, leaving both the cohort and the
  remembered set valid for the next attempt.
- **Trigger.** `Agent::maybe_collect` runs a minor when the cohort reaches
  `MINOR_YOUNG_THRESHOLD` (8192) and a major when live passes `2x` the
  post-major live count; `--gc-stress` runs **both** at every safe point (so one
  sweep audits the barrier's old->young edges *and* the minor's generation
  rule). `collect_minor_with` deliberately does not touch
  `last_collected_live` (the major's baseline) and `note_minor_collection`
  keeps `BUDGET_BACKOFF` eager: a minor that reclaims nothing is cheap, and
  backing off would starve the major trigger that shares the safe point.

**The bug this cut exposed (and fixed): the canonical empty-map cache.**
`EMPTY_MAP_CACHE` (`crux/map.rs`) is a TLS `Vec<(Option<u64>, Handle<Map>)>`
the collector does not trace, which the *major* keeps honest by pruning it
against final mark bits (`drop_unmarked_empty_maps`). A minor had no
counterpart, so a **cache-only young map** was swept and the cache kept a
dangling handle: the next `canonical_empty_map(proto)` handed the freed box
to a new object, whose `map` field then aliased whatever reused the slot. It
presented as rare (~1 per 48k fixtures, random) wrong-property reads — e.g.
`arr[1]` falling through to the prototype's accessor — and, once the barrier
verifier was enabled in normal mode, as
`write-barrier miss ... [("crux::object::JsObject", "crux::map::Map")]`.
`drop_unmarked_young_empty_maps` is the minor's counterpart: an **old** entry
is alive by definition (a minor never sweeps the old generation), so only a
young entry the mark did not reach is pruned.

**Two verifiers, and one gap between them.**

- `minor_reachable_offender` (`--gc-verify`, forced on by `--gc-stress`): a
  full precise mark after each minor's mark, asserting no box the minor would
  sweep is reachable, naming the offender's `container -> child` types. It
  cannot see a box reachable *only* from the conservative stack scan (its mark
  has no stack scan).
- `verify_no_marks_left`: no live box may carry a mark bit after a collection.
  A minor never sets an old mark bit, but a stray one would make the next major
  skip that box and sweep its children — invisible to the reachability check,
  which reads no flags. It is clean in practice (the check was added while
  chasing the above and never fired).
- **`--gc-stress` cannot audit normal-mode old->young stores.** Its
  per-allocation collection promotes every fresh box (the fresh box is the
  stress root), so at store time the value is already old and the barrier has
  nothing to record: the A2 stress gate was real but *narrow*. `Agent::
  set_gc_verify` therefore also turns the A2 barrier verifier on, and
  `test262-sweep --gc-verify` runs it in normal mode. That is what produced the
  `JsObject -> Map` diagnostic above. **Any future barrier audit must include a
  normal-mode `--gc-verify` sweep, not just `--gc-stress`.**

**Gate met.** `sweep all` (plain) = 48,464 / 0 fail / 158 skip / 0 crash /
0 hang, three consecutive runs (identical to A2; the earlier 50%-per-run
failures are gone). `sweep all --gc-verify` (minor + barrier verifiers) =
48,461 pass / 0 fail / 158 skip / **0 crash** / 3 hang (the 3 hangs are
`TypedArray/prototype/copyWithin`, the verifier's O(live) cost pushing them
past the 15s deadline; 0 hang in the plain run). `cargo test --workspace`
green (244 crux / 200 jit / 771 runtime / 3324 test262). New tests:
`minor_keeps_a_young_box_reached_through_the_remembered_set`,
`minor_sweeps_an_unreachable_young_box`,
`a_major_after_a_minor_still_traces_the_old_generation`,
`minor_verify_detects_a_missing_barrier` (`#[should_panic]`), and
`a_minor_prunes_a_cache_only_young_empty_map` (the regression test for the
cache bug). Perf: `buildString shape` 79.5 / 81.0 / 83.1ms across three runs
(vs 78.2 / 78.2 / 80.2 before A3) — at or just outside the top of the noise
band, a possible small cost from minors pacing the allocation-heavy rows;
A5 owns the threshold tuning. The `--gc-stress` gate was not re-run to
completion after A3 (stopped by the operator; A3's changes do not touch the
major's mark or sweep); it has since been run in full — see A7.

### A4 — weak semantics under minor collection

- Young dead sets drive the weak compaction; old keys/targets count as
  marked; a young FinalizationRegistry target that dies in a minor collection
  enqueues its cleanup job; `kept_during_job` (KeepDuringJob) still spans the
  job boundary.
- Gate: the GC-3/GC-4 runtime tests extended with minor-GC-driven variants.

**Landed — no production change was needed.** Every item above was already a
*correctness precondition* of A3, so A3 implements it: the young dead set is
exactly what a minor hands to the existing compaction hook (so a young target
that dies in a minor clears its `WeakRef` entry or enqueues its cleanup job
and retains the held value), the ephemeron fixpoint tests
`marked || !is_young` on both ends (so an old key or target counts as marked),
and `kept_during_job` is traced strongly in `Agent::trace_roots`, which a minor
runs unchanged. A4's deliverable is therefore the coverage: six minor-driven
variants of the GC-3/GC-4 runtime tests —
`weak_map_ephemeron_lifetime_under_minor`,
`weak_set_ephemeron_lifetime_under_minor`,
`an_old_weak_map_key_keeps_a_young_value_across_a_minor` (the `marked_or_old`
rule: a young value under an *old* key must be promoted, not swept),
`weak_ref_target_dies_after_minor_collection`,
`weak_ref_keep_during_job_under_minor`, and
`finalization_registry_cleanup_job_runs_under_minor`. All green.

### A5 — triggers and tuning

- Split minor/major triggers, nursery sizing, per-level backoff, and the
  `--nursery-stress` mode (minor collection on every allocation, to exercise
  the barrier and promotion paths). `--gc-stress` keeps forcing **full**
  collections — it stays the strongest net.
- Gate: the bench rows and the target workload; minor pause independent of the
  live set.

**Landed.**

- **Per-level triggers.** `allocation_budget_exceeded()` is now a plain pacing
  counter (one TLS read on the back edge); the *levels* decide what to do with
  the safe point, and each carries its **own** backoff: `major_disabled()` /
  `minor_disabled()`, set by `note_collection(swept)` / `note_minor_collection(
  swept)` when a level reclaimed nothing, cleared by `reset_allocation_budget`
  (script/job boundary). GC-5's single `BUDGET_BACKOFF` multiplied the *check*
  interval, so an empty major silenced every level including the minor; the two
  levels now back off independently and neither can starve the other. The minor
  backoff is what recovered the `string concat` row (below) — without it, a
  growing-live-set loop pays a root walk every 8192 allocations.
- **Nursery sizing.** `Agent::nursery_threshold` (a `Cell`, default
  `DEFAULT_NURSERY_THRESHOLD = 8192`), settable per run with
  `--nursery-threshold N`.
- **`--nursery-stress`.** A minor at every safe point with both verifiers on,
  leaving the major on its normal trigger. **Deviation from the plan's wording**
  ("a minor on every allocation"): a per-allocation collector runs *before* the
  store that would create the edge it exists to find — the fresh box is the
  collection's own root, so it is promoted by its own allocation and the store
  is old->old. The safe point lands after a run of stores, which is where
  old->young edges actually get collected. This is the mode that exercises the
  barrier without `--gc-stress`'s age-1-everything distortion.
- **`--gc-trace`** (§8, the measurement prerequisite): one line per collection
  (`level`, `pause_us`, `live=A->B`, `swept`, `young`, `remembered`,
  `stack_words`) plus an exit summary (counts by level, avg/max pause, total
  swept, and the last minor's `live`/`young`/`remembered`). Zero cost when off
  (one TLS flag read per collection); the CLI prints it.

**Measured** (`--gc-trace` on this machine; `/tmp`-style probes, both since
removed). Two heap shapes, 4M iterations, ~16M allocations:

| probe | live across minors | minor pause (first q -> last q) | minor max | major max |
|---|---|---|---|---|
| retained **chain** (old gen grows, nothing mutated after promotion) | 15.4k -> 75k | **344us -> 326us** (flat) | 897us | 10067us |
| growing **array** (`keep.push(...)` into one old array) | 15.4k -> 200k | 309us -> 668us | 1520us | 31151us |

The gate's claim holds in the form it can hold: **the minor pause is flat in the
live set** (344us -> 326us while live tripled) *when no large old container is
repeatedly mutated* — and a minor is an order of magnitude cheaper than a major
(0.9-1.5ms max vs 10-31ms max) in both shapes.

**The measured limit — box-granular remembered sets.** In the array probe the
minor's cost tracked the *live* set, and the trace says why: `remembered` held
that one array, and marking a remembered box traces all of it, so every minor
re-scanned the whole growing array. That is the known cost of a box-granular
remembered set — a large container that keeps being mutated is O(container) per
minor forever — and it is what **A5.1** fixes.

**Bench rows** (three runs each; this machine is noise-dominated, so the
comparison is against the neighbouring readings, not an absolute):

| row | before A5 | after A5 |
|---|---|---|
| `string concat` (100k-node rope) | 1.09-1.12ms | 1.35ms -> **1.21ms** with the minor backoff |
| `buildString shape` | 79.5-83.1ms | 76.7-78.4ms |
| `indexed store` | 23.9-25.1ms | 22.0-22.7ms |
| `closure capture` / `construct churn` | 31.7-32.2 / 19.7-22.0ms | 31.7-32.2 / 19.9-20.7ms |

Leak harness bounded, unchanged from A2: `cycle` +3.7MB, `chain` +4.4MB, flat
tails over the last 80k iterations.

**Gate met.** clippy clean; `cargo test --workspace` green; `sweep all` =
48,464 pass / 0 fail / 158 skip / 0 crash / 0 hang; `sweep all --gc-verify` =
48,461 / 0 fail / 158 skip / 0 crash / 3 hang (the same `copyWithin` verifier
cost artifacts); `--nursery-stress` and `--nursery-stress --nursery-threshold 64`
clean on a mutation-heavy probe with both verifiers on. The full
`sweep all --gc-stress` long gate was outstanding here (A3/A5 leave the
per-allocation major path unchanged — under stress both levels already ran at
every safe point and the backoff was already 1); it has since been run in full,
see A7.

### A5.1 — remembered-set granularity (slot range instead of the box)

**Landed.** The A5 measurement named the limiter: a remembered *box* is re-traced
in full, so an old container that keeps being mutated costs O(container) per
minor. The fix is slot granularity without a new set structure and without a
per-store hash lookup:

- `Trace` gains two default methods. `note_dirty_slot(&self, index)` reports the
  slot a young value landed in; `trace_dirty(&self, visit)` visits the children a
  *minor* must see and **defaults to the full `trace`**, so every existing type
  keeps exactly its old behaviour and only a type that wants the bound opts in.
  The `VTable` carries both (`trace_dirty` overrides `trace` for old boxes in a
  minor; `note_dirty` rides the barrier).
- `ArraySlots` opts in with a **dirty low-water mark**: `dirty_from: Cell<(u32,
  u32)>`, the minimum element index a young value was stored at in the current
  interval, stamped with that interval's generation. The write barrier lowers it
  — on its **old-target path only**, so the hot young-store case (a constructor
  populating `this`) still pays one load and a not-taken branch. `trace_dirty`
  then visits `dirty_from..elem_len` instead of the whole buffer, so an
  append-only old array costs its newly appended elements.
- `write_barrier_element(target, value, index)` is the element-store entry point,
  used at the four dense-store sites (`array_element_write_dense`'s hole-fill,
  in-place and append branches, and `dense_index_define`). It lowers the mark and
  records the edge **together**, on the same old-target branch, so a site cannot
  do one without the other (the pairing is a correctness requirement: an edge
  with no mark means the minor traces nothing at that box).
- A stale or absent mark reads as "nothing dirty", which is sound because every
  box that survived the last collection had its young children promoted with it;
  and a *stale-but-valid* mark is a low-water bound, so it only ever
  over-traces. `COLLECTION_GEN` (bumped at the end of every collection, aborts
  included) is what makes the stamp self-expire — no per-box clear is needed.
- `seed_minor` now uses `trace_dirty` for an **old** box; a *young* box is still
  traced in full (`drain_young_work`), because its children may predate its
  promotion and the barrier notes no slot for a young target.

**Measured** — the same array-append probe, same 4.5x live growth (15k -> 200k),
before and after:

| | before A5.1 | after A5.1 |
|---|---|---|
| minor pause, first quartile (live ~39k) | 309us | **247us** |
| minor pause, last quartile (live ~177k) | 668us | **278us** |
| growth across the run | **2.16x** | **1.13x** |
| minor max pause | 1520us | **853us** |

The minor pause is now flat in the live set for the shape that used to break it —
the gate's claim without the earlier caveat. Bench rows are unchanged or better:
`string concat` 1.05-1.10ms (was 1.09-1.12 before the milestone), `buildString
shape` 77.4-77.5ms, `indexed store` 21.9-22.8ms, `construct churn` 19.4-20.0ms.

**The net.** `--gc-verify` now also asserts the bound **directly**: for every
remembered array, `trace_dirty` scans below the mark and fails if any element
there is young (`A5.1 dirty bound misses a young element at index N`). That is a
sharper check than the reachability verifier (which cannot see a child that is
reachable by another path), and it is the assertion to run after touching any
slot-reporting barrier site. Clean across the full 48,622-fixture sweep, along
with the minor reachability check and the A2 barrier verifier.

**Gate met.** clippy clean; `cargo test --workspace` green (246 crux / 200 jit /
772 runtime / 3324 test262); `sweep all` 48,464 pass / 0 fail / 0 crash / 0 hang
(three runs); `sweep all --gc-verify` 48,461 / 0 fail / 158 skip / **0 crash** /
3 hang. New tests: `a_minor_traces_only_an_old_arrays_dirty_slots` (the bound
visits one slot where the full trace visits seven, and the young element
survives the minor) and `a_minor_keeps_a_young_element_of_an_old_array` (the
real array path, end to end).

**Resolved: the `15.2.3.7-6-a-173.js` canary is a body-cache key collision, not
the collector.** It is cross-fixture, which is why a `--filter` run cannot
see it: a fixture batch runs in one worker process over a shared heap, so the
canary's batch reproduces at ~5-13% per run and the reversed pair
`(173, 172)` mirrors it exactly. A/B on the pair: minors off 19/200, **both
levels off 0/200** (it needs a collection), `--jitless` still fails, and no
verifier fires — so no box is wrongly swept, no barrier miss, no crossed
dirty bound. The nursery work is exonerated.

The cause is `shared_function_body`'s key: `(node address, realm box address,
span, source hash)`. The source hash is the only part that carries *content*;
the addresses recur across parses in one process. Every certified body runs
under a context whose `source` is deliberately `None` (`ordinary_call`'s and
`tail_prepare_ordinary`'s certified fast paths skip the clone, commented "a
certified body reads only the lexical environment" — but `capture_source` is
a reader the certification analysis does not know about), so the fixture's own
`{ get: function () { … } }` getter captured no source and the key collapsed to
the address triple. Fixtures 172 and 173 place that getter at the *same span*
(identical prefix, same-width `return N;`), so once a collection recycled the
realm box slot and the parser reused the node address, 173's getter hit
**172's** cached `Rc<Block>` and returned 2 instead of 1 — with
`hasOwnProperty("1")` still true and `length` 2, i.e. exactly the observed
symptom. In reversed order 172's `arr[1]` returns 1.

Fix: `source_hash_at` now resolves the span's source from the innermost
*enclosing* context that has one covering it (the script that owns the node),
and `shared_function_body`/`shared_accessor_body` take their content hash from
it. Verified: 0/400 plain, 0/400 reversed, 0/200 `--gc-stress` (was 9-13/200);
full `sweep all` and `--gc-verify` back at the values above; `closure capture`
and `construct churn` within their recorded bands.

**A second, pre-existing bug in the same place — now fixed.** The canary probe
showed a *certified* body running under a context with no `source` (the
`ordinary_call`/`tail_prepare_ordinary` fast paths skip the clone on the ground
that a certified body reads only its lexical environment). `capture_source` is a
reader they miss, and it is not cosmetic:

- `Function.prototype.toString` returned the synthetic `[native code]` form for
every closure a certified body created (an uncertified body, whose slow path
inherits the source, returned the real text — pure path divergence).
- **`FunctionBodyContainsUseStrict` could decide strictness wrongly.**
`directive_is_use_strict` falls back to the *cooked* value when the source is
unavailable, so `(function () { 'use str\u0069ct'; })()` inside a certified body
was treated as strict although its raw text is not a Use Strict Directive
(spec 14.1.1) — flipping `this`, `arguments` mapping and assignment errors.

Both are one root cause, so the fix is one change: `capture_source` now resolves
the span through `enclosing_source` — the innermost enclosing context carrying a
source that covers the span — the same resolution `source_hash_at` uses for the
body-cache key. Verified with `scratch/source-gap-probe.js` (identical under the
JIT and `--jitless`, with an uncertified `eval("")` control proving the
mechanism is certification, not nesting): `toString` real, escaped directive
sloppy, raw directive strict. Regression tests
`a_certified_bodys_closure_keeps_its_source` and
`a_certified_bodys_escaped_directive_is_not_strict` (both fail against the
pre-fix resolution).

Cost: the *synthesized* source is now materialized per closure in a certified
body (it was `None`). The bench closures are tiny, so the string takes
`JsString`'s inline `Small` form and the rows do not move (`closure capture`
30.5-30.9ms against the 31.7-32.2 band, `per-iteration` 9.3-9.7, `function
calls` 25.7-25.9 against 26.6-27.6); a large function literal created in a hot
loop would now pay one owned copy per closure. Leak harness still bounded
(`cycle` +3480KB, `chain` +2652KB, flat tails). Sweeps at the gate values with
the fix in place.

### A6 — JIT integration

- **`gc_safepoint` reaches both levels — verified, not changed.** The safe point
  calls `Agent::maybe_collect`, the same per-level trigger the interpreter's
  back edges call, so the minor/major decision (nursery cohort vs growth) needs
  no JIT-side branch. Covered by
  `installed_jit_compiled_loop_safe_point_runs_minors`, which lowers the nursery
  threshold so the minor level answers before the major's growth trigger and
  asserts the gc-trace records contain minors with a bounded cohort.
  **Trigger interaction worth knowing:** a compiled loop that allocates only
  garbage is *major*-driven, because the major's threshold (twice the
  post-major live count, floor 1024) stays below the garbage volume when the
  live set is small — it fires first and drains the cohort itself, so a minor
  count of zero there is correct, not a dead safe point. A minor appears once
  the live set (or a lowered threshold) makes its cohort reach the nursery
  threshold first. **Measured follow-up** (`scratch/gc-small-live-probe.js`, a
  4M-iteration garbage-only loop with a small retained set): default → 733
  collections, **all majors**, avg 139us / max 537us; with the nursery
  threshold lowered so the minor answers (512) → 2,930 minors + 1 major, avg
  27us / max 135us. **Throughput is a wash** (617ms vs 600ms best-of-3 — inside
  the machine's noise; letting majors do everything, threshold 65536, is
  618ms), so nothing is lost on time; the difference is pause *shape*: the
  minor's pause is ~5x cheaper and its tail ~4x lower, which is what the
  minor-first ordering already buys once the cohort reaches the threshold. A
  candidate change (floor the major's threshold at the nursery threshold so a
  small heap waits for the minor) is deliberately **not landed**: it buys tail
  latency in a synthetic shape at the cost of a collector-policy change that
  wants its own multi-workload probe.
- **The inline dense append's old-target bail: refined, not emitted.** The A2
  guard bailed whenever the ArraySlots box was old, but the barrier is a no-op
  for a *non-heap* value — only a heap value is an edge at all — so a container
  check gated even Number appends. Measured on `scratch/gc-old-append-probe.js`
  (1M `a[l++] = i`, young vs promoted array, JIT): **young 11ms / old 17-19ms**
  before, **12ms / 12ms** after; `--jitless` flat at ~53ms either way. The guard
  now keeps the inline path when `value is not a heap value OR the container is
  young`, so only a heap value into an old box takes the helper (which is where
  the barrier must run). `installed_jit_primitive_append_into_a_promoted_array_
  stays_inline` pins it by counting the fallback (`SetMemberComputed`):
  **100000** fall-throughs with the container-only guard, **0** with the
  refinement. Emitting barrier machine code is therefore not indicated — the
  remaining bail is exactly the case that must call the helper anyway.
- **The leaf-call-cache flush is now gated on a sweep that freed a box**
  (`crux::heap::take_swept_since_check`, accumulated by both levels'
  `note_*_collection`). The old code flushed on every budget crossing; with the
  nursery pacing minors, most crossings collect nothing, so an allocating
  compiled loop re-probed its call sites for no reason. The flush's purpose is
  address recycling (a record matches its callee by payload), which only a
  sweep produces.
- **`JIT_GC_PROBE_INTERVAL` stays 1024.** It is the budget-poll latency, not a
  cadence: the interpreter checks the same budget at every back edge and the
  machine code cannot read the crux TLS counter, so it polls. 1024 matches
  `ALLOC_BUDGET` and bounds the compiler's overshoot to roughly one probe
  interval — the cohort assertion above sees a threshold+interval cohort, not a
  runaway one. No measurement argues for moving it.
- **Gate met.** clippy clean; `cargo test --workspace` green (4921 passed,
  +2 for the A6 tests); `sweep all` **48,464 pass / 0 fail / 0 crash / 0 hang**
  with the JIT *and* with `--jitless`; `sweep all --gc-verify` 48,461 / 0 fail /
  158 skip / **0 crash** / 3 hang (the known TypedArray verifier cost);
  `--jit-bench` every row result-ok (ratios 0.08-0.81, the known band);
  `--bench` rows inside the A5.1 bands (`indexed store` 21.8-22.5, `string
  concat` 1.06-1.18, `closure capture` 31.8-32.0, `construct churn` 19.1-20.0,
  `buildString shape` 73.0-73.9).

### A7 — measurement and docs

**Locked numbers** (final build, this machine). The probes live in `scratch/`:
`nursery-minor-pause-probe.js` (4M iterations, ~2 boxes of garbage each, run
under `--gc-trace`) and `gc-trace-summary.awk` (chronological quartiles).

| shape | live across the run | minor pause, first q -> last q | minor avg / max | major avg / max |
|---|---|---|---|---|
| retained **chain** (old gen grows, nothing large mutated) | 18.9k -> 39.7k | **226us -> 183us** | 199 / 608us | 349 / 3829us |
| growing **array** (one old array appended every 16th iteration) | 78.7k -> 450.6k | **191us -> 185us** | 192 / 753us | 3483 / 45681us |

The array shape is A5.1's headline: live grew 5.7x while the minor pause stayed
flat, where the A5 run on the box-granular set went 309us -> 668us for a 2.2x
growth. Its last minor reports `young=8208 remembered=2 pause_us=173` — the
cohort at the nursery threshold, two remembered boxes. In both shapes a minor is
one to two orders of magnitude cheaper than a major.

**What did not pay** (recorded so a later session does not re-try them):

- **Box-granular remembered sets.** Correct, but a large old container that keeps
  being mutated is O(container) per minor forever — the A5 array probe's minor
  cost tracked the live set. A5.1's per-box dirty low-water mark is the fix; it
  keeps the box-level set and the free `FLAG_REMEMBERED` dedup, so no per-store
  hash lookup was needed and a card table would have bought nothing.
- **Emitting write-barrier machine code in the inline dense append (A6).** The
  measured bail (11ms -> 17ms per 1M appends) came from the *container* check
  gating primitive appends, not from the helper call; refining the guard (keep
  inline when the value is not a heap value OR the container is young) removed
  the whole penalty (12ms / 12ms) without any barrier in machine code — and a
  machine-code barrier would still need a helper for the TLS remembered-set
  insertion.
- **A single shared collection backoff (GC-5's `BUDGET_BACKOFF`).** It multiplied
  the *check interval*, so one empty major silenced every level; A5's per-level
  flags replaced it. The minor's own backoff is load-bearing: without it `string
  concat` measured 1.35ms against 1.21ms.
- **A minor at every allocation (`--nursery-stress` as a cadence).** It collects
  *before* the store that would create the edge it exists to find (the fresh box
  is its own root), so it stays a verifier; the safe-point minor is the
  deliberate deviation.
- **Moving `JIT_GC_PROBE_INTERVAL` (A6).** Nothing measured argues for it: it is
  the budget-poll latency, and 1024 matches `ALLOC_BUDGET`.
- **A0's expected ~3ns/alloc win** was never established — the machine was
  noise-dominated at the time. A0's real contribution is the arena-walk
  enumeration the sweep now uses.

**Pointers.** `.notes/gc-plan.md` and `.notes/engine-redesign.md` open with a
superseded note pointing here; `.notes/perf.md`'s memory bullet names the
generational collector.

**Gate met.** The A6 gate run is the A7 gate (docs-only after it): clippy clean,
`cargo test --workspace` 4923 passed, `sweep all` 48,464 pass / 0 fail / 0 crash /
0 hang with the JIT and with `--jitless`, `sweep all --gc-verify` 48,461 / 0 fail /
158 skip / 0 crash / 3 hang, `--jit-bench` all rows result-ok, `--bench` rows
inside the A5.1 bands.

**The long `--gc-stress` gate, closed.** Per area (per-allocation collection, the
GC-2 root audit; run after A6 at the 15s deadline the rules require):

| area | elapsed | result |
|---|---|---|
| `language` | 124s | 23,711 pass, 0 fail, 3 skip, 0 crash, 10 hang |
| `built-ins` | 322s | 23,211 pass, 0 fail, 155 skip, 0 crash, 446 hang |
| `annexB` | 30s | 1,083 pass, 0 fail, 0 skip, 0 crash, 3 hang |
| total | 476s | **48,005 pass, 0 fail, 158 skip, 0 crash**, 459 hang |

The `built-ins` hang count is inside its recorded 440-450 wobble band (the
RegExp property-escape/TypedArray/Temporal clusters), so `--gc-stress` adds no
hangs there. The 10 `language` hangs are *new* information and all cost
artifacts, not stuck loops: the fixtures run tens of thousands of direct `eval`s
or deep recursion (`comments/S7.4_A5.js` and `-A6`, `expressions/call/`
`tco-call-args.js`, four `literals/regexp/*T2.js`, three `statements/try/tco-*.js`),
and the clearest one **completes in 82s under `--gc-stress` against <1s normally
with exit 0** — it simply cannot fit a 15s deadline when every allocation
collects. Run as three areas rather than one `all` invocation (the total is
identical) purely to keep each run's wall time bounded.

## 6. Risk register

1. **Barrier incompleteness is a UAF.** Mitigation: A2's exactness verifier
   lands *before* anything depends on the set; `--gc-verify` from A3; the
   `--gc-stress` sweep. Do not start A3 until the verifier is clean across the
   suite.
2. **Mark-bit consumers that assume "unmarked = dead".** `drop_unmarked_empty_
   maps` is one; the abort-sweep mark reset, the precise dead set, and the weak
   compaction are others. Mitigation: an `is_marked_or_old` accessor and an
   audit of every `is_marked` caller.
3. **JIT inline stores are machine code.** A missed site is invisible to Rust
   review. Mitigation: prefer bailing to the Rust helper for old targets in
   v1; the verifier and `--gc-verify` are the net; sweep the JIT and
   `--jitless` paths both.
4. **The young list reintroduces per-allocation cost.** Mitigation: 8-byte
   entries, no sort, no min/max, cleared each cycle; measure against A0's
   baseline. Fallback is the chunk-count walk (§4.3).
5. **Trigger thrash.** Minor collections can be frequent; the loop back-edge
   check is a hot path. Mitigation: separate budgets per level, backoff per
   level, and `note_collection` split so an empty minor does not stall the
   major.
6. **Retention only partly fixed.** The conservative scan still pins young
   objects — but for **one cohort**, not a full live-set cycle. That is the
   measurable win; the full fix remains GC-7's precise-rooting audit.
7. **`--gc-stress` semantics.** Forcing a minor collection on every allocation
   makes every object survive (age-1 promotion), which defeats its own
   generational purpose. Keep `--gc-stress` = full collection; add
   `--nursery-stress` for the minor path.
8. **Platforms.** Non-moving keeps the scan's platform quirks (Windows
   limits, Linux maps clamp, wasm linear-memory scan) unchanged; the young
   mark only has to filter scanned words to young ones.

## 7. Validation per cut

- `cargo clippy --workspace --all-targets -- -D warnings` clean;
  `cargo test --workspace` green.
- Full release sweep at zero regressions on the previously-passing union.
- From A2: `--gc-stress` clean across the sweep, plus A2's barrier verifier.
- From A3: `--gc-verify` clean across the sweep.
- From A5: leak harness bounded on both workloads; the bench rows and the
  target workload measured.

## 8. Measurement plan (do this before A5, ideally before A0)

There is no GC telemetry today (`live_count`/`chunk_count`/`free_count` exist
but only the leak harness reads them), so every claim above is currently
unmeasurable in production.

- **`--gc-trace`** (zero cost when off): per collection — level (minor/major),
  pause microseconds, live/swept counts, young/old split, bytes allocated
  since the last collection, stack words scanned, remembered-set size, barrier
  hits. Summary at exit: counts, total and max pause, bytes reclaimed.
- **Baselines to record** on the same machine, interleaved: the `--bench` and
  `--jit-bench` rows, the leak harness (`cycle`, `chain`), and the client
  workload (the Goat scene — confirm the harness for capturing its frame
  times).
- **Success metric**: the minor pause is roughly constant as the live set
  grows, and the major collection drops to a rare, bounded event; the young
  cohort's retained garbage (scan-pinned) is reclaimed one cycle later rather
  than surviving a full sweep.
