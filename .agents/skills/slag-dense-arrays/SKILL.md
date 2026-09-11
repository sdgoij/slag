---
name: slag-dense-arrays
description: "Load when working on Slag's dense Array element storage or its compiled append — `ArraySlots` (crates/crux/src/object.rs), the `Value::hole()` sentinel, the `elem_ptr`/`elem_len`/`elem_cap` cursor and `elements()`/`elements_mut()` guards, the JIT's inline dense append (`emit_dense_array_append_inline`), the array-`length` read gates, or the `properties[0]` length mirror. Documents the traps - authoritative-vs-mirror length, cursor-based GC tracing, the hole sentinel's reserved tag, inline re-validation of extensible + the chain verdict, and why the gate must stay branchy."
---

# Slag dense Array stores and the compiled append

The dense Array representation and the JIT's inline element append (the
`array-store-plan.md` Phase C landing). These are the traps that cost real
debugging time; the sweeps and the jit/jitless/`--gc-stress` differential
battery are the backstop for this area.

Locations: `crates/crux/src/object.rs` (`ArraySlots`, the cursor,
`ChainVerdict`, `array_element_write`/`array_element_write_dense`,
`spill_dense_array`, the length readers), `crates/crux/src/value.rs` (the
hole sentinel), `crates/jit/src/compiler.rs`
(`emit_dense_array_append_inline`, `emit_computed_store`,
`emit_member_cell_probe`'s length path), `crates/runtime/src/ir.rs`
(`member_cell_get`, `array_length`, `array_element_get`).

## 1. The element buffer is `Vec<Value>` with an in-band hole sentinel

`ArraySlots.elements` holds plain `Value`s; a hole is `Value::hole()`
(`HOLE_BITS`, reserved NaN-boxing tag 10). `Value` is `#[repr(transparent)]`
over its `u64`, so one element is one machine word and the compiled append
writes it with a single store.

- **Filter holes before `kind()`.** `kind()`'s reserved-tag arm is
  `unreachable!`; a hole reaching it panics. Every reader filters
  (`!value.is_hole()`) first — `dense_element`, `member_cell_get`'s callers,
  `array_element_get`, etc.
- `Trace for Value` already ignores reserved tags (its `_ => {}` arm), so
  holes trace as no-ops.

## 2. `elements` is private — go through the cursor guards

`elements` is accessed only via `ArraySlots::elements()` / `elements_mut()`.
`elem_len` is authoritative for the materialized length; the read guard's
`sync` brings the backing `Vec`'s length up, and `DenseElementsMut::drop`
refreshes the `elem_ptr`/`elem_len`/`elem_cap` cursor after a safe mutation.

- **Never touch `elements` directly**, and never hold a guard across a
  compiled call or a GC safepoint.
- **`Trace for ArraySlots` walks the cursor, not the `Vec`.** A machine-code
  append advances `elem_len` without touching the `Vec` header, so a
  length-from-`Vec` trace would miss the freshly appended elements and
  collect live values. The cursor's `elem_ptr` is stable between reallocs
  because the compiled append's capacity gate never grows the buffer.
- **The cursor is written by the JIT, not mirrored lazily.** Any new path
  that mutates `elements` must go through `elements_mut()` (which refreshes
  on drop) or the JIT read the stale geometry.

## 3. The compiled append leaves the `properties[0]` length mirror STALE

While `dense`, `slots.length` (the `Cell<f64>`) is authoritative. The
compiled append bumps the cell but **not** the `properties[0]` mirror (the
mirror's address is not offset-addressable — `PropertyKind::Data { value }`
is an enum-variant field). So any reader that could observe the mirror must
either consult the cell when `slots.dense` or re-sync from it:

- Templates that already do: `ordinary_get_own_property` (its `length`
  branch), `has_own_index_property`, `member_cell_get` (the
  interpreter/JIT value-cell read path), `Vm::array_length`.
- Re-syncer: `spill_dense_array` materializes the cell's true length instead
  of copying the mirror.
- **The bug this prevents:** `a[a.length] = i` in a compiled loop. The
  compiled append bumps the generation, so the member-value cell for
  `length` misses every read and falls to `get_member_name` →
  `member_cell_get` → a stale `properties[0]`. Adding the cell branch in
  `member_cell_get` is what fixed it.

## 4. The inlined append must re-check [[Extensible]] and the chain verdict

`emit_dense_array_append_inline` skips the `DenseArrayAppend` helper on the
hot path, so it must itself verify the two things the helper checks:

- `[[Extensible]]` (a non-extensible array must fall back to `[[Set]]`).
- The cached clean-chain verdict. `ChainVerdict` is a `pub #[repr(C)]`
  struct on `JsObject` (`first_id == 0` = "no verdict"); re-validate it by
  comparing the live two-link prototype chain's `(id, generation)` to the
  cached tuple. A chain mutation (an own index prop / accessor on a
  prototype) bumps that link's generation → mismatch → fall back to the
  helper, which re-walks and re-records. Without this an
  `Array.prototype[2] = setter` intercept is bypassed.

Null link handles are handled branchlessly by substituting the receiver's
own box as the load base (`select`), so the field loads never fault and the
unique-id compare fails. `store_chain_clean_hit`/`store_chain_walk` remain
the reference implementation.

## 5. Keep the gate branchy — do NOT merge it into one predicate

Measured on `buildString shape`: collapsing the whole gate + chain into one
branchless block (dummy-pointer `select` chain, one combined `and` tree, one
branch) **regressed 15.4 → 18.7 ms**. The serial `and` dependency costs more
than the branches, because the CPU speculates through the independent early
checks. Keep the multi-block form.

- Do not emit unreachable/dead blocks to "disable" a check for a probe — a
  dead-branch experiment made the compiled body bail entirely (ratio ~1.0,
  i.e. it ran interpreted).

## 6. The per-store generation bump is deliberate and cheap

The compiled append writes `generation += 1` (the interpreter discipline:
invalidate the generation-keyed element/length read cells). It measures
~0.06 ns/iter — **do not** try to batch it away. If you ever remove it you
must instead refresh every generation-keyed cache that can observe an array
(`array_element_value_cells`, `array_length_cells`, the `member_value_cells`
entry for `length`, the for-of verdicts).

## 7. Validating a change here

This area is GC- and semantics-sensitive; a gate change can silently bypass
an exotic intercept.

- Run the differential battery under jit, `--jitless`, and `--gc-stress`,
  and against node: `a[l++]` and `a[a.length]` fills, hole fill, `delete`,
  `Object.preventExtensions`, an `Array.prototype` index **setter**
  intercepting the append, mid-loop `length = 0`, the mirror readers
  (`getOwnPropertyDescriptor`, `Object.keys`/`entries`, `JSON.stringify`),
  `length` grow, and heap values read back.
- Rebuild `sweep.exe` and run all three areas (the append gate and the
  length probe are on shared read/store paths).

## Relationship to the other skills

- `slag-property-writes` — the named-member store machinery and the
  interpreter-bumps / JIT-does-not generation split (this skill's array
  analogue).
- `slag-jit` — the helper-table mirror, the pending-error ABI, and the
  certified-loop lowering rules.
- `slag-conformance` — the sweep/triage workflow and the release-binary
  freshness trap.
