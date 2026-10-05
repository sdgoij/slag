---
name: slag-gc-rooting
description: Load when runtime/builtin code holds Slag GC values (`Value`, `Handle<T>`, `Gc<T>`) in Rust locals across an allocation, when a `--gc-stress` or nursery collection sweeps a value that was still live, or when the A2 write-barrier verifier reports an "old box holds a young reference the barrier never recorded". Covers the local-buffer rooting trap, `StressSuppress` (which must suppress the per-allocation *and* safe-point collections), why a `Vec<Value>` on the Rust heap is invisible to the conservative stack scan, and how such a barrier "miss" is often a freed-and-recycled slot (a dangling `Value`), not a missing barrier.
---

# Slag GC rooting: the local-buffer trap

The GC is a mark-sweep heap reached from precise roots (every live `Agent`)
plus a conservative native-stack scan. Neither sees a `Vec<Value>` /
`Vec<Handle<T>>` / `Vec<Gc<T>>` on the Rust heap: the scan only sees the
`Vec`'s *pointer* on the stack, and a heap-buffer address is not a box
address, so the elements are not retained. Any collection that runs between
"the buffer is filled" and "the values are rooted by a traced structure"
will **sweep them**, and the stale `Value` bits are then a use-after-free
when read.

## The guard: `StressSuppress`

Open a window for the vulnerable region:

```rust
let _stress = crate::ir::StressSuppress::new();
```

- It bumps `ir::SUPPRESS_STRESS`; `ir::is_compiling()` is then `true`
  (`is_compiling() = COMPILING || SUPPRESS_STRESS > 0`).
- `crux::heap::maybe_stress_collect` (the per-allocation `--gc-stress`
  collector) skips while `is_compiling()`.
- `Agent::maybe_collect` (the safe-point trigger) **also** honours
  `is_compiling()` now, so the window suppresses the safe-point minor *and*
  major too. That gate is load-bearing: before it, a `StressSuppress` window
  only suppressed the per-allocation collector, so a loop reached through a
  `then`/iterator/comparator call inside the window could still trigger
  `maybe_collect` and sweep the window's buffers. **If you add a new
  collection trigger, it must check `is_compiling()`.**

The idiom is used by ~20 builtins that build a local buffer and then hand it
to a constructor (`array_from_values`, `json_parse`, `sort_indexed_properties`,
`group_by`, `promise_all_settled`, the disposable/atomics drains, …). Wrap the
*build and any user-code calls in between*, not just the final store.

An alternative to `StressSuppress` is to keep the value in a **stack local**
(a plain `let x: Value` / `let h: Handle<T>`), which the conservative scan
*does* see. `promise_all`/`promise_any`/`promise_race` are safe this way (one
closure in a local); `promise_all_settled` was not, because it pushed two
closures into a `Vec` and the second allocation swept the first. A `Vec` is
the trap; a stack local is not.

## The A2 verifier is a symptom, not always the diagnosis

`write-barrier miss: N old box(es) hold a young reference the barrier never
recorded` (`crates/crux/src/heap.rs::verify_barrier`) means an **old** box's
`trace` yields a **young** child and the box is not in the remembered set. The
usual cause is a genuinely missing `write_barrier`/`barrier_property` call.
But it can also be a **dangling `Value`**: the container legitimately held an
old value, that value's box was swept while still referenced (a local-buffer
rooting miss), and the slot was **recycled** by a young allocation — so the
container's stale bits now read young at the same address.

Distinguish them: if the child address was freed earlier this run, it is the
dangling/reuse case, not a missing barrier. A quick probe is a TLS
`HashSet<usize>` of swept addresses (filled in both sweeps) checked against
the offending child at the miss. The container being *unreachable* (dead
garbage the verifier still walks) is a separate, benign false positive.

## Diagnosing

- Reproduce a single fixture: `sweep.exe built-ins --filter '<area>/<fixture>.js'
  --jobs 1 --batch 1 --timeout 15 --recheck-timeout 15 --gc-stress` (add
  `--jitless` to rule the JIT out).
- `--gc-stress` runs a major on every allocation *and* a minor at every safe
  point; both can sweep an unrooted local buffer under stress.
- The four sweep entry points are `Heap::collect*`/`collect_minor*`; the
  conservative scan is `stack_bounds`/`scan_stack`, and the write barrier is
  `crux::heap::write_barrier` → `remember_if_young`.
- `crux::heap::value_box_is_young`, `was_freed`-style registries, and
  `for_each_live_box` (public, yields `(GcAny, name, size, edges)` with
  `GcAny::cast::<T>()`) are the tools for a field-level probe without adding
  permanent code.

## Validating a change here

Run the differential battery: `sweep.exe all --gc-stress` (0 fail / 0 crash,
`Promise/allSettled` clean), normal sweep at baseline, `--gc-verify` at
baseline; the CLI `--gc-stress` and `--nursery-stress` benches 12/12
`ok=true`; `cargo test --workspace`; clippy `-D warnings`. A `runtime`/`crux`
change also owes the wasm suites (`wasmtest run --strict <dir>` per proposal
dir — the core root and each proposal dir separately, they clash on cache
names — plus `wasmtest jsapi waspec/test/js-api`).

## Relationship to the other skills

- `slag-property-writes` — the store-side barrier and the interpreter-bumps /
  JIT-does-not generation split.
- `slag-dense-arrays` — the cursor-based `ArraySlots` trace and its dirty-slot
  bounding.
- `slag-conformance` — the sweep/triage workflow and binary freshness.
