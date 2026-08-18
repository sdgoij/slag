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
- **Interpreter**: a `Step` bytecode VM over the compiled function IR
  (`crates/runtime/src/ir.rs`): every expression and statement compiles to
  a `Step` at creation, and a `Vm` dispatch loop executes the compiled
  body for ordinary calls/constructs, generators, async functions, and
  top-level scripts. The old tree-walker survives only as isolated
  single-expression helpers (computed keys, destructuring defaults, class
  heritage); no statement or control-flow structure is walked anymore.
- **Objects**: ordinary Rust structs with a property vector and a prototype
  `RefCell`; per-object state (promises, generators, buffers, ...) lives in
  agent-side tables keyed by object id. There is no shape/IC machinery; a
  lazy key→slot hash index accelerates lookups on objects with many
  properties (the global object), invalidated by structural changes only.
- **Strings**: UTF-16 code units in a flat buffer or a depth-capped rope of
  concatenation nodes (see the string-rope milestone below); `concat` appends
  in O(1) once a string is large, and the flat form is materialized lazily
  and cached.
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
| NaN-boxed `Value` (u64 with tag fast paths) | arithmetic micro-benchmark ≥ 2x vs snapshot | **Done** — correctness landed (migration below) and the shapes work closed the gate: real-loop arithmetic is ~2.2x the corrected baseline. |
| Bytecode VM replacing the tree-walker | `--print-bytecode` dumps real bytecode; hot-path bench ≥ 5x | **Cut 1 + first half of Cut 2 delivered** — everything compiles and runs on the `Vm`; literal immediates (`BinaryImm`) and 0-2-arity fast calls (`CallFast`) landed with zero conformance regressions, but at walker parity the ≥5x gate is still open. |
| Object shapes / hidden classes + inline caches | property-access micro-benchmark ≥ 2x | **Done** — the cache layer below (interner memo, own-data fast paths, lazy property index) measured 2.1x on the corrected property-access baseline. |
| String rope representation | string-concat micro-benchmark ≥ 2x | **Done** — the rope below measured ~5x on the corrected concat baseline (0.88s → ~0.15s). |
| `--gc-stress` + leak-detection harness | stress runs clean, no leaks | Deferred: requires the arena heap + mark-sweep GC milestone (below). |

## NaN-boxed `Value` milestone (done)

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

### Benchmark analysis (the gate, closed by the shapes work)

The tag fast paths from the design landed first (direct-double arithmetic in
`apply_binary`/`numeric_binary`/`abstract_relational`), moving the real
release loop times as follows:

| Benchmark | pre-migration (real) | post-migration (real) | final |
|---|---|---|---|
| arithmetic | 2.52s | 2.18s | **1.14s** (2.2x) |
| property access | 3.22s | 2.86s | **1.50s** (2.1x) |
| string concat | 0.88s | 0.55s | 0.72s |
| function calls | 5.73s | 5.51s | 4.36s |

(The pre-migration column is the `var`-methodology measurement of the last
green mainline build; the post-migration column is after the NaN-boxing
fast paths; the final column is after the cache layer below. Same machine,
median of runs.)

The arithmetic loop is dominated by the tree-walker's per-iteration work —
identifier resolution (an env-chain walk with linear binding scans),
statement/expression dispatch, and per-iteration loop environments — and
an empty 1M-iteration `var` loop alone measured ~1.1s. Profiling the
identifier path showed the real culprit: every identifier read converts
`AtomId`→`JsString` and back through the global `Mutex`-guarded interner
four to five times (~50ns each). The cache layer below removed those
round-trips and the redundant property lookups, which is what actually
closed both the arithmetic and property-access gates.

## Shapes / inline-cache milestone (done)

A cache layer over the property and environment machinery — the shapes/IC
work deferred from the NaN-boxing milestone, done in four parts:

- **Thread-local interner memo** (`crates/crux/src/string.rs`): `intern` and
  `lookup` keep a 64-entry per-thread cache, so the identifier hot path
  (which converts the same handful of names several times per read) scans a
  few cached entries instead of taking the global interner lock and
  re-hashing/copying. The memo is a pure cache of the append-only interner
  and can never go stale.
- **Single-lookup global get** (`runtime/src/env.rs`): `GetBindingValue` on
  the global environment fetched with `has_property` + `get` (two interns,
  two hash lookups); sloppy mode now issues one `[[Get]]` and re-checks
  only when strict mode needs to distinguish a real `undefined` from an
  absent binding.
- **Own-data fast paths** (`crates/crux/src/object.rs` +
  `runtime/src/context.rs`): `get_key`/`set_key` and the runtime's
  `get_property_key` return/update an own data property on a plain
  object (Ordinary/Array) directly — no receiver construction, no
  prototype-chain accessor scan, no descriptor machinery. Array `length`
  is excluded from the write path (its define intercept validates
  non-uint32 lengths).
- **Lazy property index** (from the NaN-boxing milestone): objects with
  ≥16 properties keep a key→slot hash index, invalidated only by
  structural changes (insert/delete); in-place updates keep it valid.

Validation: `cargo clippy --workspace --all-targets --all-features -- -D
warnings` clean; `cargo test --workspace` 4,194 pass, 0 fail; full release
sweep over 48,622 fixtures: 0 fail, 229 skip (unchanged taxonomy), 0 crash.

## String rope milestone (done)

`JsString` is now either a contiguous buffer or a rope — a binary tree of
concatenation nodes (`crates/crux/src/string.rs`). `concat` appends in O(1)
once the accumulated string is large enough, and the contiguous form is
materialized lazily on first access and cached (strings are immutable):

- **Flat threshold** — concatenations of ≤16 units stay flat (a Vec copy),
  so ordinary small `+` operations never see the rope.
- **Empty operands** — `s + ""` and `"" + s` return the other side directly.
- **Depth cap** — a rope whose left side would exceed depth 64 is flattened
  first, keeping the tree shallow: a chain of small appends cannot overflow
  the stack on drop, and the amortized copy cost is one re-flatten of the
  accumulated string per 64 appends (quadratic with a tiny constant).
- **Lazy flatten** — `as_slice` materializes the concatenation once into a
  `OnceLock<Box<[u16]>>` inside the shared node (thread-safe, so `JsString`
  stays `Send` for the well-known-symbol table) and returns a stable
  reference; `len` is O(1) cached. The flatten walk is iterative (an
  explicit stack), so deep trees cannot overflow it.
- **Accessors** — `code_unit`/`code_point_at`/`code_points`/
  `to_string_lossy`/`PartialEq`/`Hash` all route through `as_slice`, so a
  rope behaves exactly like its flattened content. `Debug` prints the text
  (the derived rope Debug would recurse the tree).

The `+` operator (both the both-strings fast path and the general string
path in `apply_binary`) uses `JsString::concat`, so the O(n²) repeated copy
in `s += 'x'` loops becomes ~O(1) appends. The concat benchmark dropped
from 0.88s (pre-migration baseline) / 0.72s (post-shapes) to **~0.15s
(~5x)**, closing the ≥2x gate. Validation: clippy clean; `cargo test
--workspace` 4,199 pass, 0 fail (five new rope unit tests in crux); full
release sweep over 48,622 fixtures: 0 fail, 229 skip (unchanged taxonomy),
0 crash. (`HashSet`/`HashMap` keys that hold `JsString`/`PropertyKey` carry
a documented `#[allow(clippy::mutable_key_type)]`: a rope's first hash
materializes its flat cache, but the hash output is content-stable.)

## Bytecode VM milestone (Cut 1 delivered, gate open)

The tree-walker is gone from normal execution: every expression and
statement compiles to `Step` bytecode at creation (`compile_expr`/
`compile_statements` in `crates/runtime/src/ir.rs`), and a `Vm` dispatch
loop runs the compiled body for ordinary calls/constructs, generators,
async functions, and top-level scripts. The batching defaults that cloned
suspension-free subtrees into `Step::Expr`/`Step::Stmt` for runtime
tree-walking are deleted. Removing the walker exposed a long tail of bugs
the batching used to mask (assignment-reference timing, member/private/
super `&&=`/`||=`/`??=` short-circuits, computed-compound key conversion
order, destructure/for-of iterator-close semantics, catch env unwinding,
finally routing, super base capture, template-object caching, Annex B
call-assignment targets, `using` disposal on abrupt errors); all fixed, and
the full release sweeps are at zero regressions vs the parent commit with
122 net fixes in `language` (152 fail vs the parent's 274).

The compiled path is at or near walker parity on every benchmark — nothing
lost, but the ≥5x gate is not met (arithmetic needs ≤0.23s):

| Benchmark | walker baseline | post-Cut-1 | Cut 2 (HEAD) |
|---|---|---|---|
| arithmetic | 1.14s | 1.56s | 1.19s |
| property access | 1.50s | 1.93s | 1.59s |
| string concat | ~0.15s | 0.196s | 0.160s |
| array iteration | 13.6–15s | 13.9s | 14.1s |
| function calls | 4.2–4.4s | 4.68s | 4.56s |

(The Cut 2 column is the first two encoding tightenings — `BinaryImm`
literal immediates and `CallFast` 0-2-arity calls — plus the optional-call
nullish-short-path fix they exposed. The runs are noisy on this machine
(±15%, the array iteration bounced 13.7–19.1s across runs); only string
concat moved consistently, ~5% lower, from the fused loop-test compare.)

Closing the gate is the rest of Cut 2's encoding tightening (constant
pool, zero-operand constants — see `docs/bytecode-plan.md`), then Cut 3
(registers) if needed; the measured wins so far are within bench noise, so
the structural costs (per-identifier env resolution, per-call function
machinery) dominate and a 5x will likely need one of those. `--print-bytecode`
remains a no-op because the printer does not exist yet, not because the VM
is missing.

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
(`--print-bytecode` will gain a real printer as Cut 2 lands — the `Step`
VM itself already exists.)
