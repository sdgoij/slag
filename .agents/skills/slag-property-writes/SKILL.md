---
name: slag-property-writes
description: "Load when working on Slag's property-write machinery: put_value (crates/runtime/src/context.rs), the L1a/L1c warm-store cell (Agent::member_write_cells, Vm::warm_store_put/record, JsObject::write_data_property_slot/map_store_field), the JsObject in-place writers (set_key, write_data_property, deferred_field_write), the JIT's compiled member store (set_member_slot, emit_deferred_field_store), or the vector-free (Option-3) store paths. Documents the two in-place store disciplines (bump-on-mutate vs no-bump + refresh the read cell), the store-cell invalidation and pinned-field rules, and the trap that JsObject::property_slot materializes a vector-free object."
---

# Slag property-write machinery traps

The write side of member/property access is shared machinery for both
engines: every interpreter member write funnels through
`put_value` (step path, register ops, updates, destructuring), and the
JIT reaches the same crux writers through `call_slow` fallbacks and its
own compiled fast stores. These are the traps that cost real debugging
time; the release sweeps are the backstop for this area (a local fix can
regress `put_value`/`get_property_key`/`find_ecma_accessor` behavior in
any fixture — sweep all three areas and diff the fail+crash union against
the parent, per the `slag-conformance` skill).

## 1. The two in-place store disciplines: bump, or no-bump + refresh

A write path picks one of these and must not be half of each.

- **The BUMP discipline** — `JsObject::set_key`'s in-place value update,
  `array_element_write*`, the full-[[Set]]/define machinery, delete,
  accessor conversion, map transition, `set_prototype_of`: mutate, then
  `bump_generation()`. The read-side caches — `member_value_cells`,
  `array_element_value_cells`, `array_length_cells`, `member_chain_cells`
  (its recorded links), `global_value_cells`, and the write cells —
  validate by `(id, generation)`, so the bump is what invalidates them.
  Nothing has to be refreshed; they miss and re-resolve.
- **The NO-BUMP + REFRESH discipline** — `JsObject::write_data_property`,
  `write_data_property_slot`, `deferred_field_write`, the interpreter's
  L1a/L1c warm-store cell, the compiled `set_member_slot`, and the compiled
  vector-free field store. An in-place VALUE write does not bump: an own
  writable data property shadows the whole chain (spec 7.3.3 step 3
  consults the chain only when the own property is absent), and a value
  write changes no shape any cell depends on. Instead the WRITER fronts
  the read-side value cell with the new value at the unchanged generation
  and, when the receiver is the global object, refreshes the name-keyed
  `global_value_cells` entry (`Vm::refresh_global_read_cell`) — so the next
  read, and the compiled probe, hit without a re-resolve. Sound because
  `member_value_cells` is direct-mapped on `(object id ^ name) & 15`: the
  refreshed entry is the one the next read of that property probes.
- **A no-bump write MUST refresh (and skip the global-cell refresh only by
  declining the global receiver).** The two disciplines are not
  interchangeable within one path: the bump variant invalidates, the
  no-bump variant refreshes. A no-bump write that forgets the refresh
  serves stale values; a bump write that also refreshes is merely wasteful.
  The interpreter's L1a warm-store cell and the compiled store are the SAME
  discipline (both no-bump + refresh) — that is not a pair to "avoid
  unifying", it is exactly what lets the compiled store skip the
  invalidation round trip.
- **Structural changes always bump.** A define (even a no-op one —
  `define_property_key` over-bumps), a delete, an accessor conversion, a
  map transition, a prototype change. A cache's generation gate is
  therefore also a shape gate.

## 2. The L1a warm-store cell (interpreted member writes)

`Agent::member_write_cells` caches `(id, name, generation, slot)` —
"at this generation, `name` is an own writable data property of `id` at
property-vector `slot`". The cell is probed from two places, both gated
on a string atom:

- `assign_member` (crates/runtime/src/ir.rs) — its Assign, logical-assign,
  and compound branches probe FIRST, before `fast_fresh_store` and
  `member_reference`/`put_value`, so a hot write to an existing own
  writable data property skips the fresh-store map check, the Reference
  build, and the `put_value` call layer entirely.
- `put_value` (crates/runtime/src/context.rs) — for its other callers
  (updates, destructuring, eval, register-store fallbacks), gated on
  receiver == base (`reference.this_value.is_none()`) on an Ordinary
  object/function.

On a hit the write calls `JsObject::write_data_property_slot` (O(1)
vector write + inline-field mirror; NO generation bump — §1's no-bump
discipline), then re-records the cell at the UNCHANGED generation and
fronts `member_value_cells` with the fresh value (so the immediately
following read — and the compiled probe — hits without a vector access).

- **A generation match pins the slot's content.** The cell records the
  generation AFTER the last write; the probe compares against the object's
  CURRENT generation. An own writable data property shadows the entire
  chain (spec 7.3.3 step 3 consults the chain only when the own property
  is absent — the M9 correction), so no setter/accessor tracking is
  needed. Every STRUCTURAL mutation bumps (a value write keeps the
  generation, but it also keeps the slot), so a match means no
  redefinition/delete/accessor-conversion happened and the recorded slot
  still holds the property.
- **A cached slot is only valid under the generation gate.** Deletes shift
  later entries (`SmallProps::remove` preserves order), defines append —
  never assume slot stability across a generation change. On a miss, fall
  back to the full `[[Set]]` (or re-resolve the slot); do not write to the
  stale slot.
- **Fill points:** on a fast-path hit (re-record at the unchanged
  generation) and after a cold full-`[[Set]]` write that left an own
  writable data property (`Vm::warm_store_record`, called from the tail of
  `put_value`'s successful non-setter write). Setter-invoked writes return
  early and never record — an accessor's own property is not a writable
  data cell. `warm_store_record` resolves the slot through
  `JsObject::property_slot`, which MATERIALIZES a vector-free object (see
  §4) — reach it only from a path that already materialized or that is
  running on a vector-authoritative object.
- **`member_reference` builds `PropertyKey::String(id)` directly from the
  `Name` atom** — never round-trip the atom through `crux::lookup` +
  re-intern (`PropertyKey::from_js_string`); interning is injective, so
  the clone-and-rehash is pure per-write waste.
- **Restricted to Ordinary objects/functions** (`store_cell_object`):
  Arrays' `length`/canonical-index writes are exotic intercepts
  (ArraySetLength, element defines, typed arrays) that a direct vector
  write must never bypass; super references (`this_value = Some`) write
  the RECEIVER, not the base, so they never take the cell.

### The L1c pinned-field mirror (the store cell pins the inline field too)

- The write cell also records `(map_id, field)` — the (map id, in-object
  field offset) the object's map assigned the property
  (`JsObject::map_store_field`) — next to the vector slot. On a fast-path
  hit `write_data_property_slot` receives `pinned_field: Option<(u64,
  usize)>` and mirrors the value into `in_fields[field]` after one map-id
  compare, instead of `map_set`'s per-store descriptor scan.
- **The generation gate pins the map exactly as it pins the slot.** Every
  structural change bumps: defines through `define_property_key`'s entry
  bump (function defines delegate to the object part), deletes, accessor
  conversions under `validate_and_apply`, map transitions. A map id pins
  the descriptor layout — maps are immutable after creation, and deleting
  a MAPPED key drops the whole map (`drop_map_if_mapped`), it never
  reorders descriptors.
- **Keep the write-time map-id re-check in `write_data_property_slot`.**
  Under the generation gate it cannot fail, but it is the backstop that
  keeps a missed bump from writing a stale field offset; a mismatched or
  dropped map falls back to `map_set`. Do not delete it to save a compare.
- **Record derives, the fast-path re-record reuses.** `warm_store_record`
  derives `(map_id, field)` once (cold, after a full-[[Set]] write); the
  hit-time re-record keeps the recorded pair — a value write changes no
  shape.
- **A live map can coexist with vector-only own properties.** Past
  `INLINE_FIELDS` descriptors, fresh defines spill to the vector while
  the map stays live describing the inline fields (map full ≠ dictionary
  mode), and non-w/e/c data defines (`defineProperty` with non-default
  attributes) never enter the map. Those properties record
  `field = MEMBER_WRITE_FIELD_NONE` and take the `map_set` scan on each
  warm write — correct but slower; never pin an offset the map does not
  own.
- **`MemberWriteCell` is interpreter-only** — the JIT never reads the
  write cells (only `member_value_cells`), so its layout is free to
  extend. The value cells stay the `#[repr(C)]`-visible ABI; do not touch
  those.

## 3. Adding a path that mutates the property vector

- A new interpreter path that writes a property-vector entry in place
  (`*slot = value` on `props`) or defines an own property MUST bump the
  generation, and should mirror the inline field when the key is mapped
  (`map_set` — the map read path serves `in_fields`, so a stale field
  would win over the vector). `write_data_property_slot` is the template:
  kind gate, `props.get_mut(slot)` with a stored-key re-check, value
  write, `map_set`, `bump_generation`.
- When refreshing `member_value_cells` use the SAME index formula as the
  compiled probe: `(object_id ^ name) & (MEMBER_CELLS - 1)` on the
  `#[repr(C)]` table. The JIT reads the cells at fixed offsets
  (`offset_of!` in `crates/jit/src/compiler.rs`); changing
  `MemberValueCell`'s layout or the table's index math without updating
  the compiled probe silently breaks the inline read fast path.
- The record/refresh helpers re-read the object's current value and
  generation rather than trusting the value that was handed to the write:
  a full-`[[Set]]` may have run a setter or failed, so only a vector read
  at fill time is exact.

## 4. `property_slot` MATERIALIZES a vector-free object

`JsObject::property_slot` resolves a *vector* position, so its first statement
is `self.materialize_properties()` — on a `props_deferred` (Option-3
vector-free) receiver it rebuilds the property vector from the map descriptors
and clears the bit. That is exact, but it destroys the state: any record or
refresh path that resolves a slot through `property_slot` turns a cheap
`deferred_field_write` (one `in_fields` store) into a full vector rebuild.

This is why routing the compiled `set_member_slot` through
`Vm::warm_store_put`/`warm_store_record` regressed: the record path called
`property_slot`, materializing every constructor-created object on its first
write; the write cell's hit path also fails on a deferred receiver
(`write_data_property_slot`'s vector `get_mut(slot)` finds the empty vector).
**Never call `property_slot` on a receiver whose `props_deferred` may be set.**

The authoritative storage for a deferred receiver is `in_fields[ordinal]`, and
the ordinal IS the map-descriptor offset — which the shared `(map_id, name)`
shape cell already records as `MemberMapCell::slot`. So the store needs no new
cell at all: probe the shape cell and write the field. The compiled
`emit_deferred_field_store` (crates/jit/src/compiler.rs) serves both cases from
a shape-gate hit:

- a presize HOLE with every higher field also a hole is the `define_fresh`
  described-key fill: write the field, **bump**, refresh the value cell at the
  new generation, and gate the prototype chain (`MemberMapCell::clean` plus the
  recorded direct prototype) because a define must consult the chain;
- a WRITTEN field is the `deferred_field_write` in-place update: write the
  field, **do not bump**, refresh the value cell at the unchanged generation,
  and gate on `MemberMapCell::writable` (recorded from the map descriptor at
  both record sites; a map id pins the descriptor attrs since maps are
  immutable), on no-barrier-needed (a young receiver or a non-heap value), and
  on the receiver NOT being the global object. An own writable data property
  shadows the chain, so there is no chain gate; the global exclusion is
  load-bearing because a no-bump write cannot refresh the name-keyed
  `global_value_cells` entry the way the helper's `refresh_global_read_cell`
  does.

The step and register stores both route their value-cell HIT (a warmed read,
where the helper was still charged) through this same path via
`emit_deferred_inline_store`.
