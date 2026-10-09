---
name: slag-dense-arrays
description: "Load when working on Slag's dense Array element storage, its compiled append, or its compiled element read — `ArraySlots` (crates/crux/src/object.rs), the `Value::hole()` sentinel, the `elem_ptr`/`elem_len`/`elem_cap` cursor and `elements()`/`elements_mut()` guards, the JIT's inline dense append (`emit_dense_array_append_inline`) and inline element read (`emit_element_read` / `emit_dense_element_read_into` / `emit_typed_array_read_into`), the array-`length` read gates, or the `properties[0]` length mirror. Documents the traps - authoritative-vs-mirror length, cursor-based GC tracing, the hole sentinel's reserved tag, why a hole declines a read to the chain, inline re-validation of extensible + the chain verdict, why the read lane is single-agent only, and why the gate must stay branchy."
---

# Slag dense Array stores and the compiled append

The dense Array representation and the JIT's inline element append and read
(the `array-store-plan.md` Phase C landing plus the later inline read). These
are the traps that cost real debugging time; the sweeps and the
jit/jitless/`--gc-stress` differential battery are the backstop for this area.

Locations: `crates/crux/src/object.rs` (`ArraySlots`, the cursor,
`ChainVerdict`, `array_element_write`/`array_element_write_dense`,
`spill_dense_array`, the length readers), `crates/crux/src/value.rs` (the
hole sentinel), `crates/jit/src/compiler.rs`
(`emit_dense_array_append_inline`, `emit_element_read`,
`emit_dense_element_read_into`, `emit_typed_array_read_into`,
`emit_computed_store`, `emit_member_cell_probe`'s length path),
`crates/runtime/src/ir.rs` (`member_cell_get`, `array_length`,
`array_element_get`).

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

## 4. The inlined append re-checks [[Extensible]] and the elements protector

`emit_dense_array_append_inline` skips the `DenseArrayAppend` helper on the
hot path, so it must itself verify the two things the helper checks:

- `[[Extensible]]` (a non-extensible array must fall back to `[[Set]]`).
- The cached clean-chain verdict, now an **elements-protector epoch compare**.
  `store_chain_clean` is a `Cell<u64>` on `JsObject` holding the
  `crux::PROTOTYPE_EPOCH` at which `store_chain_walk` found the two-link chain
  clean (0 = no verdict). The compiled code loads it and compares against the
  live `PROTOTYPE_EPOCH` — one load + compare replaces the old two-link
  `(id, generation)` revalidation. A mismatch falls to the helper, which
  re-walks and re-records. Without this an `Array.prototype[2] = setter`
  intercept is bypassed.

The compiled fast path also declines a receiver that is itself a registered
prototype (`is_prototype`), folded into the `[[Extensible]]` branch: a
compiled append does not run `bump_generation`, so such a store must fall to
the helper to advance the epoch for any child relying on a verdict.

`store_chain_clean_hit`/`store_chain_walk` remain the reference
implementation and the recorder.

## 5. Keep the gate branchy — do NOT merge or fold the checks

Every attempt to cut branches lost to the multi-block form (measured on
`buildString shape`):

- Collapsing the whole gate + chain into one branchless block
  (dummy-pointer `select` chain, one combined `and` tree, one branch):
  **15.4 → 18.7 ms**.
- Folding `[[Extensible]]` into the verdict branch: **~16.4 ms**.
- Folding the cursor capacity into the chain branch: **~16.4 ms**.

The serial `and` tree / longer branch predicate costs more than the removed
branches, because the CPU speculates through the independent early checks.
Keep each check its own short-predicate branch.

- Do not emit unreachable/dead blocks to "disable" a check for a probe — a
  dead-branch experiment made the compiled body bail entirely (ratio ~1.0,
  i.e. it ran interpreted).
- The `PostInc` key (`a[l++]`) is NOT the cost: stripping its `is_double`
  branch + `canon` moves the row less than the noise, so the
  register-resident-index idea does not pay.

## 5b. What the residual actually is (why parity needs more than a tweak)

The store is ~45 instructions and ~8 branches (tag, `array_dense`, key
round-trip, length, extensible + `is_prototype`, the epoch compare, capacity,
then the element/length/generation stores); V8's is ~5 instructions and one
map/bounds branch. Do not re-derive this with more micro-folding; it has
been measured.

Measured budget per `buildString shape` iteration (~5.2 ns; node ~2.5),
isolating each check by forcing its predicate true so Cranelift DCEs the
loads:

- two-link chain revalidation: **~0.87 ns** (the biggest removable piece)
- key round-trip + bound: ~0.25 ns
- tag + `array_dense` + `index == length` + [[Extensible]] + capacity: ~0.5 ns
- element/`elem_len`/`length`/`generation` stores: ~0.5 ns
- loop + `l === 10000` test + counter: ~1.0 ns (`l++` itself is free)

The **realm-level elements protector** (V8's `no_elements_protector`) landed
as Stage 1 of `.notes/dense-store-redesign.md`: a per-realm epoch
(`crux::PROTOTYPE_EPOCH` — process-global, so it over-invalidates across
agents, which is sound) plus a prototype *registry* (`is_prototype` on
`JsObject`). `mark_prototype` registers a link from `canonical_empty_map`
(every prototype-taking constructor) and `set_prototype_of`;
`bump_generation` advances the epoch only when `is_prototype`, so the target
loop's own appends (its array is not a prototype) do not self-invalidate the
verdict. `buildString shape` 15.5 → ~13.4 ms (~1.8x node). The old
unfunneled-bump design is why it looked unbuildable; the registry is what
makes the invariant cheap.

- Trap: `is_prototype` must be set in **every** `JsObject` constructor and
the in-place initializers (`init_ordinary`, `array_create`) — a missed one
silently bypasses a prototype setter. The two-engine generation split
applies: the interpreter's `bump_generation` advances the epoch; the JIT
must not append to a prototype receiver, hence the `is_prototype` gate.

Parity needs the full V8-shaped redesign (integer index path, f64-free
length field, elements kinds, no per-store generation bump) — a large
project, not justified for one row. The gate is near-optimal for this
representation.

Loop-guard hoisting via a `readonly` load was scoped and rejected:
Cranelift's egraph does hoist loop-invariant ops, but `readonly` asserts the
memory is not mutated anywhere in the function, and the compiled body's own
`DenseArrayAppend` fallback calls `store_chain_walk`, which writes
`store_chain_clean` — so the verdict loads can't carry it; and the link
loads could only if no user code runs in the body, which `buildString`'s
`a.length = 0` (a possible setter) defeats for a sound analysis.

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
- For the read gate: in-bounds, a stored `undefined`, a hole with an
  `Array.prototype` element behind it, an out-of-range index, a string key, a
  `Uint8Array` (its own representation), and a non-Array receiver.
- Rebuild `sweep.exe` and run all three areas (the append gate and the
  length probe are on shared read/store paths).

## 8. The inline computed element read (`a[i]` / `ta[k]`)

`emit_element_read` is the read mirror of the append, and both buffer-backed
representations go through it: `Step::GetMemberComputed` and the register
`GetMemberComputed`/`GetMemberComputedLocal` all lower through it, and it
dispatches on the receiver — a live `array_dense` cursor means a dense Array
(arm `emit_dense_element_read_into`), else a live `typed_array` cursor means an
Integer-Indexed TypedArray (arm `emit_typed_array_read_into`, the read mirror of
the Uint8-only inline store). Anything else falls to the `get_member_computed`
helper.

### The dense arm

- the receiver is a tagged Object (the packed prefix+Object compare);
- `array_dense` is non-null. It IS the dense switch — a spill clears BOTH
  `dense` and `array_dense`, so a null cursor means "not dense"; do not test
  `slots.dense` separately;
- the key is a canonical index Number: `idx = ToUint64Sat(num)` round-trips
  exactly (`fcvt_from_uint(idx) == num`) and `num < 2^32-1`. That rejects
  fractional, negative, huge, and non-double keys (a NaN-boxed heap value
  bitcasts to a NaN, whose compare fails), and a `-0.0` key — which
  `ToPropertyKey` normalizes to `"0"` before it ever reaches a computed read —
  round-trips to index 0, which is what the helper does too;
- `idx < elem_len` (the authoritative materialized length — `[0, elem_len)` is
  initialized);
- the loaded element is not the hole sentinel.

**A hole must decline to the helper, never be served.** A hole is
spec-absent, so `[[Get]]` consults the prototype chain (a prototype element or
accessor wins); returning the sentinel bits would be a wrong value, not a
wrong-looking one. A stored `undefined` is a real value and IS served. Also
declining: a spilled array, a string/out-of-range key, and a non-Array
receiver.

While dense every element is a writable data property (a non-w/e/c element
descriptor spills the array), so an own buffer element shadows the whole chain
— no chain check is needed on the served path. The read allocates nothing and
holds no guard, so it needs no write barrier and no epoch check.

### The TypedArray arm

- the receiver's `typed_array` slots are non-null;
- `detached == 0` and `resizable == 0`, and the `BlockState.data` pointer is
  non-null. A detached buffer covers no element (the helper returns
  `undefined`); a RESIZABLE buffer is declined because a fixed view's
  `array_length` is the effective length only while the buffer has not shrunk
  below its byte range, and a non-resizable buffer's view is never auto-length
  — together `idx < array_length` is then `IsValidIntegerIndex` exactly.
  `immutable` is deliberately NOT checked: reads of an immutable buffer are
  legal (only the store rejects them);
- the key gate is the dense arm's, plus `idx < array_length`;
- the element kind dispatches to one conversion block per supported kind: every
  integer width (sign- or zero-extended) plus Float32 (widened) and Float64
  (its stored bytes already are its Number bits). Float16 (a soft-float decode)
  and BigInt64/BigUint64 (a BigInt allocation) are absent and decline.

**The whole lane is single-agent only.** Under the `workers` feature the
shared-buffer block layout is cfg'd, so `emit_typed_array_read_into` jumps
straight to the helper — exactly as `emit_typed_array_store_inline` does. A
test that counts the typed helper must therefore be gated on
`!crux::typed_array::WORKERS`: `crates/test262` and `crates/v8` enable
`runtime/workers`, so a `--workspace` test build unifies the feature ON and the
count assertion is vacuous there (it is meaningful under `cargo test -p jit`
and the CLI/corpus builds, which do not enable it).

Measured (`arrays/index_loop`): 37.9 → 20.7 ms with `GetMemberComputed`
21,000,000 → 0; `objects/many_objects_read` 30.6 → 21.1 ms;
`arrays/typed_array` 50.1 → 35.6 ms (`GetMemberComputed` 14,000,000 → 0);
`--jit-bench`'s `typed-array read` row 0.15 (89.5 → 13.0 ms).

**The optimizing tier mirrors both arms** (`opt_lower::emit_element_read`,
`emit_dense_element_read`, `emit_typed_element_read`). A body the tier lifts
lowers `Op::ElementLoad` through the same gates as the per-step arms — the
dense parity port took the opt path's element-read loop from the
`get_member_computed` helper (~61ms) to an inline read (~31ms) on the `while`
shape the tier lifts; the typed port is performance-neutral on the micro (the
helper is already cheap for a monomorphic typed read) but keeps the opt path
inline-identical to the per-step path, so widening the lift cannot make a
lifted typed-read body take the helper and trip the call-count tests. The G8
computed-read cell is still not ported there (it declines to the helper). See
the optimizing-tier notes, §6, for the sequence and the fusion gate.

## 9. The fused array literal (`Step::ArrayFast`)

`[a, b, c]` with every element a plain expression (no hole, no spread, and
non-empty) compiles to ONE `Step::ArrayFast { count }` instead of `ArrayBegin` +
one `ArrayElement` per element + `ArrayEnd`. The array analogue of `ObjectFast`:
the N value expressions are evaluated first (they sit on the rooted work stack /
JIT buffer), then the step pops them, creates the array, and fills it. The
shared core is `array_fast_create` (`crates/runtime/src/ir.rs`), called by the
interpreter handler and by the JIT `array_fast(count, sp)` helper.

- **The fill is `JsObject::array_extend_dense(values)`**, not a
  `create_data_property_index` per element: one `elements_mut` borrow, one
  buffer grow, one `length`/mirror/generation update. The per-element cost
  collapses to the buffer memcpy, so 6- and 10-element literals cost the same.
- **No write barrier, and that is correct here.** The target is a fresh YOUNG
  array, so it can hold no old->young edge; a young box is traced with the full
  `trace` (never `trace_dirty`), so its `dirty_from = (0,0)` from
  `ArraySlots::new` is never consulted. Do not add a per-element barrier to this
  path "for safety" — it is pure waste, and the `--gc-stress` sweep is the net
  that proves it.
- **The EMPTY literal is deliberately NOT fused** (`!elements.is_empty()`): the
  fused create measures no faster for `[]`, and leaving it unfused keeps
  `array_create` untouched.
- **Trap: do NOT refactor `array_create` to share code with the fused path.**
  A first cut added `array_create_from_values` and factored `array_create`
  through a shared `array_create_with_slots`; that added a call layer that cost
  **~4% on every `array_create` caller** (`new Array(3)` micro 0.962, `[]`
  0.931 — real, not layout: the untouched control rows measured 1.00 and
  `#[inline]` did not recover it). `array_create` is the surface for `new
  Array`, spread, `Array.from`, and every array-returning builtin, so the
  post-create extend (which leaves `array_create` byte-identical) is the shape
  to keep.
- Measured (isolated 1M-iteration micros): `[i]` 1.056x, `[i x6]` 1.593x,
  `[i x10]` 2.028x; `[]` and `new Array(3)` neutral. The residual is the
  `array_create` allocation plus one buffer malloc.

## Relationship to the other skills

- `slag-property-writes` — the named-member store machinery and the
  interpreter-bumps / JIT-does-not generation split (this skill's array
  analogue).
- `slag-jit` — the helper-table mirror, the pending-error ABI, and the
  certified-loop lowering rules.
- `slag-conformance` — the sweep/triage workflow and the release-binary
  freshness trap.
