---
name: slag-property-writes
description: "Load when working on Slag's property-write machinery: put_value (crates/runtime/src/context.rs), the L1a warm-store cell (Agent::member_write_cells, Vm::warm_store_put/record), the JsObject in-place writers (set_key, write_data_property, write_data_property_slot), or the JIT's compiled member store (set_member_slot). Documents the two-engine generation discipline — interpreter writes MUST bump (the read-side value cells validate by generation), the JIT's write_data_property path deliberately does not — and the store-cell invalidation rules."
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

## 1. The interpreter and the JIT have DIFFERENT store-generation disciplines

- **Interpreter writes bump.** Every interpreter path that mutates an own
  property in place (`set_key`'s in-place value update, the full-[[Set]]
  define machinery, the L1a warm-store cell) bumps the object's
  generation (the slice-11 / Cut-35-slice-11 discipline). The read-side
  value cells — `member_value_cells`, `array_element_value_cells`,
  `array_length_cells`, `member_chain_cells` (its recorded links), the
  write-cell itself — validate by `(id, generation)`, so a write that
  forgets the bump serves stale values on the next read.
- **The JIT's compiled member-store fast path deliberately does NOT
  bump** (`JsObject::write_data_property` + `extern "C"
  set_member_slot`): the compiled code validated the member value cell
  (`object id + name + generation`) before the store, writes the vector
  entry (mirroring the inline field), then refreshes THAT direct-mapped
  cell with the new value at the unchanged generation. Sound only because
  `member_value_cells` is direct-mapped on `(object id ^ name) & 15` — a
  later read of the same property hits the refreshed entry, and an
  in-place value write changes no shape that other cells depend on.
- **Never "unify" the two.** Do not remove the interpreter bump to match
  the JIT, and do not add a bump to the JIT fast path. The interpreter
  read cells are shared agent state the compiled path itself probes; the
  no-bump scheme is what lets the compiled store skip the
  invalidation/re-record round trip.

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
vector write + inline-field mirror + generation bump), then re-records
the cell at the NEW generation and fronts `member_value_cells` with the
fresh value (so the immediately following read — and the compiled probe
— hits without a vector access).

- **A generation match pins the slot's content.** The cell records the
  generation AFTER the last write; the probe compares against the object's
  CURRENT generation. An own writable data property shadows the entire
  chain (spec 7.3.3 step 3 consults the chain only when the own property
  is absent — the M9 correction), so no setter/accessor tracking is
  needed. Every own-property mutation bumps, so a match means no
  redefinition/delete/accessor-conversion happened and the recorded slot
  still holds the property.
- **A cached slot is only valid under the generation gate.** Deletes shift
  later entries (`SmallProps::remove` preserves order), defines append —
  never assume slot stability across a generation change. On a miss, fall
  back to the full `[[Set]]` (or re-resolve the slot); do not write to the
  stale slot.
- **Fill points:** on a fast-path hit (re-record with the bumped
  generation) and after a cold full-`[[Set]]` write that left an own
  writable data property (`Vm::warm_store_record`, called from the tail of
  `put_value`'s successful non-setter write). Setter-invoked writes return
  early and never record — an accessor's own property is not a writable
  data cell.
- **`member_reference` builds `PropertyKey::String(id)` directly from the
  `Name` atom** — never round-trip the atom through `crux::lookup` +
  re-intern (`PropertyKey::from_js_string`); interning is injective, so
  the clone-and-rehash is pure per-write waste.
- **Restricted to Ordinary objects/functions** (`store_cell_object`):
  Arrays' `length`/canonical-index writes are exotic intercepts
  (ArraySetLength, element defines, typed arrays) that a direct vector
  write must never bypass; super references (`this_value = Some`) write
  the RECEIVER, not the base, so they never take the cell.

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
