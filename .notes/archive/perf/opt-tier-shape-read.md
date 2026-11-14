# The opt tier's named read lacks the shape fast path

**Status: implemented 2026-10-10.** `Op::MemberGuard`'s miss now probes the
map-keyed shape cell before falling to `get_member_name`. `prop_read` 370.5 →
267.2 ms (−28%); `get_member_name` ×140M → 0; corpus `mean-jitGap` 20.2 →
**19.00**, `mismatches 0`; `language` 23726/0/0/0, `built-ins` 23820/0/1/0.

**The bug the first cut had (and the fix):** `slow` is reached both for a
non-object receiver (the tag gate) and for a plain object whose value cell
missed, so the shape arm must re-check `is_plain_object` before dereferencing
`object.map` — the first cut dereferenced unconditionally and segfaulted
(0xC0000005) on the corpus. The map cell is populated by the interpreter's own
read and the compiled `GetMemberName` fallback (both route through
`member_cell_get`), so no new recording site was needed. Landed as
`cells::member_map_cell_addr` + the `MemberGuard` shape arm (opt_lower.rs).
The same arm is now ALSO in the opt tier's member STORE
(`emit_member_store` → `emit_shape_store_probe`, a mirror of
`compiler.rs::emit_validated_member_store`): on a value-cell miss a shape hit
routes to the existing `SetMemberSlot`. Gates: `language` 23726/0/0/0,
`built-ins` 23820/0/1/0, clippy clean, `cargo test --workspace` green, corpus
`mismatches 0`.

**The store arm works when the read-shape cell is warm** — proven by
`poly_store3` (read-then-write `objs[i&255].x`): `SetMemberName` 140M → **0**,
`SetMemberSlot` per store, read helper 1. But a **store-only** loop never warms
it: `poly_store2` (`objs[i&255].x = i`) still shows `SetMemberName` ×140M and
opt ≈ per-step. I tried warming the read-shape cell from `warm_store_record` (the
store tail) — it did **not** take effect: the compiled cold store's
`assign_member` path does not reach `put_value`'s `warm_store_record` tail, so
the cell is never recorded for a store-only loop. That edit was reverted (unproven,
un-swept). **The real store-only lever** is the user's option 2: probe
`member_write_map_cells` (the write cell, recorded by `warm_store_record`
regardless of path) in the opt store — but that couples the JIT to the
interpreter-only write-cell layout, so it needs a deliberate decision (or port
`emit_deferred_field_store`'s write-cell gate).

---

The per-step member read has a shape
(map-id-keyed) fast path; the optimizing tier's does not. So a hot loop over
many same-shape objects thrashes the object-id-keyed value cell and calls
`get_member_name` on every read — the single biggest object-model gap
(`arrays`... `objects/prop_read`-shaped rows).

## 1. The two tiers' named reads

**Per-step** (`compiler.rs::emit_member_cell_probe`, ~L1455–1725) has three arms:

1. the **value cell** — `member_value_cells[(id ^ atom) & 15]`, validated on
   `(id, name, generation)`;
2. the **shape cell** — `member_map_cells[(map_id ^ atom) & 15]`; a hit reads
   `in_fields[slot]` inline when `slot < INLINE_FIELDS`, else calls
   `GetMemberMapSlot`. Keyed by the map, so it is valid for **any object count**
   (the shape's map is immutable and pins the descriptor layout);
3. the `get_member_name` helper.

**Optimizing tier** (`opt_lower.rs`): `Op::MemberCellLoad` (value cell) +
`Op::MemberGuard` (validates the cell; on a miss → `Helper::GetMemberName`).
**No shape arm.** `Op::MemberLoad` also goes straight to the helper.

## 2. Why it bites: `scratch/struct/prop_read.js`

```js
var objs = ...256 objects { x, y }...;
for (var i = 0; i < 20000000; i++) s = (s + objs[i & 255].x) | 0;
```

The IR (opt tier):

```
v18 = ElementLoad(v14, v17)          // objs[k]
v20 = MemberCellLoad(v18) atom652    // .x — the object-id value cell
v21 = MemberGuard(v18, v20) atom652  // miss → get_member_name
```

256 objects of ONE shape → the 16-slot id-keyed value cell misses on ~every
read → `MemberGuard`'s slow branch → `get_member_name`. Measured: 370 ms,
`get_member_name` ×140M (once per read), **27× V8** (`scratch/struct/`). The
per-step tier would serve it from ONE map cell.

## 3. The map cell is already populated — the opt tier just never probes it

Recording happens in `Vm::member_cell_get_map` (ir.rs ~L5171, via
`member_cell_warm_probe`), called from `member_cell_get`, which the shared
`get_member_name` (ir.rs ~L11014) calls. The compiled `GetMemberName` helper
(`jit.rs`) routes through `get_member_name`, so the **first** read records the
map cell and every later read of any instance of that shape can hit it. So the
port needs **no new recording site** — only the read probe.

(Caveat: `member_cell_get_map` returns early when the receiver has no map, so
the port only helps map-bearing receivers. Object literals do have a map; a
purely vector/dictionary-mode object still falls to the helper, as in the
per-step tier.)

## 4. The port

Add the shape arm to `Op::MemberGuard`'s `slow` block, mirroring
`compiler.rs`'s `shape`/`map_present`/`map_hit`/`field`/`overflow` blocks
(~L1595–1725):

```
slow:    load object.map  ; map == 0 → helper
map_present:  load map_id (MAP_ID_OFFSET)
              probe member_map_cells[(map_id ^ name) & 15]
              match cell.map_id && cell.name ? map_hit : helper
map_hit:      slot < INLINE_FIELDS ? field : overflow
field:        read in_fields[slot]; hole (UNINITIALIZED_BITS) ? helper : merge
overflow:     GetMemberMapSlot(object, name, slot) → merge
helper:       GetMemberName → merge
```

- **Factor it into `cells.rs`, not a fork.** `cells.rs`'s module doc already
  states the single-source rule ("both tiers must compute the same cell address
  and validate it the same way"); add
  `cells::emit_shape_member_read(builder, ctx, object, name, miss) -> Value`.
  Port `compiler.rs`'s inline blocks to call it so the two never drift.
- The `member_map_cells` base is already on `JitCallContext`
  (`offset_of!`); the guard's `abi.vm` is the ctx.

## 5. Watch list / gates

- Rows: `scratch/struct/prop_read`, `opcost/obj_prop` (LICM'd — must stay
  fast), `objects/own_read`, `control/for_in`, `objects/warm_store`.
- The read-cell contracts the skill names: `the_emitter_and_runtime_computed_
  read_slots_agree`, the in-place-store visibility test, the delta
  (`node tools/corpus/bench.js`, `mismatches 0`).
- `GetMemberMapSlot`'s vector-storage overflow path (a key past
  `INLINE_FIELDS`).
- Gates: clippy `-D warnings`; `cargo test --workspace`; test262 `language`
  23726/0/0/0 and `built-ins` 23820/0/1/0.
