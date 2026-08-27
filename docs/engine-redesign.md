# Engine redesign: bump-pointer generational GC + map-based object model

Engineering spec for the two structural levers identified by the construct
churn / string concat measurements (Cut 35 slice 30-31, `docs/gc-plan.md`
GC-5 "remaining structural gap to V8: bump/semi-space allocation and
register-style bytecode"). The regression net is the project standard:
`cargo clippy --workspace --all-targets -- -D warnings` clean,
`cargo test --workspace` green, and the full release test262 sweep at zero
regressions on the previously-passing union, at every cut.

The two halves are **independent** and can land in either order; Part A (GC)
is the bigger measured win on allocation-bound rows, Part B (maps) is the
bigger win on property-access rows. They compose: maps shrink the object,
the nursery makes object allocation cheap, and the constructor's property
patterns (already collected) pre-build the object's final map.

## Status (2026-08-27 — GC half substantially landed, map half not started)

Landed and committed (in order):

- `a1c8bd1` — construct-churn pre-work (property-pattern tracking, store-cache
  pre-warm, lock-free `prototype` Cell, empty-props shortcut, GC compaction
  hook skip) — see its commit body.
- `42438e3` — **A5.1**: chunked bump arena + `GcBox.size` header +
  size-classed free-list reuse + in-place payload drop on sweep.
- `2ca8cab` — **A5.1b (first half)**: sorted-`live` binary-search stack scan
  (replaces the per-collection `by_addr` HashMap rebuild) + store-then-read
  value-cache fronting in `fast_fresh_store`.

Uncommitted on top of `2ca8cab`:

- **A5.1b (second half) — direct-mapped free list**: the size-classed free
  list is now `[Vec<*mut GcBox>; 256]` indexed by `size >> 4` (sizes
  16..=4096) instead of an FxHash HashMap. The per-alloc `get_mut` was the
  sleeper cost — construct churn ~28ms → ~20ms. Commit this first.

Measured result (release, interleaved medians): construct churn ~53ms →
**~20ms**, string concat ~6.6ms → **~4.3ms**, other rows flat or better.
Collections went from ~24ms of the construct-churn run to **~2.5ms**.

The GC half's remaining idea (A5.2-A5.4, generational young GC) is now
**low ROI** (~≤2.5ms of collections left) and hits an architectural wall
(the write barrier needs cheap box access from crux mutation sites, which
`bump_generation` does not have). **The next step is Part B (maps)**, which
targets the ~19ms of allocation/init + store/read that dominate now. See
§4 for the concrete pick-up point.

## 0. Measured baselines

Pre-session baseline (this worktree, release build — the rows the plan
started from):

| Row | before | after (session end) | node --jitless |
|---|---|---|---|
| arithmetic | ~12.5ms | ~14ms | 0.6ms |
| property access | ~27ms | ~26ms | 0.3ms |
| string concat | ~6.5ms | **~4.3ms** | 1.4ms |
| array iteration | ~22ms | ~22ms | 0.6ms |
| function calls | ~22ms | ~22.5ms | 1.1ms |
| closure capture | ~26ms | ~26ms | 0.9ms |
| per-iteration | ~8.8ms | ~8.7ms | 0.2ms |
| construct churn | ~53ms | **~20ms** | 3.0ms |

Post-session decomposition of construct churn (per 100k iterations,
allocation-budget-disabled floor):

- Allocation + `JsObject` init: ~11ms (~110ns/box — `Gc::new` TLS/borrow/
  free-list-index/write/register ≈ 40-60ns, `basic_object_create` ≈ 40-60ns
  of RefCell/SmallProps struct init).
- Store `this.x =` + read `o.x` + accumulate: ~8ms (the value-cache
  fronting from `2ca8cab` made the read a cache hit; the store's probe walk
  + `fresh_data_define` borrows are near the floor for the current model).
- Construct machinery (construct_this_object, leaf-body setup): ~2ms.
- GC collections: ~2.5ms (was ~24ms — the arena + free list + sorted scan
  made them nearly free).

Why the current model is capped: `JsObject` is ~230B of RefCells + SmallProps
(init cost), every property access on a fresh object pays RefCell borrows
and cold object-keyed caches, and allocation still does 3-4 TLS accesses per
box. The map model (Part B) attacks all three at once.

## 1. Goals and success metrics

- **Allocation**: `Gc::new` from ~190ns to ~20ns (bump pointer, no malloc,
  no registration `Vec` push on the young path). **Landed** the bump + no
  malloc + direct-indexed free list; the remaining ~110ns/box is TLS/borrow
  (3-4 accesses) + the ~230B `JsObject` init. The object-init half is a
  Part B win (a map-shaped object drops most of the RefCells).
- **Young churn**: construct churn from ~53ms to ~20-25ms. **Met** (~20ms)
  without a generational collector — the arena + free-list reclamation + a
  sorted-live scan made the existing mark-sweep collections nearly free for
  this workload. The generational young GC (A5.2-A5.4) is deferred as low
  ROI (see the status block).
- **Property access on fresh objects**: `o.x` after `new C(i)` becomes a map
  check + offset read. **Not started** — Part B. Store+read is ~8ms of the
  construct-churn run; property-access row ~26ms vs node 0.3ms.
- **String concat**: ~6.6ms → **~4.3ms** (the arena + free list cut the
  rope-node allocation cost; the rope is still re-marked per collection but
  collections are cheap now).
- **Correctness parity**: all workspace tests green (3323 test262), the
  weak-* fixtures, `--gc-stress` clean, leak harness flat (cycle ~3MB growth
  over 200k evals), at every landed cut.

## 2. Part A — bump-pointer nursery + generational GC

> **What actually landed (session 1):** A5.1/A5.1b took the *non-moving*
> route — a chunked bump arena + size-classed free-list reclamation + the
> existing mark-sweep, which reached the ~20ms construct-churn target without
> a generational split (collections became nearly free once allocation was
> cheap and the sweep was a free-list push). A1-A4 below are the *deferred*
> semi-space/generational design; the free-list route sidestepped the copying
> collector's pointer-update problem entirely.

### A0. Design decisions

- **A-D1 — Copying semi-space nursery (not non-moving).** A non-moving bump
  arena cannot reclaim the holes dead objects leave without a free list
  (measured net-neutral in GC-5) or copying. Copying is the V8 model and the
  only one that reaches the ~20ns allocation target with O(survivors)
  collection. **Session-1 amendment:** the free-list route (A5.1) hit the
  target without copying; the semi-space is only worth revisiting if
  collections re-grow as a cost.
- **A-D2 — In-box forwarding pointers, not a side-table.** During a
  scavenge the mark slot becomes a forwarding pointer to the new location
  (V8 does exactly this: `mark` is either unmarked, marked, or a
  forwarded-to address). Object-field updates flow through the normal trace
  (visit returns the new address). The conservative stack scan reads the
  forwarding pointer out of the box for any word inside the nursery's
  address range — no old→new `HashMap` is needed for stack words.
- **A-D3 — Promotion by age, then old-gen mark-sweep.** Objects survive a
  scavenge by being copied; a survivor counter (or the nursery-generation
  bit) decides when an object promotes to the old gen. The old gen keeps a
  mark-sweep, now rare and incremental. This preserves the existing weak
  machinery (compaction hook, ephemeron fixpoint) unchanged.

### A1. Semi-space nursery

**Layout.** The nursery is two contiguous chunks (from-space / to-space,
each ~1-4MB to start, sized by the GC-5 allocation-budget measurement). A
thread-local bump pointer allocates in from-space. `GcBox` is unchanged in
layout (`mark: Cell<bool>` then `data`), but the mark cell now has three
states for young objects: 0 (unmarked), 1 (marked), or a forwarding address
(scavenged). A generation bit or address-range check distinguishes
young/old.

**Registration disappears from the young path.** `Gc::new` becomes: bump the
pointer, init the box, done. No `live: Vec` push, no TLS `ALLOC_SINCE_COLLECT`
needed on the young path (the scavenge is triggered by the bump pointer
reaching the chunk end). The `ALLOC_SINCE_COLLECT` budget stays for the old
gen.

**`Value` and `Handle` stay raw pointers** — copying is internal; handles
deref through the forwarding pointer only inside the collector (V8's
pointer-compression-era design is not required; correctness comes from the
scavenge updating every reference before the collector returns).

### A2. Scavenge (young collection)

Runs when the from-space bump pointer reaches the end. Steps:

1. **Roots**: agent roots (`trace_roots`), active VMs (`trace_active_vms`),
   the remembered set (A3), and the conservative native-stack scan.
2. **Copy**: for each root, copy the box to to-space (or promote), set the
   old box's mark slot to the forwarding address, and record the survivor's
   fields for tracing. Iterate the to-space worklist (the trace visit
   resolves each field through the forwarding pointer and copies on first
   sight).
3. **Update fields**: the trace visit writes the new address back into the
   field it came from (V8's `RelocationInfo`/scavenger field fixup).
4. **Update stack words**: the conservative scan's word is inside the
   nursery range → read the box's forwarding pointer → rewrite the stack
   word. Words outside the range are untouched (they are old-gen pointers or
   non-pointers; range membership makes the rewrite unambiguous).
5. **Swap from/to**, reset the bump pointer.
6. Re-run the GC-3/GC-4 weak-table compaction on the *young dead set* (the
   old boxes in from-space that were not forwarded) so WeakMap/WeakRef/
   FinalizationRegistry see young death. Old-gen weak handling stays as-is.

**Promotion.** A box that has survived N scavenges (a small age field in the
mark slot's spare bits, or the box is copied to a dedicated "old-nursery"
half) is copied into the old gen instead of to-space.

### A3. Write barrier + remembered set (generational)

Young collections must see references stored *into old-gen objects* without
tracing the whole old gen. A write barrier on every old→young edge records
the old object in a remembered set (a `Vec<Handle<JsObject>>`/box addresses,
deduplicated). The places a Value can be stored into a heap object:

- `JsObject::set_key` / `fresh_data_define` / `define_property_key`
  (property values) — the hot path.
- `SmallProps` array/`Vec` element writes.
- ConsString `left`/`right` (rope children) — the string-concat hot path.
- Env slot writes (`DeclarativeEnv::set_value` etc.), FunctionEnv captures.
- Array backing stores.
- `private_elements`, boxed wrappers, function `self`/`prototype` links.

The barrier is: if the stored value is young AND the target is old, push the
target to the remembered set. Cheap check (both addresses' generation via
range compare), amortized by the set's dedup. The barrier must be *exact*:
a missed old→young edge is a use-after-free, not a leak. This is the
highest-risk piece (see risk register).

### A4. Old-gen mark-sweep (retained, rare)

The old gen keeps the current mark-sweep (Box-based or a second arena with a
free list). Triggered by old-gen growth past a threshold, never by young
churn. The `by_addr` map rebuild now covers only the old gen — and the old
gen is stable, so either (a) keep the per-collection rebuild (cheap because
old gen is small) or (b) revisit an incrementally maintained map now that
the young-registration churn is gone. The realm (~3000 builtins) lives in
the old gen and is re-marked only by rare old-gen collections.

### A5. Cuts

- **A5.1 — Bump arena, collections still full mark-sweep. — LANDED**
  (`42438e3`). Chunked 1 MiB bump arena (address-stable `Box<[u8]>`
  chunks), `GcBox` gains a `size: u32` header (mark+size is still an 8-byte
  header, so boxes do not grow), swept boxes are **dropped in place**
  (`drop_in_place` of the payload) and their slots reclaimed on a
  size-classed free list. The in-place drop is the critical correctness
  piece: without it the free list reused slots whose payloads (Vecs,
  HashMaps, Arc buffers) leaked — the leak harness showed 526MB growth
  before the fix, flat afterwards. Construct churn ~53ms → ~35ms.
- **A5.1b (adopted as part of A5.1) — cheap scan + cheap free list.**
  First half (`2ca8cab`): the conservative stack scan resolves a stack word
  by binary-searching `live` sorted by box address once per collection,
  replacing the per-collection `by_addr` HashMap rebuild (exact — a random
  word can never be mistaken for a box). `fast_fresh_store` also fronts the
  just-defined property in `member_value_cells` so a constructor's
  `this.x =` followed by `o.x` hits without a property-vector borrow
  (isolated store-then-read ~45ms → ~39ms). Second half (uncommitted): the
  free list became `[Vec<_>; 256]` indexed by `size >> 4` instead of an
  FxHash HashMap — the per-alloc `get_mut` cost ~8ms total on construct
  churn. Construct churn ~35ms → ~20ms, concat ~4.8ms → ~4.3ms.
- **A5.2 — Semi-space + scavenge with in-box forwarding. — DEFERRED (low
  ROI).** A scavenge with promotion disabled re-copies the realm (~3000
  builtins) every collection, which is *worse* than mark-sweep for this
  workload; it only pays off with A5.3. Collections now cost ~2.5ms total
  on construct churn, so the generational machinery's ceiling is small.
  If picked up, the blocking design issue is the write barrier (A5.4): the
  collector works at the erased `GcBox` level, but mutation signals live in
  the object data (`JsObject::bump_generation` has no box/heap handle), so
  hooking the barrier cheaply needs a design pass (e.g. a box-header
  mutation bit written from the store sites that already hold the box, or a
  per-agent dirty set threaded through crux).
- **A5.3 — Promotion + old-gen. — DEFERRED with A5.2.**
- **A5.4 — Write barrier + remembered set. — DEFERRED with A5.2.** The
  store-site list in A3 stays accurate.
- **A5.5 — Tunables + measurement. — DEFERRED.**

## 3. Part B — map-based object model (V8 hidden classes)

### B0. Design decisions

- **B-D1 — `Map` is a heap object** (arena/old-gen allocated, identity
  compared), holding the ordered descriptor array, the prototype, and the
  transition tree links. Two objects with the same shape share one Map.
- **B-D2 — In-object fields.** `JsObject` gains `map: Handle<Map>` and a
  small `in_fields: [Value; N]` (V8 uses a handful of in-object slots). The
  `SmallProps` Vec stays as the dictionary-mode / overflow store.
- **B-D3 — Maps are immutable shapes; mutations fork.** Adding a property
  transitions to a child map (cached in the transition tree); attribute
  changes (`defineProperty`) and deletions fork to a new map or drop to
  dictionary mode. The existing `generation` counter keeps the runtime ICs'
  invalidation semantics.

### B1. Map type + fresh-object creation

- `Map` fields: `descriptors: SmallVec<(AtomId, usize, PropertyAttrs)>` (name,
  field offset, attributes), `prototype`, `transitions` (name → child map),
  `back_pointer`, `generation`.
- `ordinary_object_create(proto)` allocates an empty-map object: `map =
  empty_map_for(proto)` (canonicalized per prototype via the agent's map
  table, keyed by `(proto id, descriptor hash)`), `in_fields = [Hole; N]`.
- Property read: `map.descriptors.find(name)` (a small linear scan or hash;
  the IC makes this O(1) after warmup) → `in_fields[offset]` or the overflow
  store. No RefCell borrow, no property-vector scan.
- Property add: find/create the child map for `(name)` in the transition
  tree (one hash lookup), write the value at the new offset. The `generation`
  bumps only on the object whose map changed shape — but since maps are
  shared, a *shape change is one map transition*, and every object on the
  old map is unaffected.

### B2. ICs re-keyed to maps

The current caches (`member_cells`, `member_proto_cells`,
`member_value_cells`) are keyed by object id — cold for every fresh object.
Re-key to `(map_id, name) → offset`:

- **Read IC**: fresh object → map check (one pointer compare) → offset →
  field value. No proto walk (the map encodes the shape), no cold miss.
- **Store IC**: map check + offset → direct store. The "chain has no
  accessor/non-writable" verdict moves into the map: the map is only created
  when the chain is verified, and the existing generation validation covers
  chain mutation.
- The `member_value_cells` fronting cache stays (value reads with no field
  access), now keyed by map.

### B3. Constructor integration (boilerplate)

The existing `construct_property_patterns` (Cut 35 slice 30 — the compiler
already records `this.*` writes per constructor) pre-builds the
constructor's *final* map:

- First construct: create the object with the empty map; the body's stores
  transition to the final map; cache the final map on the constructor.
- Subsequent constructs: `construct_this_object` creates the object with the
  final map and `in_fields` pre-sized — every `this.x =` is an in-place
  field store (V8's FastNewObject + boilerplate). No map transitions, no
  property-vector work, no store-cache walk.

### B4. Dictionary fallback + spec semantics

- **Order**: `[[OwnPropertyKeys]]` insertion order is the descriptor order;
  the current `SmallProps` Vec stays authoritative for objects in dictionary
  mode.
- **Deletion / defineProperty**: delete removes the descriptor (tombstone or
  fork); `defineProperty` with attribute changes forks the map. Objects past
  a descriptor count threshold (V8's ~1024) drop to dictionary mode
  permanently.
- **Attributes**: `PropertyAttrs` (writable/enumerable/configurable) live in
  the descriptor; the accessor/data distinction stays in the field value
  (a tagged enum) or a descriptor kind bit.
- **Proxy / exotic objects**: keep the current `ObjectKind` dispatch; maps
  only replace the ordinary/array *property storage*, not the internal
  method dispatch.

### B5. Cuts

> **Status: not started.** This is the next session's work. The uncommitted
> free-list array (see the status block) should be committed first.

- **B5.1 — `Map` type + empty-map creation, storage still Vec.** Land the
  map as a parallel shape; `JsObject.map` exists; reads still go through
  `SmallProps`. Gate: tests green, no behavior change, measure the overhead
  of the map pointer.
- **B5.2 — In-object fields + map-based read path.** Fresh objects use
  `in_fields`; `member_cell_get` re-keyed to `(map, name)`. Gate:
  property-access row improves; `Object.keys`/iteration order tests green.
  Target: the ~8ms store+read slice of construct churn.
- **B5.3 — Map-based store + transitions.** Add-property transitions;
  store IC re-keyed. Gate: construct churn's store becomes an in-place
  field write; `defineProperty`/`delete` tests green.
- **B5.4 — Constructor boilerplate.** Final-map pre-build from
  `construct_property_patterns` (the compiler already records `this.*`
  writes per constructor — the data is in `Agent::construct_property_patterns`).
  Gate: construct churn target (§1).
- **B5.5 — Dictionary fallback + attribute forks.** Land delete/
  defineProperty/overflow semantics. Gate: full suite + sweep green.

## 4. Ordering and dependencies

- Part A and Part B are independent; each cut in either part is
  self-contained and gate-closing.
- Actual path taken (session 1 of the plan): A5.1 → A5.1b → (free-list
  array). Part A's practical wins are banked; A5.2-A5.4 are deferred.
- **Next session pick-up point: Part B.** Recommended order **B5.1 → B5.2 →
  B5.3 → B5.4 → B5.5**, in that order. B5.1 is low-risk (a parallel shape
  with no behavior change); the value lands at B5.2 (map-keyed read) and
  B5.4 (constructor boilerplate via `construct_property_patterns`).
- B5 scope warning: crux's property machinery (`ordinary_get_own_property`,
  `define_own_property`, `validate_and_apply`, `property_slot`, iteration,
  `[[OwnPropertyKeys]]` order, proxies, descriptors — ~2000 lines in
  `crates/crux/src/object.rs` + the runtime's member-cell caches) all assume
  `SmallProps` is the one store. A split store (map fields + SmallProps
  overflow) is a correctness minefield — the safest shape is B5.1 landing
  the `Map` type with **no storage change**, then B5.2 switching the read
  path, keeping `SmallProps` authoritative until the fast path is proven.
- A5.4 (write barrier) is the only cut with a hard cross-part dependency:
  it must hook `SmallProps`/ConsString/env stores regardless of maps, and
  the barrier target check uses the nursery range from A5.1. Deferred.

## 5. Risk register

**Lessons learned (session 1, all confirmed by measurement):**

0. **Free-list reuse without a payload drop leaks every swept box's
   internals.** The arena owns the slot, but `drop_in_place((*ptr).data)`
   must still run before the slot is reused — the leak harness showed 526MB
   growth until it was added. Any future free-list/arena reclamation must
   keep the payload drop.
1. **HashMap lookups on per-alloc / per-collection hot paths are expensive
   beyond their micro-cost.** The `by_addr` rebuild (~130µs/collection) and
   the free-list `get_mut` (~8ms over the construct run) both vanished when
   replaced with direct indexing / a sorted slice. When a cache is keyed by
   a bounded integer (box address range, rounded size class), prefer a
   direct-mapped structure over a hash map.
2. **Collections became nearly free** (~2.5ms of the construct run) once
   allocation was cheap and the sweep was a free-list push — re-measure
   before investing in a generational collector; the ROI moved down sharply.

**Open risks (unchanged / Part B):**

3. **Write-barrier completeness (A5.4).** A missed old→young edge is a
   use-after-free, invisible to the conservative stack scan. Deferred, but
   the store-site list in A3 stands. The blocking architectural issue: crux
   mutation sites (`JsObject::bump_generation`) have no cheap box/heap
   handle, so the barrier hook needs a design pass.
4. **Copying collector + the conservative scan.** A stack word inside the
   nursery range that is not actually a pointer gets "rewritten" — safe (it
   was going to retain garbage either way), but the rewrite must be
   idempotent across a single scavenge.
5. **Job closures / opaque regions.** The FinalizationRegistry cleanup
   closure and generic job closures hold captured `Value`s the collector
   sees only through region scans; a moving collector must recognize
   nursery addresses in those regions.
6. **Split property storage (Part B).** Two stores (map fields + SmallProps
   overflow) mean every property operation must route correctly; the
   descriptor/attribute/order corpus must run against both. Land B5.1 with
   no behavior change and keep `SmallProps` authoritative until the fast
   path is proven.
7. **Perf regression on non-target rows.** The map pointer and in-object
   fields grow `JsObject` slightly. Measured at every cut; `INLINE_FIELDS`
   capped (the construct case needs 1).

## 6. Validation per cut

- `cargo clippy --workspace --all-targets -- -D warnings` clean;
  `cargo test --workspace` green.
- Full release sweep at zero regressions on the previously-passing union.
- From A5.2 on: `--gc-stress` clean, leak harness bounded on cyclic
  workloads, and the weak-* fixtures (none force collection, per GC-0) stay
  green with new runtime unit tests exercising collection through the
  force-gc test hook.
- Per-cut benchmark gates on the §0 rows (interleaved medians, order
  rotated): no row regresses beyond machine noise; the target rows move
  toward §1.
