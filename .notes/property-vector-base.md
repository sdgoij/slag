# The property vector becomes a suffix store (`base`)

Scoped 2026-10-30, not started. Motivation, the invariant change, every reader
that has to be decided, and the slice order. The measurement that opened this is
in `.notes/perf.md` (the ninth property).

## 1. The measurement

500k iterations, one row per process, object escaping into `arr[i & 63]`:

| properties | literal | per object | `new F()` | per object |
|---:|---:|---:|---:|---:|
| 8 | 63.1 ms | 126 ns | 213 ms | 426 ns |
| 9 | 211.1 | 422 | 405 | 810 |
| 10 | 238.1 | 476 | 482 | 964 |
| 12 | 328.3 | 657 | | |

The 8 -> 9 step is +296 ns for a literal, +384 for a construct; each key past it
is ~40 ns. Both paths use the same helper, so this is the object, not the
lowering.

## 2. Why the ninth key pays

`INLINE_FIELDS` is 8 (`crates/crux/src/object.rs:170`). A fresh literal adopts its
first eight values into `in_fields` and leaves the property vector **deferred**
(`materialize_properties`, `object.rs:3545`, rebuilds it on demand from the map
descriptors plus `in_fields`).

The ninth key's descriptor ordinal is 8, so its storage is the property vector at
position 8, and the L1c transition discipline requires the vector position to
equal the descriptor ordinal. The vector therefore cannot hold only the tail: it
must be length 9 with eight entries that duplicate `in_fields`. Producing them is
`materialize_properties`, and *every* object that grows past eight properties pays
that copy once. It is the whole 115 ns step; the per-key ~40 ns after it is the
ordinary generic define.

So the vector is doing two jobs: it is the storage for ordinals at or above
`INLINE_FIELDS`, and it is a **materialized cache** of the in-field head for the
consumers that want "every own property in ordinal order".

## 3. The change

Give `SmallProps` a **base**: the descriptor ordinal of its first entry.

```rust
pub struct SmallProps {
    /// The descriptor ordinal of `inline[0]`/`heap[0]`. 0 for the classic
    /// fully-materialized vector; `INLINE_FIELDS` for an object whose in-field
    /// head is served from `in_fields` and whose vector holds only the tail.
    base: usize,
    inline: [MaybeUninit<(PropertyKey, Property)>; INLINE_PROPS],
    heap: Vec<(PropertyKey, Property)>,
}
```

`base` is only ever 0 or `INLINE_FIELDS`. That keeps every existing branch on
`INLINE_FIELDS` valid: a positional reader asks for an ordinal, and
`ordinal >= INLINE_FIELDS` implies `ordinal >= base`, so the suffix lookup is the
same arithmetic.

The accessors:

- `get(ordinal)` returns `None` for `ordinal < base` (the value is in
  `in_fields`) and `heap[ordinal - base]` above it.
- `len()` returns `base + heap.len()`, i.e. "highest ordinal + 1", so
  `map_transition_aligned` (`object.rs:1425`, `properties.len() ==
  map.descriptor_count()`) keeps exactly its current meaning.
- `push(entry)` appends at `base + heap.len()`.
- `iter()` yields `(ordinal, entry)` for the suffix only. Every caller that wants
  the *whole* object must go through §4.4.

`props_deferred` becomes derived (`base == INLINE_FIELDS && heap.is_empty()`),
not a separate bit: one state, not two.

## 4. The readers, and a decision for each

### 4.1 Positional readers (no behaviour change)

`map_field` (`object.rs:1385`), `map_set` (`1364`), `write_data_property_slot`,
`map_store_field`, `set_member_slot`, `property_slot`.

Each already branches on `ordinal < INLINE_FIELDS` to read `in_fields`. They route
the other arm through `SmallProps::get`, which now honours `base`. For the shape
this change is about, `ordinal >= 8 == base`, so the arm is unchanged; for a
classic `base == 0` object it is also unchanged.

**Decision: mechanical, no semantic change.** The one rule to write down: `None`
from `get` means "not in the vector", never "absent": the same trap the rules
already record for `Map::field_offset`. A caller that treats `None` as absent
without consulting `in_fields` returns a wrong value, not a slower one.

### 4.2 The Array length mirror (`object.rs:2400`, `2470`, `2750`, `2975`, `5128`)

`properties.first()` / `first_mut()` is the array's length mirror at ordinal 0. An
Array's vector always starts at 0 (the mirror is pushed by `array_create`, and
elements never go through the vector), so these sites are unaffected.

**Decision: `first()`/`first_mut()` are defined as ordinal accessors (`get(0)`)
and the five sites gain a `debug_assert` that the receiver is an Array.** A
non-Array reaching them would silently no-op today too, so the assert is
information, not new behaviour.

### 4.3 `map_transition_aligned` (`object.rs:1425`)

Unchanged, because `len()` is defined as the highest ordinal + 1.

### 4.4 Enumeration and iteration (the refactor's real surface)

The sites that need "every own property, in ordinal order, with attrs": the
`props.iter()` loops in `object.rs` (26 of them), `own_string_props`,
`own_property_keys`, `own_string_data_keys`, `materialize_properties` itself, and
the 18 `properties.borrow()` sites in `runtime/src/ir.rs` (`for-in` levels, the
key/values/entries builtins, spread/assign).

With `base`, the head is no longer in the vector, so these have two options:

1. **Rebuild the head into a `base == 0` vector at that point.** This is what
   `materialize_properties` does today; it becomes "rebuild, even if a tail is
   present" (it must build a fresh `SmallProps` with `base` 0, so it cannot
   prepend in place). The cost is the copy that is paid today, but only by a
   consumer that enumerates, not by every define.
2. **Walk the map for ordinals `0..base` reading `in_fields`, then the vector
   tail.** No copy at all. `materialize_properties` already computes exactly this
   for the head, so it is a matter of making the walk yield entries instead of
   storing them.

**Decision for the first slice: option 1.** It is a one-function change and it is
what today's code already does, so the enumeration consumers need no edits. A
literal that never enumerates (the measured loop, and most records) stops paying
the copy entirely. Option 2 is a follow-up if enumeration shows up hot.

### 4.5 The JIT's shape gates

`INLINE_FIELDS` appears in `compiler.rs` (7), `opt_lower.rs` (3), `lib.rs` (2),
`helpers.rs` (1), `ir.rs` (12), `jit.rs` (2), `eval.rs` (1), `function.rs` (3).
Every one is an *in-field* gate (`offset < INLINE_FIELDS`), which `base >=
INLINE_FIELDS` cannot contradict.

**Decision: no change. Verify by reading, not by editing.**

### 4.5 `property_slot` and its consumers (the sharp edge)

`JsObject::property_slot` (`object.rs:2525`) answers with a **vector position**,
not a descriptor ordinal, and it says so: "a slot is a vector position: a
vector-free object must materialize first (its own properties exist, so reporting
None would read as absent to the chain-cache walkers and slot recorders)".

The suffix store breaks that identity for an in-field property: under a base of
`INLINE_FIELDS` an in-field ordinal has no vector position at all. So
`property_slot` MUST keep materializing (it may also need to map an ordinal to
`ordinal - base` for the tail). Its consumers are the ones that make this the
sharp edge, because they cache the position and re-read the vector with it:

- the chain-cache walkers (they test whether a *prototype* link holds an
  accessor, and a wrong position reads a different property),
- the member-cell and shape probes in the JIT (`set_member_slot`,
  `emit_shape_store_probe`), where the position is compiled into a store offset,
- the interpreter's slot recorders (`runtime/src/ir.rs`).

**Decision: slice 2 does not touch `property_slot`; it keeps materializing, so
any object a chain walk or a slot probe looks at is already a base-0 vector.**
The suffix store is only visible to the paths that read the vector by ordinal
(§4.1) and to `materialize_properties`. If a later slice wants to avoid the
materialization there too, the position/ordinal distinction has to be resolved
first, and that is a bigger change than this note covers.

### 4.6 Tracing and the sweep

`Trace for JsObject` visits `in_fields` and the vector. Fewer entries in the
vector is still a subset of what is traced, and the head is traced through
`in_fields`.

**Decision: no change; the GC sweep is the net for a mistake here (a value that
stops being traced is collected and shows up as a wrong value).**

## 5. Slices

1. **Inert `base`.** Landed (`object.rs`: the field, `ordinal_len`, `get_ordinal`,
the two positional call sites, and `small_props_ordinals_shift_by_the_base`).
Gate was: inert by construction, every test and sweep green.
2. **The tail define stops materializing.** Landed (§6). `define_fresh`'s `else`
   arm reports the suffix store and takes the ordinary append at the vector
   position; `materialize_properties` rebuilds a `base == 0` vector from the map
   head plus the suffix.
3. **Map-walk enumeration** (option 2 of §4.4). Landed with slice 2 rather than
   after it: without it, every enumerating consumer of a many-key object paid the
   copy the define no longer pays, which is what sank the first attempt.

## 5b. The audit of non-materializing readers

Every `properties` access in `crux/src/object.rs` grouped by enclosing function,
checked for a `materialize_properties()` call:

| reader | sees only the tail? | disposition for slice 2 |
|---|---|---|
| `map_field`, `map_transition_aligned` | no | already base-aware via `get_ordinal`/`ordinal_len` |
| `property_slot`, `delete_key`, `own_string_props`, `ordinary_own_property_keys`, `define_fresh` | no (they materialize) | unchanged |
| `ordinary_get_own_property`, `ordinary_property_lookup`, `set_key`, `validate_and_apply`, `write_data_property`, `has_index_keyed_own_property`, `has_own_index_property`, `for_each_own_string_prop` | **yes** | add `materialize_properties()` at entry |
| `array_define_own_property`, `array_element_write`, `array_own_property_keys`, `string_own_property_keys`, `typed_array_own_property_keys`, `typed_array_delete`, `spill_dense_array` | no | Arrays and typed arrays never reach `define_fresh`, so their vector base is always 0; assert it rather than branch |
| `adopt_vector_free_fields`, `enter_vector_free` | no | the adopt/enter paths install the vector-free state; they must assert `base == 0` (a suffix store is not adoptable) |
| `props_deferred` | overlapping state | derive it from `base` (deferred = `base == INLINE_FIELDS && len == 0`) or set it false on entering the suffix store; leaving both is two sources of truth for the same fact |

That last row is the one that decides whether slice 2 is a 30-line change or a
refactor: `props_deferred` and the suffix store both mean "the vector is not the
whole story", and every consumer that tests one of them currently assumes the
other is its complement.

## 6. Hazards

- **`None` from the vector is not "absent".** Every `get` caller must fall back to
  `in_fields` below `base`. This is the one failure mode that produces wrong
  values rather than slow ones, and the sweeps may not catch it: a wrong-value path
  needs a fixture that reads a 9-key literal's ordinal below 8.
  Net it with a differential test that builds literals of 8, 9 and 16 keys and
  reads every key back, in both tiers.
- **`materialize_properties` cannot prepend.** A rebuild must construct a fresh
  `SmallProps`; `debug_assert!(props.is_empty())` at `object.rs:3557` becomes
  "empty, or a suffix that is being folded in".
- **The length mirror** is the only ordinal-0 consumer; assert it is an Array.
- **`props_deferred` is read in 6 places**; deriving it from `base` must keep
  `own_string_data_keys`'s `None` contract (it declines a non-deferred object).

## 7. Why not the alternatives

- **Raise `INLINE_FIELDS` to 16.** Already measured as a loss on box size
  (`.notes/perf.md`); it makes every object bigger to help the minority past 8.
- **Batch the tail defines.** The tail is 1 key for a 9-key literal; the cost is
  the head copy, not the number of tail keys.
- **Key the composite shape cache on the head.** Measured: no change (the
  one-entry transition hint already serves each fork step). Reverted.

## 6. Slice 2, landed (2026-10-30)

The first attempt (recorded here as reverted, the day it was reverted) paid for
its head copy in every enumerating consumer and lost the corpus. The second
attempt lands with the enumeration walk from §4.4 option 2, and the label holds:

| | before | after |
|---|---:|---:|
| 8-key literal | 51.6 ms | 49.4 |
| 9-key literal | **154.9** | **68.0** |
| 9/8 ratio | 3.00 | **1.38** |
| 8-field construct | 195.5 | 189.5 |
| 9-field construct | **343.7** | **228.1** |
| 9/8 ratio | 1.76 | 1.20 |

Corpus `mean-jitGap` 8.21 -> 8.18 over 78 rows (the 78th is the new
`objects/wide_literal`, which is the row that exercises the win: 86.2 -> 74.7 ms
against node's 10.6).

What the landed shape looks like, so this file stops describing a project:

- `SmallProps::start_at_inline_fields` is idempotent; `define_fresh`'s `else` arm
  calls it and then takes the ordinary append, which pushes at vector position
  (`ordinal_len() == descriptor_count()` is what `map_transition_aligned` checks,
  so the transition stays aligned).
- `base == INLINE_FIELDS` while `props_deferred` is set. Both bits mean "the
  vector is not the whole story", and the second is not derived from the first:
  an ordinary deferred object has `base == 0` and an EMPTY vector (the presize
  state), a suffix store has `base == 8` and a populated tail. `complete_vector`
  therefore tests `base`, not the entry count.
- `complete_vector()` at the entry of every reader that wants a vector POSITION
  or a mutable vector (§5b's "add `materialize_properties()` at entry" list, now
  landed as the cheaper `complete_vector`), and `own_entry_walk` for the one
  reader that only wants the entries (enumeration).

Three defects the sweeps found, all fixed, all worth knowing before touching this
state again:

1. `for_each_own_string_prop`'s vector-free arm skipped `offset >= INLINE_FIELDS`,
   which was sound only while a deferred object could not describe a tail ordinal.
   This is what failed `Array/prototype/Symbol.unscopables/array-find-from-last`
   (its `for-in` in `isEnumerable` saw eight of twenty-eight keys).
2. `define_fresh`'s duplicate-of-a-mapped-key arm wrote through `map_set`, which
   mirrors nothing at or above the cap, so redefining a tail key lost the value.
3. `drop_map` (three call sites clearing the map for dictionary mode) must
   complete the vector first: the head is described only by the map, and
   `own_entry_walk`'s map-less arm treats the vector as authoritative.

One near-miss worth pinning: `sync_map_after_define` compared
`properties.get(descriptor_count())`, a vector POSITION, against the key it
wanted to transition. With a base the position and the ordinal differ, so this is
`get_ordinal` now.

## 7. Slice 3, if a row shows the copy

Nothing does. The rows that enumerate many-key objects are neutral (see §6), so
the map walk stays as it is: `own_entry_walk` synthesizes the head from the map
and hands the caller `(key, enumerable)`, and no consumer needs a `Property`.
