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

## 0. Measured baselines (this worktree, release build)

| Row | current | node --jitless | gap |
|---|---|---|---|
| arithmetic | ~12.5ms | 0.6ms | ~21x |
| property access | ~27ms | 0.3ms | ~90x |
| string concat | ~6.5ms | 1.4ms | ~4.6x |
| array iteration | ~22ms | 0.6ms | ~37x |
| function calls | ~22ms | 1.1ms | ~20x |
| closure capture | ~26ms | 0.9ms | ~29x |
| per-iteration | ~8.8ms | 0.2ms | ~44x |
| construct churn | ~53ms | 3.0ms | ~18x |

Decomposition of the construct churn row (per 100k iterations):

- `Gc::new` (Box::new + TLS registration + stress checks): ~190ns/alloc,
  ~19ms. `Rc::new` on the pre-GC model was ~60ns.
- GC collections: 33 collections in isolation at ~400-500µs each (~16ms);
  the per-collection cost is `by_addr` map rebuild (~130µs at 3k live,
  ~5-7ms at 350k), realm mark (~150-260µs — the realm holds ~3000 builtin
  objects), and sweep frees (~150µs). In the full bench the string-concat
  rope (200k live nodes) inflates the heap to ~350k boxes, so the final
  collections cost 18-42ms each.
- Machinery (construct_this_object, leaf-body setup, `o.x` read): the rest.

Why the current model is capped: every allocation is a malloc plus a
thread-local map/`Vec` registration; every collection marks and sweeps the
entire heap (realm included) and rebuilds an address map; every property
access on a fresh object pays RefCell borrows and cold object-keyed caches
because each object has a fresh identity.

## 1. Goals and success metrics

- **Allocation**: `Gc::new` from ~190ns to ~20ns (bump pointer, no malloc,
  no registration `Vec` push on the young path).
- **Young churn**: a loop that allocates short-lived objects (construct
  churn) pays a scavenge proportional to the *survivor* set, not the whole
  heap. Isolated construct churn: ~53ms → target ~20-25ms (machine
  + engine-hot-path floor).
- **Property access on fresh objects**: `o.x` after `new C(i)` becomes a
  map check + offset read (no property-vector scan, no RefCell borrow,
  no cold cache miss). Property-access row: ~27ms → target ~10-15ms.
- **String concat**: the rope's 200k live nodes stop being re-marked on
  every collection (they promote to old gen and are only touched by old-gen
  collections). Row stays ≤ ~7ms.
- **Correctness parity**: all 789 unit tests, the weak-* fixtures, and the
  sweep union stay green; `--gc-stress` clean.

## 2. Part A — bump-pointer nursery + generational GC

### A0. Design decisions

- **A-D1 — Copying semi-space nursery (not non-moving).** A non-moving bump
  arena cannot reclaim the holes dead objects leave without a free list
  (measured net-neutral in GC-5) or copying. Copying is the V8 model and the
  only one that reaches the ~20ns allocation target with O(survivors)
  collection.
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

- **A5.1 — Bump arena, collections still full mark-sweep.** Swap `Box::new`
  for the arena bump; keep the collector treating the arena as one space
  (iterate `[bump base, bump end)` — contiguous, no by_addr needed for the
  young region). Gate: tests green, `--gc-stress` green, measure
  `Gc::new` (target ≤ ~60ns) and construct churn.
- **A5.2 — Semi-space + scavenge with in-box forwarding.** Implement A2 with
  promotion disabled (everything survives to the end of the scavenge; the
  to-space is the "old" side). Gate: `--gc-stress` green, weak fixtures
  green, leak harness bounded on cyclic workloads.
- **A5.3 — Promotion + old-gen.** Split survivors by age; old gen gets the
  retained mark-sweep. Gate: long-running workloads bounded, rope
  benchmarks stable.
- **A5.4 — Write barrier + remembered set.** Land the barrier on all store
  sites; verify a young-only scavenge no longer traces the old gen. Gate:
  `--gc-stress` green with a deliberately leak-detecting workload (young
  object stored into an old object, then all other roots dropped → must be
  collected).
- **A5.5 — Tunables + measurement.** Nursery size, promotion threshold,
  scavenge-vs-malloc cross-over. Gate: the benchmarks in §0.

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

- **B5.1 — `Map` type + empty-map creation, storage still Vec.** Land the
  map as a parallel shape; `JsObject.map` exists; reads still go through
  `SmallProps`. Gate: tests green, no behavior change, measure the overhead
  of the map pointer.
- **B5.2 — In-object fields + map-based read path.** Fresh objects use
  `in_fields`; `member_cell_get` re-keyed to `(map, name)`. Gate:
  property-access row improves; `Object.keys`/iteration order tests green.
- **B5.3 — Map-based store + transitions.** Add-property transitions;
  store IC re-keyed. Gate: construct churn's store becomes an in-place
  field write; `defineProperty`/`delete` tests green.
- **B5.4 — Constructor boilerplate.** Final-map pre-build from
  `construct_property_patterns`. Gate: construct churn target (§1).
- **B5.5 — Dictionary fallback + attribute forks.** Land delete/
  defineProperty/overflow semantics. Gate: full suite + sweep green.

## 4. Ordering and dependencies

- Part A and Part B are independent; each cut in either part is
  self-contained and gate-closing.
- Recommended order: **A5.1 → B5.1 → B5.2 → A5.2 → B5.3 → B5.4 → A5.3 →
  A5.4 → B5.5 → A5.5**. Rationale: A5.1 and B5.1 are low-risk foundations
  that each pay off alone; the construct-churn target needs both A5.2 (cheap
  allocation) and B5.4 (cheap stores), so landing B5.2/3 before A5.2 keeps
  the two halves independently verifiable.
- A5.4 (write barrier) is the only cut with a hard cross-part dependency: it
  must hook `SmallProps`/ConsString/env stores regardless of maps, and the
  barrier target check uses the nursery range from A5.1.

## 5. Risk register

1. **Write-barrier completeness (A5.4).** A missed old→young edge is a
   use-after-free, invisible to the conservative stack scan (the scan covers
   the native stack, not heap edges). Mitigation: enumerate every store site
   (the list in A3), and land a `--gc-stress`-style nursery-stress mode that
   scavenges after every allocation so a missing barrier fails fast.
2. **Copying collector + the conservative scan.** A stack word inside the
   nursery range that is not actually a pointer gets "rewritten" — safe (it
   was going to retain garbage either way), but the rewrite must be
   idempotent across a single scavenge (a word pointing at a *forwarded* box
   must follow the chain, not re-copy).
3. **Job closures / opaque regions.** The FinalizationRegistry cleanup
   closure and generic job closures hold captured `Value`s the collector
   sees only through region scans; the region scan must recognize nursery
   addresses (the A2 stack-word rewrite applies to the closure regions too).
4. **Map canonicalization table.** Two objects with identical shapes must
   share a Map for the ICs to hit; the table keyed by `(proto, descriptors)`
   needs invalidation when a prototype mutates (the existing generation
   mechanism covers this).
5. **Attribute/delete semantics (B5.5).** The current `ValidateAndApplyPropertyDescriptor`
   machinery is exhaustive; the map fork must reproduce it exactly. Keep the
   Vec path as the dictionary fallback and run the descriptor test corpus
   against it.
6. **Perf regression on non-target rows.** The map pointer and in-object
   fields grow `JsObject` slightly; the nursery's from/to split doubles young
   memory. Both are bounded (in_fields capped, nursery sized by measurement)
   and measured at every cut.

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
