# Performance

Current performance state of the slag runtime and the PLAN Phase 18
performance milestones, each behind a benchmark gate rather than a
correctness gate.

## Current architecture

- **Value representation**: a NaN-boxed `u64` (PLAN Phase 18): a quiet-NaN
  tag region (top 16 bits `0x7FF8`) holds a 4-bit tag plus a 44-bit `Rc`
  payload for the heap variants; every other bit pattern is a double stored
  exactly. Heap values own one strong `Rc` ref; `Clone`/`Drop` reconstruct
  the `Rc` from the payload. `match value.kind()` keeps the old enum arm
  shapes via a `ValueKind` mirror.
- **Interpreter**: a tree-walker over the parser's AST (the resumable
  function IR from Phase 7); there is no bytecode.
- **Objects**: ordinary Rust structs with a property vector and a prototype
  `RefCell`; per-object state (promises, generators, buffers, ...) lives in
  agent-side tables keyed by object id. There is no shape/IC machinery; a
  lazy key→slot hash index accelerates lookups on objects with many
  properties (the global object), invalidated by structural changes only.
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
up (interning, hook installation), then timed. The sources use `var` (not
`let`) declarations: a second evaluation in the same realm re-declaring
`let` bindings is a SyntaxError, so the original `let`-based snippets made
the timed run measure that error path (the pre-migration "57µs arithmetic"
snapshot below was such a measurement, not the real loop). Numbers are only
comparable within a build profile; record an early snapshot (debug and
release) and compare against it after each milestone below.

### Baseline snapshot (2026-08-18)

The original snapshot was recorded with `let`-based snippets whose second
evaluation errored; it is kept here for the record but is not a valid loop
time. The corrected `var`-based methodology measured the real pre-migration
loops in release on the same machine:

| Benchmark | release (original snapshot, error path) | release (corrected, real loop) |
|---|---|---|
| arithmetic | 57µs | 2.52s |
| property access | 20µs | 3.22s |
| string concat | 51µs | 0.88s |
| array iteration | 28µs | 15.42s |
| function calls | 18µs | 5.73s |

(The arithmetic benchmark is a 1M-iteration `n += i * 2` loop; the real
numbers are dominated by the tree-walker's identifier resolution and
environment machinery, not by value representation.)

## Deferred milestones

Each milestone is deferred with its gate from PLAN Phase 18. A milestone is
"done" only when it passes its gate; none are correctness gates.

| Milestone | Gate | Status |
|---|---|---|
| NaN-boxed `Value` (u64 with tag fast paths) | arithmetic micro-benchmark ≥ 2x vs snapshot | Correctness landed (migration below); the 2x gate is not yet met — see the benchmark analysis at the end of the section. |
| Bytecode VM replacing the tree-walker | `--print-bytecode` dumps real bytecode; hot-path bench ≥ 5x | Deferred: the Phase 7 IR would need to grow a full instruction set (property load/store, call/construct with ICs, closures, control flow). |
| Object shapes / hidden classes + inline caches | property-access micro-benchmark ≥ 2x | Deferred. |
| String rope representation | string-concat micro-benchmark ≥ 2x | Deferred. |
| `--gc-stress` + leak-detection harness | stress runs clean, no leaks | Deferred: requires the arena heap + mark-sweep GC milestone (below). |

## NaN-boxed `Value` milestone (landed — gate not yet met)

The enum representation (`enum Value { Undefined, Null, Boolean(bool),
Number(f64), BigInt, String, Symbol, Object, Function }`, ~16 bytes with
the `Rc` handle variants) becomes a single `u64`, so every value is one
machine word: tag dispatch is one compare, and values in arrays/arguments
vectors/VM stacks halve in size. `Handle<T> = Rc<T>`, so the box cannot be
`Copy` (dropping a copy would release the ref); it is `Clone` with manual
refcount reconstruction. The concrete layout:

- **Tagged region** — quiet NaNs whose top 16 bits are `0x7FF8` (exponent
  `0x7FF`, quiet bit 51 set, bits 50-48 zero): `tag` in bits 47-44,
  `payload` in bits 43-0 (the `Rc` pointer).
- **Doubles** — every other bit pattern, preserved exactly: signaling NaNs
  and quiet NaNs with bits 50-48 ≠ 0 survive as-is. A quiet NaN with bits
  50-48 = 0 collides with the tag region and is canonicalized on box to
  `0x7FF9_0000_0000_0000`; this is unobservable from JS (no NaN-payload
  introspection), and the `DataView`/`Float64Array` fixtures stay green
  (verified by the full sweep).
- **Tags** — `0x0` undefined, `0x1` null, `0x2` false, `0x3` true, `0x4`
  BigInt, `0x5` String, `0x6` Symbol, `0x7` Object, `0x8` Function;
  `0x9`-`0xF` reserved. Payload capacity is 44 bits (17.6 TB), far above
  any real `Rc` allocation.
- **Refcounts** — `Clone` reconstructs the `Rc` via `Rc::from_raw(ptr)`, clones
  it, and forgets the reconstruction (`Rc::into_raw` on the clone); `Drop`
  reconstructs and drops. Refcounts stay exact, and heap values move by
  plain `u64` copies until a clone/drop actually touches the refcount.
- **`PartialEq`** preserves the current derived-enum semantics: `Number`
  compares via `f64::eq` (`NaN != NaN`), heap values via their `Handle<T>`
  `PartialEq` (the `Rc` deref structural comparison the derive produces).
- **Source compatibility** — constructors keep the current variant spellings
  as associated functions/consts (`Value::Number(x)`, `Value::Undefined`, …)
  so construction sites compile unchanged; only `match`/`if let`/`matches!`
  sites move to `kind()` + `as_*` accessors.

**Migration order** (the type change is atomic across the workspace, so it
lands as one change: crux → runtime → test262, then gates):

1. Rewrite `crates/crux/src/value.rs` (layout, tags, constructors, accessors,
   `type_of`/`is_callable`/`is_constructor`, `Display`, `PartialEq`, `Clone`,
   `Drop`) with unit tests for the bit patterns, the NaN canonicalization,
   and the refcount round-trip.
2. Migrate the `match`/`if let`/`matches!` sites: crux (7 files), then
   runtime (53 files, ~4,200 sites), then test262 (1 file).
3. Gates: `cargo clippy --workspace --all-targets -- -D warnings`, `cargo
   test --workspace`, and the full release sweep (48,622 fixtures) — the
   NaN canonicalization and `PartialEq` semantics are the regression risk.
   **All three are green** (sweep: 0 fail, 229 skip — the standard
   taxonomy, 0 crash).
4. Re-run `--bench` and compare against the snapshot; arithmetic ≥ 2x
   marks the milestone done. (The `Copy` win and the refcount-free moves
   arrive with the GC milestone, which replaces `Rc` with an arena heap.)

**One correctness trap surfaced during the migration**: the `is_*`/`as_*`
accessors must reject doubles before reading the tag — a double's bits
47-44 can collide with a heap tag (e.g. `65.0` is `0x4050_4000_0000_0000`,
whose bits 47-44 read as the BigInt tag), and an unguarded `as_bigint()`
would reconstruct an `Rc` from the double's low bits and crash. Every tag
accessor now checks `!is_double()` first.

### Benchmark analysis (why the 2x gate is not yet met)

The tag fast paths from the design landed (direct-double arithmetic in
`apply_binary`/`numeric_binary`/`abstract_relational`, plus a lazy
key→slot property index on objects with many properties), and the real
release loop times moved as follows:

| Benchmark | pre-migration (real) | post-migration (real) |
|---|---|---|
| arithmetic | 2.52s | 2.18s |
| property access | 3.22s | 2.86s |
| string concat | 0.88s | 0.55s |
| function calls | 5.73s | 5.51s |

(The pre-migration column is the `var`-methodology measurement of the
last green mainline build; the post-migration column is the current tree,
same machine, median of runs.)

The arithmetic loop is dominated by the tree-walker's per-iteration work
— identifier resolution (an env-chain walk with linear binding scans),
statement/expression dispatch, and per-iteration loop environments — not
by value representation. An empty 1M-iteration `var` loop measures ~1.1s
(≈ 1.1µs/iteration) before any arithmetic, and the direct-double fast
paths contribute roughly a 1.1x total speedup. Reaching the 2x gate needs
the interpreter work deferred to the shapes/inline-cache and bytecode
milestones (which carry their own gates); the NaN-boxed representation is
the prerequisite they build on. The gate stays open until then.

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
