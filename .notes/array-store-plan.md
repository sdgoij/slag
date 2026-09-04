# Array-store plan: the buildString shape and dense elements

**Status: RESOLVED (2026-09-01)** — Slice 1a was reverted by measurement
(the HashMap memo stays; a direct-mapped table must cover the ~10k working
set to avoid thrash, and the constant-key experiment's win was the
cache-hot `key.clone()`, not the memo). Slice 1b (the
`store_chain_clean` verdict cache) and Item 2 (dense elements) landed in
`2ff893e` — the plan below is the design record; the current tree is the
source of truth. Measured (release, current tree): `buildString shape`
JIT ~600-700ms → ~42-52ms, interp ~740-840ms → ~180-200ms; the RegExp
property-escape sweep cluster is 469/0/0/0 (was the sweep's slowest
"hang" cluster).
Remaining deferred follow-ups from the plan: re-densify after a spill
(explicitly "skip in v1") and the `offset_of!` true-inline JIT store
(Phase C — the helper-call store already beats the ≤100ms target, so
this is optional).

Scope: close the gap on the `test262` harness's `buildString`
(`test262/harness/regExpUtils.js`) — the fill-reset array-store loop that
historically made the RegExp property-escape fixtures the sweep's slowest
cluster. Two work items: two cheap slices (land next) and the dense-elements
milestone (the real fix). References: `.notes/perf.md` ("Interpreter per-op
floor", measured 2026-08-31) and `.notes/jit-report.md` §6 / §7 item 12.

## Measured baseline (current tree, `--jit-bench` "buildString shape" row)

3M `codePoints[length++] = codePoint` iterations with a 10k chunk reset
(`codePoints.length = length = 0`):

| mode | time | per-iter |
|---|---|---|
| interpreter | ~740 ms | ~247 ns |
| JIT (default) | ~600 ms | ~200 ns |

The JIT ratio is ~0.82 — the loop machinery is already fast (bare-loop row
0.14, ~2.5 ns/iter); both modes pay the same `array_element_write` helper.

Decomposition of the JIT path (~200 ns/iter) — *soft numbers* (this
machine shows up to 5.5x load swings; only the row totals are solid):

| component | cost/iter | evidence |
|---|---|---|
| loop machinery | ~2.5 ns | bare-loop row (0.14 ratio) |
| `index_atom` memo + `key.clone()` | ~30-45 ns | constant-key experiment (−90 ms/3M) — see Slice 1a below |
| chain walk (Array.prototype → Object.prototype `has_own_index_property`) | ~43 ns | null-proto experiment (−128 ms/3M) |
| `index_map.insert` (lazy `property_index` HashMap) | ~0 | removal experiment (no change) |
| `props.push` + length write + `bump_generation` + RefCell borrows | remainder | |

The JIT cannot close the last ~150 ns: the dense-append machinery is shared.

---

## Item 1 — Cheap slices

### Slice 1a: direct-mapped `index_atom` cache — **implemented, measured, REVERTED**

Replaced the thread-local `HashMap<u64, AtomId>` memo with a direct-mapped
table (`member_cells` pattern). Two findings:

1. A 32-entry table **thrashed to 2.8x slower**: with sequential indices
   (`a[l++] = i`, l = 0..9999), index `i` evicts `i - 32` in the same
   slot, so every index ≥ 32 misses. A direct-mapped table must be at
   least as wide as the working set (~10k for `buildString`).
2. A 16384-entry table (128 KiB TLS) measured **neutral** vs the HashMap
   (~600 ms JIT both). The memo lookup was never the bottleneck: the
   constant-key experiment's apparent win was the *constant key* making
   `key.clone()` cache-hot, not the memo.

**Conclusion:** the per-element `PropertyKey` (creation + clone ×2 per
store) is the cost, and it only goes away with the keyless dense
representation (Item 2). The HashMap memo stays (it handles any working
set without thrash).

### Slice 1b: cache the "chain is clean for index stores" verdict

**Current** (`crates/crux/src/object.rs`, `array_element_write`): every
hole-fill / append walks `prototype` links with `has_own_index_property`
(2-3 links, each a `properties.borrow()` + map/scan miss — ~43 ns).

**Change:** a per-array verdict cache on `JsObject`, following the
`ForOfFastVerdict` precedent (Cut 24/27 — revalidated against the chain
links' generations):

```rust
// in JsObject:
store_chain_clean: Cell<Option<(u32, u64, u32, u64, u32)>>,
// (own generation, proto id, proto generation, Object.prototype id, its generation)
```

- On a successful walk (all links plain Ordinary/Array, no own index prop),
  record the tuple.
- Next store: read the 2-3 generations (~2 ns each) instead of the full
  `has_own_index_property` lookups; match → skip the walk.
- Any own-property change bumps the array's generation (already true via
  `bump_generation`); a chain mutation bumps the *link's* generation →
  mismatch → re-walk. Proto replacement bumps the array's own generation
  (the Cut 22 mechanism).
- Mismatch, a longer chain, or an exotic link: no cache (the walk runs).

**Placement:** crux (the walk is inside `array_element_write`), so the field
lives on `JsObject`; the ids are `Copy` handles — no trace edge needed
(mirrors `prototype`).

**Steps**

1. Add the field; populate on walk success; revalidate on entry.
2. Unit tests: chain clean → verdict hits; a *string* prop on the chain does
   not invalidate (the verdict is index-specific); an *index* prop added to
   `Array.prototype` bumps its generation → re-walk → the store falls back.
3. Measure; expect ~600 ms → ~510 ms (cumulative with 1a → ~470 ms).

**Risk:** moderate — invalidation correctness; the generation-revalidation
pattern is already proven here.

---

## Item 2 — Dense-elements milestone (the real fix, ~60-70% on the shape)

### Goal

Append ~200 ns → ~30-50 ns; element read ~65 ns → ~10-20 ns; truncate
O(tail). The buildString cluster approaches the mainstream floor
(~10-20 ns store), and the JIT store becomes a genuinely inline machine-code
op.

### Current state (why it is slow)

`ObjectKind::Array` carries **no fields** — elements are ordinary
`(PropertyKey, Property)` entries in `JsObject.properties`
(`[length, elem0, elem1, …]`), each with a key atom, a `Property` struct,
plus the lazy `property_index` HashMap. Every append: `from_index` + key
clone + `SmallProps::push` + `index_map.insert` + length write +
`bump_generation` + RefCell borrows. Every read (`array_element_get`) pays a
fronting IC that revalidates against `generation`.

The reference model already exists: **`IntegerIndexed(Handle<TypedArraySlots>)`**
— an indexed-buffer exotic whose internal methods dispatch on the slots.

**V8 reference (vendored `v8/` checkout) — `src/objects/elements-kind.h`,
`js-array.h`, `elements.cc`:**

- `JSArray.length` is a **direct field**, not a property entry.
- Fast elements are a **keyless `FixedArray`** — the index IS the position;
  no per-element key, atom, or map. Holes are `THE_HOLE` values.
- `ElementsKind` is a monotonic ordering: `PACKED_SMI` → `HOLEY_SMI` →
  `PACKED` (tagged) → `HOLEY` → `DICTIONARY` (plus sealed/frozen
  non-extensible kinds). A kind never transitions back down, so a map
  check is a simple ordering comparison. The `DICTIONARY_ELEMENTS`
  fallback is a number-keyed hash table — the analogue of spilling to
  `properties`.
- `push`/`pop` grow/shrink the backing store directly.

This validates the `ArraySlots` design and sharpens it: adopt a monotonic
kind instead of a boolean flag — `Dense` (contiguous, no holes) → `Holey`
(the same buffer, `None` slots) → `Dictionary`/generic (spill to
`properties`). Sealed/frozen arrays can be a kind, not a per-element
attribute, if the buffer needs to answer them (v1 may still spill).

### Design

```rust
pub struct ArraySlots {
    elements: RefCell<Vec<Option<Value>>>,   // dense 0..len-1; None = hole
    length: Cell<f64>,                        // authoritative (migration below)
}
// ObjectKind::Array -> Array(Handle<ArraySlots>)
```

- Elements `0..len-1` live in the buffer. Non-index props (string keys, and
  `length` during migration) stay in `properties`.
- **Read**: buffer index + `Option` — no props borrow, no map, no key.
- **Append** (the dense path of `array_element_write`):
  `elements.push(Some(v))` + `length.set()` — no key, no Property, no map.
- **Update in place**: `elements[i] = Some(v)`.
- **Hole fill** (`a[5] = v` with a hole at 5): `elements[5] = Some(v)`.
- **Truncate** (`a.length = 0`): `elements.truncate(0)` — O(1), replaces the
  `SmallProps::truncate` path.
- **Grow** (`a.length = N`): extend with `None` holes.
- **GC**: `ArraySlots` implements `Trace` (trace the elements' values) —
  same as `TypedArraySlots`.
- **`length`** moves to `ArraySlots.length` (a lock-free `Cell`). The
  current "length is props[0]" invariant is load-bearing in ~8 places
  (`array_length`, `has_own_index_property`, `array_set_length`, the for-of
  fast path, the JIT length helper, …); each either reads the cell directly
  (hot paths) or reads props[0] only during migration.

### Phases

**Phase A — representation + hot paths.** Add `ArraySlots`, make
`ObjectKind::Array` carry it, route the three hot paths through the buffer:
`array_element_write` (own-element update + dense append),
`array_element_get` (crux + the runtime IC front), `array_length`. Keep the
generic properties path as the fallback for everything else (spill-on-miss).
Conformance sweeps must stay zero-regression — the exotic *semantics* are
unchanged, only the storage.

**Phase B — exotic operations over the buffer.** `[[OwnPropertyKeys]]`
(indices 0..len ascending + strings, per the Array exotic),
`[[GetOwnProperty]]`, `[[Delete]]` (index → hole), `[[DefineOwnProperty]]`
(index define grows length; non-writable / sparse / spilled cases),
`array_set_length` (truncate / grow the buffer), and the `property_index`
map (now only over `properties`, the non-index props). This is the invasive
phase: every internal method that reads array own props generically must be
array-aware, exactly like the `IntegerIndexed` dispatch.

**Phase C — ICs + JIT.**

- The fronting `array_element_value_cells` / `array_length_cells` become
  buffer-direct — the generation check can drop (the buffer is
  authoritative), or stay only to invalidate cached slots elsewhere.
- `fast_array_element_write` becomes a true inline store: `offset_of!` on
  `ArraySlots` (the `VM_COMPLETION_OFFSET` pattern already proves the JIT
  can reach into such structs) — no helper call for the hot shape.
- The JIT's `array_element_get` / `array_length` helpers read the cell
  directly.

**Phase D — transitions + edge cases.**

- **Spill** (dense → generic): non-writable element define
  (`Object.defineProperty(a, 0, { writable: false })`), frozen/sealed
  arrays, index ≥ 2³²−1, an accessor or own index prop appearing on the
  chain, `a.length` set over non-configurable elements. Spill = materialize
  the buffer into `properties` entries and clear the dense flag; the array
  then behaves exactly as today.
- **Re-densify**: optional, skip in v1 (once spilled, stay generic until a
  truncate).
- **Proxies**: a proxy targeting an array reads the target via its internal
  methods — the buffer must answer those (Phase B covers it); the proxy's
  own elements never go dense.
- **`array_element_write`'s chain walk**: with the buffer, an append no
  longer walks for the write itself, but the walk's *semantics* (a chain
  accessor must intercept) still gate the dense path — if the chain has an
  own index prop / accessor, spill or fall back. Slice 1b's verdict cache
  becomes this gate; the two items compose.

### Validation

1. Every phase: `cargo test --workspace` + clippy `-D warnings` clean, plus
   the JIT/jitless release sweep comparison (the verdict-identity check) —
   the milestone changes storage, never semantics.
2. The `buildString shape` bench row: track interp/JIT each phase (target:
   JIT ≤ ~100 ms for 3M, ~30 ns/iter).
3. The RegExp property-escape cluster sweep: hang count and per-fixture
   time.
4. New unit/e2e tests: hole semantics (`a[5]` undefined, `5 in a` false),
   length interplay (append grows, define shrinks, delete is a hole), spill
   triggers (frozen, non-writable, proxy), GC trace through the buffer, JIT
   inline-store parity.

### Risks

- **Phase B is the trap**: the Array exotic's internal methods are subtle
  (length/index interplay, own-keys order, define-on-index growing length,
  delete pinning). Mitigation: keep the generic path as the reference —
  spill on any shape the buffer cannot represent exactly, so the buffer is a
  *fast path*, never a *second implementation*.
- **The "props[0] is length" invariant** is referenced in ~8 places (crux +
  runtime + jit helpers); Phase A must sweep them all — the compiler will
  not catch a stale one.
- **Generation semantics**: buffer writes currently bump `generation` to
  invalidate the slot caches. If the buffer is authoritative the fronting
  caches can drop the check, but other caches keyed on the array's
  generation (member cells for its string props, the for-of verdicts) still
  need it — do not remove `bump_generation` until Phase C proves which
  caches are buffer-bound.

---

## Recommendation

Land **Slice 1a** first (~30 min, ~15%) — a pure cache-shape change that
helps every array store in the engine. **Slice 1b** is worth doing alongside
(its verdict cache becomes Phase D's dense gate). Then scope **Item 2** as
its own milestone (Phases A-D); the `IntegerIndexed` precedent is the
blueprint, and the spill-on-miss design keeps the risk contained.
