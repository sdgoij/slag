---
name: slag-strings
description: "Load when working on Slag's string and rope machinery — crates/crux/src/string.rs (the JsString enum, concat/owned_of/as_slice, the flatten cache, Small/Flat/ConsString/Rope forms, thresholds) or the string builtins that convert `this` (crates/runtime/src/builtins/string.rs, crux convert::to_string). Documents the flatten-once cache trap — an owned JsString::clone of a rope gets a FRESH cache and re-flattens on every content access, so handle-to-owned conversions must go through JsString::owned_of — plus the leaf/rope thresholds that decide what concat produces and the arena-pointer lifetime rules."
---

# Slag string/rope machinery traps

The string representation lives in `crates/crux/src/string.rs`; the
builtins that consume it are in `crates/runtime/src/builtins/string.rs`.
Most engine code reads strings through `Value::String(Handle)` (a GC box)
and never sees an owned `JsString`; the owned copies that DO flow are
created by the primitive-to-string conversions (`to_string`,
`this_string_value`) that every string builtin calls on `this`. Those
conversions are where the flatten-cache trap lives.

## 1. What `concat` produces (don't mispredict the shape)

- `Small` holds ≤ `CONCAT_FLAT_THRESHOLD` (16) units inline in the box;
  `Flat(Arc<[u16]>)` is a larger contiguous buffer. Both are leaves:
  `as_slice` is direct, `len` O(1).
- Results ≤ 16 units are `Small`. Two leaves merging to ≤ 128 units make a
  `Flat`. Past 128: single-unit/small right operands build a lean
  `ConsString` append node (`s += 'x'` loops); two large leaves build a
  balanced `Rope`. The first ~128 units of a `+=` loop stay leaf, so short
  build loops never rope — a probe/test that needs a real `ConsString`
  must accumulate well past 128 units.
- `depth` is folded at the cap: an over-deep left side is collapsed into a
  single shared flat child, so arbitrarily long append chains stay linear.

## 2. The flatten-cache trap: owned clones re-flatten per access

- The materialized buffer cache (`OnceLock<Arc<[u16]>>`) is INLINE in the
  `ConsString`/`Rope` data. `as_slice` on a boxed rope (deref'd `Handle`)
  flattens once and caches for that box's lifetime.
- `JsString::clone` of a rope returns an owned STRUCT COPY with a FRESH
  empty cache (children shared, content identical). Any content read on
  that copy — `as_slice`, `code_unit`, `PartialEq`, `Hash` — re-flattens
  the whole rope: an O(n) Vec + Arc + node walk per read.
- This bites exactly at the builtin `this` conversion: `to_string` handed
  string builtins an owned clone, so `charCodeAt`/`indexOf`/`slice` on a
  rope re-flattened per call (measured ~4x on the per-unit-read corpus
  rows, 2026-09-06; a 256-unit `charCodeAt` loop ~416ms -> ~105ms once
  fixed).
- **Rule: never produce an owned copy of a rope by
  `s.as_ref().clone()` (or `(*handle).clone()`) when the copy's content
  will be read — use `JsString::owned_of(&handle)`.** `owned_of` leaves
  clone O(1), but for a rope it materializes the box once and seeds the
  copy's fresh cache with the same `Arc` (strings are immutable, so the
  seed is exact; per-call conversions read at leaf cost from the second
  call on). The conversion choke points that MUST use `owned_of`: crux
  `convert::to_string`'s String arm and runtime `this_string_value`
  (String primitive + String-wrapper object). A plain `JsString::clone`
  of box data still gets a fresh cache and is fine only off the per-call
  paths.
- A box flattened through `owned_of` keeps its buffer for life (flatten on
  first use, retain) — the intended V8-style behavior, not a leak.
- Do NOT re-propose sharing the cache across clones
  (`Rc<OnceLock<...>>` per node): it was tried and reverted — the extra
  allocation per `ConsString` node cost ~2x on append loops
  (`.notes/perf.md`, Failed experiments).

## 3. Handles are arena pointers — lifetime rules for owned values

- `Handle<JsString>` is `Gc<T>`, a COPY mark/sweep arena pointer (not a
  refcounted `Rc`). Cloning a handle/box data shares children at zero
  cost (pointer copies); boxes never move; the arena scan roots handles
  and conservatively scans the stack.
- An OWNED `ConsString`/`Rope` value holds child arena pointers. Only
  leaves (`Flat`/`Small`) are self-contained. Keep owned rope values
  inside `Handle` boxes or transient on the stack (conservatively
  scanned); do not store an owned rope in a non-GC Rust container where
  its children could be collected from under it. Leaf copies are safe to
  store anywhere.
