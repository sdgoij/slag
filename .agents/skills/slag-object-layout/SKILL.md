---
name: slag-object-layout
description: Load when working on Slag's `JsObject` memory footprint, its field layout, or the GC arena's size classes — e.g. considering shrinking or boxing a field for speed or memory, or reading `mem::size_of::<JsObject>()`. Documents the arena's 16-byte granularity and free-list slot reuse, why shrinking a field is speed-neutral unless it removes real hot-path writes (`INLINE_FIELDS` 16 -> 8 paid; boxing `property_index` did not), the `Property` read-density caveat, and how to measure a layout change without fooling yourself.
---

# JsObject layout and the arena size classes

The `JsObject` payload and the allocator that holds it. This is the trap that
costs real measurement time whenever "shrink the struct" is proposed as a perf
lever.

Locations: `crates/crux/src/object.rs` (the `JsObject` fields, `SmallProps`,
`Property`, `ArraySlots`), `crates/crux/src/heap.rs` (`Gc::new` /
`Gc::new_in_place`, `round_up`, `ARENA_GRANULARITY`, `FREE_CLASSES`, the
free list and the chunked bump arena).

## 1. The footprint is a rounded size class, not a byte count

`Gc::new`/`new_in_place` round `size_of::<GcBox<T>>()` up to
`ARENA_GRANULARITY` (16 B), and every box carries a 16 B `GcHeader` (4 flags +
4 rounded size + 8 vtable). The free list has one class per 16-byte rounded
size from 16 to 4096, and **a swept slot is reused by class before the bump
advances** — so in a steady-state loop an allocation is a free-list pop, not
`0.125 ns/byte` of payload.

At `INLINE_FIELDS` 8 the `JsObject` payload is **408 B** (`mem::size_of`, no
padding): `kind` 16, `id` 8, four `Cell`-handles 8 each, `in_fields` 64,
`properties` 152 (`SmallProps`: two inline `(PropertyKey 16, Property 40)`
entries + len + the `Vec`), `property_index` 56 (`RefCell<Option<HashMap>>`),
`private_elements` 32 (`RefCell<Vec>`), plus the small tails. A guard test
pinning the payload size is the cheap way to notice a layout regression.

## 2. The trap: shrinking a field is speed-neutral

A proposed field shrink only pays for **speed** if it removes *real hot-path
writes*:

- `INLINE_FIELDS` 16 -> 8 removed 64 B of `in_fields`, which is written on
  every object init → a real 2-7% on creation.
- Boxing `property_index` (`RefCell<Option<HashMap>>` 56 B ->
  `RefCell<Option<Box<HashMap>>>` 16 B) is **neutral**: the payload fell
  408 -> 368 B (arena box 432 -> 384, ~11%) yet `{a:i}`, a 24-property literal,
  `[]` and `new Array(3)` all measured within noise. Two reasons: slot reuse
  makes the class pop the cost, not the bytes; and `RefCell::new(None)` for
  `Option<HashMap>` optimizes to an 8-byte discriminant write (the map fields
  are unread when `None`), so a smaller field does not shrink the init writes.

Do not re-open a size lever expecting speed. Clippy also rejects `Box<HashMap>`
(`box_collection`), so a memory-only box needs an explicit justification.

## 3. The one that could pay is read density

A smaller `Property` (40 -> ~24) shrinks every property-vector entry and
`SmallProps`, so if it pays it pays as **cache density on reads**, not as a
cheaper allocation. Judge it against read-heavy rows
(`objects/many_objects_read`, `opcost/prop_read`), never a create loop.

## 4. Measuring a layout change

- Isolated 1M-iteration micros, min of 5 runs.
- Run a **null control** (a binary against itself) first: ~0.99-1.01 is the
  noise floor, and same-binary noise is smaller than cross-binary noise.
- Cross-binary layout noise can exceed 1%, so a ±3% swing on one row is not
  evidence — corroborate with a second shape and a control row the change
  cannot touch (`bare`, `arith`). An `INLINE_FIELDS`-adjacent "before" binary
  is only a valid baseline if its source state matches.

## Relationship to the other skills

- `slag-dense-arrays` — the `ArraySlots` element buffer, the compiled append
  and read (the same `crates/crux/src/object.rs`).
- `slag-property-writes` — the named-member store machinery and the two store
  disciplines.
- `slag-gc-rooting` — retaining GC values in Rust locals across an allocation,
  and the arena's mark/sweep mechanics.

## 5. Latency around the create is hidden by the init

Shaving latency off the calls around an allocation does not move creation
rows, even when an isolated allocation microbench says the win is large:

- folding the allocation counter and the stress flag from two thread-locals
  into one was 6.32 -> 5.20 ns per `Gc::new` in a hot ring (-18%), and moved
  no end-to-end row at all;
- a one-entry inline cache for `canonical_empty_map` cut its lookup 2.7 ->
  1.98 ns, but `ordinary_object_create` stayed at 14.3 ns (~0.3 ns surfaced).

The payload init's stores overlap the surrounding latency. Judge this area on
the creation loop (or corpus rows), never on the helper in isolation, and
prefer changes that remove *stores* over changes that remove delay.

## 6. Making a length addressable: the `Props` mirror

The compiled member probes need `properties.len() == 0` as a machine test ("this
receiver's own keys cannot shadow the chain"). The right test is
`SmallProps::len`, but it lives inside `RefCell<SmallProps>`, and `RefCell`'s
payload offset is not guaranteed — so there is no `offset_of!` for it, and the
plan of record treated that as a reason to shrink the problem.

The workable end state is a wrapper: `Props { inner: RefCell<SmallProps>, len:
Cell<u32> }` with `borrow()`/`borrow_mut()` where the mutation guard derefs to
`SmallProps` and refreshes `len` **on drop**, and
`pub const PROPS_LEN_OFFSET = offset_of!(JsObject, properties) + offset_of!(Props,
len)`. Two properties of that shape are what make it safe to expose:

- **The maintenance is structural, not a list of sites.** There are ~18
  `properties.borrow_mut()` call sites; a bare `Cell<u8>` field on `JsObject`
  would need all of them (and every future one) to remember, and a missed one is
  a *silent* wrong value, not a compile error. The Drop guard cannot be missed.
- **`u32`, saturating.** `u32::try_from(len).unwrap_or(u32::MAX)` only turns "very
  long" into "non-empty", which is the conservative answer for a `== 0` test.

Cost: the payload goes 424 → 432 B. With the 16-byte `GcHeader` that is 448 — the
same arena class — so the mirror is free, and 432 is the **last** byte of
headroom (see the guard test, which now pins 432). Measured on unaffected rows:
+0.6 / +1.5 / +1.6% (cross-binary layout noise), with the array rows it exists for
moving -22 to -42%.

A caveat worth knowing before trusting a `named_version`-based gate instead:
array `length` defines do **not** advance `named_version` and array-length writes
are in place, so a fresh array reads `named_version == 0` — not 1.
