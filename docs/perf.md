# Performance

Current performance state of the slag runtime and the PLAN Phase 18
performance milestones, each behind a benchmark gate rather than a
correctness gate.

## Current architecture

- **Value representation**: an `enum Value` with nine variants (`Undefined`,
  `Null`, `Boolean`, `Number`, `String`, `Symbol`, `BigInt`, `Object`,
  `Function`). Handles are `Rc`-based; values are cheap to clone.
- **Interpreter**: a tree-walker over the parser's AST (the resumable
  function IR from Phase 7); there is no bytecode.
- **Objects**: ordinary Rust structs with a property vector and a prototype
  `RefCell`; per-object state (promises, generators, buffers, ...) lives in
  agent-side tables keyed by object id. There is no shape/IC machinery.
- **Strings**: UTF-16 code-unit arrays; no ropes.
- **Memory**: `Rc`/`RefCell`-based ownership; there is no tracing GC.
  `WeakRef`/`FinalizationRegistry` are implemented with kept-alive
  semantics, but collection is not driven by a heap.

## Benchmark gate

The CLI's `--bench` mode runs a fixed micro-benchmark suite and reports
wall time per benchmark:

```
cargo run -p cli --release -- --bench
```

Current benchmarks: arithmetic loops, property access, string concatenation,
array iteration, and function calls. Each snippet is evaluated once to warm
up (interning, hook installation), then timed. Numbers are only comparable
within a build profile; record an early snapshot (debug and release) and
compare against it after each milestone below.

## Deferred milestones

Each milestone is deferred with its gate from PLAN Phase 18. A milestone is
"done" only when it passes its gate; none are correctness gates.

| Milestone | Gate | Status |
|---|---|---|
| NaN-boxed `Value` (u64 with tag fast paths) | arithmetic micro-benchmark ≥ 2x vs snapshot | Deferred: correctness first; the enum representation is the reference. |
| Bytecode VM replacing the tree-walker | `--print-bytecode` dumps real bytecode; hot-path bench ≥ 5x | Deferred: the Phase 7 IR would need to grow a full instruction set (property load/store, call/construct with ICs, closures, control flow). |
| Object shapes / hidden classes + inline caches | property-access micro-benchmark ≥ 2x | Deferred. |
| String rope representation | string-concat micro-benchmark ≥ 2x | Deferred. |
| `--gc-stress` + leak-detection harness | stress runs clean, no leaks | Deferred: requires the arena heap + mark-sweep GC milestone (below). |

## GC milestone (PLAN Phase 18 item 2)

The plan's GC milestone (arena heap + mark-sweep; root tracing;
ephemeron-aware WeakMap/WeakSet; `WeakRef`/`FinalizationRegistry`
semantics activated; `--gc-stress` mode) is a rewrite of the value/object
model from `Rc`-based ownership to GC-managed handles, and is deferred as a
unit. The observable spec surface (`WeakRef`, `FinalizationRegistry`,
`keptAlive`, `WeakMap`/`WeakSet` keying) is implemented and tested; only the
collector itself is missing.

## Accepted no-op CLI flags

`--stack-size`, `--max-old-space`, and `--print-bytecode` are accepted by
the CLI for compatibility. They are no-ops because the corresponding
machinery (call-stack depth control, a heap to cap, a bytecode printer)
does not exist yet; they will become live as the milestones above land.
